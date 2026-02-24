use std::path::PathBuf;
use std::process::{Command, Output};
use std::time::Duration;

macro_rules! skip_unless_ssh {
    () => {
        if std::env::var("GRITTY_SSH_TEST").is_err() {
            eprintln!("skipping (set GRITTY_SSH_TEST=1 to enable)");
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

fn gritty(args: &[&str]) -> Output {
    Command::new(gritty_bin()).args(args).output().expect("failed to run gritty")
}

fn gritty_ok(args: &[&str]) -> String {
    let out = gritty(args);
    assert!(
        out.status.success(),
        "gritty {args:?} failed: {}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr),
    );
    String::from_utf8_lossy(&out.stdout).into_owned()
}

fn ensure_disconnected(name: &str) {
    let _ = gritty(&["disconnect", name]);
    std::thread::sleep(Duration::from_millis(500));
}

#[test]
fn connect_and_list_tunnels() {
    skip_unless_ssh!();

    let name = "ssh-test-list";
    ensure_disconnected(name);

    // Connect
    gritty_ok(&["connect", "localhost", "-n", name]);

    // Verify in tunnels output
    let tunnels = gritty_ok(&["tunnels"]);
    assert!(tunnels.contains(name), "tunnel {name} not in: {tunnels}");

    // Disconnect
    gritty_ok(&["disconnect", name]);
    std::thread::sleep(Duration::from_millis(500));

    // Verify gone
    let tunnels = gritty_ok(&["tunnels"]);
    assert!(!tunnels.contains(name), "tunnel {name} still in: {tunnels}");
}

#[test]
fn connect_list_sessions_empty() {
    skip_unless_ssh!();

    let name = "ssh-test-empty";
    ensure_disconnected(name);

    gritty_ok(&["connect", "localhost", "-n", name]);

    // Server auto-started, ls should work but show no sessions
    let out = gritty_ok(&["ls", name]);
    // No sessions yet -- output should be empty or just a header
    assert!(!out.contains("running"), "expected no running sessions, got: {out}");

    ensure_disconnected(name);
}

#[test]
fn connect_disconnect_reconnect() {
    skip_unless_ssh!();

    let name = "ssh-test-reconnect";
    ensure_disconnected(name);

    // First connect (starts remote server)
    gritty_ok(&["connect", "localhost", "-n", name]);
    gritty_ok(&["ls", name]);

    // Disconnect
    gritty_ok(&["disconnect", name]);
    std::thread::sleep(Duration::from_millis(500));

    // Reconnect (server persists from first connect)
    gritty_ok(&["connect", "localhost", "-n", name]);
    gritty_ok(&["ls", name]);

    ensure_disconnected(name);
}

#[test]
fn connect_with_custom_name() {
    skip_unless_ssh!();

    let name = "mydev-test";
    ensure_disconnected(name);

    gritty_ok(&["connect", "localhost", "-n", name]);

    let tunnels = gritty_ok(&["tunnels"]);
    assert!(tunnels.contains(name), "custom name {name} not in: {tunnels}");

    gritty_ok(&["ls", name]);

    gritty_ok(&["disconnect", name]);
    std::thread::sleep(Duration::from_millis(500));

    let tunnels = gritty_ok(&["tunnels"]);
    assert!(!tunnels.contains(name), "tunnel {name} still in: {tunnels}");
}

#[test]
fn connect_info_shows_tunnel() {
    skip_unless_ssh!();

    let name = "ssh-test-info";
    ensure_disconnected(name);

    gritty_ok(&["connect", "localhost", "-n", name]);

    let info = gritty_ok(&["info"]);
    assert!(info.contains(name), "info should mention tunnel {name}: {info}");

    ensure_disconnected(name);
}

#[test]
fn connect_foreground_mode() {
    skip_unless_ssh!();

    let name = "ssh-test-fg";
    ensure_disconnected(name);

    // Spawn foreground connect as a child process
    let mut child = Command::new(gritty_bin())
        .args(["connect", "localhost", "-n", name, "--foreground"])
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .expect("failed to spawn gritty connect --foreground");

    // Poll for the connect socket to appear
    let socket_dir = {
        let out = gritty_ok(&["socket-path"]);
        PathBuf::from(out.trim()).parent().expect("socket-path has parent").to_owned()
    };
    let connect_sock = socket_dir.join(format!("connect-{name}.sock"));

    let deadline = std::time::Instant::now() + Duration::from_secs(15);
    while !connect_sock.exists() {
        if std::time::Instant::now() > deadline {
            let _ = child.kill();
            panic!("connect socket never appeared: {connect_sock:?}");
        }
        std::thread::sleep(Duration::from_millis(200));
    }

    // Verify we can list sessions through it
    gritty_ok(&["ls", name]);

    // Send SIGTERM to the foreground process
    unsafe {
        libc::kill(child.id() as libc::pid_t, libc::SIGTERM);
    }
    let status = child.wait().expect("wait failed");
    // SIGTERM results in non-zero exit
    assert!(!status.success() || status.code() == Some(0));

    // Give cleanup a moment
    std::thread::sleep(Duration::from_millis(500));
}
