use std::path::{Path, PathBuf};

use super::AutoStart;

/// Split a `host[:session]` target string on the first `:`, with no alias
/// resolution. Only for strings whose host part is already canonical (e.g.
/// rebuilt by `resolve_target_session`); everything else goes through
/// [`parse_target`].
pub(crate) fn split_target(s: &str) -> (String, Option<String>) {
    match s.split_once(':') {
        Some((host, session)) if !session.is_empty() => {
            (host.to_string(), Some(session.to_string()))
        }
        Some((host, _)) => (host.to_string(), None),
        None => (s.to_string(), None),
    }
}

/// Parse a `host[:session]` target string: split on the first `:` and
/// canonicalize the host through `[host.*]` aliases
/// ([`gritty::config::ConfigFile::canonical_host`]) -- the single chokepoint
/// that makes `gritty connect FOO.BAR.COM:x` and `gritty connect FOO:x`
/// address the same tunnel.
pub(crate) fn parse_target(
    config: &gritty::config::ConfigFile,
    s: &str,
) -> (String, Option<String>) {
    let (host, session) = split_target(s);
    (config.canonical_host(&host), session)
}

/// Parse an optional `host[:session]` target into `(host, session)`,
/// defaulting the host to `local` when no target is given.
///
/// Shared by the commands that take an optional target (connect / tail) so
/// the omitted-host-means-local rule is written once. Commands where an
/// omitted host means "every known host" (ls / refresh) must not use this.
pub(crate) fn split_optional_target(
    config: &gritty::config::ConfigFile,
    target: Option<&str>,
) -> (String, Option<String>) {
    match target {
        Some(t) => parse_target(config, t),
        None => ("local".to_string(), None),
    }
}

/// Canonicalize an optional host-only target (prune / kill-server),
/// defaulting to `local` when omitted.
pub(crate) fn parse_host_or_local(
    config: &gritty::config::ConfigFile,
    target: Option<&str>,
) -> String {
    target.map_or_else(|| "local".to_string(), |t| parse_target(config, t).0)
}

pub(crate) fn resolve_ctl_path(
    ctl_socket: Option<PathBuf>,
    host: Option<&str>,
) -> anyhow::Result<PathBuf> {
    if let Some(p) = ctl_socket {
        return Ok(p);
    }
    match host {
        Some("local") => Ok(gritty::daemon::control_socket_path()),
        Some(h) => Ok(gritty::daemon::socket_dir().join(format!("connect-{h}.sock"))),
        None => anyhow::bail!(
            "specify a host (e.g. `local`, or a tunnel name) or use --ctl-socket <path>"
        ),
    }
}

/// A control connection whose version handshake is complete, kept open so the
/// caller can keep streaming on it (unlike the one-shot [`server_request`]).
pub(crate) type HandshakedConn =
    tokio_util::codec::Framed<tokio::net::UnixStream, gritty::protocol::FrameCodec>;

/// Connect to a daemon control socket and complete the version handshake,
/// returning the live `Framed` plus the [`gritty::HandshakeInfo`] (callers
/// such as `tail` need `server_id` for reconnect detection). A failed connect
/// maps to the standard "no server running" error. `check_version` mirrors the
/// [`server_request`] / [`server_request_any_version`] split.
pub(crate) async fn connect_handshaked(
    ctl_path: &Path,
    check_version: bool,
) -> anyhow::Result<(HandshakedConn, gritty::HandshakeInfo)> {
    use gritty::protocol::FrameCodec;
    use tokio_util::codec::Framed;

    let stream = gritty::security::connect_verified(ctl_path).await.map_err(|_| {
        anyhow::anyhow!("no server running (could not connect to {})", ctl_path.display())
    })?;
    let mut framed = Framed::new(stream, FrameCodec);
    let info = gritty::handshake(&mut framed, gritty::get_or_create_device_id()).await?;
    if check_version {
        gritty::require_matched_version(&info)?;
    }
    Ok((framed, info))
}

async fn server_request_inner(
    ctl_path: &Path,
    frame: gritty::protocol::Frame,
    check_version: bool,
) -> anyhow::Result<gritty::protocol::Frame> {
    use futures_util::{SinkExt, StreamExt};
    use gritty::protocol::Frame;

    let (mut framed, _info) = connect_handshaked(ctl_path, check_version).await?;
    framed.send(frame).await?;
    Frame::expect_from(framed.next().await)
}

/// Send a control frame to the server and return the response. Bails with an
/// actionable error if the peer's `PROTOCOL_VERSION` differs from ours --
/// every normal command wants matched versions. Use [`server_request_any_version`]
/// for the `kill-server` recovery path.
pub(crate) async fn server_request(
    ctl_path: &Path,
    frame: gritty::protocol::Frame,
) -> anyhow::Result<gritty::protocol::Frame> {
    server_request_inner(ctl_path, frame, true).await
}

/// Like `server_request`, but tolerates a protocol-version mismatch -- used
/// by `kill-server` and `restart` so a user upgrading one side can still
/// tear down the old daemon without falling back to SSH.
pub(crate) async fn server_request_any_version(
    ctl_path: &Path,
    frame: gritty::protocol::Frame,
) -> anyhow::Result<gritty::protocol::Frame> {
    server_request_inner(ctl_path, frame, false).await
}

/// Build the `auto_start` argument list for launching `gritty server`,
/// threading a `--ctl-socket` override through so the respawn lands on the same
/// socket the user asked for (not the default path).
pub(crate) fn server_auto_start_args(ctl_socket: Option<&str>) -> Vec<&str> {
    match ctl_socket {
        Some(s) => vec!["--ctl-socket", s, "server"],
        None => vec!["server"],
    }
}

/// Whether the CLI was invoked with `-v/--verbose`. Recorded once in `main` so
/// `auto_start` can propagate verbosity to daemons it self-spawns.
static VERBOSE: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

/// Record the `-v/--verbose` flag for later propagation by `auto_start`.
pub(crate) fn set_verbose(verbose: bool) {
    VERBOSE.store(verbose, std::sync::atomic::Ordering::Relaxed);
}

/// Run the current binary with the given args. Both `gritty server` and
/// `gritty tunnel-create <host>` self-daemonize and return after the socket is ready.
///
/// `--verbose` is forwarded to the spawned daemon when the current process was
/// started verbose, so `gritty -v connect/restart/refresh` produces debug logs
/// in the daemon it launches -- the place those failures actually originate.
pub(crate) fn auto_start(args: &[&str]) -> anyhow::Result<()> {
    let exe = std::env::current_exe().unwrap_or_else(|_| PathBuf::from("gritty"));
    let mut cmd = std::process::Command::new(&exe);
    cmd.args(args);
    if VERBOSE.load(std::sync::atomic::Ordering::Relaxed) {
        cmd.arg("--verbose");
    }
    let status = cmd.status()?;
    if !status.success() {
        anyhow::bail!("failed to start `gritty {}` (exit {})", args.join(" "), status);
    }
    Ok(())
}

/// Try to connect to the control socket. On failure, auto-start the
/// appropriate process and retry with a bounded loop (or indefinitely
/// with `--wait`).
/// Returns `(stream, auto_started)` where `auto_started` is true when the
/// server or tunnel had to be launched before connecting.
pub(crate) async fn connect_or_start(
    ctl_path: &Path,
    auto_start_mode: &AutoStart,
    wait: bool,
) -> anyhow::Result<(tokio::net::UnixStream, bool)> {
    let auto_started = match gritty::security::connect_verified(ctl_path).await {
        Ok(s) => {
            return Ok((s, false));
        }
        Err(_) => match auto_start_mode {
            AutoStart::Server => {
                eprintln!("\x1b[2;33m\u{25b8} starting server...\x1b[0m");
                // A concurrent `gritty connect` can race with us here: both
                // spawn `gritty server`, and one child exits nonzero because
                // the winner already bound ctl.sock. Don't bail on that
                // failure -- drop into the retry loop so we attach to the
                // racer's daemon if one came up.
                if let Err(e) = auto_start(&["server"]) {
                    eprintln!(
                        "\x1b[2;33m\u{25b8} auto-start failed ({e}); retrying connect in case another process started one\x1b[0m"
                    );
                }
                true
            }
            AutoStart::Tunnel { name: host, config_dest } => {
                // The connection name alone is not a valid SSH destination
                // when the user originally passed `user@host`, `host:port`,
                // or `--name <alias>`. Recover the original destination from
                // the `.dest` sidecar, falling back to the config-implied
                // destination (first `[host.<name>] aliases` entry), then
                // the name itself.
                let destination =
                    gritty::connect::resolve_destination(host, config_dest.as_deref());
                eprintln!("\x1b[2;33m\u{25b8} starting tunnel {host}...\x1b[0m");
                // Replay any persisted CLI -o options so a reboot/respawn
                // doesn't silently lose a ProxyJump/IdentityFile/Port.
                let recreate = gritty::connect::tunnel_recreate_args(host, &destination);
                let recreate: Vec<&str> = recreate.iter().map(String::as_str).collect();
                // Same rationale: `connect::run` returns Ok(0) when another
                // instance already holds the lock, so a tunnel-create race
                // is usually fine -- but if auto_start errors for any other
                // reason, still try to connect before giving up.
                if let Err(e) = auto_start(&recreate) {
                    eprintln!(
                        "\x1b[2;33m\u{25b8} auto-start failed ({e}); retrying connect in case another process started one\x1b[0m"
                    );
                }
                true
            }
            AutoStart::None if wait => false,
            AutoStart::None => {
                anyhow::bail!("no server running (could not connect to {})", ctl_path.display());
            }
        },
    };

    // Retry loop: bounded (10 retries, 500ms apart) or indefinite (--wait)
    let max_retries = if wait { u32::MAX } else { 10 };
    for _ in 0..max_retries {
        match gritty::security::connect_verified(ctl_path).await {
            Ok(s) => {
                return Ok((s, auto_started));
            }
            Err(_) => {
                if wait {
                    eprintln!("\x1b[2;33m\u{25b8} waiting for server...\x1b[0m");
                }
                tokio::time::sleep(std::time::Duration::from_millis(500)).await;
            }
        }
    }
    anyhow::bail!("server did not become ready ({})", ctl_path.display())
}

/// Compact duration: "12s", "5m", "3h", "2d".
pub(crate) fn format_duration(secs: u64) -> String {
    if secs < 60 {
        format!("{secs}s")
    } else if secs < 3600 {
        format!("{}m", secs / 60)
    } else if secs < 86400 {
        format!("{}h", secs / 3600)
    } else {
        format!("{}d", secs / 86400)
    }
}

pub(crate) fn format_age(now: u64, created_at: u64) -> String {
    format!("{} ago", format_duration(now.saturating_sub(created_at)))
}

/// Idle column value: compact time since `last_activity`, or "-" when the
/// timestamp is unknown (0 -- e.g. an older server that doesn't track it).
pub(crate) fn format_idle(now: u64, last_activity: u64) -> String {
    if last_activity == 0 {
        return "-".to_string();
    }
    format_duration(now.saturating_sub(last_activity))
}

/// Parse a compact duration into seconds: bare seconds ("90") or a number
/// with one of [`format_duration`]'s unit suffixes ("90s", "30m", "12h",
/// "7d") -- what `gritty ls` prints in the Idle column is valid input here.
pub(crate) fn parse_duration(s: &str) -> anyhow::Result<u64> {
    let err = || anyhow::anyhow!("invalid duration: {s:?} (use e.g. 90s, 30m, 12h, 7d)");
    let (number, multiplier) = match s.chars().last() {
        Some('s') => (&s[..s.len() - 1], 1),
        Some('m') => (&s[..s.len() - 1], 60),
        Some('h') => (&s[..s.len() - 1], 3600),
        Some('d') => (&s[..s.len() - 1], 86400),
        Some(c) if c.is_ascii_digit() => (s, 1),
        _ => return Err(err()),
    };
    let n: u64 = number.parse().map_err(|_| err())?;
    n.checked_mul(multiplier).ok_or_else(err)
}

pub(crate) fn format_timestamp(epoch_secs: u64) -> String {
    let Ok(ts) = jiff::Timestamp::from_second(epoch_secs as i64) else {
        return "-".to_string();
    };
    ts.to_zoned(jiff::tz::TimeZone::system()).strftime("%Y-%m-%d %H:%M:%S").to_string()
}

/// Parse a port spec: "PORT" or "LISTEN_PORT:TARGET_PORT".
///
/// Port 0 is rejected: gritty has no way to report the OS-assigned ephemeral
/// port back to the user (unlike `ssh -L 0:` which prints it), so a `0` forward
/// would be live on an undiscoverable port.
pub(crate) fn parse_port_spec(spec: &str) -> anyhow::Result<(u16, u16)> {
    fn port(field: &str, value: &str) -> anyhow::Result<u16> {
        let p: u16 = value.parse().map_err(|_| anyhow::anyhow!("invalid {field}: {value}"))?;
        if p == 0 {
            anyhow::bail!("{field} must not be 0 (ephemeral ports are not supported)");
        }
        Ok(p)
    }
    if let Some((a, b)) = spec.split_once(':') {
        Ok((port("listen port", a)?, port("target port", b)?))
    } else {
        let p = port("port", spec)?;
        Ok((p, p))
    }
}

/// Resolve a session target (numeric ID, name, or `-`) to its numeric ID.
pub(crate) async fn resolve_session_id(ctl_path: &Path, target: &str) -> anyhow::Result<u32> {
    use gritty::protocol::Frame;

    if let Ok(id) = target.parse::<u32>() {
        return Ok(id);
    }
    let Frame::SessionInfo { sessions } = server_request(ctl_path, Frame::ListSessions).await?
    else {
        anyhow::bail!("unexpected response to ListSessions");
    };
    if target == "-" {
        if let Some(e) = sessions.iter().find(|e| e.is_last_attached) {
            return Ok(e.id);
        }
        return sessions
            .iter()
            .max_by_key(|e| e.last_heartbeat)
            .map(|e| e.id)
            .ok_or_else(|| anyhow::anyhow!("no sessions (cannot resolve '-')"));
    }
    sessions
        .iter()
        .find(|e| e.name == target)
        .map(|e| e.id)
        .ok_or_else(|| anyhow::anyhow!("no such session: {target}"))
}

/// The one usage line for `lf`/`rf` errors -- single authoring point so the
/// zero-arg and bad-port-spec paths cannot drift apart.
fn forward_usage(alias: &str) -> String {
    format!("usage: gritty {alias} [host[:session]] <PORT|LISTEN:TARGET>")
}

/// Split the `lf`/`rf` positionals: with both args, `(Some(target), port)`;
/// with one arg it is the port spec and the target is discovered.
pub(crate) fn forward_args(
    alias: &str,
    target: Option<String>,
    port: Option<String>,
) -> anyhow::Result<(Option<String>, String)> {
    match (target, port) {
        (Some(t), Some(p)) => Ok((Some(t), p)),
        (Some(p), None) => Ok((None, p)),
        _ => anyhow::bail!("{}", forward_usage(alias)),
    }
}

/// Appended to "no client attached" errors when running inside a gritty
/// session, where these commands can never work: the forward socket lives on
/// the machine running `gritty connect`.
pub(crate) fn in_session_hint(in_session: bool) -> &'static str {
    if in_session {
        "\nhint: you're inside a gritty session -- run port forwards from the machine running `gritty connect`, not from inside the session"
    } else {
        ""
    }
}

fn inside_session() -> bool {
    std::env::var_os("GRITTY_SESSION").is_some()
}

/// Hint for listen-port collisions: teach the LISTEN:TARGET form. Keyed on
/// the fwd-socket error strings authored in `client.rs` (plus a generic
/// "in use" so messages from other binary versions still match).
pub(crate) fn busy_port_hint(msg: &str, listen_port: u16, target_port: u16) -> Option<String> {
    let bind_failure = msg.contains(gritty::client::FWD_ERR_SERVER_BIND)
        || msg.contains(gritty::client::FWD_ERR_BIND_PREFIX)
        || msg.to_ascii_lowercase().contains("in use");
    if !bind_failure {
        return None;
    }
    let alt = if listen_port == u16::MAX { listen_port - 1 } else { listen_port + 1 };
    Some(format!(
        "\nhint: the listen port may be busy -- pick a free one with LISTEN:TARGET, e.g. `{alt}:{target_port}`"
    ))
}

/// A live forward socket in the socket dir: an attached `gritty connect`
/// client on this machine that can carry a port forward.
pub(crate) struct ForwardCandidate {
    pub(crate) path: PathBuf,
    pub(crate) host: String,
    pub(crate) session_id: u32,
}

/// Fallback display when the daemon cannot be asked for the session name.
fn candidate_fallback_label(c: &ForwardCandidate) -> String {
    format!("{} (session #{})", c.host, c.session_id)
}

/// Best-effort typeable label for a candidate: `host:session-name` (own
/// client prefix elided, so it round-trips as a CLI target) when the daemon
/// answers, else the untypeable-but-informative `host (session #id)`.
async fn forward_candidate_label(
    config: &gritty::config::ConfigFile,
    c: &ForwardCandidate,
) -> String {
    use gritty::protocol::Frame;

    let Ok(ctl_path) = resolve_ctl_path(None, Some(&c.host)) else {
        return candidate_fallback_label(c);
    };
    let Ok(Frame::SessionInfo { sessions }) = server_request(&ctl_path, Frame::ListSessions).await
    else {
        return candidate_fallback_label(c);
    };
    let Some(entry) = sessions.iter().find(|e| e.id == c.session_id) else {
        return candidate_fallback_label(c);
    };
    let client_name = config.resolve_session(Some(&c.host)).client_name;
    format!("{}:{}", c.host, gritty::naming::display_session_name(&entry.name, &client_name))
}

/// List the live forward sockets in `dir`, sorted by path. Liveness is probed
/// by connecting (the attached client treats connect-then-EOF as a no-op);
/// stale sockets left by crashed clients are skipped.
pub(crate) fn live_forward_sockets(dir: &Path) -> Vec<ForwardCandidate> {
    let mut live = Vec::new();
    if let Ok(entries) = std::fs::read_dir(dir) {
        for entry in entries.flatten() {
            let name = entry.file_name();
            let Some((host, session_id)) =
                name.to_str().and_then(gritty::client::parse_forward_socket_name)
            else {
                continue;
            };
            let path = entry.path();
            if std::os::unix::net::UnixStream::connect(&path).is_err() {
                continue; // stale socket from a crashed client
            }
            live.push(ForwardCandidate { host: host.to_string(), session_id, path });
        }
    }
    live.sort_by(|a, b| a.path.cmp(&b.path));
    live
}

/// A digit-shaped connection name collides with the one-arg port form
/// (`gritty rf 3000` where `3000` is a tunnel). Error rather than guess when
/// the lone arg also names a known tunnel or host alias.
fn ambiguous_host_guard(
    config: &gritty::config::ConfigFile,
    socket_dir: &Path,
    arg: &str,
    alias: &str,
) -> anyhow::Result<()> {
    let is_alias = config.canonical_host(arg) != arg;
    let is_tunnel = socket_dir.join(format!("connect-{arg}.sock")).exists();
    if is_alias || is_tunnel {
        anyhow::bail!(
            "'{arg}' is also a connection name -- pass an explicit port: gritty {alias} {arg} <port>"
        );
    }
    Ok(())
}

/// Implements `gritty lf`/`rf`. With a target, resolves the session name to
/// its numeric id via the daemon; without one, discovers the single attached
/// session by its forward socket. Then connects to fwd-{host}-{id}.sock,
/// sends the request, and blocks.
pub(crate) async fn port_forward_command(
    config: &gritty::config::ConfigFile,
    ctl_socket: Option<PathBuf>,
    target: Option<String>,
    port: Option<String>,
    direction: u8,
) -> anyhow::Result<()> {
    let alias = if direction == 0 { "lf" } else { "rf" };
    let (target, port) = forward_args(alias, target, port)?;
    let (listen_port, target_port) =
        parse_port_spec(&port).map_err(|e| anyhow::anyhow!("{e} ({})", forward_usage(alias)))?;

    let (fwd_path, label) = match target {
        Some(t) => {
            let (host, session) = parse_target(config, &t);
            let client_name = config.resolve_session(Some(&host)).client_name;
            let session = gritty::naming::resolve_session_name(
                session.as_deref().unwrap_or("0"),
                &client_name,
            );
            let ctl_path = resolve_ctl_path(ctl_socket, Some(&host))?;
            let session_id = resolve_session_id(&ctl_path, &session).await?;
            let display = gritty::naming::display_session_name(&session, &client_name);
            (
                gritty::client::forward_socket_path(&ctl_path, session_id),
                format!("{host}:{display}"),
            )
        }
        None => {
            let dir = match &ctl_socket {
                Some(p) => p
                    .parent()
                    .filter(|d| !d.as_os_str().is_empty())
                    .unwrap_or(Path::new("."))
                    .to_path_buf(),
                None => gritty::daemon::socket_dir(),
            };
            ambiguous_host_guard(config, &dir, &port, alias)?;
            let mut live = live_forward_sockets(&dir);
            match live.len() {
                1 => {
                    let c = live.swap_remove(0);
                    let label = forward_candidate_label(config, &c).await;
                    (c.path, label)
                }
                0 => anyhow::bail!(
                    "no attached session found (port forwards need an attached `gritty connect` client){}",
                    in_session_hint(inside_session())
                ),
                _ => {
                    let mut labels = Vec::with_capacity(live.len());
                    for c in &live {
                        labels.push(forward_candidate_label(config, c).await);
                    }
                    // Example must be typeable; fall back to a placeholder if
                    // the first label is an id-only form.
                    let example = labels.iter().find(|l| l.contains(':')).map(String::as_str);
                    anyhow::bail!(
                        "multiple attached sessions: {} -- specify a target, e.g. `gritty {alias} {} {port}`",
                        labels.join(", "),
                        example.unwrap_or("<host:session>")
                    )
                }
            }
        }
    };
    run_forward(&fwd_path, &label, direction, listen_port, target_port).await
}

/// Send the forward request over an attached client's forward socket and
/// block until Ctrl-C or teardown.
async fn run_forward(
    fwd_path: &Path,
    label: &str,
    direction: u8,
    listen_port: u16,
    target_port: u16,
) -> anyhow::Result<()> {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    let mut stream = tokio::net::UnixStream::connect(fwd_path).await.map_err(|_| {
        anyhow::anyhow!(
            "no client attached to {label} (could not connect to {}){}",
            fwd_path.display(),
            in_session_hint(inside_session())
        )
    })?;

    // Write: [direction][listen_port BE][target_port BE]
    let mut header = [0u8; 5];
    header[0] = direction;
    header[1..3].copy_from_slice(&listen_port.to_be_bytes());
    header[3..5].copy_from_slice(&target_port.to_be_bytes());
    stream.write_all(&header).await?;

    // Read response: 0x01 = success, 0x02 + message = error
    let mut resp = [0u8; 1];
    stream.read_exact(&mut resp).await?;
    if resp[0] == 0x02 {
        let mut msg = Vec::new();
        stream.read_to_end(&mut msg).await?;
        let msg = String::from_utf8_lossy(&msg);
        let hint = busy_port_hint(&msg, listen_port, target_port).unwrap_or_default();
        anyhow::bail!("{msg}{hint}");
    }
    if resp[0] != 0x01 {
        anyhow::bail!("unexpected response: 0x{:02x}", resp[0]);
    }

    let dir_str = if direction == 0 { "local" } else { "remote" };
    let port_str = if listen_port == target_port {
        format!("{listen_port}")
    } else {
        format!("{listen_port}:{target_port}")
    };
    eprintln!("\x1b[32m\u{25b8} {dir_str}-forward {port_str} active ({label})\x1b[0m");

    // Block until SIGINT or stream EOF (teardown on disconnect)
    let mut sigint = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::interrupt())?;
    let mut sigterm = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())?;
    let mut buf = [0u8; 1];
    tokio::select! {
        _ = sigint.recv() => {}
        _ = sigterm.recv() => {}
        _ = stream.read(&mut buf) => {}
    }
    Ok(())
}

/// One daemon endpoint discovered by [`discover_daemon_probes`].
pub(crate) struct DaemonProbe {
    pub(crate) host: String,
    pub(crate) socket: PathBuf,
    /// `(destination, status)` when this endpoint is reached through an SSH
    /// tunnel; `None` for the local daemon and orphaned socket files.
    pub(crate) tunnel: Option<(String, String)>,
}

/// Enumerate all reachable daemon sockets: local + tunnels + bare socket files.
pub(crate) fn discover_daemon_probes() -> Vec<DaemonProbe> {
    let mut probes = Vec::new();
    let local = gritty::daemon::control_socket_path();
    if local.exists() {
        probes.push(DaemonProbe { host: "local".to_string(), socket: local, tunnel: None });
    }
    let mut seen = std::collections::HashSet::new();
    for info in gritty::connect::get_tunnel_info() {
        if seen.insert(info.name.clone()) {
            probes.push(DaemonProbe {
                host: info.name.clone(),
                socket: gritty::connect::connection_socket_path(&info.name),
                tunnel: Some((info.destination, info.status)),
            });
        }
    }
    if let Ok(entries) = std::fs::read_dir(gritty::daemon::socket_dir()) {
        for entry in entries.flatten() {
            let name = entry.file_name().to_string_lossy().to_string();
            if let Some(stem) = name.strip_prefix("connect-").and_then(|s| s.strip_suffix(".sock"))
                && seen.insert(stem.to_string())
            {
                probes.push(DaemonProbe {
                    host: stem.to_string(),
                    socket: entry.path(),
                    tunnel: None,
                });
            }
        }
    }
    probes
}

/// Connect to the per-session svc socket (`$GRITTY_SOCK`).
/// `context` is appended to the "not set" error (e.g. `" with --forward-open"`).
fn connect_svc_socket(context: &str) -> std::os::unix::net::UnixStream {
    let sock_path = match std::env::var("GRITTY_SOCK") {
        Ok(p) => p,
        Err(_) => {
            eprintln!("error: GRITTY_SOCK not set (are you inside a gritty session{context}?)");
            std::process::exit(1);
        }
    };
    match std::os::unix::net::UnixStream::connect(&sock_path) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("error: could not connect to service socket ({sock_path}): {e}");
            std::process::exit(1);
        }
    }
}

/// Read stdin and send to client clipboard via svc socket.
pub(crate) fn clipboard_copy() {
    use std::io::{Read, Write};

    let mut data = Vec::new();
    if let Err(e) = std::io::stdin().read_to_end(&mut data) {
        eprintln!("error: reading stdin: {e}");
        std::process::exit(1);
    }
    let mut stream = connect_svc_socket("");
    if let Err(e) = stream
        .write_all(&[gritty::protocol::SvcRequest::Clipboard.to_byte()])
        .and_then(|_| stream.write_all(&[0x01]))
        .and_then(|_| stream.write_all(&data))
        // Half-close so the server's read_to_end sees EOF and can reply.
        .and_then(|_| stream.shutdown(std::net::Shutdown::Write))
    {
        eprintln!("error: clipboard copy failed: {e}");
        std::process::exit(1);
    }
    // Read the 1-byte delivery confirmation: 0x01 = set, 0x00 = dropped (no
    // attached client, or the client lacks clipboard support). An older server
    // sends nothing and closes -- read_exact then errors and we degrade to a
    // soft warning, matching `gritty open`.
    let _ = stream.set_read_timeout(Some(std::time::Duration::from_secs(2)));
    let mut resp = [0u8; 1];
    match stream.read_exact(&mut resp) {
        Ok(()) if resp[0] == 0x00 => {
            eprintln!(
                "error: clipboard not delivered -- no client is attached, or it lacks clipboard support"
            );
            std::process::exit(1);
        }
        Ok(()) => {}
        Err(_) => {
            eprintln!("warning: could not confirm clipboard was set (server may be older)");
        }
    }
}

/// Forward a URL to the attached client to open locally.
///
/// `is_browser_shim` is true when invoked as the `gritty-open` `$BROWSER`
/// helper. In that case a client that opted out of URL forwarding (or a
/// detached session) must NOT hard-fail the calling tool (`gh`, `cargo doc
/// --open`, ...): print the URL for manual opening and exit 0. The explicit
/// `gritty open <url>` command still reports an error and exits non-zero.
pub(crate) fn open_url(url: &str, is_browser_shim: bool) {
    use std::io::{Read, Write};

    let mut stream = connect_svc_socket(" with --forward-open");
    let _ = stream.write_all(&[gritty::protocol::SvcRequest::OpenUrl.to_byte()]);
    let _ = stream.write_all(url.as_bytes());
    let _ = stream.write_all(b"\n");

    // Read response byte: 0x01 = forwarded, 0x00 = no client
    let _ = stream.set_read_timeout(Some(std::time::Duration::from_secs(2)));
    let mut resp = [0u8; 1];
    match stream.read_exact(&mut resp) {
        Ok(()) if resp[0] == 0x00 => {
            if is_browser_shim {
                eprintln!(
                    "gritty: could not open URL in your browser (URL forwarding is off \
                           or no client is attached); open it manually:"
                );
                eprintln!("  {url}");
                // exit 0: do not break the tool that invoked $BROWSER.
            } else {
                eprintln!("error: no client is connected with --forward-open");
                std::process::exit(1);
            }
        }
        Ok(()) => {} // 0x01 or other = success
        Err(_) => {
            // Timeout or older server -- degrade gracefully
            eprintln!("warning: could not confirm URL was forwarded (server may be older)");
        }
    }
}

pub(crate) fn config_edit() -> anyhow::Result<()> {
    let path = gritty::config::config_path();
    if !path.exists() {
        if let Some(parent) = path.parent() {
            gritty::security::secure_create_dir_all(parent)?;
        }
        std::fs::write(&path, gritty::config::DEFAULT_CONFIG)?;
        eprintln!("created {}", path.display());
    }
    let editor = std::env::var("VISUAL")
        .or_else(|_| std::env::var("EDITOR"))
        .unwrap_or_else(|_| "vi".into());
    let (program, args) = parse_editor(&editor);
    let status = std::process::Command::new(&program).args(&args).arg(&path).status()?;
    if !status.success() {
        anyhow::bail!("{program} exited with {status}");
    }
    Ok(())
}

/// Split a `$VISUAL`/`$EDITOR` value into a program and its arguments so that
/// argument-bearing settings like `code --wait` or `emacsclient -nw` work --
/// `Command::new` execs the program verbatim with no word-splitting. Falls back
/// to `vi` when the value is empty or whitespace-only.
fn parse_editor(editor: &str) -> (String, Vec<String>) {
    // Unbalanced quotes (`None`) fall back to the raw string as one token --
    // no worse than the previous un-split behavior.
    let mut parts = shlex::split(editor).unwrap_or_else(|| vec![editor.to_string()]);
    parts.retain(|p| !p.is_empty());
    if parts.is_empty() {
        return ("vi".to_string(), Vec::new());
    }
    let program = parts.remove(0);
    (program, parts)
}

pub(crate) async fn info(ctl_socket: Option<PathBuf>) -> anyhow::Result<()> {
    use gritty::protocol::Frame;

    println!("gritty {}", env!("CARGO_PKG_VERSION"));
    println!();

    let cfg_path = gritty::config::config_path();
    // Re-parse strictly: a config rejected by deny_unknown_fields must not be
    // reported as "loaded" -- `config` here is the default-fallback value.
    let cfg_status = match gritty::config::config_status(&cfg_path) {
        gritty::config::ConfigStatus::NotFound => "not found".to_string(),
        gritty::config::ConfigStatus::Invalid(e) => format!("INVALID -- ignored: {e}"),
        gritty::config::ConfigStatus::Valid(cfg) if cfg.host.is_empty() => "loaded".to_string(),
        gritty::config::ConfigStatus::Valid(cfg) => {
            let n = cfg.host.len();
            let s = if n == 1 { "" } else { "s" };
            format!("loaded, {n} host{s}")
        }
    };
    println!("config:         {} ({cfg_status})", cfg_path.display());

    // A --ctl-socket override points `info` at a specific daemon; its log/pid
    // are siblings of that socket.
    let ctl_path = match &ctl_socket {
        Some(p) => canonicalize_or_raw(p.clone()),
        None => canonicalize_or_raw(gritty::daemon::socket_dir()).join("ctl.sock"),
    };
    let socket_dir =
        ctl_path.parent().map(Path::to_path_buf).unwrap_or_else(gritty::daemon::socket_dir);

    println!("socket dir:     {}", socket_dir.display());
    println!("server socket:  {}", ctl_path.display());

    let device_id = gritty::get_or_create_device_id();
    let device_id_path = gritty::device_id_path();
    println!("device id:      {device_id} ({})", device_id_path.display());

    // Probe server status via server_request (which includes handshake)
    let pid_path = gritty::daemon::pid_file_path(&ctl_path);
    let pid = std::fs::read_to_string(&pid_path).ok().and_then(|s| s.trim().parse::<u32>().ok());

    // Probe with the version-tolerant request: a running-but-mismatched daemon
    // must report as running (with the mismatch), not "not running".
    match server_request_any_version(&ctl_path, Frame::ListSessions).await {
        Ok(Frame::SessionInfo { sessions }) => {
            let n = sessions.len();
            let s = if n == 1 { "" } else { "s" };
            match pid {
                Some(p) => println!("server status:  running (pid {p}, {n} session{s})"),
                None => println!("server status:  running ({n} session{s})"),
            }
        }
        Ok(Frame::Error { code: gritty::protocol::ErrorCode::VersionMismatch, .. }) => {
            let pid_str = pid.map(|p| format!("pid {p}, ")).unwrap_or_default();
            println!(
                "server status:  running ({pid_str}protocol version mismatch -- run `gritty refresh`)"
            );
        }
        _ => {
            println!("server status:  not running");
        }
    }

    let log_path = socket_dir.join("daemon.log");
    let out_path = socket_dir.join("daemon.out");
    print_path("server log:    ", &log_path);
    print_path("server output: ", &out_path);

    // Tunnels
    let tunnels = gritty::connect::get_tunnel_info();
    if !tunnels.is_empty() {
        println!();
        println!("tunnels:");
        for t in &tunnels {
            let pid_str = match t.pid {
                Some(p) => format!(" (pid {p})"),
                None => String::new(),
            };
            println!("  {:<14}{}{pid_str}", t.name, t.status);
            print_path("                log:", &canonicalize_or_raw(t.log_path.clone()));
        }
    }

    Ok(())
}

pub(crate) fn print_path(label: &str, path: &Path) {
    if path.exists() {
        println!("{label} {}", path.display());
    } else {
        println!("{label} {} (not found)", path.display());
    }
}

/// Resolve symlinks in the path (e.g. /tmp -> /private/tmp on macOS).
pub(crate) fn canonicalize_or_raw(path: PathBuf) -> PathBuf {
    std::fs::canonicalize(&path).unwrap_or(path)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// An empty config: parse_target falls through to a raw split.
    fn no_aliases() -> gritty::config::ConfigFile {
        gritty::config::ConfigFile::default()
    }

    /// A config where `foo.bar.com` aliases the connection name `foo`.
    fn foo_alias() -> gritty::config::ConfigFile {
        toml::from_str("[host.foo]\naliases = [\"foo.bar.com\"]\n").unwrap()
    }

    #[test]
    fn parse_target_host_only() {
        let (host, session) = parse_target(&no_aliases(), "local");
        assert_eq!(host, "local");
        assert_eq!(session, None);
    }

    #[test]
    fn parse_target_host_and_session() {
        let (host, session) = parse_target(&no_aliases(), "local:work");
        assert_eq!(host, "local");
        assert_eq!(session, Some("work".to_string()));
    }

    #[test]
    fn parse_target_canonicalizes_alias_host() {
        let cfg = foo_alias();
        assert_eq!(parse_target(&cfg, "foo.bar.com:work"), parse_target(&cfg, "foo:work"));
        let (host, session) = parse_target(&cfg, "foo.bar.com:work");
        assert_eq!(host, "foo");
        assert_eq!(session, Some("work".to_string()));
        // The session part is never alias-resolved.
        let (host, session) = parse_target(&cfg, "local:foo.bar.com");
        assert_eq!(host, "local");
        assert_eq!(session, Some("foo.bar.com".to_string()));
    }

    // split_target is the raw core for pre-canonicalized strings -- it must
    // never alias-resolve, or transfer targets would be remapped twice.
    #[test]
    fn split_target_does_not_alias() {
        let (host, _) = split_target("foo.bar.com:work");
        assert_eq!(host, "foo.bar.com");
    }

    #[test]
    fn split_optional_target_none_defaults_to_local() {
        assert_eq!(split_optional_target(&no_aliases(), None), ("local".to_string(), None));
    }

    #[test]
    fn split_optional_target_splits_present_target() {
        let cfg = no_aliases();
        assert_eq!(
            split_optional_target(&cfg, Some("local:work")),
            ("local".to_string(), Some("work".to_string()))
        );
        assert_eq!(split_optional_target(&cfg, Some("local")), ("local".to_string(), None));
    }

    #[test]
    fn split_optional_target_canonicalizes_alias() {
        assert_eq!(
            split_optional_target(&foo_alias(), Some("foo.bar.com:0")),
            ("foo".to_string(), Some("0".to_string()))
        );
    }

    #[test]
    fn parse_host_or_local_none_defaults_to_local() {
        assert_eq!(parse_host_or_local(&no_aliases(), None), "local");
    }

    #[test]
    fn parse_host_or_local_canonicalizes_alias() {
        assert_eq!(parse_host_or_local(&foo_alias(), Some("foo.bar.com")), "foo");
    }

    #[test]
    fn parse_target_remote_and_id() {
        let (host, session) = parse_target(&no_aliases(), "devbox:0");
        assert_eq!(host, "devbox");
        assert_eq!(session, Some("0".to_string()));
    }

    #[test]
    fn parse_target_colon_in_session_name() {
        let (host, session) = parse_target(&no_aliases(), "local:my:weird:name");
        assert_eq!(host, "local");
        assert_eq!(session, Some("my:weird:name".to_string()));
    }

    #[test]
    fn parse_target_empty_session() {
        let (host, session) = parse_target(&no_aliases(), "local:");
        assert_eq!(host, "local");
        assert_eq!(session, None);
    }

    #[test]
    fn resolve_ctl_path_ctl_socket_wins() {
        let p = std::path::PathBuf::from("/tmp/x.sock");
        let result = resolve_ctl_path(Some(p.clone()), Some("myhost")).unwrap();
        assert_eq!(result, p);
    }

    #[test]
    fn resolve_ctl_path_ctl_socket_no_host() {
        let p = std::path::PathBuf::from("/tmp/custom.sock");
        let result = resolve_ctl_path(Some(p.clone()), None).unwrap();
        assert_eq!(result, p);
    }

    #[test]
    fn resolve_ctl_path_host_only() {
        let result = resolve_ctl_path(None, Some("devbox")).unwrap();
        let s = result.to_string_lossy();
        assert!(s.contains("connect-devbox.sock"), "got: {s}");
    }

    #[test]
    fn resolve_ctl_path_local() {
        let result = resolve_ctl_path(None, Some("local")).unwrap();
        assert_eq!(result, gritty::daemon::control_socket_path());
    }

    #[test]
    fn resolve_ctl_path_none_none_errors() {
        assert!(resolve_ctl_path(None, None).is_err());
    }

    #[test]
    fn format_age_seconds() {
        assert_eq!(format_age(100, 70), "30s ago");
    }

    #[test]
    fn format_age_minutes() {
        assert_eq!(format_age(1000, 700), "5m ago");
    }

    #[test]
    fn format_age_hours() {
        assert_eq!(format_age(10000, 0), "2h ago");
    }

    #[test]
    fn format_age_days() {
        assert_eq!(format_age(200000, 0), "2d ago");
    }

    #[test]
    fn format_idle_compact_durations() {
        assert_eq!(format_idle(100, 70), "30s");
        assert_eq!(format_idle(1000, 700), "5m");
        assert_eq!(format_idle(10000, 1), "2h");
        assert_eq!(format_idle(200000, 1), "2d");
    }

    #[test]
    fn format_idle_unknown_is_dash() {
        // 0 = older server that doesn't report last_activity.
        assert_eq!(format_idle(100, 0), "-");
    }

    #[test]
    fn parse_duration_units() {
        // The inverse of format_duration: same units, same meanings.
        assert_eq!(parse_duration("90s").unwrap(), 90);
        assert_eq!(parse_duration("30m").unwrap(), 30 * 60);
        assert_eq!(parse_duration("12h").unwrap(), 12 * 3600);
        assert_eq!(parse_duration("7d").unwrap(), 7 * 86400);
    }

    #[test]
    fn parse_duration_bare_number_is_seconds() {
        assert_eq!(parse_duration("90").unwrap(), 90);
    }

    #[test]
    fn parse_duration_rejects_garbage() {
        for bad in ["", "d", "7w", "1.5h", "-3m", "1h30m", "99999999999999999999d"] {
            assert!(parse_duration(bad).is_err(), "accepted: {bad:?}");
        }
    }

    #[test]
    fn format_timestamp_epoch_zero() {
        let s = format_timestamp(0);
        assert_eq!(s.len(), 19, "got: {s}");
        // Could be 1970 (UTC) or 1969 (negative UTC offset)
        assert!(s.contains("1970") || s.contains("1969"), "got: {s}");
    }

    #[test]
    fn format_timestamp_recent() {
        let s = format_timestamp(1_700_000_000);
        assert_eq!(s.len(), 19, "got: {s}");
        assert!(s.starts_with("202"), "got: {s}");
    }

    #[test]
    fn parse_port_spec_single() {
        let (l, t) = parse_port_spec("8080").unwrap();
        assert_eq!(l, 8080);
        assert_eq!(t, 8080);
    }

    #[test]
    fn parse_port_spec_pair() {
        let (l, t) = parse_port_spec("9090:3000").unwrap();
        assert_eq!(l, 9090);
        assert_eq!(t, 3000);
    }

    #[test]
    fn parse_port_spec_invalid() {
        assert!(parse_port_spec("abc").is_err());
        assert!(parse_port_spec("80:xyz").is_err());
        assert!(parse_port_spec("99999").is_err());
    }

    #[test]
    fn forward_args_both() {
        let (target, port) =
            forward_args("lf", Some("devbox:work".into()), Some("8080".into())).unwrap();
        assert_eq!(target.as_deref(), Some("devbox:work"));
        assert_eq!(port, "8080");
    }

    #[test]
    fn forward_args_single_is_port() {
        let (target, port) = forward_args("lf", Some("8080".into()), None).unwrap();
        assert_eq!(target, None);
        assert_eq!(port, "8080");
    }

    #[test]
    fn forward_args_none_errors_with_usage() {
        let err = forward_args("rf", None, None).unwrap_err().to_string();
        assert!(err.contains("usage: gritty rf"), "got: {err}");
    }

    #[test]
    fn in_session_hint_only_inside() {
        assert!(in_session_hint(false).is_empty());
        assert!(in_session_hint(true).contains("inside a gritty session"));
    }

    #[test]
    fn busy_port_hint_matches_both_producer_strings() {
        // rf path: client-side bind embeds the OS error.
        let rf =
            format!("{}Address already in use (os error 48)", gritty::client::FWD_ERR_BIND_PREFIX);
        let hint = busy_port_hint(&rf, 8080, 3000).unwrap();
        assert!(hint.contains("8081:3000"), "got: {hint}");
        // lf path: the server relays no OS error text, only the fixed string.
        let hint = busy_port_hint(gritty::client::FWD_ERR_SERVER_BIND, 5432, 5432).unwrap();
        assert!(hint.contains("5433:5432"), "got: {hint}");
        assert!(busy_port_hint("connection refused", 8080, 3000).is_none());
    }

    #[test]
    fn busy_port_hint_at_port_max_suggests_lower() {
        let hint = busy_port_hint("Address already in use", u16::MAX, 80).unwrap();
        assert!(hint.contains("65534:80"), "got: {hint}");
    }

    #[test]
    fn parse_forward_socket_name_roundtrip() {
        let p = gritty::client::forward_socket_path(Path::new("/run/g/connect-my-devbox.sock"), 12);
        let name = p.file_name().unwrap().to_str().unwrap();
        assert_eq!(gritty::client::parse_forward_socket_name(name), Some(("my-devbox", 12)));
        assert_eq!(
            gritty::client::parse_forward_socket_name("fwd-devbox-3.sock"),
            Some(("devbox", 3))
        );
        assert_eq!(gritty::client::parse_forward_socket_name("fwd-devbox-x.sock"), None);
        assert_eq!(gritty::client::parse_forward_socket_name("ctl.sock"), None);
    }

    #[test]
    fn live_forward_sockets_single_live() {
        let dir = tempfile::tempdir().unwrap();
        let _live =
            std::os::unix::net::UnixListener::bind(dir.path().join("fwd-devbox-3.sock")).unwrap();
        let live = live_forward_sockets(dir.path());
        assert_eq!(live.len(), 1);
        assert_eq!(live[0].host, "devbox");
        assert_eq!(live[0].session_id, 3);
        assert!(live[0].path.ends_with("fwd-devbox-3.sock"));
        assert_eq!(candidate_fallback_label(&live[0]), "devbox (session #3)");
    }

    #[test]
    fn live_forward_sockets_hyphenated_host() {
        let dir = tempfile::tempdir().unwrap();
        let _live =
            std::os::unix::net::UnixListener::bind(dir.path().join("fwd-my-devbox-12.sock"))
                .unwrap();
        let live = live_forward_sockets(dir.path());
        assert_eq!(live.len(), 1);
        assert_eq!(live[0].host, "my-devbox");
        assert_eq!(live[0].session_id, 12);
    }

    #[test]
    fn live_forward_sockets_skips_stale_and_other_sockets() {
        let dir = tempfile::tempdir().unwrap();
        // A crashed client leaves the socket file behind with no listener.
        drop(std::os::unix::net::UnixListener::bind(dir.path().join("fwd-dead-1.sock")).unwrap());
        let _ctl = std::os::unix::net::UnixListener::bind(dir.path().join("ctl.sock")).unwrap();
        let _live =
            std::os::unix::net::UnixListener::bind(dir.path().join("fwd-devbox-2.sock")).unwrap();
        let live = live_forward_sockets(dir.path());
        assert_eq!(live.len(), 1);
        assert_eq!(live[0].host, "devbox");
    }

    #[test]
    fn live_forward_sockets_empty_and_multiple() {
        let dir = tempfile::tempdir().unwrap();
        assert!(live_forward_sockets(dir.path()).is_empty());
        let _a =
            std::os::unix::net::UnixListener::bind(dir.path().join("fwd-devbox-2.sock")).unwrap();
        let _b =
            std::os::unix::net::UnixListener::bind(dir.path().join("fwd-fate-7.sock")).unwrap();
        let live = live_forward_sockets(dir.path());
        assert_eq!(live.len(), 2);
        // Sorted by path for stable listings.
        assert_eq!(live[0].host, "devbox");
        assert_eq!(live[1].host, "fate");
    }

    #[test]
    fn ambiguous_host_guard_flags_alias_and_tunnel() {
        let dir = tempfile::tempdir().unwrap();
        // Alias collision: `foo.bar.com` canonicalizes to `foo`.
        let err = ambiguous_host_guard(&foo_alias(), dir.path(), "foo.bar.com", "lf")
            .unwrap_err()
            .to_string();
        assert!(err.contains("also a connection name"), "got: {err}");
        // Tunnel collision: a connect-3000.sock exists.
        std::fs::write(dir.path().join("connect-3000.sock"), b"").unwrap();
        let err =
            ambiguous_host_guard(&no_aliases(), dir.path(), "3000", "rf").unwrap_err().to_string();
        assert!(err.contains("gritty rf 3000 <port>"), "got: {err}");
        // No collision: plain port passes.
        assert!(ambiguous_host_guard(&no_aliases(), dir.path(), "8080", "lf").is_ok());
    }

    #[test]
    fn server_auto_start_args_threads_ctl_socket() {
        assert_eq!(server_auto_start_args(None), vec!["server"]);
        assert_eq!(
            server_auto_start_args(Some("/tmp/x.sock")),
            vec!["--ctl-socket", "/tmp/x.sock", "server"]
        );
    }

    #[test]
    fn parse_port_spec_rejects_zero() {
        assert!(parse_port_spec("0").is_err());
        assert!(parse_port_spec("0:8080").is_err());
        assert!(parse_port_spec("8080:0").is_err());
    }

    #[test]
    fn parse_editor_plain() {
        assert_eq!(parse_editor("vi"), ("vi".into(), Vec::new()));
    }

    #[test]
    fn parse_editor_with_args() {
        assert_eq!(parse_editor("code --wait"), ("code".into(), vec!["--wait".into()]));
        assert_eq!(parse_editor("emacsclient -nw"), ("emacsclient".into(), vec!["-nw".into()]));
    }

    #[test]
    fn parse_editor_quoted_path() {
        assert_eq!(
            parse_editor("'/path with spaces/ed' -f"),
            ("/path with spaces/ed".into(), vec!["-f".into()])
        );
    }

    #[test]
    fn parse_editor_empty_falls_back_to_vi() {
        assert_eq!(parse_editor(""), ("vi".into(), Vec::new()));
        assert_eq!(parse_editor("   "), ("vi".into(), Vec::new()));
    }
}
