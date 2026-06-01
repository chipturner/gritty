//! Process-table reconciliation: find gritty daemon processes that are
//! running but no longer registered on disk ("orphans").
//!
//! A daemon's identity lives in files next to its control socket
//! (`daemon.pid`, `daemon.info`). External cleanup -- systemd wiping
//! `$XDG_RUNTIME_DIR` on logout, `/tmp` age sweeps -- can delete those files
//! while the daemon keeps running, making it unreachable *and* invisible to
//! `ls`, `doctor`, and `refresh`, all of which trust the socket dir. This
//! module closes that gap by walking the process table and cross-checking
//! each `gritty server` process against the registration next to the socket
//! it has bound.
//!
//! Identification is positive, not heuristic: a process is only considered
//! if its cmdline is a `gritty server` invocation, and it is only an orphan
//! if the `daemon.pid` next to a socket path it actually has bound (per
//! `/proc/net/unix`) is missing or names a different process. Daemons using
//! `--ctl-socket` or `GRITTY_SOCKET_DIR` overrides are therefore handled
//! correctly -- their registration lives next to *their* socket.
//!
//! Linux-only (it reads `/proc`); on other platforms the scan returns empty
//! and [`SUPPORTED`] is false, mirroring the `net_watch` stub pattern.

use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::time::Duration;

/// Whether orphan scanning works on this platform.
pub const SUPPORTED: bool = cfg!(target_os = "linux");

/// Grace period between the suspect scan and the confirming scan in
/// [`confirm_and_reap`]. A current-binary daemon self-heals socket/registration
/// loss within [`crate::daemon::SOCKET_CHECK_INTERVAL`]; anything still
/// orphaned after that window genuinely cannot recover.
pub const CONFIRM_DELAY: Duration =
    crate::daemon::SOCKET_CHECK_INTERVAL.saturating_add(Duration::from_secs(2));

/// A gritty daemon process whose on-disk registration no longer points at it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OrphanDaemon {
    pub pid: u32,
    /// A control-socket path the process has bound (recovered from `/proc`).
    pub bound_path: PathBuf,
    pub reason: OrphanReason,
}

/// Why a running daemon is considered orphaned.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum OrphanReason {
    /// The socket path it bound no longer exists on disk.
    SocketFileGone,
    /// No `daemon.pid` exists next to its bound socket.
    RegistrationMissing,
    /// `daemon.pid` exists but names a different (presumably newer) daemon.
    RegistrationStolen { current_pid: u32 },
}

impl std::fmt::Display for OrphanReason {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            OrphanReason::SocketFileGone => write!(f, "its socket file is gone"),
            OrphanReason::RegistrationMissing => write!(f, "its pid registration is gone"),
            OrphanReason::RegistrationStolen { current_pid } => {
                write!(f, "a newer daemon (pid {current_pid}) owns its socket path")
            }
        }
    }
}

impl std::fmt::Display for OrphanDaemon {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "pid {} (bound {}): {}", self.pid, self.bound_path.display(), self.reason)
    }
}

/// Scan the process table for orphaned gritty daemons owned by the current
/// user. Returns an empty list on unsupported platforms (see [`SUPPORTED`]).
#[cfg(target_os = "linux")]
pub fn find_orphan_daemons() -> Vec<OrphanDaemon> {
    let net_unix = std::fs::read_to_string("/proc/net/unix").unwrap_or_default();
    let inode_to_path = parse_proc_net_unix(&net_unix);
    let my_uid = nix::unistd::geteuid().as_raw();
    let my_pid = std::process::id();

    let Ok(proc_entries) = std::fs::read_dir("/proc") else {
        return Vec::new();
    };
    let mut orphans = Vec::new();
    for entry in proc_entries.flatten() {
        let Some(pid) = entry.file_name().to_str().and_then(|s| s.parse::<u32>().ok()) else {
            continue;
        };
        if pid == my_pid {
            continue;
        }
        // Only processes we own (we could neither inspect nor kill others').
        let Ok(meta) = entry.metadata() else { continue };
        if std::os::unix::fs::MetadataExt::uid(&meta) != my_uid {
            continue;
        }
        // Only `gritty server` daemons, identified by cmdline.
        let Ok(raw) = std::fs::read(format!("/proc/{pid}/cmdline")) else { continue };
        let argv: Vec<String> = raw
            .split(|b| *b == 0)
            .filter(|s| !s.is_empty())
            .map(|s| String::from_utf8_lossy(s).into_owned())
            .collect();
        if !is_gritty_server_cmdline(&argv) {
            continue;
        }
        let bound = bound_unix_paths(pid, &inode_to_path);
        if let Some(orphan) = classify(pid, &bound) {
            orphans.push(orphan);
        }
    }
    orphans
}

/// Stub for platforms without `/proc` (see [`SUPPORTED`]).
#[cfg(not(target_os = "linux"))]
pub fn find_orphan_daemons() -> Vec<OrphanDaemon> {
    Vec::new()
}

/// The filesystem paths of unix sockets this process has open, resolved by
/// joining `/proc/<pid>/fd/*` socket inodes against the `/proc/net/unix`
/// table.
#[cfg(target_os = "linux")]
fn bound_unix_paths(pid: u32, inode_to_path: &HashMap<u64, PathBuf>) -> HashSet<PathBuf> {
    let mut out = HashSet::new();
    let Ok(fds) = std::fs::read_dir(format!("/proc/{pid}/fd")) else {
        return out;
    };
    for fd in fds.flatten() {
        let Ok(target) = std::fs::read_link(fd.path()) else { continue };
        let Some(s) = target.to_str() else { continue };
        // Socket fds read as "socket:[<inode>]".
        let Some(inode) = s
            .strip_prefix("socket:[")
            .and_then(|rest| rest.strip_suffix(']'))
            .and_then(|n| n.parse::<u64>().ok())
        else {
            continue;
        };
        if let Some(path) = inode_to_path.get(&inode) {
            out.insert(path.clone());
        }
    }
    out
}

/// Classify one gritty-server process given the socket paths it has bound.
///
/// Returns `None` (properly registered) if *any* bound path's sibling
/// `daemon.pid` names this pid -- the daemon, its per-session sockets, and its
/// registration all live in the same directory, so one match is proof of
/// ownership. A process with no resolvable bound paths is never accused: we
/// cannot tell where its registration should live.
#[allow(dead_code)] // referenced by the Linux scan; unit-tested everywhere
fn classify(pid: u32, bound_paths: &HashSet<PathBuf>) -> Option<OrphanDaemon> {
    let mut best: Option<OrphanDaemon> = None;
    for path in bound_paths {
        let registered: Option<u32> = std::fs::read_to_string(crate::daemon::pid_file_path(path))
            .ok()
            .and_then(|s| s.trim().parse().ok());
        if registered == Some(pid) {
            return None;
        }
        let reason = if !path.exists() {
            OrphanReason::SocketFileGone
        } else {
            match registered {
                Some(current_pid) => OrphanReason::RegistrationStolen { current_pid },
                None => OrphanReason::RegistrationMissing,
            }
        };
        // Prefer reporting the control socket over per-session sockets.
        let is_ctl = path.file_name().is_some_and(|n| n == "ctl.sock");
        if best.is_none() || is_ctl {
            best = Some(OrphanDaemon { pid, bound_path: path.clone(), reason });
        }
    }
    best
}

/// Confirm suspected orphans by re-scanning after `confirm_delay`, then
/// SIGKILL the ones that are still orphaned. Returns each confirmed orphan
/// with its kill outcome; suspects that recovered (daemon self-heal) are
/// silently dropped.
///
/// SIGKILL -- not SIGTERM -- is deliberate: an orphan's SIGTERM handler runs
/// its normal shutdown, which unlinks whatever is at its old socket path. By
/// the time we reap it, that path may belong to a *newer* daemon, and
/// unlinking it would orphan that daemon's clients in turn.
pub async fn confirm_and_reap(
    suspects: Vec<OrphanDaemon>,
    confirm_delay: Duration,
) -> Vec<(OrphanDaemon, std::io::Result<()>)> {
    if suspects.is_empty() {
        return Vec::new();
    }
    tokio::time::sleep(confirm_delay).await;
    let confirmed: HashSet<(u32, PathBuf)> =
        find_orphan_daemons().into_iter().map(|o| (o.pid, o.bound_path)).collect();
    suspects
        .into_iter()
        .filter(|s| confirmed.contains(&(s.pid, s.bound_path.clone())))
        .map(|s| {
            let outcome = sigkill(s.pid);
            (s, outcome)
        })
        .collect()
}

/// SIGKILL a process by pid.
fn sigkill(pid: u32) -> std::io::Result<()> {
    nix::sys::signal::kill(
        nix::unistd::Pid::from_raw(pid as i32),
        nix::sys::signal::Signal::SIGKILL,
    )
    .map_err(|e| std::io::Error::other(format!("kill {pid}: {e}")))
}

/// Parse `/proc/net/unix` into a map of socket inode -> bound filesystem path.
///
/// Pure and unconditionally compiled so it can be unit-tested on any platform.
/// Abstract-namespace sockets (`@`-prefixed) and anonymous sockets (no path)
/// are skipped.
///
/// Format: `Num RefCount Protocol Flags Type St Inode Path` (path optional).
#[allow(dead_code)] // used by the Linux scan; tested everywhere
fn parse_proc_net_unix(text: &str) -> HashMap<u64, PathBuf> {
    let mut map = HashMap::new();
    for line in text.lines().skip(1) {
        let fields: Vec<&str> = line.split_whitespace().collect();
        if fields.len() < 8 {
            continue; // anonymous socket (no path column) or malformed
        }
        let Ok(inode) = fields[6].parse::<u64>() else { continue };
        let path = fields[7];
        if !path.starts_with('/') {
            continue; // abstract namespace (@...) or garbage
        }
        map.insert(inode, PathBuf::from(path));
    }
    map
}

/// Does this argv look like a `gritty server` daemon invocation?
///
/// Matches `<path-to->gritty [global flags] server [flags]` -- the executable
/// basename must be exactly `gritty` and `server` must appear as an argument.
#[allow(dead_code)]
fn is_gritty_server_cmdline(argv: &[String]) -> bool {
    let Some(argv0) = argv.first() else {
        return false;
    };
    let exe = std::path::Path::new(argv0).file_name().and_then(|n| n.to_str()).unwrap_or("");
    exe == "gritty" && argv.iter().skip(1).any(|a| a == "server")
}

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE_PROC_NET_UNIX: &str = "\
Num       RefCount Protocol Flags    Type St Inode Path
ffff888104b9d000: 00000002 00000000 00010000 0001 01 31337 /run/user/1000/gritty/ctl.sock
ffff888104b9d400: 00000002 00000000 00010000 0001 01 31338 /tmp/gritty-1000/ctl.sock
ffff888104b9d800: 00000002 00000000 00010000 0001 01 31339 @/abstract/socket
ffff888104b9dc00: 00000003 00000000 00000000 0001 03 31340
ffff888104b9e000: 00000002 00000000 00010000 0001 01 notanumber /tmp/bad.sock
";

    #[test]
    fn parse_proc_net_unix_maps_inode_to_path() {
        let map = parse_proc_net_unix(SAMPLE_PROC_NET_UNIX);
        assert_eq!(map.get(&31337), Some(&PathBuf::from("/run/user/1000/gritty/ctl.sock")));
        assert_eq!(map.get(&31338), Some(&PathBuf::from("/tmp/gritty-1000/ctl.sock")));
    }

    #[test]
    fn parse_proc_net_unix_skips_abstract_anonymous_and_malformed() {
        let map = parse_proc_net_unix(SAMPLE_PROC_NET_UNIX);
        assert_eq!(map.len(), 2, "only path-bound sockets should be kept: {map:?}");
        assert!(!map.contains_key(&31339), "abstract sockets must be skipped");
        assert!(!map.contains_key(&31340), "anonymous sockets must be skipped");
    }

    #[test]
    fn parse_proc_net_unix_empty_input() {
        assert!(parse_proc_net_unix("").is_empty());
        assert!(parse_proc_net_unix("Num RefCount Protocol Flags Type St Inode Path\n").is_empty());
    }

    fn argv(args: &[&str]) -> Vec<String> {
        args.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn cmdline_plain_server_matches() {
        assert!(is_gritty_server_cmdline(&argv(&["/usr/local/bin/gritty", "server"])));
        assert!(is_gritty_server_cmdline(&argv(&["gritty", "server"])));
        assert!(is_gritty_server_cmdline(&argv(&["gritty", "server", "-f"])));
    }

    #[test]
    fn cmdline_with_global_flags_matches() {
        assert!(is_gritty_server_cmdline(&argv(&[
            "/home/u/.cargo/bin/gritty",
            "--ctl-socket",
            "/tmp/x.sock",
            "server"
        ])));
        assert!(is_gritty_server_cmdline(&argv(&["gritty", "--verbose", "server"])));
    }

    #[test]
    fn cmdline_non_server_subcommands_do_not_match() {
        assert!(!is_gritty_server_cmdline(&argv(&["gritty", "connect", "local:0"])));
        assert!(!is_gritty_server_cmdline(&argv(&["gritty", "ls", "local"])));
        assert!(!is_gritty_server_cmdline(&argv(&["gritty", "tunnel-create", "host"])));
    }

    #[test]
    fn cmdline_other_binaries_do_not_match() {
        // Another program with "server" in its args must not be flagged.
        assert!(!is_gritty_server_cmdline(&argv(&["/usr/bin/python3", "server"])));
        assert!(!is_gritty_server_cmdline(&argv(&["nginx", "server"])));
        // A path that merely contains "gritty" is not gritty.
        assert!(!is_gritty_server_cmdline(&argv(&["/opt/gritty-monitor", "server"])));
        assert!(!is_gritty_server_cmdline(&argv(&[])));
    }

    #[test]
    fn confirm_delay_exceeds_daemon_check_interval() {
        // The reap grace period must outlast the daemon's self-heal interval,
        // or refresh could kill a daemon that was about to recover.
        assert!(CONFIRM_DELAY > crate::daemon::SOCKET_CHECK_INTERVAL);
    }

    /// Build a socket-dir fixture: a fake bound socket path plus an optional
    /// registered pid.
    fn fixture(registered_pid: Option<u32>, socket_exists: bool) -> (tempfile::TempDir, PathBuf) {
        let tmp = tempfile::tempdir().unwrap();
        let sock = tmp.path().join("ctl.sock");
        if socket_exists {
            std::fs::write(&sock, b"").unwrap();
        }
        if let Some(pid) = registered_pid {
            std::fs::write(tmp.path().join("daemon.pid"), pid.to_string()).unwrap();
        }
        (tmp, sock)
    }

    fn paths(p: &std::path::Path) -> HashSet<PathBuf> {
        HashSet::from([p.to_path_buf()])
    }

    #[test]
    fn classify_registered_daemon_is_not_orphan() {
        let (_tmp, sock) = fixture(Some(4242), true);
        assert_eq!(classify(4242, &paths(&sock)), None);
    }

    #[test]
    fn classify_missing_registration() {
        let (_tmp, sock) = fixture(None, true);
        let orphan = classify(4242, &paths(&sock)).expect("should be orphan");
        assert_eq!(orphan.reason, OrphanReason::RegistrationMissing);
        assert_eq!(orphan.pid, 4242);
        assert_eq!(orphan.bound_path, sock);
    }

    #[test]
    fn classify_stolen_registration() {
        let (_tmp, sock) = fixture(Some(9999), true);
        let orphan = classify(4242, &paths(&sock)).expect("should be orphan");
        assert_eq!(orphan.reason, OrphanReason::RegistrationStolen { current_pid: 9999 });
    }

    #[test]
    fn classify_socket_file_gone() {
        let (_tmp, sock) = fixture(None, false);
        let orphan = classify(4242, &paths(&sock)).expect("should be orphan");
        assert_eq!(orphan.reason, OrphanReason::SocketFileGone);
    }

    #[test]
    fn classify_no_bound_paths_is_never_accused() {
        // A process with no resolvable bound sockets cannot be cross-checked;
        // it must not be flagged (let alone killed) on a guess.
        assert_eq!(classify(4242, &HashSet::new()), None);
    }

    #[test]
    fn classify_any_matching_registration_clears_all_paths() {
        // The daemon binds per-session sockets next to ctl.sock; one matching
        // registration is proof of ownership for the lot.
        let (tmp, sock) = fixture(Some(4242), true);
        let session_sock = tmp.path().join("agent-1.sock");
        std::fs::write(&session_sock, b"").unwrap();
        let bound = HashSet::from([sock, session_sock]);
        assert_eq!(classify(4242, &bound), None);
    }

    #[test]
    fn classify_prefers_reporting_ctl_socket() {
        let (tmp, sock) = fixture(None, true);
        let session_sock = tmp.path().join("agent-1.sock");
        std::fs::write(&session_sock, b"").unwrap();
        let bound = HashSet::from([sock.clone(), session_sock]);
        let orphan = classify(4242, &bound).expect("should be orphan");
        assert_eq!(orphan.bound_path, sock, "report should name ctl.sock, not a session socket");
    }
}
