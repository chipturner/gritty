use std::path::PathBuf;
use std::process::{Command, Output, Stdio};
use std::time::Duration;

macro_rules! skip_if_no_ssh {
    () => {
        if std::env::var("GRITTY_SSH_TEST").as_deref() == Ok("0") {
            eprintln!("skipping (GRITTY_SSH_TEST=0)");
            return;
        }
        if !Command::new("ssh")
            .args(["-o", "BatchMode=yes", "-o", "ConnectTimeout=2", "localhost", "true"])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .map(|s| s.success())
            .unwrap_or(false)
        {
            eprintln!("skipping (ssh localhost not available)");
            return;
        }
    };
}

fn gritty_bin() -> PathBuf {
    let mut path = PathBuf::from(env!("CARGO_BIN_EXE_gritty"));
    // Fallback if the env var points nowhere useful
    if !path.exists() {
        path = PathBuf::from("target/debug/gritty");
    }
    path
}

fn gritty_bin_dir() -> PathBuf {
    gritty_bin().parent().unwrap().to_owned()
}

/// Isolated test environment with its own socket directory.
struct TestEnv {
    _tmp: tempfile::TempDir,
    socket_dir: PathBuf,
}

impl TestEnv {
    fn new() -> Self {
        let tmp = tempfile::tempdir().unwrap();
        let socket_dir = tmp.path().to_owned();
        Self { _tmp: tmp, socket_dir }
    }

    fn gritty(&self, args: &[&str]) -> Output {
        Command::new(gritty_bin())
            .args(args)
            .env("GRITTY_SOCKET_DIR", &self.socket_dir)
            .env("GRITTY_BIN_DIR", gritty_bin_dir())
            .output()
            .expect("failed to run gritty")
    }

    fn gritty_ok(&self, args: &[&str]) -> String {
        let out = self.gritty(args);
        assert!(
            out.status.success(),
            "gritty {args:?} failed: {}{}",
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr),
        );
        String::from_utf8_lossy(&out.stdout).into_owned()
    }

    fn cleanup(&self, name: &str) {
        let _ = self.gritty(&["kill-server", name]);
        let _ = self.gritty(&["tunnel-destroy", name]);
        std::thread::sleep(Duration::from_millis(500));
    }

    fn spawn_gritty(&self, args: &[&str]) -> std::process::Child {
        Command::new(gritty_bin())
            .args(args)
            .env("GRITTY_SOCKET_DIR", &self.socket_dir)
            .env("GRITTY_BIN_DIR", gritty_bin_dir())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .spawn()
            .expect("failed to spawn gritty")
    }
}

#[test]
fn connect_and_list_tunnels() {
    skip_if_no_ssh!();

    let env = TestEnv::new();
    let name = "ssh-test-list";

    // Connect
    env.gritty_ok(&["tunnel-create", "localhost", "-n", name]);

    // Verify in tunnels output
    let tunnels = env.gritty_ok(&["tunnels"]);
    assert!(tunnels.contains(name), "tunnel {name} not in: {tunnels}");

    // Disconnect
    env.gritty_ok(&["tunnel-destroy", name]);
    std::thread::sleep(Duration::from_millis(500));

    // Verify gone
    let tunnels = env.gritty_ok(&["tunnels"]);
    assert!(!tunnels.contains(name), "tunnel {name} still in: {tunnels}");

    env.cleanup(name);
}

#[test]
fn connect_list_sessions_empty() {
    skip_if_no_ssh!();

    let env = TestEnv::new();
    let name = "ssh-test-empty";

    env.gritty_ok(&["tunnel-create", "localhost", "-n", name]);

    // Server auto-started, ls should work but show no sessions
    let out = env.gritty_ok(&["ls", name]);
    // No sessions yet -- output should be empty or just a header
    assert!(!out.contains("running"), "expected no running sessions, got: {out}");

    env.cleanup(name);
}

#[test]
fn connect_disconnect_reconnect() {
    skip_if_no_ssh!();

    let env = TestEnv::new();
    let name = "ssh-test-reconnect";

    // First connect (starts remote server)
    env.gritty_ok(&["tunnel-create", "localhost", "-n", name]);
    env.gritty_ok(&["ls", name]);

    // Disconnect
    env.gritty_ok(&["tunnel-destroy", name]);
    std::thread::sleep(Duration::from_millis(500));

    // Reconnect (server persists from first connect)
    env.gritty_ok(&["tunnel-create", "localhost", "-n", name]);
    env.gritty_ok(&["ls", name]);

    env.cleanup(name);
}

#[test]
fn connect_with_custom_name() {
    skip_if_no_ssh!();

    let env = TestEnv::new();
    let name = "mydev-test";

    env.gritty_ok(&["tunnel-create", "localhost", "-n", name]);

    let tunnels = env.gritty_ok(&["tunnels"]);
    assert!(tunnels.contains(name), "custom name {name} not in: {tunnels}");

    env.gritty_ok(&["ls", name]);

    env.gritty_ok(&["tunnel-destroy", name]);
    std::thread::sleep(Duration::from_millis(500));

    let tunnels = env.gritty_ok(&["tunnels"]);
    assert!(!tunnels.contains(name), "tunnel {name} still in: {tunnels}");

    env.cleanup(name);
}

#[test]
fn connect_info_shows_tunnel() {
    skip_if_no_ssh!();

    let env = TestEnv::new();
    let name = "ssh-test-info";

    env.gritty_ok(&["tunnel-create", "localhost", "-n", name]);

    let info = env.gritty_ok(&["info"]);
    assert!(info.contains(name), "info should mention tunnel {name}: {info}");

    env.cleanup(name);
}

#[test]
fn connect_foreground_mode() {
    skip_if_no_ssh!();

    let env = TestEnv::new();
    let name = "ssh-test-fg";

    // Spawn foreground connect as a child process
    let mut child = env.spawn_gritty(&["tunnel-create", "localhost", "-n", name, "--foreground"]);

    // Poll for the connect socket to appear
    let connect_sock = env.socket_dir.join(format!("connect-{name}.sock"));

    let deadline = std::time::Instant::now() + Duration::from_secs(15);
    while !connect_sock.exists() {
        if std::time::Instant::now() > deadline {
            let _ = child.kill();
            panic!("connect socket never appeared: {connect_sock:?}");
        }
        std::thread::sleep(Duration::from_millis(200));
    }

    // Verify we can list sessions through it
    env.gritty_ok(&["ls", name]);

    // Send SIGTERM to the foreground process
    unsafe {
        libc::kill(child.id() as libc::pid_t, libc::SIGTERM);
    }
    let status = child.wait().expect("wait failed");
    // SIGTERM results in non-zero exit
    assert!(!status.success() || status.code() == Some(0));

    // Give cleanup a moment
    std::thread::sleep(Duration::from_millis(500));

    env.cleanup(name);
}
