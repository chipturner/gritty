use crate::alt_screen::AltScreenTracker;
use crate::net_watch::{NetWatcher, PathStatus};
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

/// Configuration for a client session.
pub struct ClientConfig {
    pub session: String,
    pub session_id: u32,
    pub ctl_path: PathBuf,
    pub env_vars: Vec<(String, String)>,
    pub no_escape: bool,
    pub forward_agent: bool,
    pub forward_open: bool,
    pub oauth_redirect: bool,
    pub oauth_timeout: u64,
    pub heartbeat_interval: u64,
    pub heartbeat_timeout: u64,
    pub client_name: String,
    pub expected_server_id: u64,
    pub device_id: u64,
}

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

/// Flush any buffered input bytes as a `Data` action before emitting a control
/// action. Every control transition must do this first (so buffered keystrokes
/// reach the server in order), so the ordering invariant is named once here
/// instead of open-coded at each `AfterTilde` arm.
fn flush_pending(actions: &mut Vec<EscapeAction>, data_buf: &mut Vec<u8>) {
    if !data_buf.is_empty() {
        actions.push(EscapeAction::Data(std::mem::take(data_buf)));
    }
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
                        // Buffer the tilde -- don't send yet
                        flush_pending(&mut actions, &mut data_buf);
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
                            flush_pending(&mut actions, &mut data_buf);
                            actions.push(EscapeAction::Detach);
                            return actions; // Stop processing
                        }
                        b'R' => {
                            flush_pending(&mut actions, &mut data_buf);
                            actions.push(EscapeAction::Reconnect);
                            self.state = EscapeState::Normal;
                            return actions; // Stop processing
                        }
                        0x1a => {
                            // Ctrl-Z
                            flush_pending(&mut actions, &mut data_buf);
                            actions.push(EscapeAction::Suspend);
                            self.state = EscapeState::Normal;
                        }
                        b'#' => {
                            flush_pending(&mut actions, &mut data_buf);
                            actions.push(EscapeAction::Status);
                            self.state = EscapeState::Normal;
                        }
                        b'?' => {
                            flush_pending(&mut actions, &mut data_buf);
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

        flush_pending(&mut actions, &mut data_buf);
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

/// Decide whether a handshake error should abort the reconnect loop.
///
/// `handshake rejected` is always terminal -- the server spoke and refused us.
///
/// `daemon closed connection` / `daemon protocol error` (accept-then-EOF) is
/// only terminal when there is **no** tunnel supervisor: for `local` that
/// means the daemon was kill-server'd and retrying can't help. Under a live
/// supervisor, accept-then-EOF is the normal signature of ssh dying mid-
/// handshake (ServerAlive timeout or supervisor-kill after wake-from-suspend);
/// the supervisor will respawn ssh and re-run `ensure_remote_ready`, so the
/// client should keep retrying.
fn is_terminal_handshake_err(msg: &str, tunnel_supervisor_alive: bool) -> bool {
    if msg.starts_with("handshake rejected") {
        return true;
    }
    if tunnel_supervisor_alive {
        return false;
    }
    msg.starts_with("daemon closed connection") || msg.starts_with("daemon protocol error")
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

// ---------------------------------------------------------------------------
// Reconnect status-line chrome
//
// The reconnect loop owns one line at the bottom of the terminal and repaints
// it in place (`\r` + `\x1b[K`). These helpers are the single source of truth
// for that line's look so the interactive and tail paths stay in lockstep.
// ---------------------------------------------------------------------------

/// Braille spinner frames -- same set cargo, npm, et al. use.
const SPINNER: &[char] = &['⠋', '⠙', '⠹', '⠸', '⠼', '⠴', '⠦', '⠧', '⠇', '⠏'];

/// Cosmetic refresh cadence for the reconnect status line. Drives the spinner
/// and keeps the elapsed counter advancing smoothly between attempts (backoff
/// sleeps are 1..10s, long enough to otherwise look frozen).
const RECONNECT_SPIN_INTERVAL: Duration = Duration::from_millis(120);

/// Grace period before the reconnect status line is painted at all. A
/// reconnect that completes within this window leaves the terminal untouched
/// -- no line reserved, no cursor moved -- so the common sub-second blip (lid
/// crack, wifi handoff) resumes seamlessly. Only a reconnect that visibly
/// drags gets chrome.
const RECONNECT_CHROME_DELAY: Duration = Duration::from_secs(1);

/// What the reconnect status line is reporting. Determines the body text.
#[derive(Clone, Copy, PartialEq, Eq)]
enum ReconnectPhase {
    /// Actively retrying; show elapsed seconds.
    Retrying,
    /// OS reports no usable route; parked on `net.changed()`.
    WaitingForNetwork,
}

/// Render the in-place reconnect status line. `spin` indexes the spinner
/// frames (any monotonically increasing counter); `elapsed_s` is seconds since
/// the loop started (or since the network came back).
fn reconnect_status_line(spin: usize, elapsed_s: u64, phase: ReconnectPhase) -> String {
    let glyph = SPINNER[spin % SPINNER.len()];
    let body = match phase {
        ReconnectPhase::Retrying => format!("reconnecting {elapsed_s}s"),
        ReconnectPhase::WaitingForNetwork => "waiting for network".to_string(),
    };
    format!("\r\x1b[2m{glyph} {body} \u{b7} ^C aborts\x1b[0m\x1b[K")
}

/// Terminal failure line: replaces the status line, then newline. Caller is
/// responsible for leaving alt-screen first if needed.
fn reconnect_err_line(text: &str) -> String {
    format!("\r\x1b[31m\u{25b8} {text}\x1b[0m\x1b[K\r\n")
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
            info!("link down: send error: {e}");
            false
        }
        Err(_) => {
            info!(timeout_s = SEND_TIMEOUT.as_secs(), "link down: send timed out");
            false
        }
    }
}

const DEFAULT_HEARTBEAT_INTERVAL: Duration = Duration::from_secs(10);
const DEFAULT_HEARTBEAT_TIMEOUT: Duration = Duration::from_secs(60);

/// Wall-clock elapsed between `since` and `now`. `Instant` on Linux uses
/// `CLOCK_MONOTONIC` which pauses during laptop suspend, so `Instant::elapsed`
/// under-reports silence across a lid-close. `SystemTime` keeps advancing, so
/// we use it to drive idle detection. Returns `Duration::ZERO` if the clock
/// moved backward (NTP correction, manual set) so we never declare the link
/// idle from a clock adjustment.
fn wall_elapsed(since: std::time::SystemTime, now: std::time::SystemTime) -> Duration {
    now.duration_since(since).unwrap_or(Duration::ZERO)
}

/// True when more than `hb_timeout` of wall-clock time has passed since the
/// server last proved it was alive. Shared by the heartbeat tick and the stdin
/// arm so a keystroke after laptop wake short-circuits straight to reconnect
/// instead of stalling in `timed_send` against a dead socket.
fn link_is_stale(last_activity: std::time::SystemTime, hb_timeout: Duration) -> bool {
    wall_elapsed(last_activity, std::time::SystemTime::now()) > hb_timeout
}

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
/// Channel id reserved as the "declined TunnelListen" sentinel. The client
/// sends `TunnelClose { channel_id: TUNNEL_DECLINE_CHANNEL }` when it refuses
/// or fails a `TunnelListen`; accepted connections are numbered from
/// `TUNNEL_DECLINE_CHANNEL + 1` so the sentinel can never collide with a live
/// channel (a decline must not tear down an in-flight OAuth callback).
const TUNNEL_DECLINE_CHANNEL: u32 = 0;

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
            next_channel_id: Arc::new(AtomicU32::new(TUNNEL_DECLINE_CHANNEL + 1)),
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
    /// Local-forward requests waiting for server PortForwardReady/Stop.
    pending_lf: HashMap<u32, tokio::net::UnixStream>,
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
            pending_lf: HashMap::new(),
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
        self.pending_lf.clear();
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
    last_activity: &'a mut std::time::SystemTime,
    last_outbound_at: &'a mut Instant,
    last_ping_sent: &'a mut Instant,
    last_rtt: &'a mut Option<Duration>,
    connected_at: Instant,
    bytes_relayed: &'a mut u64,
    /// Authoritative position in the PTY output stream -- see `client::run`.
    rendered_offset: &'a mut u64,
    url_limiter: &'a mut RateLimiter,
    clipboard_set_limiter: &'a mut RateLimiter,
    tunnel_listen_limiter: &'a mut RateLimiter,
    agent_open_limiter: &'a mut RateLimiter,
}

impl ClientRelay<'_> {
    /// Send a frame whose delivery is required for progress. Returns `false`
    /// if the link is down (caller bails to the reconnect loop). Threads
    /// `last_outbound_at` so the heartbeat keys off our own sends.
    async fn send(&mut self, framed: &mut Framed<UnixStream, FrameCodec>, frame: Frame) -> bool {
        timed_send(framed, frame, self.last_outbound_at).await
    }

    /// Best-effort send: a failed delivery just means the link is down, which
    /// the next required send or the heartbeat will surface. Use for frames
    /// whose loss is not independently fatal (channel teardown, acks, etc.).
    async fn notify(&mut self, framed: &mut Framed<UnixStream, FrameCodec>, frame: Frame) {
        let _ = timed_send(framed, frame, self.last_outbound_at).await;
    }

    async fn handle_server_frame(
        &mut self,
        framed: &mut Framed<UnixStream, FrameCodec>,
        frame: Option<Result<Frame, io::Error>>,
    ) -> Result<ControlFlow<RelayExit>, anyhow::Error> {
        // Any frame received from the server is proof-of-life; reset the idle
        // timer. We use SystemTime (not Instant) so the timer keeps advancing
        // across laptop suspend -- Instant is CLOCK_MONOTONIC on Linux and
        // pauses during suspend, which would hide silence that accumulated
        // while the lid was closed.
        if matches!(frame, Some(Ok(_))) {
            *self.last_activity = std::time::SystemTime::now();
        }
        match frame {
            Some(Ok(Frame::Data(data))) => {
                debug!(len = data.len(), "socket → stdout");
                *self.bytes_relayed += data.len() as u64;
                // Data is the live PTY stream: advance our rendered position
                // so the next reconnect's Attach can ask for exactly the tail
                // we missed.
                *self.rendered_offset += data.len() as u64;
                self.alt_screen.scan(&data);
                write_stdout_async(self.async_stdout, &data).await?;
            }
            Some(Ok(Frame::Resume { offset })) => {
                // The server is telling us our authoritative stream position
                // after a reconnect handoff. Trust it verbatim: on a clean
                // resume it equals what we counted; on a truncated or
                // full-repaint resume it jumps us forward past bytes we will
                // never render byte-for-byte.
                debug!(offset, "resume offset from server");
                *self.rendered_offset = offset;
            }
            Some(Ok(Frame::Notice(data))) => {
                // Server chrome (dividers, truncation markers, takeover
                // banners, alt-screen priming, line repaints). Render it but
                // do NOT count it -- it is not part of the PTY stream.
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
                // Leave alt-screen first, mirroring the ServerShutdown arm: if
                // this client was in a TUI the message would otherwise land in
                // the alt-screen buffer and be discarded by TerminalResetGuard.
                write_stdout_async(
                    self.async_stdout,
                    format!("\x1b[?1049l{}", status_msg("detached")).as_bytes(),
                )
                .await?;
                return Ok(ControlFlow::Break(RelayExit::Exit(0)));
            }
            Some(Ok(Frame::ServerShutdown)) => {
                // Daemon told us it's going away (kill-server / SIGTERM).
                // Terminal: the session is gone, reconnecting won't help.
                // Without this explicit goodbye a remote client would spin
                // the reconnect loop for minutes until the tunnel supervisor
                // happens to restart the server with a new server_id.
                info!("server shutting down");
                self.agent.teardown();
                self.tunnel.teardown();
                self.pf.teardown();
                // Leave alt-screen first so the message is visible and not
                // clobbered by RawModeGuard's Drop. No-op on main screen.
                write_stdout_async(
                    self.async_stdout,
                    format!(
                        "\x1b[?1049l{}",
                        reconnect_err_line(
                            "server shut down -- session is gone; run `gritty connect` to start fresh"
                        )
                    )
                    .as_bytes(),
                )
                .await?;
                return Ok(ControlFlow::Break(RelayExit::Exit(1)));
            }
            Some(Ok(Frame::AgentOpen { channel_id })) => {
                if let Some(sock_path) =
                    self.agent_socket.filter(|_| self.agent_open_limiter.check())
                {
                    match tokio::net::UnixStream::connect(sock_path).await {
                        Ok(stream) => {
                            let (read_half, write_half) = stream.into_split();
                            let (writer_tx, writer_rx) = crate::relay_writer_channel();
                            self.agent.channels.insert(channel_id, writer_tx);
                            let data_tx = self.agent_event_tx.clone();
                            let close_tx = self.agent_event_tx.clone();
                            crate::spawn_channel_relay(
                                channel_id,
                                read_half,
                                write_half,
                                writer_rx,
                                move |id, data| {
                                    data_tx.send(AgentEvent::Data { channel_id: id, data }).is_ok()
                                },
                                move |id| {
                                    let _ = close_tx.send(AgentEvent::Closed { channel_id: id });
                                },
                            );
                        }
                        Err(e) => {
                            debug!("failed to connect to local agent: {e}");
                            self.notify(framed, Frame::AgentClose { channel_id }).await;
                        }
                    }
                } else {
                    self.notify(framed, Frame::AgentClose { channel_id }).await;
                }
            }
            Some(Ok(Frame::AgentData { channel_id, data })) => {
                if self.agent.channels.get(&channel_id).is_some_and(|tx| tx.try_send(data).is_err())
                {
                    warn!(channel_id, "agent channel backpressured, closing");
                    self.agent.channels.remove(&channel_id);
                    // Notify the server so its half tears down too.
                    self.notify(framed, Frame::AgentClose { channel_id }).await;
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
                self.notify(framed, Frame::ClipboardData { data: Bytes::new() }).await;
            }
            Some(Ok(Frame::TunnelListen { port })) => {
                if !self.oauth_redirect {
                    debug!(port, "tunnel: oauth-redirect disabled, declining");
                    self.notify(framed, Frame::TunnelClose { channel_id: TUNNEL_DECLINE_CHANNEL })
                        .await;
                } else if !self.tunnel_listen_limiter.check() {
                    warn!(port, "rate limited TunnelListen");
                    self.notify(framed, Frame::TunnelClose { channel_id: TUNNEL_DECLINE_CHANNEL })
                        .await;
                } else {
                    // A prior TunnelListen within the rate window may have left
                    // an accept loop running. Dropping a JoinHandle detaches
                    // rather than cancels, so abort it before binding/replacing
                    // -- otherwise the task and its bound port leak until the
                    // old deadline elapses (and a same-port rebind hits
                    // EADDRINUSE). Mirrors ClientTunnelState::teardown().
                    if let Some(old) = self.tunnel.listener.take() {
                        old.abort();
                    }
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
                                            let _ = stream.set_nodelay(true);
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
                            self.notify(
                                framed,
                                Frame::TunnelClose { channel_id: TUNNEL_DECLINE_CHANNEL },
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
                    // Notify the server so its half tears down too.
                    self.notify(framed, Frame::TunnelClose { channel_id }).await;
                }
            }
            Some(Ok(Frame::TunnelClose { channel_id })) => {
                self.tunnel.channels.remove(&channel_id);
            }
            // Port forward: server asks client to bind a port -- rejected (client-initiated only)
            Some(Ok(Frame::PortForwardListen { forward_id, listen_port, .. })) => {
                warn!(forward_id, listen_port, "rejected server-initiated port bind");
                self.notify(framed, Frame::PortForwardStop { forward_id }).await;
            }
            // Port forward: new TCP connection from server side (only accept client-initiated)
            Some(Ok(Frame::PortForwardOpen { forward_id, channel_id, target_port })) => {
                let Some(&expected_port) = self.client_initiated_forwards.get(&forward_id) else {
                    warn!(forward_id, target_port, "rejected unsolicited port forward open");
                    self.notify(framed, Frame::PortForwardClose { channel_id }).await;
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
                        let _ = stream.set_nodelay(true);
                        let (read_half, write_half) = stream.into_split();
                        let (writer_tx, writer_rx) = crate::relay_writer_channel();
                        self.pf.channels.insert(channel_id, (forward_id, writer_tx));
                        let data_tx = self.pf_event_tx.clone();
                        let close_tx = self.pf_event_tx.clone();
                        crate::spawn_channel_relay(
                            channel_id,
                            read_half,
                            write_half,
                            writer_rx,
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
                    }
                    Err(e) => {
                        debug!(channel_id, expected_port, "pf connect failed: {e}");
                        self.notify(framed, Frame::PortForwardClose { channel_id }).await;
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
                    // Notify the server so its half tears down too.
                    self.notify(framed, Frame::PortForwardClose { channel_id }).await;
                }
            }
            // Port forward: channel closed by server
            Some(Ok(Frame::PortForwardClose { channel_id })) => {
                self.pf.channels.remove(&channel_id);
            }
            // Port forward: server confirmed bind
            Some(Ok(Frame::PortForwardReady { forward_id })) => {
                if let Some(mut fwd_stream) = self.pf.pending_lf.remove(&forward_id) {
                    use tokio::io::AsyncWriteExt;
                    info!(forward_id, "local-forward: server confirmed bind");
                    let _ = fwd_stream.write_all(&[0x01]).await;
                    let target_port =
                        self.client_initiated_forwards.get(&forward_id).copied().unwrap_or(0);
                    self.start_pf_keepalive(forward_id, fwd_stream, target_port);
                }
            }
            // Port forward: teardown from server
            Some(Ok(Frame::PortForwardStop { forward_id })) => {
                // If this was a pending local-forward, notify the `lf` process
                // of the bind failure before it prints "active".
                if let Some(mut fwd_stream) = self.pf.pending_lf.remove(&forward_id) {
                    use tokio::io::AsyncWriteExt;
                    warn!(forward_id, "local-forward: server bind failed");
                    let _ = fwd_stream.write_all(&[0x02]).await;
                    let _ = fwd_stream.write_all(b"server bind failed").await;
                }
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
                info!("link down: server connection error: {e}");
                return Ok(ControlFlow::Break(RelayExit::Disconnected));
            }
            None => {
                info!("link down: server stream closed");
                return Ok(ControlFlow::Break(RelayExit::Disconnected));
            }
        }
        Ok(ControlFlow::Continue(()))
    }

    #[allow(clippy::collapsible_match)]
    async fn handle_agent_event(
        &mut self,
        framed: &mut Framed<UnixStream, FrameCodec>,
        event: Option<AgentEvent>,
    ) -> bool {
        match event {
            Some(AgentEvent::Data { channel_id, data }) => {
                if self.agent.channels.contains_key(&channel_id)
                    && !self.send(framed, Frame::AgentData { channel_id, data }).await
                {
                    return false;
                }
            }
            Some(AgentEvent::Closed { channel_id }) => {
                if self.agent.channels.remove(&channel_id).is_some()
                    && !self.send(framed, Frame::AgentClose { channel_id }).await
                {
                    return false;
                }
            }
            None => {} // no more agent tasks
        }
        true
    }

    #[allow(clippy::collapsible_match)]
    async fn handle_tunnel_event(
        &mut self,
        framed: &mut Framed<UnixStream, FrameCodec>,
        event: Option<ClientTunnelEvent>,
    ) -> bool {
        match event {
            Some(ClientTunnelEvent::Accepted { channel_id, writer_tx }) => {
                self.tunnel.channels.insert(channel_id, writer_tx);
                if !self.send(framed, Frame::TunnelOpen { channel_id }).await {
                    return false;
                }
            }
            Some(ClientTunnelEvent::Data { channel_id, data }) => {
                if !self.send(framed, Frame::TunnelData { channel_id, data }).await {
                    return false;
                }
            }
            Some(ClientTunnelEvent::Closed { channel_id }) => {
                self.tunnel.channels.remove(&channel_id);
                if !self.send(framed, Frame::TunnelClose { channel_id }).await {
                    return false;
                }
            }
            None => {}
        }
        true
    }

    #[allow(clippy::collapsible_match)]
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
                    if !self
                        .send(
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
                    && !self.send(framed, Frame::PortForwardData { channel_id, data }).await
                {
                    return false;
                }
            }
            Some(ClientPortForwardEvent::Closed { channel_id }) => {
                if self.pf.channels.remove(&channel_id).is_some()
                    && !self.send(framed, Frame::PortForwardClose { channel_id }).await
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
                if !self.send(framed, Frame::PortForwardStop { forward_id }).await {
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
                            let _ = stream.set_nodelay(true);
                            let channel_id = nid.fetch_add(2, Ordering::Relaxed);
                            let (read_half, write_half) = stream.into_split();
                            let (writer_tx, writer_rx) = crate::relay_writer_channel();
                            // Accepted MUST be enqueued before the reader task
                            // can enqueue Data, or the relay loop drops Data
                            // for an unknown channel.
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
                            let data_tx = tx.clone();
                            let close_tx = tx.clone();
                            crate::spawn_channel_relay(
                                channel_id,
                                read_half,
                                write_half,
                                writer_rx,
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
        if !self
            .send(
                framed,
                Frame::PortForwardRequest { forward_id, direction, listen_port, target_port },
            )
            .await
        {
            let _ = fwd_stream.write_all(&[0x02]).await;
            let _ = fwd_stream.write_all(b"server connection lost").await;
            return;
        }

        if direction == 0 {
            // Local-forward: defer the success response until the server
            // confirms the bind with PortForwardReady (or rejects with
            // PortForwardStop). Responding immediately would tell `gritty lf`
            // the forward is "active" before the server has actually bound.
            self.pf.pending_lf.insert(forward_id, fwd_stream);
            return;
        }

        info!(forward_id, direction, listen_port, target_port, "port forward established");
        let _ = fwd_stream.write_all(&[0x01]).await;

        self.start_pf_keepalive(forward_id, fwd_stream, target_port);
    }

    fn start_pf_keepalive(
        &mut self,
        forward_id: u32,
        mut fwd_stream: tokio::net::UnixStream,
        target_port: u16,
    ) {
        use tokio::io::AsyncReadExt;

        // Keepalive: when the controlling process disconnects, tear down the forward.
        let pf_tx = self.pf_event_tx.clone();
        let keepalive_handle = tokio::spawn(async move {
            let mut buf = [0u8; 1];
            let _ = fwd_stream.read(&mut buf).await;
            let _ = pf_tx.send(ClientPortForwardEvent::ForwardStopped { forward_id });
        });
        // Track the keepalive task (and for lf, create the forwards entry) so teardown
        // aborts it -- dropping fwd_stream lets the `gritty lf`/`rf` process see EOF.
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
    net: &NetWatcher,
    rendered_offset: &mut u64,
) -> anyhow::Result<RelayExit> {
    let mut heartbeat_interval = tokio::time::interval(hb_interval);
    heartbeat_interval.reset(); // first tick is immediate otherwise; delay it
    // SystemTime (not Instant) because Instant pauses during laptop suspend on
    // Linux, which would hide silence accumulated across a lid-close. We don't
    // detect suspend; we just observe that no server frame has arrived in N
    // seconds of wall-clock time.
    let mut last_activity = std::time::SystemTime::now();
    // Timestamp of the last frame we successfully sent to the server. The
    // ping cadence is driven off this (not last_activity) so steady inbound
    // server output doesn't suppress the probes the server uses for its
    // idle-evict decision.
    let mut last_outbound_at = Instant::now();
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
        rendered_offset,
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
                        // Wake-from-suspend fast path: the heartbeat tick is
                        // monotonic and may not fire for up to hb_interval
                        // after wake, so a keystroke would otherwise stall in
                        // timed_send against a dead socket for SEND_TIMEOUT.
                        // Check wall-clock staleness here and go straight to
                        // reconnect. The keystroke is dropped -- that's fine;
                        // the user is about to see the reconnect banner.
                        if link_is_stale(*relay.last_activity, hb_timeout) {
                            info!("link down: stdin after stale link (wall-clock gap); reconnecting without send");
                            return Ok(RelayExit::Disconnected);
                        }
                        if let Some(ref mut esc) = escape {
                            for action in esc.process(&buf[..n]) {
                                match action {
                                    EscapeAction::Data(data) => {
                                        if !timed_send(framed, Frame::Data(Bytes::from(data)), relay.last_outbound_at).await {
                                            return Ok(RelayExit::Disconnected);
                                        }
                                    }
                                    EscapeAction::Detach => {
                                        // Leave alt-screen first so the message
                                        // isn't discarded from the alt buffer
                                        // by TerminalResetGuard on exit.
                                        write_stdout_async(
                                            async_stdout,
                                            format!("\x1b[?1049l{}", status_msg("detached")).as_bytes(),
                                        ).await?;
                                        return Ok(RelayExit::Exit(0));
                                    }
                                    EscapeAction::Reconnect => {
                                        // No immediate banner. A status_msg here
                                        // moves the cursor (\r\n...\r\n), but a
                                        // sub-second ~R reconnect sends
                                        // line_dirty=false and never repairs it,
                                        // splitting the resumed line. The normal
                                        // reconnect chrome (animated status line
                                        // after RECONNECT_CHROME_DELAY) is the
                                        // feedback; a fast ~R stays invisible,
                                        // like any other seamless reconnect.
                                        return Ok(RelayExit::Disconnected);
                                    }
                                    EscapeAction::Suspend => {
                                        suspend(raw_guard, nb_guard)?;
                                        // Avoid a spurious idle-timeout after returning from SIGTSTP.
                                        heartbeat_interval = tokio::time::interval(hb_interval);
                                        heartbeat_interval.reset();
                                        *relay.last_activity = std::time::SystemTime::now();
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

            _ = net.changed() => {
                // OS reports the network path changed (wifi/ethernet/VPN).
                // Advisory only: send a Ping now so a dead socket surfaces as
                // a send failure immediately and a live one round-trips a
                // Pong that refreshes last_activity. The wall-clock heartbeat
                // below remains the correctness backstop.
                debug!(status = ?net.status(), "network path changed during active relay; probing link");
                *relay.last_ping_sent = Instant::now();
                if !timed_send(framed, Frame::Ping, relay.last_outbound_at).await {
                    return Ok(RelayExit::Disconnected);
                }
            }

            _ = heartbeat_interval.tick() => {
                // Wall-clock silence check: works correctly across laptop
                // suspend because SystemTime keeps advancing while the process
                // is frozen. No suspend heuristics -- just "have we heard from
                // the server in the last N seconds of real time?"
                if link_is_stale(*relay.last_activity, hb_timeout) {
                    info!(
                        idle_s = wall_elapsed(*relay.last_activity, std::time::SystemTime::now()).as_secs(),
                        "link down: heartbeat idle timeout"
                    );
                    return Ok(RelayExit::Disconnected);
                }

                // Fire a Ping when we've been outbound-silent for a tick (so
                // the server's idle-evict sees us alive -- steady inbound
                // server output does not prove the client can still send), OR
                // inbound-silent for a tick (so a Pong refreshes last_activity
                // before it ages out to hb_timeout). Keying only off
                // last_outbound_at let a sustained one-way client->server
                // stream (a port-forward upload, no-echo typing) refresh
                // last_outbound_at on every send and suppress Pings
                // indefinitely, while last_activity aged into a spurious
                // disconnect that tore down all forwarding state.
                let inbound_idle = wall_elapsed(
                    *relay.last_activity,
                    std::time::SystemTime::now(),
                ) >= hb_interval;
                if relay.last_outbound_at.elapsed() >= hb_interval || inbound_idle {
                    *relay.last_ping_sent = Instant::now();
                    if !timed_send(framed, Frame::Ping, relay.last_outbound_at).await {
                        return Ok(RelayExit::Disconnected);
                    }
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

pub async fn run(
    mut framed: Framed<UnixStream, FrameCodec>,
    config: ClientConfig,
) -> anyhow::Result<i32> {
    let ClientConfig {
        session,
        session_id,
        ctl_path,
        env_vars,
        no_escape,
        forward_agent,
        forward_open,
        oauth_redirect,
        oauth_timeout,
        heartbeat_interval,
        heartbeat_timeout,
        client_name,
        expected_server_id,
        device_id,
    } = config;
    let session = &session;
    let ctl_path = &ctl_path;
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
    let net = NetWatcher::spawn();
    // How far we have rendered into the session's PTY output stream. Counts
    // `Data` payload bytes; a `Resume` frame from the server overrides it on
    // reconnect. Persists across reconnects so the next `Attach` can ask the
    // server to replay only what we missed.
    let mut rendered_offset: u64 = 0;
    // Mirrors the server's alt-screen tracker so we can suppress our own
    // reconnect chrome when a TUI is running -- those writes would otherwise
    // land directly in the alt screen buffer and corrupt it.
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
                &net,
                &mut rendered_offset,
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
                // Whether the reconnect status line is currently on screen.
                // Stays false until the reconnect drags past
                // `RECONNECT_CHROME_DELAY`; once painted it stays for the rest
                // of this reconnect episode. `line_dirty` in the Attach frame
                // is derived from it so the server knows to repaint the
                // cursor's line.
                let mut chrome_shown = false;
                let mut reconnect_started = Instant::now();
                // Timestamp of the most recent observation that ctl_path did
                // NOT exist. Cleared whenever the socket reappears or a probe
                // succeeds. If it persists past SOCKET_GONE_GRACE we treat
                // the tunnel/server as torn down (e.g. `gritty tunnel-destroy`
                // removed the socket file) and exit instead of looping.
                let mut socket_missing_since: Option<Instant> = None;
                const SOCKET_GONE_GRACE: Duration = Duration::from_secs(3);
                let mut backoff = Duration::ZERO;
                let mut was_offline = false;
                let mut attempt_n = 0u32;
                // Path-status gating only makes sense for tunnel sockets --
                // a local Unix socket doesn't need a network route.
                // Spinner / elapsed counter. Purely cosmetic -- keeps the
                // status line visibly alive between attempts (backoff sleeps
                // run 1..10s) and during a slow attempt (tunnel up but remote
                // handshake stalled).
                let mut spin: usize = 0;
                let mut tick = tokio::time::interval(RECONNECT_SPIN_INTERVAL);
                tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
                let is_remote = crate::connect::ctl_socket_lock_path(ctl_path).is_some();
                info!(is_remote, path_status = ?net.status(), "entering reconnect loop");

                // Paint (or first-time open) the reconnect status line, but
                // only once the reconnect has dragged past the chrome-delay
                // grace period -- a fast reconnect leaves the terminal
                // untouched. A no-op in alt-screen or inside the grace window.
                //
                // `$first_paint_ok` gates the *first* paint -- the `\r\n` that
                // opens the status line's row and moves the cursor. It must be
                // false while a reconnect attempt is in flight: that attempt's
                // Attach frame already carried `line_dirty = chrome_shown`, and
                // first-painting mid-attempt would move the cursor without the
                // server knowing to repaint the line on resume. Deferred first
                // paints land in the next backoff wait, before the following
                // attempt snapshots `line_dirty`.
                macro_rules! paint_reconnect_status {
                    ($phase:expr) => {
                        paint_reconnect_status!($phase, true)
                    };
                    ($phase:expr, $first_paint_ok:expr) => {{
                        if show_chrome && reconnect_started.elapsed() >= RECONNECT_CHROME_DELAY {
                            if !chrome_shown && $first_paint_ok {
                                // Open a fresh line below the user's content;
                                // we repaint it in place from here on.
                                write_stdout_async(&async_stdout, b"\r\n").await?;
                                chrome_shown = true;
                            }
                            if chrome_shown {
                                let elapsed = reconnect_started.elapsed().as_secs();
                                write_stdout_async(
                                    &async_stdout,
                                    reconnect_status_line(spin, elapsed, $phase).as_bytes(),
                                )
                                .await?;
                            }
                        }
                    }};
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
                    let deadline = tokio::time::Instant::now() + sleep_for;
                    let mut net_hint = false;
                    // Wait out the backoff, animating the status line. Each
                    // arm either breaks to attempt a connect or returns.
                    loop {
                        tokio::select! {
                            _ = tokio::time::sleep_until(deadline) => break,
                            _ = tick.tick() => {
                                spin = spin.wrapping_add(1);
                                let phase = if is_remote && net.status() == PathStatus::Unsatisfied {
                                    ReconnectPhase::WaitingForNetwork
                                } else {
                                    ReconnectPhase::Retrying
                                };
                                paint_reconnect_status!(phase);
                            }
                            _ = net.changed() => {
                                // Network path changed while backing off --
                                // connectivity may have returned. Attempt now
                                // and reset the backoff so a long outage
                                // followed by wifi-returns doesn't sit out the
                                // remainder of a 10s sleep.
                                debug!(status = ?net.status(), "network path changed during reconnect backoff");
                                net_hint = true;
                                break;
                            }
                            _ = sigterm.recv() => {
                                info!("reconnect: exiting -- SIGTERM");
                                write_stdout_async(&async_stdout, b"\r\n").await?;
                                return Ok(1);
                            }
                            _ = sighup.recv() => {
                                info!("reconnect: exiting -- SIGHUP");
                                write_stdout_async(&async_stdout, b"\r\n").await?;
                                return Ok(1);
                            }
                            ready = async_stdin.readable() => {
                                let mut guard = ready?;
                                let mut peek = [0u8; 1];
                                match guard.try_io(|inner| inner.get_ref().read(&mut peek)) {
                                    Ok(Ok(1)) if peek[0] == 0x03 => {
                                        info!("reconnect: exiting -- Ctrl-C");
                                        write_stdout_async(&async_stdout, b"\r\n").await?;
                                        return Ok(1);
                                    }
                                    Ok(Ok(0)) | Ok(Err(_)) => {
                                        info!("reconnect: exiting -- stdin EOF/error");
                                        return Ok(1);
                                    }
                                    _ => {}
                                }
                                // Impatient keystroke (not Ctrl-C) cuts the
                                // current sleep short and triggers an attempt
                                // now rather than restarting the outer loop --
                                // restarting re-raced the same sleep, so a
                                // user holding a key (>1 keystroke/sec)
                                // starved every reconnect attempt.
                                break;
                            }
                        }
                    }

                    // Path-status gate: while the OS says there's no usable
                    // route (lid closed / wifi down), don't burn 15s attempts
                    // against a dead interface -- park on net.changed() via
                    // the select above. On the unsatisfied->satisfied edge,
                    // reset the elapsed counter and backoff so the user sees
                    // "reconnecting 0s" from the moment the network actually
                    // returned, not from whenever the loop first entered
                    // (which may include hours of lid-closed time).
                    if is_remote && net.status() == PathStatus::Unsatisfied {
                        if !was_offline {
                            info!("reconnect: path unsatisfied, parking until network returns");
                        }
                        was_offline = true;
                        continue;
                    }
                    if was_offline {
                        info!(
                            "reconnect: path available, resuming attempts (backoff and timer reset)"
                        );
                        was_offline = false;
                        reconnect_started = Instant::now();
                        net_hint = true;
                    }
                    backoff = if net_hint { Duration::ZERO } else { sleep_for };

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
                            info!(
                                gone_s = first_seen.elapsed().as_secs_f64(),
                                "reconnect: exiting -- ctl socket gone and no supervisor"
                            );
                            // `\x1b[?1049l` leaves alt-screen first so the
                            // error is visible on main screen -- otherwise
                            // RawModeGuard's Drop emits it after the fact
                            // and clobbers the message. No-op on main
                            // screen.
                            write_stdout_async(
                                &async_stdout,
                                format!(
                                    "\x1b[?1049l{}",
                                    reconnect_err_line(
                                        "server socket gone -- session is unreachable; reconnect manually"
                                    )
                                )
                                .as_bytes(),
                            )
                            .await?;
                            return Ok(1);
                        }
                        continue;
                    } else {
                        socket_missing_since = None;
                    }

                    enum Attempt {
                        Connected(Framed<UnixStream, FrameCodec>),
                        SessionGone(String),
                        ServerRestarted,
                        OwnerChanged,
                        VersionMismatch { server_version: u16 },
                        HandshakeErr(String),
                        DaemonGone,
                        Retry,
                    }

                    attempt_n += 1;
                    let attempt_started = Instant::now();
                    debug!(
                        attempt = attempt_n,
                        backoff_s = backoff.as_secs_f64(),
                        tunnel_supervisor_alive,
                        socket_exists = ctl_path.exists(),
                        "reconnect: attempting"
                    );
                    // Snapshot the chrome state for this attempt. The
                    // attempt-loop tick arm below paints with
                    // `first_paint_ok = false`, so `chrome_shown` cannot flip
                    // while the attempt is in flight -- this snapshot stays
                    // accurate through the Attach send and the success path.
                    // `line_dirty` tells the server our cursor left
                    // `rendered_offset`'s position and the current line needs
                    // a repaint on resume.
                    let line_dirty = chrome_shown;
                    let resume_offset = rendered_offset;
                    let attempt_fut = tokio::time::timeout(RECONNECT_ATTEMPT_TIMEOUT, async {
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
                        let info = match crate::handshake(&mut new_framed, device_id).await {
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
                                // Non-zero = auto-reconnect ownership claim.
                                attach_token: device_id,
                                // Resume the PTY stream from where we left off;
                                // the server replays only what we missed.
                                rendered_offset: resume_offset,
                                line_dirty,
                            })
                            .await
                            .is_err()
                        {
                            return Attempt::Retry;
                        }
                        match new_framed.next().await {
                            Some(Ok(Frame::AttachAck { token: _, session_id: _ })) => {
                                Attempt::Connected(new_framed)
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
                    });
                    tokio::pin!(attempt_fut);
                    // Keep the spinner alive during a slow attempt (e.g. tunnel
                    // socket up but remote handshake stalled). The attempt
                    // future owns the wire; the tick arm only touches stdout.
                    let attempt = loop {
                        tokio::select! {
                            r = &mut attempt_fut => break r,
                            _ = tick.tick() => {
                                spin = spin.wrapping_add(1);
                                // first_paint_ok = false: see the macro -- the
                                // Attach is already on the wire with a fixed
                                // `line_dirty`, so the cursor must not move now.
                                paint_reconnect_status!(ReconnectPhase::Retrying, false);
                            }
                        }
                    };

                    // Emit a terminal red line on stdout. `\x1b[?1049l` leaves
                    // alt-screen first so the error is visible on main screen
                    // -- otherwise RawModeGuard's Drop emits it after the fact
                    // and clobbers the message. No-op on main screen. `lead`
                    // opens a fresh line when no status line was ever painted,
                    // so the error doesn't overwrite the user's last output.
                    macro_rules! bail_reconnect {
                        ($text:expr) => {{
                            let lead = if chrome_shown { "" } else { "\r\n" };
                            write_stdout_async(
                                &async_stdout,
                                format!("\x1b[?1049l{lead}{}", reconnect_err_line($text))
                                    .as_bytes(),
                            )
                            .await?;
                            return Ok(1);
                        }};
                    }

                    let attempt_ms = attempt_started.elapsed().as_millis();
                    match attempt {
                        Ok(Attempt::Connected(new_framed)) => {
                            info!(
                                attempt = attempt_n,
                                attempt_ms,
                                total_s = reconnect_started.elapsed().as_secs_f64(),
                                "reconnect: connected"
                            );
                            if chrome_shown {
                                // Erase the status line and reclaim its row:
                                // clear the line, then step the cursor back up
                                // onto the line where the stream left off. The
                                // server's Resume/Notice/Data replay takes it
                                // from there.
                                write_stdout_async(&async_stdout, b"\r\x1b[K\x1b[A").await?;
                            }
                            framed = new_framed;
                            break;
                        }
                        Ok(Attempt::SessionGone(message)) => {
                            info!(
                                attempt = attempt_n,
                                "reconnect: exiting -- session gone: {message}"
                            );
                            bail_reconnect!(&format!("session gone: {message}"));
                        }
                        Ok(Attempt::ServerRestarted) => {
                            info!(
                                attempt = attempt_n,
                                "reconnect: exiting -- server_id changed (remote daemon restarted)"
                            );
                            bail_reconnect!(
                                "server restarted -- session is gone; reconnect manually"
                            );
                        }
                        Ok(Attempt::OwnerChanged) => {
                            info!(
                                attempt = attempt_n,
                                "reconnect: exiting -- owner_device_id mismatch (session taken over)"
                            );
                            bail_reconnect!("session taken over by another client");
                        }
                        Ok(Attempt::VersionMismatch { server_version }) => {
                            let local = crate::protocol::PROTOCOL_VERSION;
                            info!(
                                attempt = attempt_n,
                                local,
                                remote = server_version,
                                "reconnect: exiting -- protocol version mismatch"
                            );
                            bail_reconnect!(&format!(
                                "protocol version mismatch (local={local} remote={server_version}) -- run `gritty restart` to upgrade"
                            ));
                        }
                        Ok(Attempt::HandshakeErr(msg)) => {
                            if is_terminal_handshake_err(&msg, tunnel_supervisor_alive) {
                                info!(
                                    attempt = attempt_n,
                                    tunnel_supervisor_alive,
                                    "reconnect: exiting -- terminal handshake error: {msg}"
                                );
                                bail_reconnect!(&msg);
                            }
                            // Transient accept-then-EOF from a dying ssh -L
                            // during supervisor respawn -- identical to
                            // Attempt::Retry, just reached via handshake()
                            // instead of connect(). Do NOT write the red
                            // error or ?1049l here: it kicks the user out of
                            // alt-screen on every wake-from-suspend.
                            info!(
                                attempt = attempt_n,
                                attempt_ms,
                                "reconnect: transient handshake err ({msg}), will retry"
                            );
                            if tunnel_supervisor_alive {
                                backoff = Duration::ZERO;
                            }
                            continue;
                        }
                        Ok(Attempt::DaemonGone) => {
                            info!(
                                attempt = attempt_n,
                                "reconnect: exiting -- ECONNREFUSED on stale socket, no supervisor"
                            );
                            bail_reconnect!(
                                "daemon appears to have crashed -- session is gone; run `gritty server` or `gritty restart`"
                            );
                        }
                        Ok(Attempt::Retry) => {
                            debug!(
                                attempt = attempt_n,
                                attempt_ms, "reconnect: attempt failed, will retry"
                            );
                            // While the tunnel supervisor holds the lock,
                            // it is the rate-limiter (1-60s backoff on SSH
                            // respawn). The client's connect() is to a
                            // local Unix socket and costs nothing; layering
                            // our own backoff on top just delays noticing
                            // the forward came back. Poll at 1s instead.
                            if tunnel_supervisor_alive {
                                backoff = Duration::ZERO;
                            }
                            continue;
                        }
                        Err(_) => {
                            debug!(
                                attempt = attempt_n,
                                timeout_s = RECONNECT_ATTEMPT_TIMEOUT.as_secs(),
                                "reconnect: attempt timed out"
                            );
                            if tunnel_supervisor_alive {
                                backoff = Duration::ZERO;
                            }
                            continue;
                        }
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
    device_id: u64,
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
    // SystemTime (not Instant) because Instant pauses during laptop suspend on
    // Linux, hiding silence across a lid-close. See `wall_elapsed`.
    let mut last_activity = std::time::SystemTime::now();
    // Last time we successfully sent a frame; drives ping cadence so the
    // server's idle-evict doesn't fire during steady inbound traffic.
    let mut last_outbound_at = Instant::now();
    let mut sigint = signal(SignalKind::interrupt())?;
    let mut sigterm = signal(SignalKind::terminate())?;
    let mut sighup = signal(SignalKind::hangup())?;
    let mut stdout = tokio::io::stdout();
    // Track the PTY's alt-screen mode so we can suppress reconnect chrome
    // when tailing a session where a TUI is running.
    let mut alt_screen = AltScreenTracker::new();
    let net = NetWatcher::spawn();

    let code = 'outer: loop {
        let result = 'relay: loop {
            tokio::select! {
                frame = framed.next() => {
                    if matches!(frame, Some(Ok(_))) {
                        last_activity = std::time::SystemTime::now();
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
                        Some(Ok(Frame::ServerShutdown)) => {
                            info!("tail: server shutting down");
                            eprint!("{}", reconnect_err_line("server shut down -- session is gone"));
                            break 'relay Some(1);
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
                _ = net.changed() => {
                    debug!("network path changed; probing tail link");
                    if framed.send(Frame::Ping).await.is_err() {
                        break 'relay None;
                    }
                    last_outbound_at = Instant::now();
                }
                _ = heartbeat_interval.tick() => {
                    if link_is_stale(last_activity, DEFAULT_HEARTBEAT_TIMEOUT) {
                        debug!("tail idle timeout");
                        break 'relay None;
                    }
                    if last_outbound_at.elapsed() >= DEFAULT_HEARTBEAT_INTERVAL {
                        if framed.send(Frame::Ping).await.is_err() {
                            break 'relay None;
                        }
                        last_outbound_at = Instant::now();
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
                let show_chrome = !alt_screen.in_alternate_screen();
                // Delayed-chrome flag, same contract as the interactive
                // reconnect loop: nothing is painted until the reconnect drags
                // past `RECONNECT_CHROME_DELAY`.
                let mut chrome_shown = false;
                let mut reconnect_started = Instant::now();
                let mut socket_missing_since: Option<Instant> = None;
                let mut was_offline = false;
                let mut spin: usize = 0;
                let mut tick = tokio::time::interval(RECONNECT_SPIN_INTERVAL);
                tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
                let is_remote = crate::connect::ctl_socket_lock_path(ctl_path).is_some();
                const SOCKET_GONE_GRACE: Duration = Duration::from_secs(3);

                // Paint (or first-time open) the tail reconnect status line
                // once the reconnect has dragged past the grace period.
                macro_rules! paint_tail_status {
                    ($phase:expr) => {{
                        if show_chrome && reconnect_started.elapsed() >= RECONNECT_CHROME_DELAY {
                            if !chrome_shown {
                                eprint!("\r\n");
                                chrome_shown = true;
                            }
                            let elapsed = reconnect_started.elapsed().as_secs();
                            eprint!("{}", reconnect_status_line(spin, elapsed, $phase));
                        }
                    }};
                }

                loop {
                    let deadline = tokio::time::Instant::now() + Duration::from_secs(1);
                    loop {
                        tokio::select! {
                            _ = tokio::time::sleep_until(deadline) => break,
                            _ = tick.tick() => {
                                spin = spin.wrapping_add(1);
                                let phase = if is_remote && net.status() == PathStatus::Unsatisfied {
                                    ReconnectPhase::WaitingForNetwork
                                } else {
                                    ReconnectPhase::Retrying
                                };
                                paint_tail_status!(phase);
                            }
                            _ = net.changed() => break,
                            _ = sigint.recv() => { break 'outer 0; }
                            _ = sigterm.recv() => { break 'outer 1; }
                            _ = sighup.recv() => { break 'outer 1; }
                        }
                    }
                    if is_remote && net.status() == PathStatus::Unsatisfied {
                        was_offline = true;
                        continue;
                    }
                    if was_offline {
                        was_offline = false;
                        reconnect_started = Instant::now();
                    }

                    let tunnel_supervisor_alive = crate::connect::ctl_socket_lock_path(ctl_path)
                        .as_deref()
                        .is_some_and(crate::connect::is_lock_held);
                    if !ctl_path.exists() && !tunnel_supervisor_alive {
                        let first_seen = *socket_missing_since.get_or_insert_with(Instant::now);
                        if first_seen.elapsed() >= SOCKET_GONE_GRACE {
                            info!("tail reconnect: exiting -- ctl socket gone and no supervisor");
                            eprint!(
                                "{}",
                                reconnect_err_line(
                                    "server socket gone -- session is unreachable; reconnect manually"
                                )
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
                    let outcome_fut = tokio::time::timeout(RECONNECT_ATTEMPT_TIMEOUT, async {
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
                        let info = match crate::handshake(&mut new_framed, device_id).await {
                            Ok(info) => info,
                            Err(e) => {
                                let msg = e.to_string();
                                if is_terminal_handshake_err(&msg, tunnel_supervisor_alive) {
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
                    });
                    tokio::pin!(outcome_fut);
                    let outcome = loop {
                        tokio::select! {
                            r = &mut outcome_fut => break r,
                            _ = tick.tick() => {
                                spin = spin.wrapping_add(1);
                                paint_tail_status!(ReconnectPhase::Retrying);
                            }
                        }
                    };

                    macro_rules! bail_tail {
                        ($text:expr) => {{
                            let lead = if chrome_shown { "" } else { "\r\n" };
                            eprint!("{lead}{}", reconnect_err_line($text));
                            break 'outer 1;
                        }};
                    }

                    match outcome {
                        Ok(Outcome::Connected(new_framed)) => {
                            info!("tail reconnect: connected");
                            if chrome_shown {
                                // Erase the status line; the fresh tail stream
                                // flows onto the reclaimed row.
                                eprint!("\r\x1b[K");
                            }
                            framed = new_framed;
                            heartbeat_interval.reset();
                            last_activity = std::time::SystemTime::now();
                            break;
                        }
                        Ok(Outcome::ServerRestarted) => {
                            info!("tail reconnect: exiting -- server_id changed");
                            bail_tail!("server restarted -- session is gone; reconnect manually");
                        }
                        Ok(Outcome::VersionMismatch { local, remote }) => {
                            info!(
                                local,
                                remote, "tail reconnect: exiting -- protocol version mismatch"
                            );
                            bail_tail!(&format!(
                                "protocol version mismatch (local={local} remote={remote}) -- run `gritty restart` to upgrade"
                            ));
                        }
                        Ok(Outcome::HandshakeRejected(msg)) => {
                            info!("tail reconnect: exiting -- terminal handshake error: {msg}");
                            bail_tail!(&msg);
                        }
                        Ok(Outcome::SessionGone(message)) => {
                            info!("tail reconnect: exiting -- session gone: {message}");
                            bail_tail!(&format!("session gone: {message}"));
                        }
                        Ok(Outcome::DaemonGone) => {
                            info!(
                                "tail reconnect: exiting -- ECONNREFUSED on stale socket, no supervisor"
                            );
                            bail_tail!(
                                "daemon appears to have crashed -- session is gone; run `gritty server` or `gritty restart`"
                            );
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

    #[test]
    fn reconnect_status_line_retrying() {
        let line = reconnect_status_line(0, 4, ReconnectPhase::Retrying);
        // In-place repaint: CR to column 0, dim, clear-to-eol, SGR reset.
        assert!(line.starts_with("\r\x1b[2m"), "{line:?}");
        assert!(line.ends_with("\x1b[0m\x1b[K"), "{line:?}");
        assert!(line.contains("reconnecting 4s"), "{line:?}");
        assert!(line.contains("^C aborts"), "{line:?}");
        // Spinner glyph is the frame at index 0.
        assert!(line.contains(SPINNER[0]), "{line:?}");
    }

    #[test]
    fn reconnect_status_line_waiting() {
        let line = reconnect_status_line(3, 99, ReconnectPhase::WaitingForNetwork);
        assert!(line.contains("waiting for network"), "{line:?}");
        // No elapsed counter while parked offline.
        assert!(!line.contains("99s"), "{line:?}");
        assert!(line.contains(SPINNER[3]), "{line:?}");
    }

    #[test]
    fn reconnect_status_line_spinner_wraps() {
        let a = reconnect_status_line(1, 0, ReconnectPhase::Retrying);
        let b = reconnect_status_line(1 + SPINNER.len(), 0, ReconnectPhase::Retrying);
        assert_eq!(a, b);
        let c = reconnect_status_line(2, 0, ReconnectPhase::Retrying);
        assert_ne!(a, c);
    }

    #[test]
    fn reconnect_err_line_repaints_in_place() {
        let err = reconnect_err_line("boom");
        assert!(err.starts_with("\r\x1b[31m"), "{err:?}");
        assert!(err.contains("boom"), "{err:?}");
        assert!(err.ends_with("\x1b[K\r\n"), "{err:?}");
    }

    #[test]
    fn handshake_rejected_is_always_terminal() {
        assert!(is_terminal_handshake_err("handshake rejected: nope", false));
        assert!(is_terminal_handshake_err("handshake rejected: nope", true));
    }

    #[test]
    fn handshake_eof_terminal_only_without_supervisor() {
        // Local daemon (no supervisor): accept-then-EOF means daemon is gone.
        assert!(is_terminal_handshake_err("daemon closed connection", false));
        assert!(is_terminal_handshake_err("daemon protocol error: x", false));
        // Live tunnel supervisor: ssh dying mid-handshake is transient.
        assert!(!is_terminal_handshake_err("daemon closed connection", true));
        assert!(!is_terminal_handshake_err("daemon protocol error: x", true));
    }

    #[test]
    fn handshake_other_errs_never_terminal() {
        assert!(!is_terminal_handshake_err("handshake timed out after 10s", false));
        assert!(!is_terminal_handshake_err("handshake timed out after 10s", true));
    }

    #[test]
    fn wall_elapsed_forward_returns_diff() {
        let t0 = std::time::SystemTime::UNIX_EPOCH + Duration::from_secs(1_700_000_000);
        let t1 = t0 + Duration::from_secs(30);
        assert_eq!(wall_elapsed(t0, t1), Duration::from_secs(30));
    }

    #[test]
    fn wall_elapsed_equal_is_zero() {
        let t0 = std::time::SystemTime::UNIX_EPOCH + Duration::from_secs(1_700_000_000);
        assert_eq!(wall_elapsed(t0, t0), Duration::ZERO);
    }

    // Clock going backward (NTP correction, manual set) must not declare the
    // link idle -- we have no evidence of silence, just a jumpy clock.
    #[test]
    fn wall_elapsed_backward_is_zero() {
        let t0 = std::time::SystemTime::UNIX_EPOCH + Duration::from_secs(1_700_000_000);
        let earlier = t0 - Duration::from_secs(60);
        assert_eq!(wall_elapsed(t0, earlier), Duration::ZERO);
    }

    #[test]
    fn link_is_stale_fresh_activity_is_not_stale() {
        let now = std::time::SystemTime::now();
        assert!(!link_is_stale(now, Duration::from_secs(60)));
    }

    // The wake-from-suspend case: last_activity is minutes in the past because
    // SystemTime kept advancing while the process was frozen. The stdin arm
    // checks this before timed_send so a keystroke triggers immediate reconnect
    // instead of a SEND_TIMEOUT stall against a dead socket.
    #[test]
    fn link_is_stale_old_activity_is_stale() {
        let past = std::time::SystemTime::now() - Duration::from_secs(1800);
        assert!(link_is_stale(past, Duration::from_secs(60)));
    }

    // last_activity in the future (clock stepped backward after it was
    // recorded) must not be treated as stale -- we have no evidence of silence.
    #[test]
    fn link_is_stale_future_activity_is_not_stale() {
        let future = std::time::SystemTime::now() + Duration::from_secs(3600);
        assert!(!link_is_stale(future, Duration::from_secs(60)));
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
