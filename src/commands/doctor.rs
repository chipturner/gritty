use std::os::unix::fs::{MetadataExt, PermissionsExt};
use std::path::Path;

use gritty::connect::{TunnelStatus, enumerate_tunnels, probe_tunnel_status, read_pid_hint};
use gritty::protocol::{Frame, PROTOCOL_VERSION};

use super::util::server_request;

// ---- Status / Check types ---------------------------------------------------

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Status {
    Ok,
    Warn,
    Fail,
}

struct Check {
    status: Status,
    message: String,
    hint: Option<String>,
}

impl Check {
    fn ok(msg: impl Into<String>) -> Self {
        Self { status: Status::Ok, message: msg.into(), hint: None }
    }
    fn warn(msg: impl Into<String>) -> Self {
        Self { status: Status::Warn, message: msg.into(), hint: None }
    }
    fn fail(msg: impl Into<String>) -> Self {
        Self { status: Status::Fail, message: msg.into(), hint: None }
    }
    fn with_hint(mut self, hint: impl Into<String>) -> Self {
        self.hint = Some(hint.into());
        self
    }
}

// ---- Rendering --------------------------------------------------------------

fn status_symbol(s: Status) -> &'static str {
    match s {
        Status::Ok => "\x1b[32m\u{2713}\x1b[0m",   // green ✓
        Status::Warn => "\x1b[33m!\x1b[0m",        // yellow !
        Status::Fail => "\x1b[31m\u{2717}\x1b[0m", // red ✗
    }
}

fn render(groups: &[(&str, Vec<Check>)]) {
    let mut first = true;
    for (title, checks) in groups {
        if checks.is_empty() {
            continue;
        }
        if !first {
            println!();
        }
        first = false;
        println!("{title}");
        for c in checks {
            println!("  {} {}", status_symbol(c.status), c.message);
            if let Some(hint) = &c.hint {
                println!("    \x1b[2m\u{2192} {hint}\x1b[0m");
            }
        }
    }

    let warns: usize =
        groups.iter().flat_map(|(_, cs)| cs).filter(|c| c.status == Status::Warn).count();
    let fails: usize =
        groups.iter().flat_map(|(_, cs)| cs).filter(|c| c.status == Status::Fail).count();
    println!();
    match (fails, warns) {
        (0, 0) => println!("no issues found"),
        (0, w) => println!("{w} warning{}", if w == 1 { "" } else { "s" }),
        (f, 0) => println!("{f} issue{}", if f == 1 { "" } else { "s" }),
        (f, w) => println!(
            "{f} issue{}, {w} warning{}",
            if f == 1 { "" } else { "s" },
            if w == 1 { "" } else { "s" },
        ),
    }
}

// ---- Checks -----------------------------------------------------------------

fn check_config() -> Vec<Check> {
    let mut checks = Vec::new();
    let path = gritty::config::config_path();

    match std::fs::read_to_string(&path) {
        Ok(content) => match toml::from_str::<gritty::config::ConfigFile>(&content) {
            Ok(cfg) => {
                if cfg.host.is_empty() {
                    checks.push(Check::ok("config valid"));
                } else {
                    let n = cfg.host.len();
                    let s = if n == 1 { "" } else { "s" };
                    checks.push(Check::ok(format!("config valid ({n} host{s})")));
                }
            }
            Err(e) => {
                checks.push(Check::fail(format!("config invalid: {e}")));
            }
        },
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            checks.push(Check::ok("no config file (using defaults)"));
        }
        Err(e) => {
            checks.push(Check::fail(format!("cannot read config: {e}")));
        }
    }

    // Socket directory
    let socket_dir = gritty::daemon::socket_dir();
    match std::fs::symlink_metadata(&socket_dir) {
        Ok(meta) => {
            if meta.file_type().is_symlink() {
                checks.push(Check::fail(format!(
                    "socket directory is a symlink: {}",
                    socket_dir.display()
                )));
            } else if !meta.is_dir() {
                checks.push(Check::fail(format!(
                    "socket path is not a directory: {}",
                    socket_dir.display()
                )));
            } else {
                let mode = meta.permissions().mode() & 0o777;
                let uid = meta.uid();
                let my_uid = unsafe { libc::getuid() };
                if uid != my_uid {
                    checks.push(Check::fail(format!(
                        "socket directory owned by uid {uid}, expected {my_uid}"
                    )));
                } else if mode != 0o700 {
                    checks.push(Check::warn(format!(
                        "socket directory permissions {mode:04o} (expected 0700)"
                    )));
                } else {
                    checks.push(Check::ok("socket directory permissions ok"));
                }
            }
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            checks.push(Check::ok("socket directory does not exist yet (created on first use)"));
        }
        Err(e) => {
            checks.push(Check::fail(format!("cannot stat socket directory: {e}")));
        }
    }

    checks
}

/// Check if a process is alive via kill(pid, 0).
fn is_process_alive(pid: u32) -> bool {
    unsafe { libc::kill(pid as i32, 0) == 0 }
}

/// Read a PID file and parse its contents.
fn read_pid_file(path: &Path) -> Option<u32> {
    std::fs::read_to_string(path).ok().and_then(|s| s.trim().parse().ok())
}

/// Attempt a handshake and return (version, session_count).
/// On version mismatch, the server returns an Error frame -- we return Err with context.
async fn probe_server(
    ctl_path: &Path,
) -> Result<(u16, Vec<gritty::protocol::SessionEntry>), String> {
    // Try handshake + ListSessions
    match server_request(ctl_path, Frame::ListSessions).await {
        Ok(Frame::SessionInfo { sessions }) => Ok((PROTOCOL_VERSION, sessions)),
        Ok(Frame::Error { code, message }) => {
            if code == gritty::protocol::ErrorCode::VersionMismatch {
                Err(format!("version mismatch: {message}"))
            } else {
                Err(format!("server error: {message}"))
            }
        }
        Ok(_) => Err("unexpected response from server".to_string()),
        Err(e) => Err(format!("{e}")),
    }
}

async fn check_local_server(
    socket_dir: &Path,
) -> (Vec<Check>, Vec<gritty::protocol::SessionEntry>) {
    let mut checks = Vec::new();
    let ctl_path = socket_dir.join("ctl.sock");
    let pid_path = gritty::daemon::pid_file_path(&ctl_path);
    let pid = read_pid_file(&pid_path);

    // Check daemon PID staleness
    if let Some(pid) = pid
        && !is_process_alive(pid)
    {
        checks.push(
            Check::fail(format!("stale daemon.pid (pid {pid} not running)")).with_hint(
                "remove the PID file and stale socket, or run: gritty kill-server local",
            ),
        );
        return (checks, Vec::new());
    }

    // Probe server via socket
    match probe_server(&ctl_path).await {
        Ok((version, sessions)) => {
            let n = sessions.len();
            let s = if n == 1 { "" } else { "s" };
            let pid_str = match pid {
                Some(p) => format!("pid {p}, "),
                None => String::new(),
            };
            checks.push(Check::ok(format!(
                "server running ({pid_str}protocol v{version}, {n} session{s})"
            )));
            if version != PROTOCOL_VERSION {
                checks.push(Check::warn(format!(
                    "server protocol v{version} differs from local v{PROTOCOL_VERSION}"
                )));
            }

            // Check per-session sockets
            for entry in &sessions {
                let label = if entry.name.is_empty() {
                    format!("{}", entry.id)
                } else {
                    entry.name.clone()
                };
                // svc socket is always bound at session start
                let svc_sock = socket_dir.join(format!("svc-{}.sock", entry.id));
                if !svc_sock.exists() {
                    checks.push(Check::warn(format!(
                        "session {label}: service socket missing ({})",
                        svc_sock.display()
                    )));
                }
                // agent socket is bound when -A is active
                if entry.agent_forwarding_active {
                    let agent_sock = socket_dir.join(format!("agent-{}.sock", entry.id));
                    if !agent_sock.exists() {
                        checks.push(Check::fail(format!(
                            "session {label}: agent socket missing ({})",
                            agent_sock.display()
                        )));
                    }
                }
            }

            // Log file sizes
            check_log_file(&mut checks, &socket_dir.join("daemon.log"));
            check_log_file(&mut checks, &socket_dir.join("daemon.out"));

            return (checks, sessions);
        }
        Err(msg) if msg.contains("version mismatch") => {
            let pid_str = match pid {
                Some(p) => format!(" (pid {p})"),
                None => String::new(),
            };
            checks.push(
                Check::warn(format!("server{pid_str}: {msg}"))
                    .with_hint("restart the server: gritty kill-server local && gritty server"),
            );
        }
        Err(_) => {
            if pid.is_some() {
                checks.push(Check::warn(
                    "server process may be running but socket is unresponsive".to_string(),
                ));
            } else {
                checks.push(Check::ok("server not running"));
            }
        }
    }

    (checks, Vec::new())
}

const LOG_SIZE_WARN: u64 = 50 * 1024 * 1024; // 50 MB

fn check_log_file(checks: &mut Vec<Check>, path: &Path) {
    let name = path.file_name().and_then(|f| f.to_str()).unwrap_or("?");
    if let Ok(meta) = std::fs::metadata(path) {
        let size = meta.len();
        if size >= LOG_SIZE_WARN {
            checks
                .push(Check::warn(format!("{name} is {} (consider rotating)", format_size(size))));
        }
    }
}

async fn check_tunnels(socket_dir: &Path) -> Vec<Check> {
    let mut checks = Vec::new();
    let names = enumerate_tunnels();

    if names.is_empty() {
        return checks;
    }

    for name in &names {
        let status = probe_tunnel_status(name);
        let pid = read_pid_hint(name);
        let pid_str = match pid {
            Some(p) => format!(", pid {p}"),
            None => String::new(),
        };

        match status {
            TunnelStatus::Healthy => {
                // Probe protocol version through tunnel socket
                let tunnel_ctl = socket_dir.join(format!("connect-{name}.sock"));
                let ver_note = match probe_server(&tunnel_ctl).await {
                    Ok((v, _)) if v != PROTOCOL_VERSION => {
                        format!(", remote protocol v{v}")
                    }
                    Ok((v, _)) => format!(", protocol v{v}"),
                    Err(msg) if msg.contains("version mismatch") => {
                        let msg = msg.strip_prefix("version mismatch: ").unwrap_or(&msg);
                        format!(", {msg}")
                    }
                    Err(_) => String::new(),
                };
                checks.push(Check::ok(format!("{name}: healthy{pid_str}{ver_note}")));

                // Check tunnel log size
                check_log_file(&mut checks, &gritty::connect::connect_log_path(name));
            }
            TunnelStatus::Reconnecting => {
                let log = gritty::connect::connect_log_path(name);
                let out = gritty::connect::connect_out_path(name);
                checks.push(Check::warn(format!("{name}: reconnecting{pid_str}")).with_hint(
                    format!(
                        "tracing: {}\n    \x1b[2m\u{2192} ssh output: {}",
                        log.display(),
                        out.display()
                    ),
                ));
            }
            TunnelStatus::Stale => {
                checks.push(
                    Check::fail(format!("{name}: stale process{pid_str}"))
                        .with_hint(format!("gritty tunnel-destroy {name}")),
                );
            }
        }
    }

    checks
}

fn check_sockets(socket_dir: &Path, live_session_ids: &[u32]) -> Vec<Check> {
    let entries = match std::fs::read_dir(socket_dir) {
        Ok(e) => e,
        Err(_) => return Vec::new(),
    };

    let mut orphaned = Vec::new();

    for entry in entries.filter_map(|e| e.ok()) {
        let name = entry.file_name().to_string_lossy().to_string();

        // Skip non-socket files and known non-sockets
        if !name.ends_with(".sock") {
            continue;
        }

        // Skip known sockets
        if name == "ctl.sock" || name.starts_with("connect-") {
            continue;
        }

        // Check agent-{id}.sock and svc-{id}.sock
        if let Some(id_str) = name
            .strip_prefix("agent-")
            .or_else(|| name.strip_prefix("svc-"))
            .and_then(|s| s.strip_suffix(".sock"))
        {
            if let Ok(id) = id_str.parse::<u32>()
                && !live_session_ids.contains(&id)
            {
                orphaned.push(name);
            }
            continue;
        }

        // fwd-*.sock -- check if connectable
        if name.starts_with("fwd-") {
            let path = socket_dir.join(&name);
            if std::os::unix::net::UnixStream::connect(&path).is_err() {
                orphaned.push(name);
            }
        }
    }

    if orphaned.is_empty() {
        return Vec::new();
    }

    orphaned.sort();
    let n = orphaned.len();
    let s = if n == 1 { "" } else { "s" };
    let list = orphaned.join(", ");
    vec![Check::warn(format!("{n} orphaned socket file{s}: {list}"))]
}

// ---- Helpers ----------------------------------------------------------------

fn format_size(bytes: u64) -> String {
    if bytes < 1024 {
        format!("{bytes} B")
    } else if bytes < 1024 * 1024 {
        format!("{} KB", bytes / 1024)
    } else if bytes < 1024 * 1024 * 1024 {
        format!("{} MB", bytes / (1024 * 1024))
    } else {
        format!("{:.1} GB", bytes as f64 / (1024.0 * 1024.0 * 1024.0))
    }
}

// ---- Entry point ------------------------------------------------------------

pub(crate) async fn doctor() -> anyhow::Result<()> {
    let socket_dir = super::util::canonicalize_or_raw(gritty::daemon::socket_dir());

    let config_checks = check_config();
    let (server_checks, sessions) = check_local_server(&socket_dir).await;
    let tunnel_checks = check_tunnels(&socket_dir).await;

    let live_ids: Vec<u32> = sessions.iter().map(|s| s.id).collect();
    let socket_checks = check_sockets(&socket_dir, &live_ids);

    let groups: Vec<(&str, Vec<Check>)> = vec![
        ("Configuration", config_checks),
        ("Local server", server_checks),
        ("Tunnels", tunnel_checks),
        ("Sockets", socket_checks),
    ];

    render(&groups);

    let has_failures = groups.iter().flat_map(|(_, cs)| cs).any(|c| c.status == Status::Fail);
    if has_failures {
        std::process::exit(1);
    }
    Ok(())
}

// ---- Tests ------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn format_size_bytes() {
        assert_eq!(format_size(500), "500 B");
    }

    #[test]
    fn format_size_kb() {
        assert_eq!(format_size(2048), "2 KB");
    }

    #[test]
    fn format_size_mb() {
        assert_eq!(format_size(5 * 1024 * 1024), "5 MB");
    }

    #[test]
    fn format_size_gb() {
        assert_eq!(format_size(2 * 1024 * 1024 * 1024), "2.0 GB");
    }

    #[test]
    fn check_config_missing_file() {
        // config_path() returns a deterministic path; in test env it may or may not exist.
        // We test the function indirectly -- the important thing is it doesn't panic.
        let checks = check_config();
        assert!(!checks.is_empty());
    }

    #[test]
    fn orphaned_sockets_empty_dir() {
        let dir = tempfile::tempdir().unwrap();
        let checks = check_sockets(dir.path(), &[]);
        assert!(checks.is_empty());
    }

    #[test]
    fn orphaned_sockets_detects_stale() {
        let dir = tempfile::tempdir().unwrap();
        // Create a fake agent socket file (not a real socket, just a file)
        std::fs::write(dir.path().join("agent-99.sock"), "").unwrap();
        let checks = check_sockets(dir.path(), &[1, 2]);
        assert_eq!(checks.len(), 1);
        assert_eq!(checks[0].status, Status::Warn);
        assert!(checks[0].message.contains("agent-99.sock"));
    }

    #[test]
    fn orphaned_sockets_ignores_live() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("agent-1.sock"), "").unwrap();
        std::fs::write(dir.path().join("svc-1.sock"), "").unwrap();
        let checks = check_sockets(dir.path(), &[1]);
        assert!(checks.is_empty());
    }
}
