use crate::protocol::{Frame, FrameCodec, SessionEntry};
use crate::server::{self, SessionMetadata};
use futures_util::{SinkExt, StreamExt};
use std::collections::HashMap;
use std::os::fd::OwnedFd;
use std::path::{Path, PathBuf};
use std::sync::atomic::Ordering;
use std::sync::{Arc, OnceLock};
use tokio::net::UnixStream;
use tokio::sync::mpsc;
use tokio::task::JoinHandle;
use tokio_util::codec::Framed;
use tracing::{error, info, warn};

struct SessionState {
    handle: JoinHandle<anyhow::Result<()>>,
    metadata: Arc<OnceLock<SessionMetadata>>,
    client_tx: mpsc::UnboundedSender<Framed<UnixStream, FrameCodec>>,
    name: Option<String>,
}

/// Returns the base directory for gritty sockets.
/// Prefers $XDG_RUNTIME_DIR/gritty, falls back to /tmp/gritty-$UID.
pub fn socket_dir() -> PathBuf {
    if let Ok(xdg) = std::env::var("XDG_RUNTIME_DIR") {
        PathBuf::from(xdg).join("gritty")
    } else {
        let uid = unsafe { libc::getuid() };
        PathBuf::from(format!("/tmp/gritty-{uid}"))
    }
}

/// Returns the daemon socket path.
pub fn control_socket_path() -> PathBuf {
    socket_dir().join("ctl.sock")
}

/// Returns the PID file path (sibling to ctl.sock).
fn pid_file_path(ctl_path: &Path) -> PathBuf {
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

/// Resolve a session identifier (name or id string) to a session id.
fn resolve_session(sessions: &HashMap<u32, SessionState>, target: &str) -> Option<u32> {
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

fn build_session_entries(sessions: &HashMap<u32, SessionState>) -> Vec<SessionEntry> {
    let mut entries: Vec<_> = sessions
        .iter()
        .map(|(&id, state)| {
            if let Some(meta) = state.metadata.get() {
                SessionEntry {
                    id: id.to_string(),
                    name: state.name.clone().unwrap_or_default(),
                    pty_path: meta.pty_path.clone(),
                    shell_pid: meta.shell_pid,
                    created_at: meta.created_at,
                    attached: meta.attached.load(Ordering::Relaxed),
                    last_heartbeat: meta.last_heartbeat.load(Ordering::Relaxed),
                }
            } else {
                SessionEntry {
                    id: id.to_string(),
                    name: state.name.clone().unwrap_or_default(),
                    pty_path: String::new(),
                    shell_pid: 0,
                    created_at: 0,
                    attached: false,
                    last_heartbeat: 0,
                }
            }
        })
        .collect();
    entries.sort_by_key(|e| e.id.parse::<u32>().unwrap_or(u32::MAX));
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

    // Signal readiness to parent (daemonize pipe)
    if let Some(fd) = ready_fd {
        use std::io::Write;
        let mut f = std::fs::File::from(fd);
        let _ = f.write_all(&[1]);
        // f drops here, closing the pipe
    }

    // Write PID file
    let pid_path = pid_file_path(ctl_path);
    std::fs::write(&pid_path, std::process::id().to_string())?;

    let mut sessions: HashMap<u32, SessionState> = HashMap::new();
    let mut next_id: u32 = 0;

    // Signal handlers
    let mut sigterm = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())?;
    let mut sigint = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::interrupt())?;

    loop {
        reap_sessions(&mut sessions);

        let stream = tokio::select! {
            result = listener.accept() => {
                let (stream, _addr) = result?;
                stream
            }
            _ = sigterm.recv() => {
                info!("SIGTERM received, shutting down");
                shutdown(&mut sessions, ctl_path);
                break;
            }
            _ = sigint.recv() => {
                info!("SIGINT received, shutting down");
                shutdown(&mut sessions, ctl_path);
                break;
            }
        };

        if let Err(e) = crate::security::verify_peer_uid(&stream) {
            warn!("{e}");
            continue;
        }
        let mut framed = Framed::new(stream, FrameCodec);

        // Handle one control request per connection
        let Some(Ok(frame)) = framed.next().await else {
            continue;
        };

        match frame {
            Frame::NewSession { name } => {
                // Check for duplicate name
                let name_opt = if name.is_empty() { None } else { Some(name) };
                if let Some(ref n) = name_opt {
                    let dup = sessions.values().any(|s| s.name.as_deref() == Some(n));
                    if dup {
                        let _ = framed
                            .send(Frame::Error {
                                message: format!("session name already exists: {n}"),
                            })
                            .await;
                        continue;
                    }
                }

                let id = next_id;
                next_id += 1;

                let (client_tx, client_rx) = mpsc::unbounded_channel();
                let metadata = Arc::new(OnceLock::new());
                let meta_clone = Arc::clone(&metadata);
                let handle = tokio::spawn(async move { server::run(client_rx, meta_clone).await });

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

                let _ = framed.send(Frame::SessionCreated { id: id.to_string() }).await;

                // Hand off connection to session for auto-attach
                let _ = client_tx.send(framed);
            }
            Frame::Attach { session } => {
                reap_sessions(&mut sessions);
                if let Some(id) = resolve_session(&sessions, &session) {
                    let state = &sessions[&id];
                    if state.client_tx.is_closed() {
                        sessions.remove(&id);
                        let _ = framed
                            .send(Frame::Error { message: format!("no such session: {session}") })
                            .await;
                    } else {
                        let _ = framed.send(Frame::Ok).await;
                        let _ = state.client_tx.send(framed);
                    }
                } else {
                    let _ = framed
                        .send(Frame::Error { message: format!("no such session: {session}") })
                        .await;
                }
            }
            Frame::ListSessions => {
                reap_sessions(&mut sessions);
                let entries = build_session_entries(&sessions);
                let _ = framed.send(Frame::SessionInfo { sessions: entries }).await;
            }
            Frame::KillSession { session } => {
                reap_sessions(&mut sessions);
                if let Some(id) = resolve_session(&sessions, &session) {
                    let state = sessions.remove(&id).unwrap();
                    state.handle.abort();
                    info!(id, "session killed");
                    let _ = framed.send(Frame::Ok).await;
                } else {
                    let _ = framed
                        .send(Frame::Error { message: format!("no such session: {session}") })
                        .await;
                }
            }
            Frame::KillServer => {
                info!("kill-server received, shutting down");
                shutdown(&mut sessions, ctl_path);
                let _ = framed.send(Frame::Ok).await;
                break;
            }
            other => {
                error!(?other, "unexpected frame on control socket");
                let _ = framed
                    .send(Frame::Error { message: "unexpected frame type".to_string() })
                    .await;
            }
        }
    }

    Ok(())
}
