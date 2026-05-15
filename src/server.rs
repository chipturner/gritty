use crate::alt_screen::AltScreenTracker;
use crate::protocol::{Frame, FrameCodec, IDLE_EVICT_TIMEOUT};
use crate::scrollback::ScrollbackBuffer;
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

/// Configuration for a session server task.
pub struct SessionConfig {
    pub agent_socket_path: PathBuf,
    pub svc_socket_path: PathBuf,
    pub session_id: u32,
    pub session_name: Option<String>,
    pub command: Option<String>,
    pub ring_buffer_cap: usize,
    pub oauth_tunnel_idle_timeout: u64,
    pub initial_cols: u16,
    pub initial_rows: u16,
    pub cwd: Option<String>,
    pub initial_device_id: u64,
    pub idle_evict_timeout: Duration,
}

impl Default for SessionConfig {
    fn default() -> Self {
        Self {
            agent_socket_path: PathBuf::new(),
            svc_socket_path: PathBuf::new(),
            session_id: 0,
            session_name: None,
            command: None,
            ring_buffer_cap: 1 << 20,
            oauth_tunnel_idle_timeout: 5,
            initial_cols: 0,
            initial_rows: 0,
            cwd: None,
            initial_device_id: 0,
            idle_evict_timeout: IDLE_EVICT_TIMEOUT,
        }
    }
}

/// Wrapper to distinguish active, tail, and send connections arriving via channel.
pub enum ClientConn {
    Active {
        framed: Framed<UnixStream, FrameCodec>,
        client_name: String,
        capabilities: u32,
        /// Client's current terminal size from its Attach frame (0 = unknown).
        /// The session applies this before replaying scrollback on reconnect so
        /// the shell regenerates its last line at the right winsize.
        cols: u16,
        rows: u16,
        /// How far the client has rendered into the PTY output stream. Drives
        /// offset-based reconnect replay (see `plan_replay`).
        rendered_offset: u64,
        /// The client painted a reconnect status line, so its cursor is no
        /// longer where `rendered_offset` left it and the current line needs a
        /// repaint before the stream resumes.
        line_dirty: bool,
        /// `true` for a fresh explicit `connect` (no prior stream position);
        /// `false` for an auto-reconnect. A fresh client gets scrollback
        /// context instead of an incremental resume.
        is_fresh: bool,
    },
    Tail(Framed<UnixStream, FrameCodec>),
    /// Raw stream for file transfer (local-side commands routed through daemon).
    Send(UnixStream),
    /// Daemon is shutting down (kill-server / SIGTERM). Notify the attached
    /// client with `Frame::ServerShutdown` and exit cleanly so the client
    /// stops reconnecting instead of spinning against a socket that will
    /// never answer.
    Shutdown,
}

/// Events broadcast to tail clients.
#[derive(Clone)]
enum TailEvent {
    Data(Bytes),
    Exit { code: i32 },
    Shutdown,
}

/// Upper bound on the bytes `History::line_prefix` will return. Keeps the
/// reconnect line-repaint `Notice` well under `MAX_FRAME_SIZE` even when the
/// ring is configured larger than 1 MiB and holds a newline-free tail.
const LINE_PREFIX_CAP: usize = 4096;

/// Always-on trailing byte ring of PTY output plus a monotonic `total_out`
/// counter. Together they let a reconnecting client resume the stream by
/// absolute offset: `history` covers `[base(), total()]`, so a client that
/// rendered up to byte `R` gets exactly `Data[R..total()]` replayed -- no
/// redundant repaint. When the ring overflows its cap the oldest chunks are
/// evicted and `base()` advances, which is how truncation is detected.
struct History {
    chunks: VecDeque<Bytes>,
    size: usize,
    cap: usize,
    total_out: u64,
}

impl History {
    fn new(cap: usize) -> Self {
        Self { chunks: VecDeque::new(), size: 0, cap: cap.max(1), total_out: 0 }
    }

    /// Append a chunk of PTY output, evicting oldest chunks past the cap.
    fn push(&mut self, chunk: &Bytes) {
        if chunk.is_empty() {
            return;
        }
        self.total_out += chunk.len() as u64;
        self.size += chunk.len();
        self.chunks.push_back(chunk.clone());
        while self.size > self.cap {
            match self.chunks.pop_front() {
                Some(old) => self.size -= old.len(),
                None => break,
            }
        }
    }

    /// Total PTY bytes ever produced by the session.
    fn total(&self) -> u64 {
        self.total_out
    }

    /// Offset of the oldest byte still retained. A client whose rendered
    /// offset is below this has lost content (truncation).
    fn base(&self) -> u64 {
        self.total_out - self.size as u64
    }

    /// Chunks covering `[from, total())`. `from` is clamped to `base()`.
    fn slice_from(&self, from: u64) -> Vec<Bytes> {
        let mut skip = from.saturating_sub(self.base()) as usize;
        let mut out = Vec::new();
        for chunk in &self.chunks {
            if skip >= chunk.len() {
                skip -= chunk.len();
                continue;
            }
            out.push(if skip > 0 { chunk.slice(skip..) } else { chunk.clone() });
            skip = 0;
        }
        out
    }

    /// Bytes of the current (in-progress) line up to `upto`: everything after
    /// the last `\n` at or before `upto`, clamped to what's retained. Used to
    /// repaint the cursor's line when a client's reconnect widget disturbed it.
    ///
    /// The result is capped at `LINE_PREFIX_CAP` bytes. A newline-free tail
    /// (base64, `jq -c`, a `\r`-based progress bar) in a ring configured
    /// larger than 1 MiB would otherwise produce a `Notice` frame above
    /// `MAX_FRAME_SIZE`, which the client's decoder rejects -- and since the
    /// rejection leaves `rendered_offset` unchanged, every reconnect would
    /// recompute the same oversized prefix: a permanent reconnect loop. Only
    /// the bytes nearest the cursor matter for a repaint anyway.
    fn line_prefix(&self, upto: u64) -> Bytes {
        let upto_idx = upto.saturating_sub(self.base()).min(self.size as u64) as usize;
        let mut flat: Vec<u8> = Vec::with_capacity(upto_idx);
        for chunk in &self.chunks {
            if flat.len() >= upto_idx {
                break;
            }
            let take = (upto_idx - flat.len()).min(chunk.len());
            flat.extend_from_slice(&chunk[..take]);
        }
        let nl_start = flat.iter().rposition(|&b| b == b'\n').map_or(0, |nl| nl + 1);
        let start = nl_start.max(flat.len().saturating_sub(LINE_PREFIX_CAP));
        Bytes::copy_from_slice(&flat[start..])
    }

    fn chunks(&self) -> &VecDeque<Bytes> {
        &self.chunks
    }
}

/// What a reconnecting / attaching client should receive, decided purely from
/// offsets + screen mode so it can be unit-tested. The executor
/// (`send_reconnect_replay`) turns a plan into wire frames.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ReplayPlan {
    /// Alt-screen: a byte suffix can't reconstruct a TUI screen, so force a
    /// full repaint. The client's offset jumps to `offset`.
    AltRedraw { offset: u64 },
    /// Fresh viewer (explicit connect / takeover): show scrollback context
    /// under a divider. The client's offset jumps to `offset` (current total).
    Fresh { offset: u64 },
    /// Clean incremental resume: replay exactly `Data[offset..total]`.
    Clean { offset: u64 },
    /// Truncated resume: the client's position fell out of the retained
    /// history. Replay from `offset` (= history base); `dropped` bytes lost.
    Truncated { offset: u64, dropped: u64 },
}

/// Decide how to bring a client back in sync. Pure: no I/O, fully testable.
fn plan_replay(
    rendered_offset: u64,
    is_fresh: bool,
    in_alt_screen: bool,
    history_base: u64,
    history_total: u64,
) -> ReplayPlan {
    if in_alt_screen {
        return ReplayPlan::AltRedraw { offset: history_total };
    }
    if is_fresh || rendered_offset > history_total {
        // Fresh connect, or a nonsense offset ahead of the stream -- treat as
        // a fresh viewer rather than trusting the offset.
        return ReplayPlan::Fresh { offset: history_total };
    }
    if rendered_offset < history_base {
        return ReplayPlan::Truncated {
            offset: history_base,
            dropped: history_base - rendered_offset,
        };
    }
    ReplayPlan::Clean { offset: rendered_offset }
}

pub struct SessionMetadata {
    pub pty_path: String,
    /// Shell PID, or 0 until the shell is actually spawned. Atomic so the
    /// metadata slot can be populated before shell spawn (required for
    /// owner_device_id storage on Attach-during-spawn) and updated once the
    /// shell's pid is known.
    pub shell_pid: AtomicU32,
    pub created_at: u64,
    pub attached: AtomicBool,
    pub last_heartbeat: AtomicU64,
    pub client_name: std::sync::Mutex<String>,
    pub wants_agent: AtomicBool,
    pub wants_open: AtomicBool,
    /// Persistent device identifier of the session's current owner. Set from
    /// the Hello frame's `device_id` on attach or session creation. A non-zero
    /// `attach_token` in a subsequent Attach triggers an ownership check: if
    /// the Hello's `device_id` differs from this value, the attach is rejected
    /// with `OwnerChanged`.
    pub owner_device_id: AtomicU64,
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

/// Aborts the wrapped tokio task on drop. Tokio's `JoinHandle` drop *detaches*,
/// which leaks acceptor tasks when `server::run` is aborted by kill-session.
struct AbortOnDrop(tokio::task::JoinHandle<()>);

impl Drop for AbortOnDrop {
    fn drop(&mut self) {
        self.0.abort();
    }
}

/// Removes a socket file on drop so per-session sockets are cleaned up on
/// abort / early `?`-return, not just on normal loop exit.
struct SocketCleanup(PathBuf);

impl Drop for SocketCleanup {
    fn drop(&mut self) {
        cleanup_socket(&self.0);
    }
}

/// Why the relay loop exited.
enum RelayExit {
    /// Client disconnected — re-accept.
    ClientGone,
    /// Shell exited with a code — we're done.
    ShellExited(i32),
    /// Daemon is shutting down — notify client, tear down, exit.
    Shutdown,
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

/// Events from clipboard svc connections to the main relay loop.
enum ClipboardEvent {
    /// Copy data to client clipboard. `reply` resolves `true` once the data
    /// was forwarded to an attached, clipboard-capable client, `false` if it
    /// was dropped (detached session / no `CAP_CLIPBOARD`).
    Copy { data: Bytes, reply: tokio::sync::oneshot::Sender<bool> },
    /// Paste request: send ClipboardGet to client, return data via oneshot.
    Paste { reply: tokio::sync::oneshot::Sender<Option<Bytes>> },
}

/// Events from tunnel TCP connection tasks to the main relay loop.
enum TunnelEvent {
    Data { channel_id: u32, data: Bytes },
    Closed { channel_id: u32 },
}

/// Events from port forward TCP acceptors and connections to the main relay loop.
enum PortForwardEvent {
    /// TCP connection accepted on a listening port.
    Accepted { forward_id: u32, channel_id: u32, writer_tx: mpsc::Sender<Bytes> },
    /// Data from a TCP connection.
    Data { channel_id: u32, data: Bytes },
    /// TCP connection closed.
    Closed { channel_id: u32 },
    /// Svc socket dropped -- teardown forward.
    Stopped { forward_id: u32 },
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
    enabled: Arc<AtomicBool>,
    channels: HashMap<u32, mpsc::Sender<Bytes>>,
    acceptor: Option<tokio::task::JoinHandle<()>>,
    next_channel_id: Arc<AtomicU32>,
    socket_path: PathBuf,
}

impl AgentForwardState {
    fn new(socket_path: PathBuf) -> Self {
        Self {
            enabled: Arc::new(AtomicBool::new(false)),
            channels: HashMap::new(),
            acceptor: None,
            next_channel_id: Arc::new(AtomicU32::new(0)),
            socket_path,
        }
    }

    /// Client detach/takeover: stop forwarding but keep the listener and socket
    /// file alive so `SSH_AUTH_SOCK` remains a valid path for a future `-A`
    /// reattach.
    fn disable(&mut self) {
        self.enabled.store(false, Ordering::Relaxed);
        self.channels.clear();
    }

    /// Session end: disable, abort the acceptor, and remove the socket file.
    fn teardown(&mut self) {
        self.disable();
        if let Some(handle) = self.acceptor.take() {
            handle.abort();
        }
        cleanup_socket(&self.socket_path);
    }
}

impl Drop for AgentForwardState {
    fn drop(&mut self) {
        self.teardown();
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
    next_channel_id: Arc<AtomicU32>,
}

impl PortForwardTable {
    fn new() -> Self {
        Self {
            forwards: HashMap::new(),
            channels: HashMap::new(),
            pending_remote: HashMap::new(),
            // Server-allocated PF channel_ids are odd; client-allocated are even.
            // Both sides insert into a single `channels` map keyed by channel_id,
            // so partitioning the space prevents lf/rf collisions.
            next_channel_id: Arc::new(AtomicU32::new(1)),
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

impl Drop for PortForwardTable {
    fn drop(&mut self) {
        self.teardown();
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

/// Sanitize a file path from the sender manifest: reject absolute paths, `..`
/// traversal, `.`, and embedded NUL/backslash. Nested relative paths (from
/// `send -r`) are preserved so the receiver can recreate directory structure.
fn sanitize_filename(name: &str) -> Option<String> {
    if name.is_empty() || name.contains('\0') || name.contains('\\') {
        return None;
    }
    let mut has_normal = false;
    for component in std::path::Path::new(name).components() {
        match component {
            std::path::Component::Normal(_) => has_normal = true,
            _ => return None,
        }
    }
    has_normal.then(|| name.to_string())
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
    enabled: Arc<AtomicBool>,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        loop {
            let (stream, _) = match listener.accept().await {
                Ok(conn) => conn,
                Err(e) => {
                    warn!("agent listener accept error: {e}; retrying");
                    tokio::time::sleep(Duration::from_millis(100)).await;
                    continue;
                }
            };

            if let Err(e) = crate::security::verify_peer_uid(&stream) {
                warn!("agent socket: {e}");
                continue;
            }

            if !enabled.load(Ordering::Relaxed) {
                debug!("agent connection refused: no -A client attached");
                continue;
            }

            let channel_id = next_channel_id.fetch_add(1, Ordering::Relaxed);

            let (read_half, write_half) = stream.into_split();
            let (writer_tx, writer_rx) = crate::relay_writer_channel();
            // Accepted MUST be enqueued before the reader task can enqueue
            // Data, or the relay loop drops Data for an unknown channel.
            if event_tx.send(AgentEvent::Accepted { channel_id, writer_tx }).is_err() {
                break; // relay loop is gone
            }
            let data_tx = event_tx.clone();
            let close_tx = event_tx.clone();
            crate::spawn_channel_relay(
                channel_id,
                read_half,
                write_half,
                writer_rx,
                move |id, data| data_tx.send(AgentEvent::Data { channel_id: id, data }).is_ok(),
                move |id| {
                    let _ = close_tx.send(AgentEvent::Closed { channel_id: id });
                },
            );
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
                    warn!(forward_id, "pf tcp listener accept error: {e}; retrying");
                    tokio::time::sleep(Duration::from_millis(100)).await;
                    continue;
                }
            };

            let _ = stream.set_nodelay(true);
            let channel_id = next_channel_id.fetch_add(2, Ordering::Relaxed);
            let (read_half, write_half) = stream.into_split();
            let (writer_tx, writer_rx) = crate::relay_writer_channel();
            // Accepted MUST be enqueued before the reader task can enqueue
            // Data, or the relay loop drops Data for an unknown channel.
            if event_tx
                .send(PortForwardEvent::Accepted { forward_id, channel_id, writer_tx })
                .is_err()
            {
                break;
            }
            let data_tx = event_tx.clone();
            let close_tx = event_tx.clone();
            crate::spawn_channel_relay(
                channel_id,
                read_half,
                write_half,
                writer_rx,
                move |id, data| {
                    data_tx.send(PortForwardEvent::Data { channel_id: id, data }).is_ok()
                },
                move |id| {
                    let _ = close_tx.send(PortForwardEvent::Closed { channel_id: id });
                },
            );
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
    clipboard_event_tx: mpsc::UnboundedSender<ClipboardEvent>,
    negotiated_caps: Arc<std::sync::atomic::AtomicU32>,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        loop {
            let (mut stream, _) = match listener.accept().await {
                Ok(conn) => conn,
                Err(e) => {
                    warn!("svc listener accept error: {e}; retrying");
                    tokio::time::sleep(Duration::from_millis(100)).await;
                    continue;
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
            let ctx = clipboard_event_tx.clone();
            let caps = Arc::clone(&negotiated_caps);
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
                    Some(crate::protocol::SvcRequest::Clipboard) => {
                        use tokio::io::AsyncWriteExt;
                        let has_cap = caps.load(std::sync::atomic::Ordering::Relaxed)
                            & crate::protocol::CAP_CLIPBOARD
                            != 0;
                        let mut op = [0u8; 1];
                        if stream.read_exact(&mut op).await.is_err() {
                            return;
                        }
                        match op[0] {
                            0x01 => {
                                // Copy: read remaining data, capped so it fits in a single frame.
                                const CLIPBOARD_MAX: usize = 512 * 1024;
                                let mut data = Vec::new();
                                let mut limited = stream.take((CLIPBOARD_MAX + 1) as u64);
                                let _ = limited.read_to_end(&mut data).await;
                                let mut stream = limited.into_inner();
                                if data.len() > CLIPBOARD_MAX {
                                    warn!(
                                        size = data.len(),
                                        "clipboard copy truncated to {CLIPBOARD_MAX} bytes"
                                    );
                                    data.truncate(CLIPBOARD_MAX);
                                }
                                // Reply 1 byte: 0x01 = delivered to an attached
                                // clipboard-capable client, 0x00 = dropped. An
                                // older client never reads it (harmless write to
                                // a closed socket); a current client uses it so
                                // `gritty copy` no longer exits 0 on a silent
                                // drop.
                                let delivered = if !has_cap || data.is_empty() {
                                    if !has_cap {
                                        debug!("svc socket: clipboard copy but no CAP_CLIPBOARD");
                                    }
                                    false
                                } else {
                                    let (tx, rx) = tokio::sync::oneshot::channel();
                                    ctx.send(ClipboardEvent::Copy {
                                        data: Bytes::from(data),
                                        reply: tx,
                                    })
                                    .is_ok()
                                        && rx.await.unwrap_or(false)
                                };
                                let _ = stream.write_all(&[u8::from(delivered)]).await;
                            }
                            0x02 => {
                                // Paste: request clipboard from client
                                if !has_cap {
                                    debug!("svc socket: clipboard paste but no CAP_CLIPBOARD");
                                    return;
                                }
                                let (tx, rx) = tokio::sync::oneshot::channel();
                                let _ = ctx.send(ClipboardEvent::Paste { reply: tx });
                                if let Ok(Some(data)) = rx.await {
                                    let _ = stream.write_all(&data).await;
                                }
                            }
                            _ => debug!("svc socket: unknown clipboard op: 0x{:02x}", op[0]),
                        }
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
                    // Timed send: a stalled tail reader must not park this task
                    // forever (it holds a Framed<UnixStream> and a broadcast rx).
                    if send_framed_timed(&mut framed, Frame::Data(chunk)).await.is_err() { break; }
                }
                Ok(TailEvent::Exit { code }) => {
                    let _ = send_framed_timed(&mut framed, Frame::Exit { code }).await;
                    break;
                }
                Ok(TailEvent::Shutdown) => {
                    let _ = send_framed_timed(&mut framed, Frame::ServerShutdown).await;
                    break;
                }
                Err(broadcast::error::RecvError::Lagged(n)) => {
                    let marker = format!("\r\n\x1b[2m\u{25b8} tail fell behind \u{b7} {n} events dropped\x1b[0m\r\n");
                    if send_framed_timed(&mut framed, Frame::Data(Bytes::from(marker))).await.is_err() { break; }
                    continue;
                }
                Err(broadcast::error::RecvError::Closed) => break,
            },
            frame = framed.next() => match frame {
                Some(Ok(Frame::Ping)) => { let _ = send_framed_timed(&mut framed, Frame::Pong).await; }
                _ => break,
            },
        }
    }
}

/// Drain ring buffer contents to a tail client, then subscribe to broadcast and spawn relay.
fn spawn_tail(
    mut framed: Framed<UnixStream, FrameCodec>,
    history: &History,
    tail_tx: &broadcast::Sender<TailEvent>,
) {
    let rx = tail_tx.subscribe();
    let chunks: Vec<Bytes> = history.chunks().iter().cloned().collect();
    tokio::spawn(async move {
        for chunk in chunks {
            // Timed send: a stalled reader must not wedge the initial history
            // replay and leak this task + its Framed<UnixStream>.
            if send_framed_timed(&mut framed, Frame::Data(chunk)).await.is_err() {
                return;
            }
        }
        tail_relay(framed, rx).await;
    });
}

/// Groups references needed by inner relay handler methods.
/// `framed` is kept outside (passed to handlers) so `tokio::select!` can
/// poll `framed.next()` independently without conflicting borrows.
/// Cap on buffered client→PTY input while the shell isn't reading stdin.
const PENDING_INPUT_CAP: usize = 1 << 20;

/// Upper bound on a single `framed.send` to the attached client. A half-open
/// UDS (client laptop closed / network wedged) can otherwise park the send
/// indefinitely; while the relay task is parked inside an `.await`, the
/// `client_rx.recv()` branch never re-polls, so a concurrent force-takeover
/// (`connect -F`) silently stalls. Breaking to `ClientGone` on timeout lets
/// takeover proceed and keeps the session unwedged.
const CLIENT_SEND_TIMEOUT: Duration = Duration::from_secs(30);

/// Non-blocking drain of any remaining PTY output after `child.wait()` fires,
/// capturing final write(s) that raced with the exit. The master fd is
/// O_NONBLOCK, so a raw read returns EAGAIN when the buffer is empty.
fn drain_pty_final(master: &AsyncFd<OwnedFd>, buf: &mut [u8]) -> Vec<Bytes> {
    let mut chunks = Vec::new();
    loop {
        match nix::unistd::read(master.get_ref(), buf) {
            Ok(0) => break,
            Ok(n) => chunks.push(Bytes::copy_from_slice(&buf[..n])),
            Err(_) => break,
        }
    }
    chunks
}

/// Drain queued client→PTY input. Called from select! writable arms (attached
/// and detached loops) so a full PTY input buffer never blocks the relay.
fn drain_pending_input(
    mut guard: tokio::io::unix::AsyncFdReadyGuard<'_, OwnedFd>,
    pending: &mut VecDeque<Bytes>,
    pending_bytes: &mut usize,
) -> io::Result<()> {
    loop {
        let Some(front) = pending.front().cloned() else { break };
        match guard.try_io(|inner| nix::unistd::write(inner, &front).map_err(io::Error::from)) {
            Ok(Ok(n)) if n == front.len() => {
                *pending_bytes -= n;
                pending.pop_front();
            }
            Ok(Ok(n)) => {
                *pending_bytes -= n;
                *pending.front_mut().unwrap() = front.slice(n..);
            }
            Ok(Err(e)) => {
                // EIO on a slave-side PTY write means the shell has torn down
                // the slave (exit, SIGHUP). The readable()/child.wait() arms
                // already treat the same EIO on read as a clean shell exit
                // (RelayExit::ShellExited). With `biased` select ordering,
                // writable() polls first, so a stray byte in `pending` at
                // shell-exit time would otherwise promote this to a fatal
                // error and the client sees "session gone" instead of the
                // shell's real exit code. Drop the pending bytes and let the
                // read/wait arm observe exit on the next poll.
                if e.raw_os_error() == Some(libc::EIO) {
                    pending.clear();
                    *pending_bytes = 0;
                    return Ok(());
                }
                return Err(e);
            }
            Err(_would_block) => break,
        }
    }
    Ok(())
}

struct ServerRelay<'a> {
    async_master: &'a AsyncFd<OwnedFd>,
    pending_input: &'a mut VecDeque<Bytes>,
    pending_input_bytes: &'a mut usize,
    agent: &'a mut AgentForwardState,
    tunnel: &'a mut TunnelRelayState,
    pf: &'a mut PortForwardTable,
    transfer_state: &'a mut TransferState,
    open_forward_enabled: &'a mut bool,
    tail_tx: &'a broadcast::Sender<TailEvent>,
    metadata_slot: &'a Arc<OnceLock<SessionMetadata>>,
    tunnel_event_tx: &'a mpsc::UnboundedSender<TunnelEvent>,
    pf_event_tx: &'a mpsc::UnboundedSender<PortForwardEvent>,
    send_notify_tx: &'a mpsc::UnboundedSender<Frame>,
    paste_deadline: Option<tokio::time::Instant>,
    pending_paste: &'a mut Option<tokio::sync::oneshot::Sender<Option<Bytes>>>,
    negotiated_caps: &'a Arc<std::sync::atomic::AtomicU32>,
}

impl ServerRelay<'_> {
    async fn handle_client_frame(
        &mut self,
        framed: &mut Framed<UnixStream, FrameCodec>,
        frame: Option<Result<Frame, io::Error>>,
    ) -> Result<ControlFlow<RelayExit>, anyhow::Error> {
        match frame {
            Some(Ok(Frame::Data(data))) => {
                debug!(len = data.len(), "socket -> pty (queued)");
                if *self.pending_input_bytes + data.len() > PENDING_INPUT_CAP {
                    warn!(
                        queued = *self.pending_input_bytes,
                        incoming = data.len(),
                        "pending PTY input cap exceeded, dropping frame"
                    );
                } else {
                    *self.pending_input_bytes += data.len();
                    self.pending_input.push_back(data);
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
                let _ = send_framed_timed(framed, Frame::Pong).await;
            }
            Some(Ok(Frame::AgentForward)) => {
                debug!("agent forwarding enabled by client");
                self.agent.enabled.store(true, Ordering::Relaxed);
                if let Some(meta) = self.metadata_slot.get() {
                    meta.wants_agent.store(true, Ordering::Relaxed);
                }
            }
            Some(Ok(Frame::AgentData { channel_id, data })) => {
                if self.agent.channels.get(&channel_id).is_some_and(|tx| tx.try_send(data).is_err())
                {
                    warn!(channel_id, "agent channel backpressured, closing");
                    self.agent.channels.remove(&channel_id);
                    // Tell the peer so its half (socket + relay task) tears
                    // down too -- the AgentEvent::Closed recovery is gated on
                    // a successful remove that has now already happened.
                    let _ = send_framed_timed(framed, Frame::AgentClose { channel_id }).await;
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
                    // Inline connect + register so the follow-up TunnelData
                    // (next frame, and `biased` polls `framed.next()` first)
                    // finds a writer instead of being dropped.
                    match tokio::net::TcpStream::connect(("127.0.0.1", port)).await {
                        Ok(stream) => {
                            let _ = stream.set_nodelay(true);
                            self.register_tunnel_channel(channel_id, stream);
                        }
                        Err(_) => {
                            let _ =
                                send_framed_timed(framed, Frame::TunnelClose { channel_id }).await;
                        }
                    }
                }
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
                    // Mirror the TunnelClose handler: notify the peer and arm
                    // the idle teardown once the last channel is gone.
                    let _ = send_framed_timed(framed, Frame::TunnelClose { channel_id }).await;
                    if self.tunnel.channels.is_empty() && self.tunnel.port.is_some() {
                        self.tunnel.idle_deadline =
                            Some(tokio::time::Instant::now() + self.tunnel.idle_timeout);
                    }
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
                // Connect inline (loopback is instant -- success or
                // ECONNREFUSED) so the channel is registered before the next
                // frame. Spawning this lets a biased `framed.next()` read the
                // follow-up PortForwardData first and drop it on the floor,
                // which is why rf web page loads stalled: the browser's GET
                // arrived before the connect task ran.
                if self.pf.forwards.contains_key(&forward_id) {
                    match tokio::net::TcpStream::connect(("127.0.0.1", target_port)).await {
                        Ok(stream) => {
                            let _ = stream.set_nodelay(true);
                            let (read_half, write_half) = stream.into_split();
                            let (writer_tx, writer_rx) = crate::relay_writer_channel();
                            self.pf.channels.insert(channel_id, (forward_id, writer_tx.clone()));
                            if let Some(fwd) = self.pf.forwards.get_mut(&forward_id) {
                                fwd.channels.insert(channel_id, writer_tx);
                            }
                            let data_tx = self.pf_event_tx.clone();
                            let close_tx = self.pf_event_tx.clone();
                            crate::spawn_channel_relay(
                                channel_id,
                                read_half,
                                write_half,
                                writer_rx,
                                move |id, data| {
                                    data_tx
                                        .send(PortForwardEvent::Data { channel_id: id, data })
                                        .is_ok()
                                },
                                move |id| {
                                    let _ =
                                        close_tx.send(PortForwardEvent::Closed { channel_id: id });
                                },
                            );
                        }
                        Err(e) => {
                            debug!(channel_id, target_port, "rf connect failed: {e}");
                            let _ =
                                send_framed_timed(framed, Frame::PortForwardClose { channel_id })
                                    .await;
                        }
                    }
                }
            }
            Some(Ok(Frame::PortForwardData { channel_id, data })) => {
                if self
                    .pf
                    .channels
                    .get(&channel_id)
                    .is_some_and(|(_, tx)| tx.try_send(data).is_err())
                {
                    warn!(channel_id, "pf channel backpressured, closing");
                    // Mirror the PortForwardClose handler: drop the per-forward
                    // writer_tx clone too (otherwise the relay task never FINs)
                    // and notify the peer.
                    if let Some((forward_id, _)) = self.pf.channels.remove(&channel_id) {
                        if let Some(fwd) = self.pf.forwards.get_mut(&forward_id) {
                            fwd.channels.remove(&channel_id);
                        }
                        let _ =
                            send_framed_timed(framed, Frame::PortForwardClose { channel_id }).await;
                    }
                }
            }
            Some(Ok(Frame::PortForwardClose { channel_id })) => {
                if let Some((forward_id, _)) = self.pf.channels.remove(&channel_id)
                    && let Some(fwd) = self.pf.forwards.get_mut(&forward_id)
                {
                    fwd.channels.remove(&channel_id);
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
            Some(Ok(Frame::ClipboardData { data })) => {
                self.paste_deadline = None;
                if let Some(reply) = self.pending_paste.take() {
                    let _ = reply.send(Some(data));
                }
            }
            Some(Ok(Frame::PortForwardRequest {
                forward_id,
                direction,
                listen_port,
                target_port,
            })) => {
                info!(
                    forward_id,
                    direction, listen_port, target_port, "port forward request from client"
                );
                if direction == 0 {
                    // Local-forward: server binds TCP, forwards to client
                    match tokio::net::TcpListener::bind(("127.0.0.1", listen_port)).await {
                        Ok(listener) => {
                            debug!(forward_id, listen_port, target_port, "local-forward: bound");
                            let handle = spawn_pf_tcp_acceptor(
                                listener,
                                forward_id,
                                self.pf.next_channel_id.clone(),
                                self.pf_event_tx.clone(),
                            );
                            self.pf.forwards.insert(
                                forward_id,
                                PortForwardState {
                                    listener_handle: Some(handle),
                                    channels: HashMap::new(),
                                    stop_handle: None,
                                    target_port,
                                },
                            );
                            let _ =
                                send_framed_timed(framed, Frame::PortForwardReady { forward_id })
                                    .await;
                        }
                        Err(e) => {
                            warn!(forward_id, listen_port, "local-forward: bind failed: {e}");
                            let _ =
                                send_framed_timed(framed, Frame::PortForwardStop { forward_id })
                                    .await;
                        }
                    }
                } else {
                    // Remote-forward: register forward_id so PortForwardOpen from client is accepted
                    self.pf.forwards.insert(
                        forward_id,
                        PortForwardState {
                            listener_handle: None,
                            channels: HashMap::new(),
                            stop_handle: None,
                            target_port,
                        },
                    );
                    let _ = send_framed_timed(framed, Frame::PortForwardReady { forward_id }).await;
                }
            }
            Some(Ok(Frame::Env { vars: _ })) => {
                // Env vars are only used at first-client shell spawn; ignored on reconnect.
            }
            Some(Ok(Frame::Exit { .. })) | None => {
                return Ok(ControlFlow::Break(RelayExit::ClientGone));
            }
            Some(Ok(_)) => {}
            Some(Err(e)) => {
                warn!(error = %e, "client stream error, treating as disconnect");
                return Ok(ControlFlow::Break(RelayExit::ClientGone));
            }
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
                if self.agent.enabled.load(Ordering::Relaxed) {
                    info!(channel_id, "relaying agent open");
                    self.agent.channels.insert(channel_id, writer_tx);
                    let _ = send_framed_timed(framed, Frame::AgentOpen { channel_id }).await;
                }
            }
            Some(AgentEvent::Data { channel_id, data }) => {
                if self.agent.enabled.load(Ordering::Relaxed)
                    && self.agent.channels.contains_key(&channel_id)
                {
                    let _ = send_framed_timed(framed, Frame::AgentData { channel_id, data }).await;
                }
            }
            Some(AgentEvent::Closed { channel_id }) => {
                if self.agent.channels.remove(&channel_id).is_some() {
                    let _ = send_framed_timed(framed, Frame::AgentClose { channel_id }).await;
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
                    if let Some(port) = extract_redirect_port(&url)
                        && port_in_use(port)
                    {
                        debug!(port, "setting up reverse tunnel for OAuth callback");
                        self.tunnel.port = Some(port);
                        // Clear any idle deadline left armed by a prior flow's
                        // TunnelClose. Otherwise it fires mid-setup, teardown()
                        // nulls the port, and this flow's TunnelOpen is dropped
                        // at the `if let Some(port)` check. Mirrors TunnelOpen.
                        self.tunnel.idle_deadline = None;
                        let _ = send_framed_timed(framed, Frame::TunnelListen { port }).await;
                    }
                    info!(url, "forwarding URL to client");
                    let _ = send_framed_timed(framed, Frame::OpenUrl { url }).await;
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

    fn register_tunnel_channel(&mut self, channel_id: u32, stream: tokio::net::TcpStream) {
        debug!(channel_id, "tunnel channel connected");
        let (read_half, write_half) = stream.into_split();
        let (writer_tx, writer_rx) = crate::relay_writer_channel();
        self.tunnel.channels.insert(channel_id, writer_tx);
        let tx = self.tunnel_event_tx.clone();
        let close_tx = self.tunnel_event_tx.clone();
        crate::spawn_channel_relay(
            channel_id,
            read_half,
            write_half,
            writer_rx,
            move |id, data| tx.send(TunnelEvent::Data { channel_id: id, data }).is_ok(),
            move |id| {
                let _ = close_tx.send(TunnelEvent::Closed { channel_id: id });
            },
        );
    }

    #[allow(clippy::collapsible_match)]
    async fn handle_tunnel_event(
        &mut self,
        framed: &mut Framed<UnixStream, FrameCodec>,
        event: Option<TunnelEvent>,
    ) {
        match event {
            Some(TunnelEvent::Data { channel_id, data }) => {
                // Gate on channels map membership, mirroring agent/pf
                // siblings. Without this, a stale reader task from a
                // previous client forwards bytes to the new client on a
                // channel_id the new client never opened.
                if self.tunnel.channels.contains_key(&channel_id) {
                    let _ = send_framed_timed(framed, Frame::TunnelData { channel_id, data }).await;
                }
            }
            Some(TunnelEvent::Closed { channel_id }) => {
                if self.tunnel.channels.remove(&channel_id).is_some() {
                    let _ = send_framed_timed(framed, Frame::TunnelClose { channel_id }).await;
                    if self.tunnel.channels.is_empty() && self.tunnel.port.is_some() {
                        self.tunnel.idle_deadline =
                            Some(tokio::time::Instant::now() + self.tunnel.idle_timeout);
                    }
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
            // Only clear an Active transfer; a stale Done/Cancel must not
            // clobber a newer WaitingForReceiver/Sender state.
            if matches!(frame, Frame::SendDone | Frame::SendCancel { .. })
                && matches!(*self.transfer_state, TransferState::Active { .. })
            {
                *self.transfer_state = TransferState::Idle;
            }
            let _ = send_framed_timed(framed, frame).await;
        }
    }

    #[allow(clippy::collapsible_match)]
    async fn handle_pf_event(
        &mut self,
        framed: &mut Framed<UnixStream, FrameCodec>,
        event: Option<PortForwardEvent>,
    ) {
        match event {
            Some(PortForwardEvent::Accepted { forward_id, channel_id, writer_tx }) => {
                if let Some(fwd) = self.pf.forwards.get_mut(&forward_id) {
                    fwd.channels.insert(channel_id, writer_tx.clone());
                    self.pf.channels.insert(channel_id, (forward_id, writer_tx));
                    let target_port = fwd.target_port;
                    // Timed send: this runs in the main relay select loop, so a
                    // raw send against a half-open client parks the whole loop
                    // -- idle-evict, takeover, and shell-exit all stop firing.
                    let _ = send_framed_timed(
                        framed,
                        Frame::PortForwardOpen { forward_id, channel_id, target_port },
                    )
                    .await;
                }
            }
            Some(PortForwardEvent::Data { channel_id, data }) => {
                if self.pf.channels.contains_key(&channel_id) {
                    let _ = send_framed_timed(framed, Frame::PortForwardData { channel_id, data })
                        .await;
                }
            }
            Some(PortForwardEvent::Closed { channel_id }) => {
                if let Some((forward_id, _)) = self.pf.channels.remove(&channel_id) {
                    let _ = send_framed_timed(framed, Frame::PortForwardClose { channel_id }).await;
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
                    let _ = send_framed_timed(framed, Frame::PortForwardStop { forward_id }).await;
                }
            }
            None => {}
        }
    }
}

/// Apply a new PTY window size and signal the foreground process group.
/// Centralized so reconnect flows and the `Resize` frame handler share one path.
/// Dim horizontal rule sent to the client right before a main-screen scrollback
/// replay. It marks "below this line is replayed context, not fresh output" --
/// the client already printed a `▸ reconnected` / `▸ attached` line, so we
/// don't re-announce the event, we just fence the replay. The rule spans the
/// client's terminal width (falling back when the client didn't send one) and
/// carries an optional left-aligned annotation (e.g. dropped byte count).
fn replay_divider(cols: u16, annotation: Option<&str>) -> String {
    const RULE: char = '\u{2500}'; // ─
    const FALLBACK_WIDTH: usize = 40;
    let width = if cols > 0 { cols as usize } else { FALLBACK_WIDTH };
    let rule: String = match annotation {
        Some(text) => {
            let prefix = format!("{RULE}{RULE} {text} ");
            let fill = width.saturating_sub(prefix.chars().count());
            format!("{prefix}{}", String::from_iter(std::iter::repeat_n(RULE, fill)))
        }
        None => String::from_iter(std::iter::repeat_n(RULE, width)),
    };
    format!("\r\x1b[2m{rule}\x1b[0m\r\n")
}

fn apply_winsize(master: &tokio::io::unix::AsyncFd<std::os::fd::OwnedFd>, cols: u16, rows: u16) {
    let (cols, rows) = crate::security::clamp_winsize(cols, rows);
    let ws = libc::winsize { ws_row: rows, ws_col: cols, ws_xpixel: 0, ws_ypixel: 0 };
    // SAFETY: master is a valid PTY master fd; TIOCSWINSZ takes a winsize*.
    unsafe {
        libc::ioctl(master.as_raw_fd(), libc::TIOCSWINSZ, &ws as *const _);
    }
    if let Ok(pgid) = nix::unistd::tcgetpgrp(master) {
        let _ = nix::sys::signal::killpg(pgid, nix::sys::signal::Signal::SIGWINCH);
    }
}

/// Force a TUI to fully repaint by toggling the reported window size and
/// signaling SIGWINCH twice. Many ncurses-based apps (htop, btop, nvim in
/// some modes) no-op a SIGWINCH when the reported size matches their cached
/// size, so a straight `apply_winsize` with an unchanged size does nothing.
/// Nudging rows down by one then restoring guarantees a change-of-size event
/// and a full repaint.
///
/// Between the two ioctls we actively drain the PTY master and forward what
/// we read to the client. A bare sleep here is insufficient: the TUI's
/// rows-1 repaint is typically larger than the PTY's kernel buffer, so the
/// TUI blocks in `write()` before it ever reaches the point where it would
/// re-query `TIOCGWINSZ`. With nothing reading, the second SIGWINCH finds
/// the TUI mid-write; it observes only the final (unchanged) size and
/// no-ops. Draining while we wait keeps the TUI unblocked so it actually
/// sees the intermediate size.
///
/// Returns `Err` if the client socket rejects a send, so callers can fall
/// back into the detached-drain path.
async fn send_framed_timed(
    framed: &mut Framed<UnixStream, FrameCodec>,
    frame: Frame,
) -> io::Result<()> {
    match tokio::time::timeout(CLIENT_SEND_TIMEOUT, framed.send(frame)).await {
        Ok(r) => r,
        Err(_) => Err(io::Error::new(io::ErrorKind::TimedOut, "client send timed out")),
    }
}

#[allow(clippy::too_many_arguments)]
async fn force_tui_redraw(
    master: &AsyncFd<OwnedFd>,
    framed: &mut Framed<UnixStream, FrameCodec>,
    tail_tx: &broadcast::Sender<TailEvent>,
    alt_screen: &mut AltScreenTracker,
    scrollback: &mut ScrollbackBuffer,
    history: &mut History,
    cols: u16,
    rows: u16,
) -> anyhow::Result<()> {
    // Toggle rows by default (rows-1 then rows). For rows <= 1 the toggle
    // clamps to 1 == original, so TUIs see no change-of-size event and
    // skip the repaint. Fall back to toggling cols in that degenerate
    // case so there's always a real intermediate size.
    let toggle_cols = rows <= 1 && cols >= 2;
    let nudge_cols = if toggle_cols { cols.saturating_sub(1).max(1) } else { cols };
    let nudge_rows = if toggle_cols { rows } else { rows.saturating_sub(1).max(1) };
    apply_winsize(master, nudge_cols, nudge_rows);

    // Must restore original (cols, rows) before returning, even on a send
    // failure -- otherwise the PTY stays at the intermediate nudge size.
    let result = async {
        let deadline = tokio::time::Instant::now() + Duration::from_millis(50);
        let mut buf = vec![0u8; 4096];
        loop {
            tokio::select! {
                _ = tokio::time::sleep_until(deadline) => break,
                ready = master.readable() => {
                    let Ok(mut guard) = ready else { break };
                    match guard.try_io(|inner| {
                        nix::unistd::read(inner, &mut buf).map_err(io::Error::from)
                    }) {
                        Ok(Ok(0)) | Ok(Err(_)) => break,
                        Ok(Ok(n)) => {
                            let chunk = Bytes::copy_from_slice(&buf[..n]);
                            alt_screen.scan(&chunk);
                            // Skip scrollback capture while in alt-screen: TUI
                            // apps (vim, htop) repaint full screens every
                            // frame and poison scrollback with one-shot
                            // cursor/color sequences that make no sense to
                            // replay on a main-screen reconnect.
                            if !alt_screen.in_alternate_screen() {
                                scrollback.push(&chunk);
                            }
                            // These bytes go to the client as Data, so they
                            // advance the stream offset -- they must land in
                            // history too or the client's counter drifts.
                            history.push(&chunk);
                            if tail_tx.receiver_count() > 0 {
                                let _ = tail_tx.send(TailEvent::Data(chunk.clone()));
                            }
                            send_framed_timed(framed, Frame::Data(chunk)).await?;
                        }
                        Err(_would_block) => continue,
                    }
                }
            }
        }
        Ok::<(), anyhow::Error>(())
    }
    .await;

    apply_winsize(master, cols, rows);
    result
}

/// Bring a reconnecting / attaching client back in sync with the PTY stream.
/// Sends a `Resume` frame (the client's new authoritative offset) followed by
/// whatever replay the `plan_replay` decision calls for. `line_dirty` means the
/// client painted a reconnect status line, so the chrome here also repairs the
/// cursor's line before resuming. Returns `Err` if the client socket rejects a
/// send so the caller can re-enter the detached-drain path.
#[allow(clippy::too_many_arguments)]
async fn send_reconnect_replay(
    framed: &mut Framed<UnixStream, FrameCodec>,
    async_master: &AsyncFd<OwnedFd>,
    tail_tx: &broadcast::Sender<TailEvent>,
    history: &mut History,
    scrollback: &mut ScrollbackBuffer,
    alt_screen: &mut AltScreenTracker,
    rendered_offset: u64,
    line_dirty: bool,
    is_fresh: bool,
    cols: u16,
    rows: u16,
) -> io::Result<()> {
    let plan = plan_replay(
        rendered_offset,
        is_fresh,
        alt_screen.in_alternate_screen(),
        history.base(),
        history.total(),
    );
    debug!(?plan, rendered_offset, line_dirty, is_fresh, "reconnect replay");
    match plan {
        ReplayPlan::AltRedraw { offset } => {
            send_framed_timed(framed, Frame::Resume { offset }).await?;
            // No `\r\x1b[K` here even when `line_dirty`: a client that painted
            // a status line has already erased it and stepped the cursor back
            // up onto its original row (`\r\x1b[K\x1b[A` on the reconnect
            // success path). Clearing again would wipe the user's last
            // main-screen line, and `\x1b[?1049h` below would then commit that
            // blank to the saved buffer.
            // Prime the client terminal into a clean alt screen. A byte suffix
            // can't reconstruct a TUI, so history is not replayed -- the
            // `Resume` already advanced the client's offset past those bytes,
            // and `force_tui_redraw` regenerates the screen from TUI state.
            send_framed_timed(
                framed,
                Frame::Notice(Bytes::from_static(b"\x1b[?1049h\x1b[H\x1b[2J")),
            )
            .await?;
            if cols > 0 && rows > 0 {
                force_tui_redraw(
                    async_master,
                    framed,
                    tail_tx,
                    alt_screen,
                    scrollback,
                    history,
                    cols,
                    rows,
                )
                .await
                .map_err(|e| io::Error::other(e.to_string()))?;
            }
        }
        ReplayPlan::Fresh { offset } => {
            if cols > 0 && rows > 0 {
                apply_winsize(async_master, cols, rows);
            }
            send_framed_timed(framed, Frame::Resume { offset }).await?;
            // Fence the replayed context with a dim rule. Reuse the status
            // line's row if the client painted one; the rule starts with `\r`.
            let divider = replay_divider(cols, None);
            let msg = if line_dirty { format!("\r\x1b[K{divider}") } else { divider };
            send_framed_timed(framed, Frame::Notice(Bytes::from(msg))).await?;
            for line in scrollback.lines_and_partial() {
                send_framed_timed(framed, Frame::Notice(line)).await?;
            }
            scrollback.clear();
        }
        ReplayPlan::Clean { offset } => {
            if cols > 0 && rows > 0 {
                apply_winsize(async_master, cols, rows);
            }
            send_framed_timed(framed, Frame::Resume { offset }).await?;
            if line_dirty {
                // The client erased its status line and moved the cursor back
                // up onto the line where `offset` left it (see the reconnect
                // success path in client.rs). Clear that line and repaint its
                // prefix so the incremental `Data` below continues from the
                // right column.
                send_framed_timed(framed, Frame::Notice(Bytes::from_static(b"\r\x1b[K"))).await?;
                let prefix = history.line_prefix(offset);
                if !prefix.is_empty() {
                    send_framed_timed(framed, Frame::Notice(prefix)).await?;
                }
            }
            // The heart of the seamless resume: exactly the bytes produced
            // while the client was gone, nothing it has already rendered.
            for chunk in history.slice_from(offset) {
                send_framed_timed(framed, Frame::Data(chunk)).await?;
            }
        }
        ReplayPlan::Truncated { offset, dropped } => {
            if cols > 0 && rows > 0 {
                apply_winsize(async_master, cols, rows);
            }
            send_framed_timed(framed, Frame::Resume { offset }).await?;
            let marker = format!(
                "\x1b[2m\u{25b8} {} lost while disconnected\x1b[0m\r\n",
                humansize::format_size(dropped, humansize::BINARY),
            );
            // Reuse the status line's row for the marker if there is one,
            // otherwise open a fresh line for it.
            let lead = if line_dirty { "\r\x1b[K" } else { "\r\n" };
            // Leave alt-screen first. This arm is only reachable on the main
            // screen (alt-screen takes the AltRedraw path), but the client may
            // still be stuck in alt-screen: if it disconnected inside a TUI,
            // the TUI exited during the outage, and >1 ring of output then
            // evicted the `\x1b[?1049l` from history, the client never saw the
            // exit. Re-sending it is idempotent on the main screen.
            send_framed_timed(
                framed,
                Frame::Notice(Bytes::from(format!("\x1b[?1049l{lead}{marker}"))),
            )
            .await?;
            for chunk in history.slice_from(offset) {
                send_framed_timed(framed, Frame::Data(chunk)).await?;
            }
        }
    }
    Ok(())
}

fn format_server_diag(
    history: &History,
    alt_screen: &AltScreenTracker,
    scrollback: &ScrollbackBuffer,
    relay: &ServerRelay<'_>,
    managed: &ManagedChild,
) -> String {
    use std::fmt::Write;
    let mut s = String::new();
    let _ = write!(s, "  shell pid: {}", managed.pgid);
    if let Some(meta) = relay.metadata_slot.get() {
        let cn = meta.client_name.lock().unwrap();
        if !cn.is_empty() {
            let _ = write!(s, "\n  client: {cn}");
        }
    }
    let _ = write!(
        s,
        "\n  alt screen: {}",
        if alt_screen.in_alternate_screen() { "yes" } else { "no" },
    );
    let _ = write!(s, "\n  scrollback lines: {}", scrollback.lines().len());
    let _ = write!(
        s,
        "\n  history: {} bytes in {} chunks (offset {}, cap {})",
        history.size,
        history.chunks().len(),
        history.total(),
        history.cap,
    );
    let _ = write!(s, "\n  pending pty input: {} bytes", relay.pending_input_bytes,);
    let _ = write!(s, "\n  agent channels: {}", relay.agent.channels.len(),);
    let _ = write!(s, "\n  tunnel channels: {}", relay.tunnel.channels.len(),);
    let _ = write!(
        s,
        "\n  port forwards: {} ({} connections)",
        relay.pf.forwards.len(),
        relay.pf.channels.len(),
    );
    let _ = write!(s, "\n  tail clients: {}", relay.tail_tx.receiver_count());
    s
}

pub async fn run(
    mut client_rx: mpsc::UnboundedReceiver<ClientConn>,
    metadata_slot: Arc<OnceLock<SessionMetadata>>,
    config: SessionConfig,
) -> anyhow::Result<()> {
    let SessionConfig {
        agent_socket_path,
        svc_socket_path,
        session_id,
        session_name,
        command,
        ring_buffer_cap,
        oauth_tunnel_idle_timeout,
        initial_cols,
        initial_rows,
        cwd,
        initial_device_id,
        idle_evict_timeout,
    } = config;
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
    crate::security::set_cloexec(master.as_raw_fd())?;
    crate::security::set_cloexec(slave.as_raw_fd())?;

    // Get PTY slave name before we drop the slave fd
    let pty_path =
        nix::unistd::ttyname(&slave).map(|p| p.display().to_string()).unwrap_or_default();

    // Populate the session metadata slot early. shell_pid stays 0 until the
    // shell actually spawns; client_name stays empty until the first client
    // sends its Env. This lets the daemon's Attach handler store an owner
    // device_id on sessions that are mid-spawn -- previously the set()
    // happened after shell spawn, so an Attach that landed during the
    // wait-for-first-client window saw metadata=None and couldn't persist
    // the owner. (See the `initial_device_id` param on server::run.)
    let created_at = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();
    let _ = metadata_slot.set(SessionMetadata {
        pty_path: pty_path.clone(),
        shell_pid: AtomicU32::new(0),
        created_at,
        attached: AtomicBool::new(false),
        last_heartbeat: AtomicU64::new(0),
        client_name: std::sync::Mutex::new(String::new()),
        wants_agent: AtomicBool::new(false),
        wants_open: AtomicBool::new(false),
        owner_device_id: AtomicU64::new(initial_device_id),
    });

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
    // Always-on byte history of PTY output. Unlike the old detached-only ring
    // buffer, this is populated whether or not a client is attached, so a
    // reconnecting client can resume the stream by absolute offset.
    let mut history = History::new(ring_buffer_cap);

    // Agent forwarding state. The listener is bound unconditionally so the
    // shell's SSH_AUTH_SOCK always resolves; connections are refused at accept
    // time when no `-A` client is attached.
    let mut agent = AgentForwardState::new(agent_socket_path);
    let (agent_event_tx, mut agent_event_rx) = mpsc::unbounded_channel::<AgentEvent>();
    if let Some(listener) = bind_agent_listener(&agent.socket_path) {
        agent.acceptor = Some(spawn_agent_acceptor(
            listener,
            agent_event_tx.clone(),
            agent.next_channel_id.clone(),
            agent.enabled.clone(),
        ));
    }

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

    // Port forward event channel and state
    let (pf_event_tx, mut pf_event_rx) = mpsc::unbounded_channel::<PortForwardEvent>();
    let mut pf = PortForwardTable::new();

    // Clipboard event channel and paste-pending state
    let (clipboard_event_tx, mut clipboard_event_rx) = mpsc::unbounded_channel::<ClipboardEvent>();
    let mut pending_paste: Option<tokio::sync::oneshot::Sender<Option<Bytes>>> = None;

    // Negotiated capabilities (shared with svc acceptor)
    let negotiated_caps = Arc::new(std::sync::atomic::AtomicU32::new(0));

    // Bind unified service socket immediately (always available).
    // Drop guards handle cleanup on abort / early return.
    let _svc_cleanup = SocketCleanup(svc_socket_path.clone());
    let _svc_acceptor: Option<AbortOnDrop> =
        bind_agent_listener(&svc_socket_path).map(|listener| {
            AbortOnDrop(spawn_svc_acceptor(
                listener,
                open_event_tx.clone(),
                send_event_tx.clone(),
                clipboard_event_tx.clone(),
                Arc::clone(&negotiated_caps),
            ))
        });

    // Wait for first active client + its Env frame before spawning the
    // shell. If the first client disconnects before sending Env we'd
    // otherwise spawn the shell with empty env permanently; loop back
    // and wait for the next active client instead. A genuine timeout
    // (client connected but hasn't sent Env in 2s) falls through with
    // empty env -- rare in practice.
    let (mut framed, initial_client_name) = loop {
        tokio::select! {
            client = client_rx.recv() => match client {
                Some(ClientConn::Active {
                    framed: f, client_name: cn, capabilities: caps, ..
                }) => {
                    info!("first client connected via channel");
                    negotiated_caps.store(caps, std::sync::atomic::Ordering::Relaxed);
                    break (f, cn);
                }
                Some(ClientConn::Tail(f)) => {
                    info!("tail client connected before shell spawn");
                    spawn_tail(f, &history, &tail_tx);
                    continue;
                }
                Some(ClientConn::Send(stream)) => {
                    handle_send_stream(stream, &send_event_tx);
                    continue;
                }
                Some(ClientConn::Shutdown) => {
                    info!("daemon shutting down before first client");
                    let _ = tail_tx.send(TailEvent::Shutdown);
                    return Ok(());
                }
                None => {
                    info!("client channel closed before first client");
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

    let env_vars = match tokio::time::timeout(std::time::Duration::from_secs(2), framed.next())
        .await
    {
        Ok(Some(Ok(Frame::Env { vars }))) => {
            debug!(count = vars.len(), "received env vars from client");
            vars
        }
        Ok(None) | Ok(Some(Err(_))) => {
            // Control-only callers (tests, tooling that creates but
            // doesn't attach) drop the stream immediately after
            // SessionCreated; spawning with empty env lets those cases
            // proceed. A real attached user who disconnected before
            // sending Env can force the richer env by reattaching.
            warn!("first client disconnected before sending Env frame; spawning with empty env");
            Vec::new()
        }
        Err(_) => {
            warn!("first client did not send Env frame within 2s; spawning with empty env");
            Vec::new()
        }
        Ok(Some(Ok(_other))) => {
            warn!("first client sent unexpected frame instead of Env; spawning with empty env");
            Vec::new()
        }
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
    let client_name = initial_client_name;
    for (k, v) in &env_vars {
        if crate::FORWARDED_ENV_KEYS.contains(&k.as_str()) {
            cmd.env(k, v);
        } else {
            warn!(key = k, "ignoring disallowed env var from client");
        }
    }
    cmd.env("GRITTY_CLIENT", &client_name);
    // Create gritty-open symlink and set BROWSER unconditionally
    let exe = std::env::current_exe()
        .ok()
        .and_then(|p| p.to_str().map(String::from))
        .unwrap_or_else(|| "gritty".into());
    let open_link = svc_socket_path.parent().unwrap_or(Path::new(".")).join("gritty-open");
    let _ = std::fs::remove_file(&open_link);
    let _ = std::os::unix::fs::symlink(&exe, &open_link);
    cmd.env("BROWSER", &open_link);
    // Set SSH_AUTH_SOCK only when the agent listener bound successfully, so the
    // path always resolves. Otherwise leave any inherited ambient agent intact.
    if agent.acceptor.is_some() {
        cmd.env("SSH_AUTH_SOCK", &agent.socket_path);
    }
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
    if let Some(meta) = metadata_slot.get() {
        meta.shell_pid.store(shell_pid, Ordering::Relaxed);
        if let Ok(mut slot) = meta.client_name.lock() {
            *slot = client_name;
        }
    }

    // First client is already connected — enter relay directly
    metadata_slot.get().unwrap().attached.store(true, Ordering::Relaxed);

    // Outer loop: accept clients via channel. PTY persists across reconnects.
    // First iteration skips client-wait (first client already connected above).
    let mut alt_screen = AltScreenTracker::new();
    let mut scrollback = ScrollbackBuffer::new();
    let mut first_client = true;
    let mut pending_input: VecDeque<Bytes> = VecDeque::new();
    let mut pending_input_bytes: usize = 0;
    loop {
        // Hints from the Attach frame of the client we're about to serve.
        // Applied before replay so regenerated prompts / TUI repaints use the
        // current terminal dimensions. 0 = unknown / first client.
        let mut attach_cols: u16 = 0;
        let mut attach_rows: u16 = 0;
        // How far the reconnecting client has rendered into the PTY stream,
        // whether its cursor line needs a repaint, and whether it's a fresh
        // explicit connect -- drives `plan_replay`.
        let mut attach_offset: u64 = 0;
        let mut attach_line_dirty = false;
        let mut attach_is_fresh = false;
        if !first_client {
            let mut detached_exit: Option<i32> = None;
            let got_client = 'drain: loop {
                tokio::select! {
                    biased;
                    client = client_rx.recv() => {
                        match client {
                            Some(ClientConn::Active {
                                framed: f, client_name: cn, capabilities: caps,
                                cols: new_cols, rows: new_rows,
                                rendered_offset, line_dirty, is_fresh,
                            }) => {
                                info!("client connected via channel");
                                negotiated_caps.store(caps, std::sync::atomic::Ordering::Relaxed);
                                framed = f;
                                attach_cols = new_cols;
                                attach_rows = new_rows;
                                attach_offset = rendered_offset;
                                attach_line_dirty = line_dirty;
                                attach_is_fresh = is_fresh;
                                if let Some(meta) = metadata_slot.get()
                                    && let Ok(mut n) = meta.client_name.lock()
                                {
                                    *n = cn;
                                }
                                break 'drain true;
                            }
                            Some(ClientConn::Tail(f)) => {
                                info!("tail client connected while disconnected");
                                spawn_tail(f, &history, &tail_tx);
                                continue;
                            }
                            Some(ClientConn::Send(stream)) => {
                                handle_send_stream(stream, &send_event_tx);
                                continue;
                            }
                            Some(ClientConn::Shutdown) => {
                                info!("daemon shutting down while detached");
                                let _ = tail_tx.send(TailEvent::Shutdown);
                                return Ok(());
                            }
                            None => {
                                info!("client channel closed");
                                break 'drain false;
                            }
                        }
                    }
                    ready = async_master.readable() => {
                        let mut guard = ready?;
                        match guard.try_io(|inner| {
                            nix::unistd::read(inner, &mut buf).map_err(io::Error::from)
                        }) {
                            Ok(Ok(0)) => {
                                debug!("pty EOF while disconnected");
                                detached_exit = Some(0);
                                break 'drain false;
                            }
                            Ok(Ok(n)) => {
                                let chunk = Bytes::copy_from_slice(&buf[..n]);
                                alt_screen.scan(&chunk);
                                // Capture detached output into both history
                                // (for offset-based resume) and scrollback
                                // (main-screen context for a fresh viewer).
                                if !alt_screen.in_alternate_screen() {
                                    scrollback.push(&chunk);
                                }
                                history.push(&chunk);
                                if tail_tx.receiver_count() > 0 {
                                    let _ = tail_tx.send(TailEvent::Data(chunk.clone()));
                                }
                            }
                            Ok(Err(e)) => {
                                if e.raw_os_error() == Some(libc::EIO) {
                                    debug!("pty EIO while disconnected");
                                    detached_exit = Some(0);
                                    break 'drain false;
                                }
                                return Err(e.into());
                            }
                            Err(_would_block) => continue,
                        }
                    }
                    status = managed.child.wait() => {
                        let code = status?.code().unwrap_or(1);
                        info!(code, "shell exited while awaiting client");
                        for chunk in drain_pty_final(&async_master, &mut buf) {
                            alt_screen.scan(&chunk);
                            if !alt_screen.in_alternate_screen() {
                                scrollback.push(&chunk);
                            }
                            if tail_tx.receiver_count() > 0 {
                                let _ = tail_tx.send(TailEvent::Data(chunk.clone()));
                            }
                            history.push(&chunk);
                        }
                        detached_exit = Some(code);
                        break 'drain false;
                    }
                    ready = async_master.writable(), if !pending_input.is_empty() => {
                        drain_pending_input(ready?, &mut pending_input, &mut pending_input_bytes)?;
                    }
                    event = send_event_rx.recv() => {
                        if let Some(event) = event {
                            handle_send_event(event, &mut transfer_state, &send_notify_tx);
                        }
                        continue;
                    }
                    notification = send_notify_rx.recv() => {
                        if let Some(frame) = notification
                            && matches!(frame, Frame::SendDone | Frame::SendCancel { .. })
                            && matches!(transfer_state, TransferState::Active { .. })
                        {
                            transfer_state = TransferState::Idle;
                        }
                        continue;
                    }
                    _ = open_event_rx.recv() => {
                        debug!("discarding open event while detached");
                        continue;
                    }
                    event = clipboard_event_rx.recv() => {
                        match event {
                            Some(ClipboardEvent::Paste { reply }) => {
                                let _ = reply.send(None);
                            }
                            Some(ClipboardEvent::Copy { reply, .. }) => {
                                let _ = reply.send(false);
                            }
                            None => {}
                        }
                        debug!("discarding clipboard event while detached");
                        continue;
                    }
                    _ = agent_event_rx.recv() => {
                        // Drain agent events from still-open reader tasks
                        // left over from the detached client. Without this
                        // arm the channel fills and the reader tasks
                        // block -- an unbounded pump toward a vanished
                        // client.
                        continue;
                    }
                    _ = tunnel_event_rx.recv() => {
                        continue;
                    }
                    _ = pf_event_rx.recv() => {
                        continue;
                    }
                }
            };
            if !got_client {
                if let Some(mut code) = detached_exit {
                    // PTY EOF/EIO may fire before wait() resolves; capture real code.
                    if let Ok(Ok(status)) = tokio::time::timeout(
                        std::time::Duration::from_millis(500),
                        managed.child.wait(),
                    )
                    .await
                    {
                        code = status.code().unwrap_or(code);
                    }
                    let _ = tail_tx.send(TailEvent::Exit { code });
                }
                break;
            }

            if let Some(meta) = metadata_slot.get() {
                meta.attached.store(true, Ordering::Relaxed);
            }
        }
        let is_reconnect = !first_client;
        first_client = false;

        // Bring the reconnecting client back in sync. `send_reconnect_replay`
        // applies the client's winsize before any replay, sends a `Resume`
        // frame carrying its new authoritative stream offset, and then replays
        // exactly what the `plan_replay` decision calls for -- an incremental
        // `Data` tail, a truncation marker, scrollback context, or a full TUI
        // repaint. A send failure (client dropped mid-flush) re-enters drain
        // rather than killing the session.
        let mut flush_failed = false;
        if is_reconnect
            && let Err(e) = send_reconnect_replay(
                &mut framed,
                &async_master,
                &tail_tx,
                &mut history,
                &mut scrollback,
                &mut alt_screen,
                attach_offset,
                attach_line_dirty,
                attach_is_fresh,
                attach_cols,
                attach_rows,
            )
            .await
        {
            warn!(error = %e, "client send failed during reconnect replay, detaching");
            flush_failed = true;
        }
        if flush_failed {
            if let Some(meta) = metadata_slot.get() {
                meta.attached.store(false, Ordering::Relaxed);
            }
            continue;
        }

        // Inner loop: relay between socket and PTY.
        // Scoped block so ServerRelay borrows are released before
        // the post-loop code accesses the underlying state directly.
        let exit = {
            let mut relay = ServerRelay {
                async_master: &async_master,
                pending_input: &mut pending_input,
                pending_input_bytes: &mut pending_input_bytes,
                agent: &mut agent,
                tunnel: &mut tunnel,
                pf: &mut pf,
                transfer_state: &mut transfer_state,
                open_forward_enabled: &mut open_forward_enabled,
                tail_tx: &tail_tx,
                metadata_slot: &metadata_slot,
                tunnel_event_tx: &tunnel_event_tx,
                pf_event_tx: &pf_event_tx,
                send_notify_tx: &send_notify_tx,
                paste_deadline: None,
                pending_paste: &mut pending_paste,
                negotiated_caps: &negotiated_caps,
            };
            let mut last_client_frame_at = tokio::time::Instant::now();
            loop {
                tokio::select! {
                    biased;
                    frame = framed.next() => {
                        last_client_frame_at = tokio::time::Instant::now();
                        if matches!(&frame, Some(Ok(Frame::DiagRequest))) {
                            let text = format_server_diag(
                                &history, &alt_screen, &scrollback, &relay, &managed,
                            );
                            let _ = send_framed_timed(&mut framed, Frame::DiagResponse { text }).await;
                        } else if let ControlFlow::Break(exit) = relay.handle_client_frame(&mut framed, frame).await? {
                            break exit;
                        }
                    }

                    ready = relay.async_master.writable(), if !relay.pending_input.is_empty() => {
                        drain_pending_input(ready?, relay.pending_input, relay.pending_input_bytes)?;
                    }

                    // Idle client eviction: if no frame (not even a Ping)
                    // has arrived in idle_evict_timeout, treat the TCP link
                    // as half-open and release the slot so a new client can
                    // attach. The client's heartbeat cadence (PING_IDLE=10s,
                    // PING_TIMEOUT=60s) means 120s of total silence already
                    // indicates the client has given up or the link is dead.
                    //
                    // Close the socket -- do NOT send Frame::Detached. A
                    // suspended laptop is the common cause of this timeout,
                    // and a Detached frame would sit buffered in sshd's TCP
                    // send queue until wake, at which point the client
                    // reads it as a terminal "taken over" and exits 0
                    // instead of reconnecting. EOF instead routes the
                    // client to RelayExit::Disconnected -> auto-reconnect.
                    () = tokio::time::sleep_until(last_client_frame_at + idle_evict_timeout) => {
                        warn!(
                            "client silent for {:?}, evicting to release attach slot",
                            idle_evict_timeout
                        );
                        let _ = framed.close().await;
                        break RelayExit::ClientGone;
                    }

                    new_client = client_rx.recv() => {
                        match new_client {
                            Some(ClientConn::Active {
                                framed: new_framed, client_name: cn, capabilities: caps,
                                cols: new_cols, rows: new_rows,
                                rendered_offset, line_dirty, is_fresh,
                            }) => {
                                info!("new client via channel, detaching old client");
                                relay.negotiated_caps.store(caps, std::sync::atomic::Ordering::Relaxed);
                                let _ = send_framed_timed(&mut framed, Frame::Detached).await;
                                relay.agent.disable();
                                relay.tunnel.teardown();
                                relay.pf.teardown();
                                *relay.open_forward_enabled = false;
                                relay.paste_deadline = None;
                                if let Some(old) = relay.pending_paste.take() {
                                    let _ = old.send(None);
                                }
                                // Update client_name from the new Attach
                                if let Some(meta) = relay.metadata_slot.get()
                                    && let Ok(mut n) = meta.client_name.lock()
                                {
                                    *n = cn;
                                }
                                framed = new_framed;
                                // Give the new client a full idle-evict budget.
                                // Without this it inherits the old client's
                                // last_client_frame_at, and a near-silent old
                                // client (the usual reason for a force
                                // reattach) would get the new one evicted
                                // seconds after takeover, before its first Ping.
                                last_client_frame_at = tokio::time::Instant::now();
                                // No takeover banner: the displaced client got
                                // a `Detached` frame, and for the client doing
                                // the takeover the session simply reappearing
                                // is confirmation enough. `send_reconnect_replay`
                                // fences the replayed context and handles
                                // alt-screen.
                                let _ = send_reconnect_replay(
                                    &mut framed,
                                    &async_master,
                                    relay.tail_tx,
                                    &mut history,
                                    &mut scrollback,
                                    &mut alt_screen,
                                    rendered_offset,
                                    line_dirty,
                                    is_fresh,
                                    new_cols,
                                    new_rows,
                                )
                                .await;
                            }
                            Some(ClientConn::Tail(f)) => {
                                info!("tail client connected while active");
                                spawn_tail(f, &history, relay.tail_tx);
                            }
                            Some(ClientConn::Send(stream)) => {
                                handle_send_stream(stream, &send_event_tx);
                            }
                            Some(ClientConn::Shutdown) => {
                                info!("daemon shutting down, notifying attached client");
                                break RelayExit::Shutdown;
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

                    event = clipboard_event_rx.recv() => {
                        if let Some(event) = event {
                            if relay.negotiated_caps.load(std::sync::atomic::Ordering::Relaxed)
                                & crate::protocol::CAP_CLIPBOARD == 0
                            {
                                // Client doesn't support clipboard -- drop the event
                                match event {
                                    ClipboardEvent::Paste { reply } => {
                                        let _ = reply.send(None);
                                    }
                                    ClipboardEvent::Copy { reply, .. } => {
                                        let _ = reply.send(false);
                                    }
                                }
                            } else {
                                match event {
                                    ClipboardEvent::Copy { data, reply } => {
                                        info!("clipboard operation forwarded");
                                        let _ = send_framed_timed(&mut framed, Frame::ClipboardSet { data }).await;
                                        let _ = reply.send(true);
                                    }
                                    ClipboardEvent::Paste { reply } => {
                                        info!("clipboard operation forwarded");
                                        // Drop any previous pending paste
                                        if let Some(old) = relay.pending_paste.take() {
                                            let _ = old.send(None);
                                        }
                                        *relay.pending_paste = Some(reply);
                                        relay.paste_deadline = Some(
                                            tokio::time::Instant::now() + std::time::Duration::from_secs(5),
                                        );
                                        let _ = send_framed_timed(&mut framed, Frame::ClipboardGet).await;
                                    }
                                }
                            }
                        }
                    }

                    _ = async {
                        match relay.paste_deadline {
                            Some(deadline) => tokio::time::sleep_until(deadline).await,
                            None => std::future::pending().await,
                        }
                    } => {
                        relay.paste_deadline = None;
                        if let Some(reply) = relay.pending_paste.take() {
                            let _ = reply.send(None);
                        }
                    }

                    // PTY-readable last: under sustained output every other
                    // arm (takeover, agent/pf/tunnel/open/send/clipboard
                    // events, paste/tunnel deadlines, pending-input drain) has
                    // had a chance to fire. Biased select before this change
                    // polled readable() third and starved the rest.
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
                                alt_screen.scan(&chunk);
                                if !alt_screen.in_alternate_screen() {
                                    scrollback.push(&chunk);
                                }
                                // Every byte sent as Data advances the stream
                                // offset, so it must land in history -- that's
                                // what a reconnecting client resumes against.
                                history.push(&chunk);
                                if relay.tail_tx.receiver_count() > 0 {
                                    let _ = relay.tail_tx.send(TailEvent::Data(chunk.clone()));
                                }
                                match tokio::time::timeout(
                                    CLIENT_SEND_TIMEOUT,
                                    framed.send(Frame::Data(chunk)),
                                ).await {
                                    Ok(Ok(())) => {}
                                    Ok(Err(e)) => {
                                        warn!(error = %e, "client send failed, detaching");
                                        break RelayExit::ClientGone;
                                    }
                                    Err(_) => {
                                        warn!("client send timed out, detaching");
                                        break RelayExit::ClientGone;
                                    }
                                }
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

                    status = managed.child.wait() => {
                        let code = status?.code().unwrap_or(1);
                        info!(code, "shell exited");
                        // Don't broadcast TailEvent::Exit here -- the
                        // RelayExit::ShellExited match below does it after
                        // drain_pty_final so tail clients don't exit their
                        // recv loop before the final bytes arrive.
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
                agent.disable();
                tunnel.teardown();
                pf.teardown();
                open_forward_enabled = false;
                if let Some(reply) = pending_paste.take() {
                    let _ = reply.send(None);
                }
                info!(
                    history_bytes = history.size,
                    history_chunks = history.chunks().len(),
                    stream_offset = history.total(),
                    alt_screen = alt_screen.in_alternate_screen(),
                    "client disconnected, waiting for reconnect",
                );
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
                for chunk in drain_pty_final(&async_master, &mut buf) {
                    if tail_tx.receiver_count() > 0 {
                        let _ = tail_tx.send(TailEvent::Data(chunk.clone()));
                    }
                    let _ = send_framed_timed(&mut framed, Frame::Data(chunk)).await;
                }
                let _ = tail_tx.send(TailEvent::Exit { code });
                let _ = send_framed_timed(&mut framed, Frame::Exit { code }).await;
                info!(code, "session ended");
                break;
            }
            RelayExit::Shutdown => {
                // Tell the attached client and any tail observers the daemon
                // is going away so they exit instead of auto-reconnecting.
                let _ = tail_tx.send(TailEvent::Shutdown);
                let _ = send_framed_timed(&mut framed, Frame::ServerShutdown).await;
                let _ = framed.close().await;
                info!("session shut down by daemon");
                break;
            }
        }
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

    /// Display-width in cells of a divider string, ignoring ANSI SGR wrapping.
    fn rule_width(s: &str) -> usize {
        s.trim_start_matches('\r')
            .trim_end_matches("\r\n")
            .trim_start_matches("\x1b[2m")
            .trim_end_matches("\x1b[0m")
            .chars()
            .count()
    }

    #[test]
    fn replay_divider_spans_client_width() {
        assert_eq!(rule_width(&replay_divider(80, None)), 80);
        assert_eq!(rule_width(&replay_divider(20, None)), 20);
    }

    #[test]
    fn replay_divider_falls_back_when_width_unknown() {
        assert_eq!(rule_width(&replay_divider(0, None)), 40);
    }

    #[test]
    fn replay_divider_annotation_keeps_width() {
        let d = replay_divider(80, Some("3.2 KiB dropped while detached"));
        assert_eq!(rule_width(&d), 80);
        assert!(d.contains("3.2 KiB dropped while detached"));
        // Annotation is offset from the left edge.
        assert!(d.contains("\u{2500}\u{2500} 3.2 KiB"));
    }

    #[test]
    fn replay_divider_annotation_wider_than_terminal() {
        // Never panic and never emit a negative fill; just print the prefix.
        let d = replay_divider(5, Some("this annotation is far too long"));
        assert!(d.contains("this annotation is far too long"));
    }

    #[test]
    fn replay_divider_is_dim_and_inline() {
        let d = replay_divider(10, None);
        assert!(d.starts_with("\r\x1b[2m"), "{d:?}");
        assert!(d.ends_with("\x1b[0m\r\n"), "{d:?}");
    }

    fn flatten(chunks: &[Bytes]) -> Vec<u8> {
        chunks.iter().flat_map(|c| c.iter().copied()).collect()
    }

    #[test]
    fn history_tracks_total_and_base() {
        let mut h = History::new(1024);
        assert_eq!(h.total(), 0);
        assert_eq!(h.base(), 0);
        h.push(&Bytes::from_static(b"hello"));
        h.push(&Bytes::from_static(b" world"));
        assert_eq!(h.total(), 11);
        assert_eq!(h.base(), 0);
        assert_eq!(flatten(&h.slice_from(0)), b"hello world");
        assert_eq!(flatten(&h.slice_from(5)), b" world");
        assert_eq!(flatten(&h.slice_from(11)), b"");
    }

    #[test]
    fn history_evicts_past_cap_and_advances_base() {
        let mut h = History::new(8);
        h.push(&Bytes::from_static(b"aaaa")); // total 4
        h.push(&Bytes::from_static(b"bbbb")); // total 8
        h.push(&Bytes::from_static(b"cccc")); // total 12, evict "aaaa"
        assert_eq!(h.total(), 12);
        assert_eq!(h.base(), 4);
        assert_eq!(flatten(&h.slice_from(4)), b"bbbbcccc");
        // A request below base is clamped to base (truncation is detected by
        // the caller via `plan_replay`, not here).
        assert_eq!(flatten(&h.slice_from(0)), b"bbbbcccc");
    }

    #[test]
    fn history_empty_push_is_noop() {
        let mut h = History::new(64);
        h.push(&Bytes::new());
        assert_eq!(h.total(), 0);
        assert_eq!(h.chunks().len(), 0);
    }

    #[test]
    fn history_line_prefix_returns_current_line() {
        let mut h = History::new(1024);
        h.push(&Bytes::from_static(b"first line\nsecond li"));
        // Prefix up to the current total: everything after the last newline.
        assert_eq!(&h.line_prefix(h.total())[..], b"second li");
        // Prefix mid-first-line: no newline before it yet.
        assert_eq!(&h.line_prefix(5)[..], b"first");
        // Prefix exactly at a newline boundary: the line up to and including
        // the newline, so the "current line" is empty.
        assert_eq!(&h.line_prefix(11)[..], b"");
    }

    #[test]
    fn history_line_prefix_spans_chunk_boundaries() {
        let mut h = History::new(1024);
        h.push(&Bytes::from_static(b"ab\ncd"));
        h.push(&Bytes::from_static(b"ef"));
        assert_eq!(&h.line_prefix(h.total())[..], b"cdef");
    }

    #[test]
    fn history_line_prefix_capped_for_newline_free_tail() {
        // A ring larger than 1 MiB with a newline-free tail must not produce
        // a prefix that would overflow MAX_FRAME_SIZE as a single Notice.
        let mut h = History::new(4 << 20);
        h.push(&Bytes::from(vec![b'x'; 3 << 20]));
        let prefix = h.line_prefix(h.total());
        assert_eq!(prefix.len(), LINE_PREFIX_CAP);
        const { assert!(LINE_PREFIX_CAP < (1 << 20), "must stay under MAX_FRAME_SIZE") };
        // A short line is still returned whole.
        let mut h2 = History::new(4 << 20);
        h2.push(&Bytes::from_static(b"\nshort tail"));
        assert_eq!(&h2.line_prefix(h2.total())[..], b"short tail");
    }

    #[test]
    fn plan_replay_alt_screen_always_redraws() {
        // Alt-screen wins regardless of offset or freshness.
        assert_eq!(plan_replay(50, false, true, 0, 100), ReplayPlan::AltRedraw { offset: 100 },);
        assert_eq!(plan_replay(0, true, true, 10, 100), ReplayPlan::AltRedraw { offset: 100 },);
    }

    #[test]
    fn plan_replay_fresh_viewer_gets_scrollback() {
        assert_eq!(plan_replay(0, true, false, 0, 100), ReplayPlan::Fresh { offset: 100 },);
        // A nonsense offset ahead of the stream is treated as fresh, not
        // trusted.
        assert_eq!(plan_replay(500, false, false, 0, 100), ReplayPlan::Fresh { offset: 100 },);
    }

    #[test]
    fn plan_replay_clean_resume_within_history() {
        assert_eq!(plan_replay(60, false, false, 20, 100), ReplayPlan::Clean { offset: 60 },);
        // Offset exactly at the tail: clean resume of zero bytes.
        assert_eq!(plan_replay(100, false, false, 20, 100), ReplayPlan::Clean { offset: 100 },);
        // Offset exactly at the base edge is still clean (not truncated).
        assert_eq!(plan_replay(20, false, false, 20, 100), ReplayPlan::Clean { offset: 20 },);
    }

    #[test]
    fn plan_replay_truncated_when_offset_fell_out_of_history() {
        assert_eq!(
            plan_replay(5, false, false, 40, 100),
            ReplayPlan::Truncated { offset: 40, dropped: 35 },
        );
    }

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

    #[test]
    fn sanitize_filename_preserves_nested() {
        assert_eq!(sanitize_filename("dir/sub/file.txt"), Some("dir/sub/file.txt".into()));
        assert_eq!(sanitize_filename("file.txt"), Some("file.txt".into()));
    }

    #[test]
    fn sanitize_filename_rejects_traversal() {
        assert_eq!(sanitize_filename("../../etc/passwd"), None);
        assert_eq!(sanitize_filename("a/../b"), None);
        assert_eq!(sanitize_filename("/etc/passwd"), None);
        assert_eq!(sanitize_filename("."), None);
        assert_eq!(sanitize_filename(""), None);
        assert_eq!(sanitize_filename("a\0b"), None);
    }
}
