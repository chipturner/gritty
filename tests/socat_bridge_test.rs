use bytes::Bytes;
use futures_util::{SinkExt, StreamExt};
use gritty::protocol::{Frame, FrameCodec, PROTOCOL_VERSION};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::Duration;
use tokio::net::UnixStream;
use tokio::time::timeout;
use tokio_util::codec::Framed;

// ---------------------------------------------------------------------------
// Skip macro & binary path
// ---------------------------------------------------------------------------

macro_rules! skip_if_no_socat {
    () => {
        if std::env::var("GRITTY_SOCAT_TEST").as_deref() == Ok("0") {
            eprintln!("skipping (GRITTY_SOCAT_TEST=0)");
            return;
        }
        if Command::new("socat")
            .arg("-V")
            .stdout(Stdio::null())
            .stderr(Stdio::null())
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

// ---------------------------------------------------------------------------
// Process guards
// ---------------------------------------------------------------------------

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
        // Kill the entire process group -- socat with `fork` spawns children per
        // connection and killing just the parent leaves them orphaned.
        unsafe { libc::killpg(self.0.id() as libc::pid_t, libc::SIGKILL) };
        let _ = self.0.wait();
    }
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

fn start_server(ctl_sock: &Path) -> ServerGuard {
    ServerGuard(
        Command::new(gritty_bin())
            .args(["server", "--foreground", "--ctl-socket"])
            .arg(ctl_sock)
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .expect("failed to start gritty server"),
    )
}

fn start_socat_proxy(listen: &Path, connect: &Path) -> SocatGuard {
    use std::os::unix::process::CommandExt;
    SocatGuard(unsafe {
        Command::new("socat")
            .args([
                &format!("UNIX-LISTEN:{},fork", listen.display()),
                &format!("UNIX-CONNECT:{}", connect.display()),
            ])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            // Make socat a process group leader so killpg() in Drop
            // also reaps forked children (one per proxied connection).
            .pre_exec(|| {
                libc::setpgid(0, 0);
                Ok(())
            })
            .spawn()
            .expect("failed to start socat proxy")
    })
}

fn wait_for_socket(path: &Path, timeout_secs: u64) {
    let deadline = std::time::Instant::now() + Duration::from_secs(timeout_secs);
    while !path.exists() {
        if std::time::Instant::now() > deadline {
            panic!("socket never appeared: {path:?}");
        }
        std::thread::sleep(Duration::from_millis(100));
    }
}

async fn do_handshake(framed: &mut Framed<UnixStream, FrameCodec>) {
    framed.send(Frame::Hello { version: PROTOCOL_VERSION }).await.unwrap();
    let resp = timeout(Duration::from_secs(5), framed.next())
        .await
        .expect("handshake timed out")
        .expect("stream ended during handshake")
        .expect("decode error during handshake");
    assert!(matches!(resp, Frame::HelloAck { .. }), "expected HelloAck, got {resp:?}");
}

async fn connect_and_handshake(proxy_path: &Path) -> Framed<UnixStream, FrameCodec> {
    let stream = UnixStream::connect(proxy_path).await.unwrap();
    let mut framed = Framed::new(stream, FrameCodec);
    do_handshake(&mut framed).await;
    framed
}

/// Send a control frame and return the response.
async fn control_request(proxy_path: &Path, frame: Frame) -> Frame {
    let mut framed = connect_and_handshake(proxy_path).await;
    framed.send(frame).await.unwrap();
    timeout(Duration::from_secs(5), framed.next())
        .await
        .expect("timed out waiting for response")
        .expect("stream ended")
        .expect("decode error")
}

/// Create a named session through the proxy. Returns (session_id, attached framed connection).
async fn create_session(proxy_path: &Path, name: &str) -> (String, Framed<UnixStream, FrameCodec>) {
    let mut framed = connect_and_handshake(proxy_path).await;
    framed
        .send(Frame::NewSession { name: name.to_string(), command: String::new() })
        .await
        .unwrap();
    let resp = timeout(Duration::from_secs(5), framed.next())
        .await
        .expect("timed out")
        .expect("stream ended")
        .expect("decode error");
    let id = match resp {
        Frame::SessionCreated { id } => id,
        other => panic!("expected SessionCreated, got {other:?}"),
    };
    (id, framed)
}

/// Attach to a session through the proxy. Returns the framed connection.
async fn attach_session(proxy_path: &Path, session: &str) -> Framed<UnixStream, FrameCodec> {
    let mut framed = connect_and_handshake(proxy_path).await;
    framed.send(Frame::Attach { session: session.to_string() }).await.unwrap();
    let resp = timeout(Duration::from_secs(5), framed.next())
        .await
        .expect("timed out")
        .expect("stream ended")
        .expect("decode error");
    assert_eq!(resp, Frame::Ok, "expected Ok for attach, got {resp:?}");
    framed
}

/// Send Env + Resize and wait for first Data frame (shell prompt).
async fn wait_for_shell(framed: &mut Framed<UnixStream, FrameCodec>) {
    framed.send(Frame::Env { vars: vec![] }).await.unwrap();
    framed.send(Frame::Resize { cols: 80, rows: 24 }).await.unwrap();
    let deadline = tokio::time::Instant::now() + Duration::from_secs(15);
    loop {
        match timeout(Duration::from_secs(1), framed.next()).await {
            Ok(Some(Ok(Frame::Data(_)))) => break,
            _ if tokio::time::Instant::now() >= deadline => {
                panic!("shell did not produce output within 15s")
            }
            _ => continue,
        }
    }
}

/// Drain all Data frames until timeout.
async fn drain_data(framed: &mut Framed<UnixStream, FrameCodec>, wait: Duration) -> Vec<u8> {
    let mut out = Vec::new();
    while let Ok(Some(Ok(Frame::Data(data)))) = timeout(wait, framed.next()).await {
        out.extend_from_slice(&data);
    }
    out
}

/// Send a command and wait until `expected` appears in output, or timeout.
async fn send_and_expect(
    framed: &mut Framed<UnixStream, FrameCodec>,
    cmd: &str,
    expected: &str,
) -> String {
    framed.send(Frame::Data(Bytes::from(format!("{cmd}\n")))).await.unwrap();
    let mut output = Vec::new();
    let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
    loop {
        match timeout(Duration::from_secs(1), framed.next()).await {
            Ok(Some(Ok(Frame::Data(data)))) => {
                output.extend_from_slice(&data);
                let s = String::from_utf8_lossy(&output);
                if s.contains(expected) {
                    return s.to_string();
                }
            }
            _ if tokio::time::Instant::now() >= deadline => {
                let s = String::from_utf8_lossy(&output);
                panic!("expected '{expected}' in output, got: {s}");
            }
            _ => continue,
        }
    }
}

/// Standard test setup: server + socat proxy. Returns guards + paths.
struct TestEnv {
    _tmp: tempfile::TempDir,
    ctl_path: PathBuf,
    proxy_path: PathBuf,
    _server: ServerGuard,
    socat: Option<SocatGuard>,
}

impl TestEnv {
    fn new() -> Self {
        let tmp = tempfile::tempdir().unwrap();
        let ctl_path = tmp.path().join("ctl.sock");
        let proxy_path = tmp.path().join("proxy.sock");
        let server = start_server(&ctl_path);
        wait_for_socket(&ctl_path, 5);
        let socat = start_socat_proxy(&proxy_path, &ctl_path);
        wait_for_socket(&proxy_path, 5);
        std::thread::sleep(Duration::from_millis(200));
        Self { _tmp: tmp, ctl_path, proxy_path, _server: server, socat: Some(socat) }
    }

    fn restart_socat(&mut self) {
        if let Some(mut guard) = self.socat.take() {
            // Kill parent only -- fork children die when their connections break.
            // Using killpg here would kill relay children before they finish
            // forwarding buffered data to the server.
            let _ = guard.0.kill();
            let _ = guard.0.wait();
            std::mem::forget(guard);
        }
        let _ = std::fs::remove_file(&self.proxy_path);
        self.socat = Some(start_socat_proxy(&self.proxy_path, &self.ctl_path));
        wait_for_socket(&self.proxy_path, 5);
        std::thread::sleep(Duration::from_millis(200));
    }

    fn kill_socat(&mut self) {
        if let Some(mut guard) = self.socat.take() {
            let _ = guard.0.kill();
            let _ = guard.0.wait();
            std::mem::forget(guard);
        }
        let _ = std::fs::remove_file(&self.proxy_path);
    }
}

// ===========================================================================
// Group 1: Session Lifecycle
// ===========================================================================

#[tokio::test]
async fn session_create_interact_through_proxy() {
    skip_if_no_socat!();
    let env = TestEnv::new();

    let (_, mut framed) = create_session(&env.proxy_path, "work").await;
    wait_for_shell(&mut framed).await;
    drain_data(&mut framed, Duration::from_millis(500)).await;

    send_and_expect(&mut framed, "echo HELLO", "HELLO").await;
}

#[tokio::test]
async fn session_create_list_kill_through_proxy() {
    skip_if_no_socat!();
    let env = TestEnv::new();

    let (id_a, _) = create_session(&env.proxy_path, "alpha").await;
    tokio::time::sleep(Duration::from_millis(200)).await;
    let (id_b, _) = create_session(&env.proxy_path, "beta").await;
    tokio::time::sleep(Duration::from_millis(200)).await;

    // List -- both present
    let resp = control_request(&env.proxy_path, Frame::ListSessions).await;
    match &resp {
        Frame::SessionInfo { sessions } => {
            assert_eq!(sessions.len(), 2, "expected 2 sessions, got {sessions:?}");
        }
        other => panic!("expected SessionInfo, got {other:?}"),
    }

    // Kill alpha
    let resp = control_request(&env.proxy_path, Frame::KillSession { session: id_a.clone() }).await;
    assert_eq!(resp, Frame::Ok);
    tokio::time::sleep(Duration::from_millis(300)).await;

    // List -- only beta
    let resp = control_request(&env.proxy_path, Frame::ListSessions).await;
    match &resp {
        Frame::SessionInfo { sessions } => {
            assert_eq!(sessions.len(), 1);
            assert_eq!(sessions[0].name, "beta");
        }
        other => panic!("expected SessionInfo, got {other:?}"),
    }

    // Kill beta
    let resp = control_request(&env.proxy_path, Frame::KillSession { session: id_b.clone() }).await;
    assert_eq!(resp, Frame::Ok);
    tokio::time::sleep(Duration::from_millis(300)).await;

    // List -- empty
    let resp = control_request(&env.proxy_path, Frame::ListSessions).await;
    match &resp {
        Frame::SessionInfo { sessions } => {
            assert!(sessions.is_empty(), "expected empty, got {sessions:?}");
        }
        other => panic!("expected SessionInfo, got {other:?}"),
    }
}

#[tokio::test]
async fn session_env_forwarding_through_proxy() {
    skip_if_no_socat!();
    let env = TestEnv::new();

    let (_, mut framed) = create_session(&env.proxy_path, "envtest").await;
    framed
        .send(Frame::Env { vars: vec![("TERM".to_string(), "xterm-test".to_string())] })
        .await
        .unwrap();
    framed.send(Frame::Resize { cols: 80, rows: 24 }).await.unwrap();

    // Wait for shell
    let deadline = tokio::time::Instant::now() + Duration::from_secs(15);
    loop {
        match timeout(Duration::from_secs(1), framed.next()).await {
            Ok(Some(Ok(Frame::Data(_)))) => break,
            _ if tokio::time::Instant::now() >= deadline => {
                panic!("shell did not produce output within 15s")
            }
            _ => continue,
        }
    }
    drain_data(&mut framed, Duration::from_millis(500)).await;

    send_and_expect(&mut framed, "echo $TERM", "xterm-test").await;
}

#[tokio::test]
async fn session_disconnect_reattach_preserves_state() {
    skip_if_no_socat!();
    let env = TestEnv::new();

    let (id, mut framed) = create_session(&env.proxy_path, "persist").await;
    wait_for_shell(&mut framed).await;
    drain_data(&mut framed, Duration::from_millis(500)).await;

    // Set a unique marker value
    send_and_expect(&mut framed, "export MARKER=alive999", "alive999").await;

    // Disconnect
    drop(framed);
    tokio::time::sleep(Duration::from_millis(500)).await;

    // Reattach
    let mut framed = attach_session(&env.proxy_path, &id).await;
    framed.send(Frame::Resize { cols: 80, rows: 24 }).await.unwrap();
    drain_data(&mut framed, Duration::from_millis(500)).await;

    send_and_expect(&mut framed, "echo $MARKER", "alive999").await;
}

#[tokio::test]
async fn session_exit_detected_through_proxy() {
    skip_if_no_socat!();
    let env = TestEnv::new();

    let (id, mut framed) = create_session(&env.proxy_path, "exiter").await;
    wait_for_shell(&mut framed).await;
    drain_data(&mut framed, Duration::from_millis(500)).await;

    framed.send(Frame::Data(Bytes::from("exit 42\n"))).await.unwrap();

    // Expect Exit frame
    let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
    let mut saw_exit = false;
    loop {
        match timeout(Duration::from_secs(1), framed.next()).await {
            Ok(Some(Ok(Frame::Exit { code }))) => {
                assert_eq!(code, 42, "expected exit code 42, got {code}");
                saw_exit = true;
                break;
            }
            Ok(Some(Ok(Frame::Data(_)))) => continue,
            _ if tokio::time::Instant::now() >= deadline => break,
            _ => continue,
        }
    }
    assert!(saw_exit, "should have received Exit frame");

    // Session should be reaped
    tokio::time::sleep(Duration::from_millis(500)).await;
    let resp = control_request(&env.proxy_path, Frame::ListSessions).await;
    match &resp {
        Frame::SessionInfo { sessions } => {
            assert!(
                !sessions.iter().any(|s| s.id == id),
                "exited session should be reaped, got {sessions:?}"
            );
        }
        other => panic!("expected SessionInfo, got {other:?}"),
    }
}

// ===========================================================================
// Group 2: Client Takeover
// ===========================================================================

#[tokio::test]
async fn second_client_detaches_first_through_proxy() {
    skip_if_no_socat!();
    let env = TestEnv::new();

    let (id, mut client1) = create_session(&env.proxy_path, "takeover").await;
    wait_for_shell(&mut client1).await;
    drain_data(&mut client1, Duration::from_millis(500)).await;

    // Attach client2
    let mut client2 = attach_session(&env.proxy_path, &id).await;

    // Client1 should receive Detached
    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    let mut saw_detached = false;
    loop {
        match timeout(Duration::from_secs(1), client1.next()).await {
            Ok(Some(Ok(Frame::Detached))) => {
                saw_detached = true;
                break;
            }
            Ok(Some(Ok(Frame::Data(_)))) => continue,
            _ if tokio::time::Instant::now() >= deadline => break,
            _ => break,
        }
    }
    assert!(saw_detached, "client1 should have received Detached");

    // Client2 can interact
    client2.send(Frame::Resize { cols: 80, rows: 24 }).await.unwrap();
    drain_data(&mut client2, Duration::from_millis(500)).await;

    send_and_expect(&mut client2, "echo TAKEOVER_OK", "TAKEOVER_OK").await;
}

#[tokio::test]
async fn rapid_takeover_cycles_through_proxy() {
    skip_if_no_socat!();
    let env = TestEnv::new();

    let (id, mut framed) = create_session(&env.proxy_path, "rapid").await;
    wait_for_shell(&mut framed).await;
    drain_data(&mut framed, Duration::from_millis(500)).await;

    send_and_expect(&mut framed, "export RAPID_MARKER=stable77", "stable77").await;
    drop(framed);
    tokio::time::sleep(Duration::from_millis(300)).await;

    // Cycle 3 times
    for i in 0..3 {
        let mut client = attach_session(&env.proxy_path, &id).await;
        client.send(Frame::Resize { cols: 80, rows: 24 }).await.unwrap();
        drain_data(&mut client, Duration::from_millis(500)).await;

        if i == 2 {
            // Final client verifies marker
            send_and_expect(&mut client, "echo $RAPID_MARKER", "stable77").await;
        }

        drop(client);
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
}

#[tokio::test]
async fn takeover_by_name_and_by_id() {
    skip_if_no_socat!();
    let env = TestEnv::new();

    let (id, framed) = create_session(&env.proxy_path, "named").await;
    drop(framed);
    tokio::time::sleep(Duration::from_millis(500)).await;

    // Attach by name
    let mut framed = attach_session(&env.proxy_path, "named").await;
    framed.send(Frame::Resize { cols: 80, rows: 24 }).await.unwrap();
    drain_data(&mut framed, Duration::from_millis(500)).await;
    drop(framed);
    tokio::time::sleep(Duration::from_millis(300)).await;

    // Attach by id
    let mut framed = attach_session(&env.proxy_path, &id).await;
    framed.send(Frame::Resize { cols: 80, rows: 24 }).await.unwrap();
    drain_data(&mut framed, Duration::from_millis(500)).await;
}

// ===========================================================================
// Group 3: Tunnel Disruption (highest value)
// ===========================================================================

#[tokio::test]
async fn tunnel_death_during_active_session() {
    skip_if_no_socat!();
    let mut env = TestEnv::new();

    let (id, mut framed) = create_session(&env.proxy_path, "survivor").await;
    wait_for_shell(&mut framed).await;
    drain_data(&mut framed, Duration::from_millis(500)).await;

    send_and_expect(&mut framed, "export MARKER=survived88", "survived88").await;
    drop(framed);

    // Kill socat
    env.kill_socat();
    tokio::time::sleep(Duration::from_millis(500)).await;

    // Restart socat
    env.restart_socat();

    // Reattach
    let mut framed = attach_session(&env.proxy_path, &id).await;
    framed.send(Frame::Resize { cols: 80, rows: 24 }).await.unwrap();
    drain_data(&mut framed, Duration::from_millis(500)).await;

    send_and_expect(&mut framed, "echo $MARKER", "survived88").await;
}

#[tokio::test]
async fn tunnel_death_buffered_output_preserved() {
    skip_if_no_socat!();
    let mut env = TestEnv::new();

    let (id, mut framed) = create_session(&env.proxy_path, "buffered").await;
    wait_for_shell(&mut framed).await;
    drain_data(&mut framed, Duration::from_millis(500)).await;

    // Start a background job that produces output while we're disconnected
    framed
        .send(Frame::Data(Bytes::from("{ sleep 0.3; seq 1 1000; echo BUFFERED_DONE; } &\n")))
        .await
        .unwrap();
    drain_data(&mut framed, Duration::from_millis(200)).await;
    drop(framed);

    // Kill socat immediately
    env.kill_socat();

    // Wait for output to be produced while disconnected
    tokio::time::sleep(Duration::from_secs(2)).await;

    // Restart socat
    env.restart_socat();

    // Reattach -- buffered output should drain
    let mut framed = attach_session(&env.proxy_path, &id).await;
    framed.send(Frame::Resize { cols: 80, rows: 24 }).await.unwrap();

    let output = drain_data(&mut framed, Duration::from_secs(3)).await;
    let output_str = String::from_utf8_lossy(&output);
    assert!(
        output_str.contains("BUFFERED_DONE"),
        "buffered output should contain BUFFERED_DONE, got {} bytes",
        output.len()
    );
}

#[tokio::test]
async fn tunnel_death_ring_buffer_overflow_marker() {
    skip_if_no_socat!();
    let mut env = TestEnv::new();

    let (id, mut framed) = create_session(&env.proxy_path, "overflow").await;
    wait_for_shell(&mut framed).await;
    drain_data(&mut framed, Duration::from_millis(500)).await;

    // Produce >1MB of output. Use yes | head to generate lots of data quickly.
    // Each line is 2 bytes ("y\n"), 600000 lines = ~1.2MB.
    framed
        .send(Frame::Data(Bytes::from("yes | head -n 600000; echo OVERFLOW_DONE\n")))
        .await
        .unwrap();
    // Don't drain -- disconnect immediately so output accumulates in ring buffer
    drop(framed);

    // Kill socat while output is being produced
    env.kill_socat();

    // Wait for all output to complete and overflow the ring buffer
    tokio::time::sleep(Duration::from_secs(5)).await;

    // Restart and reattach
    env.restart_socat();

    let mut framed = attach_session(&env.proxy_path, &id).await;
    framed.send(Frame::Resize { cols: 80, rows: 24 }).await.unwrap();

    let output = drain_data(&mut framed, Duration::from_secs(3)).await;
    let output_str = String::from_utf8_lossy(&output);
    assert!(
        output_str.contains("bytes of output dropped"),
        "should see ring buffer overflow marker, got {} bytes of output: {}",
        output.len(),
        &output_str[..output_str.len().min(200)]
    );
}

#[tokio::test]
async fn tunnel_death_multiple_sessions_survive() {
    skip_if_no_socat!();
    let mut env = TestEnv::new();

    // Create 3 sessions with unique markers
    let mut sessions = Vec::new();
    for (i, name) in ["sess-a", "sess-b", "sess-c"].iter().enumerate() {
        let (id, mut framed) = create_session(&env.proxy_path, name).await;
        wait_for_shell(&mut framed).await;
        drain_data(&mut framed, Duration::from_millis(500)).await;

        let marker = format!("MULTI_{i}_x7z");
        send_and_expect(&mut framed, &format!("export MARKER={marker}"), &marker).await;
        sessions.push((id, marker));
        drop(framed);
        tokio::time::sleep(Duration::from_millis(200)).await;
    }

    // Kill socat
    env.kill_socat();
    tokio::time::sleep(Duration::from_millis(500)).await;

    // Restart socat
    env.restart_socat();

    // Verify all sessions survived
    for (id, marker) in &sessions {
        let mut framed = attach_session(&env.proxy_path, id).await;
        framed.send(Frame::Resize { cols: 80, rows: 24 }).await.unwrap();
        drain_data(&mut framed, Duration::from_millis(500)).await;

        send_and_expect(&mut framed, "echo $MARKER", marker).await;
        drop(framed);
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
}

#[tokio::test]
async fn tunnel_flap_rapid_kill_restart() {
    skip_if_no_socat!();
    let mut env = TestEnv::new();

    let (id, mut framed) = create_session(&env.proxy_path, "flapper").await;
    wait_for_shell(&mut framed).await;
    drain_data(&mut framed, Duration::from_millis(500)).await;

    send_and_expect(&mut framed, "export FLAP=stable55", "stable55").await;
    drop(framed);

    // Rapid kill/restart cycles
    for _ in 0..5 {
        env.kill_socat();
        tokio::time::sleep(Duration::from_millis(200)).await;
        env.restart_socat();
    }

    // Reattach and verify
    let mut framed = attach_session(&env.proxy_path, &id).await;
    framed.send(Frame::Resize { cols: 80, rows: 24 }).await.unwrap();
    drain_data(&mut framed, Duration::from_millis(500)).await;

    send_and_expect(&mut framed, "echo $FLAP", "stable55").await;
}

// ===========================================================================
// Group 4: Tail Through Proxy
// ===========================================================================

#[tokio::test]
async fn tail_receives_output_through_proxy() {
    skip_if_no_socat!();
    let env = TestEnv::new();

    let (id, mut client) = create_session(&env.proxy_path, "tailtarget").await;
    wait_for_shell(&mut client).await;
    drain_data(&mut client, Duration::from_millis(500)).await;

    // Open tail connection
    let mut tail = connect_and_handshake(&env.proxy_path).await;
    tail.send(Frame::Tail { session: id.clone() }).await.unwrap();
    let resp = timeout(Duration::from_secs(5), tail.next())
        .await
        .expect("timed out")
        .expect("stream ended")
        .expect("decode error");
    assert_eq!(resp, Frame::Ok, "expected Ok for tail, got {resp:?}");

    // Drain any ring buffer content the tail gets initially
    drain_data(&mut tail, Duration::from_millis(500)).await;

    // Send command via active client
    client.send(Frame::Data(Bytes::from("echo TAIL_VISIBLE\n"))).await.unwrap();

    // Tail should see the output
    let mut tail_output = Vec::new();
    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    loop {
        match timeout(Duration::from_secs(1), tail.next()).await {
            Ok(Some(Ok(Frame::Data(data)))) => {
                tail_output.extend_from_slice(&data);
                let s = String::from_utf8_lossy(&tail_output);
                if s.contains("TAIL_VISIBLE") {
                    break;
                }
            }
            _ if tokio::time::Instant::now() >= deadline => {
                let s = String::from_utf8_lossy(&tail_output);
                panic!("tail did not see TAIL_VISIBLE, got: {s}");
            }
            _ => continue,
        }
    }
}

#[tokio::test]
async fn tail_survives_tunnel_death() {
    skip_if_no_socat!();
    let mut env = TestEnv::new();

    let (id, mut client) = create_session(&env.proxy_path, "taildeath").await;
    wait_for_shell(&mut client).await;
    drain_data(&mut client, Duration::from_millis(500)).await;
    drop(client);

    // Kill and restart socat
    env.kill_socat();
    tokio::time::sleep(Duration::from_millis(500)).await;
    env.restart_socat();

    // Re-establish both connections
    let mut client = attach_session(&env.proxy_path, &id).await;
    client.send(Frame::Resize { cols: 80, rows: 24 }).await.unwrap();
    drain_data(&mut client, Duration::from_millis(500)).await;

    let mut tail = connect_and_handshake(&env.proxy_path).await;
    tail.send(Frame::Tail { session: id.clone() }).await.unwrap();
    let resp = timeout(Duration::from_secs(5), tail.next())
        .await
        .expect("timed out")
        .expect("stream ended")
        .expect("decode error");
    assert_eq!(resp, Frame::Ok);
    drain_data(&mut tail, Duration::from_millis(500)).await;

    // Command via client, tail sees it
    client.send(Frame::Data(Bytes::from("echo TAIL_AFTER_DEATH\n"))).await.unwrap();

    let mut tail_output = Vec::new();
    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    loop {
        match timeout(Duration::from_secs(1), tail.next()).await {
            Ok(Some(Ok(Frame::Data(data)))) => {
                tail_output.extend_from_slice(&data);
                if String::from_utf8_lossy(&tail_output).contains("TAIL_AFTER_DEATH") {
                    break;
                }
            }
            _ if tokio::time::Instant::now() >= deadline => {
                let s = String::from_utf8_lossy(&tail_output);
                panic!("tail did not see TAIL_AFTER_DEATH, got: {s}");
            }
            _ => continue,
        }
    }
}

#[tokio::test]
async fn tail_does_not_detach_active_through_proxy() {
    skip_if_no_socat!();
    let env = TestEnv::new();

    let (id, mut client) = create_session(&env.proxy_path, "tailnodetach").await;
    wait_for_shell(&mut client).await;
    drain_data(&mut client, Duration::from_millis(500)).await;

    // Open tail
    let mut tail = connect_and_handshake(&env.proxy_path).await;
    tail.send(Frame::Tail { session: id.clone() }).await.unwrap();
    let resp = timeout(Duration::from_secs(5), tail.next())
        .await
        .expect("timed out")
        .expect("stream ended")
        .expect("decode error");
    assert_eq!(resp, Frame::Ok);
    drain_data(&mut tail, Duration::from_millis(500)).await;

    // Client should NOT receive Detached -- can still interact
    send_and_expect(&mut client, "echo STILL_ACTIVE", "STILL_ACTIVE").await;
}

// ===========================================================================
// Group 5: File Transfer Through Daemon
// ===========================================================================

#[tokio::test]
async fn file_transfer_single_file_through_daemon() {
    skip_if_no_socat!();
    let env = TestEnv::new();

    let (id, mut client) = create_session(&env.proxy_path, "xfer").await;
    wait_for_shell(&mut client).await;
    drain_data(&mut client, Duration::from_millis(500)).await;

    let file_data = b"hello from sender";
    let file_name = "test.txt";

    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    // Sender: connect, handshake, SendFile with role=2 (Send)
    let mut sender = connect_and_handshake(&env.proxy_path).await;
    sender.send(Frame::SendFile { session: id.clone(), role: 2 }).await.unwrap();
    let resp = timeout(Duration::from_secs(5), sender.next())
        .await
        .expect("timed out")
        .expect("stream ended")
        .expect("decode error");
    assert_eq!(resp, Frame::Ok, "sender should get Ok");

    // Extract raw stream and write SvcRequest discriminator
    let mut sender_stream = sender.into_inner();
    sender_stream.write_all(&[gritty::protocol::SvcRequest::Send.to_byte()]).await.unwrap();

    // Receiver: connect, handshake, SendFile with role=3 (Receive)
    let recv_dir = tempfile::tempdir().unwrap();
    let mut receiver = connect_and_handshake(&env.proxy_path).await;
    receiver.send(Frame::SendFile { session: id.clone(), role: 3 }).await.unwrap();
    let resp = timeout(Duration::from_secs(5), receiver.next())
        .await
        .expect("timed out")
        .expect("stream ended")
        .expect("decode error");
    assert_eq!(resp, Frame::Ok, "receiver should get Ok");

    let mut receiver_stream = receiver.into_inner();
    receiver_stream.write_all(&[gritty::protocol::SvcRequest::Receive.to_byte()]).await.unwrap();

    // Write sender manifest: file_count(4) + per-file(name_len(2) + name + size(8) + mode(4))
    let mut manifest = Vec::new();
    manifest.extend_from_slice(&1u32.to_be_bytes()); // file_count
    manifest.extend_from_slice(&(file_name.len() as u16).to_be_bytes()); // name_len
    manifest.extend_from_slice(file_name.as_bytes()); // name
    manifest.extend_from_slice(&(file_data.len() as u64).to_be_bytes()); // size
    manifest.extend_from_slice(&0o644u32.to_be_bytes()); // mode
    sender_stream.write_all(&manifest).await.unwrap();

    // Write receiver dest dir
    let dest = format!("{}\n", recv_dir.path().display());
    receiver_stream.write_all(dest.as_bytes()).await.unwrap();

    // Sender waits for go signal (0x01)
    let mut go = [0u8; 1];
    timeout(Duration::from_secs(10), sender_stream.read_exact(&mut go))
        .await
        .expect("timed out waiting for go signal")
        .expect("read error");
    assert_eq!(go[0], 0x01, "expected go signal");

    // Sender writes file: name_len(2) + name + size(8) + data
    let mut file_header = Vec::new();
    file_header.extend_from_slice(&(file_name.len() as u16).to_be_bytes());
    file_header.extend_from_slice(file_name.as_bytes());
    file_header.extend_from_slice(&(file_data.len() as u64).to_be_bytes());
    sender_stream.write_all(&file_header).await.unwrap();
    sender_stream.write_all(file_data).await.unwrap();

    // Sentinel: name_len=0
    sender_stream.write_all(&0u16.to_be_bytes()).await.unwrap();

    // Receiver reads file
    let mut recv_buf = vec![0u8; 4096];
    let n = timeout(Duration::from_secs(5), receiver_stream.read(&mut recv_buf))
        .await
        .expect("timed out reading received file")
        .expect("read error");
    assert!(n > 0, "receiver should get data");

    // Verify active client got SendOffer notification
    let mut saw_offer = false;
    let mut saw_done = false;
    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    loop {
        match timeout(Duration::from_secs(1), client.next()).await {
            Ok(Some(Ok(Frame::SendOffer { file_count, .. }))) => {
                assert_eq!(file_count, 1);
                saw_offer = true;
            }
            Ok(Some(Ok(Frame::SendDone))) => {
                saw_done = true;
                break;
            }
            Ok(Some(Ok(Frame::Data(_)))) => continue,
            _ if tokio::time::Instant::now() >= deadline => break,
            _ => continue,
        }
    }
    assert!(saw_offer, "should have seen SendOffer");
    assert!(saw_done, "should have seen SendDone");
}

#[tokio::test]
async fn file_transfer_receiver_first_through_daemon() {
    skip_if_no_socat!();
    let env = TestEnv::new();

    let (id, mut _client) = create_session(&env.proxy_path, "xfer-recv-first").await;
    wait_for_shell(&mut _client).await;
    drain_data(&mut _client, Duration::from_millis(500)).await;

    let file_data = b"receiver-first data";
    let file_name = "recv_first.txt";
    let recv_dir = tempfile::tempdir().unwrap();

    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    // Receiver connects first
    let mut receiver = connect_and_handshake(&env.proxy_path).await;
    receiver.send(Frame::SendFile { session: id.clone(), role: 3 }).await.unwrap();
    let resp = timeout(Duration::from_secs(5), receiver.next())
        .await
        .expect("timed out")
        .expect("stream ended")
        .expect("decode error");
    assert_eq!(resp, Frame::Ok);
    let mut receiver_stream = receiver.into_inner();
    receiver_stream.write_all(&[gritty::protocol::SvcRequest::Receive.to_byte()]).await.unwrap();

    // Write dest dir
    let dest = format!("{}\n", recv_dir.path().display());
    receiver_stream.write_all(dest.as_bytes()).await.unwrap();

    // Small delay to ensure receiver is registered
    tokio::time::sleep(Duration::from_millis(200)).await;

    // Then sender connects
    let mut sender = connect_and_handshake(&env.proxy_path).await;
    sender.send(Frame::SendFile { session: id.clone(), role: 2 }).await.unwrap();
    let resp = timeout(Duration::from_secs(5), sender.next())
        .await
        .expect("timed out")
        .expect("stream ended")
        .expect("decode error");
    assert_eq!(resp, Frame::Ok);
    let mut sender_stream = sender.into_inner();
    sender_stream.write_all(&[gritty::protocol::SvcRequest::Send.to_byte()]).await.unwrap();

    let mut manifest = Vec::new();
    manifest.extend_from_slice(&1u32.to_be_bytes());
    manifest.extend_from_slice(&(file_name.len() as u16).to_be_bytes());
    manifest.extend_from_slice(file_name.as_bytes());
    manifest.extend_from_slice(&(file_data.len() as u64).to_be_bytes());
    manifest.extend_from_slice(&0o644u32.to_be_bytes()); // mode
    sender_stream.write_all(&manifest).await.unwrap();

    // Wait for go signal
    let mut go = [0u8; 1];
    timeout(Duration::from_secs(10), sender_stream.read_exact(&mut go))
        .await
        .expect("timed out waiting for go signal")
        .expect("read error");
    assert_eq!(go[0], 0x01);

    // Write file data
    let mut file_header = Vec::new();
    file_header.extend_from_slice(&(file_name.len() as u16).to_be_bytes());
    file_header.extend_from_slice(file_name.as_bytes());
    file_header.extend_from_slice(&(file_data.len() as u64).to_be_bytes());
    sender_stream.write_all(&file_header).await.unwrap();
    sender_stream.write_all(file_data).await.unwrap();
    sender_stream.write_all(&0u16.to_be_bytes()).await.unwrap();

    // Receiver should get data
    let mut recv_buf = vec![0u8; 4096];
    let n = timeout(Duration::from_secs(5), receiver_stream.read(&mut recv_buf))
        .await
        .expect("timed out")
        .expect("read error");
    assert!(n > 0, "receiver should get data when connecting first");
}

#[tokio::test]
async fn file_transfer_survives_tunnel_death() {
    skip_if_no_socat!();
    let mut env = TestEnv::new();

    let (id, mut client) = create_session(&env.proxy_path, "xfer-death").await;
    wait_for_shell(&mut client).await;
    drain_data(&mut client, Duration::from_millis(500)).await;
    drop(client);

    // Kill socat (simulating tunnel death during potential transfer)
    env.kill_socat();
    tokio::time::sleep(Duration::from_millis(500)).await;
    env.restart_socat();

    // Now do a full transfer on the restarted tunnel
    let mut client = attach_session(&env.proxy_path, &id).await;
    client.send(Frame::Resize { cols: 80, rows: 24 }).await.unwrap();
    drain_data(&mut client, Duration::from_millis(500)).await;

    let file_data = b"post-death transfer";
    let file_name = "post_death.txt";
    let recv_dir = tempfile::tempdir().unwrap();

    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    let mut sender = connect_and_handshake(&env.proxy_path).await;
    sender.send(Frame::SendFile { session: id.clone(), role: 2 }).await.unwrap();
    let resp = timeout(Duration::from_secs(5), sender.next())
        .await
        .expect("timed out")
        .expect("stream ended")
        .expect("decode error");
    assert_eq!(resp, Frame::Ok);
    let mut sender_stream = sender.into_inner();
    sender_stream.write_all(&[gritty::protocol::SvcRequest::Send.to_byte()]).await.unwrap();

    let mut receiver = connect_and_handshake(&env.proxy_path).await;
    receiver.send(Frame::SendFile { session: id.clone(), role: 3 }).await.unwrap();
    let resp = timeout(Duration::from_secs(5), receiver.next())
        .await
        .expect("timed out")
        .expect("stream ended")
        .expect("decode error");
    assert_eq!(resp, Frame::Ok);
    let mut receiver_stream = receiver.into_inner();
    receiver_stream.write_all(&[gritty::protocol::SvcRequest::Receive.to_byte()]).await.unwrap();

    // Manifest
    let mut manifest = Vec::new();
    manifest.extend_from_slice(&1u32.to_be_bytes());
    manifest.extend_from_slice(&(file_name.len() as u16).to_be_bytes());
    manifest.extend_from_slice(file_name.as_bytes());
    manifest.extend_from_slice(&(file_data.len() as u64).to_be_bytes());
    manifest.extend_from_slice(&0o644u32.to_be_bytes()); // mode
    sender_stream.write_all(&manifest).await.unwrap();

    // Dest dir
    receiver_stream.write_all(format!("{}\n", recv_dir.path().display()).as_bytes()).await.unwrap();

    // Go signal
    let mut go = [0u8; 1];
    timeout(Duration::from_secs(10), sender_stream.read_exact(&mut go))
        .await
        .expect("timed out")
        .expect("read error");
    assert_eq!(go[0], 0x01);

    // File data
    let mut file_header = Vec::new();
    file_header.extend_from_slice(&(file_name.len() as u16).to_be_bytes());
    file_header.extend_from_slice(file_name.as_bytes());
    file_header.extend_from_slice(&(file_data.len() as u64).to_be_bytes());
    sender_stream.write_all(&file_header).await.unwrap();
    sender_stream.write_all(file_data).await.unwrap();
    sender_stream.write_all(&0u16.to_be_bytes()).await.unwrap();

    // Verify receiver gets data
    let mut recv_buf = vec![0u8; 4096];
    let n = timeout(Duration::from_secs(5), receiver_stream.read(&mut recv_buf))
        .await
        .expect("timed out")
        .expect("read error");
    assert!(n > 0, "transfer should work after tunnel death");
}

// ===========================================================================
// Group 6: Heartbeat
// ===========================================================================

#[tokio::test]
async fn ping_pong_through_proxy() {
    skip_if_no_socat!();
    let env = TestEnv::new();

    let (_, mut framed) = create_session(&env.proxy_path, "pingpong").await;
    wait_for_shell(&mut framed).await;
    drain_data(&mut framed, Duration::from_millis(500)).await;

    framed.send(Frame::Ping).await.unwrap();
    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    loop {
        match timeout(Duration::from_secs(1), framed.next()).await {
            Ok(Some(Ok(Frame::Pong))) => break,
            Ok(Some(Ok(Frame::Data(_)))) => continue,
            _ if tokio::time::Instant::now() >= deadline => panic!("no Pong received"),
            _ => continue,
        }
    }
}

#[tokio::test]
async fn heartbeat_updates_metadata_through_proxy() {
    skip_if_no_socat!();
    let env = TestEnv::new();

    let (id, mut framed) = create_session(&env.proxy_path, "hbmeta").await;
    wait_for_shell(&mut framed).await;
    drain_data(&mut framed, Duration::from_millis(500)).await;

    // Send Ping and wait for Pong
    framed.send(Frame::Ping).await.unwrap();
    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    loop {
        match timeout(Duration::from_secs(1), framed.next()).await {
            Ok(Some(Ok(Frame::Pong))) => break,
            Ok(Some(Ok(Frame::Data(_)))) => continue,
            _ if tokio::time::Instant::now() >= deadline => panic!("no Pong received"),
            _ => continue,
        }
    }

    // Check heartbeat metadata via ListSessions (use a separate connection)
    let resp = control_request(&env.proxy_path, Frame::ListSessions).await;
    let hb1 = match &resp {
        Frame::SessionInfo { sessions } => {
            let s = sessions.iter().find(|s| s.id == id).expect("session not found");
            assert!(s.last_heartbeat > 0, "heartbeat should be > 0");
            s.last_heartbeat
        }
        other => panic!("expected SessionInfo, got {other:?}"),
    };

    // Wait a moment, send another Ping
    tokio::time::sleep(Duration::from_secs(1)).await;
    framed.send(Frame::Ping).await.unwrap();
    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    loop {
        match timeout(Duration::from_secs(1), framed.next()).await {
            Ok(Some(Ok(Frame::Pong))) => break,
            Ok(Some(Ok(Frame::Data(_)))) => continue,
            _ if tokio::time::Instant::now() >= deadline => panic!("no Pong received"),
            _ => continue,
        }
    }

    // Heartbeat should have increased
    let resp = control_request(&env.proxy_path, Frame::ListSessions).await;
    match &resp {
        Frame::SessionInfo { sessions } => {
            let s = sessions.iter().find(|s| s.id == id).expect("session not found");
            assert!(
                s.last_heartbeat >= hb1,
                "heartbeat should increase: {} vs {}",
                s.last_heartbeat,
                hb1
            );
        }
        other => panic!("expected SessionInfo, got {other:?}"),
    }
}

// ===========================================================================
// Group 7: Error Handling
// ===========================================================================

#[tokio::test]
async fn attach_nonexistent_session_through_proxy() {
    skip_if_no_socat!();
    let env = TestEnv::new();

    let resp =
        control_request(&env.proxy_path, Frame::Attach { session: "ghost".to_string() }).await;
    assert!(
        matches!(resp, Frame::Error { .. }),
        "expected Error for nonexistent session, got {resp:?}"
    );
}

#[tokio::test]
async fn no_handshake_rejected_through_proxy() {
    skip_if_no_socat!();
    let env = TestEnv::new();

    // Connect and send ListSessions without Hello
    let stream = UnixStream::connect(&env.proxy_path).await.unwrap();
    let mut framed = Framed::new(stream, FrameCodec);
    framed.send(Frame::ListSessions).await.unwrap();
    let resp = timeout(Duration::from_secs(5), framed.next())
        .await
        .expect("timed out")
        .expect("stream ended")
        .expect("decode error");
    match resp {
        Frame::Error { message } => {
            assert!(message.contains("Hello"), "error should mention Hello, got: {message}");
        }
        other => panic!("expected Error, got {other:?}"),
    }
}

#[tokio::test]
async fn kill_server_through_proxy() {
    skip_if_no_socat!();
    // Use a fresh env so we don't conflict with other tests
    let tmp = tempfile::tempdir().unwrap();
    let ctl_path = tmp.path().join("ctl.sock");
    let proxy_path = tmp.path().join("proxy.sock");

    let mut server_child = Command::new(gritty_bin())
        .args(["server", "--foreground", "--ctl-socket"])
        .arg(&ctl_path)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("failed to start server");
    wait_for_socket(&ctl_path, 5);

    let _socat = start_socat_proxy(&proxy_path, &ctl_path);
    wait_for_socket(&proxy_path, 5);
    std::thread::sleep(Duration::from_millis(200));

    // Create a session first
    let (_, _) = create_session(&proxy_path, "doomed").await;
    tokio::time::sleep(Duration::from_millis(200)).await;

    // Kill server via proxy
    let resp = control_request(&proxy_path, Frame::KillServer).await;
    assert_eq!(resp, Frame::Ok);

    // Wait for the server process to exit (reap it to avoid zombie)
    let deadline = std::time::Instant::now() + Duration::from_secs(5);
    loop {
        match server_child.try_wait() {
            Ok(Some(_status)) => break, // Process exited
            Ok(None) => {
                // Still running
                if std::time::Instant::now() > deadline {
                    let _ = server_child.kill();
                    let _ = server_child.wait();
                    panic!("server process did not exit after KillServer");
                }
                std::thread::sleep(Duration::from_millis(100));
            }
            Err(e) => panic!("error waiting for server process: {e}"),
        }
    }
}

#[tokio::test]
async fn concurrent_control_requests_through_proxy() {
    skip_if_no_socat!();
    let env = TestEnv::new();

    // Create a session first so ListSessions has something to return
    let (_, _) = create_session(&env.proxy_path, "concurrent-target").await;
    tokio::time::sleep(Duration::from_millis(200)).await;

    // Spawn 5 concurrent tasks, each does ListSessions
    let mut handles = Vec::new();
    for _ in 0..5 {
        let proxy = env.proxy_path.clone();
        handles.push(tokio::spawn(async move {
            let resp = control_request(&proxy, Frame::ListSessions).await;
            match resp {
                Frame::SessionInfo { sessions } => sessions.len(),
                other => panic!("expected SessionInfo, got {other:?}"),
            }
        }));
    }

    let mut results = Vec::new();
    for handle in handles {
        results.push(handle.await.unwrap());
    }

    // All should see at least 1 session
    for (i, count) in results.iter().enumerate() {
        assert!(*count >= 1, "task {i} should see at least 1 session, got {count}");
    }

    // Results should be consistent
    let first = results[0];
    for (i, count) in results.iter().enumerate() {
        assert_eq!(*count, first, "task {i} got {count}, but task 0 got {first}");
    }
}
