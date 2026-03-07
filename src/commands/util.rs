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
        None => anyhow::bail!("specify a host or use --ctl-socket"),
    }
}

/// Send a control frame to the server and return the response.
pub(crate) async fn server_request(
    ctl_path: &PathBuf,
    frame: gritty::protocol::Frame,
) -> anyhow::Result<gritty::protocol::Frame> {
    use futures_util::{SinkExt, StreamExt};
    use gritty::protocol::{Frame, FrameCodec};
    use tokio::net::UnixStream;
    use tokio_util::codec::Framed;

    let stream = UnixStream::connect(ctl_path).await.map_err(|_| {
        anyhow::anyhow!("no server running (could not connect to {})", ctl_path.display())
    })?;
    let mut framed = Framed::new(stream, FrameCodec);
    gritty::handshake(&mut framed).await?;
    framed.send(frame).await?;
    Frame::expect_from(framed.next().await)
}

/// Run the current binary with the given args. Both `gritty server` and
/// `gritty connect <host>` self-daemonize and return after the socket is ready.
pub(crate) fn auto_start(args: &[&str]) -> anyhow::Result<()> {
    let exe = std::env::current_exe().unwrap_or_else(|_| PathBuf::from("gritty"));
    let status = std::process::Command::new(&exe).args(args).status()?;
    if !status.success() {
        anyhow::bail!("failed to start `gritty {}` (exit {})", args.join(" "), status);
    }
    Ok(())
}

/// Try to connect to the control socket. On failure, auto-start the
/// appropriate process and retry with a bounded loop (or indefinitely
/// with `--wait`).
pub(crate) async fn connect_or_start(
    ctl_path: &Path,
    auto_start_mode: &AutoStart,
    wait: bool,
) -> anyhow::Result<tokio::net::UnixStream> {
    use tokio::net::UnixStream;

    match UnixStream::connect(ctl_path).await {
        Ok(s) => return Ok(s),
        Err(_) => match auto_start_mode {
            AutoStart::Server => {
                eprintln!("\x1b[2;33m\u{25b8} starting server...\x1b[0m");
                auto_start(&["server"])?;
            }
            AutoStart::Tunnel(host) => {
                eprintln!("\x1b[2;33m\u{25b8} starting tunnel {host}...\x1b[0m");
                auto_start(&["connect", host])?;
            }
            AutoStart::None if wait => {}
            AutoStart::None => {
                anyhow::bail!("no server running (could not connect to {})", ctl_path.display());
            }
        },
    }

    // Retry loop: bounded (10 retries, 500ms apart) or indefinite (--wait)
    let max_retries = if wait { u32::MAX } else { 10 };
    for _ in 0..max_retries {
        match UnixStream::connect(ctl_path).await {
            Ok(s) => return Ok(s),
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
pub(crate) fn parse_port_spec(spec: &str) -> anyhow::Result<(u16, u16)> {
    if let Some((a, b)) = spec.split_once(':') {
        let listen: u16 = a.parse().map_err(|_| anyhow::anyhow!("invalid listen port: {a}"))?;
        let target: u16 = b.parse().map_err(|_| anyhow::anyhow!("invalid target port: {b}"))?;
        Ok((listen, target))
    } else {
        let port: u16 = spec.parse().map_err(|_| anyhow::anyhow!("invalid port: {spec}"))?;
        Ok((port, port))
    }
}

/// Run a port forward command. Connects to GRITTY_SOCK, sends the request,
/// reads the response, prints status, and blocks until SIGINT or EOF.
pub(crate) async fn port_forward_command(
    direction: u8,
    listen_port: u16,
    target_port: u16,
) -> anyhow::Result<()> {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    let sock_path = match std::env::var("GRITTY_SOCK") {
        Ok(p) => p,
        Err(_) => {
            anyhow::bail!("GRITTY_SOCK not set (are you inside a gritty session?)");
        }
    };

    let mut stream = tokio::net::UnixStream::connect(&sock_path).await?;

    // Write: [discriminator][direction][listen_port BE][target_port BE]
    let mut header = [0u8; 6];
    header[0] = gritty::protocol::SvcRequest::PortForward.to_byte();
    header[1] = direction;
    header[2..4].copy_from_slice(&listen_port.to_be_bytes());
    header[4..6].copy_from_slice(&target_port.to_be_bytes());
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

    // Block until SIGINT or stream EOF
    let mut sigint = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::interrupt())?;
    let mut sigterm = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())?;
    let mut buf = [0u8; 1];
    tokio::select! {
        _ = sigint.recv() => {}
        _ = sigterm.recv() => {}
        _ = stream.read(&mut buf) => {}
    }
    // Stream drop closes the connection, triggering server-side cleanup
    Ok(())
}

pub(crate) fn open_url(url: &str) {
    let sock_path = match std::env::var("GRITTY_SOCK") {
        Ok(p) => p,
        Err(_) => {
            eprintln!(
                "error: GRITTY_SOCK not set (are you inside a gritty session with --forward-open?)"
            );
            std::process::exit(1);
        }
    };
    match std::os::unix::net::UnixStream::connect(&sock_path) {
        Ok(mut stream) => {
            use std::io::Write;
            let _ = stream.write_all(&[gritty::protocol::SvcRequest::OpenUrl.to_byte()]);
            let _ = stream.write_all(url.as_bytes());
            let _ = stream.write_all(b"\n");
        }
        Err(e) => {
            eprintln!("error: could not connect to service socket ({sock_path}): {e}");
            std::process::exit(1);
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
    let status = std::process::Command::new(&editor).arg(&path).status()?;
    if !status.success() {
        anyhow::bail!("{editor} exited with {status}");
    }
    Ok(())
}

pub(crate) async fn info(config: &gritty::config::ConfigFile) -> anyhow::Result<()> {
    use gritty::protocol::Frame;

    println!("gritty {}", env!("CARGO_PKG_VERSION"));
    println!();

    let cfg_path = gritty::config::config_path();
    let cfg_status = if cfg_path.exists() {
        if config.host.is_empty() {
            "loaded".to_string()
        } else {
            let n = config.host.len();
            let s = if n == 1 { "" } else { "s" };
            format!("loaded, {n} host{s}")
        }
    } else {
        "not found".to_string()
    };
    println!("config:         {} ({cfg_status})", cfg_path.display());

    let socket_dir = canonicalize_or_raw(gritty::daemon::socket_dir());
    let ctl_path = socket_dir.join("ctl.sock");

    println!("socket dir:     {}", socket_dir.display());
    println!("server socket:  {}", ctl_path.display());

    // Probe server status via server_request (which includes handshake)
    let pid_path = gritty::daemon::pid_file_path(&ctl_path);
    let pid = std::fs::read_to_string(&pid_path).ok().and_then(|s| s.trim().parse::<u32>().ok());

    match server_request(&ctl_path, Frame::ListSessions).await {
        Ok(Frame::SessionInfo { sessions }) => {
            let n = sessions.len();
            match pid {
                Some(p) => {
                    let s = if n == 1 { "" } else { "s" };
                    println!("server status:  running (pid {p}, {n} session{s})");
                }
                None => println!("server status:  running"),
            }
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
}
