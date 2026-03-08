use anyhow::{Context, bail};
use std::os::fd::OwnedFd;
use std::os::unix::fs::OpenOptionsExt;
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

        // Handle bracketed IPv6: [::1] or [::1]:port
        let (host, port) = if remainder.starts_with('[') {
            if let Some(bracket) = remainder.find(']') {
                let h = &remainder[1..bracket];
                let after = &remainder[bracket + 1..];
                let p = if let Some(rest) = after.strip_prefix(':') {
                    Some(
                        rest.parse::<u16>()
                            .with_context(|| format!("invalid port in destination: {s}"))?,
                    )
                } else if after.is_empty() {
                    None
                } else {
                    bail!("unexpected characters after bracketed host: {s}");
                };
                (h.to_string(), p)
            } else {
                bail!("unclosed bracket in destination: {s}");
            }
        } else if remainder.matches(':').count() > 1 {
            // Multiple colons without brackets -- bare IPv6 address (no port)
            (remainder.to_string(), None)
        } else if let Some(colon) = remainder.rfind(':') {
            let h = &remainder[..colon];
            match remainder[colon + 1..].parse::<u16>() {
                Ok(p) => (h.to_string(), Some(p)),
                Err(_) => (remainder.to_string(), None),
            }
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

/// Reject connection names that could cause path traversal or corruption.
fn validate_connection_name(name: &str) -> anyhow::Result<()> {
    if name.is_empty() {
        bail!("connection name must not be empty");
    }
    if name.contains('/') || name.contains('\0') || name.contains("..") {
        bail!("invalid connection name: {name:?}");
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// SSH helpers
// ---------------------------------------------------------------------------

/// Tunnel-specific SSH `-o` options (keepalive, cleanup, failure behavior).
const TUNNEL_SSH_OPTS: &[&str] = &[
    "ServerAliveInterval=3",
    "ServerAliveCountMax=2",
    "StreamLocalBindUnlink=yes",
    "ExitOnForwardFailure=yes",
    // Prevent user config from leaking forwarding or connection sharing
    // into the tunnel (gritty handles agent forwarding separately).
    "ControlPath=none",
    "ForwardAgent=no",
    "ForwardX11=no",
];

/// PATH prefix prepended to remote commands so gritty is discoverable
/// in non-interactive SSH shells.
const REMOTE_PATH_PREFIX: &str = "$HOME/bin:$HOME/.local/bin:$HOME/.cargo/bin:$HOME/.nix-profile/bin:/usr/local/bin:/opt/homebrew/bin:/snap/bin:$PATH";

/// Build the common SSH args that precede the destination in every invocation:
/// port, user-supplied options, ConnectTimeout, and BatchMode (background only).
fn base_ssh_args(dest: &Destination, extra_ssh_opts: &[String], foreground: bool) -> Vec<String> {
    let mut args = Vec::new();
    args.extend(dest.port_args());
    for opt in extra_ssh_opts {
        args.push("-o".into());
        args.push(opt.clone());
    }
    args.push("-o".into());
    args.push("ConnectTimeout=5".into());
    if !foreground {
        args.push("-o".into());
        args.push("BatchMode=yes".into());
    }
    args
}

/// Build the SSH command for remote execution (without stdio config).
fn remote_exec_command(
    dest: &Destination,
    remote_cmd: &str,
    extra_ssh_opts: &[String],
    foreground: bool,
) -> Command {
    let mut preamble = format!("PATH=\"{REMOTE_PATH_PREFIX}\"");
    if let Ok(dir) = std::env::var("GRITTY_SOCKET_DIR") {
        preamble.push_str(&format!("; export GRITTY_SOCKET_DIR=\"{dir}\""));
    }
    let wrapped_cmd = format!("{preamble}; {remote_cmd}");
    let mut cmd = Command::new("ssh");
    cmd.args(base_ssh_args(dest, extra_ssh_opts, foreground));
    cmd.arg(dest.ssh_dest());
    cmd.arg(&wrapped_cmd);
    cmd
}

/// Run a command on the remote host via SSH, returning stdout.
///
/// Stderr is always piped so we can include SSH errors in our error messages.
/// SSH interactive prompts use `/dev/tty` directly, not stderr.
/// In background mode, `BatchMode=yes` is set so SSH fails fast instead of hanging.
async fn remote_exec(
    dest: &Destination,
    remote_cmd: &str,
    extra_ssh_opts: &[String],
    foreground: bool,
) -> anyhow::Result<String> {
    debug!("ssh {}: {remote_cmd}", dest.ssh_dest());

    let mut cmd = remote_exec_command(dest, remote_cmd, extra_ssh_opts, foreground);
    cmd.stdout(Stdio::piped());
    cmd.stderr(Stdio::piped());
    cmd.stdin(Stdio::null());

    let output = cmd.output().await.context("failed to run ssh")?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        let stderr = stderr.trim();
        debug!("ssh failed (status {}): {stderr}", output.status);
        if stderr.contains("command not found") || stderr.contains("No such file") {
            bail!(
                "gritty not found on remote host -- install it there with: cargo install gritty-cli"
            );
        }
        let diag = format_ssh_diag(dest, extra_ssh_opts, foreground);
        if stderr.is_empty() {
            bail!("ssh command failed (exit {})\n  to diagnose: {diag}", output.status);
        }
        bail!("ssh command failed: {stderr}\n  to diagnose: {diag}");
    }

    let stdout = String::from_utf8_lossy(&output.stdout).trim().to_string();
    debug!("ssh output: {stdout}");
    Ok(stdout)
}

/// Format a diagnostic SSH command for display in error messages.
/// Mirrors `base_ssh_args` so the suggestion matches what was actually run.
fn format_ssh_diag(dest: &Destination, extra_ssh_opts: &[String], foreground: bool) -> String {
    let mut parts = vec!["ssh".to_string()];
    for arg in base_ssh_args(dest, extra_ssh_opts, foreground) {
        parts.push(shell_quote(&arg));
    }
    parts.push(dest.ssh_dest());
    parts.join(" ")
}

/// Shell-quote a string if it contains characters that need quoting.
/// Used only for display (--dry-run output), never for command execution.
fn shell_quote(s: &str) -> String {
    shlex::try_quote(s).map(|c| c.into_owned()).unwrap_or_else(|_| s.to_string())
}

/// Format a tokio Command as a shell string for display.
fn format_command(cmd: &Command) -> String {
    let std_cmd = cmd.as_std();
    let prog = std_cmd.get_program().to_string_lossy();
    let args: Vec<_> = std_cmd.get_args().map(|a| shell_quote(&a.to_string_lossy())).collect();
    if args.is_empty() { prog.to_string() } else { format!("{prog} {}", args.join(" ")) }
}

/// Build the SSH tunnel command with hardened options.
///
/// Stderr is always piped so we can capture SSH errors on failure.
/// (SSH interactive prompts use `/dev/tty` directly, not stderr.)
fn tunnel_command(
    dest: &Destination,
    local_sock: &Path,
    remote_sock: &str,
    extra_ssh_opts: &[String],
    foreground: bool,
) -> Command {
    let mut cmd = Command::new("ssh");
    cmd.args(base_ssh_args(dest, extra_ssh_opts, foreground));
    for opt in TUNNEL_SSH_OPTS {
        cmd.arg("-o").arg(opt);
    }
    cmd.args(["-N", "-T"]);
    let forward = format!("{}:{}", local_sock.display(), remote_sock);
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
    foreground: bool,
) -> anyhow::Result<Child> {
    debug!("tunnel: {} -> {}:{}", local_sock.display(), dest.ssh_dest(), remote_sock,);
    let mut cmd = tunnel_command(dest, local_sock, remote_sock, extra_ssh_opts, foreground);
    cmd.kill_on_drop(true);
    let child = cmd.spawn().context("failed to spawn ssh tunnel")?;
    debug!("ssh tunnel pid: {:?}", child.id());
    Ok(child)
}

/// Poll until the local socket is connectable (200ms interval).
async fn wait_for_socket(path: &Path, timeout: Duration) -> anyhow::Result<()> {
    let deadline = Instant::now() + timeout;
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
/// Uses exponential backoff (1s..60s) and never gives up on transient errors.
async fn tunnel_monitor(
    mut child: Child,
    dest: Destination,
    local_sock: PathBuf,
    remote_sock: String,
    extra_ssh_opts: Vec<String>,
    stop: tokio_util::sync::CancellationToken,
) {
    let mut backoff = Duration::from_secs(1);
    const MAX_BACKOFF: Duration = Duration::from_secs(60);
    /// If the tunnel stays alive for this long, reset the backoff.
    const HEALTHY_THRESHOLD: Duration = Duration::from_secs(30);

    loop {
        let spawned_at = Instant::now();

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

                // Reset backoff if the tunnel was alive long enough
                if spawned_at.elapsed() >= HEALTHY_THRESHOLD {
                    backoff = Duration::from_secs(1);
                }

                info!("ssh tunnel died, retrying in {}s", backoff.as_secs());

                tokio::select! {
                    _ = tokio::time::sleep(backoff) => {}
                    _ = stop.cancelled() => return,
                }

                backoff = (backoff * 2).min(MAX_BACKOFF);

                match spawn_tunnel(&dest, &local_sock, &remote_sock, &extra_ssh_opts, false).await {
                    Ok(new_child) => {
                        info!("ssh tunnel respawned");
                        child = new_child;
                    }
                    Err(e) => {
                        warn!("failed to respawn ssh tunnel: {e}");
                        // Even spawn failure is retried -- the tunnel process
                        // might just not be available momentarily.
                        continue;
                    }
                }
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Remote server management
// ---------------------------------------------------------------------------

const REMOTE_ENSURE_CMD: &str = "\
    SOCK=$(gritty socket-path) && \
    (gritty ls local >/dev/null 2>&1 || \
     { gritty server && sleep 0.3; }) && \
    echo \"$SOCK\" && \
    gritty protocol-version 2>/dev/null || true";

/// Get the remote socket path and optionally auto-start the server.
/// Returns (socket_path, remote_protocol_version).
async fn ensure_remote_ready(
    dest: &Destination,
    no_server_start: bool,
    extra_ssh_opts: &[String],
    foreground: bool,
) -> anyhow::Result<(String, Option<u16>)> {
    let remote_cmd = if no_server_start { "gritty socket-path" } else { REMOTE_ENSURE_CMD };
    debug!("ensuring remote server (no_server_start={no_server_start})");

    let output = remote_exec(dest, remote_cmd, extra_ssh_opts, foreground).await?;

    // Output is "socket_path\nversion" (version line may be absent for old remotes)
    let mut lines = output.lines();
    let sock_path = lines.next().unwrap_or("").to_string();
    let remote_version = lines.next().and_then(|s| s.trim().parse::<u16>().ok());

    if sock_path.is_empty() {
        bail!("remote host returned empty socket path");
    }

    Ok((sock_path, remote_version))
}

// ---------------------------------------------------------------------------
// Local socket path
// ---------------------------------------------------------------------------

/// Compute a deterministic local socket path based on the destination.
///
/// Using the raw destination string means re-running `gritty tunnel-create user@host`
/// produces the same socket path, so sessions that used `--ctl-socket` can
/// auto-reconnect after a tunnel restart.
fn local_socket_path(destination: &str) -> PathBuf {
    crate::daemon::socket_dir().join(format!("connect-{destination}.sock"))
}

fn connect_pid_path(connection_name: &str) -> PathBuf {
    crate::daemon::socket_dir().join(format!("connect-{connection_name}.pid"))
}

fn connect_lock_path(connection_name: &str) -> PathBuf {
    crate::daemon::socket_dir().join(format!("connect-{connection_name}.lock"))
}

fn connect_dest_path(connection_name: &str) -> PathBuf {
    crate::daemon::socket_dir().join(format!("connect-{connection_name}.dest"))
}

/// Compute the local socket path for a given connection name.
/// Public so main.rs can compute the path in the parent process after daemonize.
pub fn connection_socket_path(connection_name: &str) -> PathBuf {
    local_socket_path(connection_name)
}

/// Extract the host component from a destination string (`[user@]host[:port]`).
pub fn parse_host(destination: &str) -> anyhow::Result<String> {
    Ok(Destination::parse(destination)?.host)
}

/// Synchronous SSH connectivity check -- call before daemonizing to catch
/// host-key prompts and password prompts while the terminal is still attached.
pub fn preflight_ssh(dest_str: &str, ssh_options: &[String]) -> anyhow::Result<()> {
    let dest = Destination::parse(dest_str)?;
    let mut cmd = std::process::Command::new("ssh");
    cmd.args(dest.port_args());
    for opt in ssh_options {
        cmd.arg("-o");
        cmd.arg(opt);
    }
    cmd.args(["-o", "BatchMode=yes", "-o", "ConnectTimeout=5"]);
    cmd.arg(dest.ssh_dest());
    cmd.arg("true");
    cmd.stdin(std::process::Stdio::null());
    cmd.stdout(std::process::Stdio::null());
    cmd.stderr(std::process::Stdio::null());
    let status = cmd.status().context("failed to run ssh")?;
    if !status.success() {
        bail!(
            "SSH cannot connect non-interactively to {}\n  \
             if SSH needs a password or host key accept, use: gritty tunnel-create --foreground {}",
            dest.ssh_dest(),
            dest_str
        );
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Lockfile-based liveness
// ---------------------------------------------------------------------------

/// Try to acquire an exclusive flock (non-blocking). Returns Err if already held.
fn try_acquire_lock(
    lock_path: &Path,
) -> Result<nix::fcntl::Flock<std::fs::File>, nix::errno::Errno> {
    use std::fs::OpenOptions;
    let file = OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .mode(0o600)
        .open(lock_path)
        .map_err(|_| nix::errno::Errno::EIO)?;
    nix::fcntl::Flock::lock(file, nix::fcntl::FlockArg::LockExclusiveNonblock).map_err(|(_, e)| e)
}

/// Probe whether a lockfile is held by a live process.
/// Returns true if the lock is held (process alive), false if free (process dead).
fn is_lock_held(lock_path: &Path) -> bool {
    use std::fs::OpenOptions;
    let file = match OpenOptions::new().read(true).open(lock_path) {
        Ok(f) => f,
        Err(_) => return false,
    };
    // Non-blocking exclusive lock attempt: if it succeeds, the old process is dead.
    // The lock is released immediately when the Flock drops.
    nix::fcntl::Flock::lock(file, nix::fcntl::FlockArg::LockExclusiveNonblock).is_err()
}

/// Tunnel health status.
#[derive(Debug, PartialEq, Eq)]
pub enum TunnelStatus {
    Healthy,
    Reconnecting,
    Stale,
}

/// Probe a tunnel's status using lockfile + socket connectivity.
fn probe_tunnel_status(name: &str) -> TunnelStatus {
    let lock_path = connect_lock_path(name);
    if is_lock_held(&lock_path) {
        let sock_path = local_socket_path(name);
        if std::os::unix::net::UnixStream::connect(&sock_path).is_ok() {
            TunnelStatus::Healthy
        } else {
            TunnelStatus::Reconnecting
        }
    } else {
        TunnelStatus::Stale
    }
}

/// Clean up files for a stale tunnel (process already dead).
/// No signals sent — the process is confirmed dead (lockfile released).
/// Orphaned SSH children self-terminate via ServerAliveInterval/ServerAliveCountMax.
fn read_pid_hint(name: &str) -> Option<u32> {
    std::fs::read_to_string(connect_pid_path(name)).ok().and_then(|s| s.trim().parse().ok())
}

fn cleanup_stale_files(name: &str) {
    let _ = std::fs::remove_file(local_socket_path(name));
    let _ = std::fs::remove_file(connect_pid_path(name));
    // Lock file is NOT removed here -- we already hold the flock on it.
    // It's cleaned up by ConnectGuard::Drop when the tunnel exits.
    let _ = std::fs::remove_file(connect_dest_path(name));
}

/// Extract tunnel connection names by globbing lock files in the socket dir.
fn enumerate_tunnels() -> Vec<String> {
    let dir = crate::daemon::socket_dir();
    let Ok(entries) = std::fs::read_dir(&dir) else {
        return Vec::new();
    };
    entries
        .filter_map(|e| e.ok())
        .filter_map(|e| {
            let name = e.file_name().to_string_lossy().to_string();
            if name.starts_with("connect-") && name.ends_with(".lock") {
                Some(name["connect-".len()..name.len() - ".lock".len()].to_string())
            } else {
                None
            }
        })
        .collect()
}

// ---------------------------------------------------------------------------
// Cleanup guard
// ---------------------------------------------------------------------------

struct ConnectGuard {
    child: Option<Child>,
    local_sock: PathBuf,
    pid_file: PathBuf,
    lock_file: PathBuf,
    dest_file: PathBuf,
    _lock: Option<nix::fcntl::Flock<std::fs::File>>,
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
        let _ = std::fs::remove_file(&self.pid_file);
        let _ = std::fs::remove_file(&self.lock_file);
        let _ = std::fs::remove_file(&self.dest_file);
        // _lock drops here, releasing the flock
    }
}

// ---------------------------------------------------------------------------
// Public API
// ---------------------------------------------------------------------------

pub struct ConnectOpts {
    pub destination: String,
    pub no_server_start: bool,
    pub ssh_options: Vec<String>,
    pub name: Option<String>,
    pub dry_run: bool,
    pub foreground: bool,
    pub ignore_version_mismatch: bool,
}

pub async fn run(opts: ConnectOpts, ready_fd: Option<OwnedFd>) -> anyhow::Result<i32> {
    unsafe {
        libc::umask(0o077);
    }

    let dest = Destination::parse(&opts.destination)?;
    let connection_name = opts.name.unwrap_or_else(|| dest.host.clone());
    validate_connection_name(&connection_name)?;
    let local_sock = local_socket_path(&connection_name);

    if opts.dry_run {
        let remote_cmd =
            if opts.no_server_start { "gritty socket-path" } else { REMOTE_ENSURE_CMD };
        let ensure_cmd = remote_exec_command(&dest, remote_cmd, &opts.ssh_options, true);
        let tunnel_cmd =
            tunnel_command(&dest, &local_sock, "$REMOTE_SOCK", &opts.ssh_options, true);

        println!(
            "# Get remote socket path{}",
            if opts.no_server_start { "" } else { " and start server if needed" }
        );
        println!("REMOTE_SOCK=$({})", format_command(&ensure_cmd));
        println!();
        println!("# Start SSH tunnel");
        println!("{}", format_command(&tunnel_cmd));
        return Ok(0);
    }

    // 1. Ensure socket directory exists
    let pid_file = connect_pid_path(&connection_name);
    let lock_path = connect_lock_path(&connection_name);
    let dest_file = connect_dest_path(&connection_name);
    debug!("local socket: {}", local_sock.display());
    if let Some(parent) = local_sock.parent() {
        crate::security::secure_create_dir_all(parent)?;
    }

    // 2. Try to acquire lockfile (non-blocking). If someone else holds it,
    //    the tunnel is running or being created -- signal readiness and exit.
    let lock_fd = match try_acquire_lock(&lock_path) {
        Ok(lock) => {
            // We got the lock -- any leftover files are stale (process died).
            debug!("cleaning stale tunnel files for {connection_name}");
            cleanup_stale_files(&connection_name);
            lock
        }
        Err(_) => {
            // Another process holds the lock -- tunnel is alive or starting.
            let sock_exists = std::os::unix::net::UnixStream::connect(&local_sock).is_ok();
            let pid_hint = read_pid_hint(&connection_name);
            if sock_exists {
                println!("{}", local_sock.display());
                match pid_hint {
                    Some(pid) => eprintln!(
                        "\x1b[32m\u{25b8} tunnel {connection_name} already running (pid {pid})\x1b[0m"
                    ),
                    None => eprintln!(
                        "\x1b[32m\u{25b8} tunnel {connection_name} already running\x1b[0m"
                    ),
                }
            } else {
                match pid_hint {
                    Some(pid) => eprintln!(
                        "\x1b[2;33m\u{25b8} tunnel {connection_name} starting (pid {pid})\x1b[0m"
                    ),
                    None => {
                        eprintln!("\x1b[2;33m\u{25b8} tunnel {connection_name} starting\x1b[0m")
                    }
                }
            }
            signal_ready(&ready_fd);
            return Ok(0);
        }
    };

    // 4. Ensure remote server is running and get socket path
    let (remote_sock, remote_version) =
        ensure_remote_ready(&dest, opts.no_server_start, &opts.ssh_options, opts.foreground)
            .await?;
    debug!(remote_sock, ?remote_version, "remote socket path");

    // Check protocol version compatibility
    if let Some(rv) = remote_version {
        if rv != crate::protocol::PROTOCOL_VERSION {
            let msg = format!(
                "remote protocol version ({rv}) differs from local ({}); \
                 use --ignore-version-mismatch to connect anyway",
                crate::protocol::PROTOCOL_VERSION
            );
            if opts.ignore_version_mismatch {
                warn!("{msg}");
            } else {
                bail!("{msg}");
            }
        }
    }

    // 5. Spawn SSH tunnel
    let child =
        spawn_tunnel(&dest, &local_sock, &remote_sock, &opts.ssh_options, opts.foreground).await?;
    let stop = tokio_util::sync::CancellationToken::new();

    let mut guard = ConnectGuard {
        child: Some(child),
        local_sock: local_sock.clone(),
        pid_file: pid_file.clone(),
        lock_file: lock_path,
        dest_file: dest_file.clone(),
        _lock: Some(lock_fd),
        stop: stop.clone(),
    };

    // 6. Wait for local socket to become connectable (race against child exit)
    let mut child = guard.child.take().unwrap();
    tokio::select! {
        result = wait_for_socket(&local_sock, Duration::from_secs(15)) => {
            result?;
            guard.child = Some(child);
        }
        status = child.wait() => {
            let status = status.context("failed to wait on ssh tunnel")?;
            let diag = format_ssh_diag(&dest, &opts.ssh_options, opts.foreground);
            let msg = if let Some(mut stderr) = child.stderr.take() {
                use tokio::io::AsyncReadExt;
                let mut buf = String::new();
                let _ = stderr.read_to_string(&mut buf).await;
                let buf = buf.trim().to_string();
                if buf.is_empty() { None } else { Some(buf) }
            } else {
                None
            };
            let fg_hint = if opts.foreground {
                String::new()
            } else {
                format!("\n  if SSH needs a password or host key accept, use: gritty tunnel-create --foreground {}", opts.destination)
            };
            match msg {
                Some(err) => bail!("ssh tunnel failed: {err}\n  to diagnose: {diag}{fg_hint}"),
                None => bail!("ssh tunnel exited ({status})\n  to diagnose: {diag}{fg_hint}"),
            }
        }
    }
    debug!("tunnel socket ready");

    // Write PID + dest files
    let _ = std::fs::write(&pid_file, std::process::id().to_string());
    let _ = std::fs::write(&dest_file, &opts.destination);

    // 7. Signal readiness to parent (or print if foreground)
    signal_ready(&ready_fd);

    // 8. Hand off the child to the tunnel monitor background task
    let original_child = guard.child.take().unwrap();
    let monitor_handle = tokio::spawn(tunnel_monitor(
        original_child,
        dest,
        local_sock.clone(),
        remote_sock,
        opts.ssh_options,
        stop.clone(),
    ));

    // 9. Wait for signal or monitor death
    let mut sigterm = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())?;
    tokio::select! {
        _ = sigterm.recv() => {}
        _ = monitor_handle => {}
    }

    // 10. Cleanup (guard Drop handles ssh kill + file removal + lock release)
    drop(guard);

    Ok(0)
}

/// Write readiness signal to the pipe fd: [0x01][pid: u32 LE].
fn signal_ready(ready_fd: &Option<OwnedFd>) {
    if let Some(fd) = ready_fd {
        let pid = std::process::id();
        let mut buf = [0u8; 5];
        buf[0] = 0x01;
        buf[1..5].copy_from_slice(&pid.to_le_bytes());
        let _ = nix::unistd::write(fd, &buf);
    }
}

// ---------------------------------------------------------------------------
// Disconnect
// ---------------------------------------------------------------------------

pub async fn disconnect(name: &str) -> anyhow::Result<()> {
    validate_connection_name(name)?;
    match probe_tunnel_status(name) {
        TunnelStatus::Stale => {
            cleanup_stale_files(name);
            eprintln!("\x1b[2;33m\u{25b8} tunnel {name} already stopped\x1b[0m");
            return Ok(());
        }
        TunnelStatus::Healthy | TunnelStatus::Reconnecting => {}
    }

    // Read PID and send SIGTERM (let the process handle graceful shutdown)
    let pid_file = connect_pid_path(name);
    let pid: i32 = std::fs::read_to_string(&pid_file)
        .ok()
        .and_then(|s| s.trim().parse::<u32>().ok())
        .map(|p| p as i32)
        .ok_or_else(|| anyhow::anyhow!("cannot read PID for tunnel {name}"))?;

    let lock_path = connect_lock_path(name);
    if !is_lock_held(&lock_path) {
        cleanup_stale_files(name);
        eprintln!("\x1b[2;33m\u{25b8} tunnel {name} already stopped\x1b[0m");
        return Ok(());
    }
    unsafe {
        libc::kill(pid, libc::SIGTERM);
    }

    // Poll lock for up to 2s to confirm exit
    let deadline = Instant::now() + Duration::from_secs(2);
    loop {
        tokio::time::sleep(Duration::from_millis(100)).await;
        if !is_lock_held(&lock_path) {
            cleanup_stale_files(name);
            eprintln!("\x1b[32m\u{25b8} tunnel {name} stopped\x1b[0m");
            return Ok(());
        }
        if Instant::now() >= deadline {
            break;
        }
    }

    // Still alive after timeout — escalate to SIGKILL + killpg
    if is_lock_held(&lock_path) {
        unsafe {
            libc::kill(pid, libc::SIGKILL);
            libc::killpg(pid, libc::SIGTERM);
        }
    }
    tokio::time::sleep(Duration::from_millis(100)).await;
    cleanup_stale_files(name);
    eprintln!("\x1b[32m\u{25b8} tunnel {name} killed\x1b[0m");
    Ok(())
}

// ---------------------------------------------------------------------------
// List tunnels
// ---------------------------------------------------------------------------

pub struct TunnelInfo {
    pub name: String,
    pub destination: String,
    pub status: String,
    pub pid: Option<u32>,
    pub log_path: PathBuf,
}

/// Gather info for all live tunnels (cleans stale ones as a side effect).
pub fn get_tunnel_info() -> Vec<TunnelInfo> {
    let names = enumerate_tunnels();
    let mut infos = Vec::new();
    for name in &names {
        let status = probe_tunnel_status(name);
        if status == TunnelStatus::Stale {
            debug!("cleaning stale tunnel: {name}");
            cleanup_stale_files(name);
            continue;
        }
        let dest =
            std::fs::read_to_string(connect_dest_path(name)).unwrap_or_else(|_| "-".to_string());
        let status_str = match status {
            TunnelStatus::Healthy => "healthy".to_string(),
            TunnelStatus::Reconnecting => "reconnecting".to_string(),
            TunnelStatus::Stale => unreachable!(),
        };
        infos.push(TunnelInfo {
            name: name.clone(),
            destination: dest.trim().to_string(),
            status: status_str,
            pid: read_pid_hint(name),
            log_path: crate::daemon::socket_dir().join(format!("connect-{name}.log")),
        });
    }
    infos
}

pub fn list_tunnels() {
    let infos = get_tunnel_info();
    if infos.is_empty() {
        println!("no active tunnels");
        return;
    }

    let rows: Vec<Vec<String>> = infos
        .iter()
        .map(|i| vec![i.name.clone(), i.destination.clone(), i.status.clone()])
        .collect();
    crate::table::print_table(&["Name", "Destination", "Status"], &rows);
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
    fn parse_destination_ipv6_bracketed() {
        let d = Destination::parse("[::1]").unwrap();
        assert_eq!(d.user, None);
        assert_eq!(d.host, "::1");
        assert_eq!(d.port, None);
    }

    #[test]
    fn parse_destination_ipv6_bracketed_port() {
        let d = Destination::parse("[::1]:2222").unwrap();
        assert_eq!(d.user, None);
        assert_eq!(d.host, "::1");
        assert_eq!(d.port, Some(2222));
    }

    #[test]
    fn parse_destination_ipv6_user_bracketed_port() {
        let d = Destination::parse("user@[fe80::1]:22").unwrap();
        assert_eq!(d.user.as_deref(), Some("user"));
        assert_eq!(d.host, "fe80::1");
        assert_eq!(d.port, Some(22));
    }

    #[test]
    fn parse_destination_bare_ipv6() {
        let d = Destination::parse("::1").unwrap();
        assert_eq!(d.user, None);
        assert_eq!(d.host, "::1");
        assert_eq!(d.port, None);
    }

    #[test]
    fn parse_destination_bare_ipv6_with_scope() {
        let d = Destination::parse("fe80::1").unwrap();
        assert_eq!(d.user, None);
        assert_eq!(d.host, "fe80::1");
        assert_eq!(d.port, None);
    }

    #[test]
    fn parse_destination_ipv6_unclosed_bracket() {
        assert!(Destination::parse("[::1").is_err());
    }

    #[test]
    fn tunnel_command_default_opts() {
        let dest = Destination::parse("user@host").unwrap();
        let cmd = tunnel_command(
            &dest,
            Path::new("/tmp/local.sock"),
            "/run/user/1000/gritty/ctl.sock",
            &[],
            false,
        );
        let args: Vec<_> =
            cmd.as_std().get_args().map(|a| a.to_string_lossy().to_string()).collect();
        // From base_ssh_args
        assert!(args.contains(&"ConnectTimeout=5".to_string()));
        assert!(args.contains(&"BatchMode=yes".to_string()));
        // From TUNNEL_SSH_OPTS
        assert!(args.contains(&"ServerAliveInterval=3".to_string()));
        assert!(args.contains(&"StreamLocalBindUnlink=yes".to_string()));
        assert!(args.contains(&"ExitOnForwardFailure=yes".to_string()));
        assert!(args.contains(&"ControlPath=none".to_string()));
        assert!(args.contains(&"ForwardAgent=no".to_string()));
        assert!(args.contains(&"ForwardX11=no".to_string()));
        // Tunnel flags and forward
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
            false,
        );
        let args: Vec<_> =
            cmd.as_std().get_args().map(|a| a.to_string_lossy().to_string()).collect();
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
    fn connect_pid_path_format() {
        let path = connect_pid_path("devbox");
        assert_eq!(path.file_name().unwrap().to_string_lossy(), "connect-devbox.pid");

        let path = connect_pid_path("example.com");
        assert_eq!(path.file_name().unwrap().to_string_lossy(), "connect-example.com.pid");
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

    #[test]
    fn shell_quote_simple() {
        // Safe strings pass through unquoted
        assert_eq!(shell_quote("hello"), "hello");
        assert_eq!(shell_quote("-N"), "-N");
        // shlex quotes some chars the old impl didn't (=, @, :, $), which is correct
        let q = shell_quote("ServerAliveInterval=3");
        assert!(q.contains("ServerAliveInterval") && q.contains("3"));
        let q = shell_quote("user@host");
        assert!(q.contains("user") && q.contains("host"));
    }

    #[test]
    fn shell_quote_needs_quoting() {
        let q = shell_quote("hello world");
        assert!(q.starts_with('\'') || q.starts_with('"'));
        assert!(q.contains("hello world"));
        assert_eq!(shell_quote(""), "''");
        let q = shell_quote("it's");
        assert!(q.contains("it") && q.contains("s"));
    }

    #[test]
    fn shell_quote_remote_cmd() {
        // The wrapped remote command contains spaces, quotes, semicolons —
        // must be single-quoted so $HOME expands on the remote side.
        let cmd = format!("PATH=\"{REMOTE_PATH_PREFIX}\"; gritty socket-path");
        let quoted = shell_quote(&cmd);
        assert!(quoted.starts_with('\''));
        assert!(quoted.ends_with('\''));
    }

    #[test]
    fn format_command_tunnel() {
        let dest = Destination::parse("user@host").unwrap();
        let cmd = tunnel_command(&dest, Path::new("/tmp/local.sock"), "$REMOTE_SOCK", &[], true);
        let formatted = format_command(&cmd);
        assert!(formatted.contains("ServerAliveInterval=3"));
        assert!(formatted.contains("ControlPath=none"));
        assert!(formatted.contains("ForwardAgent=no"));
        assert!(formatted.contains("-N"));
        assert!(formatted.contains("-T"));
        // Forward arg references $REMOTE_SOCK unquoted (no spaces, $ is safe)
        assert!(formatted.contains("/tmp/local.sock:$REMOTE_SOCK"));
        assert!(formatted.contains("user@host"));
    }

    #[test]
    fn format_command_remote_exec() {
        let dest = Destination::parse("user@host:2222").unwrap();
        let cmd = remote_exec_command(&dest, "gritty socket-path", &[], true);
        let formatted = format_command(&cmd);
        assert!(formatted.starts_with("ssh "));
        assert!(formatted.contains("-p 2222"));
        assert!(formatted.contains("ConnectTimeout=5"));
        assert!(formatted.contains("user@host"));
        // The wrapped command should be single-quoted (contains spaces)
        assert!(formatted.contains(&format!("PATH=\"{REMOTE_PATH_PREFIX}\"")));
    }

    #[test]
    fn format_command_remote_exec_with_extra_opts() {
        let dest = Destination::parse("user@host").unwrap();
        let cmd =
            remote_exec_command(&dest, REMOTE_ENSURE_CMD, &["ProxyJump=bastion".to_string()], true);
        let formatted = format_command(&cmd);
        assert!(formatted.contains("ProxyJump=bastion"));
        assert!(formatted.contains("gritty socket-path"));
        assert!(formatted.contains("gritty server"));
    }

    #[test]
    fn base_ssh_args_foreground() {
        let dest = Destination::parse("user@host:2222").unwrap();
        let args = base_ssh_args(&dest, &["ProxyJump=bastion".into()], true);
        assert!(args.contains(&"-p".to_string()));
        assert!(args.contains(&"2222".to_string()));
        assert!(args.contains(&"ProxyJump=bastion".to_string()));
        assert!(args.contains(&"ConnectTimeout=5".to_string()));
        assert!(!args.contains(&"BatchMode=yes".to_string()));
    }

    #[test]
    fn base_ssh_args_background() {
        let dest = Destination::parse("host").unwrap();
        let args = base_ssh_args(&dest, &[], false);
        assert!(args.contains(&"ConnectTimeout=5".to_string()));
        assert!(args.contains(&"BatchMode=yes".to_string()));
        assert!(!args.contains(&"-p".to_string()));
    }

    // -----------------------------------------------------------------------
    // Lockfile and tunnel lifecycle tests
    // -----------------------------------------------------------------------

    #[test]
    fn connect_lock_path_format() {
        let path = connect_lock_path("devbox");
        assert_eq!(path.file_name().unwrap().to_string_lossy(), "connect-devbox.lock");
    }

    #[test]
    fn connect_dest_path_format() {
        let path = connect_dest_path("devbox");
        assert_eq!(path.file_name().unwrap().to_string_lossy(), "connect-devbox.dest");
    }

    #[test]
    fn acquire_and_probe_lock() {
        let dir = tempfile::tempdir().unwrap();
        let lock_path = dir.path().join("test.lock");

        // Lock not held initially (file doesn't exist)
        assert!(!is_lock_held(&lock_path));

        // Acquire the lock
        let _fd = try_acquire_lock(&lock_path).unwrap();

        // Now it should be held
        assert!(is_lock_held(&lock_path));

        // Drop the lock
        drop(_fd);

        // Should be free again
        assert!(!is_lock_held(&lock_path));
    }

    #[test]
    fn probe_stale_no_files() {
        // No files at all → stale
        let status = probe_tunnel_status("nonexistent-test-tunnel-xyz");
        assert_eq!(status, TunnelStatus::Stale);
    }

    #[test]
    fn cleanup_stale_files_removes_all() {
        let _dir = tempfile::tempdir().unwrap();
        // We can't easily override socket_dir(), so test that cleanup_stale_files
        // at least doesn't panic on nonexistent files
        cleanup_stale_files("nonexistent-cleanup-test-xyz");
        // No panic = success
    }

    #[test]
    fn enumerate_tunnels_empty_dir() {
        // If socket dir doesn't have any lock files, should return empty
        // This tests the function doesn't crash on various filesystem states
        let names = enumerate_tunnels();
        // We can't control what's in socket_dir during tests, but at minimum
        // the function should not panic
        let _ = names;
    }

    #[test]
    fn connection_socket_path_matches_local() {
        let public_path = connection_socket_path("myhost");
        let internal_path = local_socket_path("myhost");
        assert_eq!(public_path, internal_path);
    }

    // -----------------------------------------------------------------------
    // tunnel_monitor tests
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn tunnel_monitor_non_transient_exit() {
        let child = Command::new("sh").arg("-c").arg("exit 1").spawn().unwrap();
        let dest = Destination::parse("fake-host-test").unwrap();
        let stop = tokio_util::sync::CancellationToken::new();

        let result = tokio::time::timeout(
            Duration::from_secs(5),
            tunnel_monitor(
                child,
                dest,
                PathBuf::from("/tmp/nonexistent.sock"),
                "/tmp/remote.sock".into(),
                vec![],
                stop,
            ),
        )
        .await;

        assert!(result.is_ok(), "monitor should return quickly on non-transient exit");
    }

    #[tokio::test]
    async fn tunnel_monitor_cancellation() {
        let child = Command::new("sleep").arg("60").spawn().unwrap();
        let dest = Destination::parse("fake-host-test").unwrap();
        let stop = tokio_util::sync::CancellationToken::new();
        let stop_clone = stop.clone();

        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(100)).await;
            stop_clone.cancel();
        });

        let result = tokio::time::timeout(
            Duration::from_secs(5),
            tunnel_monitor(
                child,
                dest,
                PathBuf::from("/tmp/nonexistent.sock"),
                "/tmp/remote.sock".into(),
                vec![],
                stop,
            ),
        )
        .await;

        assert!(result.is_ok(), "monitor should return after cancellation");
    }

    #[tokio::test]
    async fn tunnel_monitor_transient_exit_checks_cancellation() {
        // Child exits with 255 (transient). Monitor sleeps 1s then checks cancellation.
        let child = Command::new("sh").arg("-c").arg("exit 255").spawn().unwrap();
        let dest = Destination::parse("fake-host-test").unwrap();
        let stop = tokio_util::sync::CancellationToken::new();
        let stop_clone = stop.clone();

        // Cancel during the 1s sleep between exit and respawn attempt
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(500)).await;
            stop_clone.cancel();
        });

        let result = tokio::time::timeout(
            Duration::from_secs(5),
            tunnel_monitor(
                child,
                dest,
                PathBuf::from("/tmp/nonexistent.sock"),
                "/tmp/remote.sock".into(),
                vec![],
                stop,
            ),
        )
        .await;

        assert!(result.is_ok(), "monitor should return after cancellation during sleep");
    }

    // -----------------------------------------------------------------------
    // wait_for_socket tests
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn wait_for_socket_succeeds_after_delay() {
        let dir = tempfile::tempdir().unwrap();
        let sock_path = dir.path().join("delayed.sock");
        let sock_path_clone = sock_path.clone();

        // Bind the socket after 500ms
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(500)).await;
            let _listener = tokio::net::UnixListener::bind(&sock_path_clone).unwrap();
            // Keep listener alive
            tokio::time::sleep(Duration::from_secs(30)).await;
        });

        let result = wait_for_socket(&sock_path, Duration::from_secs(5)).await;
        assert!(result.is_ok(), "should successfully connect");
    }

    #[tokio::test]
    async fn wait_for_socket_timeout() {
        let dir = tempfile::tempdir().unwrap();
        let sock_path = dir.path().join("never.sock");
        let result = wait_for_socket(&sock_path, Duration::from_secs(1)).await;
        assert!(result.is_err());
    }
}
