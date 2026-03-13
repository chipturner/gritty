use crate::protocol::{Frame, FrameCodec};
use bytes::Bytes;
use futures_util::{SinkExt, StreamExt};
use nix::sys::termios::{self, FlushArg, LocalFlags, SetArg, SpecialCharacterIndices, Termios};
use std::collections::HashMap;
use std::io::{self, Read, Write};
use std::ops::ControlFlow;
use std::os::fd::{AsFd, AsRawFd, BorrowedFd};
use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicU32, Ordering};
use std::time::Duration;
use tokio::io::unix::AsyncFd;
use tokio::net::UnixStream;
use tokio::signal::unix::{SignalKind, signal};
use tokio::sync::mpsc;
use tokio::time::Instant;

/// Outcome from a client relay loop iteration.
enum RelayExit {
    /// Shell or server reported an exit code (or detach/signal).
    Exit(i32),
    /// Server disconnected -- caller should reconnect.
    Disconnected,
}
use tokio_util::codec::Framed;
use tracing::{debug, info};

// --- Escape sequence processing (SSH-style ~. detach, ~^Z suspend, ~? help) ---

const ESCAPE_HELP: &[u8] = b"\r\nSupported escape sequences:\r\n\
    ~.  - detach from session\r\n\
    ~R  - force reconnect\r\n\
    ~^Z - suspend client\r\n\
    ~#  - session status and RTT\r\n\
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
    Reconnect,
    Suspend,
    Status,
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
                        b'R' => {
                            if !data_buf.is_empty() {
                                actions.push(EscapeAction::Data(std::mem::take(&mut data_buf)));
                            }
                            actions.push(EscapeAction::Reconnect);
                            self.state = EscapeState::Normal;
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
                        b'#' => {
                            if !data_buf.is_empty() {
                                actions.push(EscapeAction::Data(std::mem::take(&mut data_buf)));
                            }
                            actions.push(EscapeAction::Status);
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
        // TCSADRAIN (not TCSAFLUSH) so keystrokes typed during connection
        // setup are preserved and forwarded to the session.
        termios::tcsetattr(fd, SetArg::TCSADRAIN, &raw)?;
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

/// Write all bytes to stdout asynchronously via AsyncFd.
/// Used in relay mode where stdout is non-blocking (shares fd with stdin).
async fn write_stdout_async(fd: &AsyncFd<std::os::fd::OwnedFd>, data: &[u8]) -> io::Result<()> {
    let mut written = 0;
    while written < data.len() {
        let mut guard = fd.writable().await?;
        match guard
            .try_io(|inner| nix::unistd::write(inner, &data[written..]).map_err(io::Error::from))
        {
            Ok(Ok(0)) => {
                return Err(io::Error::new(io::ErrorKind::WriteZero, "stdout closed"));
            }
            Ok(Ok(n)) => written += n,
            Ok(Err(e)) => return Err(e),
            Err(_would_block) => continue,
        }
    }
    Ok(())
}

/// Format a byte count as a human-readable size string.
pub fn format_size(bytes: u64) -> String {
    humansize::format_size(bytes, humansize::BINARY)
}

fn status_msg(text: &str) -> String {
    format!("\r\n\x1b[2;33m\u{25b8} {text}\x1b[0m\r\n")
}

fn success_msg(text: &str) -> String {
    format!("\r\n\x1b[32m\u{25b8} {text}\x1b[0m\r\n")
}

fn error_msg(text: &str) -> String {
    format!("\r\n\x1b[31m\u{25b8} {text}\x1b[0m\r\n")
}

fn get_terminal_size() -> (u16, u16) {
    terminal_size::terminal_size().map(|(w, h)| (w.0, h.0)).unwrap_or((80, 24))
}

/// Write data to the system clipboard.
fn clipboard_set(data: &[u8]) {
    use std::process::{Command, Stdio};
    let programs: &[&[&str]] = if cfg!(target_os = "macos") {
        &[&["pbcopy"]]
    } else {
        &[&["wl-copy"], &["xclip", "-selection", "clipboard"], &["xsel", "--clipboard", "--input"]]
    };
    for prog in programs {
        if let Ok(mut child) = Command::new(prog[0])
            .args(&prog[1..])
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
        {
            if let Some(ref mut stdin) = child.stdin {
                use std::io::Write;
                let _ = stdin.write_all(data);
            }
            let _ = child.wait();
            return;
        }
    }
    debug!("no clipboard program available");
}

/// Read data from the system clipboard.
fn clipboard_get() -> Option<Vec<u8>> {
    use std::process::{Command, Stdio};
    let programs: &[&[&str]] = if cfg!(target_os = "macos") {
        &[&["pbpaste"]]
    } else {
        &[
            &["wl-paste", "--no-newline"],
            &["xclip", "-selection", "clipboard", "-o"],
            &["xsel", "--clipboard", "--output"],
        ]
    };
    for prog in programs {
        if let Ok(output) = Command::new(prog[0])
            .args(&prog[1..])
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .output()
        {
            if output.status.success() {
                return Some(output.stdout);
            }
        }
    }
    debug!("no clipboard program available");
    None
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

const DEFAULT_HEARTBEAT_INTERVAL: Duration = Duration::from_secs(5);
const DEFAULT_HEARTBEAT_TIMEOUT: Duration = Duration::from_secs(15);

/// Events from local agent connection tasks to the relay loop.
enum AgentEvent {
    Data { channel_id: u32, data: Bytes },
    Closed { channel_id: u32 },
}

/// Events from the tunnel TCP listener/connection to the relay loop.
enum ClientTunnelEvent {
    Accepted { channel_id: u32, writer_tx: mpsc::Sender<Bytes> },
    Data { channel_id: u32, data: Bytes },
    Closed { channel_id: u32 },
}

/// Events from port forward TCP acceptors/connections on the client side.
enum ClientPortForwardEvent {
    Accepted { forward_id: u32, channel_id: u32, writer_tx: mpsc::Sender<Bytes> },
    Data { channel_id: u32, data: Bytes },
    Closed { channel_id: u32 },
}

/// Per-forward state on the client side.
struct ClientPortForwardState {
    listener_handle: Option<tokio::task::JoinHandle<()>>,
    target_port: u16,
}

/// Grouped state for agent channel management on the client side.
struct ClientAgentState {
    channels: HashMap<u32, mpsc::Sender<Bytes>>,
}

impl ClientAgentState {
    fn new() -> Self {
        Self { channels: HashMap::new() }
    }

    fn teardown(&mut self) {
        self.channels.clear();
    }
}

/// Grouped state for the OAuth callback tunnel on the client side (multi-channel).
struct ClientTunnelState {
    listener: Option<tokio::task::JoinHandle<()>>,
    channels: HashMap<u32, mpsc::Sender<Bytes>>,
    next_channel_id: Arc<AtomicU32>,
}

impl ClientTunnelState {
    fn new() -> Self {
        Self {
            listener: None,
            channels: HashMap::new(),
            next_channel_id: Arc::new(AtomicU32::new(0)),
        }
    }

    fn teardown(&mut self) {
        self.channels.clear();
        if let Some(handle) = self.listener.take() {
            handle.abort();
        }
    }
}

/// Grouped state for TCP port forwarding on the client side.
struct ClientPortForwardTable {
    forwards: HashMap<u32, ClientPortForwardState>,
    channels: HashMap<u32, (u32, mpsc::Sender<Bytes>)>,
    next_channel_id: std::sync::Arc<std::sync::atomic::AtomicU32>,
}

impl ClientPortForwardTable {
    fn new() -> Self {
        Self {
            forwards: HashMap::new(),
            channels: HashMap::new(),
            next_channel_id: std::sync::Arc::new(std::sync::atomic::AtomicU32::new(0)),
        }
    }

    fn teardown(&mut self) {
        for (_, fwd) in self.forwards.drain() {
            if let Some(h) = fwd.listener_handle {
                h.abort();
            }
        }
        self.channels.clear();
    }
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
    if !timed_send(framed, Frame::Env { vars: env_vars.to_vec() }).await {
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

/// `framed` is kept outside (passed to handlers) so `tokio::select!` can
/// poll `framed.next()` independently without conflicting borrows.
struct ClientRelay<'a> {
    async_stdout: &'a AsyncFd<std::os::fd::OwnedFd>,
    agent: &'a mut ClientAgentState,
    agent_event_tx: &'a mpsc::UnboundedSender<AgentEvent>,
    agent_socket: Option<&'a str>,
    tunnel: &'a mut ClientTunnelState,
    tunnel_event_tx: &'a mpsc::UnboundedSender<ClientTunnelEvent>,
    oauth_redirect: bool,
    oauth_timeout: u64,
    pf: &'a mut ClientPortForwardTable,
    pf_event_tx: &'a mpsc::UnboundedSender<ClientPortForwardEvent>,
    last_pong: &'a mut Instant,
    last_ping_sent: &'a mut Instant,
    last_rtt: &'a mut Option<Duration>,
    connected_at: Instant,
    bytes_relayed: &'a mut u64,
}

impl ClientRelay<'_> {
    async fn handle_server_frame(
        &mut self,
        framed: &mut Framed<UnixStream, FrameCodec>,
        frame: Option<Result<Frame, io::Error>>,
    ) -> Result<ControlFlow<RelayExit>, anyhow::Error> {
        match frame {
            Some(Ok(Frame::Data(data))) => {
                debug!(len = data.len(), "socket → stdout");
                *self.bytes_relayed += data.len() as u64;
                write_stdout_async(self.async_stdout, &data).await?;
            }
            Some(Ok(Frame::Pong)) => {
                *self.last_rtt = Some(self.last_ping_sent.elapsed());
                debug!(rtt_ms = self.last_rtt.unwrap().as_secs_f64() * 1000.0, "pong received");
                *self.last_pong = Instant::now();
            }
            Some(Ok(Frame::Exit { code })) => {
                debug!(code, "server sent exit");
                return Ok(ControlFlow::Break(RelayExit::Exit(code)));
            }
            Some(Ok(Frame::Detached)) => {
                info!("detached by another client");
                self.agent.teardown();
                self.tunnel.teardown();
                self.pf.teardown();
                write_stdout_async(self.async_stdout, status_msg("detached").as_bytes()).await?;
                return Ok(ControlFlow::Break(RelayExit::Exit(0)));
            }
            Some(Ok(Frame::AgentOpen { channel_id })) => {
                if let Some(sock_path) = self.agent_socket {
                    match tokio::net::UnixStream::connect(sock_path).await {
                        Ok(stream) => {
                            let (read_half, write_half) = stream.into_split();
                            let data_tx = self.agent_event_tx.clone();
                            let close_tx = self.agent_event_tx.clone();
                            let writer_tx = crate::spawn_channel_relay(
                                channel_id,
                                read_half,
                                write_half,
                                move |id, data| {
                                    data_tx.send(AgentEvent::Data { channel_id: id, data }).is_ok()
                                },
                                move |id| {
                                    let _ = close_tx.send(AgentEvent::Closed { channel_id: id });
                                },
                            );
                            self.agent.channels.insert(channel_id, writer_tx);
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
                if let Some(tx) = self.agent.channels.get(&channel_id) {
                    let _ = tx.send(data).await;
                }
            }
            Some(Ok(Frame::AgentClose { channel_id })) => {
                self.agent.channels.remove(&channel_id);
            }
            Some(Ok(Frame::OpenUrl { url })) => {
                if url.starts_with("http://") || url.starts_with("https://") {
                    debug!("opening URL locally: {url}");
                    tokio::task::spawn_blocking(move || {
                        let _ = opener::open(&url);
                    });
                } else {
                    debug!("rejected non-http(s) URL: {url}");
                }
            }
            Some(Ok(Frame::ClipboardSet { data })) => {
                debug!(len = data.len(), "clipboard set from remote");
                tokio::task::spawn_blocking(move || {
                    clipboard_set(&data);
                });
            }
            Some(Ok(Frame::ClipboardGet)) => {
                debug!("clipboard get requested by remote");
                let data = tokio::task::spawn_blocking(clipboard_get).await.ok().flatten();
                let data = data.unwrap_or_default();
                let _ = timed_send(framed, Frame::ClipboardData { data: Bytes::from(data) }).await;
            }
            Some(Ok(Frame::TunnelListen { port })) => {
                if !self.oauth_redirect {
                    debug!(port, "tunnel: oauth-redirect disabled, declining");
                    let _ = timed_send(framed, Frame::TunnelClose { channel_id: 0 }).await;
                } else {
                    // Bind synchronously to guarantee port is ready before OpenUrl
                    match std::net::TcpListener::bind(("127.0.0.1", port)) {
                        Ok(std_listener) => {
                            debug!(port, "tunnel: bound local port");
                            std_listener.set_nonblocking(true).ok();
                            let listener = tokio::net::TcpListener::from_std(std_listener).unwrap();
                            let tx = self.tunnel_event_tx.clone();
                            let timeout = self.oauth_timeout;
                            let next_id = Arc::clone(&self.tunnel.next_channel_id);
                            self.tunnel.listener = Some(tokio::spawn(async move {
                                let deadline =
                                    tokio::time::Instant::now() + Duration::from_secs(timeout);
                                loop {
                                    let accept =
                                        tokio::time::timeout_at(deadline, listener.accept()).await;
                                    match accept {
                                        Ok(Ok((stream, _))) => {
                                            let channel_id =
                                                next_id.fetch_add(1, Ordering::Relaxed);
                                            let (read_half, write_half) = stream.into_split();
                                            let (writer_tx, mut writer_rx) =
                                                mpsc::channel::<Bytes>(crate::CHANNEL_RELAY_BUFFER);

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

                                            let _ = tx.send(ClientTunnelEvent::Accepted {
                                                channel_id,
                                                writer_tx,
                                            });

                                            // Reader task: TCP -> events (spawned so we
                                            // can keep accepting new connections)
                                            let reader_tx = tx.clone();
                                            tokio::spawn(async move {
                                                use tokio::io::AsyncReadExt;
                                                let mut read_half = read_half;
                                                let mut buf = vec![0u8; 4096];
                                                loop {
                                                    match read_half.read(&mut buf).await {
                                                        Ok(0) | Err(_) => {
                                                            let _ = reader_tx.send(
                                                                ClientTunnelEvent::Closed {
                                                                    channel_id,
                                                                },
                                                            );
                                                            break;
                                                        }
                                                        Ok(n) => {
                                                            let data =
                                                                Bytes::copy_from_slice(&buf[..n]);
                                                            if reader_tx
                                                                .send(ClientTunnelEvent::Data {
                                                                    channel_id,
                                                                    data,
                                                                })
                                                                .is_err()
                                                            {
                                                                break;
                                                            }
                                                        }
                                                    }
                                                }
                                            });
                                        }
                                        _ => {
                                            debug!(port, "tunnel: accept timed out or failed");
                                            break;
                                        }
                                    }
                                }
                            }));
                        }
                        Err(e) => {
                            debug!(port, "tunnel: bind failed: {e}");
                            let _ = timed_send(framed, Frame::TunnelClose { channel_id: 0 }).await;
                        }
                    }
                }
            }
            Some(Ok(Frame::SendOffer { file_count, total_bytes })) => {
                let size_str = format_size(total_bytes);
                let s = if file_count == 1 { "" } else { "s" };
                write_stdout_async(
                    self.async_stdout,
                    status_msg(&format!("receiving {file_count} file{s} ({size_str})")).as_bytes(),
                )
                .await?;
            }
            Some(Ok(Frame::SendDone)) => {
                write_stdout_async(self.async_stdout, success_msg("transfer complete").as_bytes())
                    .await?;
            }
            Some(Ok(Frame::SendCancel { reason })) => {
                write_stdout_async(
                    self.async_stdout,
                    error_msg(&format!("transfer cancelled: {reason}")).as_bytes(),
                )
                .await?;
            }
            Some(Ok(Frame::TunnelData { channel_id, data })) => {
                if let Some(tx) = self.tunnel.channels.get(&channel_id) {
                    let _ = tx.send(data).await;
                }
            }
            Some(Ok(Frame::TunnelClose { channel_id })) => {
                self.tunnel.channels.remove(&channel_id);
            }
            // Port forward: server asks client to bind a port (remote-fwd)
            Some(Ok(Frame::PortForwardListen { forward_id, listen_port, target_port })) => {
                match std::net::TcpListener::bind(("127.0.0.1", listen_port)) {
                    Ok(std_listener) => {
                        debug!(forward_id, listen_port, "port forward: bound local port");
                        std_listener.set_nonblocking(true).ok();
                        let listener = tokio::net::TcpListener::from_std(std_listener).unwrap();
                        let tx = self.pf_event_tx.clone();
                        let nid = self.pf.next_channel_id.clone();
                        let handle = tokio::spawn(async move {
                            loop {
                                let (stream, _) = match listener.accept().await {
                                    Ok(conn) => conn,
                                    Err(_) => break,
                                };
                                let channel_id =
                                    nid.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                                let (read_half, write_half) = stream.into_split();
                                let data_tx = tx.clone();
                                let close_tx = tx.clone();
                                let writer_tx = crate::spawn_channel_relay(
                                    channel_id,
                                    read_half,
                                    write_half,
                                    move |id, data| {
                                        data_tx
                                            .send(ClientPortForwardEvent::Data {
                                                channel_id: id,
                                                data,
                                            })
                                            .is_ok()
                                    },
                                    move |id| {
                                        let _ = close_tx.send(ClientPortForwardEvent::Closed {
                                            channel_id: id,
                                        });
                                    },
                                );
                                if tx
                                    .send(ClientPortForwardEvent::Accepted {
                                        forward_id,
                                        channel_id,
                                        writer_tx,
                                    })
                                    .is_err()
                                {
                                    break;
                                }
                            }
                        });
                        self.pf.forwards.insert(
                            forward_id,
                            ClientPortForwardState { listener_handle: Some(handle), target_port },
                        );
                        if !timed_send(framed, Frame::PortForwardReady { forward_id }).await {
                            return Ok(ControlFlow::Break(RelayExit::Disconnected));
                        }
                    }
                    Err(e) => {
                        debug!(forward_id, listen_port, "port forward: bind failed: {e}");
                        let _ = timed_send(framed, Frame::PortForwardStop { forward_id }).await;
                    }
                }
            }
            // Port forward: new TCP connection from server side
            Some(Ok(Frame::PortForwardOpen { forward_id, channel_id, target_port })) => {
                match tokio::net::TcpStream::connect(("127.0.0.1", target_port)).await {
                    Ok(stream) => {
                        let (read_half, write_half) = stream.into_split();
                        let data_tx = self.pf_event_tx.clone();
                        let close_tx = self.pf_event_tx.clone();
                        let writer_tx = crate::spawn_channel_relay(
                            channel_id,
                            read_half,
                            write_half,
                            move |id, data| {
                                data_tx
                                    .send(ClientPortForwardEvent::Data { channel_id: id, data })
                                    .is_ok()
                            },
                            move |id| {
                                let _ = close_tx
                                    .send(ClientPortForwardEvent::Closed { channel_id: id });
                            },
                        );
                        self.pf.channels.insert(channel_id, (forward_id, writer_tx));
                    }
                    Err(e) => {
                        debug!(channel_id, target_port, "pf connect failed: {e}");
                        let _ = timed_send(framed, Frame::PortForwardClose { channel_id }).await;
                    }
                }
            }
            // Port forward: channel data from server
            Some(Ok(Frame::PortForwardData { channel_id, data })) => {
                if let Some((_, tx)) = self.pf.channels.get(&channel_id) {
                    let _ = tx.send(data).await;
                }
            }
            // Port forward: channel closed by server
            Some(Ok(Frame::PortForwardClose { channel_id })) => {
                self.pf.channels.remove(&channel_id);
            }
            // Port forward: teardown from server
            Some(Ok(Frame::PortForwardStop { forward_id })) => {
                if let Some(fwd) = self.pf.forwards.remove(&forward_id) {
                    if let Some(h) = fwd.listener_handle {
                        h.abort();
                    }
                }
                // Remove channels belonging to this forward
                self.pf.channels.retain(|_, (fid, _)| *fid != forward_id);
            }
            Some(Ok(_)) => {} // ignore control/resize frames
            Some(Err(e)) => {
                debug!("server connection error: {e}");
                return Ok(ControlFlow::Break(RelayExit::Disconnected));
            }
            None => {
                debug!("server disconnected");
                return Ok(ControlFlow::Break(RelayExit::Disconnected));
            }
        }
        Ok(ControlFlow::Continue(()))
    }

    async fn handle_agent_event(
        &mut self,
        framed: &mut Framed<UnixStream, FrameCodec>,
        event: Option<AgentEvent>,
    ) -> bool {
        match event {
            Some(AgentEvent::Data { channel_id, data }) => {
                if self.agent.channels.contains_key(&channel_id)
                    && !timed_send(framed, Frame::AgentData { channel_id, data }).await
                {
                    return false;
                }
            }
            Some(AgentEvent::Closed { channel_id }) => {
                if self.agent.channels.remove(&channel_id).is_some()
                    && !timed_send(framed, Frame::AgentClose { channel_id }).await
                {
                    return false;
                }
            }
            None => {} // no more agent tasks
        }
        true
    }

    async fn handle_tunnel_event(
        &mut self,
        framed: &mut Framed<UnixStream, FrameCodec>,
        event: Option<ClientTunnelEvent>,
    ) -> bool {
        match event {
            Some(ClientTunnelEvent::Accepted { channel_id, writer_tx }) => {
                self.tunnel.channels.insert(channel_id, writer_tx);
                if !timed_send(framed, Frame::TunnelOpen { channel_id }).await {
                    return false;
                }
            }
            Some(ClientTunnelEvent::Data { channel_id, data }) => {
                if !timed_send(framed, Frame::TunnelData { channel_id, data }).await {
                    return false;
                }
            }
            Some(ClientTunnelEvent::Closed { channel_id }) => {
                self.tunnel.channels.remove(&channel_id);
                if !timed_send(framed, Frame::TunnelClose { channel_id }).await {
                    return false;
                }
            }
            None => {}
        }
        true
    }

    async fn handle_pf_event(
        &mut self,
        framed: &mut Framed<UnixStream, FrameCodec>,
        event: Option<ClientPortForwardEvent>,
    ) -> bool {
        match event {
            Some(ClientPortForwardEvent::Accepted { forward_id, channel_id, writer_tx }) => {
                if let Some(fwd) = self.pf.forwards.get(&forward_id) {
                    let target_port = fwd.target_port;
                    self.pf.channels.insert(channel_id, (forward_id, writer_tx));
                    if !timed_send(
                        framed,
                        Frame::PortForwardOpen { forward_id, channel_id, target_port },
                    )
                    .await
                    {
                        return false;
                    }
                }
            }
            Some(ClientPortForwardEvent::Data { channel_id, data }) => {
                if self.pf.channels.contains_key(&channel_id)
                    && !timed_send(framed, Frame::PortForwardData { channel_id, data }).await
                {
                    return false;
                }
            }
            Some(ClientPortForwardEvent::Closed { channel_id }) => {
                if self.pf.channels.remove(&channel_id).is_some()
                    && !timed_send(framed, Frame::PortForwardClose { channel_id }).await
                {
                    return false;
                }
            }
            None => {}
        }
        true
    }
}

/// Relay between stdin/stdout and the framed socket.
#[allow(clippy::too_many_arguments)]
async fn relay(
    framed: &mut Framed<UnixStream, FrameCodec>,
    async_stdin: &AsyncFd<io::Stdin>,
    async_stdout: &AsyncFd<std::os::fd::OwnedFd>,
    sigwinch: &mut tokio::signal::unix::Signal,
    buf: &mut [u8],
    mut escape: Option<&mut EscapeProcessor>,
    raw_guard: &RawModeGuard,
    nb_guard: &NonBlockGuard,
    agent_socket: Option<&str>,
    oauth_redirect: bool,
    oauth_timeout: u64,
    session: &str,
    hb_interval: Duration,
    hb_timeout: Duration,
) -> anyhow::Result<RelayExit> {
    let mut sigterm = signal(SignalKind::terminate())?;
    let mut sighup = signal(SignalKind::hangup())?;

    let mut heartbeat_interval = tokio::time::interval(hb_interval);
    heartbeat_interval.reset(); // first tick is immediate otherwise; delay it
    let mut last_pong = Instant::now();
    let mut last_ping_sent = Instant::now();
    let mut last_rtt: Option<Duration> = None;

    // Agent channel management
    let mut agent = ClientAgentState::new();
    let (agent_event_tx, mut agent_event_rx) = mpsc::unbounded_channel::<AgentEvent>();

    // Tunnel state (reverse TCP tunnel for OAuth callbacks)
    let mut tunnel = ClientTunnelState::new();
    let (tunnel_event_tx, mut tunnel_event_rx) = mpsc::unbounded_channel::<ClientTunnelEvent>();

    // Port forward state
    let (pf_event_tx, mut pf_event_rx) = mpsc::unbounded_channel::<ClientPortForwardEvent>();
    let mut pf = ClientPortForwardTable::new();

    let mut bytes_relayed = 0u64;
    let mut relay = ClientRelay {
        async_stdout,
        agent: &mut agent,
        agent_event_tx: &agent_event_tx,
        agent_socket,
        tunnel: &mut tunnel,
        tunnel_event_tx: &tunnel_event_tx,
        oauth_redirect,
        oauth_timeout,
        pf: &mut pf,
        pf_event_tx: &pf_event_tx,
        last_pong: &mut last_pong,
        last_ping_sent: &mut last_ping_sent,
        last_rtt: &mut last_rtt,
        connected_at: Instant::now(),
        bytes_relayed: &mut bytes_relayed,
    };

    loop {
        tokio::select! {
            ready = async_stdin.readable() => {
                let mut guard = ready?;
                match guard.try_io(|inner| inner.get_ref().read(buf)) {
                    Ok(Ok(0)) => {
                        debug!("stdin EOF");
                        return Ok(RelayExit::Exit(0));
                    }
                    Ok(Ok(n)) => {
                        debug!(len = n, "stdin → socket");
                        if let Some(ref mut esc) = escape {
                            for action in esc.process(&buf[..n]) {
                                match action {
                                    EscapeAction::Data(data) => {
                                        if !timed_send(framed, Frame::Data(Bytes::from(data))).await {
                                            return Ok(RelayExit::Disconnected);
                                        }
                                    }
                                    EscapeAction::Detach => {
                                        write_stdout_async(async_stdout, status_msg("detached").as_bytes()).await?;
                                        return Ok(RelayExit::Exit(0));
                                    }
                                    EscapeAction::Reconnect => {
                                        write_stdout_async(async_stdout, status_msg("force reconnect").as_bytes()).await?;
                                        return Ok(RelayExit::Disconnected);
                                    }
                                    EscapeAction::Suspend => {
                                        suspend(raw_guard, nb_guard)?;
                                        // Re-sync terminal size after resume
                                        let (cols, rows) = get_terminal_size();
                                        if !timed_send(framed, Frame::Resize { cols, rows }).await {
                                            return Ok(RelayExit::Disconnected);
                                        }
                                    }
                                    EscapeAction::Status => {
                                        let rtt_str = match *relay.last_rtt {
                                            Some(d) => format!("{:.1}ms", d.as_secs_f64() * 1000.0),
                                            None => "n/a".to_string(),
                                        };
                                        let uptime = relay.connected_at.elapsed();
                                        let uptime_str = if uptime.as_secs() >= 3600 {
                                            format!(
                                                "{}h {}m {}s",
                                                uptime.as_secs() / 3600,
                                                (uptime.as_secs() % 3600) / 60,
                                                uptime.as_secs() % 60,
                                            )
                                        } else if uptime.as_secs() >= 60 {
                                            format!(
                                                "{}m {}s",
                                                uptime.as_secs() / 60,
                                                uptime.as_secs() % 60,
                                            )
                                        } else {
                                            format!("{}s", uptime.as_secs())
                                        };
                                        let bytes_str = format_size(*relay.bytes_relayed);
                                        let agent_info = if relay.agent_socket.is_some() {
                                            format!(
                                                "on ({} channels)",
                                                relay.agent.channels.len()
                                            )
                                        } else {
                                            "off".to_string()
                                        };
                                        let open_str = if relay.oauth_redirect { "on" } else { "off" };
                                        let mut pf_lines = Vec::new();
                                        for (&fwd_id, fwd) in &relay.pf.forwards {
                                            let ch_count = relay.pf.channels.values()
                                                .filter(|(fid, _)| *fid == fwd_id)
                                                .count();
                                            pf_lines.push(format!(
                                                "    :{} ({} connections)",
                                                fwd.target_port,
                                                ch_count,
                                            ));
                                        }
                                        let tunnel_str = if !relay.tunnel.channels.is_empty() {
                                            format!("active ({} channels)", relay.tunnel.channels.len())
                                        } else if relay.tunnel.listener.is_some() {
                                            "listening".to_string()
                                        } else {
                                            "idle".to_string()
                                        };
                                        let mut status = format!(
                                            "\r\n\x1b[2;33m[gritty status]\r\n\
                                             \x1b[0m\x1b[2m  session: {session}\r\n\
                                             \x1b[0m\x1b[2m  rtt: {rtt_str}\r\n\
                                             \x1b[0m\x1b[2m  connected: {uptime_str}\r\n\
                                             \x1b[0m\x1b[2m  bytes relayed: {bytes_str}\r\n\
                                             \x1b[0m\x1b[2m  agent forwarding: {agent_info}\r\n\
                                             \x1b[0m\x1b[2m  open forwarding: {open_str}\r\n\
                                             \x1b[0m\x1b[2m  oauth tunnel: {tunnel_str}\r\n",
                                        );
                                        for line in &pf_lines {
                                            status.push_str(&format!(
                                                "\x1b[0m\x1b[2m  port forward{line}\r\n"
                                            ));
                                        }
                                        status.push_str("\x1b[0m");
                                        write_stdout_async(
                                            async_stdout,
                                            status.as_bytes(),
                                        ).await?;
                                    }
                                    EscapeAction::Help => {
                                        write_stdout_async(async_stdout, ESCAPE_HELP).await?;
                                    }
                                }
                            }
                        } else if !timed_send(framed, Frame::Data(Bytes::copy_from_slice(&buf[..n]))).await {
                            return Ok(RelayExit::Disconnected);
                        }
                    }
                    Ok(Err(e)) => return Err(e.into()),
                    Err(_would_block) => continue,
                }
            }

            frame = framed.next() => {
                if let ControlFlow::Break(exit) = relay.handle_server_frame(framed, frame).await? {
                    return Ok(exit);
                }
            }

            event = agent_event_rx.recv() => {
                if !relay.handle_agent_event(framed, event).await {
                    return Ok(RelayExit::Disconnected);
                }
            }

            event = tunnel_event_rx.recv() => {
                if !relay.handle_tunnel_event(framed, event).await {
                    return Ok(RelayExit::Disconnected);
                }
            }

            event = pf_event_rx.recv() => {
                if !relay.handle_pf_event(framed, event).await {
                    return Ok(RelayExit::Disconnected);
                }
            }

            _ = sigwinch.recv() => {
                let (cols, rows) = get_terminal_size();
                debug!(cols, rows, "SIGWINCH → resize");
                if !timed_send(framed, Frame::Resize { cols, rows }).await {
                    return Ok(RelayExit::Disconnected);
                }
            }

            _ = heartbeat_interval.tick() => {
                if relay.last_pong.elapsed() > hb_timeout {
                    debug!("heartbeat timeout");
                    return Ok(RelayExit::Disconnected);
                }
                *relay.last_ping_sent = Instant::now();
                if !timed_send(framed, Frame::Ping).await {
                    return Ok(RelayExit::Disconnected);
                }
            }

            _ = sigterm.recv() => {
                debug!("SIGTERM received, exiting");
                return Ok(RelayExit::Exit(1));
            }

            _ = sighup.recv() => {
                debug!("SIGHUP received, exiting");
                return Ok(RelayExit::Exit(1));
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
    heartbeat_interval: u64,
    heartbeat_timeout: u64,
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
    // dup() stdout so we get an independent fd for AsyncFd (stdin may share the same fd).
    let stdout_fd = crate::security::checked_dup(io::stdout().as_raw_fd())?;
    let async_stdout = AsyncFd::new(stdout_fd)?;
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
                &async_stdout,
                &mut sigwinch,
                &mut buf,
                escape.as_mut(),
                &raw_guard,
                &nb_guard,
                agent_socket.as_deref(),
                oauth_redirect,
                oauth_timeout,
                session,
                Duration::from_secs(heartbeat_interval),
                Duration::from_secs(heartbeat_timeout),
            )
            .await?
        } else {
            RelayExit::Disconnected
        };
        match result {
            RelayExit::Exit(code) => return Ok(code),
            RelayExit::Disconnected => {
                current_env.clear();
                let reconnect_started = Instant::now();
                write_stdout_async(
                    &async_stdout,
                    b"\r\n\x1b[2;33m\xe2\x96\xb8 reconnecting... (Ctrl-C to abort)\x1b[0m",
                )
                .await?;

                loop {
                    // Race sleep against stdin so Ctrl-C is instant
                    tokio::select! {
                        _ = tokio::time::sleep(Duration::from_secs(1)) => {}
                        ready = async_stdin.readable() => {
                            let mut guard = ready?;
                            let mut peek = [0u8; 1];
                            match guard.try_io(|inner| inner.get_ref().read(&mut peek)) {
                                Ok(Ok(1)) if peek[0] == 0x03 => {
                                    write_stdout_async(&async_stdout, b"\r\n").await?;
                                    return Ok(1);
                                }
                                Ok(Ok(0)) | Ok(Err(_)) => {
                                    // stdin EOF or error -- terminal is gone
                                    return Ok(1);
                                }
                                _ => {}
                            }
                            continue;
                        }
                    }

                    let elapsed = reconnect_started.elapsed().as_secs();
                    write_stdout_async(
                        &async_stdout,
                        format!("\r\x1b[2;33m\u{25b8} reconnecting... {elapsed}s (Ctrl-C to abort)\x1b[0m\x1b[K")
                            .as_bytes(),
                    )
                    .await?;

                    let stream = match UnixStream::connect(ctl_path).await {
                        Ok(s) => s,
                        Err(_) => continue,
                    };

                    let mut new_framed = Framed::new(stream, FrameCodec);
                    if crate::handshake(&mut new_framed).await.is_err() {
                        continue;
                    }
                    if new_framed
                        .send(Frame::Attach {
                            session: session.to_string(),
                            client_name: String::new(),
                            force: true,
                        })
                        .await
                        .is_err()
                    {
                        continue;
                    }

                    match new_framed.next().await {
                        Some(Ok(Frame::Ok)) => {
                            write_stdout_async(
                                &async_stdout,
                                b"\r\x1b[32m\xe2\x96\xb8 reconnected\x1b[0m\x1b[K\r\n",
                            )
                            .await?;
                            framed = new_framed;
                            current_redraw = true;
                            break;
                        }
                        Some(Ok(Frame::Error { message, .. })) => {
                            write_stdout_async(
                                &async_stdout,
                                format!(
                                    "\r\x1b[31m\u{25b8} session gone: {message}\x1b[0m\x1b[K\r\n"
                                )
                                .as_bytes(),
                            )
                            .await?;
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

    let mut heartbeat_interval = tokio::time::interval(DEFAULT_HEARTBEAT_INTERVAL);
    heartbeat_interval.reset();
    let mut last_pong = Instant::now();
    let mut sigint = signal(SignalKind::interrupt())?;
    let mut sigterm = signal(SignalKind::terminate())?;
    let mut sighup = signal(SignalKind::hangup())?;
    let mut stdout = tokio::io::stdout();

    let code = 'outer: loop {
        let result = 'relay: loop {
            tokio::select! {
                frame = framed.next() => {
                    match frame {
                        Some(Ok(Frame::Data(data))) => {
                            use tokio::io::AsyncWriteExt;
                            stdout.write_all(&data).await?;
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
                    if last_pong.elapsed() > DEFAULT_HEARTBEAT_TIMEOUT {
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
                let reconnect_started = Instant::now();
                eprint!("\x1b[2;33m\u{25b8} reconnecting... (Ctrl-C to abort)\x1b[0m");
                loop {
                    tokio::time::sleep(Duration::from_secs(1)).await;
                    let elapsed = reconnect_started.elapsed().as_secs();
                    eprint!(
                        "\r\x1b[2;33m\u{25b8} reconnecting... {elapsed}s (Ctrl-C to abort)\x1b[0m\x1b[K"
                    );

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
                            eprintln!("\r\x1b[32m\u{25b8} reconnected\x1b[0m\x1b[K");
                            framed = new_framed;
                            heartbeat_interval.reset();
                            last_pong = Instant::now();
                            break;
                        }
                        Some(Ok(Frame::Error { message, .. })) => {
                            eprintln!("\r\x1b[31m\u{25b8} session gone: {message}\x1b[0m\x1b[K");
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
    {
        use tokio::io::AsyncWriteExt;
        let _ = stdout.write_all(b"\x1b[0m\x1b[?25h").await;
    }
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
    fn tilde_status() {
        let mut ep = EscapeProcessor { state: EscapeState::Normal };
        let actions = ep.process(b"\n~#");
        assert_eq!(actions, vec![EscapeAction::Data(b"\n".to_vec()), EscapeAction::Status,]);
    }

    #[test]
    fn tilde_reconnect() {
        let mut ep = EscapeProcessor { state: EscapeState::Normal };
        let actions = ep.process(b"\n~R");
        assert_eq!(actions, vec![EscapeAction::Data(b"\n".to_vec()), EscapeAction::Reconnect,]);
    }

    #[test]
    fn tilde_reconnect_stops_processing() {
        let mut ep = EscapeProcessor { state: EscapeState::Normal };
        let actions = ep.process(b"\n~Rremaining");
        assert_eq!(actions, vec![EscapeAction::Data(b"\n".to_vec()), EscapeAction::Reconnect,]);
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
