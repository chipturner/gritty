use std::path::{Path, PathBuf};

use super::AutoStart;

/// Parse a `host[:session]` target string. Splits on the first `:`.
pub(crate) fn parse_target(s: &str) -> (String, Option<String>) {
    match s.split_once(':') {
        Some((host, session)) if !session.is_empty() => {
            (host.to_string(), Some(session.to_string()))
        }
        Some((host, _)) => (host.to_string(), None),
        None => (s.to_string(), None),
    }
}

/// Parse an optional `host[:session]` target into `(host, session)`.
///
/// `None` (no target given) yields `(None, None)`; a present target is split
/// via [`parse_target`]. Shared by the commands that take an optional target
/// (connect / tail / kill-session) so the mapping is written once.
pub(crate) fn split_optional_target(target: Option<&str>) -> (Option<String>, Option<String>) {
    match target {
        Some(t) => {
            let (host, session) = parse_target(t);
            (Some(host), session)
        }
        None => (None, None),
    }
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
            AutoStart::Tunnel(host) => {
                // The connection name alone is not a valid SSH destination
                // when the user originally passed `user@host`, `host:port`,
                // or `--name <alias>`. Recover the original destination from
                // the `.dest` sidecar (falls back to the name if missing).
                let destination = gritty::connect::resolve_destination(host);
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

pub(crate) fn format_age(now: u64, created_at: u64) -> String {
    let secs = now.saturating_sub(created_at);
    if secs < 60 {
        format!("{secs}s ago")
    } else if secs < 3600 {
        format!("{}m ago", secs / 60)
    } else if secs < 86400 {
        format!("{}h ago", secs / 3600)
    } else {
        format!("{}d ago", secs / 86400)
    }
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

/// Run a port forward command via the client-side forward socket.
/// Resolves the session name to its numeric id via the daemon, then connects
/// to fwd-{host}-{id}.sock, sends the request, and blocks.
pub(crate) async fn port_forward_client_command(
    ctl_socket: Option<PathBuf>,
    target: &str,
    client_name: &str,
    direction: u8,
    listen_port: u16,
    target_port: u16,
) -> anyhow::Result<()> {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    let (host, session) = parse_target(target);
    let session =
        gritty::naming::resolve_session_name(session.as_deref().unwrap_or("0"), client_name);
    let ctl_path = resolve_ctl_path(ctl_socket, Some(&host))?;
    let session_id = resolve_session_id(&ctl_path, &session).await?;

    let fwd_path = gritty::client::forward_socket_path(&ctl_path, session_id);

    let mut stream = tokio::net::UnixStream::connect(&fwd_path).await.map_err(|_| {
        anyhow::anyhow!(
            "no client attached to {host}:{session} (could not connect to {})",
            fwd_path.display()
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
        anyhow::bail!("{msg}");
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
    eprintln!("\x1b[32m\u{25b8} {dir_str}-forward {port_str} active\x1b[0m");

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

    #[test]
    fn parse_target_host_only() {
        let (host, session) = parse_target("local");
        assert_eq!(host, "local");
        assert_eq!(session, None);
    }

    #[test]
    fn parse_target_host_and_session() {
        let (host, session) = parse_target("local:work");
        assert_eq!(host, "local");
        assert_eq!(session, Some("work".to_string()));
    }

    #[test]
    fn split_optional_target_none_is_all_none() {
        assert_eq!(split_optional_target(None), (None, None));
    }

    #[test]
    fn split_optional_target_splits_present_target() {
        assert_eq!(
            split_optional_target(Some("local:work")),
            (Some("local".to_string()), Some("work".to_string()))
        );
        assert_eq!(split_optional_target(Some("local")), (Some("local".to_string()), None));
    }

    #[test]
    fn parse_target_remote_and_id() {
        let (host, session) = parse_target("devbox:0");
        assert_eq!(host, "devbox");
        assert_eq!(session, Some("0".to_string()));
    }

    #[test]
    fn parse_target_colon_in_session_name() {
        let (host, session) = parse_target("local:my:weird:name");
        assert_eq!(host, "local");
        assert_eq!(session, Some("my:weird:name".to_string()));
    }

    #[test]
    fn parse_target_empty_session() {
        let (host, session) = parse_target("local:");
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
