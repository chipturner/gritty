//! Whole-command tests for the one-shot CLI: the real binary, run the way a
//! user runs it, against a `gritty server -f` child in a private socket dir.
//!
//! `commands/*` is binary-private, so its functions can only be reached this
//! way; the in-file unit tests cover the pure helpers, this file covers the
//! commands end to end -- including `restart`, which mutates a live daemon
//! and had no automated invocation anywhere.

use std::path::PathBuf;
use std::process::{Child, Command, Output, Stdio};
use std::time::Duration;

mod common;
use common::{GRITTY, stderr, stdout, wait_for_socket};

const WAIT: Duration = Duration::from_secs(10);

/// A socket dir and a home dir, kept apart: `doctor` audits every entry of
/// the socket dir, so the device-id state that lands under `$HOME` must not
/// be inside it.
struct Sandbox {
    dir: tempfile::TempDir,
    daemon: Option<Child>,
}

impl Sandbox {
    fn new() -> Self {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir(dir.path().join("sock")).unwrap();
        std::fs::set_permissions(dir.path().join("sock"), std::fs::Permissions::from_mode(0o700))
            .unwrap();
        std::fs::create_dir(dir.path().join("home")).unwrap();
        Self { dir, daemon: None }
    }

    fn with_daemon() -> Self {
        let mut sb = Self::new();
        let child = Command::new(GRITTY)
            .args(["server", "-f"])
            .envs(sb.env())
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .expect("spawn gritty server -f");
        sb.daemon = Some(child);
        wait_for_socket(&sb.ctl_path(), WAIT);
        sb
    }

    /// The socket dir.
    fn path(&self) -> PathBuf {
        self.dir.path().join("sock")
    }

    fn home(&self) -> PathBuf {
        self.dir.path().join("home")
    }

    fn ctl_path(&self) -> PathBuf {
        self.path().join("ctl.sock")
    }

    fn env(&self) -> Vec<(&'static str, String)> {
        let home = self.home().to_str().unwrap().to_string();
        vec![
            ("GRITTY_SOCKET_DIR", self.path().to_str().unwrap().to_string()),
            ("XDG_CONFIG_HOME", home.clone()),
            ("HOME", home),
            ("SHELL", "/bin/sh".to_string()),
            ("NO_COLOR", "1".to_string()),
        ]
    }

    fn gritty(&self, args: &[&str]) -> Output {
        self.gritty_env(args, &[])
    }

    fn gritty_env(&self, args: &[&str], extra: &[(&str, &str)]) -> Output {
        Command::new(GRITTY)
            .args(args)
            .envs(self.env())
            .envs(extra.iter().copied())
            .stdin(Stdio::null())
            .output()
            .expect("run gritty")
    }

    fn ok(&self, args: &[&str]) -> Output {
        let out = self.gritty(args);
        assert!(out.status.success(), "gritty {args:?} failed: {}", stderr(&out));
        out
    }

    fn daemon_pid(&self) -> u32 {
        let text = std::fs::read_to_string(self.path().join("daemon.pid")).expect("daemon.pid");
        text.trim().parse().expect("pid")
    }
}

impl Drop for Sandbox {
    fn drop(&mut self) {
        // Whatever daemon is current -- the original child or a `restart`ed
        // background one -- goes through kill-server; the child is reaped too.
        let _ = self.gritty(&["kill-server", "local"]);
        if let Some(mut child) = self.daemon.take() {
            let _ = child.kill();
            let _ = child.wait();
        }
    }
}

fn json(out: &Output) -> serde_json::Value {
    serde_json::from_str(&stdout(out)).unwrap_or_else(|e| panic!("not JSON ({e}): {}", stdout(out)))
}

// ---------------------------------------------------------------------------
// One-shot informational commands
// ---------------------------------------------------------------------------

#[test]
fn protocol_version_prints_the_constant() {
    let sb = Sandbox::new();
    let out = sb.ok(&["protocol-version"]);
    assert_eq!(stdout(&out).trim(), gritty::protocol::PROTOCOL_VERSION.to_string());
}

#[test]
fn socket_path_honors_the_socket_dir_and_the_ctl_socket_override() {
    let sb = Sandbox::new();
    let out = sb.ok(&["socket-path"]);
    assert_eq!(stdout(&out).trim(), sb.ctl_path().to_str().unwrap());

    let out = sb.ok(&["--ctl-socket", "/tmp/elsewhere.sock", "socket-path"]);
    assert_eq!(stdout(&out).trim(), "/tmp/elsewhere.sock");
}

#[test]
fn info_json_reports_version_and_paths() {
    let sb = Sandbox::with_daemon();
    let out = sb.ok(&["info", "--json"]);
    let v = json(&out);
    assert_eq!(v["version"], env!("CARGO_PKG_VERSION"), "{v}");
    assert!(v["config_path"].as_str().unwrap().ends_with("config.toml"), "{v}");
}

#[test]
fn mangen_writes_one_page_per_subcommand() {
    let sb = Sandbox::new();
    let dir = sb.home().join("man");
    let out = sb.ok(&["mangen", dir.to_str().unwrap()]);
    assert!(stdout(&out).contains("man pages written to"), "{}", stdout(&out));
    let pages: Vec<String> = std::fs::read_dir(&dir)
        .unwrap()
        .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
        .collect();
    for page in ["gritty.1", "gritty-connect.1", "gritty-tunnel-create.1", "gritty-doctor.1"] {
        assert!(pages.contains(&page.to_string()), "missing {page} in {pages:?}");
    }
    // Every subcommand in `--help` has a page (plus the top-level one).
    let help = stdout(&sb.ok(&["--help"]));
    let subcommands = help
        .lines()
        .filter(|l| l.starts_with("  gritty-") || l.starts_with("  "))
        .filter_map(|l| l.split_whitespace().next())
        .filter(|w| w.chars().all(|c| c.is_ascii_lowercase() || c == '-') && w.len() > 1)
        .filter(|w| pages.contains(&format!("gritty-{w}.1")))
        .count();
    assert!(subcommands >= 20, "only {subcommands} subcommands matched a man page: {pages:?}");
}

// ---------------------------------------------------------------------------
// config
// ---------------------------------------------------------------------------

#[test]
fn config_creates_the_file_from_the_template_then_runs_the_editor() {
    let sb = Sandbox::new();
    let record = sb.home().join("editor-args");
    // An "editor" that records what it was asked to open.
    let editor = format!("sh -c 'echo \"$@\" > {}' editor", record.display());
    let out = sb.gritty_env(&["config"], &[("VISUAL", &editor), ("EDITOR", "false")]);
    assert!(out.status.success(), "{}", stderr(&out));
    let created = stderr(&out);
    assert!(created.contains("created "), "{created}");

    let path = std::fs::read_to_string(&record).unwrap().trim().to_string();
    assert!(path.ends_with("gritty/config.toml"), "editor opened {path:?}");
    assert_eq!(std::fs::read_to_string(&path).unwrap(), gritty::config::DEFAULT_CONFIG);

    // Second run: file exists, nothing is recreated, editor still runs.
    let out = sb.gritty_env(&["config"], &[("VISUAL", &editor)]);
    assert!(out.status.success(), "{}", stderr(&out));
    assert!(!stderr(&out).contains("created "), "{}", stderr(&out));
}

#[test]
fn config_reports_an_editor_that_fails() {
    let sb = Sandbox::new();
    let out = sb.gritty_env(&["config"], &[("VISUAL", "false"), ("EDITOR", "false")]);
    assert!(!out.status.success());
    assert!(stderr(&out).contains("false exited with"), "{}", stderr(&out));
}

// ---------------------------------------------------------------------------
// restart
// ---------------------------------------------------------------------------

#[test]
fn restart_replaces_the_local_daemon() {
    let sb = Sandbox::with_daemon();
    sb.ok(&["connect", "-d", "local:job", "-c", "sleep 30"]);
    let old_pid = sb.daemon_pid();

    let out = sb.ok(&["restart"]);
    let err = stderr(&out);
    assert!(err.contains("server killed"), "{err}");
    assert!(err.contains("server restarted"), "{err}");

    wait_for_socket(&sb.ctl_path(), WAIT);
    assert_ne!(sb.daemon_pid(), old_pid, "restart must start a new daemon process");
    // Sessions do not survive a restart (unlike refresh, which refuses).
    let out = sb.ok(&["ls", "local", "--json"]);
    assert!(!stdout(&out).contains("\"job\""), "{}", stdout(&out));
}

#[test]
fn restart_with_no_daemon_starts_one() {
    let sb = Sandbox::new();
    let out = sb.ok(&["restart"]);
    let err = stderr(&out);
    assert!(err.contains("no server running"), "{err}");
    assert!(err.contains("server restarted"), "{err}");
    wait_for_socket(&sb.ctl_path(), WAIT);
}

// ---------------------------------------------------------------------------
// doctor
// ---------------------------------------------------------------------------

#[test]
fn doctor_is_clean_against_a_healthy_daemon() {
    let sb = Sandbox::with_daemon();
    let out = sb.ok(&["doctor", "--json"]);
    let v = json(&out);
    assert_eq!(v["failures"], 0, "{v}");
    assert_eq!(v["warnings"], 0, "{v}");
    let labels: Vec<&str> =
        v["paths"].as_array().unwrap().iter().map(|p| p["label"].as_str().unwrap()).collect();
    for label in ["config file", "socket dir", "server socket", "server log"] {
        assert!(labels.contains(&label), "{labels:?}");
    }
    assert_eq!(
        v["paths"].as_array().unwrap().iter().find(|p| p["label"] == "server socket").unwrap()["exists"],
        true
    );
    // Plain mode exits 0 too and names the server check group.
    let out = sb.ok(&["doctor"]);
    assert!(stdout(&out).to_lowercase().contains("server"), "{}", stdout(&out));
}

#[test]
fn doctor_flags_unknown_files_and_clean_removes_only_those() {
    let sb = Sandbox::with_daemon();
    let stray = sb.path().join("mystery.txt");
    std::fs::write(&stray, "?").unwrap();

    let out = sb.ok(&["doctor", "--json"]);
    let v = json(&out);
    assert!(v["warnings"].as_u64().unwrap() >= 1, "{v}");
    let text = v.to_string();
    assert!(text.contains("unknown file: mystery.txt"), "{text}");

    sb.ok(&["doctor", "--clean"]);
    assert!(!stray.exists(), "--clean must remove the unknown file");
    for keep in ["ctl.sock", "daemon.pid", "daemon.info"] {
        assert!(sb.path().join(keep).exists(), "--clean removed {keep}");
    }
    let v = json(&sb.ok(&["doctor", "--json"]));
    assert_eq!(v["warnings"], 0, "{v}");
}

#[test]
fn doctor_reports_a_stale_pid_file_without_a_daemon() {
    let sb = Sandbox::new();
    // A pid file pointing at a process that cannot exist.
    std::fs::write(sb.path().join("daemon.pid"), "4000000").unwrap();
    let out = sb.gritty(&["doctor", "--json"]);
    let v = json(&out);
    let text = v.to_string().to_lowercase();
    assert!(text.contains("stale") || text.contains("not running"), "{text}");
}
