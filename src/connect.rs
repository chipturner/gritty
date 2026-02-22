use anyhow::{bail, Context};
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::{Duration, Instant};
use tokio::process::{Child, Command};
use tracing::{debug, info, warn};

// ---------------------------------------------------------------------------
// Destination parsing
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq, Eq)]
struct Destination {
    user: Option<String>,
    host: String,
    port: Option<u16>,
}

impl Destination {
    fn parse(s: &str) -> anyhow::Result<Self> {
        if s.is_empty() {
            bail!("empty destination");
        }

        let (user, remainder) = if let Some(at) = s.find('@') {
            let u = &s[..at];
            if u.is_empty() {
                bail!("empty user in destination: {s}");
            }
            (Some(u.to_string()), &s[at + 1..])
        } else {
            (None, s)
        };

        let (host, port) = if let Some(colon) = remainder.rfind(':') {
            let h = &remainder[..colon];
            let p = remainder[colon + 1..]
                .parse::<u16>()
                .with_context(|| format!("invalid port in destination: {s}"))?;
            (h.to_string(), Some(p))
        } else {
            (remainder.to_string(), None)
        };

        if host.is_empty() {
            bail!("empty host in destination: {s}");
        }

        Ok(Self { user, host, port })
    }

    /// Build the SSH destination string (`user@host` or just `host`).
    fn ssh_dest(&self) -> String {
        match &self.user {
            Some(u) => format!("{u}@{}", self.host),
            None => self.host.clone(),
        }
    }

    /// Common SSH args for port, if set.
    fn port_args(&self) -> Vec<String> {
        match self.port {
            Some(p) => vec!["-p".to_string(), p.to_string()],
            None => vec![],
        }
    }
}

// ---------------------------------------------------------------------------
// SSH helpers
// ---------------------------------------------------------------------------

/// Hardened SSH options embedded in every tunnel.
const SSH_TUNNEL_OPTS: &[&str] = &[
    "-o", "ServerAliveInterval=3",
    "-o", "ServerAliveCountMax=2",
    "-o", "StreamLocalBindUnlink=yes",
    "-o", "ExitOnForwardFailure=yes",
    "-o", "ConnectTimeout=5",
    "-N", "-T",
];

/// Run a command on the remote host via SSH, returning stdout.
async fn remote_exec(
    dest: &Destination,
    remote_cmd: &str,
    extra_ssh_opts: &[String],
) -> anyhow::Result<String> {
    // Prepend common binary paths — SSH non-interactive shells don't source
    // .bashrc/.zshrc, so ~/bin etc. won't be in PATH by default.
    let wrapped_cmd = format!(
        "PATH=\"$HOME/bin:$HOME/.local/bin:$HOME/.cargo/bin:$PATH\"; {remote_cmd}"
    );

    debug!("ssh {}: {remote_cmd}", dest.ssh_dest());

    let mut cmd = Command::new("ssh");
    cmd.args(dest.port_args());
    for opt in extra_ssh_opts {
        cmd.arg("-o").arg(opt);
    }
    cmd.arg("-o").arg("ConnectTimeout=5");
    cmd.arg(dest.ssh_dest());
    cmd.arg(&wrapped_cmd);
    cmd.stdout(Stdio::piped());
    cmd.stderr(Stdio::piped());
    cmd.stdin(Stdio::null());

    let output = cmd.output().await.context("failed to run ssh")?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        let stderr = stderr.trim();
        debug!("ssh failed (status {}): {stderr}", output.status);
        if stderr.contains("command not found") || stderr.contains("No such file") {
            bail!("gritty not found on remote host (is it in PATH?)");
        }
        bail!("ssh command failed: {stderr}");
    }

    let stdout = String::from_utf8_lossy(&output.stdout).trim().to_string();
    debug!("ssh output: {stdout}");
    Ok(stdout)
}

/// Build the SSH tunnel command with hardened options.
fn tunnel_command(
    dest: &Destination,
    local_sock: &Path,
    remote_sock: &str,
    extra_ssh_opts: &[String],
) -> Command {
    let mut cmd = Command::new("ssh");
    cmd.args(dest.port_args());
    cmd.args(SSH_TUNNEL_OPTS);
    for opt in extra_ssh_opts {
        cmd.arg("-o").arg(opt);
    }
    let forward = format!(
        "{}:{}",
        local_sock.display(),
        remote_sock
    );
    cmd.arg("-L").arg(forward);
    cmd.arg(dest.ssh_dest());
    cmd.stdout(Stdio::null());
    cmd.stderr(Stdio::piped());
    cmd.stdin(Stdio::null());
    cmd
}

/// Spawn the SSH tunnel, returning the child process.
async fn spawn_tunnel(
    dest: &Destination,
    local_sock: &Path,
    remote_sock: &str,
    extra_ssh_opts: &[String],
) -> anyhow::Result<Child> {
    debug!(
        "tunnel: {} -> {}:{}",
        local_sock.display(),
        dest.ssh_dest(),
        remote_sock,
    );
    let mut cmd = tunnel_command(dest, local_sock, remote_sock, extra_ssh_opts);
    let child = cmd.spawn().context("failed to spawn ssh tunnel")?;
    debug!("ssh tunnel pid: {:?}", child.id());
    Ok(child)
}

/// Poll until the local socket is connectable (200ms interval, 15s timeout).
async fn wait_for_socket(path: &Path) -> anyhow::Result<()> {
    let deadline = Instant::now() + Duration::from_secs(15);
    loop {
        if std::os::unix::net::UnixStream::connect(path).is_ok() {
            return Ok(());
        }
        if Instant::now() >= deadline {
            bail!("timeout waiting for SSH tunnel socket at {}", path.display());
        }
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
}

/// Background task: monitor SSH child, respawn on transient failure.
async fn tunnel_monitor(
    mut child: Child,
    dest: Destination,
    local_sock: PathBuf,
    remote_sock: String,
    extra_ssh_opts: Vec<String>,
    stop: tokio_util::sync::CancellationToken,
) {
    let mut exit_times: Vec<Instant> = Vec::new();

    loop {
        tokio::select! {
            _ = stop.cancelled() => {
                let _ = child.kill().await;
                return;
            }
            status = child.wait() => {
                let status = match status {
                    Ok(s) => s,
                    Err(e) => {
                        warn!("failed to wait on ssh tunnel: {e}");
                        return;
                    }
                };

                if stop.is_cancelled() {
                    return;
                }

                let code = status.code();
                debug!("ssh tunnel exited: {:?}", code);

                // Non-transient failure: don't retry
                // SSH exit 255 = connection error (transient). Signal-killed = no code.
                // Everything else (auth failure, config error) = bail.
                if let Some(c) = code
                    && c != 255
                {
                    warn!("ssh tunnel exited with code {c} (not retrying)");
                    return;
                }

                // Rate limit: 5 exits in 10s = give up
                let now = Instant::now();
                exit_times.push(now);
                exit_times.retain(|t| now.duration_since(*t) < Duration::from_secs(10));
                if exit_times.len() >= 5 {
                    warn!("ssh tunnel failing too fast (5 exits in 10s), giving up");
                    return;
                }

                tokio::time::sleep(Duration::from_secs(1)).await;

                if stop.is_cancelled() {
                    return;
                }

                match spawn_tunnel(&dest, &local_sock, &remote_sock, &extra_ssh_opts).await {
                    Ok(new_child) => {
                        info!("ssh tunnel respawned");
                        child = new_child;
                    }
                    Err(e) => {
                        warn!("failed to respawn ssh tunnel: {e}");
                        return;
                    }
                }
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Remote daemon management
// ---------------------------------------------------------------------------

const REMOTE_ENSURE_CMD: &str = "\
    SOCK=$(gritty socket-path) && \
    (gritty ls >/dev/null 2>&1 || \
     { gritty daemon && sleep 0.3; }) && \
    echo \"$SOCK\"";

/// Get the remote socket path and optionally auto-start the daemon.
async fn ensure_remote_ready(
    dest: &Destination,
    no_daemon_start: bool,
    extra_ssh_opts: &[String],
) -> anyhow::Result<String> {
    let remote_cmd = if no_daemon_start {
        "gritty socket-path"
    } else {
        REMOTE_ENSURE_CMD
    };
    debug!("ensuring remote daemon (no_daemon_start={no_daemon_start})");

    let sock_path = remote_exec(dest, remote_cmd, extra_ssh_opts).await?;

    if sock_path.is_empty() {
        bail!("remote host returned empty socket path");
    }

    Ok(sock_path)
}

// ---------------------------------------------------------------------------
// Local socket path
// ---------------------------------------------------------------------------

/// Compute a deterministic local socket path based on the destination.
///
/// Using the raw destination string means re-running `gritty connect user@host`
/// produces the same socket path, so sessions that used `--ctl-socket` can
/// auto-reconnect after a tunnel restart.
fn local_socket_path(destination: &str) -> PathBuf {
    crate::daemon::socket_dir().join(format!("connect-{destination}.sock"))
}

// ---------------------------------------------------------------------------
// Cleanup guard
// ---------------------------------------------------------------------------

struct ConnectGuard {
    child: Option<Child>,
    local_sock: PathBuf,
    stop: tokio_util::sync::CancellationToken,
}

impl Drop for ConnectGuard {
    fn drop(&mut self) {
        self.stop.cancel();

        if let Some(ref mut child) = self.child
            && let Some(pid) = child.id()
        {
            unsafe {
                libc::kill(pid as i32, libc::SIGTERM);
            }
        }

        let _ = std::fs::remove_file(&self.local_sock);
    }
}

// ---------------------------------------------------------------------------
// Public API
// ---------------------------------------------------------------------------

pub struct ConnectOpts {
    pub destination: String,
    pub no_daemon_start: bool,
    pub ssh_options: Vec<String>,
    pub name: Option<String>,
}

pub async fn run(opts: ConnectOpts) -> anyhow::Result<i32> {
    let dest = Destination::parse(&opts.destination)?;
    let connection_name = opts.name.unwrap_or_else(|| dest.host.clone());

    // 1. Ensure remote daemon is running and get socket path
    eprintln!("starting remote daemon...");
    let remote_sock = ensure_remote_ready(&dest, opts.no_daemon_start, &opts.ssh_options).await?;
    debug!(remote_sock, "remote socket path");

    // 2. Compute local socket path
    let local_sock = local_socket_path(&connection_name);
    debug!("local socket: {}", local_sock.display());
    if let Some(parent) = local_sock.parent() {
        crate::security::secure_create_dir_all(parent)?;
    }
    // Remove stale socket if it exists
    let _ = std::fs::remove_file(&local_sock);

    // 3. Spawn SSH tunnel
    let child = spawn_tunnel(&dest, &local_sock, &remote_sock, &opts.ssh_options).await?;
    let stop = tokio_util::sync::CancellationToken::new();

    let mut guard = ConnectGuard {
        child: Some(child),
        local_sock: local_sock.clone(),
        stop: stop.clone(),
    };

    // 4. Wait for local socket to become connectable
    wait_for_socket(&local_sock).await?;
    debug!("tunnel socket ready");

    // 5. Hand off the child to the tunnel monitor background task
    let original_child = guard.child.take().unwrap();
    let monitor_handle = tokio::spawn(tunnel_monitor(
        original_child,
        dest,
        local_sock.clone(),
        remote_sock,
        opts.ssh_options,
        stop.clone(),
    ));

    // 6. Print socket path and usage hints
    println!("{}", local_sock.display());
    eprintln!("tunnel ready (name: {connection_name}). to use:");
    eprintln!("  gritty new {connection_name}");
    eprintln!("  gritty attach {connection_name} -t <name>");

    // 7. Wait for signal or monitor death
    let mut sigterm = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())?;
    tokio::select! {
        _ = tokio::signal::ctrl_c() => {}
        _ = sigterm.recv() => {}
        _ = monitor_handle => {
            eprintln!("tunnel lost");
        }
    }

    // 8. Cleanup (guard Drop handles ssh kill + socket removal)
    drop(guard);

    Ok(0)
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_destination_user_host() {
        let d = Destination::parse("user@host").unwrap();
        assert_eq!(d.user.as_deref(), Some("user"));
        assert_eq!(d.host, "host");
        assert_eq!(d.port, None);
    }

    #[test]
    fn parse_destination_host_only() {
        let d = Destination::parse("myhost").unwrap();
        assert_eq!(d.user, None);
        assert_eq!(d.host, "myhost");
        assert_eq!(d.port, None);
    }

    #[test]
    fn parse_destination_host_port() {
        let d = Destination::parse("host:2222").unwrap();
        assert_eq!(d.user, None);
        assert_eq!(d.host, "host");
        assert_eq!(d.port, Some(2222));
    }

    #[test]
    fn parse_destination_user_host_port() {
        let d = Destination::parse("user@host:2222").unwrap();
        assert_eq!(d.user.as_deref(), Some("user"));
        assert_eq!(d.host, "host");
        assert_eq!(d.port, Some(2222));
    }

    #[test]
    fn parse_destination_invalid_empty() {
        assert!(Destination::parse("").is_err());
    }

    #[test]
    fn parse_destination_invalid_at_only() {
        assert!(Destination::parse("@host").is_err());
    }

    #[test]
    fn parse_destination_invalid_colon_only() {
        assert!(Destination::parse(":2222").is_err());
    }

    #[test]
    fn tunnel_command_default_opts() {
        let dest = Destination::parse("user@host").unwrap();
        let cmd = tunnel_command(
            &dest,
            Path::new("/tmp/local.sock"),
            "/run/user/1000/gritty/ctl.sock",
            &[],
        );
        let args: Vec<_> = cmd.as_std().get_args().map(|a| a.to_string_lossy().to_string()).collect();
        assert!(args.contains(&"ServerAliveInterval=3".to_string()));
        assert!(args.contains(&"StreamLocalBindUnlink=yes".to_string()));
        assert!(args.contains(&"ExitOnForwardFailure=yes".to_string()));
        assert!(args.contains(&"ConnectTimeout=5".to_string()));
        assert!(args.contains(&"-N".to_string()));
        assert!(args.contains(&"-T".to_string()));
        assert!(args.contains(&"/tmp/local.sock:/run/user/1000/gritty/ctl.sock".to_string()));
        assert!(args.contains(&"user@host".to_string()));
    }

    #[test]
    fn tunnel_command_extra_opts() {
        let dest = Destination::parse("host:2222").unwrap();
        let cmd = tunnel_command(
            &dest,
            Path::new("/tmp/local.sock"),
            "/tmp/remote.sock",
            &["ProxyJump=bastion".to_string()],
        );
        let args: Vec<_> = cmd.as_std().get_args().map(|a| a.to_string_lossy().to_string()).collect();
        assert!(args.contains(&"ProxyJump=bastion".to_string()));
        assert!(args.contains(&"-p".to_string()));
        assert!(args.contains(&"2222".to_string()));
    }

    #[test]
    fn local_socket_path_format() {
        // With hostname-based naming, connect uses just the host part
        let path = local_socket_path("devbox");
        assert_eq!(path.file_name().unwrap().to_string_lossy(), "connect-devbox.sock");

        let path = local_socket_path("example.com");
        assert_eq!(path.file_name().unwrap().to_string_lossy(), "connect-example.com.sock");

        // Custom name override
        let path = local_socket_path("myproject");
        assert_eq!(path.file_name().unwrap().to_string_lossy(), "connect-myproject.sock");
    }

    #[test]
    fn ssh_dest_with_user() {
        let d = Destination::parse("alice@example.com").unwrap();
        assert_eq!(d.ssh_dest(), "alice@example.com");
    }

    #[test]
    fn ssh_dest_without_user() {
        let d = Destination::parse("example.com").unwrap();
        assert_eq!(d.ssh_dest(), "example.com");
    }

    #[test]
    fn port_args_with_port() {
        let d = Destination::parse("host:9999").unwrap();
        assert_eq!(d.port_args(), vec!["-p", "9999"]);
    }

    #[test]
    fn port_args_without_port() {
        let d = Destination::parse("host").unwrap();
        assert!(d.port_args().is_empty());
    }
}
