//! `gritty refresh [host]` -- surgical restart of stale long-lived processes.
//!
//! Unlike `gritty restart`, which unconditionally kills and recreates
//! everything for a host, `refresh` first reads each process's `.info`
//! sidecar (see the `runinfo` module) and only restarts what is actually
//! running older code than the binary on disk. Running it twice in a row is
//! a no-op the second time.
//!
//! `refresh local` compares the local daemon's `.info` against the on-disk
//! binary, restarting only if stale. `refresh <host>` additionally compares
//! the tunnel supervisor's `.info` and -- crucially -- delegates the remote
//! daemon check to `gritty refresh local` *run on the remote*, so the remote
//! daemon is measured against its own on-disk binary, not ours. This makes
//! the common post-upgrade dance a single idempotent verb that works for
//! source-built remotes where `bootstrap` doesn't apply.

use std::path::{Path, PathBuf};

use gritty::runinfo::{RunInfo, Staleness};

use super::util;
use gritty::ui;

/// What `refresh` decided about a long-lived process after reading its
/// `.info` sidecar (or noticing one is absent).
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Verdict {
    /// Not running -- nothing to refresh.
    NotRunning,
    /// Running and matching the on-disk binary. No action needed.
    Current,
    /// Running older code than the on-disk binary. Restart required.
    Stale(Staleness),
    /// Running, but predates the `.info` sidecar, so we can't tell what
    /// version it is. Since the sidecar landed alongside a protocol bump,
    /// a running process without one is definitionally behind -- treat as
    /// stale.
    Unknown,
}

impl std::fmt::Display for Verdict {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Verdict::NotRunning => write!(f, "not running"),
            Verdict::Current => write!(f, "up to date"),
            Verdict::Stale(s) => write!(f, "{s}"),
            Verdict::Unknown => write!(f, "running, version unknown (predates .info)"),
        }
    }
}

impl Verdict {
    pub(crate) fn needs_restart(&self) -> bool {
        matches!(self, Verdict::Stale(_) | Verdict::Unknown)
    }
}

/// Decide whether a long-lived process needs restarting, from its `.info`
/// sidecar and a liveness probe. `alive` should check the process's PID file
/// (or equivalent); an orphaned `.info` left behind by a SIGKILL'd process
/// must not be trusted.
pub(crate) fn assess(info_path: &Path, alive: impl FnOnce() -> bool) -> Verdict {
    if !alive() {
        return Verdict::NotRunning;
    }
    match RunInfo::read(info_path) {
        Ok(info) => match info.staleness_vs_current() {
            None => Verdict::Current,
            Some(s) => Verdict::Stale(s),
        },
        // Running but no `.info` sidecar: predates this feature, so it is
        // definitionally behind the binary on disk.
        Err(_) => Verdict::Unknown,
    }
}

/// True if the given PID file names a live process.
fn pid_file_alive(pid_path: &Path) -> bool {
    std::fs::read_to_string(pid_path)
        .ok()
        .and_then(|s| s.trim().parse::<i32>().ok())
        .is_some_and(|pid| unsafe { libc::kill(pid, 0) == 0 })
}

/// Refresh the local daemon if it's running stale code, then reap any
/// orphaned daemons. Never auto-starts a daemon that wasn't already running
/// -- refresh is about picking up a new binary, not about bringing services
/// up.
async fn refresh_local(ctl_socket: Option<PathBuf>) -> anyhow::Result<()> {
    // Snapshot the override before resolve_ctl_path consumes it, so the
    // respawn below lands on the same socket.
    let ctl_socket_arg = ctl_socket.as_ref().map(|p| p.to_string_lossy().into_owned());
    let ctl_path = util::resolve_ctl_path(ctl_socket, Some("local"))?;
    let info_path = gritty::runinfo::daemon_info_path(&ctl_path);
    let pid_path = ctl_path.with_file_name("daemon.pid");

    let verdict = assess(&info_path, || pid_file_alive(&pid_path));
    ui::detail(&format!("local daemon: {verdict}"));
    if verdict.needs_restart() {
        use gritty::protocol::Frame;
        match util::server_request_any_version(&ctl_path, Frame::KillServer).await {
            Ok(Frame::Ok) => {}
            Ok(Frame::Error { message, .. }) => {
                ui::status(&format!("kill-server: {message} (continuing)"));
            }
            Ok(other) => {
                ui::status(&format!("kill-server: unexpected response {other:?} (continuing)"));
            }
            Err(_) => {
                // Can't reach the daemon (crashed? socket gone?). The PID check
                // said it was alive, so the socket is wedged -- hit it with
                // SIGTERM and move on. Best-effort.
                if let Some(pid) = std::fs::read_to_string(&pid_path)
                    .ok()
                    .and_then(|s| s.trim().parse::<i32>().ok())
                {
                    unsafe { libc::kill(pid, libc::SIGTERM) };
                }
            }
        }
        util::auto_start(&util::server_auto_start_args(ctl_socket_arg.as_deref()))?;
        ui::success("local daemon restarted");
    }

    // Orphan reaping is independent of the registered daemon's verdict --
    // orphans are by definition *not* the daemon registered in the socket
    // dir. They are the processes users previously had to `kill` by hand.
    reap_orphans().await;
    Ok(())
}

/// Find and reap orphaned daemons: processes that are running but unreachable
/// because their socket-dir registration was wiped (systemd `$XDG_RUNTIME_DIR`
/// teardown, `/tmp` sweeps) or taken over by a newer daemon.
///
/// Suspects get a grace period (`procscan::CONFIRM_DELAY`) before the kill:
/// a current-binary daemon self-heals socket loss within its check interval
/// and must be spared. Anything still orphaned after the window genuinely
/// cannot recover -- its sessions are unreachable no matter what -- so
/// reaping it loses nothing.
async fn reap_orphans() {
    use gritty::procscan;

    if !procscan::SUPPORTED {
        return;
    }
    let suspects = procscan::find_orphan_daemons();
    if suspects.is_empty() {
        return;
    }
    for s in &suspects {
        ui::warn(&format!("possible orphaned daemon: {s}"));
    }
    ui::detail(&format!(
        "confirming ({}s grace for self-heal)...",
        procscan::CONFIRM_DELAY.as_secs()
    ));
    let reaped = procscan::confirm_and_reap(suspects, procscan::CONFIRM_DELAY).await;
    if reaped.is_empty() {
        ui::success("no orphans confirmed (recovered on their own)");
        return;
    }
    for (orphan, outcome) in reaped {
        match outcome {
            Ok(()) => {
                ui::success(&format!("reaped orphaned daemon pid {}", orphan.pid));
            }
            Err(e) => {
                ui::warn(&format!("could not kill orphan pid {}: {e}", orphan.pid));
            }
        }
    }
}

/// Refresh a remote host: its tunnel supervisor and, via `gritty refresh
/// local` over SSH, its remote daemon.
async fn refresh_remote(host: &str, config: &gritty::config::ConfigFile) -> anyhow::Result<()> {
    // Assess the local tunnel supervisor first (cheap, no network).
    let info_path = gritty::runinfo::connect_info_path(host);
    let supervisor_verdict = assess(&info_path, || {
        gritty::connect::probe_tunnel_status(host) != gritty::connect::TunnelStatus::Stale
    });
    ui::detail(&format!("{host} supervisor: {supervisor_verdict}"));

    if supervisor_verdict == Verdict::NotRunning {
        return Ok(());
    }

    let dest =
        gritty::connect::resolve_destination(host, config.alias_destination(host).as_deref());
    let tun_cfg = config.resolve_tunnel(host);
    // resolve_tunnel only knows config `ssh-options`; the CLI `-o` options the
    // tunnel was created with live in the `.ssh-opts` sidecar. Merge them or a
    // host reachable only via a CLI -o ProxyJump/IdentityFile/Port can't be
    // reached here and its daemon is silently never refreshed. The sidecar is
    // still present -- `disconnect` (below) is what wipes it.
    let ssh_options = gritty::connect::merge_ssh_options(
        &gritty::connect::read_persisted_ssh_options(host),
        &tun_cfg.ssh_options,
    );

    // Delegate the remote daemon check to the remote binary. This keeps the
    // remote the source of truth about its own staleness (its daemon may be
    // stale relative to its own on-disk binary regardless of ours) and works
    // for source-built remotes where `bootstrap` isn't in the picture.
    ui::detail(&format!("{host}: checking remote daemon..."));
    match gritty::connect::run_remote_gritty(
        &dest,
        &["refresh", "local"],
        &ssh_options,
        tun_cfg.connect_timeout,
    )
    .await
    {
        Ok(status) if status.success() => {}
        Ok(_) => {
            // The remote `gritty refresh local` failed. Most likely the
            // remote binary predates `refresh` (or gritty isn't on PATH,
            // which the ssh error path already surfaces). Either way we
            // can't fix it from here.
            ui::warn(&format!(
                "{host}: remote `gritty refresh local` failed -- remote binary may predate \
                 `refresh`; run `gritty restart {host}` or update the remote binary"
            ));
        }
        Err(e) => {
            ui::warn(&format!("{host}: could not reach remote: {e}"));
        }
    }

    // Restart the supervisor last, so its `ensure_remote_ready` sees the
    // (possibly freshly-refreshed) remote daemon and connects cleanly
    // instead of tripping over a half-restarted one.
    if supervisor_verdict.needs_restart() {
        // Capture the recreate args (incl. persisted CLI -o options) before
        // disconnect wipes the sidecars.
        let recreate = gritty::connect::tunnel_recreate_args(host, &dest);
        gritty::connect::disconnect(host).await?;
        let recreate: Vec<&str> = recreate.iter().map(String::as_str).collect();
        util::auto_start(&recreate)?;
        ui::success(&format!("{host} supervisor restarted"));
    }

    // Final end-to-end probe: Hello/HelloAck through the tunnel. The checks
    // above measure each process against its *own* on-disk binary, so they
    // all report "up to date" even when the remote binary itself is an older
    // release than ours -- the one failure mode refresh cannot repair. Catch
    // it here and say exactly what will fix it.
    let local = gritty::protocol::PROTOCOL_VERSION;
    match gritty::connect::probe_socket_protocol(&gritty::connect::tunnel_local_socket_path(host))
        .await
    {
        Ok(remote) if remote == local => {
            ui::success(&format!("{host}: end-to-end protocol verified (v{remote})"));
            Ok(())
        }
        Ok(remote) => anyhow::bail!("{}", remote_binary_outdated_msg(host, remote, local)),
        Err(e) => {
            // A flaky probe (slow SSH, tunnel mid-reconnect) shouldn't fail
            // refresh -- the .info-based work above already happened.
            ui::warn(&format!(
                "{host}: could not verify protocol end to end ({e}); \
                 try `gritty connect {host}` to test the path"
            ));
            Ok(())
        }
    }
}

/// The actionable error for a cross-machine protocol mismatch that `refresh`
/// itself cannot repair: the remote *binary* is older/newer than ours, so
/// restarting processes on either side changes nothing.
fn remote_binary_outdated_msg(host: &str, remote: u16, local: u16) -> String {
    format!(
        "{host}: remote daemon speaks protocol v{remote} but local gritty speaks v{local} -- \
         the remote gritty binary itself is a different release, which refresh cannot fix. \
         Run `gritty bootstrap {host}` (or update gritty on the remote by hand), \
         then `gritty refresh {host}` again"
    )
}

/// `gritty refresh [host]` entrypoint. No host = refresh everything
/// (local daemon + all tunnels).
pub(crate) async fn refresh(
    host: Option<String>,
    ctl_socket: Option<PathBuf>,
    config: &gritty::config::ConfigFile,
) -> anyhow::Result<()> {
    match host.as_deref() {
        Some("local") => refresh_local(ctl_socket).await,
        Some(name) => refresh_remote(name, config).await,
        None => {
            // Best-effort: `refresh` is documented as idempotent, so one
            // unreachable host must not abort the rest. Collect failures and
            // report an aggregate at the end.
            let mut failed = 0usize;
            if let Err(e) = refresh_local(ctl_socket).await {
                ui::error(&format!("refresh local: {e}"));
                failed += 1;
            }
            for name in gritty::connect::enumerate_tunnels() {
                if let Err(e) = refresh_remote(&name, config).await {
                    ui::error(&format!("refresh {name}: {e}"));
                    failed += 1;
                }
            }
            if failed > 0 {
                anyhow::bail!("refresh failed for {failed} target(s) (see errors above)");
            }
            Ok(())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use gritty::protocol::PROTOCOL_VERSION;

    #[test]
    fn assess_not_running_when_no_info_and_dead() {
        let tmp = tempfile::tempdir().unwrap();
        let info = tmp.path().join("x.info");
        assert_eq!(assess(&info, || false), Verdict::NotRunning);
    }

    #[test]
    fn assess_unknown_when_no_info_but_alive() {
        // Process running without a `.info` sidecar predates this feature --
        // definitionally stale, needs a restart to pick up the new binary.
        let tmp = tempfile::tempdir().unwrap();
        let info = tmp.path().join("x.info");
        let v = assess(&info, || true);
        assert_eq!(v, Verdict::Unknown);
        assert!(v.needs_restart());
    }

    #[test]
    fn assess_current_when_info_matches() {
        let tmp = tempfile::tempdir().unwrap();
        let info_path = tmp.path().join("x.info");
        RunInfo::current().write(&info_path).unwrap();
        let v = assess(&info_path, || true);
        assert_eq!(v, Verdict::Current);
        assert!(!v.needs_restart());
    }

    #[test]
    fn assess_stale_on_protocol_mismatch() {
        let tmp = tempfile::tempdir().unwrap();
        let info_path = tmp.path().join("x.info");
        let mut info = RunInfo::current();
        info.protocol = PROTOCOL_VERSION.wrapping_sub(1);
        info.write(&info_path).unwrap();
        let v = assess(&info_path, || true);
        assert!(matches!(v, Verdict::Stale(Staleness::Protocol { .. })));
        assert!(v.needs_restart());
    }

    #[test]
    fn assess_stale_on_build_mismatch() {
        let tmp = tempfile::tempdir().unwrap();
        let info_path = tmp.path().join("x.info");
        let mut info = RunInfo::current();
        info.git_hash = "old-hash".to_string();
        info.write(&info_path).unwrap();
        let v = assess(&info_path, || true);
        assert!(matches!(v, Verdict::Stale(Staleness::Build { .. })));
        assert!(v.needs_restart());
    }

    #[test]
    fn assess_not_running_with_orphaned_info() {
        // `.info` exists but the process is dead (SIGKILL'd, crashed):
        // the orphaned sidecar must not be trusted.
        let tmp = tempfile::tempdir().unwrap();
        let info_path = tmp.path().join("x.info");
        RunInfo::current().write(&info_path).unwrap();
        assert_eq!(assess(&info_path, || false), Verdict::NotRunning);
    }

    #[test]
    fn verdict_display() {
        assert_eq!(Verdict::NotRunning.to_string(), "not running");
        assert_eq!(Verdict::Current.to_string(), "up to date");
        assert!(Verdict::Unknown.to_string().contains("predates"));
    }

    #[test]
    fn outdated_remote_message_is_actionable() {
        // The whole point of the end-to-end probe is to break the loop where
        // every tool reports success while nothing works: the message must
        // name both versions and the exact commands that fix it.
        let msg = remote_binary_outdated_msg("devbox", 21, 22);
        assert!(msg.contains("devbox"));
        assert!(msg.contains("v21"));
        assert!(msg.contains("v22"));
        assert!(msg.contains("gritty bootstrap devbox"));
        assert!(msg.contains("gritty refresh devbox"));
    }
}
