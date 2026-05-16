use anyhow::{Context, bail};
use std::os::fd::OwnedFd;
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::{Duration, Instant};
use tokio::process::{Child, Command};
use tracing::{Instrument, debug, info, warn};

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
    // Prevent user config from leaking forwarding into the tunnel
    // (gritty handles agent forwarding separately).
    "ForwardAgent=no",
    "ForwardX11=no",
];

/// PATH prefix prepended to remote commands so gritty is discoverable
/// in non-interactive SSH shells.
const REMOTE_PATH_PREFIX: &str = "$HOME/bin:$HOME/.local/bin:$HOME/.cargo/bin:$HOME/.nix-profile/bin:/usr/local/bin:/opt/homebrew/bin:/snap/bin:$PATH";

/// Build the common SSH args that precede the destination in every invocation:
/// port, user-supplied options, ConnectTimeout, and BatchMode (background only).
/// `connect_timeout == 0` means "don't pass ConnectTimeout" (defer to ssh_config).
fn base_ssh_args(
    dest: &Destination,
    extra_ssh_opts: &[String],
    foreground: bool,
    connect_timeout: u64,
) -> Vec<String> {
    let mut args = Vec::new();
    args.extend(dest.port_args());
    for opt in extra_ssh_opts {
        args.push("-o".into());
        args.push(opt.clone());
    }
    if connect_timeout > 0 {
        args.push("-o".into());
        args.push(format!("ConnectTimeout={connect_timeout}"));
    }
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
    connect_timeout: u64,
) -> Command {
    let mut preamble = if let Ok(dir) = std::env::var("GRITTY_BIN_DIR") {
        format!("PATH=\"{dir}:{REMOTE_PATH_PREFIX}\"")
    } else {
        format!("PATH=\"{REMOTE_PATH_PREFIX}\"")
    };
    if let Ok(dir) = std::env::var("GRITTY_SOCKET_DIR") {
        preamble.push_str(&format!("; export GRITTY_SOCKET_DIR=\"{dir}\""));
    }
    let wrapped_cmd = format!("{preamble}; {remote_cmd}");
    let mut cmd = Command::new("ssh");
    cmd.args(base_ssh_args(dest, extra_ssh_opts, foreground, connect_timeout));
    // Bound post-connect hangs for remote_exec: the ConnectTimeout in
    // base_ssh_args only covers TCP/auth handshake. Without ServerAlive*
    // a remote that wedges after login blocks indefinitely.
    for opt in ["ServerAliveInterval=3", "ServerAliveCountMax=2"] {
        cmd.arg("-o");
        cmd.arg(opt);
    }
    cmd.arg(dest.ssh_dest());
    cmd.arg(&wrapped_cmd);
    // Kill the spawned ssh if this future is dropped (timeout / cancellation).
    cmd.kill_on_drop(true);
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
    connect_timeout: u64,
) -> anyhow::Result<String> {
    debug!("ssh {}: {remote_cmd}", dest.ssh_dest());

    let mut cmd =
        remote_exec_command(dest, remote_cmd, extra_ssh_opts, foreground, connect_timeout);
    cmd.stdout(Stdio::piped());
    cmd.stderr(Stdio::piped());
    cmd.stdin(Stdio::null());

    // Wall-clock ceiling on the entire ssh invocation. ServerAlive* bounds
    // post-connect TCP hangs, but we still want an upper bound on e.g. a
    // stuck shell profile or a fuse-stalled remote filesystem.
    let output = tokio::time::timeout(Duration::from_secs(60), cmd.output())
        .await
        .map_err(|_| {
            anyhow::anyhow!(
                "ssh command timed out after 60s\n  to diagnose: {}",
                format_ssh_diag(dest, extra_ssh_opts, foreground, connect_timeout)
            )
        })?
        .context("failed to run ssh")?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        let stderr = stderr.trim();
        debug!("ssh failed (status {}): {stderr}", output.status);
        if stderr.contains("command not found") || stderr.contains("No such file") {
            bail!(
                "gritty not found on remote host\n  \
                 quick install:  gritty bootstrap {}\n  \
                 manual install: ssh {} 'cargo install gritty-cli'",
                dest.ssh_dest(),
                dest.ssh_dest(),
            );
        }
        let diag = format_ssh_diag(dest, extra_ssh_opts, foreground, connect_timeout);
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
fn format_ssh_diag(
    dest: &Destination,
    extra_ssh_opts: &[String],
    foreground: bool,
    connect_timeout: u64,
) -> String {
    let mut parts = vec!["ssh".to_string()];
    for arg in base_ssh_args(dest, extra_ssh_opts, foreground, connect_timeout) {
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
    isolate_control_path: bool,
    connect_timeout: u64,
) -> Command {
    let mut cmd = Command::new("ssh");
    cmd.args(base_ssh_args(dest, extra_ssh_opts, foreground, connect_timeout));
    for opt in TUNNEL_SSH_OPTS {
        cmd.arg("-o").arg(opt);
    }
    if isolate_control_path {
        cmd.arg("-o").arg("ControlPath=none");
    }
    cmd.arg("-T");
    let forward = format!("{}:{}", local_sock.display(), remote_sock);
    cmd.arg("-L").arg(forward);
    if isolate_control_path {
        // Standalone ssh: -N blocks until the connection closes and spawns no
        // remote process -- so a half-open TCP (ServerAliveInterval kills the
        // local side; remote sshd waits ~2h for TCP keepalive) leaks nothing
        // user-visible on the remote.
        cmd.arg("-N");
    }
    cmd.arg(dest.ssh_dest());
    if !isolate_control_path {
        // Mux client: -N exits 0 immediately after the master accepts the
        // forward, so the supervisor can't track the forward's lifetime. A
        // blocking remote command keeps a session channel open so the child's
        // lifetime tracks the forward. This can leak `sleep` processes on the
        // remote across half-open drops -- documented opt-in footgun.
        cmd.arg("exec sleep 2147483647");
    }
    cmd.stdout(Stdio::null());
    cmd.stderr(Stdio::piped());
    cmd.stdin(Stdio::null());
    cmd
}

/// Spawn the SSH tunnel, returning the child process.
///
/// Stderr is drained here (not in the monitor) so that forward-setup errors
/// like `mux_client_forward: forwarding request failed` surface in `.out`
/// while `wait_for_socket` is still polling -- otherwise they sit unread in
/// the pipe and are dropped when the wait times out.
async fn spawn_tunnel(
    dest: &Destination,
    local_sock: &Path,
    remote_sock: &str,
    extra_ssh_opts: &[String],
    foreground: bool,
    isolate_control_path: bool,
    connect_timeout: u64,
) -> anyhow::Result<Child> {
    // Clear any stale socket so SSH's bind() doesn't hit EADDRINUSE. With the
    // default isolate_control_path=true this is sufficient; when a user opts
    // back into ControlMaster mux, a master that still holds this forward will
    // keep its listener on the now-deleted inode -- that's the opt-in footgun.
    let _ = std::fs::remove_file(local_sock);
    let mut cmd = tunnel_command(
        dest,
        local_sock,
        remote_sock,
        extra_ssh_opts,
        foreground,
        isolate_control_path,
        connect_timeout,
    );
    cmd.kill_on_drop(true);
    info!("spawning ssh tunnel: {}", format_command(&cmd));
    let mut child = cmd.spawn().context("failed to spawn ssh tunnel")?;
    info!("ssh tunnel pid: {:?}", child.id());
    drain_stderr(&mut child);
    Ok(child)
}

/// How long to wait for the tunnel socket to appear, given the ssh
/// ConnectTimeout. Leaves headroom for ProxyCommand startup and forward setup.
fn socket_wait_deadline(connect_timeout: u64) -> Duration {
    Duration::from_secs(if connect_timeout == 0 { 60 } else { connect_timeout.max(5) + 10 })
}

/// Poll until the local socket is connectable (200ms interval).
///
/// On timeout, the last `connect()` error is included so callers can tell
/// NotFound (ssh never bound the -L listener) from ConnectionRefused
/// (socket exists, nothing listening) from InvalidInput (path too long).
async fn wait_for_socket(path: &Path, timeout: Duration) -> anyhow::Result<()> {
    let deadline = Instant::now() + timeout;
    let mut last_kind = None;
    loop {
        match std::os::unix::net::UnixStream::connect(path) {
            Ok(_) => return Ok(()),
            Err(e) => {
                if last_kind != Some(e.kind()) {
                    info!("waiting for tunnel socket {}: {}: {e}", path.display(), e.kind());
                    last_kind = Some(e.kind());
                }
                if Instant::now() >= deadline {
                    bail!(
                        "timed out after {}s waiting for SSH tunnel socket at {} ({}: {e})",
                        timeout.as_secs(),
                        path.display(),
                        e.kind(),
                    );
                }
            }
        }
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
}

/// Drain a child's piped stderr in the background so it can never fill the
/// kernel pipe buffer and wedge SSH. Output goes to our own stderr, which in
/// daemonized mode is the tunnel's `.out` file.
fn drain_stderr(child: &mut Child) {
    if let Some(mut stderr) = child.stderr.take() {
        tokio::spawn(async move {
            let _ = tokio::io::copy(&mut stderr, &mut tokio::io::stderr()).await;
        });
    }
}

// ---------------------------------------------------------------------------
// Tunnel monitor timing constants and pure helpers
// ---------------------------------------------------------------------------

/// Minimum backoff between respawn attempts; also the initial value.
const MIN_RESPAWN_BACKOFF: Duration = Duration::from_secs(1);
/// Maximum backoff between respawn attempts.
const MAX_RESPAWN_BACKOFF: Duration = Duration::from_secs(60);
/// A child that lived at least this long before dying is considered a
/// healthy cycle; the next respawn uses `MIN_RESPAWN_BACKOFF`.
const HEALTHY_CHILD_THRESHOLD: Duration = Duration::from_secs(30);
/// App-layer tunnel liveness probe cadence. SSH's ServerAliveInterval
/// covers TCP-level liveness, but can't detect a remote gritty daemon
/// that died while ssh stayed up (OOM, crash, manual kill).
const PROBE_INTERVAL: Duration = Duration::from_secs(30);
/// Consecutive probe failures before we kill ssh to force a respawn.
const PROBE_FAILURES_BEFORE_RESPAWN: u32 = 2;
/// Debounce window for the net-change-triggered probe. macOS
/// `nw_path_monitor` fires a burst of events (~100-200ms apart) during wifi
/// renegotiation; probing on the first one races the outage itself and
/// kills an SSH whose TCP would have survived a sub-second blip. Waiting
/// for the burst to settle lets the probe test the post-transition state.
const NET_PROBE_DEBOUNCE: Duration = Duration::from_millis(500);
/// A freshly-spawned ssh needs this long to connect and bind its `-L`
/// forward; a net-change probe failure before then means "not ready yet",
/// not "broken", so the single-strike kill is suppressed.
const SPAWN_GRACE: Duration = Duration::from_secs(5);
/// If wall-clock time advanced this much more than monotonic time across a
/// backoff sleep, the process was suspended (lid close / Power Nap). Used to
/// reset the respawn backoff for one immediate attempt on wake, since the
/// Unsatisfied->Satisfied edge detector can't observe the Unsatisfied that
/// happened while the process was frozen. 5s is well above NTP slew/step
/// noise and well below any real suspend.
const SUSPEND_JUMP_THRESHOLD: Duration = Duration::from_secs(5);
/// How often to sample for a suspend jump during backoff. Instant-based, so
/// after a suspend this fires ~2s (monotonic) after wake.
const SUSPEND_POLL: Duration = Duration::from_secs(2);
/// If the tunnel last passed an app-layer probe within this window, skip
/// `ensure_remote_ready` on respawn -- the remote daemon was just seen
/// alive, so the ~2s SSH-exec round-trip to re-verify it is pure latency
/// on a transient network blip. Beyond this window, re-verify (covers
/// remote reboot / daemon upgrade).
const SKIP_ENSURE_REMOTE_THRESHOLD: Duration = Duration::from_secs(60);

/// Classification of an ssh child's exit status. The monitor retries on
/// `Transient` and gives up on `NonTransient`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ExitClass {
    Transient,
    NonTransient,
}

/// Map an ssh exit status to our retry policy.
///
/// Transient (retry):
/// - `None` (code): locally signal-killed; typically our own
///   `child.kill()` after a probe failure.
/// - `Some(255)`: ssh's own connection-error exit.
/// - `Some(128..=159)`: remote-side signal death (reboot, OOM,
///   SIGTERM during remote shutdown) -- ssh reports these as
///   `128 + signum`.
///
/// Non-transient (give up):
/// - Any other exit code (auth failure, config error, etc.).
fn classify_exit(status: std::process::ExitStatus) -> ExitClass {
    match status.code() {
        None => ExitClass::Transient,
        Some(255) => ExitClass::Transient,
        Some(c) if (128..=159).contains(&c) => ExitClass::Transient,
        Some(_) => ExitClass::NonTransient,
    }
}

/// How long to wait before the next respawn, given the previous backoff and
/// the just-exited child's uptime.
///
/// If the prior child lived at least `HEALTHY_CHILD_THRESHOLD` the cycle is
/// considered healthy and the sleep resets to `MIN_RESPAWN_BACKOFF` -- so a
/// tunnel that stabilized for 30s+ gets an aggressive 1s retry on its first
/// death. Otherwise the already-climbing previous backoff is reused. The
/// progression is advanced separately via [`double_backoff`] after the wait
/// (the wait can shorten `sleep`, e.g. on a network-recovery wake).
fn respawn_sleep(prev: Duration, prior_uptime: Duration) -> Duration {
    if prior_uptime >= HEALTHY_CHILD_THRESHOLD { MIN_RESPAWN_BACKOFF } else { prev }
}

/// Advance the respawn backoff: double it, clamped to
/// `[MIN_RESPAWN_BACKOFF, MAX_RESPAWN_BACKOFF]`.
///
/// The lower clamp is load-bearing, not cosmetic: the `we_killed` path sleeps
/// for `Duration::ZERO`, so a plain `* 2` would pin the backoff at zero and
/// defeat the climb. This is the single source of the doubling rule.
fn double_backoff(cur: Duration) -> Duration {
    (cur.max(MIN_RESPAWN_BACKOFF) * 2).min(MAX_RESPAWN_BACKOFF)
}

/// Per-child supervised state. Grouping child handle, spawn time, and
/// probe-failure counter into one struct prevents counters from leaking
/// across respawns: replacing the `ChildState` is the only way to
/// install a new child, and that construction zeroes `probe_failures`
/// and resets `spawned_at` by virtue of the `new()` constructor.
struct ChildState {
    child: Child,
    spawned_at: Instant,
    probe_failures: u32,
}

impl ChildState {
    fn new(child: Child) -> Self {
        Self { child, spawned_at: Instant::now(), probe_failures: 0 }
    }

    /// True once the `-L` forward has had time to bind. Gates the
    /// net-change single-strike kill: before this, a probe failure means
    /// "not ready yet", not "broken".
    fn past_spawn_grace(&self) -> bool {
        self.uptime() >= SPAWN_GRACE
    }

    fn uptime(&self) -> Duration {
        self.spawned_at.elapsed()
    }
}

/// Background task: monitor SSH child, respawn on transient failure.
/// Uses exponential backoff (1s..60s) and never gives up on transient errors.
///
/// `lock_path` + `lock_identity` let the monitor notice when it has become a
/// ghost: if the lock file is unlinked or replaced (another supervisor now
/// owns the path), this process keeps holding an flock on a deleted inode that
/// no one else can observe. Rather than silently compete with the real owner
/// over the `-L` socket, it exits on the next probe tick; `ConnectGuard::drop`
/// then sees `!matches_path` and leaves the owner's files alone.
#[allow(clippy::too_many_arguments)]
async fn tunnel_monitor(
    child: Child,
    dest: Destination,
    local_sock: PathBuf,
    initial_remote_sock: String,
    extra_ssh_opts: Vec<String>,
    foreground: bool,
    no_server_start: bool,
    isolate_control_path: bool,
    connect_timeout: u64,
    lock_path: PathBuf,
    lock_identity: Option<LockIdentity>,
    stop: tokio_util::sync::CancellationToken,
) {
    // Seeded from the caller's initial ensure_remote_ready so a fast
    // respawn can reuse it; re-derived via ensure_remote_ready whenever
    // the tunnel has been down long enough that a reboot / daemon upgrade
    // / remote relocation is plausible.
    let mut remote_sock = initial_remote_sock;
    let mut backoff = MIN_RESPAWN_BACKOFF;
    // Most recent time the remote daemon was confirmed alive (by an
    // app-layer probe here, or by the caller's ensure_remote_ready that
    // produced initial_remote_sock). Gates whether respawn re-runs
    // ensure_remote_ready. Seeded now because the caller just verified it.
    // SystemTime (not Instant) so a laptop suspend counts toward the
    // SKIP_ENSURE_REMOTE_THRESHOLD -- Instant pauses across suspend, which
    // made a 4-minute lid-close read as "recently healthy".
    let mut last_healthy =
        if remote_sock.is_empty() { None } else { Some(std::time::SystemTime::now()) };
    let net = crate::net_watch::NetWatcher::spawn();
    let mut probe_ticker = tokio::time::interval(PROBE_INTERVAL);
    probe_ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    probe_ticker.tick().await; // consume the immediate first tick
    let mut state = ChildState::new(child);
    // Debounced net-change probe: set on each net.changed(), cleared when
    // the deadline elapses and the probe runs (or when the child is
    // replaced). See NET_PROBE_DEBOUNCE.
    let mut net_probe_at: Option<Instant> = None;
    // Set when the supervisor itself killed the child (probe failure).
    // Suppresses the respawn backoff: we already rate-limited via the
    // debounce + probe timeout, and there's no flap to guard against.
    let mut we_killed = false;
    // Whether the OS has reported Unsatisfied since the last successful
    // probe. A burst that stays Satisfied throughout is interface-property
    // noise (not an outage) -- probing on it risks a transient HelloAck
    // timeout killing a working SSH. ServerAliveInterval still covers a
    // genuine seamless route switch in ~6-9s.
    let mut saw_unsatisfied = false;

    loop {
        tokio::select! {
            _ = stop.cancelled() => {
                let _ = state.child.kill().await;
                return;
            }
            _ = net.changed() => {
                // OS says the network path changed. Don't probe yet --
                // nw_path_monitor fires a burst during wifi renegotiation,
                // and probing mid-burst races the outage itself. Arm a
                // debounced probe; each further event pushes it out.
                let status = net.status();
                if status == crate::net_watch::PathStatus::Unsatisfied {
                    saw_unsatisfied = true;
                }
                info!(?status, saw_unsatisfied, "network path changed; arming debounced probe");
                net_probe_at = Some(Instant::now() + NET_PROBE_DEBOUNCE);
                continue;
            }
            _ = async {
                match net_probe_at {
                    Some(at) => tokio::time::sleep_until(at.into()).await,
                    None => std::future::pending().await,
                }
            } => {
                // Debounce window elapsed with no further path changes --
                // probe the post-transition state. ssh's ServerAlive* would
                // self-detect a dead TCP socket in ~6-9s; this gets us to
                // respawn in ~1.5s instead. Single-strike, but only once
                // this child is past SPAWN_GRACE (so a fresh ssh whose -L
                // hasn't bound yet isn't killed by its own startup race).
                net_probe_at = None;
                if !saw_unsatisfied {
                    info!("debounced network probe: path never went unsatisfied; skipping");
                    continue;
                }
                info!(
                    status = ?net.status(),
                    uptime_s = state.uptime().as_secs(),
                    "debounced network probe"
                );
                match probe_tunnel_alive(&local_sock).await {
                    // Version is irrelevant to tunnel liveness -- the
                    // supervisor is a byte proxy and must not kill ssh over a
                    // protocol mismatch; that is the client handshake's job.
                    Ok(_) => {
                        state.probe_failures = 0;
                        last_healthy = Some(std::time::SystemTime::now());
                        saw_unsatisfied = false;
                    }
                    Err(why) if state.past_spawn_grace() => {
                        info!("tunnel probe failed after path change ({why}); killing ssh to respawn");
                        we_killed = true;
                        let _ = state.child.kill().await;
                    }
                    Err(why) => {
                        info!(
                            uptime_s = state.uptime().as_secs(),
                            "tunnel probe failed after path change ({why}); ssh still in spawn grace, not killing"
                        );
                    }
                }
                continue;
            }
            _ = probe_ticker.tick() => {
                if !lock_still_owned(lock_identity, &lock_path) {
                    warn!(
                        lock_path = %lock_path.display(),
                        "lock file replaced or removed; another supervisor owns this \
                         tunnel -- exiting"
                    );
                    let _ = state.child.kill().await;
                    return;
                }
                match probe_tunnel_alive(&local_sock).await {
                    Ok(_) => {
                        if state.probe_failures > 0 {
                            info!("tunnel probe recovered");
                        }
                        state.probe_failures = 0;
                        last_healthy = Some(std::time::SystemTime::now());
                        saw_unsatisfied = false;
                    }
                    Err(why) => {
                        state.probe_failures += 1;
                        info!(
                            "tunnel probe failed ({why}) [{}/{PROBE_FAILURES_BEFORE_RESPAWN}]",
                            state.probe_failures
                        );
                        if state.probe_failures >= PROBE_FAILURES_BEFORE_RESPAWN {
                            warn!(
                                "tunnel probe failed {}x; remote daemon looks dead, killing ssh to respawn",
                                state.probe_failures
                            );
                            we_killed = true;
                            let _ = state.child.kill().await;
                            // The failure counter is zeroed when the
                            // replacement ChildState is installed below;
                            // no need to reset here.
                        }
                    }
                }
                continue;
            }
            status = state.child.wait() => {
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

                // Drop any pending debounced net-probe and the Unsatisfied
                // latch -- both were aimed at the dead child. probe_failures
                // is reset by ChildState::new() when the replacement is
                // installed below.
                saw_unsatisfied = false;

                let code = status.code();
                info!("ssh tunnel exited: {:?}", code);

                if classify_exit(status) == ExitClass::NonTransient {
                    warn!("ssh tunnel exited with code {code:?} (not retrying)");
                    return;
                }

                // Apply the healthy-reset ONCE, based on how long the child
                // that just died actually ran. Previously `state.uptime()`
                // was re-evaluated on every retry (via `continue` to the
                // outer select and `child.wait()` re-resolving the cached
                // status), which meant a long-dead child's ever-growing
                // uptime kept resetting backoff to MIN -- so
                // ensure_remote_ready failures hammered the remote at ~1s
                // with no climb, tripping its auth rate-limit during macOS
                // dark wakes when the Keychain SSH agent refuses to sign.
                let died_uptime = state.uptime();
                let mut sleep = if we_killed {
                    we_killed = false;
                    Duration::ZERO
                } else {
                    respawn_sleep(backoff, died_uptime)
                };
                let mut backoff_saw_unsatisfied = false;

                let new_child = loop {
                    // Re-check lock ownership every iteration. The outer
                    // select's probe_ticker arm -- the only other place this
                    // is checked -- never runs while control is inside this
                    // respawn loop, so during a prolonged outage a ghost
                    // supervisor whose lock file was replaced externally would
                    // otherwise spin here for hours and eventually spawn_tunnel
                    // -> remove_file() the legitimate supervisor's socket out
                    // from under it. ConnectGuard::drop gates its own cleanup
                    // on the same identity, so a plain return is safe here.
                    if !lock_still_owned(lock_identity, &lock_path) {
                        warn!(
                            lock_path = %lock_path.display(),
                            "lock ownership lost during respawn; another supervisor \
                             owns this tunnel -- exiting"
                        );
                        return;
                    }
                    info!("respawn: sleeping {}s (backoff)", sleep.as_secs());
                    // Fixed deadline so net.changed() noise re-entering the
                    // select doesn't restart the sleep from scratch -- a
                    // lid-open Satisfied burst after backoff has climbed to
                    // 60s must neither reset the climb nor extend the wait.
                    let deadline = tokio::time::Instant::now() + sleep;
                    // Anchors for suspend detection: if wall-clock advanced
                    // far more than monotonic across the wait, the process
                    // was frozen. Instant pauses during suspend on both
                    // Linux (CLOCK_MONOTONIC) and macOS (CLOCK_UPTIME_RAW);
                    // SystemTime (CLOCK_REALTIME) does not. Same assumption
                    // client.rs::wall_elapsed() already relies on.
                    let wall_anchor = std::time::SystemTime::now();
                    let mono_anchor = Instant::now();
                    'wait: loop {
                        tokio::select! {
                            _ = tokio::time::sleep_until(deadline) => break 'wait,
                            _ = tokio::time::sleep(SUSPEND_POLL) => {
                                let wall = std::time::SystemTime::now()
                                    .duration_since(wall_anchor)
                                    .unwrap_or_default();
                                let mono = mono_anchor.elapsed();
                                if wall > mono + SUSPEND_JUMP_THRESHOLD {
                                    // Cut this sleep short for one attempt;
                                    // do NOT reset the climb -- a dark wake
                                    // with a locked Keychain should cost
                                    // exactly one failed auth and then
                                    // resume the climbed backoff.
                                    info!(
                                        jump_s = wall.saturating_sub(mono).as_secs(),
                                        "detected wake from suspend during backoff; retrying now"
                                    );
                                    break 'wait;
                                }
                            }
                            _ = net.changed() => {
                                let status = net.status();
                                if status == crate::net_watch::PathStatus::Unsatisfied {
                                    backoff_saw_unsatisfied = true;
                                    debug!(?status, "network path changed during backoff (no route); continuing to wait");
                                } else if backoff_saw_unsatisfied {
                                    info!(?status, "network path recovered during backoff; retrying now");
                                    backoff_saw_unsatisfied = false;
                                    sleep = MIN_RESPAWN_BACKOFF;
                                    break 'wait;
                                } else {
                                    debug!(?status, "network path changed during backoff (noise); ignoring");
                                }
                            }
                            _ = stop.cancelled() => return,
                        }
                    }
                    backoff = double_backoff(sleep);

                    // Re-run ensure_remote_ready so a remote that rebooted,
                    // crashed, or was upgraded gets its gritty server
                    // started again before we point SSH at its ctl socket.
                    // Skipped when the tunnel was just proven healthy -- a
                    // sub-minute network blip can't have rebooted the
                    // remote, and the ~2s SSH-exec is pure latency.
                    let healthy_ago =
                        last_healthy.map(|t| t.elapsed().unwrap_or(Duration::ZERO));
                    let recently_healthy =
                        healthy_ago.is_some_and(|d| d < SKIP_ENSURE_REMOTE_THRESHOLD);
                    if recently_healthy && !remote_sock.is_empty() {
                        info!(
                            ago_s = healthy_ago.map(|d| d.as_secs()),
                            "skipping ensure_remote_ready (tunnel recently healthy)"
                        );
                    } else {
                        match ensure_remote_ready(
                            &dest,
                            no_server_start,
                            &extra_ssh_opts,
                            foreground,
                            connect_timeout,
                        )
                        .await
                        {
                            Ok((sock, _ver)) => {
                                remote_sock = sock;
                            }
                            Err(e) => {
                                warn!("ensure_remote_ready failed on respawn: {e}");
                                sleep = backoff;
                                continue;
                            }
                        }
                    }

                    match spawn_tunnel(
                        &dest,
                        &local_sock,
                        &remote_sock,
                        &extra_ssh_opts,
                        foreground,
                        isolate_control_path,
                        connect_timeout,
                    )
                    .await
                    {
                        Ok(c) => break c,
                        Err(e) => {
                            warn!("failed to respawn ssh tunnel: {e}");
                            sleep = backoff;
                            continue;
                        }
                    }
                };
                info!("ssh tunnel respawned");
                state = ChildState::new(new_child);
                net_probe_at = None;
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Remote server management
// ---------------------------------------------------------------------------

// Stdout contract: socket path, protocol version, then an optional third
// line. On success the third line is absent. On failure it starts with
// `ERR:` and carries the stderr of the `gritty ls local` probe -- which is
// the most useful diagnostic because `ls` already produces precise errors
// like "protocol version mismatch: run gritty restart" or "no server
// running". Previously, any failure in the `ls || server` fallback collapsed
// the whole `&&` chain to empty stdout and the user got the unhelpful
// "remote host returned empty socket path" with no clue which step broke.
//
// Any command before the `echo`s must redirect stdout to /dev/null so its
// output doesn't leak into the line protocol. `gritty server` today only
// prints to stderr, but redirecting defensively keeps the contract from
// silently breaking if that ever changes.
const REMOTE_ENSURE_CMD: &str = "\
    SOCK=$(gritty socket-path) && \
    V=$(gritty protocol-version 2>/dev/null || true) && \
    LS_ERR=$(gritty ls local 2>&1 >/dev/null) && \
    { echo \"$SOCK\"; echo \"$V\"; exit 0; }; \
    gritty server >/dev/null 2>&1 && sleep 0.3 && \
    { echo \"$SOCK\"; echo \"$V\"; exit 0; }; \
    echo \"$SOCK\"; echo \"$V\"; echo \"ERR:$LS_ERR\"";

/// Parse the 2-or-3-line `REMOTE_ENSURE_CMD` output. Factored out so the
/// line-protocol contract can be tested without an SSH host.
fn parse_ensure_remote_output(output: &str) -> anyhow::Result<(String, Option<u16>)> {
    let mut lines = output.lines();
    let sock_path = lines.next().unwrap_or("").trim().to_string();
    let remote_version = lines.next().and_then(|s| s.trim().parse::<u16>().ok());
    // Third line, if present, is an error tag the remote shell emitted because
    // the daemon couldn't be reached or started. Surface it verbatim -- it's
    // usually the `gritty ls` error message, which already tells the user what
    // to do (e.g. "protocol version mismatch: run `gritty restart`").
    if let Some(err_line) = lines.next()
        && let Some(reason) = err_line.strip_prefix("ERR:")
    {
        let reason = reason.trim();
        if reason.is_empty() {
            bail!("remote daemon is unusable and could not be started");
        }
        bail!(
            "remote daemon is unusable and could not be started: {reason}\n  \
             hint: `gritty restart <host>` kills the remote daemon and respawns the tunnel"
        );
    }
    if sock_path.is_empty() {
        bail!("remote host returned empty socket path");
    }
    Ok((sock_path, remote_version))
}

/// Get the remote socket path and optionally auto-start the server.
/// Returns (socket_path, remote_protocol_version).
async fn ensure_remote_ready(
    dest: &Destination,
    no_server_start: bool,
    extra_ssh_opts: &[String],
    foreground: bool,
    connect_timeout: u64,
) -> anyhow::Result<(String, Option<u16>)> {
    let remote_cmd = if no_server_start { "gritty socket-path" } else { REMOTE_ENSURE_CMD };
    info!("ensuring remote server (no_server_start={no_server_start})");

    let output = remote_exec(dest, remote_cmd, extra_ssh_opts, foreground, connect_timeout).await?;
    parse_ensure_remote_output(&output)
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

pub fn connect_dest_path(connection_name: &str) -> PathBuf {
    crate::daemon::socket_dir().join(format!("connect-{connection_name}.dest"))
}

/// Sidecar recording the CLI `-o` SSH options (one per line) so a restart /
/// auto-start can replay them. Only the *pre-merge* CLI options are stored --
/// config-file `ssh-options` are re-resolved by `tunnel-create`, so persisting
/// the merged set would double them on replay.
pub fn connect_ssh_opts_path(connection_name: &str) -> PathBuf {
    crate::daemon::socket_dir().join(format!("connect-{connection_name}.ssh-opts"))
}

/// Read the persisted CLI `-o` SSH options for a tunnel (empty if none).
pub fn read_persisted_ssh_options(connection_name: &str) -> Vec<String> {
    std::fs::read_to_string(connect_ssh_opts_path(connection_name))
        .map(|s| s.lines().filter(|l| !l.is_empty()).map(str::to_string).collect())
        .unwrap_or_default()
}

/// Build the `tunnel-create` argument list to recreate a tunnel on restart /
/// auto-start, replaying any persisted CLI `-o` options. Config `ssh-options`
/// are intentionally omitted -- `tunnel-create` re-resolves them.
pub fn tunnel_recreate_args(connection_name: &str, destination: &str) -> Vec<String> {
    build_tunnel_recreate_args(
        connection_name,
        destination,
        &read_persisted_ssh_options(connection_name),
    )
}

fn build_tunnel_recreate_args(
    connection_name: &str,
    destination: &str,
    ssh_options: &[String],
) -> Vec<String> {
    let mut args =
        vec!["tunnel-create".to_string(), "--name".to_string(), connection_name.to_string()];
    for opt in ssh_options {
        args.push("-o".to_string());
        args.push(opt.clone());
    }
    args.push(destination.to_string());
    args
}

/// Cache of the remote daemon's ctl socket path. Lets a subsequent
/// `tunnel-create` skip the ~2s `ensure_remote_ready` SSH-exec when the
/// path is known (it only changes if the remote UID / `XDG_RUNTIME_DIR`
/// changes). Deliberately NOT removed on tunnel teardown -- it is a
/// persistence cache, not live state.
pub fn connect_remote_sock_path(connection_name: &str) -> PathBuf {
    crate::daemon::socket_dir().join(format!("connect-{connection_name}.remote-sock"))
}

pub fn connect_log_path(connection_name: &str) -> PathBuf {
    crate::daemon::socket_dir().join(format!("connect-{connection_name}.log"))
}

pub fn connect_out_path(connection_name: &str) -> PathBuf {
    crate::daemon::socket_dir().join(format!("connect-{connection_name}.out"))
}

/// Compute the local socket path for a given connection name.
/// Public so main.rs can compute the path in the parent process after daemonize.
pub fn connection_socket_path(connection_name: &str) -> PathBuf {
    local_socket_path(connection_name)
}

/// Return the tunnel lock path corresponding to a ctl socket path, if the
/// ctl path looks like one produced by `tunnel-create` (i.e. a
/// `connect-<name>.sock` file in the gritty socket dir). Returns `None`
/// for a plain local daemon ctl socket or any unrecognized path -- the
/// client uses this to distinguish a tunnel respawning (socket temporarily
/// gone but supervisor still holding the lock) from a tunnel destroyed.
pub fn ctl_socket_lock_path(ctl_path: &Path) -> Option<PathBuf> {
    let file = ctl_path.file_name()?.to_str()?;
    let name = file.strip_prefix("connect-")?.strip_suffix(".sock")?;
    Some(connect_lock_path(name))
}

/// Extract the host component from a destination string (`[user@]host[:port]`).
pub fn parse_host(destination: &str) -> anyhow::Result<String> {
    Ok(Destination::parse(destination)?.host)
}

/// Synchronous SSH connectivity check -- call before daemonizing to catch
/// host-key prompts and password prompts while the terminal is still attached.
pub fn preflight_ssh(
    dest_str: &str,
    ssh_options: &[String],
    connect_timeout: u64,
) -> anyhow::Result<()> {
    let dest = Destination::parse(dest_str)?;
    let mut cmd = std::process::Command::new("ssh");
    cmd.args(dest.port_args());
    for opt in ssh_options {
        cmd.arg("-o");
        cmd.arg(opt);
    }
    cmd.args(["-o", "BatchMode=yes"]);
    if connect_timeout > 0 {
        cmd.args(["-o", &format!("ConnectTimeout={connect_timeout}")]);
    }
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
// Bootstrap
// ---------------------------------------------------------------------------

const INSTALL_SCRIPT_URL: &str =
    "https://raw.githubusercontent.com/chipturner/gritty/main/install.sh";

/// Quote `--install-dir` for safe interpolation into the remote bootstrap
/// shell command. A leading `~` becomes `"$HOME"` so it still expands on the
/// remote; the rest is single-quoted so spaces and shell metacharacters in the
/// path are inert. (A naive "just single-quote it" would break the shipped
/// `~/.local/bin` default, which only works because it is unquoted.)
fn quote_remote_install_dir(install_dir: &str) -> String {
    let quote =
        |s: &str| shlex::try_quote(s).map(|c| c.into_owned()).unwrap_or_else(|_| s.to_string());
    if install_dir == "~" {
        "\"$HOME\"".to_string()
    } else if let Some(rest) = install_dir.strip_prefix("~/") {
        format!("\"$HOME\"/{}", quote(rest))
    } else {
        quote(install_dir)
    }
}

/// Install gritty on a remote host by running the install script via SSH.
pub async fn bootstrap(
    dest_str: &str,
    ssh_options: &[String],
    install_dir: &str,
    connect_timeout: u64,
) -> anyhow::Result<()> {
    let dest = Destination::parse(dest_str)?;

    eprintln!("\x1b[2m\u{25b8} installing gritty on {}...\x1b[0m", dest.ssh_dest());

    let install_cmd = format!(
        "GRITTY_INSTALL_DIR={} sh -c \"$(curl -sSfL {INSTALL_SCRIPT_URL})\"",
        quote_remote_install_dir(install_dir)
    );

    // Run interactively (foreground=true) so SSH can prompt if needed,
    // and pipe stdout/stderr through so the user sees install progress.
    let mut cmd = Command::new("ssh");
    cmd.args(base_ssh_args(&dest, ssh_options, true, connect_timeout));
    cmd.arg(dest.ssh_dest());
    cmd.arg(&install_cmd);

    let status = cmd.status().await.context("failed to run ssh")?;
    if !status.success() {
        bail!("remote install failed (exit {status})");
    }

    Ok(())
}

/// Run `gritty <args>` on a remote host interactively (stdin/stdout/stderr
/// inherited so the user sees progress live). Used by `gritty refresh <host>`
/// to delegate the remote-side daemon refresh to the remote binary rather
/// than reimplementing it over the wire -- keeps the remote the source of
/// truth about its own daemon staleness.
pub async fn run_remote_gritty(
    dest_str: &str,
    gritty_args: &[&str],
    ssh_options: &[String],
    connect_timeout: u64,
) -> anyhow::Result<std::process::ExitStatus> {
    let dest = Destination::parse(dest_str)?;
    // Prefix PATH so `gritty` resolves even when SSH's non-interactive shell
    // skips the user's profile (same rationale as `remote_exec_command`).
    let remote_cmd = format!("PATH=\"{REMOTE_PATH_PREFIX}\" gritty {}", gritty_args.join(" "));
    let mut cmd = Command::new("ssh");
    cmd.args(base_ssh_args(&dest, ssh_options, true, connect_timeout));
    cmd.arg(dest.ssh_dest());
    cmd.arg(&remote_cmd);
    cmd.status().await.context("failed to run ssh")
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
pub fn is_lock_held(lock_path: &Path) -> bool {
    use std::fs::OpenOptions;
    let file = match OpenOptions::new().read(true).open(lock_path) {
        Ok(f) => f,
        Err(_) => return false,
    };
    // Non-blocking exclusive lock attempt: if it succeeds, the old process is dead.
    // The lock is released immediately when the Flock drops.
    nix::fcntl::Flock::lock(file, nix::fcntl::FlockArg::LockExclusiveNonblock).is_err()
}

/// Device + inode pair that uniquely names the file a supervisor flock'd at
/// startup. An flock is held on an *inode*, not a *path*: if the lock file
/// is unlinked out from under the supervisor (an external `rm`, a racy
/// cleanup, or another supervisor's Drop), our flock keeps existing on a
/// deleted inode while `is_lock_held(path)` reports false and a fresh
/// `tunnel-create` O_CREATs a new inode at the same path. Comparing
/// `(dev, ino)` against the on-disk path is the only way to tell whether
/// the lock we hold is the one the rest of the system can observe.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct LockIdentity {
    dev: u64,
    ino: u64,
}

impl LockIdentity {
    /// Identity of the open file backing a held flock.
    fn of_held(lock: &nix::fcntl::Flock<std::fs::File>) -> Option<Self> {
        use std::os::unix::fs::MetadataExt;
        let m = lock.metadata().ok()?;
        Some(Self { dev: m.dev(), ino: m.ino() })
    }

    /// Identity of whatever is currently at `path` (if anything).
    fn of_path(path: &Path) -> Option<Self> {
        use std::os::unix::fs::MetadataExt;
        let m = std::fs::metadata(path).ok()?;
        Some(Self { dev: m.dev(), ino: m.ino() })
    }

    /// Does this held-lock identity still match the file at `path`?
    /// False if `path` is gone or points at a different inode (a fresh
    /// supervisor re-created it) -- either way, we no longer own the path
    /// and must not disturb it.
    fn matches_path(self, path: &Path) -> bool {
        Self::of_path(path) == Some(self)
    }
}

/// Is the lock at `path` still the one we flock'd at startup? Treats an
/// unknown startup identity (a failed `fstat` -- pathological, never
/// observed) as "still owned" so a one-off stat failure can't make the
/// supervisor exit and tear down a working tunnel.
fn lock_still_owned(held: Option<LockIdentity>, path: &Path) -> bool {
    held.is_none_or(|id| id.matches_path(path))
}

/// Remove a stale tunnel's sidecar files, but only after proving no live
/// supervisor holds the lock. Acquiring the flock non-blocking both verifies
/// staleness *and* guards the unlink window: while we hold the flock on this
/// inode, no concurrent `try_acquire_lock` can observe the lock as free (it
/// opens the same path, same inode), so nothing can sneak in and re-create
/// sidecar files between our check and our unlink. Returns `true` if cleanup
/// ran, `false` if a live supervisor held the lock.
///
/// This replaces the old pattern of `is_lock_held(p)` followed by
/// `cleanup_stale_files(name, true)`, which had a TOCTOU window: a new
/// supervisor could acquire the lock between the probe and the unlink, and
/// the unlink would then yank the winner's fresh lock file.
fn cleanup_if_unheld(name: &str) -> bool {
    let lock_path = connect_lock_path(name);
    let lock = match try_acquire_lock(&lock_path) {
        Ok(lock) => lock,
        Err(_) => return false,
    };
    cleanup_stale_files(name);
    // Unlink *before* releasing: a racer that O_CREATs the path after the
    // unlink gets a brand-new inode and flocks it independently of ours, so
    // there is never a window where the path points at an inode we're about
    // to delete. See `ConnectGuard::drop` for the same ordering rationale.
    let _ = std::fs::remove_file(&lock_path);
    drop(lock);
    true
}

/// Tunnel health status.
#[derive(Debug, PartialEq, Eq)]
pub enum TunnelStatus {
    Healthy,
    Reconnecting,
    Stale,
}

/// Probe a tunnel's status using lockfile + socket connectivity.
/// App-layer tunnel probe for the monitor loop. Opens the local socket and
/// exchanges Hello/HelloAck with whatever the tunnel is forwarding to; any
/// failure (connection refused, timeout, EOF) means "daemon unresponsive".
///
/// Uses tight timeouts (3s total, 1s handshake) because this runs in the
/// hot path -- a slow probe that blocks the select loop is worse than a
/// false positive that triggers an unnecessary respawn.
/// Gate the connect on remote/local protocol compatibility.
///
/// A mismatch bails with an actionable message unless `ignore` is set, in
/// which case it only warns. Shared by the slow path (the version
/// `ensure_remote_ready` reports) and the cached fast path (the version
/// `probe_tunnel_alive` observes): without this being applied on *both*, a
/// cached connect to a freshly-upgraded remote would silently succeed and
/// defeat the fail-fast diagnostic this check exists to provide.
fn check_remote_protocol_version(remote: u16, ignore: bool) -> anyhow::Result<()> {
    if remote == crate::protocol::PROTOCOL_VERSION {
        return Ok(());
    }
    let msg = format!(
        "remote protocol version ({remote}) differs from local ({}); \
         use --ignore-version-mismatch to connect anyway",
        crate::protocol::PROTOCOL_VERSION
    );
    if ignore {
        warn!("{msg}");
        Ok(())
    } else {
        bail!("{msg}");
    }
}

/// Probe the tunnel for a live remote daemon, returning the remote's protocol
/// version from its `HelloAck`. The caller uses it to apply the version gate
/// on the cached fast path (where `ensure_remote_ready` was skipped).
async fn probe_tunnel_alive(local_sock: &std::path::Path) -> Result<u16, &'static str> {
    use crate::protocol::{Frame, PROTOCOL_VERSION};
    use futures_util::{SinkExt, StreamExt};

    let probe = async {
        let stream =
            tokio::net::UnixStream::connect(local_sock).await.map_err(|_| "connect failed")?;
        let codec = crate::protocol::FrameCodec;
        let mut framed = tokio_util::codec::Framed::new(stream, codec);
        framed
            .send(Frame::Hello { version: PROTOCOL_VERSION, capabilities: 0, device_id: 0 })
            .await
            .map_err(|_| "Hello send failed")?;
        let remote_version = match tokio::time::timeout(Duration::from_secs(1), framed.next()).await
        {
            Ok(Some(Ok(Frame::HelloAck { version, .. }))) => version,
            Ok(Some(Ok(_))) => return Err("unexpected frame"),
            Ok(Some(Err(_))) => return Err("decode error"),
            Ok(None) => return Err("EOF before HelloAck"),
            Err(_) => return Err("HelloAck timeout"),
        };
        // Send a control frame so the daemon doesn't wait 5s for one and
        // log a spurious "control connection timed out" warning on every probe.
        framed.send(Frame::ListSessions).await.map_err(|_| "ListSessions send failed")?;
        let _ = tokio::time::timeout(Duration::from_secs(1), framed.next()).await;
        Ok(remote_version)
    };
    tokio::time::timeout(Duration::from_secs(3), probe).await.unwrap_or(Err("probe timeout"))
}

pub fn probe_tunnel_status(name: &str) -> TunnelStatus {
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
pub fn read_pid_hint(name: &str) -> Option<u32> {
    std::fs::read_to_string(connect_pid_path(name)).ok().and_then(|s| s.trim().parse().ok())
}

/// Remove the non-lock sidecar files (`.sock`, `.pid`, `.info`, `.dest`,
/// `.ssh-opts`).
/// Callers must hold the tunnel flock -- either they just acquired it
/// (`run()`, `cleanup_if_unheld()`) or they verified ownership via
/// `LockIdentity::matches_path` (`ConnectGuard::drop`). The lock file itself
/// is never removed here: it is the thing that *makes* this call safe, and
/// its removal has its own inode-ownership rules.
fn cleanup_stale_files(name: &str) {
    let _ = std::fs::remove_file(local_socket_path(name));
    let _ = std::fs::remove_file(connect_pid_path(name));
    let _ = std::fs::remove_file(crate::runinfo::connect_info_path(name));
    let _ = std::fs::remove_file(connect_dest_path(name));
    let _ = std::fs::remove_file(connect_ssh_opts_path(name));
}

/// Extract tunnel connection names by globbing lock files in the socket dir.
pub fn enumerate_tunnels() -> Vec<String> {
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
    info_file: PathBuf,
    lock_file: PathBuf,
    dest_file: PathBuf,
    ssh_opts_file: PathBuf,
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

        // Only touch the sidecar files if we still own the inode at the lock
        // path. Two supervisors can end up concurrently alive when the lock
        // file is unlinked out from under one of them (external `rm`, a /tmp
        // sweeper, or a pre-fix racy cleanup): the loser keeps its flock on a
        // *deleted* inode while a fresh `tunnel-create` O_CREATs a new one and
        // becomes the real owner. If the loser's Drop blindly `remove_file`s
        // the path, it yanks the winner's fresh lock file and strands it with
        // `is_lock_held() == false` -- which the client interprets as
        // "tunnel destroyed, give up" instead of "transient, retry".
        //
        // Ordering once we've confirmed ownership:
        //   1. Check + unlink *while still holding the flock*. A racer that
        //      O_CREATs the path after our unlink gets a brand-new inode and
        //      flocks it independently; there is never a window where the path
        //      points at an inode we're about to delete.
        //   2. Release the flock last. It only covers a deleted inode at that
        //      point, so releasing it is invisible to everyone else.
        let owns_lock = self
            ._lock
            .as_ref()
            .and_then(LockIdentity::of_held)
            .is_some_and(|id| id.matches_path(&self.lock_file));
        if owns_lock {
            let _ = std::fs::remove_file(&self.local_sock);
            let _ = std::fs::remove_file(&self.pid_file);
            let _ = std::fs::remove_file(&self.info_file);
            let _ = std::fs::remove_file(&self.dest_file);
            let _ = std::fs::remove_file(&self.ssh_opts_file);
            let _ = std::fs::remove_file(&self.lock_file);
        } else {
            warn!(
                lock_path = %self.lock_file.display(),
                "lock file was replaced or removed out from under us; \
                 leaving sidecar files for the current owner"
            );
        }
        let _ = self._lock.take();
    }
}

// ---------------------------------------------------------------------------
// Public API
// ---------------------------------------------------------------------------

pub struct ConnectOpts {
    pub destination: String,
    pub no_server_start: bool,
    /// SSH options used to run ssh -- the merged CLI + config set.
    pub ssh_options: Vec<String>,
    /// Just the CLI `-o` options (pre-merge), persisted so a restart /
    /// auto-start can replay them without double-counting config options.
    pub cli_ssh_options: Vec<String>,
    pub name: Option<String>,
    pub dry_run: bool,
    pub foreground: bool,
    pub ignore_version_mismatch: bool,
    pub isolate_control_path: bool,
    pub connect_timeout: u64,
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
        let ensure_cmd =
            remote_exec_command(&dest, remote_cmd, &opts.ssh_options, true, opts.connect_timeout);
        let tunnel_cmd = tunnel_command(
            &dest,
            &local_sock,
            "$REMOTE_SOCK",
            &opts.ssh_options,
            true,
            opts.isolate_control_path,
            opts.connect_timeout,
        );

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
            // Write PID immediately so `tunnel-destroy` can find us even
            // during the startup window (ensure_remote_ready + spawn_tunnel
            // + wait_for_socket can take tens of seconds on WAN links).
            // Previously the PID was written only after the socket came up;
            // disconnect saw lock-held-but-no-PID and failed with
            // "cannot read PID".
            let _ = std::fs::write(&pid_file, std::process::id().to_string());
            // Record our identity so `gritty doctor` can detect a stale
            // supervisor (binary rebuilt after we daemonized). This is the
            // only way to catch it -- the supervisor is a pure byte proxy,
            // so handshake version checks never see its code at all.
            let _ = crate::runinfo::RunInfo::current()
                .write(&crate::runinfo::connect_info_path(&connection_name));
            lock
        }
        Err(_) => {
            // Another process holds the lock -- tunnel is alive or starting.
            let sock_exists = std::os::unix::net::UnixStream::connect(&local_sock).is_ok();
            let pid_hint = read_pid_hint(&connection_name);
            // If someone else holds the lock but the socket isn't up yet,
            // wait for it before claiming success. auto_start() relies on
            // "tunnel-create returned 0 ==> socket is ready to connect"
            // (util.rs:connect_or_start). Previously we signaled ready
            // immediately, so parallel invocations could race: one bound
            // the lock, the other saw the lock held and returned 0 before
            // the socket existed, causing connect_or_start to bail.
            if !sock_exists {
                match pid_hint {
                    Some(pid) => eprintln!(
                        "\x1b[2;33m\u{25b8} tunnel {connection_name} starting (pid {pid})\x1b[0m"
                    ),
                    None => {
                        eprintln!("\x1b[2;33m\u{25b8} tunnel {connection_name} starting\x1b[0m")
                    }
                }
                // Race the socket-up wait against the peer supervisor
                // dying during its own startup. Without this second arm,
                // a peer that crashes after we've observed its lock would
                // leave us polling the socket path for the full deadline
                // before giving up. `is_lock_held` releases its probe
                // flock immediately, so polling it is cheap.
                let deadline = socket_wait_deadline(opts.connect_timeout);
                let peer_watch = async {
                    loop {
                        tokio::time::sleep(Duration::from_millis(500)).await;
                        if !is_lock_held(&lock_path) {
                            return;
                        }
                    }
                };
                tokio::select! {
                    r = wait_for_socket(&local_sock, deadline) => {
                        if let Err(e) = r {
                            bail!(
                                "another tunnel-create for {connection_name} is in progress but its socket never came up: {e}"
                            );
                        }
                    }
                    _ = peer_watch => {
                        bail!(
                            "another tunnel-create for {connection_name} released its lock before the socket came up; retry this command"
                        );
                    }
                }
                println!("{}", local_sock.display());
                match pid_hint {
                    Some(pid) => eprintln!(
                        "\x1b[32m\u{25b8} tunnel {connection_name} ready (pid {pid})\x1b[0m"
                    ),
                    None => eprintln!("\x1b[32m\u{25b8} tunnel {connection_name} ready\x1b[0m"),
                }
            } else {
                println!("{}", local_sock.display());
                match pid_hint {
                    Some(pid) => eprintln!(
                        "\x1b[32m\u{25b8} tunnel {connection_name} already running (pid {pid})\x1b[0m"
                    ),
                    None => eprintln!(
                        "\x1b[32m\u{25b8} tunnel {connection_name} already running\x1b[0m"
                    ),
                }
            }
            signal_ready(&ready_fd);
            return Ok(0);
        }
    };

    // 3. Install SIGTERM/SIGINT handlers *before* any slow step so a signal
    //    during ensure_remote_ready / spawn_tunnel / wait_for_socket no
    //    longer terminates us via the default handler -- which would skip
    //    ConnectGuard::drop and leave .sock/.pid/.lock behind for the next
    //    invocation to clean up. The handlers are queued until we poll
    //    them in the main select below.
    let mut sigterm = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())?;
    let mut sigint = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::interrupt())?;

    // 4. Ensure remote server is running and get socket path. If we have a
    //    cached path from a prior connect, use it and skip the ~2s SSH-exec;
    //    the post-wait_for_socket probe below falls back to the full
    //    ensure_remote_ready if the cache turns out to be stale or the
    //    remote daemon isn't running.
    let remote_sock_cache = connect_remote_sock_path(&connection_name);
    let cached_remote_sock = std::fs::read_to_string(&remote_sock_cache)
        .ok()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty());
    let used_cache = cached_remote_sock.is_some();
    let (remote_sock, remote_version) = match cached_remote_sock {
        Some(sock) => {
            info!(remote_sock = %sock, "using cached remote socket path");
            (sock, None)
        }
        None => {
            ensure_remote_ready(
                &dest,
                opts.no_server_start,
                &opts.ssh_options,
                opts.foreground,
                opts.connect_timeout,
            )
            .await?
        }
    };
    info!(remote_sock, ?remote_version, "remote socket path");

    // Check protocol version compatibility. Only the slow path
    // (ensure_remote_ready) reports a version here; the cached fast path has
    // `remote_version == None` and is gated later against the version
    // observed by probe_tunnel_alive.
    if let Some(rv) = remote_version {
        check_remote_protocol_version(rv, opts.ignore_version_mismatch)?;
    }

    // Snapshot the inode we flock'd so the monitor can detect the lock file
    // being replaced or removed out from under us. See `LockIdentity`.
    let lock_identity = LockIdentity::of_held(&lock_fd);

    // 5. Spawn SSH tunnel
    let child = spawn_tunnel(
        &dest,
        &local_sock,
        &remote_sock,
        &opts.ssh_options,
        opts.foreground,
        opts.isolate_control_path,
        opts.connect_timeout,
    )
    .await?;
    let stop = tokio_util::sync::CancellationToken::new();

    let mut guard = ConnectGuard {
        child: Some(child),
        local_sock: local_sock.clone(),
        pid_file: pid_file.clone(),
        info_file: crate::runinfo::connect_info_path(&connection_name),
        lock_file: lock_path,
        dest_file: dest_file.clone(),
        ssh_opts_file: connect_ssh_opts_path(&connection_name),
        _lock: Some(lock_fd),
        stop: stop.clone(),
    };

    // 6. Wait for local socket to become connectable (race against child exit).
    // SSH stderr is already draining to our own stderr (-> .out in daemonized
    // mode), so both the timeout arm and the child-exit arm point the user at
    // that output rather than trying to re-read the pipe here.
    let mut child = guard.child.take().unwrap();
    let diag = format_ssh_diag(&dest, &opts.ssh_options, opts.foreground, opts.connect_timeout);
    let fg_hint = if opts.foreground {
        String::new()
    } else {
        format!(
            "\n  ssh output: {}\n  if SSH needs a password or host key accept, use: gritty tunnel-create --foreground {}",
            connect_out_path(&connection_name).display(),
            opts.destination,
        )
    };
    tokio::select! {
        result = wait_for_socket(&local_sock, socket_wait_deadline(opts.connect_timeout)) => {
            result.with_context(|| {
                format!("ssh is running but never bound the -L forward\n  to diagnose: {diag}{fg_hint}")
            })?;
            guard.child = Some(child);
        }
        status = child.wait() => {
            let status = status.context("failed to wait on ssh tunnel")?;
            bail!("ssh tunnel exited ({status})\n  to diagnose: {diag}{fg_hint}");
        }
    }
    info!("tunnel socket ready");

    // Verify the forward actually reaches a live remote daemon, and learn the
    // remote's protocol version from the probe handshake. On the cached-path
    // fast path we skipped ensure_remote_ready, so the remote server may not
    // be running yet (first connect after remote reboot) or the cached path
    // may be stale. On the slow path this is a cheap belt-and-suspenders check.
    let probed_version = match probe_tunnel_alive(&local_sock).await {
        Ok(v) => v,
        Err(why) => {
            if used_cache {
                info!(why, "cached remote socket unreachable; running ensure_remote_ready");
                let (fresh_sock, _) = ensure_remote_ready(
                    &dest,
                    opts.no_server_start,
                    &opts.ssh_options,
                    opts.foreground,
                    opts.connect_timeout,
                )
                .await?;
                if fresh_sock != remote_sock {
                    // Stale cache (remote UID / runtime dir changed). Our -L is
                    // aimed at the wrong path; invalidate and error -- a retry
                    // will take the slow path with the fresh value.
                    let _ = std::fs::remove_file(&remote_sock_cache);
                    bail!(
                        "cached remote socket path is stale ({remote_sock} -> {fresh_sock}); retry"
                    );
                }
                // Server was down; ensure_remote_ready started it. The existing
                // -L connects on-demand, so no respawn needed -- just re-probe.
                match probe_tunnel_alive(&local_sock).await {
                    Ok(v) => v,
                    Err(why) => bail!("remote daemon unreachable after start: {why}"),
                }
            } else {
                bail!("tunnel forward bound but remote daemon is not responding: {why}");
            }
        }
    };

    // The cached fast path skipped the version gate above (remote_version was
    // None). The probe just completed a real handshake with the remote daemon
    // through the tunnel, so apply the same gate to what it reported --
    // otherwise a cached connect to a freshly-upgraded remote would succeed
    // silently, defeating the fail-fast diagnostic the slow path provides.
    if used_cache {
        check_remote_protocol_version(probed_version, opts.ignore_version_mismatch)?;
    }
    let _ = std::fs::write(&remote_sock_cache, &remote_sock);

    // PID is already written (right after we got the lock, above). Record
    // the original destination and the CLI -o options so `restart` /
    // auto-start can recover them.
    let _ = std::fs::write(&dest_file, &opts.destination);
    let ssh_opts_file = connect_ssh_opts_path(&connection_name);
    if opts.cli_ssh_options.is_empty() {
        let _ = std::fs::remove_file(&ssh_opts_file);
    } else {
        let _ = std::fs::write(&ssh_opts_file, opts.cli_ssh_options.join("\n"));
    }

    // 7. Signal readiness to parent (or print if foreground)
    signal_ready(&ready_fd);

    // 8. Hand off the child to the tunnel monitor background task
    let original_child = guard.child.take().unwrap();
    let tunnel_span = tracing::info_span!("tunnel", name = %connection_name);
    let mut monitor_handle = tokio::spawn(
        tunnel_monitor(
            original_child,
            dest,
            local_sock.clone(),
            remote_sock,
            opts.ssh_options,
            opts.foreground,
            opts.no_server_start,
            opts.isolate_control_path,
            opts.connect_timeout,
            guard.lock_file.clone(),
            lock_identity,
            stop.clone(),
        )
        .instrument(tunnel_span),
    );

    // 9. Wait for signal or monitor death. Signal handlers were installed
    // back at step 3 so a signal during setup doesn't bypass cleanup.
    let monitor_done = tokio::select! {
        _ = sigterm.recv() => false,
        _ = sigint.recv() => false,
        res = &mut monitor_handle => { let _ = res; true }
    };

    // 10. Cleanup: cancel the monitor and wait for it to reap the SSH child
    // before we exit(0), otherwise the child.kill() races process teardown.
    stop.cancel();
    if !monitor_done {
        let _ = monitor_handle.await;
    }
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
    // `cleanup_if_unheld` is the race-safe "acquire flock, sweep, unlink lock"
    // primitive. If it succeeds, nothing was holding the lock; if it fails,
    // something live holds it and we must SIGTERM it. A plain `is_lock_held`
    // probe followed by `cleanup_stale_files` would have a TOCTOU window
    // where a fresh `tunnel-create` could slip in between the probe and the
    // unlink and get its lock file yanked.
    if cleanup_if_unheld(name) {
        eprintln!("\x1b[2;33m\u{25b8} tunnel {name} already stopped\x1b[0m");
        return Ok(());
    }

    // Read PID and send SIGTERM (let the process handle graceful shutdown)
    let pid_file = connect_pid_path(name);
    let pid: i32 = std::fs::read_to_string(&pid_file)
        .ok()
        .and_then(|s| s.trim().parse::<u32>().ok())
        .map(|p| p as i32)
        .ok_or_else(|| anyhow::anyhow!("cannot read PID for tunnel {name}"))?;

    unsafe {
        libc::kill(pid, libc::SIGTERM);
    }

    // Poll for up to 2s for the supervisor to release the lock and clean up
    // its own files via ConnectGuard::drop. Once `cleanup_if_unheld` succeeds
    // the lock is genuinely free and any remaining files are stragglers.
    let lock_path = connect_lock_path(name);
    let deadline = Instant::now() + Duration::from_secs(2);
    loop {
        tokio::time::sleep(Duration::from_millis(100)).await;
        if cleanup_if_unheld(name) {
            eprintln!("\x1b[32m\u{25b8} tunnel {name} stopped\x1b[0m");
            return Ok(());
        }
        if Instant::now() >= deadline {
            break;
        }
    }

    // Still alive after timeout -- escalate to SIGKILL + killpg.
    if is_lock_held(&lock_path) {
        unsafe {
            libc::kill(pid, libc::SIGKILL);
            libc::killpg(pid, libc::SIGTERM);
        }
    }
    tokio::time::sleep(Duration::from_millis(100)).await;
    // SIGKILL means ConnectGuard::drop never ran -- force the sweep. If the
    // process is somehow still holding the flock (kill raced), skip rather
    // than risk yanking a lock out from under a live supervisor.
    if cleanup_if_unheld(name) {
        eprintln!("\x1b[32m\u{25b8} tunnel {name} killed\x1b[0m");
    } else {
        eprintln!(
            "\x1b[33m\u{25b8} tunnel {name} still holding its lock after SIGKILL; \
             files left in place\x1b[0m"
        );
    }
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
            // Race-safe: takes the flock before touching anything. If a
            // supervisor slipped in between `probe_tunnel_status` and here,
            // this is a no-op instead of yanking its fresh lock file.
            cleanup_if_unheld(name);
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
            log_path: connect_log_path(name),
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
    fn quote_install_dir_default_expands_tilde() {
        // The shipped default must still resolve $HOME on the remote.
        assert_eq!(quote_remote_install_dir("~/.local/bin"), "\"$HOME\"/.local/bin");
        assert_eq!(quote_remote_install_dir("~"), "\"$HOME\"");
    }

    #[test]
    fn quote_install_dir_absolute_is_quoted() {
        assert_eq!(quote_remote_install_dir("/opt/gritty/bin"), "/opt/gritty/bin");
    }

    #[test]
    fn quote_install_dir_with_space_is_safe() {
        // A space must not split the GRITTY_INSTALL_DIR assignment.
        let q = quote_remote_install_dir("/opt/gritty tools/bin");
        assert!(!q.contains("tools/bin sh"), "unquoted space: {q}");
        assert_eq!(q, "'/opt/gritty tools/bin'");
        // Tilde form with a space: $HOME stays live, the rest is quoted.
        assert_eq!(quote_remote_install_dir("~/my tools"), "\"$HOME\"/'my tools'");
    }

    #[test]
    fn quote_install_dir_neutralizes_metacharacters() {
        // Command substitution in the path must be inert -- single-quoted,
        // so the remote shell treats `$(...)` as literal characters.
        assert_eq!(quote_remote_install_dir("/opt/$(touch pwned)"), "'/opt/$(touch pwned)'");
    }

    #[test]
    fn build_tunnel_recreate_args_without_options() {
        assert_eq!(
            build_tunnel_recreate_args("dev", "user@dev.example.com", &[]),
            vec!["tunnel-create", "--name", "dev", "user@dev.example.com"]
        );
    }

    #[test]
    fn build_tunnel_recreate_args_replays_each_option() {
        let opts = vec!["ProxyJump=bastion".to_string(), "Port=2222".to_string()];
        assert_eq!(
            build_tunnel_recreate_args("dev", "dev", &opts),
            vec![
                "tunnel-create",
                "--name",
                "dev",
                "-o",
                "ProxyJump=bastion",
                "-o",
                "Port=2222",
                "dev",
            ]
        );
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
            false,
            30,
        );
        let args: Vec<_> =
            cmd.as_std().get_args().map(|a| a.to_string_lossy().to_string()).collect();
        // From base_ssh_args
        assert!(args.contains(&"ConnectTimeout=30".to_string()));
        assert!(args.contains(&"BatchMode=yes".to_string()));
        // From TUNNEL_SSH_OPTS
        assert!(args.contains(&"ServerAliveInterval=3".to_string()));
        assert!(args.contains(&"StreamLocalBindUnlink=yes".to_string()));
        assert!(args.contains(&"ExitOnForwardFailure=yes".to_string()));
        assert!(!args.contains(&"ControlPath=none".to_string()));
        assert!(args.contains(&"ForwardAgent=no".to_string()));
        assert!(args.contains(&"ForwardX11=no".to_string()));
        // Tunnel flags and forward. Mux (non-isolated) mode keeps the remote
        // sleep so the mux client's lifetime tracks the forward.
        assert!(!args.contains(&"-N".to_string()));
        assert!(args.contains(&"-T".to_string()));
        assert!(args.contains(&"/tmp/local.sock:/run/user/1000/gritty/ctl.sock".to_string()));
        assert!(args.contains(&"user@host".to_string()));
        assert!(args.contains(&"exec sleep 2147483647".to_string()));
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
            false,
            30,
        );
        let args: Vec<_> =
            cmd.as_std().get_args().map(|a| a.to_string_lossy().to_string()).collect();
        assert!(args.contains(&"ProxyJump=bastion".to_string()));
        assert!(args.contains(&"-p".to_string()));
        assert!(args.contains(&"2222".to_string()));
    }

    #[test]
    fn tunnel_command_isolate_control_path() {
        let dest = Destination::parse("host").unwrap();
        let cmd = tunnel_command(
            &dest,
            Path::new("/tmp/local.sock"),
            "/tmp/remote.sock",
            &[],
            false,
            true,
            30,
        );
        let args: Vec<_> =
            cmd.as_std().get_args().map(|a| a.to_string_lossy().to_string()).collect();
        assert!(args.contains(&"ControlPath=none".to_string()));
        // Isolated (default) path uses -N -- no remote sleep to leak across
        // half-open drops.
        assert!(args.contains(&"-N".to_string()));
        assert!(!args.iter().any(|a| a.contains("sleep")));
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
        let cmd = tunnel_command(
            &dest,
            Path::new("/tmp/local.sock"),
            "$REMOTE_SOCK",
            &[],
            true,
            false,
            30,
        );
        let formatted = format_command(&cmd);
        assert!(formatted.contains("ServerAliveInterval=3"));
        assert!(!formatted.contains("ControlPath=none"));
        assert!(formatted.contains("ForwardAgent=no"));
        assert!(formatted.contains("-T"));
        assert!(formatted.contains("sleep 2147483647"));
        // Forward arg references $REMOTE_SOCK unquoted (no spaces, $ is safe)
        assert!(formatted.contains("/tmp/local.sock:$REMOTE_SOCK"));
        assert!(formatted.contains("user@host"));
    }

    #[test]
    fn format_command_remote_exec() {
        let dest = Destination::parse("user@host:2222").unwrap();
        let cmd = remote_exec_command(&dest, "gritty socket-path", &[], true, 30);
        let formatted = format_command(&cmd);
        assert!(formatted.starts_with("ssh "));
        assert!(formatted.contains("-p 2222"));
        assert!(formatted.contains("ConnectTimeout=30"));
        assert!(formatted.contains("user@host"));
        // The wrapped command should be single-quoted (contains spaces)
        assert!(formatted.contains(&format!("PATH=\"{REMOTE_PATH_PREFIX}\"")));
    }

    #[test]
    fn format_command_remote_exec_with_extra_opts() {
        let dest = Destination::parse("user@host").unwrap();
        let cmd = remote_exec_command(
            &dest,
            REMOTE_ENSURE_CMD,
            &["ProxyJump=bastion".to_string()],
            true,
            30,
        );
        let formatted = format_command(&cmd);
        assert!(formatted.contains("ProxyJump=bastion"));
        assert!(formatted.contains("gritty socket-path"));
        assert!(formatted.contains("gritty server"));
    }

    #[test]
    fn base_ssh_args_foreground() {
        let dest = Destination::parse("user@host:2222").unwrap();
        let args = base_ssh_args(&dest, &["ProxyJump=bastion".into()], true, 30);
        assert!(args.contains(&"-p".to_string()));
        assert!(args.contains(&"2222".to_string()));
        assert!(args.contains(&"ProxyJump=bastion".to_string()));
        assert!(args.contains(&"ConnectTimeout=30".to_string()));
        assert!(!args.contains(&"BatchMode=yes".to_string()));
    }

    #[test]
    fn base_ssh_args_background() {
        let dest = Destination::parse("host").unwrap();
        let args = base_ssh_args(&dest, &[], false, 30);
        assert!(args.contains(&"ConnectTimeout=30".to_string()));
        assert!(args.contains(&"BatchMode=yes".to_string()));
        assert!(!args.contains(&"-p".to_string()));
    }

    #[test]
    fn base_ssh_args_zero_timeout() {
        let dest = Destination::parse("host").unwrap();
        let args = base_ssh_args(&dest, &[], false, 0);
        assert!(!args.iter().any(|a| a.starts_with("ConnectTimeout")));
        assert!(args.contains(&"BatchMode=yes".to_string()));
    }

    #[test]
    fn socket_wait_deadline_values() {
        assert_eq!(socket_wait_deadline(0), Duration::from_secs(60));
        assert_eq!(socket_wait_deadline(30), Duration::from_secs(40));
        assert_eq!(socket_wait_deadline(3), Duration::from_secs(15));
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
    fn lock_identity_detects_replaced_inode() {
        let dir = tempfile::tempdir().unwrap();
        let lock_path = dir.path().join("test.lock");

        // Supervisor A acquires the lock.
        let a = try_acquire_lock(&lock_path).unwrap();
        let a_id = LockIdentity::of_held(&a).unwrap();
        assert!(a_id.matches_path(&lock_path), "A should own the path it just created");
        assert!(lock_still_owned(Some(a_id), &lock_path));

        // Someone unlinks the lock path out from under A (the root-cause
        // scenario: external rm, /tmp sweeper, or a pre-fix racy cleanup).
        std::fs::remove_file(&lock_path).unwrap();
        assert!(!a_id.matches_path(&lock_path), "A's inode is gone; it no longer owns the path");
        assert!(!lock_still_owned(Some(a_id), &lock_path));

        // Supervisor B acquires a *fresh* inode at the same path.
        let b = try_acquire_lock(&lock_path).unwrap();
        let b_id = LockIdentity::of_held(&b).unwrap();
        assert_ne!(a_id, b_id, "fresh O_CREAT must produce a different inode");
        assert!(!a_id.matches_path(&lock_path), "A still does not own the path");
        assert!(b_id.matches_path(&lock_path), "B is the real owner");

        // The critical invariant: A dropping must not disturb B's lock file.
        // ConnectGuard::drop gates `remove_file` on `matches_path`, so A's
        // drop is a no-op here and B's lock file survives.
        assert!(is_lock_held(&lock_path), "B's flock is observable via the path");
        drop(a);
        assert!(is_lock_held(&lock_path), "B's flock must survive A releasing its ghost lock");
        drop(b);
        assert!(!is_lock_held(&lock_path));
    }

    #[test]
    fn lock_still_owned_treats_unknown_as_owned() {
        // A failed startup fstat must not cause the supervisor to self-destruct.
        let dir = tempfile::tempdir().unwrap();
        let missing = dir.path().join("never-created.lock");
        assert!(lock_still_owned(None, &missing));
    }

    #[test]
    fn cleanup_if_unheld_skips_live_lock() {
        // Use the real socket_dir() so the connect_*_path helpers line up;
        // pick a name that can't collide with a real tunnel.
        let name = "test-cleanup-live-xyz";
        let lock_path = connect_lock_path(name);
        let pid_path = connect_pid_path(name);
        let _ = std::fs::create_dir_all(lock_path.parent().unwrap());
        std::fs::write(&pid_path, "1").unwrap();
        let lock = try_acquire_lock(&lock_path).unwrap();

        // A live supervisor holds the flock -- cleanup must refuse.
        assert!(!cleanup_if_unheld(name));
        assert!(lock_path.exists(), "live lock file must not be removed");
        assert!(pid_path.exists(), "sidecar files of a live supervisor must not be removed");

        // Once the lock is released, cleanup proceeds.
        drop(lock);
        assert!(cleanup_if_unheld(name));
        assert!(!lock_path.exists());
        assert!(!pid_path.exists());
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
                String::new(),
                vec![],
                false,
                true,
                false,
                30,
                PathBuf::from("/tmp/nonexistent.lock"),
                None,
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
                String::new(),
                vec![],
                false,
                true,
                false,
                30,
                PathBuf::from("/tmp/nonexistent.lock"),
                None,
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
                String::new(),
                vec![],
                false,
                true,
                false,
                30,
                PathBuf::from("/tmp/nonexistent.lock"),
                None,
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
        let err = wait_for_socket(&sock_path, Duration::from_secs(1)).await.unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("timed out after 1s"), "msg: {msg}");
        assert!(msg.contains("not found") || msg.contains("No such file"), "msg: {msg}");
    }

    // -----------------------------------------------------------------------
    // classify_exit / respawn_sleep / double_backoff (pure helpers)
    // -----------------------------------------------------------------------

    /// Build an `ExitStatus` with a given exit code (Unix wait status
    /// encoding: exit code in bits 8..=15).
    fn status_with_code(code: i32) -> std::process::ExitStatus {
        use std::os::unix::process::ExitStatusExt;
        std::process::ExitStatus::from_raw(code << 8)
    }

    /// Build an `ExitStatus` that reports death-by-signal (signal in
    /// bits 0..=6 of the wait status).
    fn status_killed_by_signal(signal: i32) -> std::process::ExitStatus {
        use std::os::unix::process::ExitStatusExt;
        std::process::ExitStatus::from_raw(signal)
    }

    #[test]
    fn classify_exit_ssh_255_is_transient() {
        assert_eq!(classify_exit(status_with_code(255)), ExitClass::Transient);
    }

    #[test]
    fn classify_exit_local_signal_kill_is_transient() {
        // code() == None -- typical of our own child.kill() after probe fail
        assert_eq!(classify_exit(status_killed_by_signal(9)), ExitClass::Transient);
        assert_eq!(classify_exit(status_killed_by_signal(15)), ExitClass::Transient);
    }

    #[test]
    fn classify_exit_remote_signal_range_is_transient() {
        // ssh reports remote signal death as 128 + signum
        for code in 128..=159 {
            assert_eq!(
                classify_exit(status_with_code(code)),
                ExitClass::Transient,
                "code {code} should be transient"
            );
        }
    }

    #[test]
    fn classify_exit_auth_config_codes_are_nontransient() {
        // Genuine failures we don't want to retry-hammer
        for code in [1, 2, 5, 127, 160, 254] {
            assert_eq!(
                classify_exit(status_with_code(code)),
                ExitClass::NonTransient,
                "code {code} should be non-transient"
            );
        }
    }

    #[test]
    fn respawn_sleep_resets_after_healthy_child() {
        // Child lived 45s > HEALTHY_CHILD_THRESHOLD (30s): sleep resets to MIN
        let sleep = respawn_sleep(Duration::from_secs(16), Duration::from_secs(45));
        assert_eq!(sleep, MIN_RESPAWN_BACKOFF);
    }

    #[test]
    fn respawn_sleep_keeps_growing_after_unhealthy_child() {
        let sleep = respawn_sleep(Duration::from_secs(8), Duration::from_secs(5));
        assert_eq!(sleep, Duration::from_secs(8));
    }

    #[test]
    fn respawn_sleep_threshold_is_inclusive() {
        // exactly HEALTHY_CHILD_THRESHOLD: counts as healthy
        let sleep = respawn_sleep(Duration::from_secs(16), HEALTHY_CHILD_THRESHOLD);
        assert_eq!(sleep, MIN_RESPAWN_BACKOFF);
        // just under threshold: unhealthy, keeps current backoff
        let sleep = respawn_sleep(
            Duration::from_secs(16),
            HEALTHY_CHILD_THRESHOLD - Duration::from_millis(1),
        );
        assert_eq!(sleep, Duration::from_secs(16));
    }

    #[test]
    fn double_backoff_doubles_and_caps() {
        assert_eq!(double_backoff(MIN_RESPAWN_BACKOFF), Duration::from_secs(2));
        assert_eq!(double_backoff(Duration::from_secs(8)), Duration::from_secs(16));
        assert_eq!(double_backoff(MAX_RESPAWN_BACKOFF), MAX_RESPAWN_BACKOFF);
    }

    #[test]
    fn double_backoff_climbs_from_zero() {
        // The we_killed path sleeps for ZERO; the backoff must still advance
        // to a full MIN step, not collapse to zero (the divergence that the
        // discarded next_backoff second return would have reintroduced).
        assert_eq!(double_backoff(Duration::ZERO), Duration::from_secs(2));
    }

    // --- ensure_remote_ready output parsing -------------------------------

    #[test]
    fn parse_ensure_remote_two_lines() {
        let (sock, ver) = parse_ensure_remote_output("/run/user/1000/gritty/ctl.sock\n20").unwrap();
        assert_eq!(sock, "/run/user/1000/gritty/ctl.sock");
        assert_eq!(ver, Some(20));
    }

    #[test]
    fn parse_ensure_remote_socket_only() {
        // Older remotes that don't emit a version line.
        let (sock, ver) = parse_ensure_remote_output("/tmp/gritty/ctl.sock").unwrap();
        assert_eq!(sock, "/tmp/gritty/ctl.sock");
        assert_eq!(ver, None);
    }

    #[test]
    fn parse_ensure_remote_garbage_version_ignored() {
        let (sock, ver) = parse_ensure_remote_output("/tmp/ctl.sock\nnot-a-number").unwrap();
        assert_eq!(sock, "/tmp/ctl.sock");
        assert_eq!(ver, None);
    }

    #[test]
    fn parse_ensure_remote_empty_fails() {
        let err = parse_ensure_remote_output("").unwrap_err();
        assert!(err.to_string().contains("empty socket path"), "{err}");
    }

    #[test]
    fn parse_ensure_remote_err_line_surfaces_reason() {
        let out = "/run/user/1000/gritty/ctl.sock\n20\nERR:error: protocol version mismatch: local=20 remote=19; run `gritty restart`";
        let err = parse_ensure_remote_output(out).unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("protocol version mismatch"), "{msg}");
        assert!(msg.contains("gritty restart"), "{msg}");
        assert!(msg.contains("remote daemon is unusable"), "{msg}");
    }

    #[test]
    fn parse_ensure_remote_err_line_with_no_reason() {
        let err = parse_ensure_remote_output("/tmp/ctl.sock\n20\nERR:").unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("remote daemon is unusable"), "{msg}");
        // Don't emit a dangling colon when the reason is empty.
        assert!(!msg.contains(": \n"), "{msg}");
    }

    #[test]
    fn parse_ensure_remote_trailing_non_err_line_ignored() {
        // A spurious third line without the ERR: prefix should not be treated
        // as an error -- it's probably a shell profile leaking into stdout.
        let (sock, ver) = parse_ensure_remote_output("/tmp/ctl.sock\n20\nsome noise").unwrap();
        assert_eq!(sock, "/tmp/ctl.sock");
        assert_eq!(ver, Some(20));
    }

    #[test]
    fn version_gate_accepts_matching_version() {
        assert!(check_remote_protocol_version(crate::protocol::PROTOCOL_VERSION, false).is_ok());
    }

    #[test]
    fn version_gate_rejects_mismatch() {
        let rv = crate::protocol::PROTOCOL_VERSION.wrapping_add(1);
        let err = check_remote_protocol_version(rv, false).unwrap_err().to_string();
        assert!(err.contains(&rv.to_string()), "message names remote version: {err}");
        assert!(
            err.contains(&crate::protocol::PROTOCOL_VERSION.to_string()),
            "message names local version: {err}"
        );
        assert!(err.contains("--ignore-version-mismatch"), "message is actionable: {err}");
    }

    #[test]
    fn version_gate_ignore_flag_downgrades_to_warn() {
        let rv = crate::protocol::PROTOCOL_VERSION.wrapping_add(1);
        // With the escape hatch set, a mismatch must not bail.
        assert!(check_remote_protocol_version(rv, true).is_ok());
    }
}
