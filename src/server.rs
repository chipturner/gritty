use crate::protocol::{Frame, FrameCodec};
use bytes::Bytes;
use futures_util::{SinkExt, StreamExt};
use nix::pty::openpty;
use std::collections::{HashMap, VecDeque};
use std::io;
use std::ops::ControlFlow;
use std::os::fd::{AsRawFd, OwnedFd};
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
use std::sync::{Arc, OnceLock};
use std::time::Duration;
use tokio::io::AsyncReadExt;
use tokio::io::unix::AsyncFd;
use tokio::net::{UnixListener, UnixStream};
use tokio::process::Command;
use tokio::sync::{broadcast, mpsc};
use tokio_util::codec::Framed;
use tracing::{debug, info, warn};

/// Wrapper to distinguish active, tail, and send connections arriving via channel.
pub enum ClientConn {
    Active {
        framed: Framed<UnixStream, FrameCodec>,
        client_name: String,
    },
    Tail(Framed<UnixStream, FrameCodec>),
    /// Raw stream for file transfer (local-side commands routed through daemon).
    Send(UnixStream),
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
    pub client_name: std::sync::Mutex<String>,
    pub wants_agent: AtomicBool,
    pub wants_open: AtomicBool,
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
    Accepted { channel_id: u32, writer_tx: mpsc::Sender<Bytes> },
    Data { channel_id: u32, data: Bytes },
    Closed { channel_id: u32 },
}

/// Events from open socket acceptor to the main relay loop.
enum OpenEvent {
    Url { url: String, stream: UnixStream },
}

/// Events from tunnel TCP connection tasks to the main relay loop.
enum TunnelEvent {
    Connected { channel_id: u32, stream: tokio::net::TcpStream },
    ConnectFailed { channel_id: u32 },
    Data { channel_id: u32, data: Bytes },
    Closed { channel_id: u32 },
}

/// Events from port forward TCP acceptors and connections to the main relay loop.
enum PortForwardEvent {
    /// Svc socket requested a port forward.
    Requested { stream: UnixStream, direction: u8, listen_port: u16, target_port: u16 },
    /// TCP connection accepted on a listening port.
    Accepted { forward_id: u32, channel_id: u32, writer_tx: mpsc::Sender<Bytes> },
    /// Background TCP connect completed (remote-fwd PortForwardOpen).
    Connected { forward_id: u32, channel_id: u32, stream: tokio::net::TcpStream },
    /// Background TCP connect failed.
    ConnectFailed { channel_id: u32 },
    /// Data from a TCP connection.
    Data { channel_id: u32, data: Bytes },
    /// TCP connection closed.
    Closed { channel_id: u32 },
    /// Svc socket dropped -- teardown forward.
    Stopped { forward_id: u32 },
    /// Remote-forward: client didn't respond with PortForwardReady in time.
    RemoteTimeout { forward_id: u32 },
}

/// Per-forward state tracked by the server.
struct PortForwardState {
    /// Handle for the TCP listener task (aborted on teardown).
    listener_handle: Option<tokio::task::JoinHandle<()>>,
    /// Active TCP relay channels.
    channels: HashMap<u32, mpsc::Sender<Bytes>>,
    /// Handle for the svc stop watcher (aborted on teardown).
    stop_handle: Option<tokio::task::JoinHandle<()>>,
    target_port: u16,
}

/// Grouped state for SSH agent forwarding within a session.
struct AgentForwardState {
    enabled: bool,
    channels: HashMap<u32, mpsc::Sender<Bytes>>,
    acceptor: Option<tokio::task::JoinHandle<()>>,
    next_channel_id: Arc<AtomicU32>,
    socket_path: PathBuf,
}

impl AgentForwardState {
    fn new(socket_path: PathBuf) -> Self {
        Self {
            enabled: false,
            channels: HashMap::new(),
            acceptor: None,
            next_channel_id: Arc::new(AtomicU32::new(0)),
            socket_path,
        }
    }

    fn teardown(&mut self) {
        self.channels.clear();
        self.enabled = false;
        if let Some(handle) = self.acceptor.take() {
            handle.abort();
        }
        cleanup_socket(&self.socket_path);
    }
}

/// Grouped state for the OAuth callback reverse tunnel (multi-channel).
struct TunnelRelayState {
    port: Option<u16>,
    channels: HashMap<u32, mpsc::Sender<Bytes>>,
    idle_deadline: Option<tokio::time::Instant>,
    idle_timeout: Duration,
}

impl TunnelRelayState {
    fn new(idle_timeout: Duration) -> Self {
        Self { port: None, channels: HashMap::new(), idle_deadline: None, idle_timeout }
    }

    fn teardown(&mut self) {
        self.channels.clear();
        self.port = None;
        self.idle_deadline = None;
    }
}

/// Grouped state for TCP port forwarding (local and remote).
struct PortForwardTable {
    forwards: HashMap<u32, PortForwardState>,
    channels: HashMap<u32, (u32, mpsc::Sender<Bytes>)>,
    pending_remote: HashMap<u32, UnixStream>,
    next_forward_id: u32,
    next_channel_id: Arc<AtomicU32>,
}

impl PortForwardTable {
    fn new() -> Self {
        Self {
            forwards: HashMap::new(),
            channels: HashMap::new(),
            pending_remote: HashMap::new(),
            next_forward_id: 0,
            next_channel_id: Arc::new(AtomicU32::new(0)),
        }
    }

    fn teardown(&mut self) {
        for (_, pf) in self.forwards.drain() {
            if let Some(h) = pf.listener_handle {
                h.abort();
            }
            if let Some(h) = pf.stop_handle {
                h.abort();
            }
        }
        self.channels.clear();
        self.pending_remote.clear();
    }
}

/// File manifest entry parsed from sender protocol.
struct FileManifest {
    files: Vec<(String, u64, u32)>, // (basename, size, mode)
}

impl FileManifest {
    fn total_bytes(&self) -> u64 {
        self.files.iter().map(|(_, s, _)| s).sum()
    }
}

/// Events from the send socket acceptor to the main relay loop.
enum SendEvent {
    SenderArrived { stream: UnixStream, manifest: FileManifest },
    ReceiverArrived { stream: UnixStream },
}

/// State machine for file transfer rendezvous.
enum TransferState {
    Idle,
    WaitingForReceiver { sender_stream: UnixStream, manifest: FileManifest },
    WaitingForSender { receiver_stream: UnixStream },
    Active { relay_handle: tokio::task::JoinHandle<()> },
}

/// Sanitize a filename: strip path separators, reject ".." and empty names.
fn sanitize_filename(name: &str) -> Option<String> {
    let basename = std::path::Path::new(name).file_name().and_then(|n| n.to_str()).unwrap_or(name);
    if basename.is_empty() || basename == ".." || basename == "." {
        return None;
    }
    if basename.contains('\0') || basename.contains('\\') {
        return None;
    }
    Some(basename.to_string())
}

/// Extract redirect port from a URL's redirect_uri/redirect_url query parameter.
/// Returns Some(port) if the redirect target is localhost or 127.0.0.1 with a port.
fn extract_redirect_port(url_str: &str) -> Option<u16> {
    let parsed = url::Url::parse(url_str).ok()?;
    for (key, value) in parsed.query_pairs() {
        if key != "redirect_uri" && key != "redirect_url" {
            continue;
        }
        let redirect = url::Url::parse(&value).ok()?;
        match redirect.host_str()? {
            "localhost" | "127.0.0.1" | "::1" => return redirect.port(),
            _ => {}
        }
    }
    None
}

/// Check if a TCP port is in use by attempting to bind it.
fn port_in_use(port: u16) -> bool {
    std::net::TcpListener::bind(("127.0.0.1", port)).is_err()
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

/// Spawn a TCP acceptor for port forwarding. Each accepted connection assigns
/// a channel_id and spawns a bidirectional relay via `spawn_channel_relay`.
fn spawn_pf_tcp_acceptor(
    listener: tokio::net::TcpListener,
    forward_id: u32,
    next_channel_id: Arc<AtomicU32>,
    event_tx: mpsc::UnboundedSender<PortForwardEvent>,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        loop {
            let (stream, _) = match listener.accept().await {
                Ok(conn) => conn,
                Err(e) => {
                    debug!(forward_id, "pf tcp listener accept error: {e}");
                    break;
                }
            };

            let channel_id = next_channel_id.fetch_add(1, Ordering::Relaxed);
            let (read_half, write_half) = stream.into_split();
            let data_tx = event_tx.clone();
            let close_tx = event_tx.clone();
            let writer_tx = crate::spawn_channel_relay(
                channel_id,
                read_half,
                write_half,
                move |id, data| {
                    data_tx.send(PortForwardEvent::Data { channel_id: id, data }).is_ok()
                },
                move |id| {
                    let _ = close_tx.send(PortForwardEvent::Closed { channel_id: id });
                },
            );

            if event_tx
                .send(PortForwardEvent::Accepted { forward_id, channel_id, writer_tx })
                .is_err()
            {
                break;
            }
        }
    })
}

/// Spawn a task that watches a svc stream for EOF and sends a Stopped event.
fn spawn_pf_svc_watcher(
    stream: UnixStream,
    forward_id: u32,
    event_tx: mpsc::UnboundedSender<PortForwardEvent>,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        let mut stream = stream;
        let mut buf = [0u8; 1];
        // Block until EOF or error
        let _ = stream.read(&mut buf).await;
        let _ = event_tx.send(PortForwardEvent::Stopped { forward_id });
    })
}

/// Maximum URL length accepted on the service socket.
const URL_MAX_LEN: usize = 4096;

/// Parse sender manifest from the send socket stream.
/// Expected format after 'S' byte: file_count(u32 BE), then for each file:
///   filename_len(u16 BE), filename(UTF-8 bytes), file_size(u64 BE)
async fn parse_sender_manifest(stream: &mut UnixStream) -> io::Result<FileManifest> {
    let mut buf4 = [0u8; 4];
    stream.read_exact(&mut buf4).await?;
    let file_count = u32::from_be_bytes(buf4);
    if file_count == 0 || file_count > 10_000 {
        return Err(io::Error::new(io::ErrorKind::InvalidData, "invalid file count"));
    }
    let mut files = Vec::with_capacity(file_count as usize);
    for _ in 0..file_count {
        let mut buf2 = [0u8; 2];
        stream.read_exact(&mut buf2).await?;
        let name_len = u16::from_be_bytes(buf2) as usize;
        if name_len == 0 || name_len > 4096 {
            return Err(io::Error::new(io::ErrorKind::InvalidData, "invalid filename length"));
        }
        let mut name_buf = vec![0u8; name_len];
        stream.read_exact(&mut name_buf).await?;
        let name = String::from_utf8(name_buf)
            .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
        let name = match sanitize_filename(&name) {
            Some(n) => n,
            None => {
                return Err(io::Error::new(io::ErrorKind::InvalidData, "invalid filename"));
            }
        };
        let mut buf8 = [0u8; 8];
        stream.read_exact(&mut buf8).await?;
        let file_size = u64::from_be_bytes(buf8);
        let mut buf4m = [0u8; 4];
        stream.read_exact(&mut buf4m).await?;
        let mode = u32::from_be_bytes(buf4m);
        files.push((name, file_size, mode));
    }
    Ok(FileManifest { files })
}

/// Parse receiver dest_dir from the send socket stream.
/// Expected format after 'R' byte: UTF-8 string, newline-terminated.
async fn parse_receiver_dest(stream: &mut UnixStream) -> io::Result<String> {
    let mut buf = Vec::with_capacity(256);
    let mut byte = [0u8; 1];
    loop {
        match stream.read_exact(&mut byte).await {
            Ok(_) => {
                if byte[0] == b'\n' {
                    break;
                }
                buf.push(byte[0]);
                if buf.len() > 4096 {
                    return Err(io::Error::new(io::ErrorKind::InvalidData, "dest dir too long"));
                }
            }
            Err(e) if e.kind() == io::ErrorKind::UnexpectedEof => break,
            Err(e) => return Err(e),
        }
    }
    String::from_utf8(buf).map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))
}

/// Spawn the unified service socket acceptor. Reads a SvcRequest discriminator
/// byte then dispatches to the appropriate handler.
fn spawn_svc_acceptor(
    listener: UnixListener,
    open_event_tx: mpsc::UnboundedSender<OpenEvent>,
    send_event_tx: mpsc::UnboundedSender<SendEvent>,
    pf_event_tx: mpsc::UnboundedSender<PortForwardEvent>,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        loop {
            let (mut stream, _) = match listener.accept().await {
                Ok(conn) => conn,
                Err(e) => {
                    debug!("svc listener accept error: {e}");
                    break;
                }
            };

            // Lenient peer UID check: reject known-bad UIDs but tolerate
            // OS-level errors (fire-and-forget open connections on macOS may
            // disconnect before getpeereid returns).
            match crate::security::verify_peer_uid(&stream) {
                Ok(()) => {}
                Err(e) if e.kind() == std::io::ErrorKind::PermissionDenied => {
                    warn!("svc socket: {e}");
                    continue;
                }
                Err(e) => {
                    debug!("svc socket peer_cred unavailable: {e}");
                }
            }

            let otx = open_event_tx.clone();
            let stx = send_event_tx.clone();
            let ptx = pf_event_tx.clone();
            tokio::spawn(async move {
                // Read discriminator byte
                let mut disc = [0u8; 1];
                if stream.read_exact(&mut disc).await.is_err() {
                    return;
                }
                match crate::protocol::SvcRequest::from_byte(disc[0]) {
                    Some(crate::protocol::SvcRequest::OpenUrl) => {
                        let mut buf = vec![0u8; URL_MAX_LEN];
                        let mut total = 0;
                        loop {
                            match stream.read(&mut buf[total..]).await {
                                Ok(0) => break,
                                Ok(n) => {
                                    total += n;
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
                            let _ = otx.send(OpenEvent::Url { url: url.to_string(), stream });
                        }
                    }
                    Some(crate::protocol::SvcRequest::Send) => {
                        match parse_sender_manifest(&mut stream).await {
                            Ok(manifest) => {
                                let _ = stx.send(SendEvent::SenderArrived { stream, manifest });
                            }
                            Err(e) => debug!("svc socket: bad sender manifest: {e}"),
                        }
                    }
                    Some(crate::protocol::SvcRequest::Receive) => {
                        match parse_receiver_dest(&mut stream).await {
                            Ok(_) => {
                                let _ = stx.send(SendEvent::ReceiverArrived { stream });
                            }
                            Err(e) => debug!("svc socket: bad receiver dest: {e}"),
                        }
                    }
                    Some(crate::protocol::SvcRequest::PortForward) => {
                        // Wire: [direction: u8][listen_port: u16 BE][target_port: u16 BE]
                        let mut hdr = [0u8; 5];
                        if stream.read_exact(&mut hdr).await.is_err() {
                            return;
                        }
                        let direction = hdr[0];
                        let listen_port = u16::from_be_bytes([hdr[1], hdr[2]]);
                        let target_port = u16::from_be_bytes([hdr[3], hdr[4]]);
                        let _ = ptx.send(PortForwardEvent::Requested {
                            stream,
                            direction,
                            listen_port,
                            target_port,
                        });
                    }
                    None => {
                        debug!("svc socket: unknown request byte: 0x{:02x}", disc[0]);
                    }
                }
            });
        }
    })
}

/// Spawn the transfer relay task. Reads file data from sender, writes to receiver.
/// Sends SendOffer/SendDone/SendCancel notification frames to the active client.
fn spawn_transfer_relay(
    mut sender: UnixStream,
    mut receiver: UnixStream,
    manifest: FileManifest,
    notify_tx: mpsc::UnboundedSender<Frame>,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        use tokio::io::AsyncWriteExt;

        let file_count = manifest.files.len() as u32;
        let total_bytes = manifest.total_bytes();

        // Notify active client about transfer start
        let _ = notify_tx.send(Frame::SendOffer { file_count, total_bytes });

        // Signal sender to start streaming (write 0x01 go byte)
        if sender.write_all(&[0x01]).await.is_err() {
            let _ = notify_tx.send(Frame::SendCancel { reason: "sender disconnected".into() });
            return;
        }

        // Write file_count to receiver
        if receiver.write_all(&file_count.to_be_bytes()).await.is_err() {
            let _ = notify_tx.send(Frame::SendCancel { reason: "receiver disconnected".into() });
            return;
        }

        // For each file: write metadata to receiver, then relay file data
        let mut buf = vec![0u8; 64 * 1024];
        for (name, size, mode) in &manifest.files {
            // Write per-file header to receiver
            let name_bytes = name.as_bytes();
            let mut hdr = Vec::with_capacity(2 + name_bytes.len() + 8 + 4);
            hdr.extend_from_slice(&(name_bytes.len() as u16).to_be_bytes());
            hdr.extend_from_slice(name_bytes);
            hdr.extend_from_slice(&size.to_be_bytes());
            hdr.extend_from_slice(&mode.to_be_bytes());
            if receiver.write_all(&hdr).await.is_err() {
                let _ =
                    notify_tx.send(Frame::SendCancel { reason: "receiver disconnected".into() });
                return;
            }

            // Relay exactly file_size bytes from sender to receiver
            let mut remaining = *size;
            while remaining > 0 {
                let to_read = (remaining as usize).min(buf.len());
                match sender.read_exact(&mut buf[..to_read]).await {
                    Ok(_) => {
                        if receiver.write_all(&buf[..to_read]).await.is_err() {
                            let _ = notify_tx
                                .send(Frame::SendCancel { reason: "receiver disconnected".into() });
                            return;
                        }
                        remaining -= to_read as u64;
                    }
                    Err(_) => {
                        let _ = notify_tx
                            .send(Frame::SendCancel { reason: "sender disconnected".into() });
                        return;
                    }
                }
            }
        }

        // Write sentinel: filename_len = 0
        if receiver.write_all(&[0u8; 2]).await.is_err() {
            let _ = notify_tx.send(Frame::SendCancel { reason: "receiver disconnected".into() });
            return;
        }

        let _ = notify_tx.send(Frame::SendDone);
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

/// Groups references needed by inner relay handler methods.
/// `framed` is kept outside (passed to handlers) so `tokio::select!` can
/// poll `framed.next()` independently without conflicting borrows.
struct ServerRelay<'a> {
    async_master: &'a AsyncFd<OwnedFd>,
    agent: &'a mut AgentForwardState,
    tunnel: &'a mut TunnelRelayState,
    pf: &'a mut PortForwardTable,
    transfer_state: &'a mut TransferState,
    open_forward_enabled: &'a mut bool,
    tail_tx: &'a broadcast::Sender<TailEvent>,
    metadata_slot: &'a Arc<OnceLock<SessionMetadata>>,
    agent_event_tx: &'a mpsc::UnboundedSender<AgentEvent>,
    tunnel_event_tx: &'a mpsc::UnboundedSender<TunnelEvent>,
    pf_event_tx: &'a mpsc::UnboundedSender<PortForwardEvent>,
    send_notify_tx: &'a mpsc::UnboundedSender<Frame>,
    capability_warn_deadline: Option<tokio::time::Instant>,
}

impl ServerRelay<'_> {
    async fn check_capability_warning(&mut self, framed: &mut Framed<UnixStream, FrameCodec>) {
        self.capability_warn_deadline = None;
        let Some(meta) = self.metadata_slot.get() else { return };
        let wants_agent = meta.wants_agent.load(Ordering::Relaxed);
        let wants_open = meta.wants_open.load(Ordering::Relaxed);
        let missing_agent = wants_agent && !self.agent.enabled;
        let missing_open = wants_open && !*self.open_forward_enabled;
        if !missing_agent && !missing_open {
            return;
        }
        let mut missing = Vec::new();
        if missing_agent {
            missing.push("-A");
        }
        if missing_open {
            missing.push("-O");
        }
        let flags = missing.join(" ");
        let msg = format!(
            "\r\n\x1b[2;33m[gritty: session expects {flags} but current client is missing it]\x1b[0m\r\n"
        );
        let _ = framed.send(Frame::Data(Bytes::from(msg))).await;
    }

    async fn handle_client_frame(
        &mut self,
        framed: &mut Framed<UnixStream, FrameCodec>,
        frame: Option<Result<Frame, io::Error>>,
    ) -> Result<ControlFlow<RelayExit>, anyhow::Error> {
        match frame {
            Some(Ok(Frame::Data(data))) => {
                debug!(len = data.len(), "socket -> pty");
                let mut written = 0;
                while written < data.len() {
                    let mut guard = self.async_master.writable().await?;
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
                let ws = libc::winsize { ws_row: rows, ws_col: cols, ws_xpixel: 0, ws_ypixel: 0 };
                unsafe {
                    libc::ioctl(self.async_master.as_raw_fd(), libc::TIOCSWINSZ, &ws as *const _);
                }
                if let Ok(pgid) = nix::unistd::tcgetpgrp(self.async_master) {
                    let _ = nix::sys::signal::killpg(pgid, nix::sys::signal::Signal::SIGWINCH);
                }
            }
            Some(Ok(Frame::Ping)) => {
                if let Some(meta) = self.metadata_slot.get() {
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
                self.agent.enabled = true;
                if let Some(meta) = self.metadata_slot.get() {
                    meta.wants_agent.store(true, Ordering::Relaxed);
                }
                if self.agent.acceptor.is_none() {
                    if let Some(listener) = bind_agent_listener(&self.agent.socket_path) {
                        self.agent.acceptor = Some(spawn_agent_acceptor(
                            listener,
                            self.agent_event_tx.clone(),
                            self.agent.next_channel_id.clone(),
                        ));
                    }
                }
            }
            Some(Ok(Frame::AgentData { channel_id, data })) => {
                if let Some(tx) = self.agent.channels.get(&channel_id) {
                    let _ = tx.send(data).await;
                }
            }
            Some(Ok(Frame::AgentClose { channel_id })) => {
                self.agent.channels.remove(&channel_id);
            }
            Some(Ok(Frame::OpenForward)) => {
                debug!("open forwarding enabled by client");
                *self.open_forward_enabled = true;
                if let Some(meta) = self.metadata_slot.get() {
                    meta.wants_open.store(true, Ordering::Relaxed);
                }
            }
            Some(Ok(Frame::TunnelOpen { channel_id })) => {
                if let Some(port) = self.tunnel.port {
                    self.tunnel.idle_deadline = None;
                    let tx = self.tunnel_event_tx.clone();
                    tokio::spawn(async move {
                        match tokio::net::TcpStream::connect(("127.0.0.1", port)).await {
                            Ok(stream) => {
                                let _ = tx.send(TunnelEvent::Connected { channel_id, stream });
                            }
                            Err(_) => {
                                let _ = tx.send(TunnelEvent::ConnectFailed { channel_id });
                            }
                        }
                    });
                }
            }
            Some(Ok(Frame::TunnelData { channel_id, data })) => {
                if let Some(tx) = self.tunnel.channels.get(&channel_id) {
                    let _ = tx.send(data).await;
                }
            }
            Some(Ok(Frame::TunnelClose { channel_id })) => {
                self.tunnel.channels.remove(&channel_id);
                if self.tunnel.channels.is_empty() && self.tunnel.port.is_some() {
                    self.tunnel.idle_deadline =
                        Some(tokio::time::Instant::now() + self.tunnel.idle_timeout);
                }
            }
            Some(Ok(Frame::PortForwardReady { forward_id })) => {
                if let Some(mut svc_stream) = self.pf.pending_remote.remove(&forward_id) {
                    use tokio::io::AsyncWriteExt;
                    let _ = svc_stream.write_all(&[0x01]).await;
                    let stop_handle =
                        spawn_pf_svc_watcher(svc_stream, forward_id, self.pf_event_tx.clone());
                    if let Some(fwd) = self.pf.forwards.get_mut(&forward_id) {
                        fwd.stop_handle = Some(stop_handle);
                    }
                }
            }
            Some(Ok(Frame::PortForwardOpen { forward_id, channel_id, target_port })) => {
                if self.pf.forwards.contains_key(&forward_id) {
                    let tx = self.pf_event_tx.clone();
                    tokio::spawn(async move {
                        match tokio::net::TcpStream::connect(("127.0.0.1", target_port)).await {
                            Ok(stream) => {
                                let _ = tx.send(PortForwardEvent::Connected {
                                    forward_id,
                                    channel_id,
                                    stream,
                                });
                            }
                            Err(_) => {
                                let _ = tx.send(PortForwardEvent::ConnectFailed { channel_id });
                            }
                        }
                    });
                }
            }
            Some(Ok(Frame::PortForwardData { channel_id, data })) => {
                if let Some((_, tx)) = self.pf.channels.get(&channel_id) {
                    let _ = tx.send(data).await;
                }
            }
            Some(Ok(Frame::PortForwardClose { channel_id })) => {
                if let Some((forward_id, _)) = self.pf.channels.remove(&channel_id) {
                    if let Some(fwd) = self.pf.forwards.get_mut(&forward_id) {
                        fwd.channels.remove(&channel_id);
                    }
                }
            }
            Some(Ok(Frame::PortForwardStop { forward_id })) => {
                if let Some(mut svc_stream) = self.pf.pending_remote.remove(&forward_id) {
                    use tokio::io::AsyncWriteExt;
                    let _ = svc_stream.write_all(&[0x02]).await;
                    let _ = svc_stream.write_all(b"client declined forward").await;
                }
                if let Some(fwd) = self.pf.forwards.remove(&forward_id) {
                    if let Some(h) = fwd.listener_handle {
                        h.abort();
                    }
                    if let Some(h) = fwd.stop_handle {
                        h.abort();
                    }
                    for ch_id in fwd.channels.keys() {
                        self.pf.channels.remove(ch_id);
                    }
                }
            }
            Some(Ok(Frame::Env { vars })) => {
                // Update client name on reconnect/takeover
                if let Some(meta) = self.metadata_slot.get() {
                    if let Some((_, v)) = vars.iter().find(|(k, _)| k == "GRITTY_CLIENT") {
                        if let Ok(mut name) = meta.client_name.lock() {
                            *name = v.clone();
                        }
                    }
                }
            }
            Some(Ok(Frame::Exit { .. })) | None => {
                return Ok(ControlFlow::Break(RelayExit::ClientGone));
            }
            Some(Ok(_)) => {}
            Some(Err(e)) => return Err(e.into()),
        }
        Ok(ControlFlow::Continue(()))
    }

    async fn handle_agent_event(
        &mut self,
        framed: &mut Framed<UnixStream, FrameCodec>,
        event: Option<AgentEvent>,
    ) {
        match event {
            Some(AgentEvent::Accepted { channel_id, writer_tx }) => {
                if self.agent.enabled {
                    self.agent.channels.insert(channel_id, writer_tx);
                    let _ = framed.send(Frame::AgentOpen { channel_id }).await;
                }
            }
            Some(AgentEvent::Data { channel_id, data }) => {
                if self.agent.enabled && self.agent.channels.contains_key(&channel_id) {
                    let _ = framed.send(Frame::AgentData { channel_id, data }).await;
                }
            }
            Some(AgentEvent::Closed { channel_id }) => {
                if self.agent.channels.remove(&channel_id).is_some() {
                    let _ = framed.send(Frame::AgentClose { channel_id }).await;
                }
            }
            None => {
                debug!("agent event channel closed");
            }
        }
    }

    async fn handle_open_event(
        &mut self,
        framed: &mut Framed<UnixStream, FrameCodec>,
        event: Option<OpenEvent>,
    ) {
        match event {
            Some(OpenEvent::Url { url, mut stream }) => {
                use tokio::io::AsyncWriteExt;
                if *self.open_forward_enabled {
                    if let Some(port) = extract_redirect_port(&url) {
                        if port_in_use(port) {
                            debug!(port, "setting up reverse tunnel for OAuth callback");
                            self.tunnel.port = Some(port);
                            let _ = framed.send(Frame::TunnelListen { port }).await;
                        }
                    }
                    let _ = framed.send(Frame::OpenUrl { url }).await;
                    let _ = stream.write_all(&[0x01]).await;
                } else {
                    let _ = stream.write_all(&[0x00]).await;
                }
            }
            None => {
                debug!("open event channel closed");
            }
        }
    }

    async fn handle_tunnel_event(
        &mut self,
        framed: &mut Framed<UnixStream, FrameCodec>,
        event: Option<TunnelEvent>,
    ) {
        match event {
            Some(TunnelEvent::Connected { channel_id, stream }) => {
                if self.tunnel.port.is_some() {
                    debug!(channel_id, "tunnel channel connected");
                    self.tunnel.idle_deadline = None;
                    let (mut read_half, write_half) = stream.into_split();
                    let (writer_tx, mut writer_rx) =
                        mpsc::channel::<Bytes>(crate::CHANNEL_RELAY_BUFFER);
                    self.tunnel.channels.insert(channel_id, writer_tx);

                    // Writer task: channel -> TCP
                    tokio::spawn(async move {
                        use tokio::io::AsyncWriteExt;
                        let mut writer = write_half;
                        while let Some(data) = writer_rx.recv().await {
                            if writer.write_all(&data).await.is_err() {
                                break;
                            }
                        }
                    });

                    // Reader task: TCP -> TunnelEvent
                    let tx = self.tunnel_event_tx.clone();
                    tokio::spawn(async move {
                        let mut buf = vec![0u8; 4096];
                        loop {
                            match read_half.read(&mut buf).await {
                                Ok(0) | Err(_) => {
                                    let _ = tx.send(TunnelEvent::Closed { channel_id });
                                    break;
                                }
                                Ok(n) => {
                                    let data = Bytes::copy_from_slice(&buf[..n]);
                                    if tx.send(TunnelEvent::Data { channel_id, data }).is_err() {
                                        break;
                                    }
                                }
                            }
                        }
                    });
                }
            }
            Some(TunnelEvent::ConnectFailed { channel_id }) => {
                let _ = framed.send(Frame::TunnelClose { channel_id }).await;
            }
            Some(TunnelEvent::Data { channel_id, data }) => {
                let _ = framed.send(Frame::TunnelData { channel_id, data }).await;
            }
            Some(TunnelEvent::Closed { channel_id }) => {
                self.tunnel.channels.remove(&channel_id);
                let _ = framed.send(Frame::TunnelClose { channel_id }).await;
                if self.tunnel.channels.is_empty() && self.tunnel.port.is_some() {
                    self.tunnel.idle_deadline =
                        Some(tokio::time::Instant::now() + self.tunnel.idle_timeout);
                }
            }
            None => {}
        }
    }

    async fn handle_send_notification(
        &mut self,
        framed: &mut Framed<UnixStream, FrameCodec>,
        notification: Option<Frame>,
    ) {
        if let Some(frame) = notification {
            if matches!(frame, Frame::SendDone | Frame::SendCancel { .. }) {
                *self.transfer_state = TransferState::Idle;
            }
            let _ = framed.send(frame).await;
        }
    }

    async fn handle_pf_event(
        &mut self,
        framed: &mut Framed<UnixStream, FrameCodec>,
        event: Option<PortForwardEvent>,
    ) {
        match event {
            Some(PortForwardEvent::Requested { stream, direction, listen_port, target_port }) => {
                use tokio::io::AsyncWriteExt;
                let fwd_id = self.pf.next_forward_id;
                self.pf.next_forward_id += 1;
                if direction == 0 {
                    // Local-forward: server binds TCP, forwards to client
                    match tokio::net::TcpListener::bind(("127.0.0.1", listen_port)).await {
                        Ok(listener) => {
                            debug!(fwd_id, listen_port, target_port, "local-forward: bound");
                            let handle = spawn_pf_tcp_acceptor(
                                listener,
                                fwd_id,
                                self.pf.next_channel_id.clone(),
                                self.pf_event_tx.clone(),
                            );
                            let mut s = stream;
                            let _ = s.write_all(&[0x01]).await;
                            let stream = s;
                            let stop_handle =
                                spawn_pf_svc_watcher(stream, fwd_id, self.pf_event_tx.clone());
                            self.pf.forwards.insert(
                                fwd_id,
                                PortForwardState {
                                    listener_handle: Some(handle),
                                    channels: HashMap::new(),
                                    stop_handle: Some(stop_handle),
                                    target_port,
                                },
                            );
                        }
                        Err(e) => {
                            debug!(listen_port, "local-forward: bind failed: {e}");
                            let mut s = stream;
                            let msg = format!("bind failed: {e}");
                            let _ = s.write_all(&[0x02]).await;
                            let _ = s.write_all(msg.as_bytes()).await;
                        }
                    }
                } else {
                    // Remote-forward: tell client to bind, wait for Ready
                    let _ = framed
                        .send(Frame::PortForwardListen {
                            forward_id: fwd_id,
                            listen_port,
                            target_port,
                        })
                        .await;
                    self.pf.pending_remote.insert(fwd_id, stream);
                    let timeout_tx = self.pf_event_tx.clone();
                    tokio::spawn(async move {
                        tokio::time::sleep(Duration::from_secs(5)).await;
                        let _ =
                            timeout_tx.send(PortForwardEvent::RemoteTimeout { forward_id: fwd_id });
                    });
                    self.pf.forwards.insert(
                        fwd_id,
                        PortForwardState {
                            listener_handle: None,
                            channels: HashMap::new(),
                            stop_handle: None,
                            target_port,
                        },
                    );
                }
            }
            Some(PortForwardEvent::Accepted { forward_id, channel_id, writer_tx }) => {
                if let Some(fwd) = self.pf.forwards.get_mut(&forward_id) {
                    fwd.channels.insert(channel_id, writer_tx.clone());
                    self.pf.channels.insert(channel_id, (forward_id, writer_tx));
                    let _ = framed
                        .send(Frame::PortForwardOpen {
                            forward_id,
                            channel_id,
                            target_port: fwd.target_port,
                        })
                        .await;
                }
            }
            Some(PortForwardEvent::Connected { forward_id, channel_id, stream }) => {
                if self.pf.forwards.contains_key(&forward_id) {
                    let (read_half, write_half) = stream.into_split();
                    let data_tx = self.pf_event_tx.clone();
                    let close_tx = self.pf_event_tx.clone();
                    let writer_tx = crate::spawn_channel_relay(
                        channel_id,
                        read_half,
                        write_half,
                        move |id, data| {
                            data_tx.send(PortForwardEvent::Data { channel_id: id, data }).is_ok()
                        },
                        move |id| {
                            let _ = close_tx.send(PortForwardEvent::Closed { channel_id: id });
                        },
                    );
                    self.pf.channels.insert(channel_id, (forward_id, writer_tx.clone()));
                    if let Some(fwd) = self.pf.forwards.get_mut(&forward_id) {
                        fwd.channels.insert(channel_id, writer_tx);
                    }
                }
            }
            Some(PortForwardEvent::ConnectFailed { channel_id }) => {
                let _ = framed.send(Frame::PortForwardClose { channel_id }).await;
            }
            Some(PortForwardEvent::Data { channel_id, data }) => {
                if self.pf.channels.contains_key(&channel_id) {
                    let _ = framed.send(Frame::PortForwardData { channel_id, data }).await;
                }
            }
            Some(PortForwardEvent::Closed { channel_id }) => {
                if let Some((forward_id, _)) = self.pf.channels.remove(&channel_id) {
                    let _ = framed.send(Frame::PortForwardClose { channel_id }).await;
                    if let Some(fwd) = self.pf.forwards.get_mut(&forward_id) {
                        fwd.channels.remove(&channel_id);
                    }
                }
            }
            Some(PortForwardEvent::Stopped { forward_id }) => {
                debug!(forward_id, "port forward stopped (svc socket dropped)");
                if let Some(fwd) = self.pf.forwards.remove(&forward_id) {
                    if let Some(h) = fwd.listener_handle {
                        h.abort();
                    }
                    if let Some(h) = fwd.stop_handle {
                        h.abort();
                    }
                    for ch_id in fwd.channels.keys() {
                        self.pf.channels.remove(ch_id);
                    }
                    let _ = framed.send(Frame::PortForwardStop { forward_id }).await;
                }
            }
            Some(PortForwardEvent::RemoteTimeout { forward_id }) => {
                if let Some(mut svc_stream) = self.pf.pending_remote.remove(&forward_id) {
                    use tokio::io::AsyncWriteExt;
                    debug!(forward_id, "remote-forward: client did not respond in time");
                    let _ = svc_stream.write_all(&[0x02]).await;
                    let _ = svc_stream.write_all(b"timed out waiting for client").await;
                    self.pf.forwards.remove(&forward_id);
                    let _ = framed.send(Frame::PortForwardStop { forward_id }).await;
                }
            }
            None => {}
        }
    }
}

#[allow(clippy::too_many_arguments)]
pub async fn run(
    mut client_rx: mpsc::UnboundedReceiver<ClientConn>,
    metadata_slot: Arc<OnceLock<SessionMetadata>>,
    agent_socket_path: PathBuf,
    svc_socket_path: PathBuf,
    session_id: u32,
    session_name: Option<String>,
    command: Option<String>,
    ring_buffer_cap: usize,
    oauth_tunnel_idle_timeout: u64,
    initial_cols: u16,
    initial_rows: u16,
    cwd: Option<String>,
) -> anyhow::Result<()> {
    // Allocate PTY with initial window size when available
    let winsize = if initial_cols > 0 && initial_rows > 0 {
        Some(nix::pty::Winsize {
            ws_row: initial_rows,
            ws_col: initial_cols,
            ws_xpixel: 0,
            ws_ypixel: 0,
        })
    } else {
        None
    };
    let pty = openpty(winsize.as_ref(), None)?;
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

    // Agent forwarding state
    let mut agent = AgentForwardState::new(agent_socket_path);
    let (agent_event_tx, mut agent_event_rx) = mpsc::unbounded_channel::<AgentEvent>();

    // Open forwarding state (no acceptor -- svc_acceptor handles the socket)
    let mut open_forward_enabled = false;

    // Tunnel state (reverse TCP tunnel for OAuth callbacks)
    let (tunnel_event_tx, mut tunnel_event_rx) = mpsc::unbounded_channel::<TunnelEvent>();
    let mut tunnel = TunnelRelayState::new(Duration::from_secs(oauth_tunnel_idle_timeout));

    // Broadcast channel for tail clients
    let (tail_tx, _) = broadcast::channel::<TailEvent>(256);

    // Open and send event channels (created at session start, persist across clients)
    let (open_event_tx, mut open_event_rx) = mpsc::unbounded_channel::<OpenEvent>();
    let (send_event_tx, mut send_event_rx) = mpsc::unbounded_channel::<SendEvent>();
    let (send_notify_tx, mut send_notify_rx) = mpsc::unbounded_channel::<Frame>();
    let mut transfer_state = TransferState::Idle;
    let mut svc_acceptor: Option<tokio::task::JoinHandle<()>> = None;

    // Port forward event channel and state
    let (pf_event_tx, mut pf_event_rx) = mpsc::unbounded_channel::<PortForwardEvent>();
    let mut pf = PortForwardTable::new();

    // Bind unified service socket immediately (always available)
    if let Some(listener) = bind_agent_listener(&svc_socket_path) {
        svc_acceptor = Some(spawn_svc_acceptor(
            listener,
            open_event_tx.clone(),
            send_event_tx.clone(),
            pf_event_tx.clone(),
        ));
    }

    // Wait for first active client before spawning shell (so we can read Env frame).
    // Tail and send clients that arrive before the first active client get handled
    // appropriately (tail subscribed to broadcast, send queued for rendezvous).
    let initial_client_name;
    let mut framed = loop {
        tokio::select! {
            client = client_rx.recv() => match client {
                Some(ClientConn::Active { framed: f, client_name: cn }) => {
                    info!("first client connected via channel");
                    // Stash client_name so it's available before Env frame
                    initial_client_name = cn;
                    break f;
                }
                Some(ClientConn::Tail(f)) => {
                    info!("tail client connected before shell spawn");
                    spawn_tail(f, &ring_buf, &tail_tx);
                    continue;
                }
                Some(ClientConn::Send(stream)) => {
                    handle_send_stream(stream, &send_event_tx);
                    continue;
                }
                None => {
                    info!("client channel closed before first client");
                    cleanup_socket(&agent.socket_path);
                    cleanup_socket(&svc_socket_path);
                    return Ok(());
                }
            },
            event = send_event_rx.recv() => {
                if let Some(event) = event {
                    handle_send_event(event, &mut transfer_state, &send_notify_tx);
                }
                continue;
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

    // Spawn shell (or custom command) on slave PTY
    let shell = std::env::var("SHELL").unwrap_or_else(|_| "/bin/sh".to_string());
    let home = std::env::var("HOME").ok();
    let mut cmd = Command::new(&shell);
    if let Some(ref user_cmd) = command {
        cmd.arg("-c").arg(user_cmd);
    } else {
        cmd.arg("-l");
    }
    // Use cwd from NewSession if provided, otherwise fall back to $HOME
    let work_dir = cwd.as_deref().filter(|s| !s.is_empty()).or(home.as_deref());
    if let Some(dir) = work_dir {
        cmd.current_dir(dir);
    }
    // Prefer client_name from Env frame, fall back to Attach-provided name
    let client_name = env_vars
        .iter()
        .find(|(k, _)| k == "GRITTY_CLIENT")
        .map(|(_, v)| v.clone())
        .unwrap_or(initial_client_name);
    const ALLOWED_ENV_KEYS: &[&str] = &["TERM", "LANG", "COLORTERM", "GRITTY_CLIENT"];
    for (k, v) in &env_vars {
        if ALLOWED_ENV_KEYS.contains(&k.as_str()) {
            cmd.env(k, v);
        } else if k == "BROWSER" {
            // Client signals open forwarding desired; create a gritty-open symlink
            // so BROWSER is a single path with no spaces.
            let exe = std::env::current_exe()
                .ok()
                .and_then(|p| p.to_str().map(String::from))
                .unwrap_or_else(|| "gritty".into());
            let open_link = svc_socket_path.parent().unwrap_or(Path::new(".")).join("gritty-open");
            let _ = std::fs::remove_file(&open_link);
            let _ = std::os::unix::fs::symlink(&exe, &open_link);
            cmd.env("BROWSER", &open_link);
        } else {
            warn!(key = k, "ignoring disallowed env var from client");
        }
    }
    // Set SSH_AUTH_SOCK to the agent socket path
    cmd.env("SSH_AUTH_SOCK", &agent.socket_path);
    // Set GRITTY_SOCK so `gritty open`/`gritty send`/`gritty receive` find the service socket
    cmd.env("GRITTY_SOCK", &svc_socket_path);
    // Session context env vars
    cmd.env("GRITTY_SESSION", session_id.to_string());
    if let Some(ref name) = session_name {
        cmd.env("GRITTY_SESSION_NAME", name);
    }
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
        client_name: std::sync::Mutex::new(client_name),
        wants_agent: AtomicBool::new(false),
        wants_open: AtomicBool::new(false),
    });

    // First client is already connected — enter relay directly
    metadata_slot.get().unwrap().attached.store(true, Ordering::Relaxed);

    // Outer loop: accept clients via channel. PTY persists across reconnects.
    // First iteration skips client-wait (first client already connected above).
    let mut first_client = true;
    loop {
        if !first_client {
            let got_client = 'drain: loop {
                tokio::select! {
                    client = client_rx.recv() => {
                        match client {
                            Some(ClientConn::Active { framed: f, client_name: cn }) => {
                                info!("client connected via channel");
                                framed = f;
                                if let Some(meta) = metadata_slot.get() {
                                    if let Ok(mut n) = meta.client_name.lock() {
                                        *n = cn;
                                    }
                                }
                                break 'drain true;
                            }
                            Some(ClientConn::Tail(f)) => {
                                info!("tail client connected while disconnected");
                                spawn_tail(f, &ring_buf, &tail_tx);
                                continue;
                            }
                            Some(ClientConn::Send(stream)) => {
                                handle_send_stream(stream, &send_event_tx);
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
                                if tail_tx.receiver_count() > 0 {
                                    let _ = tail_tx.send(TailEvent::Data(chunk.clone()));
                                }
                                ring_buf_size += chunk.len();
                                ring_buf.push_back(chunk);
                                while ring_buf_size > ring_buffer_cap {
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
                    event = send_event_rx.recv() => {
                        if let Some(event) = event {
                            handle_send_event(event, &mut transfer_state, &send_notify_tx);
                        }
                        continue;
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
                let msg = format!(
                    "\r\n\x1b[2;33m[gritty: {} bytes of output dropped]\x1b[0m\r\n",
                    ring_buf_dropped
                );
                framed.send(Frame::Data(Bytes::from(msg))).await?;
                ring_buf_dropped = 0;
            }
            while let Some(chunk) = ring_buf.pop_front() {
                framed.send(Frame::Data(chunk)).await?;
            }
            ring_buf_size = 0;
        }

        // Inner loop: relay between socket and PTY.
        // Scoped block so ServerRelay borrows are released before
        // the post-loop code accesses the underlying state directly.
        let exit = {
            // On reconnect (not first client), schedule a capability check after
            // init frames (Env, AgentForward, OpenForward) have had time to arrive.
            let cap_deadline = if !first_client {
                Some(tokio::time::Instant::now() + std::time::Duration::from_millis(500))
            } else {
                None
            };
            let mut relay = ServerRelay {
                async_master: &async_master,
                agent: &mut agent,
                tunnel: &mut tunnel,
                pf: &mut pf,
                transfer_state: &mut transfer_state,
                open_forward_enabled: &mut open_forward_enabled,
                tail_tx: &tail_tx,
                metadata_slot: &metadata_slot,
                agent_event_tx: &agent_event_tx,
                tunnel_event_tx: &tunnel_event_tx,
                pf_event_tx: &pf_event_tx,
                send_notify_tx: &send_notify_tx,
                capability_warn_deadline: cap_deadline,
            };
            loop {
                tokio::select! {
                    frame = framed.next() => {
                        if let ControlFlow::Break(exit) = relay.handle_client_frame(&mut framed, frame).await? {
                            break exit;
                        }
                    }

                    ready = relay.async_master.readable() => {
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
                                if relay.tail_tx.receiver_count() > 0 {
                                    let _ = relay.tail_tx.send(TailEvent::Data(chunk.clone()));
                                }
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

                    new_client = client_rx.recv() => {
                        match new_client {
                            Some(ClientConn::Active { framed: new_framed, client_name: cn }) => {
                                info!("new client via channel, detaching old client");
                                let _ = framed.send(Frame::Detached).await;
                                relay.agent.teardown();
                                relay.tunnel.teardown();
                                relay.pf.teardown();
                                *relay.open_forward_enabled = false;
                                // Update client_name from the new Attach
                                if let Some(meta) = relay.metadata_slot.get() {
                                    if let Ok(mut n) = meta.client_name.lock() {
                                        *n = cn;
                                    }
                                }
                                // Inform the new client about the takeover
                                let was_attached = relay.metadata_slot.get()
                                    .map(|m| m.attached.load(Ordering::Relaxed))
                                    .unwrap_or(false);
                                framed = new_framed;
                                if was_attached {
                                    let hb_age = relay.metadata_slot.get()
                                        .and_then(|m| {
                                            let hb = m.last_heartbeat.load(Ordering::Relaxed);
                                            if hb == 0 { return None; }
                                            let now = std::time::SystemTime::now()
                                                .duration_since(std::time::UNIX_EPOCH)
                                                .unwrap_or_default()
                                                .as_secs();
                                            Some(now.saturating_sub(hb))
                                        });
                                    let hb_str = match hb_age {
                                        Some(s) => format!("{s}s ago"),
                                        None => "n/a".to_string(),
                                    };
                                    let msg = format!(
                                        "\r\n\x1b[2;33m[gritty: took over session (was active, heartbeat {hb_str})]\x1b[0m\r\n"
                                    );
                                    let _ = framed.send(Frame::Data(Bytes::from(msg))).await;
                                }
                                // Schedule capability check after init frames arrive
                                relay.capability_warn_deadline = Some(
                                    tokio::time::Instant::now() + std::time::Duration::from_millis(500)
                                );
                            }
                            Some(ClientConn::Tail(f)) => {
                                info!("tail client connected while active");
                                spawn_tail(f, &ring_buf, relay.tail_tx);
                            }
                            Some(ClientConn::Send(stream)) => {
                                handle_send_stream(stream, &send_event_tx);
                            }
                            None => {}
                        }
                    }

                    event = agent_event_rx.recv() => {
                        relay.handle_agent_event(&mut framed, event).await;
                    }

                    event = open_event_rx.recv() => {
                        relay.handle_open_event(&mut framed, event).await;
                    }

                    event = tunnel_event_rx.recv() => {
                        relay.handle_tunnel_event(&mut framed, event).await;
                    }

                    _ = async {
                        match relay.tunnel.idle_deadline {
                            Some(deadline) => tokio::time::sleep_until(deadline).await,
                            None => std::future::pending().await,
                        }
                    } => {
                        if relay.tunnel.channels.is_empty() {
                            debug!("tunnel idle timeout, tearing down");
                            relay.tunnel.teardown();
                        }
                    }

                    event = send_event_rx.recv() => {
                        if let Some(event) = event {
                            handle_send_event(event, relay.transfer_state, relay.send_notify_tx);
                        }
                    }

                    notification = send_notify_rx.recv() => {
                        relay.handle_send_notification(&mut framed, notification).await;
                    }

                    event = pf_event_rx.recv() => {
                        relay.handle_pf_event(&mut framed, event).await;
                    }

                    _ = async {
                        match relay.capability_warn_deadline {
                            Some(deadline) => tokio::time::sleep_until(deadline).await,
                            None => std::future::pending().await,
                        }
                    } => {
                        relay.check_capability_warning(&mut framed).await;
                    }

                    status = managed.child.wait() => {
                        let code = status?.code().unwrap_or(1);
                        info!(code, "shell exited");
                        let _ = relay.tail_tx.send(TailEvent::Exit { code });
                        break RelayExit::ShellExited(code);
                    }
                }
            }
        };

        match exit {
            RelayExit::ClientGone => {
                if let Some(meta) = metadata_slot.get() {
                    meta.attached.store(false, Ordering::Relaxed);
                }
                agent.teardown();
                tunnel.teardown();
                pf.teardown();
                open_forward_enabled = false;
                info!("client disconnected, waiting for reconnect");
                continue;
            }
            RelayExit::ShellExited(mut code) => {
                // PTY EOF/EIO may fire before child.wait() -- especially on
                // macOS where the race window is wider. Give the child a
                // moment to actually exit so we can capture the real code.
                if let Ok(Ok(status)) = tokio::time::timeout(
                    std::time::Duration::from_millis(500),
                    managed.child.wait(),
                )
                .await
                {
                    code = status.code().unwrap_or(code);
                }
                let _ = tail_tx.send(TailEvent::Exit { code });
                let _ = framed.send(Frame::Exit { code }).await;
                info!(code, "session ended");
                break;
            }
        }
    }

    cleanup_socket(&agent.socket_path);
    cleanup_socket(&svc_socket_path);
    if let Some(handle) = svc_acceptor.take() {
        handle.abort();
    }
    Ok(())
}

/// Handle a raw send stream from ClientConn::Send (local-side commands).
/// Spawns a task to read the SvcRequest discriminator and manifest/dest, then sends the event.
fn handle_send_stream(mut stream: UnixStream, send_event_tx: &mpsc::UnboundedSender<SendEvent>) {
    let etx = send_event_tx.clone();
    tokio::spawn(async move {
        let mut disc = [0u8; 1];
        if stream.read_exact(&mut disc).await.is_err() {
            return;
        }
        match crate::protocol::SvcRequest::from_byte(disc[0]) {
            Some(crate::protocol::SvcRequest::Send) => {
                match parse_sender_manifest(&mut stream).await {
                    Ok(manifest) => {
                        let _ = etx.send(SendEvent::SenderArrived { stream, manifest });
                    }
                    Err(e) => debug!("send stream: bad sender manifest: {e}"),
                }
            }
            Some(crate::protocol::SvcRequest::Receive) => {
                match parse_receiver_dest(&mut stream).await {
                    Ok(_) => {
                        let _ = etx.send(SendEvent::ReceiverArrived { stream });
                    }
                    Err(e) => debug!("send stream: bad receiver dest: {e}"),
                }
            }
            _ => {}
        }
    });
}

/// Check if a stream's peer has disconnected (EOF or error).
/// Returns true if the stream is dead and should be discarded.
fn stream_is_dead(stream: &UnixStream) -> bool {
    let mut probe = [0u8; 1];
    match stream.try_read(&mut probe) {
        Ok(0) => true,                                                     // EOF
        Err(ref e) if e.kind() == std::io::ErrorKind::WouldBlock => false, // alive
        Err(_) => true,                                                    // error
        Ok(_) => false, // unexpected data, treat as alive
    }
}

/// Handle a send event: pair sender and receiver for rendezvous.
fn handle_send_event(
    event: SendEvent,
    state: &mut TransferState,
    notify_tx: &mpsc::UnboundedSender<Frame>,
) {
    match event {
        SendEvent::SenderArrived { stream, manifest } => {
            let old = std::mem::replace(state, TransferState::Idle);
            match old {
                TransferState::WaitingForSender { receiver_stream }
                    if !stream_is_dead(&receiver_stream) =>
                {
                    info!(
                        files = manifest.files.len(),
                        bytes = manifest.total_bytes(),
                        "transfer: sender+receiver paired"
                    );
                    let handle =
                        spawn_transfer_relay(stream, receiver_stream, manifest, notify_tx.clone());
                    *state = TransferState::Active { relay_handle: handle };
                }
                _ => {
                    if let TransferState::Active { relay_handle } = old {
                        let _ = notify_tx.send(Frame::SendCancel {
                            reason: "superseded by new sender".to_string(),
                        });
                        relay_handle.abort();
                    }
                    info!(files = manifest.files.len(), "transfer: sender waiting for receiver");
                    *state = TransferState::WaitingForReceiver { sender_stream: stream, manifest };
                }
            }
        }
        SendEvent::ReceiverArrived { stream } => {
            let old = std::mem::replace(state, TransferState::Idle);
            match old {
                TransferState::WaitingForReceiver { sender_stream, manifest }
                    if !stream_is_dead(&sender_stream) =>
                {
                    info!(
                        files = manifest.files.len(),
                        bytes = manifest.total_bytes(),
                        "transfer: receiver+sender paired"
                    );
                    let handle =
                        spawn_transfer_relay(sender_stream, stream, manifest, notify_tx.clone());
                    *state = TransferState::Active { relay_handle: handle };
                }
                _ => {
                    if let TransferState::Active { relay_handle } = old {
                        let _ = notify_tx.send(Frame::SendCancel {
                            reason: "superseded by new receiver".to_string(),
                        });
                        relay_handle.abort();
                    }
                    info!("transfer: receiver waiting for sender");
                    *state = TransferState::WaitingForSender { receiver_stream: stream };
                }
            }
        }
    }
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extract_redirect_port_basic() {
        let url = "https://accounts.google.com/o/oauth2/auth?redirect_uri=http://localhost:8080/callback&client_id=xyz";
        assert_eq!(extract_redirect_port(url), Some(8080));
    }

    #[test]
    fn extract_redirect_port_127() {
        let url = "https://auth.example.com/authorize?redirect_uri=http://127.0.0.1:9090/cb";
        assert_eq!(extract_redirect_port(url), Some(9090));
    }

    #[test]
    fn extract_redirect_port_url_encoded() {
        let url = "https://auth.example.com/authorize?redirect_uri=http%3A%2F%2Flocalhost%3A3000%2Fcallback";
        assert_eq!(extract_redirect_port(url), Some(3000));
    }

    #[test]
    fn extract_redirect_port_no_port() {
        let url = "https://auth.example.com/authorize?redirect_uri=http://localhost/callback";
        assert_eq!(extract_redirect_port(url), None);
    }

    #[test]
    fn extract_redirect_port_no_redirect_uri() {
        let url = "https://example.com/page?foo=bar";
        assert_eq!(extract_redirect_port(url), None);
    }

    #[test]
    fn extract_redirect_port_non_localhost() {
        let url =
            "https://auth.example.com/authorize?redirect_uri=https://example.com:8080/callback";
        assert_eq!(extract_redirect_port(url), None);
    }

    #[test]
    fn extract_redirect_port_https_redirect() {
        let url = "https://auth.example.com/authorize?redirect_uri=https://localhost:4443/callback";
        assert_eq!(extract_redirect_port(url), Some(4443));
    }

    #[test]
    fn extract_redirect_url_variant() {
        let url = "https://auth.example.com/authorize?redirect_url=http://localhost:5000/cb";
        assert_eq!(extract_redirect_port(url), Some(5000));
    }
}
