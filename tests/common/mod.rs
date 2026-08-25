//! Helpers shared by the integration suites that drive the real binary.
//! Each suite is its own crate, so this file is `mod common;`-included; keep
//! it free of anything suite-specific.

#![allow(dead_code)] // each suite uses a subset

use std::path::Path;
use std::process::Output;
use std::time::{Duration, Instant};

/// The `gritty` binary under test (built by cargo for this test run).
pub const GRITTY: &str = env!("CARGO_BIN_EXE_gritty");

/// Block until a Unix socket at `path` accepts connections. Polls every 20ms;
/// panics (with the path) at `timeout`, so a daemon that never comes up fails
/// the test instead of hanging it.
pub fn wait_for_socket(path: &Path, timeout: Duration) {
    let deadline = Instant::now() + timeout;
    while Instant::now() < deadline {
        if std::os::unix::net::UnixStream::connect(path).is_ok() {
            return;
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    panic!("socket {} never became connectable within {timeout:?}", path.display());
}

pub fn stdout(out: &Output) -> String {
    String::from_utf8_lossy(&out.stdout).into_owned()
}

pub fn stderr(out: &Output) -> String {
    String::from_utf8_lossy(&out.stderr).into_owned()
}

/// socat is a hard requirement of the tunnel/bridge suites: a missing tool
/// must fail the run, never silently pass it. `GRITTY_SOCAT_TEST=0` is the
/// only, explicit, opt-out (for a developer box without socat; CI never sets it).
pub fn require_socat() {
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
