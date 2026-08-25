//! The SSH tunnel supervisor (`connect.rs`) driven end to end with a scripted
//! `ssh` -- no sshd, no Docker, deterministic failure injection.
//!
//! `connect.rs` finds `ssh` on `PATH`, so a shell script first on `PATH` is a
//! real supervisor with real sidecars, real backoff, and a real `-L` bridge
//! (socat). The script's behavior is chosen per invocation by a mode file the
//! test rewrites between steps, which is how a tunnel that came up healthy is
//! made to die transiently, die non-transiently, or refuse to respawn.
//!
//! The "remote" is this machine: `remote_exec` forwards `GRITTY_SOCKET_DIR`
//! into the remote command, so the remote daemon the tunnel reaches is the
//! daemon in the test's own socket dir (reached through `connect-NAME.sock`
//! rather than `ctl.sock`). The container suite covers a genuinely remote
//! daemon; this file covers the state machine in docs/tunnel-state-machine.md.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Output, Stdio};
use std::time::{Duration, Instant};

use nix::sys::signal::{Signal, kill};
use nix::unistd::Pid;

const GRITTY: &str = env!("CARGO_BIN_EXE_gritty");
const WAIT: Duration = Duration::from_secs(15);
const NAME: &str = "fakehost";

/// socat is a hard requirement of this suite: a missing tool must fail the
/// run, never silently pass it. `GRITTY_SOCAT_TEST=0` is the only, explicit,
/// opt-out (for a developer box without socat; CI never sets it).
fn require_socat() {
    if std::env::var("GRITTY_SOCAT_TEST").as_deref() == Ok("0") {
        panic!("GRITTY_SOCAT_TEST=0 set: this suite's socat tests are disabled on this machine");
    }
    let found = std::process::Command::new("socat")
        .arg("-V")
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .is_ok();
    assert!(
        found,
        "socat not found on PATH -- install it (apt/brew install socat); these tests do not skip"
    );
}

/// The scripted ssh. `$FAKE_SSH_DIR/mode` selects behavior for the *next*
/// invocation; `$FAKE_SSH_DIR/calls` records every argv; a `-L` bridge writes
/// its pid to `$FAKE_SSH_DIR/bridge.pid` so tests can kill "ssh".
const FAKE_SSH: &str = r#"#!/bin/sh
echo "$*" >> "$FAKE_SSH_DIR/calls"
mode=$(cat "$FAKE_SSH_DIR/mode" 2>/dev/null || echo ok)
prev=""; fwd=""
for a in "$@"; do [ "$prev" = "-L" ] && fwd=$a; prev=$a; done
for a; do last=$a; done
if [ -n "$fwd" ]; then
    case $mode in
        spawn-fail)
            echo "ssh: connect to host fakehost port 22: Network is unreachable" >&2
            exit 255 ;;
        spawn-die-nontransient)
            # Bind, live briefly, then exit like an auth/config error would.
            socat "UNIX-LISTEN:${fwd%%:*},fork" "UNIX-CONNECT:${fwd#*:}" &
            sleep 0.5; kill $!
            echo "fakehost: Permission denied (publickey)." >&2
            exit 1 ;;
        *)
            echo $$ > "$FAKE_SSH_DIR/bridge.pid"
            exec socat "UNIX-LISTEN:${fwd%%:*},fork" "UNIX-CONNECT:${fwd#*:}" ;;
    esac
fi
case $mode in
    preflight-fail)
        echo "fakehost: Permission denied (publickey)." >&2
        exit 255 ;;
    ensure-fail)
        [ "$last" = true ] && exit 0
        echo "ERR:gritty socket-path failed on the remote (binary missing or broken? try gritty bootstrap)"
        exit 0 ;;
esac
exec sh -c "$last"
"#;

struct Fixture {
    dir: tempfile::TempDir,
    fake: PathBuf,
    /// Foreground supervisors spawned by the test, killed on drop.
    children: Vec<Child>,
}

impl Fixture {
    fn new() -> Self {
        let dir = tempfile::tempdir().unwrap();
        let fake = dir.path().join("fake");
        fs::create_dir_all(fake.join("bin")).unwrap();
        let ssh = fake.join("bin/ssh");
        fs::write(&ssh, FAKE_SSH).unwrap();
        set_executable(&ssh);
        // The remote command's PATH is rebuilt from GRITTY_BIN_DIR, so the
        // "remote" gritty must live in the same bin dir as the fake ssh.
        std::os::unix::fs::symlink(GRITTY, fake.join("bin/gritty")).unwrap();
        Self { dir, fake, children: Vec::new() }
    }

    fn set_mode(&self, mode: &str) {
        fs::write(self.fake.join("mode"), mode).unwrap();
    }

    fn env(&self) -> Vec<(&'static str, String)> {
        let d = self.dir.path().to_str().unwrap().to_string();
        let bin = self.fake.join("bin").to_str().unwrap().to_string();
        let path = format!("{bin}:{}", std::env::var("PATH").unwrap_or_default());
        vec![
            ("GRITTY_SOCKET_DIR", d.clone()),
            ("XDG_CONFIG_HOME", d.clone()),
            ("HOME", d),
            ("PATH", path),
            ("GRITTY_BIN_DIR", bin),
            ("FAKE_SSH_DIR", self.fake.to_str().unwrap().to_string()),
            ("SHELL", "/bin/sh".to_string()),
            ("NO_COLOR", "1".to_string()),
            ("RUST_LOG", "info".to_string()),
        ]
    }

    fn gritty(&self, args: &[&str]) -> Output {
        Command::new(GRITTY)
            .args(args)
            .envs(self.env())
            .stdin(Stdio::null())
            .output()
            .expect("run gritty")
    }

    /// `tunnel-create NAME` in background mode; asserts it reported success.
    fn create(&self) {
        let out = self.gritty(&["tunnel-create", NAME]);
        let err = String::from_utf8_lossy(&out.stderr);
        assert!(out.status.success(), "tunnel-create failed: {err}");
        assert!(err.contains(&format!("tunnel {NAME} started")), "{err}");
    }

    /// `tunnel-create -f NAME` as a child whose stderr (the supervisor's log
    /// in foreground mode) lands in `supervisor.log`.
    fn create_foreground(&mut self) {
        let log = fs::File::create(self.supervisor_log()).unwrap();
        let child = Command::new(GRITTY)
            .args(["tunnel-create", "-f", NAME])
            .envs(self.env())
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(log)
            .spawn()
            .expect("spawn tunnel-create -f");
        self.children.push(child);
        // "healthy" from outside (lock held + socket connectable) can precede
        // the supervisor's own wait_for_socket poll noticing the bind. The
        // .dest sidecar is written after the post-bind probe, immediately
        // before the monitor loop takes over the child.
        self.wait_for_file(&self.sidecar("dest"));
        self.wait_status("healthy");
    }

    fn wait_for_file(&self, path: &Path) {
        let deadline = Instant::now() + WAIT;
        while !path.exists() {
            if Instant::now() >= deadline {
                panic!("{} never appeared; {}", path.display(), self.diagnostics());
            }
            std::thread::sleep(Duration::from_millis(20));
        }
    }

    fn supervisor_log(&self) -> PathBuf {
        self.dir.path().join("supervisor.log")
    }

    fn sidecar(&self, ext: &str) -> PathBuf {
        self.dir.path().join(format!("connect-{NAME}.{ext}"))
    }

    fn calls(&self) -> String {
        fs::read_to_string(self.fake.join("calls")).unwrap_or_default()
    }

    fn bridge_calls(&self) -> usize {
        self.calls().lines().filter(|l| l.contains(" -L ")).count()
    }

    fn bridge_pid(&self) -> Pid {
        let text = fs::read_to_string(self.fake.join("bridge.pid")).expect("bridge.pid");
        Pid::from_raw(text.trim().parse().unwrap())
    }

    /// Kill the current `-L` bridge -- "ssh died" -- and forget its pid so the
    /// next `bridge_pid` is the respawn's.
    fn kill_bridge(&self) {
        kill(self.bridge_pid(), Signal::SIGKILL).unwrap();
        fs::remove_file(self.fake.join("bridge.pid")).unwrap();
    }

    fn tunnels_json(&self) -> serde_json::Value {
        let out = self.gritty(&["tunnels", "--json"]);
        let text = String::from_utf8_lossy(&out.stdout);
        serde_json::from_str(&text).unwrap_or_else(|e| panic!("tunnels --json ({e}): {text}"))
    }

    fn status(&self) -> Option<String> {
        self.tunnels_json()
            .as_array()?
            .iter()
            .find(|t| t["name"] == NAME)
            .map(|t| t["status"].as_str().unwrap().to_string())
    }

    fn wait_status(&self, want: &str) {
        let deadline = Instant::now() + WAIT;
        loop {
            if self.status().as_deref() == Some(want) {
                return;
            }
            if Instant::now() >= deadline {
                panic!("tunnel never reached {want:?}; {}", self.diagnostics());
            }
            std::thread::sleep(Duration::from_millis(50));
        }
    }

    fn wait_logged(&self, marker: &str) {
        let deadline = Instant::now() + WAIT;
        loop {
            if self.all_logs().contains(marker) {
                return;
            }
            if Instant::now() >= deadline {
                panic!("never logged {marker:?}; {}", self.diagnostics());
            }
            std::thread::sleep(Duration::from_millis(50));
        }
    }

    fn all_logs(&self) -> String {
        [self.supervisor_log(), self.sidecar("log"), self.sidecar("out")]
            .iter()
            .filter_map(|p| fs::read_to_string(p).ok())
            .collect()
    }

    fn diagnostics(&self) -> String {
        let files: Vec<String> = fs::read_dir(self.dir.path())
            .unwrap()
            .filter_map(|e| e.ok())
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .collect();
        format!(
            "status={:?} files={files:?}\n--- ssh calls ---\n{}\n--- logs ---\n{}",
            self.status(),
            self.calls(),
            self.all_logs()
        )
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        for child in &mut self.children {
            let _ = child.kill();
            let _ = child.wait();
        }
        let _ = self.gritty(&["tunnel-destroy", NAME]);
        let _ = self.gritty(&["kill-server", "local"]);
        // A background supervisor (or its bridge) that outlived tunnel-destroy.
        for pid_file in [self.sidecar("pid"), self.fake.join("bridge.pid")] {
            if let Ok(text) = fs::read_to_string(pid_file)
                && let Ok(pid) = text.trim().parse::<i32>()
            {
                let _ = kill(Pid::from_raw(pid), Signal::SIGKILL);
            }
        }
    }
}

fn set_executable(path: &Path) {
    use std::os::unix::fs::PermissionsExt;
    fs::set_permissions(path, fs::Permissions::from_mode(0o755)).unwrap();
}

fn stderr_of(out: &Output) -> String {
    String::from_utf8_lossy(&out.stderr).into_owned()
}

// ---------------------------------------------------------------------------
// Startup and teardown
// ---------------------------------------------------------------------------

#[test]
fn tunnel_create_brings_up_a_healthy_tunnel_and_destroy_tears_it_down() {
    require_socat();
    let fx = Fixture::new();
    fx.create();
    assert_eq!(fx.status().as_deref(), Some("healthy"), "{}", fx.diagnostics());
    for ext in ["lock", "pid", "sock", "dest", "info", "remote-sock"] {
        assert!(fx.sidecar(ext).exists(), "missing connect-{NAME}.{ext}; {}", fx.diagnostics());
    }
    assert_eq!(fs::read_to_string(fx.sidecar("dest")).unwrap().trim(), NAME);
    // preflight (`true`), ensure-remote, bridge: three ssh invocations.
    assert_eq!(fx.calls().lines().count(), 3, "{}", fx.calls());
    assert_eq!(fx.bridge_calls(), 1);

    let out = fx.gritty(&["tunnel-destroy", NAME]);
    assert!(out.status.success(), "{}", stderr_of(&out));
    assert!(stderr_of(&out).contains(&format!("tunnel {NAME} stopped")), "{}", stderr_of(&out));
    for ext in ["lock", "pid", "sock", "info"] {
        assert!(!fx.sidecar(ext).exists(), "connect-{NAME}.{ext} survived destroy");
    }
    // Persistence caches survive so the next connect lands on the same host.
    assert!(fx.sidecar("dest").exists());
    assert!(fx.sidecar("remote-sock").exists());
    assert_eq!(fx.status(), None);
}

#[test]
fn tunnel_create_is_idempotent_while_healthy() {
    require_socat();
    let fx = Fixture::new();
    fx.create();
    let calls_before = fx.calls().lines().count();

    let out = fx.gritty(&["tunnel-create", NAME]);
    assert!(out.status.success(), "{}", stderr_of(&out));
    assert!(stderr_of(&out).contains("already running"), "{}", stderr_of(&out));
    // Found, not re-established: no second supervisor, no ssh at all.
    assert_eq!(fx.calls().lines().count(), calls_before, "{}", fx.calls());
    assert_eq!(fx.bridge_calls(), 1);
}

#[test]
fn sessions_are_reachable_through_the_tunnel() {
    require_socat();
    let fx = Fixture::new();
    fx.create();

    let out = fx.gritty(&["connect", "-d", &format!("{NAME}:job"), "-c", "sleep 30"]);
    assert!(out.status.success(), "{}", stderr_of(&out));
    let out = fx.gritty(&["ls", NAME, "--json"]);
    let text = String::from_utf8_lossy(&out.stdout);
    assert!(
        text.contains("\"display_name\": \"job\"") || text.contains("\"display_name\":\"job\""),
        "{text}"
    );

    let out = fx.gritty(&["kill-session", &format!("{NAME}:job")]);
    assert!(out.status.success(), "{}", stderr_of(&out));
}

#[test]
fn dry_run_prints_the_ssh_commands_without_invoking_ssh() {
    let fx = Fixture::new();
    let out = fx.gritty(&["tunnel-create", "--dry-run", NAME]);
    assert!(out.status.success(), "{}", stderr_of(&out));
    let text = format!("{}{}", String::from_utf8_lossy(&out.stdout), stderr_of(&out));
    assert!(text.contains("ssh ") && text.contains("-L "), "{text}");
    assert!(fx.calls().is_empty(), "dry-run ran ssh: {}", fx.calls());
    assert!(!fx.sidecar("lock").exists());
}

#[test]
fn tunnel_destroy_unknown_name_names_the_known_tunnels() {
    require_socat();
    let fx = Fixture::new();
    let out = fx.gritty(&["tunnel-destroy", "nosuch"]);
    assert!(!out.status.success());
    assert!(stderr_of(&out).contains("no tunnel named nosuch"), "{}", stderr_of(&out));

    fx.create();
    let out = fx.gritty(&["tunnel-destroy", "nosuch"]);
    assert!(!out.status.success());
    assert!(stderr_of(&out).contains(NAME), "known tunnels missing: {}", stderr_of(&out));
}

// ---------------------------------------------------------------------------
// Startup failures: nothing may be left behind
// ---------------------------------------------------------------------------

#[test]
fn preflight_failure_reports_ssh_stderr_and_leaves_no_sidecars() {
    let fx = Fixture::new();
    fx.set_mode("preflight-fail");
    let out = fx.gritty(&["tunnel-create", NAME]);
    assert!(!out.status.success(), "{}", stderr_of(&out));
    assert!(stderr_of(&out).contains("Permission denied"), "{}", stderr_of(&out));
    assert_eq!(
        fx.calls().lines().count(),
        1,
        "preflight must be the only ssh call: {}",
        fx.calls()
    );
    for ext in ["lock", "pid", "sock", "dest", "info"] {
        assert!(
            !fx.sidecar(ext).exists(),
            "connect-{NAME}.{ext} left behind by a failed preflight"
        );
    }
}

#[test]
fn remote_ensure_failure_is_reported_and_leaves_no_ghost_lock() {
    let fx = Fixture::new();
    fx.set_mode("ensure-fail");
    let out = fx.gritty(&["tunnel-create", NAME]);
    assert!(!out.status.success(), "{}", stderr_of(&out));
    // Background mode: the supervisor child reports through .out; the parent
    // relays the failure. Either way the user is pointed at bootstrap.
    let text = format!("{}{}", stderr_of(&out), fx.all_logs());
    assert!(text.contains("bootstrap"), "{text}");
    // The LockFileBailGuard contract: an early bail after acquiring the
    // flock must not strand the .lock file, nor the .pid/.info written with it.
    for ext in ["lock", "pid", "info", "sock"] {
        assert!(!fx.sidecar(ext).exists(), "connect-{NAME}.{ext} stranded; {}", fx.diagnostics());
    }
    assert_eq!(fx.status(), None);
}

// ---------------------------------------------------------------------------
// Supervisor loop: exit classification, respawn, backoff, kick
// ---------------------------------------------------------------------------

#[test]
fn ssh_signal_death_is_transient_and_respawned() {
    require_socat();
    let mut fx = Fixture::new();
    fx.create_foreground();
    let first = fx.bridge_pid();

    fx.kill_bridge();
    fx.wait_logged("ssh tunnel exited");
    fx.wait_logged("ssh tunnel respawned");
    fx.wait_status("healthy");
    assert_ne!(fx.bridge_pid(), first, "respawn must be a new bridge process");
    assert_eq!(fx.bridge_calls(), 2, "{}", fx.calls());
    // Still the same supervisor (same lock holder): no second tunnel-create.
    assert!(
        fx.children[0].try_wait().unwrap().is_none(),
        "supervisor exited; {}",
        fx.diagnostics()
    );
}

#[test]
fn nontransient_ssh_exit_stops_the_supervisor_and_cleans_up() {
    require_socat();
    let mut fx = Fixture::new();
    fx.create_foreground();

    // The respawn will bind, then exit 1 -- an auth/config class error the
    // supervisor must not retry.
    fx.set_mode("spawn-die-nontransient");
    fx.kill_bridge();
    fx.wait_logged("not retrying");

    let deadline = Instant::now() + WAIT;
    let status = loop {
        if let Some(s) = fx.children[0].try_wait().unwrap() {
            break s;
        }
        assert!(Instant::now() < deadline, "supervisor kept running; {}", fx.diagnostics());
        std::thread::sleep(Duration::from_millis(50));
    };
    // The documented contract is "log and return without retry"; the
    // supervisor's own exit code is unspecified (it exits 0 today).
    let _ = status;
    for ext in ["lock", "pid", "sock", "info"] {
        assert!(
            !fx.sidecar(ext).exists(),
            "connect-{NAME}.{ext} left behind; {}",
            fx.diagnostics()
        );
    }
    assert!(fx.sidecar("dest").exists(), ".dest is a persistence cache and must survive");
    assert_eq!(fx.status(), None);
}

#[test]
fn failed_respawns_back_off_and_a_kick_retries_immediately() {
    require_socat();
    let mut fx = Fixture::new();
    fx.create_foreground();

    // Every respawn attempt fails: backoff climbs 1s, 2s, 4s, ...
    fx.set_mode("spawn-fail");
    fx.kill_bridge();
    fx.wait_logged("respawn: sleeping 1s (backoff)");
    fx.wait_logged("respawn: sleeping 2s (backoff)");
    // Lock held, socket gone: externally "reconnecting", not "stale".
    assert_eq!(fx.status().as_deref(), Some("reconnecting"), "{}", fx.diagnostics());
    fx.wait_logged("respawn: sleeping 4s (backoff)");

    // The network is back (ssh works again) and the user is at the keyboard:
    // the client's kick must cut the remaining sleep short instead of
    // waiting out the 4s.
    fx.set_mode("ok");
    fs::write(fx.sidecar("kick"), "").unwrap();
    fx.wait_logged("client kick received during backoff");
    fx.wait_status("healthy");
    assert!(!fx.sidecar("kick").exists(), "kick must be consumed");
}
