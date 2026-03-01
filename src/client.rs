use crate::protocol::{Frame, FrameCodec};
use bytes::Bytes;
use futures_util::{SinkExt, StreamExt};
use nix::sys::termios::{self, FlushArg, LocalFlags, SetArg, SpecialCharacterIndices, Termios};
use std::collections::HashMap;
use std::io::{self, Read, Write};
use std::os::fd::{AsFd, AsRawFd, BorrowedFd};
use std::path::Path;
use std::time::Duration;
use tokio::io::unix::AsyncFd;
use tokio::net::UnixStream;
use tokio::signal::unix::{SignalKind, signal};
use tokio::sync::mpsc;
use tokio::time::Instant;
use tokio_util::codec::Framed;
use tracing::{debug, info};

// --- Escape sequence processing (SSH-style ~. detach, ~^Z suspend, ~? help) ---

const ESCAPE_HELP: &[u8] = b"\r\nSupported escape sequences:\r\n\
    ~.  - detach from session\r\n\
    ~^Z - suspend client\r\n\
    ~?  - this message\r\n\
    ~~  - send the escape character by typing it twice\r\n\
(Note that escapes are only recognized immediately after newline.)\r\n";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum EscapeState {
    Normal,
    AfterNewline,
    AfterTilde,
}

#[derive(Debug, PartialEq, Eq)]
enum EscapeAction {
    Data(Vec<u8>),
    Detach,
    Suspend,
    Help,
}

struct EscapeProcessor {
    state: EscapeState,
}

impl EscapeProcessor {
    fn new() -> Self {
        Self { state: EscapeState::AfterNewline }
    }

    fn process(&mut self, input: &[u8]) -> Vec<EscapeAction> {
        let mut actions = Vec::new();
        let mut data_buf = Vec::new();

        for &b in input {
            match self.state {
                EscapeState::Normal => {
                    if b == b'\n' || b == b'\r' {
                        self.state = EscapeState::AfterNewline;
                    }
                    data_buf.push(b);
                }
                EscapeState::AfterNewline => {
                    if b == b'~' {
                        self.state = EscapeState::AfterTilde;
                        // Buffer the tilde — don't send yet
                        if !data_buf.is_empty() {
                            actions.push(EscapeAction::Data(std::mem::take(&mut data_buf)));
                        }
                    } else if b == b'\n' || b == b'\r' {
                        // Stay in AfterNewline
                        data_buf.push(b);
                    } else {
                        self.state = EscapeState::Normal;
                        data_buf.push(b);
                    }
                }
                EscapeState::AfterTilde => {
                    match b {
                        b'.' => {
                            if !data_buf.is_empty() {
                                actions.push(EscapeAction::Data(std::mem::take(&mut data_buf)));
                            }
                            actions.push(EscapeAction::Detach);
                            return actions; // Stop processing
                        }
                        0x1a => {
                            // Ctrl-Z
                            if !data_buf.is_empty() {
                                actions.push(EscapeAction::Data(std::mem::take(&mut data_buf)));
                            }
                            actions.push(EscapeAction::Suspend);
                            self.state = EscapeState::Normal;
                        }
                        b'?' => {
                            if !data_buf.is_empty() {
                                actions.push(EscapeAction::Data(std::mem::take(&mut data_buf)));
                            }
                            actions.push(EscapeAction::Help);
                            self.state = EscapeState::Normal;
                        }
                        b'~' => {
                            // Literal tilde
                            data_buf.push(b'~');
                            self.state = EscapeState::Normal;
                        }
                        b'\n' | b'\r' => {
                            // Flush buffered tilde + this byte
                            data_buf.push(b'~');
                            data_buf.push(b);
                            self.state = EscapeState::AfterNewline;
                        }
                        _ => {
                            // Unknown — flush tilde + byte
                            data_buf.push(b'~');
                            data_buf.push(b);
                            self.state = EscapeState::Normal;
                        }
                    }
                }
            }
        }

        if !data_buf.is_empty() {
            actions.push(EscapeAction::Data(data_buf));
        }
        actions
    }
}

fn suspend(raw_guard: &RawModeGuard, nb_guard: &NonBlockGuard) -> anyhow::Result<()> {
    // Restore cooked mode and blocking stdin so the parent shell works normally
    termios::tcsetattr(raw_guard.fd, SetArg::TCSAFLUSH, &raw_guard.original)?;
    let _ = nix::fcntl::fcntl(nb_guard.fd, nix::fcntl::FcntlArg::F_SETFL(nb_guard.original_flags));

    nix::sys::signal::kill(nix::unistd::Pid::from_raw(0), nix::sys::signal::Signal::SIGTSTP)?;

    // After resume (fg): re-enter raw mode and non-blocking stdin
    let _ = nix::fcntl::fcntl(
        nb_guard.fd,
        nix::fcntl::FcntlArg::F_SETFL(nb_guard.original_flags | nix::fcntl::OFlag::O_NONBLOCK),
    );
    let mut raw = raw_guard.original.clone();
    termios::cfmakeraw(&mut raw);
    termios::tcsetattr(raw_guard.fd, SetArg::TCSAFLUSH, &raw)?;
    Ok(())
}

const SEND_TIMEOUT: Duration = Duration::from_secs(5);

struct NonBlockGuard {
    fd: BorrowedFd<'static>,
    original_flags: nix::fcntl::OFlag,
}

impl NonBlockGuard {
    fn set(fd: BorrowedFd<'static>) -> nix::Result<Self> {
        let flags = nix::fcntl::fcntl(fd, nix::fcntl::FcntlArg::F_GETFL)?;
        let original_flags = nix::fcntl::OFlag::from_bits_truncate(flags);
        nix::fcntl::fcntl(
            fd,
            nix::fcntl::FcntlArg::F_SETFL(original_flags | nix::fcntl::OFlag::O_NONBLOCK),
        )?;
        Ok(Self { fd, original_flags })
    }
}

impl Drop for NonBlockGuard {
    fn drop(&mut self) {
        let _ = nix::fcntl::fcntl(self.fd, nix::fcntl::FcntlArg::F_SETFL(self.original_flags));
    }
}

struct RawModeGuard {
    fd: BorrowedFd<'static>,
    original: Termios,
}

impl RawModeGuard {
    fn enter(fd: BorrowedFd<'static>) -> nix::Result<Self> {
        let original = termios::tcgetattr(fd)?;
        let mut raw = original.clone();
        termios::cfmakeraw(&mut raw);
        termios::tcsetattr(fd, SetArg::TCSAFLUSH, &raw)?;
        Ok(Self { fd, original })
    }
}

impl Drop for RawModeGuard {
    fn drop(&mut self) {
        let _ = termios::tcsetattr(self.fd, SetArg::TCSAFLUSH, &self.original);
    }
}

/// Suppresses stdin echo for tail mode: disables ECHO and ICANON but keeps
/// ISIG so Ctrl-C still generates SIGINT. Flushes pending input on drop.
struct SuppressInputGuard {
    fd: BorrowedFd<'static>,
    original: Termios,
}

impl SuppressInputGuard {
    fn enter(fd: BorrowedFd<'static>) -> nix::Result<Self> {
        let original = termios::tcgetattr(fd)?;
        let mut modified = original.clone();
        modified.local_flags.remove(LocalFlags::ECHO | LocalFlags::ICANON);
        modified.control_chars[SpecialCharacterIndices::VMIN as usize] = 1;
        modified.control_chars[SpecialCharacterIndices::VTIME as usize] = 0;
        termios::tcsetattr(fd, SetArg::TCSAFLUSH, &modified)?;
        Ok(Self { fd, original })
    }
}

impl Drop for SuppressInputGuard {
    fn drop(&mut self) {
        let _ = termios::tcflush(self.fd, FlushArg::TCIFLUSH);
        let _ = termios::tcsetattr(self.fd, SetArg::TCSAFLUSH, &self.original);
    }
}

/// Write all bytes to stdout, retrying on WouldBlock.
/// Needed because setting O_NONBLOCK on stdin also affects stdout
/// when they share the same terminal file description.
fn write_stdout(data: &[u8]) -> io::Result<()> {
    let mut stdout = io::stdout();
    let mut written = 0;
    while written < data.len() {
        match stdout.write(&data[written..]) {
            Ok(n) => written += n,
            Err(e) if e.kind() == io::ErrorKind::WouldBlock => {
                std::thread::sleep(Duration::from_millis(1));
            }
            Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
            Err(e) => return Err(e),
        }
    }
    loop {
        match stdout.flush() {
            Ok(()) => return Ok(()),
            Err(e) if e.kind() == io::ErrorKind::WouldBlock => {
                std::thread::sleep(Duration::from_millis(1));
            }
            Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
            Err(e) => return Err(e),
        }
    }
}

/// Format a byte count as a human-readable size string.
pub fn format_size(bytes: u64) -> String {
    const KB: u64 = 1024;
    const MB: u64 = 1024 * KB;
    const GB: u64 = 1024 * MB;
    if bytes >= GB {
        format!("{:.1}GB", bytes as f64 / GB as f64)
    } else if bytes >= MB {
        format!("{:.1}MB", bytes as f64 / MB as f64)
    } else if bytes >= KB {
        format!("{:.1}KB", bytes as f64 / KB as f64)
    } else {
        format!("{bytes}B")
    }
}

fn status_msg(text: &str) -> String {
    format!("\r\n\x1b[2;33m[{text}]\x1b[0m\r\n")
}

fn success_msg(text: &str) -> String {
    format!("\r\n\x1b[32m[{text}]\x1b[0m\r\n")
}

fn error_msg(text: &str) -> String {
    format!("\r\n\x1b[31m[{text}]\x1b[0m\r\n")
}

fn get_terminal_size() -> (u16, u16) {
    let mut ws: libc::winsize = unsafe { std::mem::zeroed() };
    if unsafe { libc::ioctl(libc::STDIN_FILENO, libc::TIOCGWINSZ, &mut ws) } != 0
        || ws.ws_col == 0
        || ws.ws_row == 0
    {
        return (80, 24);
    }
    (ws.ws_col, ws.ws_row)
}

/// Send a frame with a timeout. Returns false if the send failed or timed out.
async fn timed_send(framed: &mut Framed<UnixStream, FrameCodec>, frame: Frame) -> bool {
    match tokio::time::timeout(SEND_TIMEOUT, framed.send(frame)).await {
        Ok(Ok(())) => true,
        Ok(Err(e)) => {
            debug!("send error: {e}");
            false
        }
        Err(_) => {
            debug!("send timed out");
            false
        }
    }
}

const HEARTBEAT_INTERVAL: Duration = Duration::from_secs(5);
const HEARTBEAT_TIMEOUT: Duration = Duration::from_secs(15);

/// Events from local agent connection tasks to the relay loop.
enum AgentEvent {
    Data { channel_id: u32, data: Bytes },
    Closed { channel_id: u32 },
}

/// Events from the tunnel TCP listener/connection to the relay loop.
enum ClientTunnelEvent {
    Accepted(mpsc::UnboundedSender<Bytes>),
    Data(Bytes),
    Closed,
}

/// Events from port forward TCP acceptors/connections on the client side.
enum ClientPortForwardEvent {
    Accepted { forward_id: u32, channel_id: u32, writer_tx: mpsc::UnboundedSender<Bytes> },
    Data { channel_id: u32, data: Bytes },
    Closed { channel_id: u32 },
}

/// Per-forward state on the client side.
struct ClientPortForwardState {
    listener_handle: Option<tokio::task::JoinHandle<()>>,
    target_port: u16,
}

/// Send session setup frames (env, agent/open forwarding, resize, redraw).
/// Returns false if the connection dropped during setup.
async fn send_init_frames(
    framed: &mut Framed<UnixStream, FrameCodec>,
    env_vars: &[(String, String)],
    forward_agent: bool,
    agent_socket: Option<&str>,
    forward_open: bool,
    redraw: bool,
) -> bool {
    if !env_vars.is_empty() && !timed_send(framed, Frame::Env { vars: env_vars.to_vec() }).await {
        return false;
    }
    if forward_agent && agent_socket.is_some() && !timed_send(framed, Frame::AgentForward).await {
        return false;
    }
    if forward_open && !timed_send(framed, Frame::OpenForward).await {
        return false;
    }
    let (cols, rows) = get_terminal_size();
    if !timed_send(framed, Frame::Resize { cols, rows }).await {
        return false;
    }
    if redraw && !timed_send(framed, Frame::Data(Bytes::from_static(b"\x0c"))).await {
        return false;
    }
    true
}

/// Relay between stdin/stdout and the framed socket.
/// Returns `Some(code)` on clean shell exit or detach, `None` on server disconnect / heartbeat timeout.
#[allow(clippy::too_many_arguments)]
async fn relay(
    framed: &mut Framed<UnixStream, FrameCodec>,
    async_stdin: &AsyncFd<io::Stdin>,
    sigwinch: &mut tokio::signal::unix::Signal,
    buf: &mut [u8],
    mut escape: Option<&mut EscapeProcessor>,
    raw_guard: &RawModeGuard,
    nb_guard: &NonBlockGuard,
    agent_socket: Option<&str>,
    oauth_redirect: bool,
    oauth_timeout: u64,
) -> anyhow::Result<Option<i32>> {
    let mut sigterm = signal(SignalKind::terminate())?;
    let mut sighup = signal(SignalKind::hangup())?;

    let mut heartbeat_interval = tokio::time::interval(HEARTBEAT_INTERVAL);
    heartbeat_interval.reset(); // first tick is immediate otherwise; delay it
    let mut last_pong = Instant::now();

    // Agent channel management
    let mut agent_channels: HashMap<u32, mpsc::UnboundedSender<Bytes>> = HashMap::new();
    let (agent_event_tx, mut agent_event_rx) = mpsc::unbounded_channel::<AgentEvent>();

    // Tunnel state (reverse TCP tunnel for OAuth callbacks)
    let mut tunnel_listener: Option<tokio::task::JoinHandle<()>> = None;
    let (tunnel_event_tx, mut tunnel_event_rx) = mpsc::unbounded_channel::<ClientTunnelEvent>();
    let mut tunnel_writer: Option<mpsc::UnboundedSender<Bytes>> = None;

    // Port forward state
    let (pf_event_tx, mut pf_event_rx) = mpsc::unbounded_channel::<ClientPortForwardEvent>();
    let mut pf_forwards: HashMap<u32, ClientPortForwardState> = HashMap::new();
    let mut pf_channels: HashMap<u32, (u32, mpsc::UnboundedSender<Bytes>)> = HashMap::new(); // channel_id -> (forward_id, writer_tx)
    let next_pf_channel_id = std::sync::Arc::new(std::sync::atomic::AtomicU32::new(0));

    loop {
        tokio::select! {
            ready = async_stdin.readable() => {
                let mut guard = ready?;
                match guard.try_io(|inner| inner.get_ref().read(buf)) {
                    Ok(Ok(0)) => {
                        debug!("stdin EOF");
                        return Ok(Some(0));
                    }
                    Ok(Ok(n)) => {
                        debug!(len = n, "stdin → socket");
                        if let Some(ref mut esc) = escape {
                            for action in esc.process(&buf[..n]) {
                                match action {
                                    EscapeAction::Data(data) => {
                                        if !timed_send(framed, Frame::Data(Bytes::from(data))).await {
                                            return Ok(None);
                                        }
                                    }
                                    EscapeAction::Detach => {
                                        write_stdout(status_msg("detached").as_bytes())?;
                                        return Ok(Some(0));
                                    }
                                    EscapeAction::Suspend => {
                                        suspend(raw_guard, nb_guard)?;
                                        // Re-sync terminal size after resume
                                        let (cols, rows) = get_terminal_size();
                                        if !timed_send(framed, Frame::Resize { cols, rows }).await {
                                            return Ok(None);
                                        }
                                    }
                                    EscapeAction::Help => {
                                        write_stdout(ESCAPE_HELP)?;
                                    }
                                }
                            }
                        } else if !timed_send(framed, Frame::Data(Bytes::copy_from_slice(&buf[..n]))).await {
                            return Ok(None);
                        }
                    }
                    Ok(Err(e)) => return Err(e.into()),
                    Err(_would_block) => continue,
                }
            }

            frame = framed.next() => {
                match frame {
                    Some(Ok(Frame::Data(data))) => {
                        debug!(len = data.len(), "socket → stdout");
                        write_stdout(&data)?;
                    }
                    Some(Ok(Frame::Pong)) => {
                        debug!("pong received");
                        last_pong = Instant::now();
                    }
                    Some(Ok(Frame::Exit { code })) => {
                        debug!(code, "server sent exit");
                        return Ok(Some(code));
                    }
                    Some(Ok(Frame::Detached)) => {
                        info!("detached by another client");
                        agent_channels.clear();
                        drop(tunnel_writer.take());
                        if let Some(handle) = tunnel_listener.take() {
                            handle.abort();
                        }
                        for (_, pf) in pf_forwards.drain() {
                            if let Some(h) = pf.listener_handle {
                                h.abort();
                            }
                        }
                        pf_channels.clear();
                        write_stdout(status_msg("detached").as_bytes())?;
                        return Ok(Some(0));
                    }
                    Some(Ok(Frame::AgentOpen { channel_id })) => {
                        if let Some(sock_path) = agent_socket {
                            match tokio::net::UnixStream::connect(sock_path).await {
                                Ok(stream) => {
                                    let (read_half, write_half) = stream.into_split();
                                    let data_tx = agent_event_tx.clone();
                                    let close_tx = agent_event_tx.clone();
                                    let writer_tx = crate::spawn_channel_relay(
                                        channel_id,
                                        read_half,
                                        write_half,
                                        move |id, data| data_tx.send(AgentEvent::Data { channel_id: id, data }).is_ok(),
                                        move |id| { let _ = close_tx.send(AgentEvent::Closed { channel_id: id }); },
                                    );
                                    agent_channels.insert(channel_id, writer_tx);
                                }
                                Err(e) => {
                                    debug!("failed to connect to local agent: {e}");
                                    let _ = timed_send(framed, Frame::AgentClose { channel_id }).await;
                                }
                            }
                        } else {
                            let _ = timed_send(framed, Frame::AgentClose { channel_id }).await;
                        }
                    }
                    Some(Ok(Frame::AgentData { channel_id, data })) => {
                        if let Some(tx) = agent_channels.get(&channel_id) {
                            let _ = tx.send(data);
                        }
                    }
                    Some(Ok(Frame::AgentClose { channel_id })) => {
                        agent_channels.remove(&channel_id);
                    }
                    Some(Ok(Frame::OpenUrl { url })) => {
                        if url.starts_with("http://") || url.starts_with("https://") {
                            debug!("opening URL locally: {url}");
                            let cmd = if cfg!(target_os = "macos") { "open" } else { "xdg-open" };
                            let _ = std::process::Command::new(cmd)
                                .arg("--")
                                .arg(&url)
                                .stdin(std::process::Stdio::null())
                                .stdout(std::process::Stdio::null())
                                .stderr(std::process::Stdio::null())
                                .spawn();
                        } else {
                            debug!("rejected non-http(s) URL: {url}");
                        }
                    }
                    Some(Ok(Frame::TunnelListen { port })) => {
                        if !oauth_redirect {
                            debug!(port, "tunnel: oauth-redirect disabled, declining");
                            let _ = timed_send(framed, Frame::TunnelClose).await;
                        } else {
                            // Bind synchronously to guarantee port is ready before OpenUrl
                            match std::net::TcpListener::bind(("127.0.0.1", port)) {
                                Ok(std_listener) => {
                                    debug!(port, "tunnel: bound local port");
                                    std_listener.set_nonblocking(true).ok();
                                    let listener = tokio::net::TcpListener::from_std(std_listener).unwrap();
                                    let tx = tunnel_event_tx.clone();
                                    let timeout = oauth_timeout;
                                    tunnel_listener = Some(tokio::spawn(async move {
                                        let accept = tokio::time::timeout(
                                            Duration::from_secs(timeout),
                                            listener.accept(),
                                        ).await;
                                        match accept {
                                            Ok(Ok((stream, _))) => {
                                                let (mut read_half, write_half) = stream.into_split();
                                                let (writer_tx, mut writer_rx) = mpsc::unbounded_channel::<Bytes>();

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

                                                let _ = tx.send(ClientTunnelEvent::Accepted(writer_tx));

                                                // Reader task: TCP -> events
                                                let mut buf = vec![0u8; 4096];
                                                loop {
                                                    use tokio::io::AsyncReadExt;
                                                    match read_half.read(&mut buf).await {
                                                        Ok(0) | Err(_) => {
                                                            let _ = tx.send(ClientTunnelEvent::Closed);
                                                            break;
                                                        }
                                                        Ok(n) => {
                                                            let data = Bytes::copy_from_slice(&buf[..n]);
                                                            if tx.send(ClientTunnelEvent::Data(data)).is_err() {
                                                                break;
                                                            }
                                                        }
                                                    }
                                                }
                                            }
                                            _ => {
                                                debug!(port, "tunnel: accept timed out or failed");
                                                let _ = tx.send(ClientTunnelEvent::Closed);
                                            }
                                        }
                                    }));
                                }
                                Err(e) => {
                                    debug!(port, "tunnel: bind failed: {e}");
                                    let _ = timed_send(framed, Frame::TunnelClose).await;
                                }
                            }
                        }
                    }
                    Some(Ok(Frame::SendOffer { file_count, total_bytes })) => {
                        let size_str = format_size(total_bytes);
                        let s = if file_count == 1 { "" } else { "s" };
                        write_stdout(status_msg(&format!("gritty: receiving {file_count} file{s} ({size_str})")).as_bytes())?;
                    }
                    Some(Ok(Frame::SendDone)) => {
                        write_stdout(success_msg("gritty: transfer complete").as_bytes())?;
                    }
                    Some(Ok(Frame::SendCancel { reason })) => {
                        write_stdout(error_msg(&format!("gritty: transfer cancelled: {reason}")).as_bytes())?;
                    }
                    Some(Ok(Frame::TunnelData(data))) => {
                        if let Some(ref tx) = tunnel_writer {
                            let _ = tx.send(data);
                        }
                    }
                    Some(Ok(Frame::TunnelClose)) => {
                        tunnel_writer = None;
                        if let Some(handle) = tunnel_listener.take() {
                            handle.abort();
                        }
                    }
                    // Port forward: server asks client to bind a port (remote-fwd)
                    Some(Ok(Frame::PortForwardListen { forward_id, listen_port, target_port })) => {
                        match std::net::TcpListener::bind(("127.0.0.1", listen_port)) {
                            Ok(std_listener) => {
                                debug!(forward_id, listen_port, "port forward: bound local port");
                                std_listener.set_nonblocking(true).ok();
                                let listener = tokio::net::TcpListener::from_std(std_listener).unwrap();
                                let tx = pf_event_tx.clone();
                                let nid = next_pf_channel_id.clone();
                                let handle = tokio::spawn(async move {
                                    loop {
                                        let (stream, _) = match listener.accept().await {
                                            Ok(conn) => conn,
                                            Err(_) => break,
                                        };
                                        let channel_id = nid.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                                        let (read_half, write_half) = stream.into_split();
                                        let data_tx = tx.clone();
                                        let close_tx = tx.clone();
                                        let writer_tx = crate::spawn_channel_relay(
                                            channel_id,
                                            read_half,
                                            write_half,
                                            move |id, data| data_tx.send(ClientPortForwardEvent::Data { channel_id: id, data }).is_ok(),
                                            move |id| { let _ = close_tx.send(ClientPortForwardEvent::Closed { channel_id: id }); },
                                        );
                                        if tx.send(ClientPortForwardEvent::Accepted { forward_id, channel_id, writer_tx }).is_err() {
                                            break;
                                        }
                                    }
                                });
                                pf_forwards.insert(forward_id, ClientPortForwardState {
                                    listener_handle: Some(handle),
                                    target_port,
                                });
                                if !timed_send(framed, Frame::PortForwardReady { forward_id }).await {
                                    return Ok(None);
                                }
                            }
                            Err(e) => {
                                debug!(forward_id, listen_port, "port forward: bind failed: {e}");
                                let _ = timed_send(framed, Frame::PortForwardStop { forward_id }).await;
                            }
                        }
                    }
                    // Port forward: new TCP connection from server side (local-fwd)
                    Some(Ok(Frame::PortForwardOpen { forward_id, channel_id, target_port })) => {
                        if pf_forwards.contains_key(&forward_id) || forward_id == u32::MAX {
                            // forward_id == u32::MAX is a "don't track" sentinel for local-fwd
                            match tokio::net::TcpStream::connect(("127.0.0.1", target_port)).await {
                                Ok(stream) => {
                                    let (read_half, write_half) = stream.into_split();
                                    let data_tx = pf_event_tx.clone();
                                    let close_tx = pf_event_tx.clone();
                                    let writer_tx = crate::spawn_channel_relay(
                                        channel_id,
                                        read_half,
                                        write_half,
                                        move |id, data| data_tx.send(ClientPortForwardEvent::Data { channel_id: id, data }).is_ok(),
                                        move |id| { let _ = close_tx.send(ClientPortForwardEvent::Closed { channel_id: id }); },
                                    );
                                    pf_channels.insert(channel_id, (forward_id, writer_tx));
                                }
                                Err(e) => {
                                    debug!(channel_id, target_port, "pf connect failed: {e}");
                                    let _ = timed_send(framed, Frame::PortForwardClose { channel_id }).await;
                                }
                            }
                        }
                    }
                    // Port forward: channel data from server
                    Some(Ok(Frame::PortForwardData { channel_id, data })) => {
                        if let Some((_, tx)) = pf_channels.get(&channel_id) {
                            let _ = tx.send(data);
                        }
                    }
                    // Port forward: channel closed by server
                    Some(Ok(Frame::PortForwardClose { channel_id })) => {
                        pf_channels.remove(&channel_id);
                    }
                    // Port forward: teardown from server
                    Some(Ok(Frame::PortForwardStop { forward_id })) => {
                        if let Some(pf) = pf_forwards.remove(&forward_id) {
                            if let Some(h) = pf.listener_handle {
                                h.abort();
                            }
                        }
                        // Remove channels belonging to this forward
                        pf_channels.retain(|_, (fid, _)| *fid != forward_id);
                    }
                    Some(Ok(_)) => {} // ignore control/resize frames
                    Some(Err(e)) => {
                        debug!("server connection error: {e}");
                        return Ok(None);
                    }
                    None => {
                        debug!("server disconnected");
                        return Ok(None);
                    }
                }
            }

            // Agent events from local agent connections
            event = agent_event_rx.recv() => {
                match event {
                    Some(AgentEvent::Data { channel_id, data }) => {
                        if agent_channels.contains_key(&channel_id)
                            && !timed_send(framed, Frame::AgentData { channel_id, data }).await
                        {
                            return Ok(None);
                        }
                    }
                    Some(AgentEvent::Closed { channel_id }) => {
                        if agent_channels.remove(&channel_id).is_some()
                            && !timed_send(framed, Frame::AgentClose { channel_id }).await
                        {
                            return Ok(None);
                        }
                    }
                    None => {} // no more agent tasks
                }
            }

            // Tunnel events from local TCP listener/connection
            event = tunnel_event_rx.recv() => {
                match event {
                    Some(ClientTunnelEvent::Accepted(writer_tx)) => {
                        tunnel_writer = Some(writer_tx);
                        if !timed_send(framed, Frame::TunnelOpen).await {
                            return Ok(None);
                        }
                    }
                    Some(ClientTunnelEvent::Data(data)) => {
                        if !timed_send(framed, Frame::TunnelData(data)).await {
                            return Ok(None);
                        }
                    }
                    Some(ClientTunnelEvent::Closed) => {
                        tunnel_writer = None;
                        if let Some(handle) = tunnel_listener.take() {
                            handle.abort();
                        }
                        if !timed_send(framed, Frame::TunnelClose).await {
                            return Ok(None);
                        }
                    }
                    None => {}
                }
            }

            // Port forward events from local TCP listeners/connections
            event = pf_event_rx.recv() => {
                match event {
                    Some(ClientPortForwardEvent::Accepted { forward_id, channel_id, writer_tx }) => {
                        if let Some(pf) = pf_forwards.get(&forward_id) {
                            pf_channels.insert(channel_id, (forward_id, writer_tx));
                            if !timed_send(framed, Frame::PortForwardOpen {
                                forward_id, channel_id, target_port: pf.target_port,
                            }).await {
                                return Ok(None);
                            }
                        }
                    }
                    Some(ClientPortForwardEvent::Data { channel_id, data }) => {
                        if pf_channels.contains_key(&channel_id)
                            && !timed_send(framed, Frame::PortForwardData { channel_id, data }).await
                        {
                            return Ok(None);
                        }
                    }
                    Some(ClientPortForwardEvent::Closed { channel_id }) => {
                        if pf_channels.remove(&channel_id).is_some()
                            && !timed_send(framed, Frame::PortForwardClose { channel_id }).await
                        {
                            return Ok(None);
                        }
                    }
                    None => {}
                }
            }

            _ = sigwinch.recv() => {
                let (cols, rows) = get_terminal_size();
                debug!(cols, rows, "SIGWINCH → resize");
                if !timed_send(framed, Frame::Resize { cols, rows }).await {
                    return Ok(None);
                }
            }

            _ = heartbeat_interval.tick() => {
                if last_pong.elapsed() > HEARTBEAT_TIMEOUT {
                    debug!("heartbeat timeout");
                    return Ok(None);
                }
                if !timed_send(framed, Frame::Ping).await {
                    return Ok(None);
                }
            }

            _ = sigterm.recv() => {
                debug!("SIGTERM received, exiting");
                return Ok(Some(1));
            }

            _ = sighup.recv() => {
                debug!("SIGHUP received, exiting");
                return Ok(Some(1));
            }
        }
    }
}

#[allow(clippy::too_many_arguments)]
pub async fn run(
    session: &str,
    mut framed: Framed<UnixStream, FrameCodec>,
    redraw: bool,
    ctl_path: &Path,
    env_vars: Vec<(String, String)>,
    no_escape: bool,
    forward_agent: bool,
    forward_open: bool,
    oauth_redirect: bool,
    oauth_timeout: u64,
) -> anyhow::Result<i32> {
    let stdin = io::stdin();
    let stdin_fd = stdin.as_fd();
    // Safety: stdin lives for the duration of the program
    let stdin_borrowed: BorrowedFd<'static> =
        unsafe { BorrowedFd::borrow_raw(stdin_fd.as_raw_fd()) };
    let raw_guard = RawModeGuard::enter(stdin_borrowed)?;

    // Set stdin to non-blocking for AsyncFd — guard restores on drop.
    // Declared BEFORE async_stdin so it drops AFTER AsyncFd (reverse drop order).
    let nb_guard = NonBlockGuard::set(stdin_borrowed)?;
    let async_stdin = AsyncFd::new(io::stdin())?;
    let mut sigwinch = signal(SignalKind::window_change())?;
    let mut buf = vec![0u8; 4096];
    let mut current_redraw = redraw;
    let mut current_env = env_vars;
    let mut escape = if no_escape { None } else { Some(EscapeProcessor::new()) };
    let agent_socket = if forward_agent { std::env::var("SSH_AUTH_SOCK").ok() } else { None };

    loop {
        let result = if send_init_frames(
            &mut framed,
            &current_env,
            forward_agent,
            agent_socket.as_deref(),
            forward_open,
            current_redraw,
        )
        .await
        {
            relay(
                &mut framed,
                &async_stdin,
                &mut sigwinch,
                &mut buf,
                escape.as_mut(),
                &raw_guard,
                &nb_guard,
                agent_socket.as_deref(),
                oauth_redirect,
                oauth_timeout,
            )
            .await?
        } else {
            None
        };
        match result {
            Some(code) => return Ok(code),
            None => {
                // Env vars only sent on first connection; clear for reconnect
                current_env.clear();
                // Disconnected — try to reconnect
                write_stdout(status_msg("reconnecting...").as_bytes())?;

                loop {
                    // Race sleep against stdin so Ctrl-C is instant
                    tokio::select! {
                        _ = tokio::time::sleep(Duration::from_secs(1)) => {}
                        _ = async_stdin.readable() => {
                            let mut peek = [0u8; 1];
                            match async_stdin.get_ref().read(&mut peek) {
                                Ok(1) if peek[0] == 0x03 => {
                                    write_stdout(b"\r\n")?;
                                    return Ok(1);
                                }
                                _ => {}
                            }
                            continue;
                        }
                    }

                    let stream = match UnixStream::connect(ctl_path).await {
                        Ok(s) => s,
                        Err(_) => continue,
                    };

                    let mut new_framed = Framed::new(stream, FrameCodec);
                    if crate::handshake(&mut new_framed).await.is_err() {
                        continue;
                    }
                    if new_framed
                        .send(Frame::Attach { session: session.to_string() })
                        .await
                        .is_err()
                    {
                        continue;
                    }

                    match new_framed.next().await {
                        Some(Ok(Frame::Ok)) => {
                            write_stdout(success_msg("reconnected").as_bytes())?;
                            framed = new_framed;
                            current_redraw = true;
                            break;
                        }
                        Some(Ok(Frame::Error { message })) => {
                            write_stdout(
                                error_msg(&format!("session gone: {message}")).as_bytes(),
                            )?;
                            return Ok(1);
                        }
                        _ => continue,
                    }
                }
            }
        }
    }
}

/// Read-only tail of a session's PTY output.
/// No raw mode, no stdin, no escape processing, no forwarding.
/// Ctrl-C triggers clean exit with terminal reset.
pub async fn tail(
    session: &str,
    mut framed: Framed<UnixStream, FrameCodec>,
    ctl_path: &Path,
) -> anyhow::Result<i32> {
    // Suppress stdin echo — tail is read-only. Guard restores on drop.
    let stdin_fd = unsafe { BorrowedFd::borrow_raw(libc::STDIN_FILENO) };
    let _input_guard = SuppressInputGuard::enter(stdin_fd).ok();

    // Drain stdin in background, ring bell on first keystroke
    tokio::task::spawn_blocking(|| {
        let mut buf = [0u8; 64];
        let mut belled = false;
        loop {
            match io::stdin().read(&mut buf) {
                Ok(0) | Err(_) => break,
                Ok(_) if !belled => {
                    let _ = io::stderr().write_all(b"\x07");
                    let _ = io::stderr().flush();
                    belled = true;
                }
                _ => {}
            }
        }
    });

    let mut heartbeat_interval = tokio::time::interval(HEARTBEAT_INTERVAL);
    heartbeat_interval.reset();
    let mut last_pong = Instant::now();
    let mut sigint = signal(SignalKind::interrupt())?;
    let mut sigterm = signal(SignalKind::terminate())?;
    let mut sighup = signal(SignalKind::hangup())?;

    let code = 'outer: loop {
        let result = 'relay: loop {
            tokio::select! {
                frame = framed.next() => {
                    match frame {
                        Some(Ok(Frame::Data(data))) => {
                            write_stdout(&data)?;
                        }
                        Some(Ok(Frame::Pong)) => {
                            last_pong = Instant::now();
                        }
                        Some(Ok(Frame::Exit { code })) => {
                            break 'relay Some(code);
                        }
                        Some(Ok(_)) => {}
                        Some(Err(e)) => {
                            debug!("tail connection error: {e}");
                            break 'relay None;
                        }
                        None => {
                            debug!("tail server disconnected");
                            break 'relay None;
                        }
                    }
                }
                _ = heartbeat_interval.tick() => {
                    if last_pong.elapsed() > HEARTBEAT_TIMEOUT {
                        debug!("tail heartbeat timeout");
                        break 'relay None;
                    }
                    if framed.send(Frame::Ping).await.is_err() {
                        break 'relay None;
                    }
                }
                _ = sigint.recv() => {
                    break 'outer 0;
                }
                _ = sigterm.recv() => {
                    break 'outer 1;
                }
                _ = sighup.recv() => {
                    break 'outer 1;
                }
            }
        };

        match result {
            Some(code) => break code,
            None => {
                eprintln!("\x1b[2;33m[reconnecting...]\x1b[0m");
                loop {
                    tokio::time::sleep(Duration::from_secs(1)).await;

                    let stream = match UnixStream::connect(ctl_path).await {
                        Ok(s) => s,
                        Err(_) => continue,
                    };

                    let mut new_framed = Framed::new(stream, FrameCodec);
                    if crate::handshake(&mut new_framed).await.is_err() {
                        continue;
                    }
                    if new_framed.send(Frame::Tail { session: session.to_string() }).await.is_err()
                    {
                        continue;
                    }

                    match new_framed.next().await {
                        Some(Ok(Frame::Ok)) => {
                            eprintln!("\x1b[32m[reconnected]\x1b[0m");
                            framed = new_framed;
                            heartbeat_interval.reset();
                            last_pong = Instant::now();
                            break;
                        }
                        Some(Ok(Frame::Error { message })) => {
                            eprintln!("\x1b[31m[session gone: {message}]\x1b[0m");
                            break 'outer 1;
                        }
                        _ => continue,
                    }
                }
            }
        }
    };

    // Reset terminal state: clear attributes and show cursor.
    // PTY output may have left colors/bold set or cursor hidden.
    let _ = write_stdout(b"\x1b[0m\x1b[?25h");
    Ok(code)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normal_passthrough() {
        let mut ep = EscapeProcessor::new();
        // No newlines — after initial AfterNewline, 'h' transitions to Normal
        let actions = ep.process(b"hello");
        assert_eq!(actions, vec![EscapeAction::Data(b"hello".to_vec())]);
    }

    #[test]
    fn tilde_after_newline_detach() {
        let mut ep = EscapeProcessor { state: EscapeState::Normal };
        let actions = ep.process(b"\n~.");
        assert_eq!(actions, vec![EscapeAction::Data(b"\n".to_vec()), EscapeAction::Detach,]);
    }

    #[test]
    fn tilde_after_cr_detach() {
        let mut ep = EscapeProcessor { state: EscapeState::Normal };
        let actions = ep.process(b"\r~.");
        assert_eq!(actions, vec![EscapeAction::Data(b"\r".to_vec()), EscapeAction::Detach,]);
    }

    #[test]
    fn tilde_not_after_newline() {
        let mut ep = EscapeProcessor { state: EscapeState::Normal };
        let actions = ep.process(b"a~.");
        assert_eq!(actions, vec![EscapeAction::Data(b"a~.".to_vec())]);
    }

    #[test]
    fn initial_state_detach() {
        let mut ep = EscapeProcessor::new();
        let actions = ep.process(b"~.");
        assert_eq!(actions, vec![EscapeAction::Detach]);
    }

    #[test]
    fn tilde_suspend() {
        let mut ep = EscapeProcessor { state: EscapeState::Normal };
        let actions = ep.process(b"\n~\x1a");
        assert_eq!(actions, vec![EscapeAction::Data(b"\n".to_vec()), EscapeAction::Suspend,]);
    }

    #[test]
    fn tilde_help() {
        let mut ep = EscapeProcessor { state: EscapeState::Normal };
        let actions = ep.process(b"\n~?");
        assert_eq!(actions, vec![EscapeAction::Data(b"\n".to_vec()), EscapeAction::Help,]);
    }

    #[test]
    fn double_tilde() {
        let mut ep = EscapeProcessor { state: EscapeState::Normal };
        let actions = ep.process(b"\n~~");
        assert_eq!(
            actions,
            vec![EscapeAction::Data(b"\n".to_vec()), EscapeAction::Data(b"~".to_vec()),]
        );
        assert_eq!(ep.state, EscapeState::Normal);
    }

    #[test]
    fn tilde_unknown_char() {
        let mut ep = EscapeProcessor { state: EscapeState::Normal };
        let actions = ep.process(b"\n~x");
        assert_eq!(
            actions,
            vec![EscapeAction::Data(b"\n".to_vec()), EscapeAction::Data(b"~x".to_vec()),]
        );
    }

    #[test]
    fn split_across_reads() {
        let mut ep = EscapeProcessor { state: EscapeState::Normal };
        let a1 = ep.process(b"\n");
        assert_eq!(a1, vec![EscapeAction::Data(b"\n".to_vec())]);
        let a2 = ep.process(b"~");
        assert_eq!(a2, vec![]); // tilde buffered
        let a3 = ep.process(b".");
        assert_eq!(a3, vec![EscapeAction::Detach]);
    }

    #[test]
    fn split_tilde_then_normal() {
        let mut ep = EscapeProcessor { state: EscapeState::Normal };
        let a1 = ep.process(b"\n");
        assert_eq!(a1, vec![EscapeAction::Data(b"\n".to_vec())]);
        let a2 = ep.process(b"~");
        assert_eq!(a2, vec![]);
        let a3 = ep.process(b"a");
        assert_eq!(a3, vec![EscapeAction::Data(b"~a".to_vec())]);
    }

    #[test]
    fn multiple_escapes_one_buffer() {
        let mut ep = EscapeProcessor { state: EscapeState::Normal };
        let actions = ep.process(b"\n~?\n~.");
        assert_eq!(
            actions,
            vec![
                EscapeAction::Data(b"\n".to_vec()),
                EscapeAction::Help,
                EscapeAction::Data(b"\n".to_vec()),
                EscapeAction::Detach,
            ]
        );
    }

    #[test]
    fn consecutive_newlines() {
        let mut ep = EscapeProcessor { state: EscapeState::Normal };
        let actions = ep.process(b"\n\n\n~.");
        assert_eq!(actions, vec![EscapeAction::Data(b"\n\n\n".to_vec()), EscapeAction::Detach,]);
    }

    #[test]
    fn detach_stops_processing() {
        let mut ep = EscapeProcessor { state: EscapeState::Normal };
        let actions = ep.process(b"\n~.remaining");
        assert_eq!(actions, vec![EscapeAction::Data(b"\n".to_vec()), EscapeAction::Detach,]);
    }

    #[test]
    fn tilde_then_newline() {
        let mut ep = EscapeProcessor { state: EscapeState::Normal };
        let actions = ep.process(b"\n~\n");
        assert_eq!(
            actions,
            vec![EscapeAction::Data(b"\n".to_vec()), EscapeAction::Data(b"~\n".to_vec()),]
        );
        assert_eq!(ep.state, EscapeState::AfterNewline);
    }

    #[test]
    fn empty_input() {
        let mut ep = EscapeProcessor::new();
        let actions = ep.process(b"");
        assert_eq!(actions, vec![]);
    }

    #[test]
    fn only_tilde_buffered() {
        let mut ep = EscapeProcessor { state: EscapeState::Normal };
        let a1 = ep.process(b"\n~");
        assert_eq!(a1, vec![EscapeAction::Data(b"\n".to_vec())]);
        assert_eq!(ep.state, EscapeState::AfterTilde);
        let a2 = ep.process(b".");
        assert_eq!(a2, vec![EscapeAction::Detach]);
    }
}
