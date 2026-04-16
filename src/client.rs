use crate::alt_screen::AltScreenTracker;
use crate::protocol::{ErrorCode, Frame, FrameCodec};
use bytes::Bytes;
use futures_util::{SinkExt, StreamExt};
use nix::sys::termios::{self, FlushArg, LocalFlags, SetArg, SpecialCharacterIndices, Termios};
use std::collections::HashMap;
use std::io::{self, Read, Write};
use std::ops::ControlFlow;
use std::os::fd::{AsFd, AsRawFd, BorrowedFd};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicU32, Ordering};
use std::time::Duration;
use tokio::io::unix::AsyncFd;
use tokio::net::UnixStream;
use tokio::signal::unix::{SignalKind, signal};
use tokio::sync::mpsc;
use tokio::time::Instant;

/// Compute the path for the forward socket used by `gritty lf`/`gritty rf`.
/// Keyed on the immutable numeric session id so rename/`/`-in-name cannot
/// desync the attached client and the `lf`/`rf` command.
pub fn forward_socket_path(ctl_path: &Path, session_id: u32) -> PathBuf {
    let dir = ctl_path.parent().unwrap_or(Path::new("."));
    let host = ctl_path
        .file_stem()
        .and_then(|s| s.to_str())
        .map(|s| {
            if s == "ctl" {
                "local".to_string()
            } else {
                s.strip_prefix("connect-").unwrap_or(s).to_string()
            }
        })
        .unwrap_or_else(|| "unknown".to_string());
    dir.join(format!("fwd-{host}-{session_id}.sock"))
}

/// Outcome from a client relay loop iteration.
enum RelayExit {
    /// Shell or server reported an exit code (or detach/signal).
    Exit(i32),
    /// Server disconnected -- caller should reconnect.
    Disconnected,
}
use tokio_util::codec::Framed;
use tracing::{debug, info, warn};

// --- Rate limiter ---

use std::collections::VecDeque;

struct RateLimiter {
    timestamps: VecDeque<Instant>,
    max_count: usize,
    window: Duration,
}

impl RateLimiter {
    fn new(max_count: usize, window: Duration) -> Self {
        Self { timestamps: VecDeque::new(), max_count, window }
    }

    /// Returns true if the action is allowed, false if rate limited.
    fn check(&mut self) -> bool {
        let now = Instant::now();
        while self.timestamps.front().is_some_and(|t| now - *t > self.window) {
            self.timestamps.pop_front();
        }
        if self.timestamps.len() >= self.max_count {
            return false;
        }
        self.timestamps.push_back(now);
        true
    }
}

/// Rate limiters for server-initiated actions. Owned by `run()` so budgets
/// persist across auto-reconnect and a rogue server cannot reset them by
/// dropping the connection.
struct SecurityLimiters {
    url: RateLimiter,
    clipboard_set: RateLimiter,
    tunnel_listen: RateLimiter,
    agent_open: RateLimiter,
}

impl SecurityLimiters {
    fn new() -> Self {
        let window = Duration::from_secs(30);
        Self {
            url: RateLimiter::new(2, window),
            clipboard_set: RateLimiter::new(5, window),
            tunnel_listen: RateLimiter::new(2, window),
            agent_open: RateLimiter::new(10, window),
        }
    }
}

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

const SEND_TIMEOUT: Duration = Duration::from_secs(10);
/// Deadline for a single reconnect attempt: UDS connect + handshake + Attach reply.
/// Sized generously for cellular/high-RTT links where one retransmit can push a
/// 5s budget over the edge.
const RECONNECT_ATTEMPT_TIMEOUT: Duration = Duration::from_secs(15);

/// First sleep between reconnect attempts. Doubled on each failure, capped at
/// `RECONNECT_BACKOFF_MAX`. Resets on successful reconnect.
const RECONNECT_BACKOFF_INITIAL: Duration = Duration::from_secs(1);
/// Upper bound on the reconnect retry sleep. Kept modest so a recovered link
/// reattaches quickly.
const RECONNECT_BACKOFF_MAX: Duration = Duration::from_secs(10);

/// Treat these handshake-error prefixes as permanent: the server rejected
/// us, or the endpoint accepted the connection but EOF'd before/during
/// reply (typical of a tunnel forwarding to a daemon that's been
/// kill-server'd). No amount of client-side retry recovers -- the user
/// has to run `gritty restart`.
fn is_terminal_handshake_err(msg: &str) -> bool {
    msg.starts_with("handshake rejected")
        || msg.starts_with("daemon closed connection")
        || msg.starts_with("daemon protocol error")
}

/// Compute the next reconnect sleep given the previous one. Pure function so the
/// schedule is unit-testable.
fn next_reconnect_delay(prev: Duration) -> Duration {
    if prev < RECONNECT_BACKOFF_INITIAL {
        RECONNECT_BACKOFF_INITIAL
    } else {
        prev.saturating_mul(2).min(RECONNECT_BACKOFF_MAX)
    }
}

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
/// Requires the fd to have O_NONBLOCK set (done in `run()`).
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

pub fn get_terminal_size() -> (u16, u16) {
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

/// Send a frame with a timeout. Returns false if the send failed or timed out.
/// On success, updates `last_outbound_at` so the heartbeat probe can key its
/// cadence off the client's own sends (not inbound server traffic).
async fn timed_send(
    framed: &mut Framed<UnixStream, FrameCodec>,
    frame: Frame,
    last_outbound_at: &mut Instant,
) -> bool {
    match tokio::time::timeout(SEND_TIMEOUT, framed.send(frame)).await {
        Ok(Ok(())) => {
            *last_outbound_at = Instant::now();
            true
        }
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

const DEFAULT_HEARTBEAT_INTERVAL: Duration = Duration::from_secs(10);
const DEFAULT_HEARTBEAT_TIMEOUT: Duration = Duration::from_secs(60);
/// If wall-clock advances much faster than monotonic between heartbeat ticks,
/// the host likely slept. Force a liveness probe on resume.
const SUSPEND_SKEW_THRESHOLD: Duration = Duration::from_secs(5);
/// Deadline for the post-suspend probe reply before declaring the link dead.
/// Must tolerate NAT rebind + DNS recovery + a fresh TCP RTT on cellular, so
/// this is intentionally generous -- a false disconnect on wake is worse than
/// a slightly delayed real one (the idle timeout still catches truly dead links).
const SUSPEND_PROBE_DEADLINE: Duration = Duration::from_secs(15);

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
    /// Client accepted a TCP connection on a client-initiated remote-forward listener.
    Accepted {
        forward_id: u32,
        channel_id: u32,
        writer_tx: mpsc::Sender<Bytes>,
    },
    Data {
        channel_id: u32,
        data: Bytes,
    },
    Closed {
        channel_id: u32,
    },
    /// The controlling `gritty lf`/`gritty rf` process disconnected -- tear down the forward.
    ForwardStopped {
        forward_id: u32,
    },
}

/// Per-forward state on the client side.
struct ClientPortForwardState {
    listener_handle: Option<tokio::task::JoinHandle<()>>,
    keepalive_handle: Option<tokio::task::JoinHandle<()>>,
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
            // Client-allocated PF channel_ids (and forward_ids, which share this
            // counter) are even; server-allocated channel_ids are odd. Both sides
            // insert into a single `channels` map, so partitioning prevents lf/rf
            // collisions.
            next_channel_id: std::sync::Arc::new(std::sync::atomic::AtomicU32::new(2)),
        }
    }

    fn teardown(&mut self) {
        for (_, fwd) in self.forwards.drain() {
            if let Some(h) = fwd.listener_handle {
                h.abort();
            }
            if let Some(h) = fwd.keepalive_handle {
                h.abort();
            }
        }
        self.channels.clear();
    }
}

impl Drop for ClientPortForwardTable {
    fn drop(&mut self) {
        self.teardown();
    }
}

impl Drop for ClientTunnelState {
    fn drop(&mut self) {
        self.teardown();
    }
}

/// Send session setup frames (env, agent/open forwarding, resize).
/// Returns false if the connection dropped during setup.
async fn send_init_frames(
    framed: &mut Framed<UnixStream, FrameCodec>,
    env_vars: &[(String, String)],
    forward_agent: bool,
    agent_socket: Option<&str>,
    forward_open: bool,
    last_outbound_at: &mut Instant,
) -> bool {
    if !timed_send(framed, Frame::Env { vars: env_vars.to_vec() }, last_outbound_at).await {
        return false;
    }
    if forward_agent
        && agent_socket.is_some()
        && !timed_send(framed, Frame::AgentForward, last_outbound_at).await
    {
        return false;
    }
    if forward_open && !timed_send(framed, Frame::OpenForward, last_outbound_at).await {
        return false;
    }
    let (cols, rows) = get_terminal_size();
    if !timed_send(framed, Frame::Resize { cols, rows }, last_outbound_at).await {
        return false;
    }
    true
}

/// `framed` is kept outside (passed to handlers) so `tokio::select!` can
/// poll `framed.next()` independently without conflicting borrows.
struct ClientRelay<'a> {
    async_stdout: &'a AsyncFd<std::os::fd::OwnedFd>,
    alt_screen: &'a mut AltScreenTracker,
    agent: &'a mut ClientAgentState,
    agent_event_tx: &'a mpsc::UnboundedSender<AgentEvent>,
    agent_socket: Option<&'a str>,
    tunnel: &'a mut ClientTunnelState,
    tunnel_event_tx: &'a mpsc::UnboundedSender<ClientTunnelEvent>,
    oauth_redirect: bool,
    oauth_timeout: u64,
    forward_open: bool,
    pf: &'a mut ClientPortForwardTable,
    pf_event_tx: &'a mpsc::UnboundedSender<ClientPortForwardEvent>,
    client_initiated_forwards: &'a mut std::collections::HashMap<u32, u16>,
    last_activity: &'a mut Instant,
    last_outbound_at: &'a mut Instant,
    last_ping_sent: &'a mut Instant,
    last_rtt: &'a mut Option<Duration>,
    connected_at: Instant,
    bytes_relayed: &'a mut u64,
    url_limiter: &'a mut RateLimiter,
    clipboard_set_limiter: &'a mut RateLimiter,
    tunnel_listen_limiter: &'a mut RateLimiter,
    agent_open_limiter: &'a mut RateLimiter,
}

impl ClientRelay<'_> {
    async fn handle_server_frame(
        &mut self,
        framed: &mut Framed<UnixStream, FrameCodec>,
        frame: Option<Result<Frame, io::Error>>,
    ) -> Result<ControlFlow<RelayExit>, anyhow::Error> {
        // Any frame received from the server is proof-of-life; reset the idle timer.
        if matches!(frame, Some(Ok(_))) {
            *self.last_activity = Instant::now();
        }
        match frame {
            Some(Ok(Frame::Data(data))) => {
                debug!(len = data.len(), "socket → stdout");
                *self.bytes_relayed += data.len() as u64;
                self.alt_screen.scan(&data);
                write_stdout_async(self.async_stdout, &data).await?;
            }
            Some(Ok(Frame::Pong)) => {
                *self.last_rtt = Some(self.last_ping_sent.elapsed());
                debug!(rtt_ms = self.last_rtt.unwrap().as_secs_f64() * 1000.0, "pong received");
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
                if let Some(sock_path) =
                    self.agent_socket.filter(|_| self.agent_open_limiter.check())
                {
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
                            let _ = timed_send(
                                framed,
                                Frame::AgentClose { channel_id },
                                self.last_outbound_at,
                            )
                            .await;
                        }
                    }
                } else {
                    let _ =
                        timed_send(framed, Frame::AgentClose { channel_id }, self.last_outbound_at)
                            .await;
                }
            }
            Some(Ok(Frame::AgentData { channel_id, data })) => {
                if self.agent.channels.get(&channel_id).is_some_and(|tx| tx.try_send(data).is_err())
                {
                    warn!(channel_id, "agent channel backpressured, closing");
                    self.agent.channels.remove(&channel_id);
                }
            }
            Some(Ok(Frame::AgentClose { channel_id })) => {
                self.agent.channels.remove(&channel_id);
            }
            Some(Ok(Frame::OpenUrl { url })) => {
                if !self.forward_open {
                    debug!("rejected OpenUrl: forward_open disabled");
                } else if !self.url_limiter.check() {
                    warn!(url, "rate limited OpenUrl");
                } else if url.starts_with("http://") || url.starts_with("https://") {
                    info!(url, "opening URL from remote");
                    tokio::task::spawn_blocking(move || {
                        let _ = opener::open(&url);
                    });
                } else {
                    debug!("rejected non-http(s) URL: {url}");
                }
            }
            Some(Ok(Frame::ClipboardSet { data })) => {
                if self.clipboard_set_limiter.check() {
                    info!(len = data.len(), "clipboard set from remote");
                    tokio::task::spawn_blocking(move || {
                        clipboard_set(&data);
                    });
                } else {
                    warn!("rate limited ClipboardSet");
                }
            }
            Some(Ok(Frame::ClipboardGet)) => {
                warn!("rejected clipboard read from remote");
                let _ = timed_send(
                    framed,
                    Frame::ClipboardData { data: Bytes::new() },
                    self.last_outbound_at,
                )
                .await;
            }
            Some(Ok(Frame::TunnelListen { port })) => {
                if !self.oauth_redirect {
                    debug!(port, "tunnel: oauth-redirect disabled, declining");
                    let _ = timed_send(
                        framed,
                        Frame::TunnelClose { channel_id: 0 },
                        self.last_outbound_at,
                    )
                    .await;
                } else if !self.tunnel_listen_limiter.check() {
                    warn!(port, "rate limited TunnelListen");
                    let _ = timed_send(
                        framed,
                        Frame::TunnelClose { channel_id: 0 },
                        self.last_outbound_at,
                    )
                    .await;
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
                            let _ = timed_send(
                                framed,
                                Frame::TunnelClose { channel_id: 0 },
                                self.last_outbound_at,
                            )
                            .await;
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
                if self
                    .tunnel
                    .channels
                    .get(&channel_id)
                    .is_some_and(|tx| tx.try_send(data).is_err())
                {
                    warn!(channel_id, "tunnel channel backpressured, closing");
                    self.tunnel.channels.remove(&channel_id);
                }
            }
            Some(Ok(Frame::TunnelClose { channel_id })) => {
                self.tunnel.channels.remove(&channel_id);
            }
            // Port forward: server asks client to bind a port -- rejected (client-initiated only)
            Some(Ok(Frame::PortForwardListen { forward_id, listen_port, .. })) => {
                warn!(forward_id, listen_port, "rejected server-initiated port bind");
                let _ = timed_send(
                    framed,
                    Frame::PortForwardStop { forward_id },
                    self.last_outbound_at,
                )
                .await;
            }
            // Port forward: new TCP connection from server side (only accept client-initiated)
            Some(Ok(Frame::PortForwardOpen { forward_id, channel_id, target_port })) => {
                let Some(&expected_port) = self.client_initiated_forwards.get(&forward_id) else {
                    warn!(forward_id, target_port, "rejected unsolicited port forward open");
                    let _ = timed_send(
                        framed,
                        Frame::PortForwardClose { channel_id },
                        self.last_outbound_at,
                    )
                    .await;
                    return Ok(ControlFlow::Continue(()));
                };
                if target_port != expected_port {
                    warn!(
                        forward_id,
                        target_port, expected_port, "server-supplied port mismatch; using expected"
                    );
                }
                match tokio::net::TcpStream::connect(("127.0.0.1", expected_port)).await {
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
                        debug!(channel_id, expected_port, "pf connect failed: {e}");
                        let _ = timed_send(
                            framed,
                            Frame::PortForwardClose { channel_id },
                            self.last_outbound_at,
                        )
                        .await;
                    }
                }
            }
            // Port forward: channel data from server
            Some(Ok(Frame::PortForwardData { channel_id, data })) => {
                if self
                    .pf
                    .channels
                    .get(&channel_id)
                    .is_some_and(|(_, tx)| tx.try_send(data).is_err())
                {
                    warn!(channel_id, "pf channel backpressured, closing");
                    self.pf.channels.remove(&channel_id);
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
                    if let Some(h) = fwd.keepalive_handle {
                        h.abort();
                    }
                }
                // Remove channels belonging to this forward
                self.pf.channels.retain(|_, (fid, _)| *fid != forward_id);
                self.client_initiated_forwards.remove(&forward_id);
            }
            Some(Ok(Frame::DiagResponse { text })) => {
                let mut output = String::from("\r\n\x1b[2;33m[server diagnostics]\r\n");
                for line in text.lines() {
                    output.push_str(&format!("\x1b[0m\x1b[2m{line}\r\n"));
                }
                output.push_str("\x1b[0m");
                write_stdout_async(self.async_stdout, output.as_bytes()).await?;
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
                    && !timed_send(
                        framed,
                        Frame::AgentData { channel_id, data },
                        self.last_outbound_at,
                    )
                    .await
                {
                    return false;
                }
            }
            Some(AgentEvent::Closed { channel_id }) => {
                if self.agent.channels.remove(&channel_id).is_some()
                    && !timed_send(framed, Frame::AgentClose { channel_id }, self.last_outbound_at)
                        .await
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
                if !timed_send(framed, Frame::TunnelOpen { channel_id }, self.last_outbound_at)
                    .await
                {
                    return false;
                }
            }
            Some(ClientTunnelEvent::Data { channel_id, data }) => {
                if !timed_send(
                    framed,
                    Frame::TunnelData { channel_id, data },
                    self.last_outbound_at,
                )
                .await
                {
                    return false;
                }
            }
            Some(ClientTunnelEvent::Closed { channel_id }) => {
                self.tunnel.channels.remove(&channel_id);
                if !timed_send(framed, Frame::TunnelClose { channel_id }, self.last_outbound_at)
                    .await
                {
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
                        self.last_outbound_at,
                    )
                    .await
                    {
                        return false;
                    }
                }
            }
            Some(ClientPortForwardEvent::Data { channel_id, data }) => {
                if self.pf.channels.contains_key(&channel_id)
                    && !timed_send(
                        framed,
                        Frame::PortForwardData { channel_id, data },
                        self.last_outbound_at,
                    )
                    .await
                {
                    return false;
                }
            }
            Some(ClientPortForwardEvent::Closed { channel_id }) => {
                if self.pf.channels.remove(&channel_id).is_some()
                    && !timed_send(
                        framed,
                        Frame::PortForwardClose { channel_id },
                        self.last_outbound_at,
                    )
                    .await
                {
                    return false;
                }
            }
            Some(ClientPortForwardEvent::ForwardStopped { forward_id }) => {
                if let Some(fwd) = self.pf.forwards.remove(&forward_id)
                    && let Some(h) = fwd.listener_handle
                {
                    h.abort();
                }
                self.pf.channels.retain(|_, (fid, _)| *fid != forward_id);
                self.client_initiated_forwards.remove(&forward_id);
                if !timed_send(framed, Frame::PortForwardStop { forward_id }, self.last_outbound_at)
                    .await
                {
                    return false;
                }
            }
            None => {}
        }
        true
    }

    async fn handle_fwd_request(
        &mut self,
        framed: &mut Framed<UnixStream, FrameCodec>,
        mut fwd_stream: tokio::net::UnixStream,
    ) {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};

        let mut header = [0u8; 5];
        let read_result = tokio::time::timeout(
            std::time::Duration::from_secs(5),
            fwd_stream.read_exact(&mut header),
        )
        .await;
        match read_result {
            Ok(Ok(_)) => {}
            _ => return,
        }
        let direction = header[0];
        let listen_port = u16::from_be_bytes([header[1], header[2]]);
        let target_port = u16::from_be_bytes([header[3], header[4]]);

        let forward_id = self.pf.next_channel_id.fetch_add(2, Ordering::Relaxed);

        if direction == 1 {
            // Remote-forward: client binds locally
            match std::net::TcpListener::bind(("127.0.0.1", listen_port)) {
                Ok(std_listener) => {
                    std_listener.set_nonblocking(true).ok();
                    let listener = tokio::net::TcpListener::from_std(std_listener).unwrap();
                    let tx = self.pf_event_tx.clone();
                    let nid = self.pf.next_channel_id.clone();
                    let fwd_id = forward_id;
                    let handle = tokio::spawn(async move {
                        loop {
                            let (stream, _) = match listener.accept().await {
                                Ok(conn) => conn,
                                Err(_) => break,
                            };
                            let channel_id = nid.fetch_add(2, Ordering::Relaxed);
                            let (read_half, write_half) = stream.into_split();
                            let data_tx = tx.clone();
                            let close_tx = tx.clone();
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
                            if tx
                                .send(ClientPortForwardEvent::Accepted {
                                    forward_id: fwd_id,
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
                        ClientPortForwardState {
                            listener_handle: Some(handle),
                            keepalive_handle: None,
                            target_port,
                        },
                    );
                    info!(
                        forward_id,
                        listen_port, target_port, "remote-forward: listening locally"
                    );
                }
                Err(e) => {
                    warn!(listen_port, "remote-forward: bind failed: {e}");
                    let _ = fwd_stream.write_all(&[0x02]).await;
                    let _ = fwd_stream.write_all(format!("bind failed: {e}").as_bytes()).await;
                    return;
                }
            }
        }

        // Track the client-intended target port for lf so PortForwardOpen cannot
        // pivot to a server-chosen port. rf forwards never receive PFOpen from the
        // server, so they are intentionally not registered here.
        if direction == 0 {
            self.client_initiated_forwards.insert(forward_id, target_port);
        }
        // Send PortForwardRequest to server
        if !timed_send(
            framed,
            Frame::PortForwardRequest { forward_id, direction, listen_port, target_port },
            self.last_outbound_at,
        )
        .await
        {
            let _ = fwd_stream.write_all(&[0x02]).await;
            let _ = fwd_stream.write_all(b"server connection lost").await;
            return;
        }

        info!(forward_id, direction, listen_port, target_port, "port forward established");
        let _ = fwd_stream.write_all(&[0x01]).await;

        // Keepalive: when the controlling process disconnects, tear down the forward.
        let pf_tx = self.pf_event_tx.clone();
        let keepalive_handle = tokio::spawn(async move {
            let mut buf = [0u8; 1];
            let _ = fwd_stream.read(&mut buf).await;
            let _ = pf_tx.send(ClientPortForwardEvent::ForwardStopped { forward_id });
        });
        // Track the keepalive task (and for lf, create the forwards entry) so teardown
        // aborts it — dropping fwd_stream lets the `gritty lf`/`rf` process see EOF.
        self.pf
            .forwards
            .entry(forward_id)
            .or_insert(ClientPortForwardState {
                listener_handle: None,
                keepalive_handle: None,
                target_port,
            })
            .keepalive_handle = Some(keepalive_handle);
    }
}

/// Relay between stdin/stdout and the framed socket.
#[allow(clippy::too_many_arguments)]
async fn relay(
    framed: &mut Framed<UnixStream, FrameCodec>,
    async_stdin: &AsyncFd<io::Stdin>,
    async_stdout: &AsyncFd<std::os::fd::OwnedFd>,
    sigwinch: &mut tokio::signal::unix::Signal,
    sigterm: &mut tokio::signal::unix::Signal,
    sighup: &mut tokio::signal::unix::Signal,
    buf: &mut [u8],
    mut escape: Option<&mut EscapeProcessor>,
    raw_guard: &RawModeGuard,
    nb_guard: &NonBlockGuard,
    agent_socket: Option<&str>,
    oauth_redirect: bool,
    oauth_timeout: u64,
    forward_open: bool,
    session: &str,
    hb_interval: Duration,
    hb_timeout: Duration,
    fwd_listener: &Option<tokio::net::UnixListener>,
    limiters: &mut SecurityLimiters,
    alt_screen: &mut AltScreenTracker,
) -> anyhow::Result<RelayExit> {
    let mut heartbeat_interval = tokio::time::interval(hb_interval);
    heartbeat_interval.reset(); // first tick is immediate otherwise; delay it
    let mut last_activity = Instant::now();
    // Timestamp of the last frame we successfully sent to the server. The
    // ping cadence is driven off this (not last_activity) so steady inbound
    // server output doesn't suppress the probes the server uses for its
    // idle-evict decision.
    let mut last_outbound_at = Instant::now();
    let mut last_ping_sent = Instant::now();
    let mut last_rtt: Option<Duration> = None;
    let mut last_tick_mono = Instant::now();
    let mut last_tick_wall = std::time::SystemTime::now();
    // When set, a post-suspend probe is outstanding; if no server frame arrives
    // by this deadline we treat the connection as dead and reconnect.
    let mut suspend_probe_deadline: Option<Instant> = None;
    let mut suspend_probe_baseline: Option<Instant> = None;

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
    let mut client_initiated_forwards = std::collections::HashMap::new();
    let mut relay = ClientRelay {
        async_stdout,
        alt_screen,
        agent: &mut agent,
        agent_event_tx: &agent_event_tx,
        agent_socket,
        tunnel: &mut tunnel,
        tunnel_event_tx: &tunnel_event_tx,
        oauth_redirect,
        oauth_timeout,
        forward_open,
        pf: &mut pf,
        pf_event_tx: &pf_event_tx,
        client_initiated_forwards: &mut client_initiated_forwards,
        last_activity: &mut last_activity,
        last_outbound_at: &mut last_outbound_at,
        last_ping_sent: &mut last_ping_sent,
        last_rtt: &mut last_rtt,
        connected_at: Instant::now(),
        bytes_relayed: &mut bytes_relayed,
        url_limiter: &mut limiters.url,
        clipboard_set_limiter: &mut limiters.clipboard_set,
        tunnel_listen_limiter: &mut limiters.tunnel_listen,
        agent_open_limiter: &mut limiters.agent_open,
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
                                        if !timed_send(framed, Frame::Data(Bytes::from(data)), relay.last_outbound_at).await {
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
                                        // Avoid a spurious idle-timeout or suspend-probe after
                                        // returning from SIGTSTP.
                                        heartbeat_interval = tokio::time::interval(hb_interval);
                                        heartbeat_interval.reset();
                                        *relay.last_activity = Instant::now();
                                        last_tick_mono = Instant::now();
                                        last_tick_wall = std::time::SystemTime::now();
                                        suspend_probe_deadline = None;
                                        suspend_probe_baseline = None;
                                        // Re-sync terminal size after resume
                                        let (cols, rows) = get_terminal_size();
                                        if !timed_send(framed, Frame::Resize { cols, rows }, relay.last_outbound_at).await {
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
                                        // Request server-side diagnostics (displayed when DiagResponse arrives)
                                        if !timed_send(framed, Frame::DiagRequest, relay.last_outbound_at).await {
                                            return Ok(RelayExit::Disconnected);
                                        }
                                    }
                                    EscapeAction::Help => {
                                        write_stdout_async(async_stdout, ESCAPE_HELP).await?;
                                    }
                                }
                            }
                        } else if !timed_send(framed, Frame::Data(Bytes::copy_from_slice(&buf[..n])), relay.last_outbound_at).await {
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
                // Any server frame satisfies an outstanding post-suspend probe.
                if let Some(baseline) = suspend_probe_baseline
                    && *relay.last_activity > baseline
                {
                    debug!("post-suspend probe satisfied");
                    suspend_probe_deadline = None;
                    suspend_probe_baseline = None;
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

            result = async {
                match fwd_listener.as_ref() {
                    Some(listener) => listener.accept().await,
                    None => std::future::pending().await,
                }
            } => {
                if let Ok((stream, _)) = result
                    && crate::security::verify_peer_uid(&stream).is_ok() {
                        relay.handle_fwd_request(framed, stream).await;
                    }
            }

            _ = sigwinch.recv() => {
                let (cols, rows) = get_terminal_size();
                debug!(cols, rows, "SIGWINCH → resize");
                if !timed_send(framed, Frame::Resize { cols, rows }, relay.last_outbound_at).await {
                    return Ok(RelayExit::Disconnected);
                }
            }

            _ = heartbeat_interval.tick() => {
                // Detect suspend/resume by comparing wall-clock and monotonic deltas.
                // Both Instant and SystemTime tick together while the process runs; during
                // a full suspend Instant pauses but SystemTime doesn't, so wall >> mono on
                // the first tick after resume.
                let mono_now = Instant::now();
                let wall_now = std::time::SystemTime::now();
                let mono_delta = mono_now.saturating_duration_since(last_tick_mono);
                let wall_delta = wall_now
                    .duration_since(last_tick_wall)
                    .unwrap_or(Duration::ZERO);
                last_tick_mono = mono_now;
                last_tick_wall = wall_now;
                let suspended = wall_delta > mono_delta + SUSPEND_SKEW_THRESHOLD;

                if relay.last_activity.elapsed() > hb_timeout {
                    debug!("idle timeout");
                    return Ok(RelayExit::Disconnected);
                }

                // Fire a probe if we haven't sent anything recently OR we just
                // came back from suspend. Keying off last_outbound_at (not
                // last_activity) is what proves liveness to the server's
                // idle-evict: steady inbound server output does not prove the
                // client can still send.
                let should_probe = suspended
                    || relay.last_outbound_at.elapsed() >= hb_interval;
                if should_probe {
                    if suspended {
                        debug!(
                            wall_ms = wall_delta.as_millis(),
                            mono_ms = mono_delta.as_millis(),
                            "suspend detected, probing link",
                        );
                        suspend_probe_baseline = Some(*relay.last_activity);
                        suspend_probe_deadline =
                            Some(Instant::now() + SUSPEND_PROBE_DEADLINE);
                    }
                    *relay.last_ping_sent = Instant::now();
                    if !timed_send(framed, Frame::Ping, relay.last_outbound_at).await {
                        return Ok(RelayExit::Disconnected);
                    }
                }
            }

            _ = async {
                match suspend_probe_deadline {
                    Some(d) => tokio::time::sleep_until(d).await,
                    None => std::future::pending().await,
                }
            } => {
                // Deadline fired with no activity past baseline -> link is dead.
                if let Some(baseline) = suspend_probe_baseline
                    && *relay.last_activity <= baseline
                {
                    debug!("post-suspend probe timed out");
                    return Ok(RelayExit::Disconnected);
                }
                suspend_probe_deadline = None;
                suspend_probe_baseline = None;
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
    session_id: u32,
    mut framed: Framed<UnixStream, FrameCodec>,
    ctl_path: &Path,
    env_vars: Vec<(String, String)>,
    no_escape: bool,
    forward_agent: bool,
    forward_open: bool,
    oauth_redirect: bool,
    oauth_timeout: u64,
    heartbeat_interval: u64,
    heartbeat_timeout: u64,
    client_name: String,
    expected_server_id: u64,
    mut attach_token: u64,
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
    // Set O_NONBLOCK explicitly: when stdout is a separate OFD (pipe/redirect), stdin's
    // NonBlockGuard doesn't cover it and write() would block the relay loop.
    let stdout_fd = crate::security::checked_dup(io::stdout().as_raw_fd())?;
    {
        use nix::fcntl::{FcntlArg, OFlag, fcntl};
        let flags = fcntl(&stdout_fd, FcntlArg::F_GETFL)?;
        fcntl(&stdout_fd, FcntlArg::F_SETFL(OFlag::from_bits_truncate(flags) | OFlag::O_NONBLOCK))?;
    }
    let async_stdout = AsyncFd::new(stdout_fd)?;

    // PTY output (vim/htop/less) may leave the alt-screen active or the cursor
    // hidden. RawModeGuard only restores termios, not in-band DEC private modes,
    // so emit reset escapes on every exit path via Drop.
    struct TerminalResetGuard;
    impl Drop for TerminalResetGuard {
        fn drop(&mut self) {
            let _ = io::stdout().write_all(b"\x1b[?1049l\x1b[0m\x1b[?25h");
            let _ = io::stdout().flush();
        }
    }
    let _term_reset = TerminalResetGuard;

    let mut sigwinch = signal(SignalKind::window_change())?;
    // Hoisted so they stay live across the reconnect loop — tokio::signal() permanently
    // replaces the libc disposition, so dropping the stream would swallow the signal.
    let mut sigterm = signal(SignalKind::terminate())?;
    let mut sighup = signal(SignalKind::hangup())?;
    let mut buf = vec![0u8; 4096];
    let mut current_env = env_vars;
    let mut escape = if no_escape { None } else { Some(EscapeProcessor::new()) };
    let agent_socket = if forward_agent { std::env::var("SSH_AUTH_SOCK").ok() } else { None };

    // Forward socket: lets `gritty lf`/`gritty rf` request port forwards from this client.
    let fwd_path = forward_socket_path(ctl_path, session_id);
    struct FwdCleanup(PathBuf);
    impl Drop for FwdCleanup {
        fn drop(&mut self) {
            let _ = std::fs::remove_file(&self.0);
        }
    }
    // On same-host force-takeover the old client's fwd socket may still
    // be bound for a brief window before its Drop releases it. Retry
    // briefly instead of giving up on first AddrInUse so lf/rf survives a
    // local takeover. Any other bind error is reported immediately.
    let (fwd_listener, _fwd_cleanup) = {
        let mut attempts = 0u32;
        loop {
            match crate::security::bind_unix_listener(&fwd_path) {
                Ok(listener) => break (Some(listener), Some(FwdCleanup(fwd_path))),
                Err(e) if attempts < 20 => {
                    attempts += 1;
                    tokio::time::sleep(Duration::from_millis(100)).await;
                    if attempts == 20 {
                        warn!(
                            "forward socket bind failed after retries (lf/rf will be unavailable): {e}"
                        );
                    }
                }
                Err(_) => break (None, None),
            }
        }
    };

    let mut limiters = SecurityLimiters::new();
    // Mirrors the server's alt-screen tracker so we can suppress our own
    // reconnect chrome (▸ reconnecting..., ▸ reconnected) when a TUI is
    // running -- those writes would otherwise land directly in the alt
    // screen buffer and corrupt it.
    let mut alt_screen = AltScreenTracker::new();

    loop {
        // Reset every reconnect: a fresh TCP/UDS connection is a fresh liveness
        // window, so the next ping cadence starts from now.
        let mut last_outbound_at = Instant::now();
        let result = if send_init_frames(
            &mut framed,
            &current_env,
            forward_agent,
            agent_socket.as_deref(),
            forward_open,
            &mut last_outbound_at,
        )
        .await
        {
            relay(
                &mut framed,
                &async_stdin,
                &async_stdout,
                &mut sigwinch,
                &mut sigterm,
                &mut sighup,
                &mut buf,
                escape.as_mut(),
                &raw_guard,
                &nb_guard,
                agent_socket.as_deref(),
                oauth_redirect,
                oauth_timeout,
                forward_open,
                session,
                Duration::from_secs(heartbeat_interval),
                Duration::from_secs(heartbeat_timeout),
                &fwd_listener,
                &mut limiters,
                &mut alt_screen,
            )
            .await?
        } else {
            RelayExit::Disconnected
        };
        match result {
            RelayExit::Exit(code) => return Ok(code),
            RelayExit::Disconnected => {
                current_env.clear();
                // Snapshot alt-screen state once: during a disconnect we're
                // not receiving bytes, so the TUI can't change mode under us.
                // Skip our reconnect chrome when a TUI owns the screen so we
                // don't corrupt its buffer; the server's post-reconnect
                // force_tui_redraw will visibly repaint the TUI for the user.
                let show_chrome = !alt_screen.in_alternate_screen();
                let reconnect_started = Instant::now();
                // Timestamp of the most recent observation that ctl_path did
                // NOT exist. Cleared whenever the socket reappears or a probe
                // succeeds. If it persists past SOCKET_GONE_GRACE we treat
                // the tunnel/server as torn down (e.g. `gritty tunnel-destroy`
                // removed the socket file) and exit instead of looping.
                let mut socket_missing_since: Option<Instant> = None;
                const SOCKET_GONE_GRACE: Duration = Duration::from_secs(3);
                let mut backoff = Duration::ZERO;
                if show_chrome {
                    write_stdout_async(
                        &async_stdout,
                        b"\r\n\x1b[2;33m\xe2\x96\xb8 reconnecting... (Ctrl-C to abort)\x1b[0m",
                    )
                    .await?;
                }

                loop {
                    // Only advance the backoff when we actually slept+tried
                    // (or the backoff was interrupted by something other
                    // than a non-Ctrl-C keystroke). Previously this ran at
                    // the top of the loop, so every impatient keystroke
                    // while "reconnecting..." was displayed re-entered via
                    // `continue` below and doubled the backoff without ever
                    // attempting to connect.
                    let sleep_for = next_reconnect_delay(backoff);
                    // Race sleep against stdin so Ctrl-C is instant
                    tokio::select! {
                        _ = tokio::time::sleep(sleep_for) => {}
                        _ = sigterm.recv() => {
                            write_stdout_async(&async_stdout, b"\r\n").await?;
                            return Ok(1);
                        }
                        _ = sighup.recv() => {
                            write_stdout_async(&async_stdout, b"\r\n").await?;
                            return Ok(1);
                        }
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
                            // Fall through: impatient keystroke (not Ctrl-C)
                            // cuts the current sleep short and triggers an
                            // attempt now rather than restarting the outer
                            // loop. Restarting here re-raced the same sleep,
                            // so a user holding a key (>1 keystroke/sec)
                            // starved every reconnect attempt.
                        }
                    }
                    backoff = sleep_for;

                    let elapsed = reconnect_started.elapsed().as_secs();
                    if show_chrome {
                        write_stdout_async(
                            &async_stdout,
                            format!("\r\x1b[2;33m\u{25b8} reconnecting... {elapsed}s (Ctrl-C to abort)\x1b[0m\x1b[K")
                                .as_bytes(),
                        )
                        .await?;
                    }

                    // For tunnels, the supervisor holds an flock on a
                    // companion `.lock` even while it's respawning the SSH
                    // child; the ctl socket can vanish for several seconds
                    // during respawn (backoff is 1-60s). Treat a held lock
                    // as "tunnel still live, keep retrying" so the short
                    // SOCKET_GONE_GRACE can't kill us mid-respawn. Only a
                    // genuinely destroyed tunnel or a dead local daemon
                    // should trip the grace window.
                    let tunnel_supervisor_alive = crate::connect::ctl_socket_lock_path(ctl_path)
                        .as_deref()
                        .is_some_and(crate::connect::is_lock_held);
                    if !ctl_path.exists() && !tunnel_supervisor_alive {
                        let first_seen = *socket_missing_since.get_or_insert_with(Instant::now);
                        if first_seen.elapsed() >= SOCKET_GONE_GRACE {
                            // `\x1b[?1049l` leaves alt-screen first so the
                            // error is visible on main screen -- otherwise
                            // RawModeGuard's Drop emits it after the fact
                            // and clobbers the message. No-op on main
                            // screen.
                            write_stdout_async(
                                &async_stdout,
                                b"\x1b[?1049l\r\x1b[31m\xe2\x96\xb8 server socket gone -- session is unreachable; reconnect manually\x1b[0m\x1b[K\r\n",
                            )
                            .await?;
                            return Ok(1);
                        }
                        continue;
                    } else {
                        socket_missing_since = None;
                    }

                    enum Attempt {
                        Connected(Framed<UnixStream, FrameCodec>, u64),
                        SessionGone(String),
                        ServerRestarted,
                        OwnerChanged,
                        VersionMismatch { server_version: u16 },
                        HandshakeErr(String),
                        DaemonGone,
                        Retry,
                    }

                    let attempt = tokio::time::timeout(RECONNECT_ATTEMPT_TIMEOUT, async {
                        let stream = match crate::security::connect_verified(ctl_path).await {
                            Ok(s) => s,
                            Err(e) => {
                                // ECONNREFUSED with the socket file still on
                                // disk and no tunnel supervisor is the stale-
                                // socket signature of a crashed local daemon.
                                // The SOCKET_GONE_GRACE window above only
                                // covers socket-missing, so without this the
                                // client would spin "reconnecting..." forever.
                                if e.kind() == std::io::ErrorKind::ConnectionRefused
                                    && !tunnel_supervisor_alive
                                {
                                    return Attempt::DaemonGone;
                                }
                                return Attempt::Retry;
                            }
                        };
                        let mut new_framed = Framed::new(stream, FrameCodec);
                        let info = match crate::handshake(&mut new_framed).await {
                            Ok(info) => info,
                            Err(e) => return Attempt::HandshakeErr(e.to_string()),
                        };
                        if info.server_id != expected_server_id {
                            return Attempt::ServerRestarted;
                        }
                        if info.version != crate::protocol::PROTOCOL_VERSION {
                            return Attempt::VersionMismatch { server_version: info.version };
                        }
                        let (cols, rows) = get_terminal_size();
                        if new_framed
                            .send(Frame::Attach {
                                // Reconnect by numeric id, not the original
                                // target string. The user may have passed `-`
                                // or a name that since resolved to a
                                // different session; the id the daemon
                                // handed us at attach time is stable.
                                session: session_id.to_string(),
                                client_name: client_name.clone(),
                                force: true,
                                no_replay: false,
                                cols,
                                rows,
                                attach_token,
                            })
                            .await
                            .is_err()
                        {
                            return Attempt::Retry;
                        }
                        match new_framed.next().await {
                            Some(Ok(Frame::AttachAck { token, session_id: _ })) => {
                                Attempt::Connected(new_framed, token)
                            }
                            Some(Ok(Frame::Error { code: ErrorCode::AlreadyAttached, .. })) => {
                                Attempt::Retry
                            }
                            Some(Ok(Frame::Error { code: ErrorCode::OwnerChanged, .. })) => {
                                Attempt::OwnerChanged
                            }
                            Some(Ok(Frame::Error { message, .. })) => Attempt::SessionGone(message),
                            _ => Attempt::Retry,
                        }
                    })
                    .await;

                    match attempt {
                        Ok(Attempt::Connected(new_framed, new_token)) => {
                            if show_chrome {
                                write_stdout_async(
                                    &async_stdout,
                                    b"\r\x1b[32m\xe2\x96\xb8 reconnected\x1b[0m\x1b[K\r\n",
                                )
                                .await?;
                            }
                            attach_token = new_token;
                            framed = new_framed;
                            break;
                        }
                        Ok(Attempt::SessionGone(message)) => {
                            write_stdout_async(
                                &async_stdout,
                                format!(
                                    "\x1b[?1049l\r\x1b[31m\u{25b8} session gone: {message}\x1b[0m\x1b[K\r\n"
                                )
                                .as_bytes(),
                            )
                            .await?;
                            return Ok(1);
                        }
                        Ok(Attempt::ServerRestarted) => {
                            write_stdout_async(
                                &async_stdout,
                                b"\x1b[?1049l\r\x1b[31m\xe2\x96\xb8 server restarted -- session is gone; reconnect manually\x1b[0m\x1b[K\r\n",
                            )
                            .await?;
                            return Ok(1);
                        }
                        Ok(Attempt::OwnerChanged) => {
                            write_stdout_async(
                                &async_stdout,
                                b"\x1b[?1049l\r\x1b[31m\xe2\x96\xb8 session taken over by another client\x1b[0m\x1b[K\r\n",
                            )
                            .await?;
                            return Ok(1);
                        }
                        Ok(Attempt::VersionMismatch { server_version }) => {
                            let local = crate::protocol::PROTOCOL_VERSION;
                            let msg = format!(
                                "\x1b[?1049l\r\x1b[31m\u{25b8} protocol version mismatch (local={local} remote={server_version}) -- run `gritty restart` to upgrade\x1b[0m\x1b[K\r\n"
                            );
                            write_stdout_async(&async_stdout, msg.as_bytes()).await?;
                            return Ok(1);
                        }
                        Ok(Attempt::HandshakeErr(msg)) => {
                            write_stdout_async(
                                &async_stdout,
                                format!("\x1b[?1049l\r\x1b[31m\u{25b8} {msg}\x1b[0m\x1b[K\r\n")
                                    .as_bytes(),
                            )
                            .await?;
                            // Server-side rejection (version mismatch, etc.)
                            // is permanent. "daemon closed connection" is
                            // also terminal: the UDS endpoint accepts but
                            // EOFs during handshake -- typically the tunnel
                            // is forwarding to a dead/killed daemon. No
                            // number of client-side retries fixes this;
                            // user has to run `gritty restart`.
                            if is_terminal_handshake_err(&msg) {
                                return Ok(1);
                            }
                            continue;
                        }
                        Ok(Attempt::DaemonGone) => {
                            write_stdout_async(
                                &async_stdout,
                                b"\x1b[?1049l\r\x1b[31m\xe2\x96\xb8 daemon appears to have crashed -- session is gone; run `gritty server` or `gritty restart`\x1b[0m\x1b[K\r\n",
                            )
                            .await?;
                            return Ok(1);
                        }
                        Ok(Attempt::Retry) | Err(_) => continue,
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
    session_id: u32,
    mut framed: Framed<UnixStream, FrameCodec>,
    ctl_path: &Path,
    expected_server_id: u64,
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
    let mut last_activity = Instant::now();
    // Last time we successfully sent a frame; drives ping cadence so the
    // server's idle-evict doesn't fire during steady inbound traffic.
    let mut last_outbound_at = Instant::now();
    let mut last_tick_mono = Instant::now();
    let mut last_tick_wall = std::time::SystemTime::now();
    let mut suspend_probe_deadline: Option<Instant> = None;
    let mut suspend_probe_baseline: Option<Instant> = None;
    let mut sigint = signal(SignalKind::interrupt())?;
    let mut sigterm = signal(SignalKind::terminate())?;
    let mut sighup = signal(SignalKind::hangup())?;
    let mut stdout = tokio::io::stdout();
    // Track the PTY's alt-screen mode so we can suppress reconnect chrome
    // when tailing a session where a TUI is running.
    let mut alt_screen = AltScreenTracker::new();

    let code = 'outer: loop {
        let result = 'relay: loop {
            tokio::select! {
                frame = framed.next() => {
                    if matches!(frame, Some(Ok(_))) {
                        last_activity = Instant::now();
                        if let Some(baseline) = suspend_probe_baseline
                            && last_activity > baseline
                        {
                            suspend_probe_deadline = None;
                            suspend_probe_baseline = None;
                        }
                    }
                    match frame {
                        Some(Ok(Frame::Data(data))) => {
                            use tokio::io::AsyncWriteExt;
                            alt_screen.scan(&data);
                            stdout.write_all(&data).await?;
                        }
                        Some(Ok(Frame::Pong)) => {}
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
                    let mono_now = Instant::now();
                    let wall_now = std::time::SystemTime::now();
                    let mono_delta = mono_now.saturating_duration_since(last_tick_mono);
                    let wall_delta = wall_now
                        .duration_since(last_tick_wall)
                        .unwrap_or(Duration::ZERO);
                    last_tick_mono = mono_now;
                    last_tick_wall = wall_now;
                    let suspended = wall_delta > mono_delta + SUSPEND_SKEW_THRESHOLD;

                    if last_activity.elapsed() > DEFAULT_HEARTBEAT_TIMEOUT {
                        debug!("tail idle timeout");
                        break 'relay None;
                    }

                    let should_probe = suspended
                        || last_outbound_at.elapsed() >= DEFAULT_HEARTBEAT_INTERVAL;
                    if should_probe {
                        if suspended {
                            suspend_probe_baseline = Some(last_activity);
                            suspend_probe_deadline =
                                Some(Instant::now() + SUSPEND_PROBE_DEADLINE);
                        }
                        if framed.send(Frame::Ping).await.is_err() {
                            break 'relay None;
                        }
                        last_outbound_at = Instant::now();
                    }
                }
                _ = async {
                    match suspend_probe_deadline {
                        Some(d) => tokio::time::sleep_until(d).await,
                        None => std::future::pending().await,
                    }
                } => {
                    if let Some(baseline) = suspend_probe_baseline
                        && last_activity <= baseline
                    {
                        debug!("tail post-suspend probe timed out");
                        break 'relay None;
                    }
                    suspend_probe_deadline = None;
                    suspend_probe_baseline = None;
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
                let show_chrome = !alt_screen.in_alternate_screen();
                let reconnect_started = Instant::now();
                let mut socket_missing_since: Option<Instant> = None;
                const SOCKET_GONE_GRACE: Duration = Duration::from_secs(3);
                if show_chrome {
                    eprint!("\x1b[2;33m\u{25b8} reconnecting... (Ctrl-C to abort)\x1b[0m");
                }
                loop {
                    tokio::select! {
                        _ = tokio::time::sleep(Duration::from_secs(1)) => {}
                        _ = sigint.recv() => { break 'outer 0; }
                        _ = sigterm.recv() => { break 'outer 1; }
                        _ = sighup.recv() => { break 'outer 1; }
                    }
                    let elapsed = reconnect_started.elapsed().as_secs();
                    if show_chrome {
                        eprint!(
                            "\r\x1b[2;33m\u{25b8} reconnecting... {elapsed}s (Ctrl-C to abort)\x1b[0m\x1b[K"
                        );
                    }

                    let tunnel_supervisor_alive = crate::connect::ctl_socket_lock_path(ctl_path)
                        .as_deref()
                        .is_some_and(crate::connect::is_lock_held);
                    if !ctl_path.exists() && !tunnel_supervisor_alive {
                        let first_seen = *socket_missing_since.get_or_insert_with(Instant::now);
                        if first_seen.elapsed() >= SOCKET_GONE_GRACE {
                            eprintln!(
                                "\r\x1b[31m\u{25b8} server socket gone -- session is unreachable; reconnect manually\x1b[0m\x1b[K"
                            );
                            break 'outer 1;
                        }
                        continue;
                    } else {
                        socket_missing_since = None;
                    }

                    // Bound a single attempt (connect + handshake + Tail
                    // request + Ok wait) so a wedged server/tunnel can't
                    // strand the reconnect loop inside one .await while
                    // Ctrl-C is unreachable.
                    enum Outcome {
                        Connected(Framed<UnixStream, FrameCodec>),
                        ServerRestarted,
                        VersionMismatch { local: u16, remote: u16 },
                        HandshakeRejected(String),
                        SessionGone(String),
                        DaemonGone,
                        Retry,
                    }
                    let outcome = tokio::time::timeout(RECONNECT_ATTEMPT_TIMEOUT, async {
                        let stream = match crate::security::connect_verified(ctl_path).await {
                            Ok(s) => s,
                            Err(e) => {
                                if e.kind() == std::io::ErrorKind::ConnectionRefused
                                    && !tunnel_supervisor_alive
                                {
                                    return Outcome::DaemonGone;
                                }
                                return Outcome::Retry;
                            }
                        };
                        let mut new_framed = Framed::new(stream, FrameCodec);
                        let info = match crate::handshake(&mut new_framed).await {
                            Ok(info) => info,
                            Err(e) => {
                                let msg = e.to_string();
                                if is_terminal_handshake_err(&msg) {
                                    return Outcome::HandshakeRejected(msg);
                                }
                                return Outcome::Retry;
                            }
                        };
                        if info.server_id != expected_server_id {
                            return Outcome::ServerRestarted;
                        }
                        if info.version != crate::protocol::PROTOCOL_VERSION {
                            return Outcome::VersionMismatch {
                                local: crate::protocol::PROTOCOL_VERSION,
                                remote: info.version,
                            };
                        }
                        // Reconnect by numeric id -- the original target
                        // string may have been `-` (which would re-resolve
                        // to a different session) or a name that's since
                        // been taken over by a different session.
                        if new_framed
                            .send(Frame::Tail { session: session_id.to_string() })
                            .await
                            .is_err()
                        {
                            return Outcome::Retry;
                        }
                        match new_framed.next().await {
                            Some(Ok(Frame::Ok)) => Outcome::Connected(new_framed),
                            Some(Ok(Frame::Error { message, .. })) => Outcome::SessionGone(message),
                            _ => Outcome::Retry,
                        }
                    })
                    .await;

                    match outcome {
                        Ok(Outcome::Connected(new_framed)) => {
                            if show_chrome {
                                eprintln!("\r\x1b[32m\u{25b8} reconnected\x1b[0m\x1b[K");
                            }
                            framed = new_framed;
                            heartbeat_interval.reset();
                            last_activity = Instant::now();
                            last_tick_mono = Instant::now();
                            last_tick_wall = std::time::SystemTime::now();
                            suspend_probe_deadline = None;
                            suspend_probe_baseline = None;
                            break;
                        }
                        Ok(Outcome::ServerRestarted) => {
                            eprintln!(
                                "\r\x1b[31m\u{25b8} server restarted -- session is gone; reconnect manually\x1b[0m\x1b[K"
                            );
                            break 'outer 1;
                        }
                        Ok(Outcome::VersionMismatch { local, remote }) => {
                            eprintln!(
                                "\r\x1b[31m\u{25b8} protocol version mismatch (local={local} remote={remote}) -- run `gritty restart` to upgrade\x1b[0m\x1b[K"
                            );
                            break 'outer 1;
                        }
                        Ok(Outcome::HandshakeRejected(msg)) => {
                            eprintln!("\r\x1b[31m\u{25b8} {msg}\x1b[0m\x1b[K");
                            break 'outer 1;
                        }
                        Ok(Outcome::SessionGone(message)) => {
                            eprintln!("\r\x1b[31m\u{25b8} session gone: {message}\x1b[0m\x1b[K");
                            break 'outer 1;
                        }
                        Ok(Outcome::DaemonGone) => {
                            eprintln!(
                                "\r\x1b[31m\u{25b8} daemon appears to have crashed -- session is gone; run `gritty server` or `gritty restart`\x1b[0m\x1b[K"
                            );
                            break 'outer 1;
                        }
                        Ok(Outcome::Retry) | Err(_) => continue,
                    }
                }
            }
        }
    };

    // Reset terminal state: exit alt-screen, clear attributes, show cursor.
    // PTY output may have left colors/bold set, cursor hidden, or alt-screen active.
    {
        use tokio::io::AsyncWriteExt;
        let _ = stdout.write_all(b"\x1b[?1049l\x1b[0m\x1b[?25h").await;
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

    #[test]
    fn reconnect_backoff_starts_at_initial() {
        assert_eq!(next_reconnect_delay(Duration::ZERO), RECONNECT_BACKOFF_INITIAL);
    }

    #[test]
    fn reconnect_backoff_doubles_each_step() {
        let d = next_reconnect_delay(Duration::from_secs(1));
        assert_eq!(d, Duration::from_secs(2));
        let d = next_reconnect_delay(d);
        assert_eq!(d, Duration::from_secs(4));
        let d = next_reconnect_delay(d);
        assert_eq!(d, Duration::from_secs(8));
    }

    #[test]
    fn reconnect_backoff_caps_at_max() {
        assert_eq!(next_reconnect_delay(Duration::from_secs(8)), RECONNECT_BACKOFF_MAX);
        assert_eq!(next_reconnect_delay(RECONNECT_BACKOFF_MAX), RECONNECT_BACKOFF_MAX);
        assert_eq!(next_reconnect_delay(Duration::from_secs(300)), RECONNECT_BACKOFF_MAX);
    }

    // Regression: the client's ping cadence must key off outbound silence, not
    // inbound. If it keyed off inbound activity, a session with steady server
    // output (e.g. a full-screen TUI or tail -f) would never trigger the probe,
    // the client would never send a Ping, and the server's idle-evict would
    // close the connection. See commit 7fe1c08 for the idle-evict that exposed
    // this.
    #[tokio::test]
    async fn timed_send_updates_last_outbound_at() {
        use tokio::net::UnixStream;
        use tokio_util::codec::Framed;

        let (a, b) = UnixStream::pair().unwrap();
        let mut framed_a = Framed::new(a, FrameCodec);
        let mut framed_b = Framed::new(b, FrameCodec);

        let mut last_outbound_at = Instant::now();
        let before = last_outbound_at;
        tokio::time::sleep(Duration::from_millis(20)).await;

        assert!(
            timed_send(&mut framed_a, Frame::Ping, &mut last_outbound_at).await,
            "send should succeed",
        );
        assert!(last_outbound_at > before, "successful send must advance last_outbound_at",);

        // Drain the receiver so the pipe stays open.
        let _ = framed_b.next().await;
    }
}
