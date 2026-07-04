use std::os::unix::fs::{MetadataExt, PermissionsExt};
use std::path::{Path, PathBuf};

use gritty::connect::{TunnelStatus, enumerate_tunnels, probe_tunnel_status, read_pid_hint};
use gritty::protocol::{Frame, PROTOCOL_VERSION};
use gritty::runinfo::{RunInfo, Staleness};

use super::util::server_request;

// ---- Status / Check types ---------------------------------------------------

#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "lowercase")]
enum Status {
    Ok,
    Warn,
    Fail,
}

/// Also the per-check shape of `doctor --json` -- extend rather than
/// rename/remove fields.
#[derive(serde::Serialize)]
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

/// Read a long-lived process's `.info` sidecar and flag it if the on-disk
/// binary has been replaced since the process started. This is the only way
/// to catch a same-protocol rebuild -- the wire handshake can't see it, and
/// for the tunnel supervisor (a pure byte proxy) no handshake ever touches
/// its code at all.
fn check_staleness(info_path: &Path, label: &str, stale_hint: &str, checks: &mut Vec<Check>) {
    let Ok(info) = RunInfo::read(info_path) else {
        // No `.info` file -- process predates this feature or wrote it to a
        // different socket dir. Not an error, just no staleness signal.
        return;
    };
    match info.staleness_vs_current() {
        None => {}
        Some(s @ Staleness::Protocol { .. }) => {
            checks.push(Check::fail(format!("{label}: {s}")).with_hint(stale_hint));
        }
        Some(s @ Staleness::Build { .. }) => {
            checks.push(Check::warn(format!("{label}: {s}")).with_hint(stale_hint));
        }
    }
}

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

// ---- Paths ------------------------------------------------------------------

/// The key filesystem locations worth surfacing up front: where config,
/// sockets, logs, and the device id live. Pure so it can be unit tested;
/// existence is resolved at render time. Log/socket paths are derived from
/// `server_dir` so a `--ctl-socket` override is reflected.
fn path_report(
    config_path: PathBuf,
    device_id_path: PathBuf,
    ctl_path: &Path,
    server_dir: &Path,
) -> Vec<(&'static str, PathBuf)> {
    vec![
        ("config file", config_path),
        ("socket dir", server_dir.to_path_buf()),
        ("server socket", ctl_path.to_path_buf()),
        ("device id", device_id_path),
        ("server log", server_dir.join("daemon.log")),
        ("server output", server_dir.join("daemon.out")),
    ]
}

fn render_paths(paths: &[(&str, PathBuf)]) {
    println!("Paths");
    let width = paths.iter().map(|(label, _)| label.len()).max().unwrap_or(0);
    for (label, path) in paths {
        if path.exists() {
            println!("  {label:<width$}  {}", path.display());
        } else {
            println!("  {label:<width$}  {} \x1b[2m(not found)\x1b[0m", path.display());
        }
    }
}

// ---- Checks -----------------------------------------------------------------

fn check_config() -> Vec<Check> {
    let mut checks = Vec::new();
    let path = gritty::config::config_path();

    match gritty::config::config_status(&path) {
        gritty::config::ConfigStatus::NotFound => {
            checks.push(Check::ok("no config file (using defaults)"));
        }
        gritty::config::ConfigStatus::Valid(cfg) => {
            if cfg.host.is_empty() {
                checks.push(Check::ok("config valid"));
            } else {
                let n = cfg.host.len();
                let s = if n == 1 { "" } else { "s" };
                checks.push(Check::ok(format!("config valid ({n} host{s})")));
            }
        }
        gritty::config::ConfigStatus::Invalid(e) => {
            checks.push(Check::fail(format!("config invalid: {e}")));
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

async fn check_local_server(ctl_path: &Path) -> (Vec<Check>, Vec<gritty::protocol::SessionEntry>) {
    let mut checks = Vec::new();
    // Per-session sockets and the daemon log/pid are siblings of the ctl
    // socket (the daemon derives its dir from ctl_path.parent()).
    let socket_dir = ctl_path.parent().unwrap_or(Path::new("."));
    let pid_path = gritty::daemon::pid_file_path(ctl_path);
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
    match probe_server(ctl_path).await {
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
            check_staleness(
                &gritty::runinfo::daemon_info_path(ctl_path),
                "server",
                "gritty refresh local",
                &mut checks,
            );

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
                Check::warn(format!("server{pid_str}: {msg}")).with_hint("gritty refresh local"),
            );
            // The handshake detected the protocol mismatch; the `.info`
            // sidecar can say which side is stale (running daemon vs on-disk
            // binary) -- often more actionable than the raw version numbers.
            check_staleness(
                &gritty::runinfo::daemon_info_path(ctl_path),
                "server",
                "gritty refresh local",
                &mut checks,
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

        // Supervisor staleness is orthogonal to tunnel health -- a perfectly
        // healthy tunnel can be running code from last month. Check it for any
        // live supervisor (Healthy or Reconnecting; a Stale one is already
        // flagged for teardown).
        if status != TunnelStatus::Stale {
            check_staleness(
                &gritty::runinfo::connect_info_path(name),
                &format!("{name} supervisor"),
                &format!("gritty refresh {name}"),
                &mut checks,
            );
        }
    }

    checks
}

/// Sidecar files of tunnels that have no `.lock` file at all.
///
/// These are invisible to [`check_tunnels`] (which enumerates lock files) and
/// were historically skipped by [`check_sockets`], so a tunnel that died
/// without running cleanup (SIGKILL, power loss, crash before `ConnectGuard`
/// existed) leaves residue no diagnostic could see. `.log`/`.out` (kept for
/// post-mortem debugging) and `.remote-sock` (a cache that deliberately
/// survives teardown) are not residue.
fn check_tunnel_residue(socket_dir: &Path) -> Vec<Check> {
    const RESIDUE_EXTS: [&str; 5] = ["sock", "pid", "dest", "info", "ssh-opts"];
    let Ok(entries) = std::fs::read_dir(socket_dir) else {
        return Vec::new();
    };

    // name -> leftover files, BTreeMap for deterministic report order.
    let mut residue: std::collections::BTreeMap<String, Vec<String>> = Default::default();
    for entry in entries.filter_map(|e| e.ok()) {
        let fname = entry.file_name().to_string_lossy().to_string();
        let Some(rest) = fname.strip_prefix("connect-") else {
            continue;
        };
        let Some((name, ext)) = rest.rsplit_once('.') else {
            continue;
        };
        if !RESIDUE_EXTS.contains(&ext) {
            continue;
        }
        // A `.lock` file means check_tunnels owns this name (live or stale).
        if socket_dir.join(format!("connect-{name}.lock")).exists() {
            continue;
        }
        residue.entry(name.to_string()).or_default().push(fname);
    }

    residue
        .into_iter()
        .map(|(name, mut files)| {
            files.sort();
            Check::warn(format!("dead tunnel {name}: leftover {}", files.join(", ")))
                .with_hint(format!("gritty tunnel-destroy {name}"))
        })
        .collect()
}

/// Live client processes on this machine, discovered through their forward
/// sockets. A connectable `fwd-{host}-{id}.sock` means a client process here
/// is attached to that host:session and serving `lf`/`rf` requests. This is
/// the only client-side introspection surface gritty has -- the daemon knows
/// a session is attached, but not from which machine or process.
fn check_clients(socket_dir: &Path) -> Vec<Check> {
    let Ok(entries) = std::fs::read_dir(socket_dir) else {
        return Vec::new();
    };

    let mut live: Vec<(String, u32)> = entries
        .filter_map(|e| e.ok())
        .filter_map(|e| {
            let name = e.file_name().to_string_lossy().to_string();
            let (host, id) = gritty::client::parse_forward_socket_name(&name)?;
            std::os::unix::net::UnixStream::connect(e.path())
                .is_ok()
                .then(|| (host.to_string(), id))
        })
        .collect();
    live.sort();

    live.into_iter()
        .map(|(host, id)| Check::ok(format!("client on this machine holds {host}:{id}")))
        .collect()
}

/// Bind-lock companions whose socket no longer exists. Cleanup paths now
/// remove the `.bindlock` together with its socket; anything left is litter
/// from older versions or an unclean process death.
fn check_stale_bindlocks(socket_dir: &Path) -> Vec<Check> {
    let Ok(entries) = std::fs::read_dir(socket_dir) else {
        return Vec::new();
    };

    let stale: Vec<String> = entries
        .filter_map(|e| e.ok())
        .filter_map(|e| {
            let name = e.file_name().to_string_lossy().to_string();
            let sock = name.strip_suffix(".bindlock")?;
            (!socket_dir.join(sock).exists()).then_some(name)
        })
        .collect();

    if stale.is_empty() {
        return Vec::new();
    }
    let n = stale.len();
    let s = if n == 1 { "" } else { "s" };
    vec![
        Check::warn(format!("{n} stale bind-lock file{s} (socket already gone)"))
            .with_hint(format!("safe to remove: rm '{}'/*.bindlock", socket_dir.display())),
    ]
}

/// Filename patterns this gritty version writes into the socket directory.
/// Mirrors the on-disk state inventory in docs/internals.md -- update both
/// together. Classification errs toward "known": a false "known" keeps a
/// little litter, a false "unknown" would let `--clean` delete a real
/// artifact.
fn is_known_artifact(name: &str) -> bool {
    // Bind locks are companions named `<socket-file>.bindlock`: classify the base.
    let name = name.strip_suffix(".bindlock").unwrap_or(name);

    if matches!(name, "ctl.sock" | "daemon.pid" | "daemon.info" | "gritty-open") {
        return true;
    }
    // Daemon logs, including external-rotation suffixes (daemon.log.1, .gz).
    if name.starts_with("daemon.log") || name.starts_with("daemon.out") {
        return true;
    }
    // Per-session sockets: agent-{id}.sock / svc-{id}.sock, numeric id.
    if let Some(id) = name
        .strip_prefix("agent-")
        .or_else(|| name.strip_prefix("svc-"))
        .and_then(|s| s.strip_suffix(".sock"))
    {
        return !id.is_empty() && id.bytes().all(|b| b.is_ascii_digit());
    }
    // Client forward sockets: fwd-{host}-{id}.sock.
    if name.starts_with("fwd-") && name.ends_with(".sock") {
        return true;
    }
    // Tunnel sidecars: connect-{name}.{ext}. Tunnel names may contain dots,
    // so the extension is whatever follows the last one.
    if let Some(rest) = name.strip_prefix("connect-") {
        const EXTS: [&str; 9] =
            ["sock", "pid", "info", "lock", "dest", "ssh-opts", "remote-sock", "log", "out"];
        if rest.rsplit_once('.').is_some_and(|(_, ext)| EXTS.contains(&ext)) {
            return true;
        }
        // Rotated tunnel logs: connect-x.log.1, connect-x.out.gz.
        if rest.contains(".log") || rest.contains(".out") {
            return true;
        }
    }
    false
}

/// Entries in the socket directory matching no pattern this gritty version
/// writes -- litter from a release whose artifact set differed, or a stray
/// file. Reported by default; `doctor --clean` removes them. Never removes
/// directories or sockets something is actively serving, whoever created them.
fn check_unknown_files(socket_dir: &Path, ctl_name: Option<&str>, clean: bool) -> Vec<Check> {
    use std::os::unix::fs::FileTypeExt;

    let Ok(entries) = std::fs::read_dir(socket_dir) else {
        return Vec::new();
    };

    // A `--ctl-socket` override in the default dir makes that socket (and its
    // bindlock) a known artifact even though its name isn't `ctl.sock`.
    let mut names: Vec<String> = entries
        .filter_map(|e| e.ok())
        .map(|e| e.file_name().to_string_lossy().to_string())
        .filter(|name| {
            let base = name.strip_suffix(".bindlock").unwrap_or(name);
            Some(base) != ctl_name && !is_known_artifact(name)
        })
        .collect();
    names.sort();

    let mut checks = Vec::new();
    for name in names {
        let path = socket_dir.join(&name);
        let Ok(meta) = std::fs::symlink_metadata(&path) else {
            continue; // vanished between readdir and stat
        };
        if meta.is_dir() {
            checks.push(Check::warn(format!("unknown directory: {name} (not removing)")));
            continue;
        }
        // A connectable socket has a live process serving it -- deleting the
        // path out from under that process is never right.
        if meta.file_type().is_socket() && std::os::unix::net::UnixStream::connect(&path).is_ok() {
            checks.push(Check::warn(format!("unknown socket in use: {name} (not removing)")));
            continue;
        }
        if !clean {
            checks.push(
                Check::warn(format!("unknown file: {name}"))
                    .with_hint("gritty doctor --clean removes it"),
            );
            continue;
        }
        match std::fs::remove_file(&path) {
            Ok(()) => checks.push(Check::ok(format!("removed unknown file: {name}"))),
            Err(e) => checks.push(Check::warn(format!("could not remove {name}: {e}"))),
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

/// Walk the process table for gritty daemons that are running but no longer
/// registered on disk -- unreachable "orphans" invisible to `ls`/`refresh`
/// through the socket dir. These accumulate when something external (systemd
/// `$XDG_RUNTIME_DIR` teardown on logout, `/tmp` age sweeps) deletes the
/// socket dir out from under a running daemon.
fn check_orphan_daemons() -> Vec<Check> {
    gritty::procscan::find_orphan_daemons()
        .into_iter()
        .map(|o| {
            Check::fail(format!("orphaned daemon: {o}")).with_hint(
                "run `gritty refresh` to reap it; if this host wipes $XDG_RUNTIME_DIR on \
                 logout, `loginctl enable-linger` prevents new orphans",
            )
        })
        .collect()
}

// ---- Entry point ------------------------------------------------------------

pub(crate) async fn doctor(
    ctl_socket: Option<std::path::PathBuf>,
    clean: bool,
    json: bool,
) -> anyhow::Result<()> {
    let default_dir = super::util::canonicalize_or_raw(gritty::daemon::socket_dir());
    // The server's ctl/svc/agent sockets and log follow a --ctl-socket
    // override; tunnel connect-*.sock always live in the default dir.
    let ctl_path = match &ctl_socket {
        Some(p) => super::util::canonicalize_or_raw(p.clone()),
        None => default_dir.join("ctl.sock"),
    };
    let server_dir =
        ctl_path.parent().map(Path::to_path_buf).unwrap_or_else(|| default_dir.clone());

    let config_checks = check_config();
    let (server_checks, sessions) = check_local_server(&ctl_path).await;
    let mut tunnel_checks = check_tunnels(&default_dir).await;
    tunnel_checks.extend(check_tunnel_residue(&default_dir));
    let client_checks = check_clients(&server_dir);

    let live_ids: Vec<u32> = sessions.iter().map(|s| s.id).collect();
    let mut socket_checks = check_sockets(&server_dir, &live_ids);
    socket_checks.extend(check_stale_bindlocks(&server_dir));
    // The unknown-file scan covers only the default (gritty-owned) socket
    // dir -- a --ctl-socket override may point into a directory gritty does
    // not own, where flagging the user's files would be wrong.
    let ctl_name = (ctl_path.parent() == Some(default_dir.as_path()))
        .then(|| ctl_path.file_name())
        .flatten()
        .and_then(|f| f.to_str());
    socket_checks.extend(check_unknown_files(&default_dir, ctl_name, clean));

    let groups: Vec<(&str, Vec<Check>)> = vec![
        ("Configuration", config_checks),
        ("Local server", server_checks),
        ("Orphaned processes", check_orphan_daemons()),
        ("Tunnels", tunnel_checks),
        ("Clients", client_checks),
        ("Sockets", socket_checks),
    ];

    let paths = path_report(
        gritty::config::config_path(),
        gritty::device_id_path(),
        &ctl_path,
        &server_dir,
    );

    if json {
        println!("{}", serde_json::to_string_pretty(&report_json(&paths, &groups))?);
    } else {
        render_paths(&paths);
        println!();
        render(&groups);
    }

    let has_failures = groups.iter().flat_map(|(_, cs)| cs).any(|c| c.status == Status::Fail);
    if has_failures {
        std::process::exit(1);
    }
    Ok(())
}

/// `doctor --json` output contract -- extend rather than rename/remove fields.
fn report_json(paths: &[(&str, PathBuf)], groups: &[(&str, Vec<Check>)]) -> serde_json::Value {
    let count =
        |status| groups.iter().flat_map(|(_, cs)| cs).filter(|c| c.status == status).count();
    serde_json::json!({
        "paths": paths.iter().map(|(label, path)| {
            serde_json::json!({ "label": label, "path": path, "exists": path.exists() })
        }).collect::<Vec<_>>(),
        "groups": groups.iter().map(|(name, checks)| {
            serde_json::json!({ "name": name, "checks": checks })
        }).collect::<Vec<_>>(),
        "warnings": count(Status::Warn),
        "failures": count(Status::Fail),
    })
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
    fn path_report_labels_and_derivation() {
        let cfg = PathBuf::from("/cfg/config.toml");
        let dev = PathBuf::from("/state/device_id");
        let server_dir = PathBuf::from("/sock");
        let ctl = server_dir.join("ctl.sock");

        let report = path_report(cfg.clone(), dev.clone(), &ctl, &server_dir);

        assert_eq!(
            report,
            vec![
                ("config file", cfg),
                ("socket dir", server_dir.clone()),
                ("server socket", ctl),
                ("device id", dev),
                ("server log", PathBuf::from("/sock/daemon.log")),
                ("server output", PathBuf::from("/sock/daemon.out")),
            ]
        );
    }

    #[test]
    fn path_report_follows_ctl_socket_override() {
        // A --ctl-socket override points logs/sockets at the override's dir.
        let server_dir = PathBuf::from("/custom/dir");
        let ctl = PathBuf::from("/custom/dir/my.sock");
        let report =
            path_report(PathBuf::from("/cfg.toml"), PathBuf::from("/dev"), &ctl, &server_dir);

        let log = report.iter().find(|(l, _)| *l == "server log").unwrap();
        assert_eq!(log.1, PathBuf::from("/custom/dir/daemon.log"));
        let sock = report.iter().find(|(l, _)| *l == "server socket").unwrap();
        assert_eq!(sock.1, ctl);
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

    // --- check_tunnel_residue ---

    #[test]
    fn tunnel_residue_flags_lockless_sidecars() {
        let dir = tempfile::tempdir().unwrap();
        // Dead tunnel: sock + pid, no lock. Tunnel name contains dots and
        // hyphens (the realistic case).
        std::fs::write(dir.path().join("connect-coder.chip-the-human.sock"), "").unwrap();
        std::fs::write(dir.path().join("connect-coder.chip-the-human.pid"), "123").unwrap();
        let checks = check_tunnel_residue(dir.path());
        assert_eq!(checks.len(), 1);
        assert_eq!(checks[0].status, Status::Warn);
        assert!(checks[0].message.contains("coder.chip-the-human"));
        assert!(checks[0].message.contains(".sock"));
        assert!(checks[0].message.contains(".pid"));
        assert_eq!(checks[0].hint.as_deref(), Some("gritty tunnel-destroy coder.chip-the-human"));
    }

    #[test]
    fn tunnel_residue_skips_locked_tunnels() {
        let dir = tempfile::tempdir().unwrap();
        // A tunnel with a .lock file belongs to check_tunnels, not residue.
        std::fs::write(dir.path().join("connect-devbox.sock"), "").unwrap();
        std::fs::write(dir.path().join("connect-devbox.lock"), "").unwrap();
        assert!(check_tunnel_residue(dir.path()).is_empty());
    }

    #[test]
    fn tunnel_residue_ignores_deliberate_survivors() {
        let dir = tempfile::tempdir().unwrap();
        // Logs and the remote-sock cache survive teardown by design.
        std::fs::write(dir.path().join("connect-old.log"), "").unwrap();
        std::fs::write(dir.path().join("connect-old.out"), "").unwrap();
        std::fs::write(dir.path().join("connect-old.remote-sock"), "").unwrap();
        assert!(check_tunnel_residue(dir.path()).is_empty());
    }

    // --- is_known_artifact ---

    #[test]
    fn known_artifacts_match_full_inventory() {
        // One representative per row of the docs/internals.md inventory table.
        for name in [
            "ctl.sock",
            "daemon.pid",
            "daemon.info",
            "daemon.log",
            "daemon.out",
            "daemon.log.1", // external rotation
            "gritty-open",
            "agent-7.sock",
            "svc-7.sock",
            "fwd-coder.chip-the-human-3.sock",
            "connect-devbox.sock",
            "connect-devbox.pid",
            "connect-devbox.info",
            "connect-devbox.lock",
            "connect-devbox.dest",
            "connect-devbox.ssh-opts",
            "connect-devbox.remote-sock",
            "connect-devbox.log",
            "connect-devbox.out",
            "connect-devbox.log.1.gz",     // rotated tunnel log
            "connect-my.dotted.host.sock", // tunnel name containing dots
            "ctl.sock.bindlock",           // bindlock companions classify by base
            "connect-devbox.sock.bindlock",
            "fwd-devbox-3.sock.bindlock",
        ] {
            assert!(is_known_artifact(name), "{name} should be known");
        }
    }

    #[test]
    fn unknown_artifacts_rejected() {
        for name in [
            "daemon.state",        // plausible future sidecar
            "agent-xyz.sock",      // non-numeric session id
            "svc-.sock",           // empty session id
            "connect-devbox.toml", // unrecognized tunnel extension
            "random-file",
            "ctl.sock.bak",
            ".bindlock", // bare suffix, empty base
        ] {
            assert!(!is_known_artifact(name), "{name} should be unknown");
        }
    }

    // --- check_unknown_files ---

    #[test]
    fn unknown_files_reported_not_removed_by_default() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("daemon.state"), "").unwrap();
        std::fs::write(dir.path().join("ctl.sock"), "").unwrap(); // known, skipped
        let checks = check_unknown_files(dir.path(), None, false);
        assert_eq!(checks.len(), 1);
        assert_eq!(checks[0].status, Status::Warn);
        assert!(checks[0].message.contains("daemon.state"));
        assert!(checks[0].hint.as_deref().unwrap().contains("--clean"));
        assert!(dir.path().join("daemon.state").exists());
    }

    #[test]
    fn clean_removes_unknown_keeps_known() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("daemon.state"), "").unwrap();
        std::fs::write(dir.path().join("daemon.pid"), "1").unwrap();
        let checks = check_unknown_files(dir.path(), None, true);
        assert_eq!(checks.len(), 1);
        assert_eq!(checks[0].status, Status::Ok);
        assert!(checks[0].message.contains("removed unknown file: daemon.state"));
        assert!(!dir.path().join("daemon.state").exists());
        assert!(dir.path().join("daemon.pid").exists());
    }

    #[test]
    fn clean_spares_live_sockets_and_directories() {
        let dir = tempfile::tempdir().unwrap();
        // A socket with a live listener: in use, whoever created it.
        let _listener =
            std::os::unix::net::UnixListener::bind(dir.path().join("mystery.sock")).unwrap();
        std::fs::create_dir(dir.path().join("mystery-dir")).unwrap();
        let checks = check_unknown_files(dir.path(), None, true);
        assert_eq!(checks.len(), 2);
        assert!(checks.iter().all(|c| c.status == Status::Warn));
        assert!(checks.iter().any(|c| c.message.contains("unknown directory: mystery-dir")));
        assert!(checks.iter().any(|c| c.message.contains("unknown socket in use: mystery.sock")));
        assert!(dir.path().join("mystery.sock").exists());
        assert!(dir.path().join("mystery-dir").exists());
    }

    #[test]
    fn clean_removes_dead_unknown_socket() {
        let dir = tempfile::tempdir().unwrap();
        // Bind then drop the listener: the socket file remains but nothing
        // serves it -- removable litter.
        drop(std::os::unix::net::UnixListener::bind(dir.path().join("mystery.sock")).unwrap());
        let checks = check_unknown_files(dir.path(), None, true);
        assert_eq!(checks.len(), 1);
        assert_eq!(checks[0].status, Status::Ok);
        assert!(!dir.path().join("mystery.sock").exists());
    }

    #[test]
    fn ctl_socket_override_and_its_bindlock_are_known() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("my.sock"), "").unwrap();
        std::fs::write(dir.path().join("my.sock.bindlock"), "").unwrap();
        assert!(check_unknown_files(dir.path(), Some("my.sock"), false).is_empty());
        // Without the override hint the same files are unknown.
        assert_eq!(check_unknown_files(dir.path(), None, false).len(), 2);
    }

    // --- check_stale_bindlocks ---

    #[test]
    fn stale_bindlocks_counted_when_socket_gone() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("fwd-devbox-3.sock.bindlock"), "").unwrap();
        std::fs::write(dir.path().join("fwd-devbox-9.sock.bindlock"), "").unwrap();
        let checks = check_stale_bindlocks(dir.path());
        assert_eq!(checks.len(), 1);
        assert_eq!(checks[0].status, Status::Warn);
        assert!(checks[0].message.contains("2 stale bind-lock files"));
    }

    #[test]
    fn bindlock_with_live_socket_not_stale() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("fwd-devbox-3.sock"), "").unwrap();
        std::fs::write(dir.path().join("fwd-devbox-3.sock.bindlock"), "").unwrap();
        assert!(check_stale_bindlocks(dir.path()).is_empty());
    }

    // --- check_clients ---

    #[test]
    fn clients_reports_connectable_fwd_sockets() {
        let dir = tempfile::tempdir().unwrap();
        // A real listener = a live client. Host names contain dots and hyphens;
        // the session id is after the last hyphen.
        let _listener = std::os::unix::net::UnixListener::bind(
            dir.path().join("fwd-fate.x.pattern-net-14.sock"),
        )
        .unwrap();
        let checks = check_clients(dir.path());
        assert_eq!(checks.len(), 1);
        assert_eq!(checks[0].status, Status::Ok);
        assert!(checks[0].message.contains("fate.x.pattern-net:14"));
    }

    #[test]
    fn clients_skips_dead_fwd_sockets() {
        let dir = tempfile::tempdir().unwrap();
        // A plain file (or a socket nobody listens on) is not a live client --
        // check_sockets reports it as orphaned instead.
        std::fs::write(dir.path().join("fwd-devbox-3.sock"), "").unwrap();
        assert!(check_clients(dir.path()).is_empty());
    }
}
