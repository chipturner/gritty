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
use tracing::{error, info, warn};

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

/// Returns the base directory for gritty sockets.
/// Prefers $GRITTY_SOCKET_DIR, then $XDG_RUNTIME_DIR/gritty, falls back to /tmp/gritty-$UID.
pub fn socket_dir() -> PathBuf {
    if let Ok(dir) = std::env::var("GRITTY_SOCKET_DIR") {
        return PathBuf::from(dir);
    }
    if let Some(proj) = directories::ProjectDirs::from("", "", "gritty") {
        if let Some(runtime) = proj.runtime_dir() {
            return runtime.to_path_buf();
        }
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
    sessions.retain(|id, state| {
        if state.handle.is_finished() {
            info!(id, "session ended");
            false
        } else {
            true
        }
    });
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

fn build_session_entries(sessions: &HashMap<u32, SessionState>) -> Vec<SessionEntry> {
    let mut entries: Vec<_> = sessions
        .iter()
        .map(|(&id, state)| {
            if let Some(meta) = state.metadata.get() {
                SessionEntry {
                    id,
                    name: state.name.clone().unwrap_or_default(),
                    pty_path: meta.pty_path.clone(),
                    shell_pid: meta.shell_pid,
                    created_at: meta.created_at,
                    attached: meta.attached.load(Ordering::Relaxed),
                    last_heartbeat: meta.last_heartbeat.load(Ordering::Relaxed),
                    foreground_cmd: foreground_process(meta.shell_pid),
                    cwd: foreground_cwd(meta.shell_pid),
                    client_name: meta.client_name.lock().map(|n| n.clone()).unwrap_or_default(),
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
                }
            }
        })
        .collect();
    entries.sort_by_key(|e| e.id);
    entries
}

fn shutdown(sessions: &mut HashMap<u32, SessionState>, ctl_path: &Path) {
    for (id, state) in sessions.drain() {
        state.handle.abort();
        info!(id, "session aborted");
    }
    let _ = std::fs::remove_file(ctl_path);
    let _ = std::fs::remove_file(pid_file_path(ctl_path));
}

/// Perform Hello/HelloAck handshake and read control frame for a single connection.
/// Spawned as a per-connection task so slow clients don't block the accept loop.
async fn connection_handshake(
    stream: UnixStream,
    tx: mpsc::Sender<(Frame, Framed<UnixStream, FrameCodec>, u32)>,
) {
    let mut framed = Framed::new(stream, FrameCodec);

    // Read Hello handshake (5s timeout)
    let (version, client_caps) =
        match tokio::time::timeout(Duration::from_secs(5), framed.next()).await {
            Ok(Some(Ok(Frame::Hello { version, capabilities }))) => (version, capabilities),
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

    // Reject version mismatch
    if version != PROTOCOL_VERSION {
        let _ = timed_send(
            &mut framed,
            Frame::Error {
                code: ErrorCode::VersionMismatch,
                message: format!(
                    "protocol version mismatch: client={version} server={PROTOCOL_VERSION}; \
                     both sides must run the same version"
                ),
            },
        )
        .await;
        return;
    }

    let server_caps = CAP_CLIPBOARD;
    if timed_send(
        &mut framed,
        Frame::HelloAck { version: PROTOCOL_VERSION, capabilities: server_caps },
    )
    .await
    .is_err()
    {
        return;
    }

    let negotiated = client_caps & server_caps;

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

    let _ = tx.send((frame, framed, negotiated)).await;
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
    info!(path = %ctl_path.display(), "daemon listening");

    // Signal readiness to parent (daemonize pipe): [0x01][pid: u32 LE]
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

    // Write PID file
    let pid_path = pid_file_path(ctl_path);
    std::fs::write(&pid_path, std::process::id().to_string())?;

    let mut sessions: HashMap<u32, SessionState> = HashMap::new();
    let mut next_id: u32 = 0;
    let mut last_attached: Option<u32> = None;
    let session_config = crate::config::ConfigFile::load().resolve_session(None);
    let ring_buffer_cap = session_config.ring_buffer_size as usize;
    let oauth_tunnel_idle_timeout = session_config.oauth_tunnel_idle_timeout;

    // Signal handlers
    let mut sigterm = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())?;
    let mut sigint = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::interrupt())?;

    // Channel for handshake results -- spawned tasks send completed handshakes here
    let (conn_tx, mut conn_rx) = mpsc::channel::<(Frame, Framed<UnixStream, FrameCodec>, u32)>(64);

    loop {
        reap_sessions(&mut sessions);

        let should_break = tokio::select! {
            result = listener.accept() => {
                let (stream, _addr) = result?;
                if let Err(e) = crate::security::verify_peer_uid(&stream) {
                    warn!("{e}");
                } else {
                    let tx = conn_tx.clone();
                    tokio::spawn(connection_handshake(stream, tx));
                }
                false
            }
            Some((frame, framed, capabilities)) = conn_rx.recv() => {
                dispatch_control(
                    frame, framed, &mut sessions, &mut next_id, ctl_path, &mut last_attached,
                    ring_buffer_cap, oauth_tunnel_idle_timeout, capabilities,
                ).await
            }
            _ = sigterm.recv() => {
                info!("SIGTERM received, shutting down");
                shutdown(&mut sessions, ctl_path);
                true
            }
            _ = sigint.recv() => {
                info!("SIGINT received, shutting down");
                shutdown(&mut sessions, ctl_path);
                true
            }
        };

        if should_break {
            break;
        }
    }

    Ok(())
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
) -> bool {
    match frame {
        Frame::NewSession { name, command, cwd, cols, rows, client_name } => {
            // Reject names containing control characters
            let name_opt = if name.is_empty() { None } else { Some(name) };
            let command_opt = if command.is_empty() { None } else { Some(command) };
            let cwd_opt = if cwd.is_empty() { None } else { Some(cwd) };
            if let Some(ref n) = name_opt {
                if n.bytes().any(|b| b.is_ascii_control()) {
                    let _ = timed_send(
                        &mut framed,
                        Frame::Error {
                            code: ErrorCode::InvalidName,
                            message: "session name must not contain control characters".to_string(),
                        },
                    )
                    .await;
                    return false;
                }
                if n.parse::<u32>().is_ok() {
                    let _ = timed_send(
                        &mut framed,
                        Frame::Error {
                            code: ErrorCode::InvalidName,
                            message: "session name must not be purely numeric (ambiguous with session IDs)".to_string(),
                        },
                    )
                    .await;
                    return false;
                }
                let dup = sessions.values().any(|s| s.name.as_deref() == Some(n));
                if dup {
                    let _ = timed_send(
                        &mut framed,
                        Frame::Error {
                            code: ErrorCode::NameAlreadyExists,
                            message: format!("session name already exists: {n}"),
                        },
                    )
                    .await;
                    return false;
                }
            }

            let id = *next_id;
            *next_id += 1;

            let (client_tx, client_rx) = mpsc::unbounded_channel();
            let metadata = Arc::new(OnceLock::new());
            let meta_clone = Arc::clone(&metadata);
            let sock_dir = ctl_path.parent().expect("ctl_path must have a parent");
            let agent_socket_path = sock_dir.join(format!("agent-{id}.sock"));
            let svc_socket_path = sock_dir.join(format!("svc-{id}.sock"));
            let name_for_server = name_opt.clone();
            let cmd_for_server = command_opt;
            let cwd_for_server = cwd_opt;
            let handle = tokio::spawn(async move {
                server::run(
                    client_rx,
                    meta_clone,
                    agent_socket_path,
                    svc_socket_path,
                    id,
                    name_for_server,
                    cmd_for_server,
                    ring_buffer_cap,
                    oauth_tunnel_idle_timeout,
                    cols,
                    rows,
                    cwd_for_server,
                )
                .await
            });

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

            if timed_send(&mut framed, Frame::SessionCreated { id }).await.is_err() {
                return false;
            }

            // Hand off connection to session for auto-attach
            *last_attached = Some(id);
            let _ = client_tx.send(ClientConn::Active { framed, client_name, capabilities });
            false
        }
        Frame::Attach { session, client_name, force } => {
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
                } else {
                    // Check if session is already attached and force not requested
                    let is_attached = state
                        .metadata
                        .get()
                        .map(|m| m.attached.load(Ordering::Relaxed))
                        .unwrap_or(false);
                    if is_attached && !force {
                        let _ = timed_send(
                            &mut framed,
                            Frame::Error {
                                code: ErrorCode::AlreadyAttached,
                                message: format!("session {session} is already attached"),
                            },
                        )
                        .await;
                    } else if timed_send(&mut framed, Frame::Ok).await.is_ok() {
                        *last_attached = Some(id);
                        let _ = state.client_tx.send(ClientConn::Active {
                            framed,
                            client_name,
                            capabilities,
                        });
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
                } else if timed_send(&mut framed, Frame::Ok).await.is_ok() {
                    let _ = state.client_tx.send(ClientConn::Tail(framed));
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
        Frame::ListSessions => {
            reap_sessions(sessions);
            let entries = build_session_entries(sessions);
            let _ = timed_send(&mut framed, Frame::SessionInfo { sessions: entries }).await;
            false
        }
        Frame::KillSession { session } => {
            reap_sessions(sessions);
            if let Some(id) = resolve_session(sessions, &session, *last_attached) {
                let state = sessions.remove(&id).unwrap();
                state.handle.abort();
                info!(id, "session killed");
                let _ = timed_send(&mut framed, Frame::Ok).await;
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
        Frame::SendFile { session } => {
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
                } else if timed_send(&mut framed, Frame::Ok).await.is_ok() {
                    let stream = framed.into_inner();
                    let _ = state.client_tx.send(ClientConn::Send(stream));
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
        Frame::RenameSession { session, new_name } => {
            reap_sessions(sessions);
            if let Some(id) = resolve_session(sessions, &session, *last_attached) {
                if new_name.is_empty() {
                    let _ = timed_send(
                        &mut framed,
                        Frame::Error {
                            code: ErrorCode::EmptyName,
                            message: "new name must not be empty".to_string(),
                        },
                    )
                    .await;
                } else if new_name.bytes().any(|b| b.is_ascii_control()) {
                    let _ = timed_send(
                        &mut framed,
                        Frame::Error {
                            code: ErrorCode::InvalidName,
                            message: "session name must not contain control characters".to_string(),
                        },
                    )
                    .await;
                } else if new_name.parse::<u32>().is_ok() {
                    let _ = timed_send(
                        &mut framed,
                        Frame::Error {
                            code: ErrorCode::InvalidName,
                            message: "session name must not be purely numeric (ambiguous with session IDs)".to_string(),
                        },
                    )
                    .await;
                } else if sessions.values().any(|s| s.name.as_deref() == Some(&new_name)) {
                    let _ = timed_send(
                        &mut framed,
                        Frame::Error {
                            code: ErrorCode::NameAlreadyExists,
                            message: format!("session name already exists: {new_name}"),
                        },
                    )
                    .await;
                } else {
                    sessions.get_mut(&id).unwrap().name = Some(new_name.clone());
                    info!(id, new_name, "session renamed");
                    let _ = timed_send(&mut framed, Frame::Ok).await;
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
        Frame::KillServer => {
            info!("kill-server received, shutting down");
            shutdown(sessions, ctl_path);
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
