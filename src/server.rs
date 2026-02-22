use crate::protocol::{Frame, FrameCodec};
use bytes::Bytes;
use futures_util::{SinkExt, StreamExt};
use nix::pty::openpty;
use std::io;
use std::os::fd::{AsRawFd, OwnedFd};
use std::process::Stdio;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, OnceLock};
use tokio::io::unix::AsyncFd;
use tokio::net::UnixStream;
use tokio::process::Command;
use tokio::sync::mpsc;
use tokio_util::codec::Framed;
use tracing::{debug, info};

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

pub async fn run(
    mut client_rx: mpsc::UnboundedReceiver<Framed<UnixStream, FrameCodec>>,
    metadata_slot: Arc<OnceLock<SessionMetadata>>,
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
    let raw_master = master.as_raw_fd();
    let flags = nix::fcntl::fcntl(raw_master, nix::fcntl::FcntlArg::F_GETFL)?;
    let mut oflags = nix::fcntl::OFlag::from_bits_truncate(flags);
    oflags |= nix::fcntl::OFlag::O_NONBLOCK;
    nix::fcntl::fcntl(raw_master, nix::fcntl::FcntlArg::F_SETFL(oflags))?;

    let async_master = AsyncFd::new(master)?;
    let mut buf = vec![0u8; 4096];

    // Wait for first client before spawning shell (so we can read Env frame)
    let mut framed = match client_rx.recv().await {
        Some(framed) => {
            info!("first client connected via channel");
            framed
        }
        None => {
            info!("client channel closed before first client");
            return Ok(());
        }
    };

    // Read optional Env frame from first client (100ms timeout)
    let env_vars =
        match tokio::time::timeout(std::time::Duration::from_millis(100), framed.next()).await {
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
    for (k, v) in &env_vars {
        cmd.env(k, v);
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
    });

    // First client is already connected — enter relay directly
    metadata_slot.get().unwrap().attached.store(true, Ordering::Relaxed);

    // Outer loop: accept clients via channel. PTY persists across reconnects.
    // First iteration skips client-wait (first client already connected above).
    let mut first_client = true;
    loop {
        if !first_client {
            framed = tokio::select! {
                client = client_rx.recv() => {
                    match client {
                        Some(f) => {
                            info!("client connected via channel");
                            f
                        }
                        None => {
                            info!("client channel closed");
                            break;
                        }
                    }
                }
                status = managed.child.wait() => {
                    let code = status?.code().unwrap_or(1);
                    info!(code, "shell exited while awaiting client");
                    break;
                }
            };

            if let Some(meta) = metadata_slot.get() {
                meta.attached.store(true, Ordering::Relaxed);
            }
        }
        first_client = false;

        // Inner loop: relay between socket and PTY
        let exit = loop {
            tokio::select! {
                frame = framed.next() => {
                    match frame {
                        Some(Ok(Frame::Data(data))) => {
                            debug!(len = data.len(), "socket -> pty");
                            let mut guard = async_master.writable().await?;
                            match guard.try_io(|inner| {
                                nix::unistd::write(inner, &data).map_err(io::Error::from)
                            }) {
                                Ok(Ok(_)) => {}
                                Ok(Err(e)) => return Err(e.into()),
                                Err(_would_block) => continue,
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
                        nix::unistd::read(inner.as_raw_fd(), &mut buf).map_err(io::Error::from)
                    }) {
                        Ok(Ok(0)) => {
                            debug!("pty EOF");
                            break RelayExit::ShellExited(0);
                        }
                        Ok(Ok(n)) => {
                            debug!(len = n, "pty -> socket");
                            framed.send(Frame::Data(Bytes::copy_from_slice(&buf[..n]))).await?;
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

                // Client takeover via channel
                new_client = client_rx.recv() => {
                    if let Some(new_framed) = new_client {
                        info!("new client via channel, detaching old client");
                        let _ = framed.send(Frame::Detached).await;
                        framed = new_framed;
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
                info!("client disconnected, waiting for reconnect");
                continue;
            }
            RelayExit::ShellExited(mut code) => {
                // PTY EOF/EIO may fire before child.wait(), giving code=0.
                // Try to get the real exit code from the child.
                if let Ok(Some(status)) = managed.child.try_wait() {
                    code = status.code().unwrap_or(code);
                }
                let _ = framed.send(Frame::Exit { code }).await;
                info!(code, "session ended");
                break;
            }
        }
    }

    Ok(())
}
