//! Integration tests for orphaned-daemon detection and reaping.
//!
//! These spawn the real `gritty` binary (a self-daemonizing `gritty server`)
//! so the scan exercises real `/proc` data: cmdline matching, fd-to-socket
//! correlation, and pid-file cross-checks. Linux-only, like the scan itself.
#![cfg(target_os = "linux")]

use std::path::Path;
use std::time::Duration;

use gritty::procscan::{self, OrphanReason};

/// Kills the daemon on drop so failed tests don't leak processes.
struct DaemonGuard {
    pid: u32,
}

impl Drop for DaemonGuard {
    fn drop(&mut self) {
        let _ = nix::sys::signal::kill(
            nix::unistd::Pid::from_raw(self.pid as i32),
            nix::sys::signal::Signal::SIGKILL,
        );
    }
}

/// Spawn a real `gritty server` with its socket dir inside a tempdir.
///
/// `check_secs` controls the daemon's socket self-check interval: large to
/// park self-heal out of the way (deterministic orphan tests), small to
/// exercise it.
fn spawn_real_daemon(check_secs: u64) -> (tempfile::TempDir, DaemonGuard) {
    let tmp = tempfile::tempdir().unwrap();
    let status = std::process::Command::new(env!("CARGO_BIN_EXE_gritty"))
        .arg("server")
        .env("GRITTY_SOCKET_DIR", tmp.path())
        .env("GRITTY_SOCKET_CHECK_SECS", check_secs.to_string())
        .status()
        .expect("failed to launch gritty server");
    assert!(status.success(), "gritty server launcher failed: {status}");

    // The launcher exits once the daemonized child is ready; its pid is in
    // daemon.pid.
    let pid_file = tmp.path().join("daemon.pid");
    let deadline = std::time::Instant::now() + Duration::from_secs(10);
    loop {
        if let Ok(s) = std::fs::read_to_string(&pid_file)
            && let Ok(pid) = s.trim().parse::<u32>()
        {
            return (tmp, DaemonGuard { pid });
        }
        assert!(std::time::Instant::now() < deadline, "daemon.pid never appeared");
        std::thread::sleep(Duration::from_millis(50));
    }
}

/// Find our daemon (by socket dir) in a scan result.
fn scan_for(dir: &Path, pid: u32) -> Option<procscan::OrphanDaemon> {
    procscan::find_orphan_daemons()
        .into_iter()
        .find(|o| o.pid == pid && o.bound_path.starts_with(dir))
}

/// Poll a condition until it holds or the timeout expires.
fn wait_until(timeout: Duration, mut cond: impl FnMut() -> bool) -> bool {
    let deadline = std::time::Instant::now() + timeout;
    loop {
        if cond() {
            return true;
        }
        if std::time::Instant::now() >= deadline {
            return false;
        }
        std::thread::sleep(Duration::from_millis(100));
    }
}

#[test]
fn registered_daemon_is_not_an_orphan() {
    let (tmp, guard) = spawn_real_daemon(3600);
    assert!(
        scan_for(tmp.path(), guard.pid).is_none(),
        "freshly started, properly registered daemon must not be flagged"
    );
}

#[test]
fn daemon_with_deleted_registration_is_orphan() {
    // Self-heal parked (3600s) so the deleted registration stays deleted.
    let (tmp, guard) = spawn_real_daemon(3600);

    std::fs::remove_file(tmp.path().join("daemon.pid")).unwrap();

    let orphan = scan_for(tmp.path(), guard.pid)
        .expect("daemon with deleted pid registration must be flagged as orphan");
    assert_eq!(orphan.reason, OrphanReason::RegistrationMissing);
}

#[test]
fn daemon_with_deleted_socket_is_orphan() {
    let (tmp, guard) = spawn_real_daemon(3600);

    // Full external wipe: socket, pid file, the lot.
    std::fs::remove_file(tmp.path().join("ctl.sock")).unwrap();
    std::fs::remove_file(tmp.path().join("daemon.pid")).unwrap();

    let orphan = scan_for(tmp.path(), guard.pid)
        .expect("daemon whose socket was wiped must be flagged as orphan");
    assert_eq!(orphan.reason, OrphanReason::SocketFileGone);
}

#[test]
fn daemon_with_stolen_registration_is_orphan() {
    let (tmp, guard) = spawn_real_daemon(3600);

    // A "newer daemon" (simulated) overwrites the registration.
    std::fs::write(tmp.path().join("daemon.pid"), "999999").unwrap();

    let orphan = scan_for(tmp.path(), guard.pid)
        .expect("daemon whose registration was overwritten must be flagged as orphan");
    assert_eq!(orphan.reason, OrphanReason::RegistrationStolen { current_pid: 999999 });
}

#[test]
fn current_daemon_self_heals_registration_and_is_spared() {
    // Fast self-heal (1s): the daemon repairs a deleted pid file on its next
    // check, so it must drop out of the orphan list on its own.
    let (tmp, guard) = spawn_real_daemon(1);

    std::fs::remove_file(tmp.path().join("daemon.pid")).unwrap();

    assert!(
        wait_until(Duration::from_secs(10), || scan_for(tmp.path(), guard.pid).is_none()
            && tmp.path().join("daemon.pid").exists()),
        "daemon should have repaired its own registration"
    );
}

#[tokio::test]
async fn confirm_and_reap_kills_persistent_orphan() {
    // Self-heal parked: this orphan can never recover, so reap must kill it.
    let (tmp, guard) = spawn_real_daemon(3600);
    let pid = guard.pid;

    std::fs::remove_file(tmp.path().join("ctl.sock")).unwrap();
    std::fs::remove_file(tmp.path().join("daemon.pid")).unwrap();

    let suspects: Vec<_> =
        procscan::find_orphan_daemons().into_iter().filter(|o| o.pid == pid).collect();
    assert_eq!(suspects.len(), 1, "expected exactly our orphan: {suspects:?}");

    let reaped = procscan::confirm_and_reap(suspects, Duration::from_millis(200)).await;
    assert_eq!(reaped.len(), 1, "the persistent orphan must be confirmed and reaped");
    assert!(reaped[0].1.is_ok(), "kill should succeed: {:?}", reaped[0].1);

    // The process must actually be gone (poll: SIGKILL delivery is async).
    assert!(
        wait_until(Duration::from_secs(5), || {
            nix::sys::signal::kill(nix::unistd::Pid::from_raw(pid as i32), None).is_err()
        }),
        "orphan pid {pid} still alive after reap"
    );
}

#[tokio::test]
async fn confirm_and_reap_spares_recovering_daemon() {
    // Fast self-heal (1s): the suspect recovers during the confirm window and
    // must NOT be killed.
    let (tmp, guard) = spawn_real_daemon(1);
    let pid = guard.pid;

    std::fs::remove_file(tmp.path().join("daemon.pid")).unwrap();

    // Race to catch it as a suspect before it self-heals; if we lose the race
    // the suspect list is empty and the test trivially passes (also fine).
    let suspects: Vec<_> =
        procscan::find_orphan_daemons().into_iter().filter(|o| o.pid == pid).collect();

    let reaped = procscan::confirm_and_reap(suspects, Duration::from_secs(3)).await;
    assert!(reaped.is_empty(), "recovering daemon must be spared, but was reaped: {reaped:?}");

    // And it must still be alive.
    assert!(
        nix::sys::signal::kill(nix::unistd::Pid::from_raw(pid as i32), None).is_ok(),
        "daemon was killed despite recovering"
    );
}
