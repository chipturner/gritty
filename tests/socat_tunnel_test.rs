use futures_util::{SinkExt, StreamExt};
use gritty::protocol::{Frame, FrameCodec};
use std::path::PathBuf;
use std::process::{Child, Command};
use std::time::Duration;
use tokio::net::UnixStream;
use tokio::time::timeout;
use tokio_util::codec::Framed;

macro_rules! skip_if_no_socat {
    () => {
        if std::env::var("GRITTY_SOCAT_TEST").as_deref() == Ok("0") {
            eprintln!("skipping (GRITTY_SOCAT_TEST=0)");
            return;
        }
        if std::process::Command::new("socat")
            .arg("-V")
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()
            .is_err()
        {
            eprintln!("skipping (socat not found)");
            return;
        }
    };
}

fn gritty_bin() -> PathBuf {
    let mut path = PathBuf::from(env!("CARGO_BIN_EXE_gritty"));
    if !path.exists() {
        path = PathBuf::from("target/debug/gritty");
    }
    path
}

fn start_server(ctl_sock: &std::path::Path) -> Child {
    Command::new(gritty_bin())
        .args(["server", "--foreground", "--ctl-socket"])
        .arg(ctl_sock)
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
        .expect("failed to start gritty server")
}

fn start_socat_proxy(listen: &std::path::Path, connect: &std::path::Path) -> Child {
    Command::new("socat")
        .args([
            &format!("UNIX-LISTEN:{},fork", listen.display()),
            &format!("UNIX-CONNECT:{}", connect.display()),
        ])
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
        .expect("failed to start socat proxy")
}

fn wait_for_socket(path: &std::path::Path, timeout_secs: u64) {
    let deadline = std::time::Instant::now() + Duration::from_secs(timeout_secs);
    while !path.exists() {
        if std::time::Instant::now() > deadline {
            panic!("socket never appeared: {path:?}");
        }
        std::thread::sleep(Duration::from_millis(100));
    }
}

fn gritty_list(ctl_sock: &std::path::Path) -> Result<String, String> {
    let out = Command::new(gritty_bin())
        .args(["ls", "--ctl-socket"])
        .arg(ctl_sock)
        .output()
        .expect("failed to run gritty ls");
    if out.status.success() {
        Ok(String::from_utf8_lossy(&out.stdout).into_owned())
    } else {
        Err(String::from_utf8_lossy(&out.stderr).into_owned())
    }
}

struct ServerGuard(Child);

impl Drop for ServerGuard {
    fn drop(&mut self) {
        unsafe { libc::kill(self.0.id() as libc::pid_t, libc::SIGTERM) };
        let _ = self.0.wait();
    }
}

struct SocatGuard(Child);

impl Drop for SocatGuard {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

#[test]
fn tunnel_death_server_survives() {
    skip_if_no_socat!();

    let tmp = tempfile::tempdir().unwrap();
    let ctl_sock = tmp.path().join("ctl.sock");
    let proxy_sock = tmp.path().join("proxy.sock");

    // Start server
    let _server = ServerGuard(start_server(&ctl_sock));
    wait_for_socket(&ctl_sock, 5);

    // Start socat proxy
    let socat = start_socat_proxy(&proxy_sock, &ctl_sock);
    wait_for_socket(&proxy_sock, 5);
    let mut socat = SocatGuard(socat);

    // Works through proxy
    assert!(gritty_list(&proxy_sock).is_ok(), "ls via proxy should work");

    // Kill socat
    let _ = socat.0.kill();
    let _ = socat.0.wait();
    std::thread::sleep(Duration::from_millis(200));

    // Proxy gone -- ls through proxy fails
    assert!(gritty_list(&proxy_sock).is_err(), "ls via dead proxy should fail");

    // Clean up old socket so socat can rebind
    let _ = std::fs::remove_file(&proxy_sock);

    // Restart socat on same path
    let _socat2 = SocatGuard(start_socat_proxy(&proxy_sock, &ctl_sock));
    wait_for_socket(&proxy_sock, 5);
    // Small delay for socat to be ready to accept
    std::thread::sleep(Duration::from_millis(200));

    // Server survived -- works again
    assert!(gritty_list(&proxy_sock).is_ok(), "ls via restarted proxy should work");
}

#[tokio::test]
async fn tunnel_death_session_persists() {
    skip_if_no_socat!();

    let tmp = tempfile::tempdir().unwrap();
    let ctl_sock = tmp.path().join("ctl.sock");
    let proxy_sock = tmp.path().join("proxy.sock");

    // Start server
    let _server = ServerGuard(start_server(&ctl_sock));
    wait_for_socket(&ctl_sock, 5);

    // Start socat proxy
    let socat = start_socat_proxy(&proxy_sock, &ctl_sock);
    wait_for_socket(&proxy_sock, 5);
    let mut socat = SocatGuard(socat);

    // Create a session via protocol through the proxy
    let stream = UnixStream::connect(&proxy_sock).await.unwrap();
    let mut framed = Framed::new(stream, FrameCodec);
    framed.send(Frame::NewSession { name: "persist-test".to_string() }).await.unwrap();
    let resp = timeout(Duration::from_secs(5), framed.next())
        .await
        .expect("timed out")
        .expect("stream ended")
        .expect("decode error");
    let session_id = match resp {
        Frame::SessionCreated { id } => id,
        other => panic!("expected SessionCreated, got {other:?}"),
    };
    drop(framed);

    // Let the session settle
    tokio::time::sleep(Duration::from_millis(500)).await;

    // Kill socat (tunnel death)
    let _ = socat.0.kill();
    let _ = socat.0.wait();
    std::thread::sleep(Duration::from_millis(200));

    // Clean up old socket
    let _ = std::fs::remove_file(&proxy_sock);

    // Restart socat
    let _socat2 = SocatGuard(start_socat_proxy(&proxy_sock, &ctl_sock));
    wait_for_socket(&proxy_sock, 5);
    tokio::time::sleep(Duration::from_millis(200)).await;

    // List sessions -- our session should still be there
    let stream = UnixStream::connect(&proxy_sock).await.unwrap();
    let mut framed = Framed::new(stream, FrameCodec);
    framed.send(Frame::ListSessions).await.unwrap();
    let resp = timeout(Duration::from_secs(5), framed.next())
        .await
        .expect("timed out")
        .expect("stream ended")
        .expect("decode error");
    match resp {
        Frame::SessionInfo { sessions } => {
            assert!(
                sessions.iter().any(|s| s.id.to_string() == session_id),
                "session {session_id} not found in: {sessions:?}"
            );
        }
        other => panic!("expected SessionInfo, got {other:?}"),
    }
}
