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

/// Refresh the local daemon if it's running stale code. Never auto-starts a
/// daemon that wasn't already running -- refresh is about picking up a new
/// binary, not about bringing services up.
async fn refresh_local(ctl_socket: Option<PathBuf>) -> anyhow::Result<()> {
    // Snapshot the override before resolve_ctl_path consumes it, so the
    // respawn below lands on the same socket.
    let ctl_socket_arg = ctl_socket.as_ref().map(|p| p.to_string_lossy().into_owned());
    let ctl_path = util::resolve_ctl_path(ctl_socket, Some("local"))?;
    let info_path = gritty::runinfo::daemon_info_path(&ctl_path);
    let pid_path = ctl_path.with_file_name("daemon.pid");

    let verdict = assess(&info_path, || pid_file_alive(&pid_path));
    eprintln!("\x1b[2m\u{25b8} local daemon: {verdict}\x1b[0m");
    if !verdict.needs_restart() {
        return Ok(());
    }

    use gritty::protocol::Frame;
    match util::server_request_any_version(&ctl_path, Frame::KillServer).await {
        Ok(Frame::Ok) => {}
        Ok(Frame::Error { message, .. }) => {
            eprintln!("\x1b[2;33m\u{25b8} kill-server: {message} (continuing)\x1b[0m");
        }
        Ok(other) => {
            eprintln!(
                "\x1b[2;33m\u{25b8} kill-server: unexpected response {other:?} (continuing)\x1b[0m"
            );
        }
        Err(_) => {
            // Can't reach the daemon (crashed? socket gone?). The PID check
            // said it was alive, so the socket is wedged -- hit it with
            // SIGTERM and move on. Best-effort.
            if let Some(pid) =
                std::fs::read_to_string(&pid_path).ok().and_then(|s| s.trim().parse::<i32>().ok())
            {
                unsafe { libc::kill(pid, libc::SIGTERM) };
            }
        }
    }
    util::auto_start(&util::server_auto_start_args(ctl_socket_arg.as_deref()))?;
    eprintln!("\x1b[32m\u{25b8} local daemon restarted\x1b[0m");
    Ok(())
}

/// SSH destination for a tunnel, from the `.dest` sidecar the supervisor
/// wrote at create time. Falls back to the connection name so a missing
/// sidecar degrades to "try the name as a hostname" rather than failing.
fn tunnel_destination(host: &str) -> String {
    std::fs::read_to_string(gritty::connect::connect_dest_path(host))
        .ok()
        .and_then(|s| {
            let t = s.trim();
            (!t.is_empty()).then(|| t.to_string())
        })
        .unwrap_or_else(|| host.to_string())
}

/// Refresh a remote host: its tunnel supervisor and, via `gritty refresh
/// local` over SSH, its remote daemon.
async fn refresh_remote(host: &str, config: &gritty::config::ConfigFile) -> anyhow::Result<()> {
    // Assess the local tunnel supervisor first (cheap, no network).
    let info_path = gritty::runinfo::connect_info_path(host);
    let supervisor_verdict = assess(&info_path, || {
        gritty::connect::probe_tunnel_status(host) != gritty::connect::TunnelStatus::Stale
    });
    eprintln!("\x1b[2m\u{25b8} {host} supervisor: {supervisor_verdict}\x1b[0m");

    if supervisor_verdict == Verdict::NotRunning {
        return Ok(());
    }

    let dest = tunnel_destination(host);
    let tun_cfg = config.resolve_tunnel(host);

    // Delegate the remote daemon check to the remote binary. This keeps the
    // remote the source of truth about its own staleness (its daemon may be
    // stale relative to its own on-disk binary regardless of ours) and works
    // for source-built remotes where `bootstrap` isn't in the picture.
    eprintln!("\x1b[2m\u{25b8} {host}: checking remote daemon...\x1b[0m");
    match gritty::connect::run_remote_gritty(
        &dest,
        &["refresh", "local"],
        &tun_cfg.ssh_options,
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
            eprintln!(
                "\x1b[33m\u{25b8} {host}: remote `gritty refresh local` failed -- \
                 remote binary may predate `refresh`; run `gritty restart {host}` \
                 or update the remote binary\x1b[0m"
            );
        }
        Err(e) => {
            eprintln!("\x1b[33m\u{25b8} {host}: could not reach remote: {e}\x1b[0m");
        }
    }

    // Restart the supervisor last, so its `ensure_remote_ready` sees the
    // (possibly freshly-refreshed) remote daemon and connects cleanly
    // instead of tripping over a half-restarted one.
    if supervisor_verdict.needs_restart() {
        gritty::connect::disconnect(host).await?;
        util::auto_start(&["tunnel-create", "--name", host, &dest])?;
        eprintln!("\x1b[32m\u{25b8} {host} supervisor restarted\x1b[0m");
    }
    Ok(())
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
                eprintln!("error: refresh local: {e}");
                failed += 1;
            }
            for name in gritty::connect::enumerate_tunnels() {
                if let Err(e) = refresh_remote(&name, config).await {
                    eprintln!("error: refresh {name}: {e}");
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
}
