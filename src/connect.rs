use anyhow::{Context, bail};
use std::os::fd::OwnedFd;
use std::os::unix::fs::OpenOptionsExt;
use std::os::unix::io::AsRawFd;
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
    "-o",
    "ServerAliveInterval=3",
    "-o",
    "ServerAliveCountMax=2",
    "-o",
    "StreamLocalBindUnlink=yes",
    "-o",
    "ExitOnForwardFailure=yes",
    "-o",
    "ConnectTimeout=5",
    "-N",
    "-T",
];

/// PATH prefix prepended to remote commands so gritty is discoverable
/// in non-interactive SSH shells.
const REMOTE_PATH_PREFIX: &str = "$HOME/bin:$HOME/.local/bin:$HOME/.cargo/bin:$PATH";

/// Build the SSH command for remote execution (without stdio config).
fn remote_exec_command(dest: &Destination, remote_cmd: &str, extra_ssh_opts: &[String]) -> Command {
    let wrapped_cmd = format!("PATH=\"{REMOTE_PATH_PREFIX}\"; {remote_cmd}");
    let mut cmd = Command::new("ssh");
    cmd.args(dest.port_args());
    for opt in extra_ssh_opts {
        cmd.arg("-o").arg(opt);
    }
    cmd.arg("-o").arg("ConnectTimeout=5");
    cmd.arg(dest.ssh_dest());
    cmd.arg(&wrapped_cmd);
    cmd
}

/// Run a command on the remote host via SSH, returning stdout.
async fn remote_exec(
    dest: &Destination,
    remote_cmd: &str,
    extra_ssh_opts: &[String],
) -> anyhow::Result<String> {
    debug!("ssh {}: {remote_cmd}", dest.ssh_dest());

    let mut cmd = remote_exec_command(dest, remote_cmd, extra_ssh_opts);
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

/// Shell-quote a string if it contains characters that need quoting.
/// Used only for display (--dry-run output), never for command execution.
fn shell_quote(s: &str) -> String {
    if s.is_empty() {
        return "''".to_string();
    }
    if s.bytes().all(|b| b.is_ascii_alphanumeric() || b"-_./=:@$+%,".contains(&b)) {
        return s.to_string();
    }
    format!("'{}'", s.replace('\'', "'\\''"))
}

/// Format a tokio Command as a shell string for display.
fn format_command(cmd: &Command) -> String {
    let std_cmd = cmd.as_std();
    let prog = std_cmd.get_program().to_string_lossy();
    let args: Vec<_> = std_cmd.get_args().map(|a| shell_quote(&a.to_string_lossy())).collect();
    if args.is_empty() { prog.to_string() } else { format!("{prog} {}", args.join(" ")) }
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
) -> anyhow::Result<Child> {
    debug!("tunnel: {} -> {}:{}", local_sock.display(), dest.ssh_dest(), remote_sock,);
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
// Remote server management
// ---------------------------------------------------------------------------

const REMOTE_ENSURE_CMD: &str = "\
    SOCK=$(gritty socket-path) && \
    (gritty ls >/dev/null 2>&1 || \
     { gritty server && sleep 0.3; }) && \
    echo \"$SOCK\"";

/// Get the remote socket path and optionally auto-start the server.
async fn ensure_remote_ready(
    dest: &Destination,
    no_server_start: bool,
    extra_ssh_opts: &[String],
) -> anyhow::Result<String> {
    let remote_cmd = if no_server_start { "gritty socket-path" } else { REMOTE_ENSURE_CMD };
    debug!("ensuring remote server (no_server_start={no_server_start})");

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

// ---------------------------------------------------------------------------
// Lockfile-based liveness
// ---------------------------------------------------------------------------

/// Acquire an exclusive flock on the lockfile. Returns the locked fd on success.
/// The lock is held for the lifetime of the returned `OwnedFd`.
fn acquire_lock(lock_path: &Path) -> anyhow::Result<OwnedFd> {
    use std::fs::OpenOptions;
    let file = OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .mode(0o600)
        .open(lock_path)
        .with_context(|| format!("failed to open lockfile: {}", lock_path.display()))?;
    let fd = OwnedFd::from(file);
    if unsafe { libc::flock(fd.as_raw_fd(), libc::LOCK_EX) } != 0 {
        bail!("failed to acquire lock on {}", lock_path.display());
    }
    Ok(fd)
}

/// Probe whether a lockfile is held by a live process.
/// Returns true if the lock is held (process alive), false if free (process dead).
fn is_lock_held(lock_path: &Path) -> bool {
    use std::fs::OpenOptions;
    let file = match OpenOptions::new().read(true).open(lock_path) {
        Ok(f) => f,
        Err(_) => return false,
    };
    // Non-blocking exclusive lock attempt: if it succeeds, the old process is dead
    if unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } == 0 {
        // We got the lock — old process is gone. Release it immediately (fd drop).
        false
    } else {
        true // Lock held by another process
    }
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
    let _ = std::fs::remove_file(connect_lock_path(name));
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
    _lock_fd: Option<OwnedFd>,
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
        // _lock_fd drops here, releasing the flock
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
}

pub async fn run(opts: ConnectOpts, ready_fd: Option<OwnedFd>) -> anyhow::Result<i32> {
    let dest = Destination::parse(&opts.destination)?;
    let connection_name = opts.name.unwrap_or_else(|| dest.host.clone());
    let local_sock = local_socket_path(&connection_name);

    if opts.dry_run {
        let remote_cmd =
            if opts.no_server_start { "gritty socket-path" } else { REMOTE_ENSURE_CMD };
        let ensure_cmd = remote_exec_command(&dest, remote_cmd, &opts.ssh_options);
        let tunnel_cmd = tunnel_command(&dest, &local_sock, "$REMOTE_SOCK", &opts.ssh_options);

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

    // 2. Check for existing tunnel via lockfile (authoritative)
    match probe_tunnel_status(&connection_name) {
        TunnelStatus::Healthy => {
            println!("{}", local_sock.display());
            let pid_hint = read_pid_hint(&connection_name);
            eprint!("tunnel already running (name: {connection_name})");
            if let Some(pid) = pid_hint {
                eprintln!(" (pid {pid})");
                eprintln!("  to stop: gritty disconnect {connection_name}");
            } else {
                eprintln!();
            }
            eprintln!("  to use:");
            eprintln!("    gritty new {connection_name}");
            eprintln!("    gritty attach {connection_name} -t <name>");
            // Signal readiness to parent even for already-running case
            signal_ready(&ready_fd);
            return Ok(0);
        }
        TunnelStatus::Reconnecting => {
            let pid_hint = read_pid_hint(&connection_name);
            eprint!("tunnel exists but is reconnecting (name: {connection_name})");
            if let Some(pid) = pid_hint {
                eprintln!(" (pid {pid})");
            } else {
                eprintln!();
            }
            eprintln!("  wait for it, or: gritty disconnect {connection_name}");
            // Signal readiness to parent so it doesn't hang
            signal_ready(&ready_fd);
            return Ok(0);
        }
        TunnelStatus::Stale => {
            debug!("cleaning stale tunnel files for {connection_name}");
            cleanup_stale_files(&connection_name);
        }
    }

    // 3. Acquire lockfile (held for entire lifetime of this process)
    let lock_fd = acquire_lock(&lock_path)?;

    // 4. Ensure remote server is running and get socket path
    let remote_sock = ensure_remote_ready(&dest, opts.no_server_start, &opts.ssh_options).await?;
    debug!(remote_sock, "remote socket path");

    // 5. Spawn SSH tunnel
    let child = spawn_tunnel(&dest, &local_sock, &remote_sock, &opts.ssh_options).await?;
    let stop = tokio_util::sync::CancellationToken::new();

    let mut guard = ConnectGuard {
        child: Some(child),
        local_sock: local_sock.clone(),
        pid_file: pid_file.clone(),
        lock_file: lock_path,
        dest_file: dest_file.clone(),
        _lock_fd: Some(lock_fd),
        stop: stop.clone(),
    };

    // 6. Wait for local socket to become connectable
    wait_for_socket(&local_sock).await?;
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

/// Write one readiness byte to the pipe fd (if present).
fn signal_ready(ready_fd: &Option<OwnedFd>) {
    if let Some(fd) = ready_fd {
        let _ = nix::unistd::write(fd, b"\x01");
    }
}

// ---------------------------------------------------------------------------
// Disconnect
// ---------------------------------------------------------------------------

pub async fn disconnect(name: &str) -> anyhow::Result<()> {
    match probe_tunnel_status(name) {
        TunnelStatus::Stale => {
            cleanup_stale_files(name);
            eprintln!("tunnel already stopped: {name}");
            return Ok(());
        }
        TunnelStatus::Healthy | TunnelStatus::Reconnecting => {}
    }

    // Read PID and send SIGTERM (let the process handle graceful shutdown)
    let pid_file = connect_pid_path(name);
    let pid = std::fs::read_to_string(&pid_file)
        .ok()
        .and_then(|s| s.trim().parse::<i32>().ok())
        .ok_or_else(|| anyhow::anyhow!("cannot read PID for tunnel {name}"))?;

    unsafe {
        libc::kill(pid, libc::SIGTERM);
    }

    // Poll lock for up to 2s to confirm exit
    let deadline = Instant::now() + Duration::from_secs(2);
    loop {
        tokio::time::sleep(Duration::from_millis(100)).await;
        if !is_lock_held(&connect_lock_path(name)) {
            cleanup_stale_files(name);
            eprintln!("tunnel stopped: {name}");
            return Ok(());
        }
        if Instant::now() >= deadline {
            break;
        }
    }

    // Still alive after timeout — escalate to SIGKILL + killpg
    unsafe {
        libc::kill(pid, libc::SIGKILL);
        libc::killpg(pid, libc::SIGTERM);
    }
    tokio::time::sleep(Duration::from_millis(100)).await;
    cleanup_stale_files(name);
    eprintln!("tunnel killed: {name}");
    Ok(())
}

// ---------------------------------------------------------------------------
// List tunnels
// ---------------------------------------------------------------------------

pub fn list_tunnels() {
    let names = enumerate_tunnels();
    if names.is_empty() {
        println!("no active tunnels");
        return;
    }

    // Probe each, clean stale ones, collect live entries
    let mut rows: Vec<(String, String, String)> = Vec::new();
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
        rows.push((name.clone(), dest.trim().to_string(), status_str));
    }

    if rows.is_empty() {
        println!("no active tunnels");
        return;
    }

    let w_name = rows.iter().map(|r| r.0.len()).max().unwrap().max(4);
    let w_dest = rows.iter().map(|r| r.1.len()).max().unwrap().max(11);

    println!("{:<w_name$}  {:<w_dest$}  Status", "Name", "Destination");
    for (name, dest, status) in &rows {
        println!("{:<w_name$}  {:<w_dest$}  {status}", name, dest);
    }
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
        let args: Vec<_> =
            cmd.as_std().get_args().map(|a| a.to_string_lossy().to_string()).collect();
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
        assert_eq!(shell_quote("hello"), "hello");
        assert_eq!(shell_quote("-N"), "-N");
        assert_eq!(shell_quote("ServerAliveInterval=3"), "ServerAliveInterval=3");
        assert_eq!(shell_quote("user@host"), "user@host");
        assert_eq!(
            shell_quote("/tmp/local.sock:/tmp/remote.sock"),
            "/tmp/local.sock:/tmp/remote.sock"
        );
        assert_eq!(shell_quote("$REMOTE_SOCK"), "$REMOTE_SOCK");
    }

    #[test]
    fn shell_quote_needs_quoting() {
        assert_eq!(shell_quote("hello world"), "'hello world'");
        assert_eq!(shell_quote(""), "''");
        assert_eq!(shell_quote("it's"), "'it'\\''s'");
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
        let cmd = tunnel_command(&dest, Path::new("/tmp/local.sock"), "$REMOTE_SOCK", &[]);
        let formatted = format_command(&cmd);
        // Uses the same SSH_TUNNEL_OPTS
        assert!(formatted.contains("ServerAliveInterval=3"));
        assert!(formatted.contains("-N"));
        assert!(formatted.contains("-T"));
        // Forward arg references $REMOTE_SOCK unquoted (no spaces, $ is safe)
        assert!(formatted.contains("/tmp/local.sock:$REMOTE_SOCK"));
        assert!(formatted.contains("user@host"));
    }

    #[test]
    fn format_command_remote_exec() {
        let dest = Destination::parse("user@host:2222").unwrap();
        let cmd = remote_exec_command(&dest, "gritty socket-path", &[]);
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
        let cmd = remote_exec_command(&dest, REMOTE_ENSURE_CMD, &["ProxyJump=bastion".to_string()]);
        let formatted = format_command(&cmd);
        assert!(formatted.contains("ProxyJump=bastion"));
        assert!(formatted.contains("gritty socket-path"));
        assert!(formatted.contains("gritty server"));
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
        let _fd = acquire_lock(&lock_path).unwrap();

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

        let result = tokio::time::timeout(
            Duration::from_secs(5),
            wait_for_socket(&sock_path),
        )
        .await;

        assert!(result.is_ok(), "should complete within timeout");
        assert!(result.unwrap().is_ok(), "should successfully connect");
    }

    #[ignore] // Takes 15s (full timeout)
    #[tokio::test]
    async fn wait_for_socket_timeout() {
        let dir = tempfile::tempdir().unwrap();
        let sock_path = dir.path().join("never.sock");
        let result = wait_for_socket(&sock_path).await;
        assert!(result.is_err());
    }
}
