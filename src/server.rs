use crate::protocol::{Frame, FrameCodec};
use bytes::Bytes;
use futures_util::{SinkExt, StreamExt};
use nix::pty::openpty;
use std::collections::{HashMap, VecDeque};
use std::io;
use std::os::fd::{AsRawFd, OwnedFd};
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
use std::sync::{Arc, OnceLock};
use tokio::io::AsyncReadExt;
use tokio::io::unix::AsyncFd;
use tokio::net::{UnixListener, UnixStream};
use tokio::process::Command;
use tokio::sync::{broadcast, mpsc};
use tokio_util::codec::Framed;
use tracing::{debug, info, warn};

/// Wrapper to distinguish active vs tail connections arriving via channel.
pub enum ClientConn {
    Active(Framed<UnixStream, FrameCodec>),
    Tail(Framed<UnixStream, FrameCodec>),
}

/// Events broadcast to tail clients.
#[derive(Clone)]
enum TailEvent {
    Data(Bytes),
    Exit { code: i32 },
}

pub struct SessionMetadata {
    pub pty_path: String,
    pub shell_pid: u32,
    pub created_at: u64,
    pub attached: AtomicBool,
    pub last_heartbeat: AtomicU64,
}

/// Wraps a child process and its process group ID.
/// On drop, sends SIGHUP to the entire process group.
struct ManagedChild {
    child: tokio::process::Child,
    pgid: nix::unistd::Pid,
}

impl ManagedChild {
    fn new(child: tokio::process::Child) -> Self {
        let pid = child.id().expect("child should have pid") as i32;
        Self { child, pgid: nix::unistd::Pid::from_raw(pid) }
    }
}

impl Drop for ManagedChild {
    fn drop(&mut self) {
        let _ = nix::sys::signal::killpg(self.pgid, nix::sys::signal::Signal::SIGHUP);
        let _ = self.child.try_wait();
    }
}

/// Why the relay loop exited.
enum RelayExit {
    /// Client disconnected — re-accept.
    ClientGone,
    /// Shell exited with a code — we're done.
    ShellExited(i32),
}

/// Events from agent connection tasks to the main relay loop.
enum AgentEvent {
    Accepted { channel_id: u32, writer_tx: mpsc::UnboundedSender<Bytes> },
    Data { channel_id: u32, data: Bytes },
    Closed { channel_id: u32 },
}

/// Events from open socket acceptor to the main relay loop.
enum OpenEvent {
    Url(String),
}

/// Spawn the agent acceptor task that accepts connections on the agent socket
/// and creates per-connection relay tasks.
fn spawn_agent_acceptor(
    listener: UnixListener,
    event_tx: mpsc::UnboundedSender<AgentEvent>,
    next_channel_id: Arc<AtomicU32>,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        loop {
            let (stream, _) = match listener.accept().await {
                Ok(conn) => conn,
                Err(e) => {
                    debug!("agent listener accept error: {e}");
                    break;
                }
            };

            if let Err(e) = crate::security::verify_peer_uid(&stream) {
                warn!("agent socket: {e}");
                continue;
            }

            let channel_id = next_channel_id.fetch_add(1, Ordering::Relaxed);

            let (read_half, write_half) = stream.into_split();
            let data_tx = event_tx.clone();
            let close_tx = event_tx.clone();
            let writer_tx = crate::spawn_channel_relay(
                channel_id,
                read_half,
                write_half,
                move |id, data| data_tx.send(AgentEvent::Data { channel_id: id, data }).is_ok(),
                move |id| {
                    let _ = close_tx.send(AgentEvent::Closed { channel_id: id });
                },
            );

            // Notify the relay loop about the new connection
            if event_tx.send(AgentEvent::Accepted { channel_id, writer_tx }).is_err() {
                break; // relay loop is gone
            }
        }
    })
}

/// Spawn the open acceptor task that accepts connections on the open socket,
/// reads a URL (up to 8KB, newline-terminated or EOF), and sends it as an event.
fn spawn_open_acceptor(
    listener: UnixListener,
    event_tx: mpsc::UnboundedSender<OpenEvent>,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        loop {
            let (mut stream, _) = match listener.accept().await {
                Ok(conn) => conn,
                Err(e) => {
                    debug!("open listener accept error: {e}");
                    break;
                }
            };

            // Open socket uses fire-and-forget connections (connect, write URL,
            // close). On macOS, getpeereid() can fail if the peer has already
            // disconnected by the time accept() returns. Reject known-bad UIDs
            // but tolerate OS-level errors (filesystem perms still protect).
            match crate::security::verify_peer_uid(&stream) {
                Ok(()) => {}
                Err(e) if e.kind() == std::io::ErrorKind::PermissionDenied => {
                    warn!("open socket: {e}");
                    continue;
                }
                Err(e) => {
                    // Intentional fallthrough: do NOT reject here. Unlike
                    // PermissionDenied (wrong UID), this means the OS couldn't
                    // retrieve credentials at all -- normal on macOS when the
                    // peer has already disconnected. Socket is 0600 so only
                    // the owning user can connect in the first place.
                    debug!("open socket peer_cred unavailable: {e}");
                }
            }

            let etx = event_tx.clone();
            tokio::spawn(async move {
                let mut buf = vec![0u8; 8192];
                let mut total = 0;
                loop {
                    match stream.read(&mut buf[total..]).await {
                        Ok(0) => break,
                        Ok(n) => {
                            total += n;
                            // Stop at newline or buffer full
                            if buf[..total].contains(&b'\n') || total >= buf.len() {
                                break;
                            }
                        }
                        Err(_) => return,
                    }
                }
                let s = String::from_utf8_lossy(&buf[..total]);
                let url = s.trim();
                if !url.is_empty() {
                    let _ = etx.send(OpenEvent::Url(url.to_string()));
                }
            });
        }
    })
}

/// Relay broadcast events to a tail client. Handles Ping/Pong for keepalive.
async fn tail_relay(
    mut framed: Framed<UnixStream, FrameCodec>,
    mut rx: broadcast::Receiver<TailEvent>,
) {
    loop {
        tokio::select! {
            event = rx.recv() => match event {
                Ok(TailEvent::Data(chunk)) => {
                    if framed.send(Frame::Data(chunk)).await.is_err() { break; }
                }
                Ok(TailEvent::Exit { code }) => {
                    let _ = framed.send(Frame::Exit { code }).await;
                    break;
                }
                Err(broadcast::error::RecvError::Lagged(_)) => continue,
                Err(broadcast::error::RecvError::Closed) => break,
            },
            frame = framed.next() => match frame {
                Some(Ok(Frame::Ping)) => { let _ = framed.send(Frame::Pong).await; }
                _ => break,
            },
        }
    }
}

/// Drain ring buffer contents to a tail client, then subscribe to broadcast and spawn relay.
fn spawn_tail(
    mut framed: Framed<UnixStream, FrameCodec>,
    ring_buf: &VecDeque<Bytes>,
    tail_tx: &broadcast::Sender<TailEvent>,
) {
    let rx = tail_tx.subscribe();
    let chunks: Vec<Bytes> = ring_buf.iter().cloned().collect();
    tokio::spawn(async move {
        for chunk in chunks {
            if framed.send(Frame::Data(chunk)).await.is_err() {
                return;
            }
        }
        tail_relay(framed, rx).await;
    });
}

pub async fn run(
    mut client_rx: mpsc::UnboundedReceiver<ClientConn>,
    metadata_slot: Arc<OnceLock<SessionMetadata>>,
    agent_socket_path: PathBuf,
    open_socket_path: PathBuf,
) -> anyhow::Result<()> {
    // Allocate PTY (once, before accept loop)
    let pty = openpty(None, None)?;
    let master: OwnedFd = pty.master;
    let slave: OwnedFd = pty.slave;

    // Get PTY slave name before we drop the slave fd
    let pty_path =
        nix::unistd::ttyname(&slave).map(|p| p.display().to_string()).unwrap_or_default();

    // Dup slave fds for shell stdio (before dropping slave)
    let slave_fd = slave.as_raw_fd();
    let stdin_fd = crate::security::checked_dup(slave_fd)?;
    let stdout_fd = crate::security::checked_dup(slave_fd)?;
    let stderr_fd = crate::security::checked_dup(slave_fd)?;
    let raw_stdin = stdin_fd.as_raw_fd();
    drop(slave);

    // Set master to non-blocking for AsyncFd
    let flags = nix::fcntl::fcntl(&master, nix::fcntl::FcntlArg::F_GETFL)?;
    let mut oflags = nix::fcntl::OFlag::from_bits_truncate(flags);
    oflags |= nix::fcntl::OFlag::O_NONBLOCK;
    nix::fcntl::fcntl(&master, nix::fcntl::FcntlArg::F_SETFL(oflags))?;

    let async_master = AsyncFd::new(master)?;
    let mut buf = vec![0u8; 4096];
    let mut ring_buf: VecDeque<Bytes> = VecDeque::new();
    let mut ring_buf_size: usize = 0;
    let mut ring_buf_dropped: usize = 0;
    const RING_BUF_CAP: usize = 1 << 20; // 1 MB

    // Agent event channel persists across acceptor lifetimes
    let (agent_event_tx, mut agent_event_rx) = mpsc::unbounded_channel::<AgentEvent>();

    // Broadcast channel for tail clients
    let (tail_tx, _) = broadcast::channel::<TailEvent>(256);

    // Wait for first active client before spawning shell (so we can read Env frame).
    // Tail clients that arrive before the first active client get subscribed to the
    // broadcast and will receive output once the shell starts.
    let mut framed = loop {
        match client_rx.recv().await {
            Some(ClientConn::Active(f)) => {
                info!("first client connected via channel");
                break f;
            }
            Some(ClientConn::Tail(f)) => {
                info!("tail client connected before shell spawn");
                spawn_tail(f, &ring_buf, &tail_tx);
                continue;
            }
            None => {
                info!("client channel closed before first client");
                cleanup_socket(&agent_socket_path);
                return Ok(());
            }
        }
    };

    // Read optional Env frame from first client (2s timeout -- generous for slow SSH tunnels)
    let env_vars =
        match tokio::time::timeout(std::time::Duration::from_secs(2), framed.next()).await {
            Ok(Some(Ok(Frame::Env { vars }))) => {
                debug!(count = vars.len(), "received env vars from client");
                vars
            }
            _ => Vec::new(),
        };

    // Spawn login shell on slave PTY
    let shell = std::env::var("SHELL").unwrap_or_else(|_| "/bin/sh".to_string());
    let home = std::env::var("HOME").ok();
    let mut cmd = Command::new(&shell);
    cmd.arg("-l");
    if let Some(ref dir) = home {
        cmd.current_dir(dir);
    }
    const ALLOWED_ENV_KEYS: &[&str] = &["TERM", "LANG", "COLORTERM", "BROWSER"];
    for (k, v) in &env_vars {
        if ALLOWED_ENV_KEYS.contains(&k.as_str()) {
            cmd.env(k, v);
        } else {
            warn!(key = k, "ignoring disallowed env var from client");
        }
    }
    // Set SSH_AUTH_SOCK to the agent socket path
    cmd.env("SSH_AUTH_SOCK", &agent_socket_path);
    // Set GRITTY_OPEN_SOCK so `gritty open` can find the open socket
    cmd.env("GRITTY_OPEN_SOCK", &open_socket_path);
    let mut managed = ManagedChild::new(unsafe {
        cmd.pre_exec(move || {
            nix::unistd::setsid().map_err(io::Error::other)?;
            libc::ioctl(raw_stdin, libc::TIOCSCTTY as libc::c_ulong, 0);
            Ok(())
        })
        .stdin(Stdio::from(stdin_fd))
        .stdout(Stdio::from(stdout_fd))
        .stderr(Stdio::from(stderr_fd))
        .spawn()?
    });

    let shell_pid = managed.child.id().unwrap_or(0);
    let created_at = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();

    let _ = metadata_slot.set(SessionMetadata {
        pty_path,
        shell_pid,
        created_at,
        attached: AtomicBool::new(false),
        last_heartbeat: AtomicU64::new(0),
    });

    // First client is already connected — enter relay directly
    metadata_slot.get().unwrap().attached.store(true, Ordering::Relaxed);

    // Agent forwarding state
    let mut agent_forward_enabled = false;
    let mut agent_channels: HashMap<u32, mpsc::UnboundedSender<Bytes>> = HashMap::new();
    let mut agent_acceptor: Option<tokio::task::JoinHandle<()>> = None;
    let next_agent_channel_id = Arc::new(AtomicU32::new(0));

    // Open forwarding state
    let mut open_forward_enabled = false;
    let mut open_acceptor: Option<tokio::task::JoinHandle<()>> = None;
    let (open_event_tx, mut open_event_rx) = mpsc::unbounded_channel::<OpenEvent>();

    let teardown_forwarding =
        |agent_channels: &mut HashMap<u32, mpsc::UnboundedSender<Bytes>>,
         agent_forward_enabled: &mut bool,
         agent_acceptor: &mut Option<tokio::task::JoinHandle<()>>,
         open_forward_enabled: &mut bool,
         open_acceptor: &mut Option<tokio::task::JoinHandle<()>>| {
            agent_channels.clear();
            *agent_forward_enabled = false;
            if let Some(handle) = agent_acceptor.take() {
                handle.abort();
            }
            cleanup_socket(&agent_socket_path);
            *open_forward_enabled = false;
            if let Some(handle) = open_acceptor.take() {
                handle.abort();
            }
            cleanup_socket(&open_socket_path);
        };

    // Outer loop: accept clients via channel. PTY persists across reconnects.
    // First iteration skips client-wait (first client already connected above).
    let mut first_client = true;
    loop {
        if !first_client {
            let got_client = 'drain: loop {
                tokio::select! {
                    client = client_rx.recv() => {
                        match client {
                            Some(ClientConn::Active(f)) => {
                                info!("client connected via channel");
                                framed = f;
                                break 'drain true;
                            }
                            Some(ClientConn::Tail(f)) => {
                                info!("tail client connected while disconnected");
                                spawn_tail(f, &ring_buf, &tail_tx);
                                continue;
                            }
                            None => {
                                info!("client channel closed");
                                break 'drain false;
                            }
                        }
                    }
                    status = managed.child.wait() => {
                        let code = status?.code().unwrap_or(1);
                        info!(code, "shell exited while awaiting client");
                        break 'drain false;
                    }
                    ready = async_master.readable() => {
                        let mut guard = ready?;
                        match guard.try_io(|inner| {
                            nix::unistd::read(inner, &mut buf).map_err(io::Error::from)
                        }) {
                            Ok(Ok(0)) => {
                                debug!("pty EOF while disconnected");
                                break 'drain false;
                            }
                            Ok(Ok(n)) => {
                                let chunk = Bytes::copy_from_slice(&buf[..n]);
                                let _ = tail_tx.send(TailEvent::Data(chunk.clone()));
                                ring_buf_size += chunk.len();
                                ring_buf.push_back(chunk);
                                while ring_buf_size > RING_BUF_CAP {
                                    if let Some(old) = ring_buf.pop_front() {
                                        ring_buf_size -= old.len();
                                        ring_buf_dropped += old.len();
                                    }
                                }
                            }
                            Ok(Err(e)) => {
                                if e.raw_os_error() == Some(libc::EIO) {
                                    debug!("pty EIO while disconnected");
                                    break 'drain false;
                                }
                                return Err(e.into());
                            }
                            Err(_would_block) => continue,
                        }
                    }
                }
            };
            if !got_client {
                break;
            }

            if let Some(meta) = metadata_slot.get() {
                meta.attached.store(true, Ordering::Relaxed);
            }
        }
        first_client = false;

        // Flush any buffered PTY output to the new client
        if !ring_buf.is_empty() {
            debug!(
                chunks = ring_buf.len(),
                bytes = ring_buf_size,
                dropped = ring_buf_dropped,
                "flushing ring buffer"
            );
            if ring_buf_dropped > 0 {
                let msg = format!("\r\n[gritty: {} bytes of output dropped]\r\n", ring_buf_dropped);
                framed.send(Frame::Data(Bytes::from(msg))).await?;
                ring_buf_dropped = 0;
            }
            while let Some(chunk) = ring_buf.pop_front() {
                framed.send(Frame::Data(chunk)).await?;
            }
            ring_buf_size = 0;
        }

        // Inner loop: relay between socket and PTY
        let exit = loop {
            tokio::select! {
                frame = framed.next() => {
                    match frame {
                        Some(Ok(Frame::Data(data))) => {
                            debug!(len = data.len(), "socket -> pty");
                            let mut written = 0;
                            while written < data.len() {
                                let mut guard = async_master.writable().await?;
                                match guard.try_io(|inner| {
                                    nix::unistd::write(inner, &data[written..]).map_err(io::Error::from)
                                }) {
                                    Ok(Ok(n)) => written += n,
                                    Ok(Err(e)) => return Err(e.into()),
                                    Err(_would_block) => continue,
                                }
                            }
                        }
                        Some(Ok(Frame::Resize { cols, rows })) => {
                            let (cols, rows) = crate::security::clamp_winsize(cols, rows);
                            debug!(cols, rows, "resize pty");
                            let ws = libc::winsize {
                                ws_row: rows,
                                ws_col: cols,
                                ws_xpixel: 0,
                                ws_ypixel: 0,
                            };
                            unsafe {
                                libc::ioctl(
                                    async_master.as_raw_fd(),
                                    libc::TIOCSWINSZ,
                                    &ws as *const _,
                                );
                            }
                            if let Ok(pgid) = nix::unistd::tcgetpgrp(&async_master) {
                                let _ = nix::sys::signal::killpg(pgid, nix::sys::signal::Signal::SIGWINCH);
                            }
                        }
                        Some(Ok(Frame::Ping)) => {
                            if let Some(meta) = metadata_slot.get() {
                                let now = std::time::SystemTime::now()
                                    .duration_since(std::time::UNIX_EPOCH)
                                    .unwrap_or_default()
                                    .as_secs();
                                meta.last_heartbeat.store(now, Ordering::Relaxed);
                            }
                            let _ = framed.send(Frame::Pong).await;
                        }
                        Some(Ok(Frame::AgentForward)) => {
                            debug!("agent forwarding enabled by client");
                            agent_forward_enabled = true;
                            // Bind agent socket so SSH_AUTH_SOCK points to a live file
                            if agent_acceptor.is_none() {
                                if let Some(listener) = bind_agent_listener(&agent_socket_path) {
                                    agent_acceptor = Some(spawn_agent_acceptor(listener, agent_event_tx.clone(), next_agent_channel_id.clone()));
                                }
                            }
                        }
                        Some(Ok(Frame::AgentData { channel_id, data })) => {
                            if let Some(tx) = agent_channels.get(&channel_id) {
                                let _ = tx.send(data);
                            }
                        }
                        Some(Ok(Frame::AgentClose { channel_id })) => {
                            // Drop the sender, writer task sees closed channel and exits
                            agent_channels.remove(&channel_id);
                        }
                        Some(Ok(Frame::OpenForward)) => {
                            debug!("open forwarding enabled by client");
                            open_forward_enabled = true;
                            if open_acceptor.is_none() {
                                if let Some(listener) = bind_agent_listener(&open_socket_path) {
                                    open_acceptor = Some(spawn_open_acceptor(listener, open_event_tx.clone()));
                                }
                            }
                        }
                        // Client disconnected or sent Exit
                        Some(Ok(Frame::Exit { .. })) | None => {
                            break RelayExit::ClientGone;
                        }
                        // Control frames ignored on session connections
                        Some(Ok(_)) => {}
                        Some(Err(e)) => return Err(e.into()),
                    }
                }

                ready = async_master.readable() => {
                    let mut guard = ready?;
                    match guard.try_io(|inner| {
                        nix::unistd::read(inner, &mut buf).map_err(io::Error::from)
                    }) {
                        Ok(Ok(0)) => {
                            debug!("pty EOF");
                            break RelayExit::ShellExited(0);
                        }
                        Ok(Ok(n)) => {
                            debug!(len = n, "pty -> socket");
                            let chunk = Bytes::copy_from_slice(&buf[..n]);
                            let _ = tail_tx.send(TailEvent::Data(chunk.clone()));
                            framed.send(Frame::Data(chunk)).await?;
                        }
                        Ok(Err(e)) => {
                            if e.raw_os_error() == Some(libc::EIO) {
                                debug!("pty EIO (shell exited)");
                                break RelayExit::ShellExited(0);
                            }
                            return Err(e.into());
                        }
                        Err(_would_block) => continue,
                    }
                }

                // Client takeover or tail via channel
                new_client = client_rx.recv() => {
                    match new_client {
                        Some(ClientConn::Active(new_framed)) => {
                            info!("new client via channel, detaching old client");
                            let _ = framed.send(Frame::Detached).await;
                            teardown_forwarding(
                                &mut agent_channels,
                                &mut agent_forward_enabled,
                                &mut agent_acceptor,
                                &mut open_forward_enabled,
                                &mut open_acceptor,
                            );
                            framed = new_framed;
                        }
                        Some(ClientConn::Tail(f)) => {
                            info!("tail client connected while active");
                            spawn_tail(f, &ring_buf, &tail_tx);
                        }
                        None => {}
                    }
                }

                // Agent events from acceptor/connection tasks
                event = agent_event_rx.recv() => {
                    match event {
                        Some(AgentEvent::Accepted { channel_id, writer_tx }) => {
                            if agent_forward_enabled {
                                agent_channels.insert(channel_id, writer_tx);
                                let _ = framed.send(Frame::AgentOpen { channel_id }).await;
                            }
                            // If forwarding not enabled, drop writer_tx (closes the connection)
                        }
                        Some(AgentEvent::Data { channel_id, data }) => {
                            if agent_forward_enabled && agent_channels.contains_key(&channel_id) {
                                let _ = framed.send(Frame::AgentData { channel_id, data }).await;
                            }
                        }
                        Some(AgentEvent::Closed { channel_id }) => {
                            if agent_channels.remove(&channel_id).is_some() {
                                let _ = framed.send(Frame::AgentClose { channel_id }).await;
                            }
                        }
                        None => {
                            // Agent acceptor exited — not fatal
                            debug!("agent event channel closed");
                        }
                    }
                }

                // Open URL events from open acceptor
                event = open_event_rx.recv() => {
                    match event {
                        Some(OpenEvent::Url(url)) => {
                            if open_forward_enabled {
                                let _ = framed.send(Frame::OpenUrl { url }).await;
                            }
                        }
                        None => {
                            debug!("open event channel closed");
                        }
                    }
                }

                status = managed.child.wait() => {
                    let code = status?.code().unwrap_or(1);
                    info!(code, "shell exited");
                    break RelayExit::ShellExited(code);
                }
            }
        };

        match exit {
            RelayExit::ClientGone => {
                if let Some(meta) = metadata_slot.get() {
                    meta.attached.store(false, Ordering::Relaxed);
                }
                teardown_forwarding(
                    &mut agent_channels,
                    &mut agent_forward_enabled,
                    &mut agent_acceptor,
                    &mut open_forward_enabled,
                    &mut open_acceptor,
                );
                info!("client disconnected, waiting for reconnect");
                continue;
            }
            RelayExit::ShellExited(mut code) => {
                // PTY EOF/EIO may fire before child.wait(), giving code=0.
                // Try to get the real exit code from the child.
                if let Ok(Some(status)) = managed.child.try_wait() {
                    code = status.code().unwrap_or(code);
                }
                let _ = tail_tx.send(TailEvent::Exit { code });
                let _ = framed.send(Frame::Exit { code }).await;
                info!(code, "session ended");
                break;
            }
        }
    }

    cleanup_socket(&agent_socket_path);
    cleanup_socket(&open_socket_path);
    Ok(())
}

fn bind_agent_listener(path: &Path) -> Option<UnixListener> {
    match crate::security::bind_unix_listener(path) {
        Ok(listener) => {
            info!(path = %path.display(), "agent socket listening");
            Some(listener)
        }
        Err(e) => {
            warn!("failed to bind agent socket at {}: {e}", path.display());
            None
        }
    }
}

fn cleanup_socket(path: &Path) {
    let _ = std::fs::remove_file(path);
}
