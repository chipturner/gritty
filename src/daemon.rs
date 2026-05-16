use crate::protocol::{
    CAP_CLIPBOARD, ErrorCode, Frame, FrameCodec, PROTOCOL_VERSION, SessionEntry,
};
use crate::server::{self, ClientConn, SessionMetadata};
use futures_util::{SinkExt, StreamExt};
use std::collections::HashMap;
use std::os::fd::OwnedFd;
use std::path::{Path, PathBuf};
use std::sync::atomic::Ordering;
use std::sync::{Arc, OnceLock};
use std::time::Duration;
use tokio::net::UnixStream;
use tokio::sync::mpsc;
use tokio::task::JoinHandle;
use tokio_util::codec::Framed;
use tracing::{Instrument, debug, error, info, warn};

const SEND_TIMEOUT: Duration = Duration::from_secs(5);

/// Send a frame with a timeout. Returns `Ok(())` on success, `Err` on
/// send failure or timeout (error is logged before returning).
async fn timed_send(
    framed: &mut Framed<UnixStream, FrameCodec>,
    frame: Frame,
) -> Result<(), std::io::Error> {
    match tokio::time::timeout(SEND_TIMEOUT, framed.send(frame)).await {
        Ok(Ok(())) => Ok(()),
        Ok(Err(e)) => {
            warn!("control send error: {e}");
            Err(e)
        }
        Err(_) => {
            warn!("control send timed out");
            Err(std::io::Error::new(std::io::ErrorKind::TimedOut, "send timed out"))
        }
    }
}

struct SessionState {
    handle: JoinHandle<anyhow::Result<()>>,
    metadata: Arc<OnceLock<SessionMetadata>>,
    client_tx: mpsc::UnboundedSender<ClientConn>,
    name: Option<String>,
}

/// Validate a `GRITTY_SOCKET_DIR` override. It must be a non-empty absolute
/// path: `daemonize()` does `chdir("/")` before the daemon binds its socket, so
/// a relative value resolves against three different working directories
/// (launcher, daemon, client) and produces an opaque three-way path mismatch.
fn validated_socket_dir_override(dir: &str) -> Result<PathBuf, String> {
    if dir.is_empty() {
        return Err("GRITTY_SOCKET_DIR is set but empty".to_string());
    }
    let path = PathBuf::from(dir);
    if !path.is_absolute() {
        return Err(format!("GRITTY_SOCKET_DIR must be an absolute path (got {dir:?})"));
    }
    Ok(path)
}

/// Returns the base directory for gritty sockets.
/// Prefers $GRITTY_SOCKET_DIR, then $XDG_RUNTIME_DIR/gritty, falls back to /tmp/gritty-$UID.
pub fn socket_dir() -> PathBuf {
    if let Ok(dir) = std::env::var("GRITTY_SOCKET_DIR") {
        match validated_socket_dir_override(&dir) {
            Ok(path) => return path,
            Err(e) => {
                eprintln!("error: {e}");
                std::process::exit(1);
            }
        }
    }
    if let Some(proj) = directories::ProjectDirs::from("", "", "gritty")
        && let Some(runtime) = proj.runtime_dir()
    {
        return runtime.to_path_buf();
    }
    let uid = unsafe { libc::getuid() };
    PathBuf::from(format!("/tmp/gritty-{uid}"))
}

/// Returns the daemon socket path.
pub fn control_socket_path() -> PathBuf {
    socket_dir().join("ctl.sock")
}

/// Returns the PID file path (sibling to ctl.sock).
pub fn pid_file_path(ctl_path: &Path) -> PathBuf {
    ctl_path.with_file_name("daemon.pid")
}

fn reap_sessions(sessions: &mut HashMap<u32, SessionState>) {
    use futures_util::future::FutureExt;
    let finished: Vec<u32> = sessions
        .iter()
        .filter_map(|(&id, state)| state.handle.is_finished().then_some(id))
        .collect();
    for id in finished {
        if let Some(state) = sessions.remove(&id) {
            // Handle is already finished, so `now_or_never` completes
            // synchronously. Log panics / inner errors so operators
            // aren't left wondering why a session vanished.
            match state.handle.now_or_never() {
                Some(Ok(Ok(()))) => info!(id, "session ended"),
                Some(Ok(Err(e))) => {
                    tracing::error!(id, error = %e, "session task returned error");
                }
                Some(Err(join_err)) => {
                    if join_err.is_panic() {
                        tracing::error!(id, "session task panicked: {join_err}");
                    } else {
                        tracing::warn!(id, "session task cancelled: {join_err}");
                    }
                }
                None => tracing::warn!(id, "session task finished but join pending"),
            }
        }
    }
}

/// Resolve a session identifier (name, id string, or "-" for last attached) to a session id.
fn resolve_session(
    sessions: &HashMap<u32, SessionState>,
    target: &str,
    last_attached: Option<u32>,
) -> Option<u32> {
    // "-" means last attached session
    if target == "-" {
        return last_attached.filter(|id| sessions.contains_key(id));
    }
    // Try name match first
    for (&id, state) in sessions {
        if state.name.as_deref() == Some(target) {
            return Some(id);
        }
    }
    // Then try parsing as numeric id
    if let Ok(id) = target.parse::<u32>()
        && sessions.contains_key(&id)
    {
        return Some(id);
    }
    None
}

/// Validate a proposed session name against the rules shared by `NewSession`
/// and `RenameSession`, returning the error frame to send on rejection.
///
/// Empty names are intentionally not checked here: `NewSession` maps an empty
/// name to "unnamed" before validating, while `RenameSession` rejects it with
/// its own distinct `EmptyName` code. Everything else (control chars, purely
/// numeric, reserved `-`, duplicate) is identical between the two and lives
/// here so the rules and their exact messages cannot drift.
fn validate_session_name(name: &str, sessions: &HashMap<u32, SessionState>) -> Result<(), Frame> {
    if name.bytes().any(|b| b.is_ascii_control()) {
        return Err(Frame::Error {
            code: ErrorCode::InvalidName,
            message: "session name must not contain control characters".to_string(),
        });
    }
    if name.parse::<u32>().is_ok() {
        return Err(Frame::Error {
            code: ErrorCode::InvalidName,
            message: "session name must not be purely numeric (ambiguous with session IDs)"
                .to_string(),
        });
    }
    if name == "-" {
        return Err(Frame::Error {
            code: ErrorCode::InvalidName,
            message: "session name must not be '-' (reserved for last-attached)".to_string(),
        });
    }
    if sessions.values().any(|s| s.name.as_deref() == Some(name)) {
        return Err(Frame::Error {
            code: ErrorCode::NameAlreadyExists,
            message: format!("session name already exists: {name}"),
        });
    }
    Ok(())
}

/// Reap finished sessions, then resolve `session` to a *live* session id.
///
/// Returns the `NoSuchSession` error frame when the name doesn't resolve, or
/// when the resolved session's task has already ended (`client_tx` closed) --
/// in which case the stale entry is removed. Centralizes the
/// reap -> resolve -> dead-task-sweep skeleton that control arms repeat; the
/// dead-task check used to be applied inconsistently (Attach/Tail/SendFile had
/// it, KillSession/RenameSession did not).
fn resolve_live_session(
    sessions: &mut HashMap<u32, SessionState>,
    session: &str,
    last_attached: Option<u32>,
) -> Result<u32, Frame> {
    reap_sessions(sessions);
    let not_found = || Frame::Error {
        code: ErrorCode::NoSuchSession,
        message: format!("no such session: {session}"),
    };
    let Some(id) = resolve_session(sessions, session, last_attached) else {
        return Err(not_found());
    };
    if sessions[&id].client_tx.is_closed() {
        sessions.remove(&id);
        return Err(not_found());
    }
    Ok(id)
}

/// Read the foreground process command name for a shell pid.
/// Returns "-" on any failure.
#[cfg(target_os = "linux")]
fn foreground_process(shell_pid: u32) -> String {
    // Read /proc/{shell_pid}/stat to get tpgid (field 8, 1-indexed)
    let stat = match std::fs::read_to_string(format!("/proc/{shell_pid}/stat")) {
        Ok(s) => s,
        Err(_) => return "-".to_string(),
    };
    // Fields are space-separated, but field 2 (comm) is in parens and may contain spaces.
    // Find the closing paren, then parse fields after it.
    let after_comm = match stat.rfind(')') {
        Some(pos) => &stat[pos + 2..], // skip ") "
        None => return "-".to_string(),
    };
    // Fields after comm: state(3) ppid(4) pgrp(5) session(6) tty_nr(7) tpgid(8)
    // That's index 5 in the remaining space-separated fields (0-indexed)
    let fields: Vec<&str> = after_comm.split_whitespace().collect();
    let tpgid = match fields.get(5).and_then(|s| s.parse::<u32>().ok()) {
        Some(t) if t > 0 => t,
        _ => return "-".to_string(),
    };
    // Read /proc/{tpgid}/comm
    std::fs::read_to_string(format!("/proc/{tpgid}/comm"))
        .map(|s| s.trim().to_string())
        .unwrap_or_else(|_| "-".to_string())
}

/// Read the foreground process command name for a shell pid via libproc.
/// Returns "-" on any failure.
#[cfg(target_os = "macos")]
fn foreground_process(shell_pid: u32) -> String {
    use libproc::libproc::bsd_info::BSDInfo;
    use libproc::libproc::proc_pid::{name, pidinfo};

    let pid = shell_pid as i32;
    let tpgid = match pidinfo::<BSDInfo>(pid, 0) {
        Ok(info) if info.e_tpgid > 0 => info.e_tpgid as i32,
        _ => return "-".to_string(),
    };
    name(tpgid).unwrap_or_else(|_| "-".to_string())
}

#[cfg(target_os = "linux")]
fn foreground_cwd(shell_pid: u32) -> String {
    libproc::libproc::proc_pid::pidcwd(shell_pid as i32)
        .map(|p| p.to_string_lossy().into_owned())
        .unwrap_or_default()
}

#[cfg(target_os = "macos")]
fn foreground_cwd(shell_pid: u32) -> String {
    // libproc's pidcwd is unimplemented on macOS; call proc_pidinfo directly.
    const PROC_PIDVNODEPATHINFO: i32 = 9;
    const MAXPATHLEN: usize = 1024;
    // vnode_info: vinfo_stat(136) + vi_type(4) + vi_pad(4) + vi_fsid(8) = 152
    const VNODE_INFO_SIZE: usize = 152;
    // vnode_info_path: vnode_info + path
    const VNODE_INFO_PATH_SIZE: usize = VNODE_INFO_SIZE + MAXPATHLEN;
    // proc_vnodepathinfo: cdir + rdir
    const BUF_SIZE: usize = VNODE_INFO_PATH_SIZE * 2;

    let mut buf = vec![0u8; BUF_SIZE];
    let ret = unsafe {
        libc::proc_pidinfo(
            shell_pid as i32,
            PROC_PIDVNODEPATHINFO,
            0,
            buf.as_mut_ptr().cast(),
            BUF_SIZE as i32,
        )
    };
    if ret <= 0 {
        return String::new();
    }
    // cdir path starts after the vnode_info struct
    let path_bytes = &buf[VNODE_INFO_SIZE..VNODE_INFO_SIZE + MAXPATHLEN];
    let len = path_bytes.iter().position(|&b| b == 0).unwrap_or(MAXPATHLEN);
    String::from_utf8_lossy(&path_bytes[..len]).into_owned()
}

fn build_session_entries(
    sessions: &HashMap<u32, SessionState>,
    last_attached: Option<u32>,
) -> Vec<SessionEntry> {
    let mut entries: Vec<_> = sessions
        .iter()
        .map(|(&id, state)| {
            let is_last_attached = last_attached == Some(id);
            if let Some(meta) = state.metadata.get() {
                SessionEntry {
                    id,
                    name: state.name.clone().unwrap_or_default(),
                    pty_path: meta.pty_path.clone(),
                    shell_pid: meta.shell_pid.load(Ordering::Relaxed),
                    created_at: meta.created_at,
                    attached: meta.attached.load(Ordering::Relaxed),
                    last_heartbeat: meta.last_heartbeat.load(Ordering::Relaxed),
                    foreground_cmd: foreground_process(meta.shell_pid.load(Ordering::Relaxed)),
                    cwd: foreground_cwd(meta.shell_pid.load(Ordering::Relaxed)),
                    client_name: meta.client_name.lock().map(|n| n.clone()).unwrap_or_default(),
                    agent_forwarding_active: meta.wants_agent.load(Ordering::Relaxed),
                    is_last_attached,
                }
            } else {
                SessionEntry {
                    id,
                    name: state.name.clone().unwrap_or_default(),
                    pty_path: String::new(),
                    shell_pid: 0,
                    created_at: 0,
                    attached: false,
                    last_heartbeat: 0,
                    foreground_cmd: "-".to_string(),
                    cwd: String::new(),
                    client_name: String::new(),
                    agent_forwarding_active: false,
                    is_last_attached,
                }
            }
        })
        .collect();
    entries.sort_by_key(|e| e.id);
    entries
}

/// Graceful daemon shutdown. Sends `ClientConn::Shutdown` to every session so
/// they can tell their attached/tail clients `Frame::ServerShutdown` before
/// exiting -- a client that sees that frame exits immediately instead of
/// spinning in its reconnect loop against a socket that will never answer
/// (which, for a remote host behind a live tunnel, can take minutes to
/// resolve). Waits a bounded window for all sessions to flush, then aborts
/// any stragglers so `kill-server` stays prompt.
async fn shutdown(sessions: &mut HashMap<u32, SessionState>, ctl_path: &Path) {
    // Signal first, collect handles, then await -- so sessions drain
    // concurrently under a single shared deadline rather than serially.
    let mut handles = Vec::with_capacity(sessions.len());
    for (id, state) in sessions.drain() {
        let _ = state.client_tx.send(ClientConn::Shutdown);
        handles.push((id, state.handle));
    }
    let deadline = tokio::time::Instant::now() + Duration::from_millis(500);
    for (id, mut handle) in handles {
        match tokio::time::timeout_at(deadline, &mut handle).await {
            Ok(_) => info!(id, "session shut down gracefully"),
            Err(_) => {
                handle.abort();
                info!(id, "session did not shut down in time; aborted");
            }
        }
    }
    let _ = std::fs::remove_file(ctl_path);
    let _ = std::fs::remove_file(pid_file_path(ctl_path));
    let _ = std::fs::remove_file(crate::runinfo::daemon_info_path(ctl_path));
}

/// Outcome of the version check during `connection_handshake`. A mismatched
/// client is still handed to the main loop so it can ask for `KillServer`
/// (the recovery path during upgrades), but all other frames are rejected
/// with `VersionMismatch` before they touch session state.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum VersionCheck {
    Matched,
    Mismatched { client_version: u16 },
}

/// Perform Hello/HelloAck handshake and read control frame for a single connection.
/// Spawned as a per-connection task so slow clients don't block the accept loop.
#[allow(clippy::type_complexity)]
async fn connection_handshake(
    stream: UnixStream,
    tx: mpsc::Sender<(Frame, Framed<UnixStream, FrameCodec>, u32, VersionCheck, u64)>,
    server_id: u64,
) {
    let mut framed = Framed::new(stream, FrameCodec);

    // Read Hello handshake (5s timeout)
    let (version, client_caps, device_id) =
        match tokio::time::timeout(Duration::from_secs(5), framed.next()).await {
            Ok(Some(Ok(Frame::Hello { version, capabilities, device_id }))) => {
                (version, capabilities, device_id)
            }
            Ok(Some(Ok(_))) => {
                let _ = timed_send(
                    &mut framed,
                    Frame::Error {
                        code: ErrorCode::UnexpectedFrame,
                        message: "expected Hello handshake".to_string(),
                    },
                )
                .await;
                return;
            }
            Ok(Some(Err(e))) => {
                warn!("frame decode error: {e}");
                return;
            }
            Ok(None) => return,
            Err(_) => {
                warn!("control connection timed out (hello)");
                return;
            }
        };

    // Always send HelloAck, even on version mismatch, so the client sees the
    // server's version and can either bail with an actionable error or proceed
    // with a KillServer recovery request. Per-frame version gating in the main
    // loop keeps session operations safe.
    let server_caps = CAP_CLIPBOARD;
    if timed_send(
        &mut framed,
        Frame::HelloAck { version: PROTOCOL_VERSION, capabilities: server_caps, server_id },
    )
    .await
    .is_err()
    {
        return;
    }

    let (check, negotiated) = if version == PROTOCOL_VERSION {
        (VersionCheck::Matched, client_caps & server_caps)
    } else {
        warn!(
            client_version = version,
            server_version = PROTOCOL_VERSION,
            "version mismatch -- connection restricted to KillServer"
        );
        (VersionCheck::Mismatched { client_version: version }, 0u32)
    };

    // Read control frame (5s timeout)
    let frame = match tokio::time::timeout(Duration::from_secs(5), framed.next()).await {
        Ok(Some(Ok(f))) => f,
        Ok(Some(Err(e))) => {
            warn!("frame decode error: {e}");
            return;
        }
        Ok(None) => return,
        Err(_) => {
            warn!("control connection timed out");
            return;
        }
    };

    let _ = tx.send((frame, framed, negotiated, check, device_id)).await;
}

/// Run the daemon, listening on its socket.
///
/// If `ready_fd` is provided, a single byte is written to it after the socket
/// is bound, then the fd is dropped. This unblocks the parent process after
/// `daemonize()` forks.
pub async fn run(ctl_path: &Path, ready_fd: Option<OwnedFd>) -> anyhow::Result<()> {
    // Restrictive umask for all files/sockets created by the daemon
    unsafe {
        libc::umask(0o077);
    }

    // Ensure parent directory exists with secure permissions
    if let Some(parent) = ctl_path.parent() {
        crate::security::secure_create_dir_all(parent)?;
    }

    let listener = crate::security::bind_unix_listener(ctl_path)?;
    // Ephemeral identifier included in every HelloAck; a reconnecting client
    // that sees a different value knows this is a different daemon and its
    // session is gone. Nanos XOR pid is unique enough in practice.
    let server_id: u64 = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos() as u64)
        .unwrap_or(0)
        ^ (std::process::id() as u64);
    info!(path = %ctl_path.display(), server_id, "daemon listening");

    // Complete initialization BEFORE signaling readiness. If PID-file write
    // or signal-handler setup fails after the readiness byte is sent, the
    // parent has already reported success and exited; the daemon then
    // returns Err into a closed pipe and the user sees no failure. Only a
    // fully-initialized daemon should report ready.
    let pid_path = pid_file_path(ctl_path);
    std::fs::write(&pid_path, std::process::id().to_string())?;
    // Record our identity so `gritty doctor` can detect a stale daemon
    // (binary replaced on disk after we started). Best-effort -- a missing
    // `.info` just means doctor can't flag staleness, not a hard failure.
    let _ = crate::runinfo::RunInfo::current().write(&crate::runinfo::daemon_info_path(ctl_path));

    let mut sessions: HashMap<u32, SessionState> = HashMap::new();
    let mut next_id: u32 = 0;
    let mut next_conn_id: u64 = 0;
    let mut last_attached: Option<u32> = None;
    let session_config = crate::config::ConfigFile::load().resolve_session(None);
    let ring_buffer_cap = session_config.ring_buffer_size as usize;
    let oauth_tunnel_idle_timeout = session_config.oauth_tunnel_idle_timeout;

    // Signal handlers
    let mut sigterm = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())?;
    let mut sigint = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::interrupt())?;
    let mut sigusr1 =
        tokio::signal::unix::signal(tokio::signal::unix::SignalKind::user_defined1())?;
    let mut sigusr2 =
        tokio::signal::unix::signal(tokio::signal::unix::SignalKind::user_defined2())?;

    // Now fully initialized -- signal readiness to parent (daemonize pipe):
    // [0x01][pid: u32 LE]
    if let Some(fd) = ready_fd {
        use std::io::Write;
        let mut f = std::fs::File::from(fd);
        let pid = std::process::id();
        let mut buf = [0u8; 5];
        buf[0] = 0x01;
        buf[1..5].copy_from_slice(&pid.to_le_bytes());
        let _ = f.write_all(&buf);
        // f drops here, closing the pipe
    }

    // Channel for handshake results -- spawned tasks send completed handshakes here
    let (conn_tx, mut conn_rx) =
        mpsc::channel::<(Frame, Framed<UnixStream, FrameCodec>, u32, VersionCheck, u64)>(64);

    loop {
        reap_sessions(&mut sessions);

        let should_break = tokio::select! {
            result = listener.accept() => {
                match result {
                    Ok((stream, _addr)) => {
                        if let Err(e) = crate::security::verify_peer_uid(&stream) {
                            warn!("{e}");
                        } else {
                            let conn_id = next_conn_id;
                            next_conn_id = next_conn_id.wrapping_add(1);
                            debug!(conn_id, "accepted connection");
                            let tx = conn_tx.clone();
                            let conn_span = tracing::debug_span!("conn", id = conn_id);
                            tokio::spawn(
                                connection_handshake(stream, tx, server_id)
                                    .instrument(conn_span),
                            );
                        }
                    }
                    Err(e) => {
                        warn!("ctl accept error: {e}; retrying");
                        tokio::time::sleep(Duration::from_millis(100)).await;
                    }
                }
                false
            }
            Some((frame, mut framed, capabilities, check, device_id)) = conn_rx.recv() => {
                // Under a version mismatch the only frame we honor is
                // KillServer -- it's the escape hatch for recovering from a
                // half-upgraded deployment. Any other request would touch
                // session state we can't safely interpret across a version
                // boundary, so reject it with an actionable error.
                if let VersionCheck::Mismatched { client_version } = check
                    && !matches!(frame, Frame::KillServer)
                {
                    let _ = timed_send(
                        &mut framed,
                        Frame::Error {
                            code: ErrorCode::VersionMismatch,
                            message: format!(
                                "protocol version mismatch: client={client_version} server={PROTOCOL_VERSION}; \
                                 only KillServer is accepted -- run `gritty restart` to upgrade"
                            ),
                        },
                    )
                    .await;
                    false
                } else {
                    dispatch_control(
                        frame, framed, &mut sessions, &mut next_id, ctl_path, &mut last_attached,
                        ring_buffer_cap, oauth_tunnel_idle_timeout, capabilities, device_id,
                    ).await
                }
            }
            _ = sigterm.recv() => {
                info!("SIGTERM received, shutting down");
                shutdown(&mut sessions, ctl_path).await;
                true
            }
            _ = sigint.recv() => {
                info!("SIGINT received, shutting down");
                shutdown(&mut sessions, ctl_path).await;
                true
            }
            _ = sigusr1.recv() => {
                crate::logging::cycle_log_level();
                info!(level = crate::logging::current_log_level_name(), "log level changed via SIGUSR1");
                false
            }
            _ = sigusr2.recv() => {
                crate::logging::reopen_log_file();
                info!("log file reopened via SIGUSR2");
                false
            }
        };

        if should_break {
            break;
        }
    }

    Ok(())
}

/// Wait briefly for the session task to publish its metadata, then mark the
/// session attached.
///
/// The session task publishes `metadata` early (before the shell spawn and
/// Env wait) but sets `attached` only much later, after its Env wait. Because
/// control-frame dispatch is serialized, marking the session attached here --
/// before the NewSession handler returns -- guarantees a racing follow-up
/// non-forced Attach observes it and is rejected with AlreadyAttached instead
/// of silently stealing the brand-new session from its creator.
///
/// Best-effort: the session task also sets this flag, so a timeout here (the
/// task never published metadata) is harmless.
async fn mark_attached_when_ready(metadata: &Arc<OnceLock<SessionMetadata>>) {
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(3);
    loop {
        if let Some(m) = metadata.get() {
            m.attached.store(true, Ordering::Relaxed);
            return;
        }
        if std::time::Instant::now() >= deadline {
            return;
        }
        tokio::time::sleep(std::time::Duration::from_millis(25)).await;
    }
}

/// Dispatch a single control frame. Takes ownership of the framed connection
/// so it can be handed off to session tasks when needed. Returns `true` for
/// KillServer (daemon should exit).
#[allow(clippy::too_many_arguments)]
async fn dispatch_control(
    frame: Frame,
    mut framed: Framed<UnixStream, FrameCodec>,
    sessions: &mut HashMap<u32, SessionState>,
    next_id: &mut u32,
    ctl_path: &Path,
    last_attached: &mut Option<u32>,
    ring_buffer_cap: usize,
    oauth_tunnel_idle_timeout: u64,
    capabilities: u32,
    device_id: u64,
) -> bool {
    match frame {
        Frame::NewSession { name, command, cwd, cols, rows, client_name } => {
            // Reap before checking for duplicate names -- a session that
            // just exited still lives in `sessions` until the next reap,
            // and would otherwise trigger a spurious NameAlreadyExists.
            reap_sessions(sessions);
            let name_opt = if name.is_empty() { None } else { Some(name) };
            let command_opt = if command.is_empty() { None } else { Some(command) };
            let cwd_opt = if cwd.is_empty() { None } else { Some(cwd) };
            // An empty name means "unnamed" (None); only a provided name is
            // validated. The rules are shared with RenameSession.
            if let Some(ref n) = name_opt
                && let Err(f) = validate_session_name(n, sessions)
            {
                let _ = timed_send(&mut framed, f).await;
                return false;
            }

            let id = *next_id;
            *next_id += 1;

            let (client_tx, client_rx) = mpsc::unbounded_channel();
            let metadata = Arc::new(OnceLock::new());
            let meta_clone = Arc::clone(&metadata);
            let meta_for_mark = Arc::clone(&metadata);
            let sock_dir = ctl_path.parent().expect("ctl_path must have a parent");
            let agent_socket_path = sock_dir.join(format!("agent-{id}.sock"));
            let svc_socket_path = sock_dir.join(format!("svc-{id}.sock"));
            let name_for_server = name_opt.clone();
            let cmd_for_server = command_opt;
            let cwd_for_server = cwd_opt;
            // Record the creator's device as the session owner.
            let session_span =
                tracing::info_span!("session", id = id, name = name_opt.as_deref().unwrap_or(""),);
            let handle = tokio::spawn(
                server::run(
                    client_rx,
                    meta_clone,
                    server::SessionConfig {
                        agent_socket_path,
                        svc_socket_path,
                        session_id: id,
                        session_name: name_for_server,
                        command: cmd_for_server,
                        ring_buffer_cap,
                        oauth_tunnel_idle_timeout,
                        initial_cols: cols,
                        initial_rows: rows,
                        cwd: cwd_for_server,
                        initial_device_id: device_id,
                        idle_evict_timeout: crate::protocol::IDLE_EVICT_TIMEOUT,
                    },
                )
                .instrument(session_span),
            );

            sessions.insert(
                id,
                SessionState {
                    handle,
                    metadata,
                    client_tx: client_tx.clone(),
                    name: name_opt.clone(),
                },
            );

            info!(id, name = ?name_opt, "session created");

            // If the creator vanishes before we can hand off, the session
            // task blocks forever awaiting its first ClientConn::Active
            // -- the shell never spawns, but the name/id remain
            // reserved. Abort the task and drop the entry so future
            // NewSession for the same name succeeds.
            let send_ok = timed_send(&mut framed, Frame::SessionCreated { id }).await.is_ok()
                && timed_send(&mut framed, Frame::AttachAck { token: device_id, session_id: id })
                    .await
                    .is_ok();
            if !send_ok {
                if let Some(state) = sessions.remove(&id) {
                    state.handle.abort();
                }
                info!(id, "NewSession creator gone before hand-off; rolled back");
                return false;
            }

            // Mark the session attached before returning. Control dispatch is
            // serialized, so a follow-up non-forced `gritty connect` must see
            // the creator's session as attached and get AlreadyAttached,
            // rather than stealing it during the birth window before the
            // session task (after its Env wait) would set this itself.
            mark_attached_when_ready(&meta_for_mark).await;

            // Hand off connection to session for auto-attach. The session was
            // just created with the requested cols/rows, so no Attach-side
            // winsize override is needed on this path.
            *last_attached = Some(id);
            let _ = client_tx.send(ClientConn::Active {
                framed,
                client_name,
                capabilities,
                cols: 0,
                rows: 0,
                // The creator auto-attaches to a brand-new session: no prior
                // stream position, nothing to replay.
                rendered_offset: 0,
                line_dirty: false,
                is_fresh: true,
            });
            false
        }
        Frame::Attach {
            session,
            client_name,
            force,
            no_replay,
            cols,
            rows,
            attach_token: provided_token,
            rendered_offset,
            line_dirty,
        } => {
            reap_sessions(sessions);
            if let Some(id) = resolve_session(sessions, &session, *last_attached) {
                let state = &sessions[&id];
                if state.client_tx.is_closed() {
                    sessions.remove(&id);
                    let _ = timed_send(
                        &mut framed,
                        Frame::Error {
                            code: ErrorCode::NoSuchSession,
                            message: format!("no such session: {session}"),
                        },
                    )
                    .await;
                } else if no_replay {
                    // Probe only (`connect -d`): confirm existence without
                    // handing off to the session task, so the ring buffer is
                    // not drained and no attached client is evicted. Probes
                    // do not claim ownership and get plain `Ok`.
                    let _ = timed_send(&mut framed, Frame::Ok).await;
                } else {
                    let current_owner = state
                        .metadata
                        .get()
                        .map(|m| m.owner_device_id.load(Ordering::Relaxed))
                        .unwrap_or(0);
                    // Auto-reconnect (provided_token != 0): the client claims
                    // ownership. Check Hello's device_id against the stored
                    // owner. A mismatch means a different device took over.
                    if provided_token != 0 && device_id != current_owner {
                        let _ = timed_send(
                            &mut framed,
                            Frame::Error {
                                code: ErrorCode::OwnerChanged,
                                message: format!(
                                    "session {session} was taken over by another client"
                                ),
                            },
                        )
                        .await;
                        return false;
                    }
                    let is_attached = state
                        .metadata
                        .get()
                        .map(|m| m.attached.load(Ordering::Relaxed))
                        .unwrap_or(false);
                    if is_attached && !force {
                        let current = state
                            .metadata
                            .get()
                            .and_then(|m| m.client_name.lock().ok().map(|g| g.clone()))
                            .unwrap_or_default();
                        let message = if current.is_empty() {
                            format!("session {session} is already attached")
                        } else {
                            format!("session {session} is already attached by {current}")
                        };
                        let _ = timed_send(
                            &mut framed,
                            Frame::Error { code: ErrorCode::AlreadyAttached, message },
                        )
                        .await;
                    } else {
                        // If the session was just created but server::run
                        // hasn't yet populated `metadata` (shell still
                        // spawning), we can't persist the new owner's
                        // device_id. Poll briefly so a user's follow-up
                        // `gritty connect host:name` right after create
                        // doesn't race the spawn.
                        let meta = {
                            let deadline =
                                std::time::Instant::now() + std::time::Duration::from_secs(3);
                            loop {
                                if let Some(m) = state.metadata.get() {
                                    break Some(m);
                                }
                                if std::time::Instant::now() >= deadline {
                                    break None;
                                }
                                tokio::time::sleep(std::time::Duration::from_millis(25)).await;
                            }
                        };
                        let Some(meta) = meta else {
                            let _ = timed_send(
                                &mut framed,
                                Frame::Error {
                                    code: ErrorCode::NoSuchSession,
                                    message: format!("session {session} is still initializing"),
                                },
                            )
                            .await;
                            return false;
                        };
                        // Only update the stored owner after the new client
                        // has ACK'd receipt. If the AttachAck send fails,
                        // the previous owner should keep their device_id so
                        // their next reconnect doesn't get OwnerChanged on
                        // a takeover that never actually landed.
                        if timed_send(
                            &mut framed,
                            Frame::AttachAck { token: device_id, session_id: id },
                        )
                        .await
                        .is_ok()
                        {
                            meta.owner_device_id.store(device_id, Ordering::Relaxed);
                            // Mark attached here, at hand-off, not later in the
                            // session task. Control dispatch is serialized, so
                            // a subsequent non-forced Attach is guaranteed to
                            // observe this before the session task would have
                            // set it (after its Env wait) -- without it a racing
                            // second `connect` steals the session instead of
                            // getting AlreadyAttached.
                            meta.attached.store(true, Ordering::Relaxed);
                            *last_attached = Some(id);
                            let _ = state.client_tx.send(ClientConn::Active {
                                framed,
                                client_name,
                                capabilities,
                                cols,
                                rows,
                                rendered_offset,
                                line_dirty,
                                // attach_token == 0 is an explicit `connect`
                                // (fresh viewer); non-zero is an auto-reconnect
                                // that resumes from `rendered_offset`.
                                is_fresh: provided_token == 0,
                            });
                        }
                    }
                }
            } else {
                let _ = timed_send(
                    &mut framed,
                    Frame::Error {
                        code: ErrorCode::NoSuchSession,
                        message: format!("no such session: {session}"),
                    },
                )
                .await;
            }
            false
        }
        Frame::Tail { session } => {
            let id = match resolve_live_session(sessions, &session, *last_attached) {
                Ok(id) => id,
                Err(f) => {
                    let _ = timed_send(&mut framed, f).await;
                    return false;
                }
            };
            let state = &sessions[&id];
            if timed_send(&mut framed, Frame::Ok).await.is_ok() {
                let _ = state.client_tx.send(ClientConn::Tail(framed));
            }
            false
        }
        Frame::ListSessions => {
            reap_sessions(sessions);
            let entries = build_session_entries(sessions, *last_attached);
            let _ = timed_send(&mut framed, Frame::SessionInfo { sessions: entries }).await;
            false
        }
        Frame::KillSession { session } => {
            let id = match resolve_live_session(sessions, &session, *last_attached) {
                Ok(id) => id,
                Err(f) => {
                    let _ = timed_send(&mut framed, f).await;
                    return false;
                }
            };
            let state = sessions.remove(&id).unwrap();
            state.handle.abort();
            info!(id, "session killed");
            let _ = timed_send(&mut framed, Frame::Ok).await;
            false
        }
        Frame::SendFile { session } => {
            let id = match resolve_live_session(sessions, &session, *last_attached) {
                Ok(id) => id,
                Err(f) => {
                    let _ = timed_send(&mut framed, f).await;
                    return false;
                }
            };
            let state = &sessions[&id];
            if timed_send(&mut framed, Frame::Ok).await.is_ok() {
                let stream = framed.into_inner();
                let _ = state.client_tx.send(ClientConn::Send(stream));
            }
            false
        }
        Frame::RenameSession { session, new_name } => {
            let id = match resolve_live_session(sessions, &session, *last_attached) {
                Ok(id) => id,
                Err(f) => {
                    let _ = timed_send(&mut framed, f).await;
                    return false;
                }
            };
            // Empty is rename-specific (distinct EmptyName code); the rest of
            // the rules are shared with NewSession via validate_session_name.
            if new_name.is_empty() {
                let _ = timed_send(
                    &mut framed,
                    Frame::Error {
                        code: ErrorCode::EmptyName,
                        message: "new name must not be empty".to_string(),
                    },
                )
                .await;
                return false;
            }
            if let Err(f) = validate_session_name(&new_name, sessions) {
                let _ = timed_send(&mut framed, f).await;
                return false;
            }
            sessions.get_mut(&id).unwrap().name = Some(new_name.clone());
            info!(id, new_name, "session renamed");
            let _ = timed_send(&mut framed, Frame::Ok).await;
            false
        }
        Frame::KillServer => {
            info!("kill-server received, shutting down");
            shutdown(sessions, ctl_path).await;
            let _ = timed_send(&mut framed, Frame::Ok).await;
            true
        }
        other => {
            error!(?other, "unexpected frame on control socket");
            let _ = timed_send(
                &mut framed,
                Frame::Error {
                    code: ErrorCode::UnexpectedFrame,
                    message: "unexpected frame type".to_string(),
                },
            )
            .await;
            false
        }
    }
}

#[cfg(test)]
mod tests {
    use super::validated_socket_dir_override;

    #[test]
    fn socket_dir_override_accepts_absolute() {
        let p = validated_socket_dir_override("/tmp/gritty-test").unwrap();
        assert!(p.is_absolute());
    }

    #[test]
    fn socket_dir_override_rejects_relative() {
        assert!(validated_socket_dir_override("sockets").is_err());
        assert!(validated_socket_dir_override("./sockets").is_err());
        assert!(validated_socket_dir_override("../sockets").is_err());
    }

    #[test]
    fn socket_dir_override_rejects_empty() {
        assert!(validated_socket_dir_override("").is_err());
    }
}
