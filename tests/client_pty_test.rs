//! The interactive client, end to end: the real `gritty connect` binary on a
//! real pseudo-terminal talking to a real daemon.
//!
//! Everything else in the suite stops at the wire -- `server::run()` over a
//! socketpair, or the daemon driven by hand-built frames -- so the relay loop
//! in `client.rs` (raw mode, the `~` escape processor, heartbeat/liveness,
//! auto-reconnect, exit-code passthrough) only ever ran under a human or the
//! container suite's tmux. This file gives it a controlling tty of its own.
//!
//! The harness: `openpty()`, spawn the binary with the slave as stdin/stdout/
//! stderr and `TIOCSCTTY` so it is a controlling terminal, read the master
//! from a thread into a transcript. Tests type into the master and wait for
//! markers in the transcript -- event-driven, never a fixed settle.

use std::fs::File;
use std::io::{Read, Write};
use std::os::fd::OwnedFd;
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, ExitStatus, Stdio};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use nix::sys::signal::{Signal, kill};
use nix::unistd::Pid;

const GRITTY: &str = env!("CARGO_BIN_EXE_gritty");
const WAIT: Duration = Duration::from_secs(10);

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

// ---------------------------------------------------------------------------
// Daemon: a foreground `gritty server -f` child in its own socket dir.
// ---------------------------------------------------------------------------

struct Daemon {
    child: Child,
    dir: tempfile::TempDir,
}

impl Daemon {
    fn start() -> Self {
        let dir = tempfile::tempdir().unwrap();
        let child = Command::new(GRITTY)
            .args(["server", "-f"])
            .envs(base_env(dir.path()))
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .expect("spawn gritty server -f");
        let me = Self { child, dir };
        wait_for_socket(&me.ctl_path());
        me
    }

    fn ctl_path(&self) -> PathBuf {
        self.dir.path().join("ctl.sock")
    }

    fn pid(&self) -> Pid {
        Pid::from_raw(self.child.id() as i32)
    }

    /// Write a user config where the client will look for it. `base_env`
    /// points both `XDG_CONFIG_HOME` (Linux) and `HOME` (macOS, via
    /// `~/Library/Application Support`) at the temp dir, so write both.
    fn write_config(&self, body: &str) {
        for rel in ["gritty", "Library/Application Support/gritty"] {
            let dir = self.dir.path().join(rel);
            std::fs::create_dir_all(&dir).unwrap();
            std::fs::write(dir.join("config.toml"), body).unwrap();
        }
    }

    /// Run a non-interactive gritty command against this daemon.
    fn gritty(&self, args: &[&str]) -> std::process::Output {
        Command::new(GRITTY)
            .args(args)
            .envs(base_env(self.dir.path()))
            .stdin(Stdio::null())
            .output()
            .expect("run gritty")
    }

    fn session_attached(&self, name: &str) -> bool {
        let out = self.gritty(&["ls", "local", "--json"]);
        let text = String::from_utf8_lossy(&out.stdout);
        let json: serde_json::Value = serde_json::from_str(&text)
            .unwrap_or_else(|e| panic!("ls --json not JSON ({e}): {text}"));
        fn find<'a>(v: &'a serde_json::Value, name: &str) -> Option<&'a serde_json::Value> {
            match v {
                serde_json::Value::Array(items) => items.iter().find_map(|i| find(i, name)),
                serde_json::Value::Object(map) => {
                    if map.get("display_name").and_then(|n| n.as_str()) == Some(name) {
                        return Some(v);
                    }
                    map.values().find_map(|i| find(i, name))
                }
                _ => None,
            }
        }
        find(&json, name)
            .unwrap_or_else(|| panic!("session {name} not in ls output: {text}"))
            .get("attached")
            .and_then(|a| a.as_bool())
            .expect("attached field")
    }

    /// Start an interactive client on a fresh pty. `args` follow `connect`.
    fn connect(&self, args: &[&str]) -> Pty {
        let ctl = self.ctl_path();
        let mut full = vec!["--ctl-socket", ctl.to_str().unwrap(), "connect"];
        full.extend_from_slice(args);
        Pty::spawn(&full, self.dir.path())
    }
}

impl Drop for Daemon {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

fn base_env(dir: &Path) -> Vec<(&'static str, String)> {
    let d = dir.to_str().unwrap().to_string();
    vec![
        ("GRITTY_SOCKET_DIR", d.clone()),
        ("XDG_CONFIG_HOME", d.clone()),
        ("HOME", d),
        // A herd of login shells sourcing real dotfiles is what made the
        // PTY suites flaky; sessions get a bare sh.
        ("SHELL", "/bin/sh".to_string()),
        ("TERM", "xterm-256color".to_string()),
        // Plain text in the transcript so assertions need no SGR stripping.
        ("NO_COLOR", "1".to_string()),
    ]
}

fn wait_for_socket(path: &Path) {
    let deadline = Instant::now() + WAIT;
    while Instant::now() < deadline {
        if std::os::unix::net::UnixStream::connect(path).is_ok() {
            return;
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    panic!("socket {} never became connectable", path.display());
}

// ---------------------------------------------------------------------------
// Pty: the client binary with a controlling terminal, plus its transcript.
// ---------------------------------------------------------------------------

struct Pty {
    master: File,
    child: Child,
    transcript: Arc<Mutex<Vec<u8>>>,
    log_path: PathBuf,
}

impl Pty {
    fn spawn(args: &[&str], dir: &Path) -> Self {
        let winsize = nix::pty::Winsize { ws_row: 24, ws_col: 80, ws_xpixel: 0, ws_ypixel: 0 };
        let pty = nix::pty::openpty(Some(&winsize), None).expect("openpty");
        let slave: OwnedFd = pty.slave;
        let child = {
            let mut cmd = Command::new(GRITTY);
            cmd.args(args)
                .envs(base_env(dir))
                .stdin(Stdio::from(slave.try_clone().unwrap()))
                .stdout(Stdio::from(slave.try_clone().unwrap()))
                .stderr(Stdio::from(slave));
            // Safety: setsid/ioctl are async-signal-safe and touch only the
            // child's own process state.
            unsafe {
                cmd.pre_exec(|| {
                    if libc::setsid() < 0 {
                        return Err(std::io::Error::last_os_error());
                    }
                    if libc::ioctl(0, libc::TIOCSCTTY as _, 0) < 0 {
                        return Err(std::io::Error::last_os_error());
                    }
                    Ok(())
                });
            }
            cmd.spawn().expect("spawn gritty connect")
            // `cmd` drops here, closing the parent's slave fds so the master
            // reads EOF once the child exits.
        };
        let master = File::from(pty.master);
        let transcript = Arc::new(Mutex::new(Vec::new()));
        let mut reader = master.try_clone().unwrap();
        let sink = Arc::clone(&transcript);
        std::thread::spawn(move || {
            let mut buf = [0u8; 4096];
            loop {
                match reader.read(&mut buf) {
                    Ok(0) | Err(_) => break,
                    Ok(n) => sink.lock().unwrap().extend_from_slice(&buf[..n]),
                }
            }
        });
        Self { master, child, transcript, log_path: dir.join("client.log") }
    }

    /// The transcript plus the client's own log -- what a timeout panic shows.
    fn diagnostics(&self) -> String {
        let log = std::fs::read_to_string(&self.log_path).unwrap_or_default();
        format!("transcript:\n{}\n--- client.log ---\n{log}", self.transcript())
    }

    fn send(&mut self, text: &str) {
        self.master.write_all(text.as_bytes()).expect("write to pty master");
        self.master.flush().unwrap();
    }

    fn transcript(&self) -> String {
        String::from_utf8_lossy(&self.transcript.lock().unwrap()).into_owned()
    }

    /// Block until `marker` appears in the transcript (searching only past
    /// `from`, so a test can wait for the *next* occurrence of a repeated
    /// marker). Returns the transcript length to use as the next `from`.
    fn expect_from(&self, from: usize, marker: &str) -> usize {
        let deadline = Instant::now() + WAIT;
        loop {
            let t = self.transcript();
            if let Some(i) = t.get(from..).and_then(|s| s.find(marker)) {
                return from + i + marker.len();
            }
            if Instant::now() >= deadline {
                panic!("timed out waiting for {marker:?}; {}", self.diagnostics());
            }
            std::thread::sleep(Duration::from_millis(10));
        }
    }

    fn expect(&self, marker: &str) -> usize {
        self.expect_from(0, marker)
    }

    /// Block until the client's own log contains `marker` -- for state the
    /// client deliberately keeps off the terminal.
    fn expect_logged(&self, marker: &str) {
        let deadline = Instant::now() + WAIT;
        loop {
            if std::fs::read_to_string(&self.log_path).unwrap_or_default().contains(marker) {
                return;
            }
            if Instant::now() >= deadline {
                panic!("timed out waiting for log line {marker:?}; {}", self.diagnostics());
            }
            std::thread::sleep(Duration::from_millis(10));
        }
    }

    fn wait_exit(&mut self) -> ExitStatus {
        let deadline = Instant::now() + WAIT;
        loop {
            if let Some(status) = self.child.try_wait().expect("try_wait") {
                return status;
            }
            if Instant::now() >= deadline {
                panic!("client did not exit; {}", self.diagnostics());
            }
            std::thread::sleep(Duration::from_millis(10));
        }
    }

    fn is_running(&mut self) -> bool {
        self.child.try_wait().expect("try_wait").is_none()
    }
}

impl Drop for Pty {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// Attach and prove the shell is executing input, not merely echoing it: the
/// line discipline echoes typed bytes before the shell has read them, and a
/// shell's init may flush pending input (tcsetattr TCSAFLUSH), so keep
/// re-sending the probe until its *result* appears.
fn attach_ready(daemon: &Daemon, name: &str) -> Pty {
    let target = format!("local:{name}");
    let mut pty = daemon.connect(&[&target]);
    probe_ready(&mut pty);
    pty
}

fn probe_ready(pty: &mut Pty) {
    let deadline = Instant::now() + WAIT;
    loop {
        pty.send("echo READY:$((41+1))\n");
        let until = Instant::now() + Duration::from_millis(250);
        while Instant::now() < until {
            if pty.transcript().contains("READY:42") {
                return;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        if Instant::now() >= deadline {
            panic!("shell never executed the probe; {}", pty.diagnostics());
        }
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[test]
fn attach_relays_shell_io_and_tilde_dot_detaches() {
    let daemon = Daemon::start();
    let mut pty = attach_ready(&daemon, "io");
    assert!(daemon.session_attached("io"));

    // Escapes are recognized only immediately after a newline.
    pty.send("\n~.");
    pty.expect("detached");
    let status = pty.wait_exit();
    assert!(status.success(), "detach must exit 0: {status:?}");
    assert!(!daemon.session_attached("io"), "session must read detached after ~.");
}

#[test]
fn detached_session_keeps_running_and_reattach_replays_it() {
    let daemon = Daemon::start();
    let mut first = attach_ready(&daemon, "persist");
    first.send("MARK=kept-$((100+23)); echo $MARK\n");
    first.expect("kept-123");
    first.send("\n~.");
    first.wait_exit();

    // Same shell, same variables, and the scrollback replay shows history.
    let mut second = daemon.connect(&["local:persist"]);
    second.expect("kept-123");
    second.send("echo again:$MARK\n");
    second.expect("again:kept-123");
}

#[test]
fn session_exit_code_passes_through_to_the_client() {
    let daemon = Daemon::start();
    let mut pty = daemon.connect(&["local:code", "-c", "sleep 0.2; exit 7"]);
    let status = pty.wait_exit();
    assert_eq!(status.code(), Some(7), "transcript:\n{}", pty.transcript());
}

#[test]
fn shell_exit_ends_the_client_cleanly() {
    let daemon = Daemon::start();
    let mut pty = attach_ready(&daemon, "bye");
    pty.send("exit 0\n");
    let status = pty.wait_exit();
    assert!(status.success(), "{status:?}; transcript:\n{}", pty.transcript());
    // No raw-mode residue: the exit path resets SGR and re-shows the cursor.
    assert!(pty.transcript().contains("\x1b[?25h"), "cursor-show reset missing");
}

#[test]
fn help_and_status_escapes_print_client_chrome() {
    let daemon = Daemon::start();
    let mut pty = attach_ready(&daemon, "chrome");
    pty.send("\n~?");
    let after_help = pty.expect("~.  - detach from session");
    pty.send("\n~#");
    pty.expect_from(after_help, "[gritty status]");
    // The shell never saw any of it.
    pty.send("echo still:$((1+1))\n");
    pty.expect("still:2");
    assert!(pty.is_running());
}

#[test]
fn doubled_tilde_sends_a_literal_tilde() {
    let daemon = Daemon::start();
    let mut pty = attach_ready(&daemon, "literal");
    // `~~` collapses to one `~` for the shell, so this is the (nonexistent)
    // command `~.` followed by an echo -- not a detach.
    pty.send("\n~~. 2>/dev/null; echo lit:$((5+5))\n");
    pty.expect("lit:10");
    assert!(pty.is_running(), "~~. must not detach");
}

#[test]
fn no_escape_passes_tilde_sequences_to_the_shell() {
    let daemon = Daemon::start();
    let mut pty = daemon.connect(&["--no-escape", "local:raw"]);
    probe_ready(&mut pty);
    pty.send("\n~. 2>/dev/null; echo raw:$((6+6))\n");
    pty.expect("raw:12");
    assert!(pty.is_running(), "~. must not detach under --no-escape");
    pty.send("exit\n");
    assert!(pty.wait_exit().success());
}

#[test]
fn force_takeover_detaches_the_first_client() {
    let daemon = Daemon::start();
    let mut first = attach_ready(&daemon, "shared");
    let mut second = daemon.connect(&["--force", "local:shared"]);
    first.expect("detached");
    assert!(first.wait_exit().success());

    second.send("echo two:$((1+1))\n");
    second.expect("two:2");
}

#[test]
fn resize_reaches_the_session() {
    let daemon = Daemon::start();
    let mut pty = attach_ready(&daemon, "size");
    pty.send("stty size\n");
    pty.expect("24 80");
}

#[test]
fn client_reconnects_after_the_daemon_link_drops() {
    require_socat();
    let daemon = Daemon::start();
    let proxy_path = daemon.dir.path().join("proxy.sock");
    let mut proxy = Socat::start(&proxy_path, &daemon.ctl_path());

    let mut pty = Pty::spawn(
        &["--ctl-socket", proxy_path.to_str().unwrap(), "connect", "local:link"],
        daemon.dir.path(),
    );
    probe_ready(&mut pty);
    let mark = pty.expect("READY:42");

    // Sever every proxied connection: the client sees EOF on its link while
    // the daemon-side session stays alive and attached.
    proxy.kill_all();
    pty.expect_from(mark, "reconnecting");
    let _proxy = Socat::start(&proxy_path, &daemon.ctl_path());

    // Keystrokes during the reconnect loop mean "retry now" and are dropped,
    // so wait for the reattach before typing. Same shell, resumed from the
    // offset the client already had.
    pty.expect_logged("reconnect: connected");
    pty.send("echo BACK:$((2+2))\n");
    pty.expect_from(mark, "BACK:4");
    assert!(pty.is_running());
}

#[test]
fn stalled_daemon_is_detected_by_heartbeat_and_recovered_from() {
    let daemon = Daemon::start();
    daemon.write_config("[defaults]\nheartbeat-interval = 1\nheartbeat-timeout = 2\n");
    let mut pty = attach_ready(&daemon, "stall");
    let mark = pty.expect("READY:42");

    // A SIGSTOPped daemon is the wifi->cellular blackhole in miniature: the
    // socket stays connected, nothing answers. No Pong within the heartbeat
    // timeout must be treated as a dead link. The client keeps this off the
    // terminal until a reconnect attempt has actually failed (the first one
    // hangs in the handshake's 10s timeout against a stopped daemon), so the
    // observable is its log, not the status line.
    kill(daemon.pid(), Signal::SIGSTOP).unwrap();
    pty.expect_logged("link down: heartbeat idle timeout");
    pty.expect_logged("entering reconnect loop");
    kill(daemon.pid(), Signal::SIGCONT).unwrap();

    pty.send("echo ALIVE:$((3+3))\n");
    pty.expect_from(mark, "ALIVE:6");
    assert!(pty.is_running());
}

// ---------------------------------------------------------------------------
// Socat proxy between client and daemon, killable as a whole process group.
// ---------------------------------------------------------------------------

struct Socat(Child);

impl Socat {
    fn start(listen: &Path, connect: &Path) -> Self {
        let _ = std::fs::remove_file(listen);
        // Safety: setpgid in the child touches only its own process state.
        let child = unsafe {
            Command::new("socat")
                .args([
                    &format!("UNIX-LISTEN:{},fork", listen.display()),
                    &format!("UNIX-CONNECT:{}", connect.display()),
                ])
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .pre_exec(|| {
                    libc::setpgid(0, 0);
                    Ok(())
                })
                .spawn()
                .expect("start socat proxy")
        };
        wait_for_socket(listen);
        Self(child)
    }

    /// Kill the listener and every per-connection child.
    fn kill_all(&mut self) {
        let pgid = Pid::from_raw(self.0.id() as i32);
        let _ = nix::sys::signal::killpg(pgid, Signal::SIGKILL);
        let _ = self.0.wait();
    }
}

impl Drop for Socat {
    fn drop(&mut self) {
        self.kill_all();
    }
}
