use bytes::Bytes;
use futures_util::{SinkExt, StreamExt};
use gritty::protocol::{ErrorCode, Frame, FrameCodec, PROTOCOL_VERSION};
use std::time::Duration;
use tokio::net::UnixStream;
use tokio::time::timeout;
use tokio_util::codec::Framed;

/// Create an isolated temp directory with a control socket path inside it.
/// The TempDir must be kept alive for the duration of the test.
fn test_ctl() -> (tempfile::TempDir, std::path::PathBuf) {
    let tmp = tempfile::tempdir().unwrap();
    let ctl_path = tmp.path().join("ctl.sock");
    (tmp, ctl_path)
}

/// Poll until the daemon socket exists and is connectable.
async fn wait_for_daemon(ctl_path: &std::path::Path) {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    loop {
        if ctl_path.exists() && UnixStream::connect(ctl_path).await.is_ok() {
            return;
        }
        if tokio::time::Instant::now() >= deadline {
            panic!("daemon did not start within 5s");
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

/// Perform Hello handshake on a framed connection.
async fn do_handshake(framed: &mut Framed<UnixStream, FrameCodec>) {
    do_handshake_as(framed, 1).await;
}

async fn do_handshake_as(framed: &mut Framed<UnixStream, FrameCodec>, device_id: u64) {
    framed
        .send(Frame::Hello { version: PROTOCOL_VERSION, capabilities: 0, device_id })
        .await
        .unwrap();
    let resp = timeout(Duration::from_secs(3), framed.next())
        .await
        .expect("timed out")
        .expect("stream ended")
        .expect("decode error");
    assert!(matches!(resp, Frame::HelloAck { .. }), "expected HelloAck, got {resp:?}");
}

/// Helper: send a control frame and get the response.
async fn control_request(ctl_path: &std::path::Path, frame: Frame) -> Frame {
    control_request_as(ctl_path, frame, 1).await
}

async fn control_request_as(ctl_path: &std::path::Path, frame: Frame, device_id: u64) -> Frame {
    let stream = UnixStream::connect(ctl_path).await.unwrap();
    let mut framed = Framed::new(stream, FrameCodec);
    do_handshake_as(&mut framed, device_id).await;
    framed.send(frame).await.unwrap();
    timeout(Duration::from_secs(3), framed.next())
        .await
        .expect("timed out")
        .expect("stream ended")
        .expect("decode error")
}

/// Drain all available Data frames within a timeout.
async fn drain_data(framed: &mut Framed<UnixStream, FrameCodec>, wait: Duration) {
    while let Ok(Some(Ok(Frame::Data(_)))) = timeout(wait, framed.next()).await {}
}

/// Create a session via NewSession, return the session id.
async fn create_session(ctl_path: &std::path::Path, name: &str) -> String {
    let resp = control_request(
        ctl_path,
        Frame::NewSession {
            name: name.to_string(),
            command: String::new(),
            cwd: String::new(),
            cols: 0,
            rows: 0,
            client_name: String::new(),
        },
    )
    .await;
    match resp {
        Frame::SessionCreated { id } => id.to_string(),
        other => panic!("expected SessionCreated, got {other:?}"),
    }
}

/// Attach to a session via daemon, return the framed connection.
async fn attach_session(
    ctl_path: &std::path::Path,
    session: &str,
) -> Framed<UnixStream, FrameCodec> {
    let stream = UnixStream::connect(ctl_path).await.unwrap();
    let mut framed = Framed::new(stream, FrameCodec);
    do_handshake(&mut framed).await;
    framed
        .send(Frame::Attach {
            session: session.to_string(),
            client_name: String::new(),
            force: true,
            no_replay: false,
            cols: 0,
            rows: 0,
            attach_token: 0,
            rendered_offset: 0,
            line_dirty: false,
        })
        .await
        .unwrap();
    let resp = timeout(Duration::from_secs(3), framed.next())
        .await
        .expect("timed out")
        .expect("stream ended")
        .expect("decode error");
    assert!(matches!(resp, Frame::AttachAck { .. }), "expected AttachAck for attach, got {resp:?}");

    // Send resize and wait for shell output
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

    framed
}

/// Kill a session by id or name.
async fn kill_cleanup(ctl_path: &std::path::Path, session: &str) {
    let _ = control_request(ctl_path, Frame::KillSession { session: session.to_string() }).await;
}

#[tokio::test]
async fn daemon_hello_ack_carries_server_version_on_mismatch() {
    // New semantics: the daemon never hard-rejects the handshake on version
    // mismatch. It replies with HelloAck carrying its own version so the
    // client can see what it's talking to, and per-frame gating below keeps
    // session operations safe.
    let (_tmp, ctl_path) = test_ctl();

    let ctl = ctl_path.clone();
    let _daemon = tokio::spawn(async move { gritty::daemon::run(&ctl, None).await });
    wait_for_daemon(&ctl_path).await;

    let stream = UnixStream::connect(&ctl_path).await.unwrap();
    let mut framed = Framed::new(stream, FrameCodec);
    framed
        .send(Frame::Hello { version: PROTOCOL_VERSION + 1, capabilities: 0, device_id: 1 })
        .await
        .unwrap();
    let resp = timeout(Duration::from_secs(3), framed.next())
        .await
        .expect("timed out")
        .expect("stream ended")
        .expect("decode error");
    match resp {
        Frame::HelloAck { version, .. } => {
            assert_eq!(version, PROTOCOL_VERSION, "server should advertise its own version");
        }
        other => panic!("expected HelloAck, got {other:?}"),
    }
}

#[tokio::test]
async fn daemon_accepts_killserver_under_version_mismatch() {
    // Recovery path for users who upgraded one side: a client with a
    // mismatched PROTOCOL_VERSION must still be able to kill the daemon so
    // they can restart it with the matching binary.
    let (_tmp, ctl_path) = test_ctl();

    let ctl = ctl_path.clone();
    let daemon = tokio::spawn(async move { gritty::daemon::run(&ctl, None).await });
    wait_for_daemon(&ctl_path).await;

    let stream = UnixStream::connect(&ctl_path).await.unwrap();
    let mut framed = Framed::new(stream, FrameCodec);
    // Pretend to be a newer client.
    framed
        .send(Frame::Hello { version: PROTOCOL_VERSION + 7, capabilities: 0, device_id: 1 })
        .await
        .unwrap();
    let resp = timeout(Duration::from_secs(3), framed.next())
        .await
        .expect("timed out")
        .expect("stream ended")
        .expect("decode error");
    assert!(matches!(resp, Frame::HelloAck { .. }), "expected HelloAck, got {resp:?}");

    framed.send(Frame::KillServer).await.unwrap();
    let resp = timeout(Duration::from_secs(3), framed.next())
        .await
        .expect("timed out")
        .expect("stream ended")
        .expect("decode error");
    assert_eq!(resp, Frame::Ok, "KillServer should succeed under version mismatch");

    // Daemon should have exited.
    let _ = timeout(Duration::from_secs(3), daemon).await;
    assert!(!ctl_path.exists(), "ctl socket should be cleaned up after kill-server");
}

#[tokio::test]
async fn daemon_rejects_non_killserver_frames_under_version_mismatch() {
    // Under a version mismatch the daemon still refuses anything that could
    // touch session state: new-session, attach, list, etc. all return
    // VersionMismatch so the user gets an actionable error instead of a
    // half-working control plane.
    let (_tmp, ctl_path) = test_ctl();

    let ctl = ctl_path.clone();
    let _daemon = tokio::spawn(async move { gritty::daemon::run(&ctl, None).await });
    wait_for_daemon(&ctl_path).await;

    let stream = UnixStream::connect(&ctl_path).await.unwrap();
    let mut framed = Framed::new(stream, FrameCodec);
    framed
        .send(Frame::Hello { version: PROTOCOL_VERSION + 3, capabilities: 0, device_id: 1 })
        .await
        .unwrap();
    let resp = timeout(Duration::from_secs(3), framed.next())
        .await
        .expect("timed out")
        .expect("stream ended")
        .expect("decode error");
    assert!(matches!(resp, Frame::HelloAck { .. }));

    framed.send(Frame::ListSessions).await.unwrap();
    let resp = timeout(Duration::from_secs(3), framed.next())
        .await
        .expect("timed out")
        .expect("stream ended")
        .expect("decode error");
    match resp {
        Frame::Error { code: ErrorCode::VersionMismatch, message } => {
            assert!(
                message.contains("version"),
                "expected message mentioning version, got: {message}"
            );
        }
        other => panic!("expected VersionMismatch error, got {other:?}"),
    }
}

#[tokio::test]
async fn daemon_creates_and_lists_sessions() {
    let (_tmp, ctl_path) = test_ctl();

    let ctl = ctl_path.clone();
    let _daemon = tokio::spawn(async move { gritty::daemon::run(&ctl, None).await });
    wait_for_daemon(&ctl_path).await;

    let id = create_session(&ctl_path, "mytest").await;

    // List sessions — should see one
    let resp = control_request(&ctl_path, Frame::ListSessions).await;
    match &resp {
        Frame::SessionInfo { sessions } => {
            assert_eq!(sessions.len(), 1);
            assert_eq!(sessions[0].id.to_string(), id);
            assert_eq!(sessions[0].name, "mytest");
        }
        other => panic!("expected SessionInfo, got {other:?}"),
    }

    kill_cleanup(&ctl_path, &id).await;
}

#[tokio::test]
async fn daemon_rejects_duplicate_name() {
    let (_tmp, ctl_path) = test_ctl();

    let ctl = ctl_path.clone();
    let _daemon = tokio::spawn(async move { gritty::daemon::run(&ctl, None).await });
    wait_for_daemon(&ctl_path).await;

    let id = create_session(&ctl_path, "dupname").await;

    // Try to create session with same name again
    let resp = control_request(
        &ctl_path,
        Frame::NewSession {
            name: "dupname".to_string(),
            command: String::new(),
            cwd: String::new(),
            cols: 0,
            rows: 0,
            client_name: String::new(),
        },
    )
    .await;
    assert!(matches!(resp, Frame::Error { .. }), "expected Error for duplicate name, got {resp:?}");

    kill_cleanup(&ctl_path, &id).await;
}

#[tokio::test]
async fn daemon_rejects_name_with_tab() {
    let (_tmp, ctl_path) = test_ctl();

    let ctl = ctl_path.clone();
    let _daemon = tokio::spawn(async move { gritty::daemon::run(&ctl, None).await });
    wait_for_daemon(&ctl_path).await;

    let resp = control_request(
        &ctl_path,
        Frame::NewSession {
            name: "bad\tname".to_string(),
            command: String::new(),
            cwd: String::new(),
            cols: 0,
            rows: 0,
            client_name: String::new(),
        },
    )
    .await;
    assert!(matches!(resp, Frame::Error { .. }), "expected Error for name with tab, got {resp:?}");

    control_request(&ctl_path, Frame::KillServer).await;
}

#[tokio::test]
async fn daemon_rejects_name_with_newline() {
    let (_tmp, ctl_path) = test_ctl();

    let ctl = ctl_path.clone();
    let _daemon = tokio::spawn(async move { gritty::daemon::run(&ctl, None).await });
    wait_for_daemon(&ctl_path).await;

    let resp = control_request(
        &ctl_path,
        Frame::NewSession {
            name: "bad\nname".to_string(),
            command: String::new(),
            cwd: String::new(),
            cols: 0,
            rows: 0,
            client_name: String::new(),
        },
    )
    .await;
    assert!(
        matches!(resp, Frame::Error { .. }),
        "expected Error for name with newline, got {resp:?}"
    );

    control_request(&ctl_path, Frame::KillServer).await;
}

#[tokio::test]
async fn daemon_allows_multiple_unnamed_sessions() {
    let (_tmp, ctl_path) = test_ctl();

    let ctl = ctl_path.clone();
    let _daemon = tokio::spawn(async move { gritty::daemon::run(&ctl, None).await });
    wait_for_daemon(&ctl_path).await;

    // Create two unnamed sessions (empty name)
    let id1 = create_session(&ctl_path, "").await;
    let id2 = create_session(&ctl_path, "").await;
    assert_ne!(id1, id2);

    let resp = control_request(&ctl_path, Frame::ListSessions).await;
    match &resp {
        Frame::SessionInfo { sessions } => {
            assert_eq!(sessions.len(), 2, "expected 2 sessions");
        }
        other => panic!("expected SessionInfo, got {other:?}"),
    }

    kill_cleanup(&ctl_path, &id1).await;
    kill_cleanup(&ctl_path, &id2).await;
}

#[tokio::test]
async fn daemon_kills_session() {
    let (_tmp, ctl_path) = test_ctl();

    let ctl = ctl_path.clone();
    let _daemon = tokio::spawn(async move { gritty::daemon::run(&ctl, None).await });
    wait_for_daemon(&ctl_path).await;

    let id = create_session(&ctl_path, "killme").await;

    let resp = control_request(&ctl_path, Frame::KillSession { session: id.clone() }).await;
    assert_eq!(resp, Frame::Ok);

    // List should be empty
    tokio::time::sleep(Duration::from_millis(200)).await;
    let resp = control_request(&ctl_path, Frame::ListSessions).await;
    match &resp {
        Frame::SessionInfo { sessions } => {
            assert!(sessions.is_empty(), "expected no sessions after kill");
        }
        other => panic!("expected SessionInfo, got {other:?}"),
    }
}

#[tokio::test]
async fn daemon_kills_session_by_name() {
    let (_tmp, ctl_path) = test_ctl();

    let ctl = ctl_path.clone();
    let _daemon = tokio::spawn(async move { gritty::daemon::run(&ctl, None).await });
    wait_for_daemon(&ctl_path).await;

    let _id = create_session(&ctl_path, "named-kill").await;

    // Kill by name
    let resp =
        control_request(&ctl_path, Frame::KillSession { session: "named-kill".to_string() }).await;
    assert_eq!(resp, Frame::Ok);

    tokio::time::sleep(Duration::from_millis(200)).await;
    let resp = control_request(&ctl_path, Frame::ListSessions).await;
    match &resp {
        Frame::SessionInfo { sessions } => {
            assert!(sessions.is_empty(), "expected no sessions after kill by name");
        }
        other => panic!("expected SessionInfo, got {other:?}"),
    }
}

#[tokio::test]
async fn list_sessions_reports_last_activity() {
    let (_tmp, ctl_path) = test_ctl();

    let ctl = ctl_path.clone();
    let _daemon = tokio::spawn(async move { gritty::daemon::run(&ctl, None).await });
    wait_for_daemon(&ctl_path).await;

    let id = create_session(&ctl_path, "activity-list").await;

    // A fresh session reports last_activity (initialized to its creation
    // time) -- the field survives the SessionEntry encode/decode round trip.
    // Poll: the session task publishes its metadata asynchronously, and until
    // then the daemon reports placeholder zeros.
    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    loop {
        let resp = control_request(&ctl_path, Frame::ListSessions).await;
        let sessions = match &resp {
            Frame::SessionInfo { sessions } => sessions,
            other => panic!("expected SessionInfo, got {other:?}"),
        };
        assert_eq!(sessions.len(), 1);
        if sessions[0].last_activity > 0 {
            assert!(
                sessions[0].last_activity >= sessions[0].created_at,
                "last_activity should not predate creation"
            );
            break;
        }
        if tokio::time::Instant::now() >= deadline {
            panic!("last_activity never reported, got {:?}", sessions[0]);
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }

    kill_cleanup(&ctl_path, &id).await;
}

#[tokio::test]
async fn daemon_kills_server() {
    let (_tmp, ctl_path) = test_ctl();

    let ctl = ctl_path.clone();
    let daemon = tokio::spawn(async move { gritty::daemon::run(&ctl, None).await });
    wait_for_daemon(&ctl_path).await;

    let _id = create_session(&ctl_path, "doomed").await;

    let resp = control_request(&ctl_path, Frame::KillServer).await;
    assert_eq!(resp, Frame::Ok);

    let result = timeout(Duration::from_secs(3), daemon).await;
    assert!(result.is_ok(), "daemon should exit after kill-server");

    assert!(!ctl_path.exists(), "control socket should be removed");
}

#[tokio::test]
async fn create_after_kill_same_name() {
    let (_tmp, ctl_path) = test_ctl();

    let ctl = ctl_path.clone();
    let _daemon = tokio::spawn(async move { gritty::daemon::run(&ctl, None).await });
    wait_for_daemon(&ctl_path).await;

    let id1 = create_session(&ctl_path, "reuse").await;

    // Kill it
    let resp = control_request(&ctl_path, Frame::KillSession { session: id1.clone() }).await;
    assert_eq!(resp, Frame::Ok);
    tokio::time::sleep(Duration::from_millis(200)).await;

    // Create again with same name
    let id2 = create_session(&ctl_path, "reuse").await;
    assert_ne!(id1, id2, "should get a new id");

    // Verify session appears with valid metadata
    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    loop {
        tokio::time::sleep(Duration::from_millis(200)).await;
        let resp = control_request(&ctl_path, Frame::ListSessions).await;
        match &resp {
            Frame::SessionInfo { sessions } if sessions.len() == 1 && sessions[0].shell_pid > 0 => {
                assert_eq!(sessions[0].id.to_string(), id2);
                assert_eq!(sessions[0].name, "reuse");
                break;
            }
            _ if tokio::time::Instant::now() < deadline => continue,
            other => panic!("recreated session should appear with valid PID, got {other:?}"),
        }
    }

    kill_cleanup(&ctl_path, &id2).await;
}

#[tokio::test]
async fn multiple_concurrent_sessions() {
    let (_tmp, ctl_path) = test_ctl();

    let ctl = ctl_path.clone();
    let _daemon = tokio::spawn(async move { gritty::daemon::run(&ctl, None).await });
    wait_for_daemon(&ctl_path).await;

    let id1 = create_session(&ctl_path, "sess-a").await;
    let id2 = create_session(&ctl_path, "sess-b").await;

    let resp = control_request(&ctl_path, Frame::ListSessions).await;
    match &resp {
        Frame::SessionInfo { sessions } => {
            assert_eq!(sessions.len(), 2, "expected 2 sessions");
            let ids: Vec<String> = sessions.iter().map(|s| s.id.to_string()).collect();
            assert!(ids.contains(&id1), "should contain session 1");
            assert!(ids.contains(&id2), "should contain session 2");
        }
        other => panic!("expected SessionInfo, got {other:?}"),
    }

    // Both sessions should be alive — verify via metadata (PID > 0)
    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    loop {
        tokio::time::sleep(Duration::from_millis(200)).await;
        let resp = control_request(&ctl_path, Frame::ListSessions).await;
        match &resp {
            Frame::SessionInfo { sessions }
                if sessions.len() == 2 && sessions.iter().all(|s| s.shell_pid > 0) =>
            {
                break;
            }
            _ if tokio::time::Instant::now() < deadline => continue,
            other => panic!("expected 2 sessions with valid PIDs, got {other:?}"),
        }
    }

    kill_cleanup(&ctl_path, &id1).await;
    kill_cleanup(&ctl_path, &id2).await;
}

#[tokio::test]
async fn daemon_unexpected_frame() {
    let (_tmp, ctl_path) = test_ctl();

    let ctl = ctl_path.clone();
    let _daemon = tokio::spawn(async move { gritty::daemon::run(&ctl, None).await });
    wait_for_daemon(&ctl_path).await;

    // Send a Data frame (makes no sense on control socket)
    let resp = control_request(&ctl_path, Frame::Data(Bytes::from("hello"))).await;
    assert!(
        matches!(resp, Frame::Error { .. }),
        "expected Error for unexpected frame, got {resp:?}"
    );

    // Send a Resize frame
    let resp = control_request(&ctl_path, Frame::Resize { cols: 80, rows: 24 }).await;
    assert!(
        matches!(resp, Frame::Error { .. }),
        "expected Error for Resize on control socket, got {resp:?}"
    );

    // Daemon should still be alive
    let resp = control_request(&ctl_path, Frame::ListSessions).await;
    match &resp {
        Frame::SessionInfo { sessions } => {
            assert!(sessions.is_empty());
        }
        other => panic!("expected SessionInfo, got {other:?}"),
    }
}

#[tokio::test]
async fn kill_server_notifies_attached_client() {
    // A client attached when the daemon is killed should receive
    // Frame::ServerShutdown so it can exit cleanly instead of spinning in
    // its reconnect loop. Without the explicit goodbye, a remote client
    // (tunnel still alive) retries for ~1-2 minutes until the supervisor
    // finally respawns the remote daemon with a new server_id.
    let (_tmp, ctl_path) = test_ctl();

    let ctl = ctl_path.clone();
    let daemon = tokio::spawn(async move { gritty::daemon::run(&ctl, None).await });
    wait_for_daemon(&ctl_path).await;

    create_session(&ctl_path, "doomed").await;
    let mut attached = attach_session(&ctl_path, "doomed").await;

    let resp = control_request(&ctl_path, Frame::KillServer).await;
    assert_eq!(resp, Frame::Ok);

    // The attached client must see ServerShutdown (possibly after some
    // remaining Data frames from the PTY) before the stream closes.
    let deadline = tokio::time::Instant::now() + Duration::from_secs(3);
    let mut got_shutdown = false;
    loop {
        match timeout(Duration::from_millis(500), attached.next()).await {
            Ok(Some(Ok(Frame::ServerShutdown))) => {
                got_shutdown = true;
                break;
            }
            Ok(Some(Ok(Frame::Data(_)))) => continue,
            Ok(Some(Ok(other))) => panic!("expected ServerShutdown, got {other:?}"),
            Ok(Some(Err(e))) => panic!("decode error before ServerShutdown: {e}"),
            Ok(None) => break, // EOF without ServerShutdown -- the old bug
            Err(_) if tokio::time::Instant::now() < deadline => continue,
            Err(_) => break,
        }
    }
    assert!(got_shutdown, "attached client should receive ServerShutdown on kill-server");

    let result = timeout(Duration::from_secs(3), daemon).await;
    assert!(result.is_ok(), "daemon should exit after kill-server");
    assert!(!ctl_path.exists(), "control socket should be removed");
}

#[tokio::test]
async fn kill_server_no_sessions() {
    let (_tmp, ctl_path) = test_ctl();

    let ctl = ctl_path.clone();
    let daemon = tokio::spawn(async move { gritty::daemon::run(&ctl, None).await });
    wait_for_daemon(&ctl_path).await;

    let resp = control_request(&ctl_path, Frame::KillServer).await;
    assert_eq!(resp, Frame::Ok);

    let result = timeout(Duration::from_secs(3), daemon).await;
    assert!(result.is_ok(), "daemon should exit after kill-server with no sessions");

    assert!(!ctl_path.exists(), "control socket should be removed");
}

#[tokio::test]
async fn daemon_writes_and_removes_info_file() {
    // The `.info` sidecar records protocol version + git hash so `gritty
    // doctor` can flag a daemon running stale code after a rebuild. It must
    // exist while the daemon runs and be cleaned up on shutdown.
    let (_tmp, ctl_path) = test_ctl();
    let info_path = gritty::runinfo::daemon_info_path(&ctl_path);

    let ctl = ctl_path.clone();
    let daemon = tokio::spawn(async move { gritty::daemon::run(&ctl, None).await });
    wait_for_daemon(&ctl_path).await;

    let info = gritty::runinfo::RunInfo::read(&info_path).expect("daemon.info should exist");
    assert_eq!(info.protocol, PROTOCOL_VERSION);
    assert_eq!(info.git_hash, gritty::runinfo::GIT_HASH);
    assert!(info.pid > 0);
    assert_eq!(info.staleness_vs_current(), None, "running daemon should not be stale");

    control_request(&ctl_path, Frame::KillServer).await;
    timeout(Duration::from_secs(3), daemon).await.expect("daemon should exit").unwrap().unwrap();
    assert!(!info_path.exists(), "daemon.info should be removed on shutdown");
}

#[tokio::test]
async fn kill_nonexistent_session() {
    let (_tmp, ctl_path) = test_ctl();

    let ctl = ctl_path.clone();
    let _daemon = tokio::spawn(async move { gritty::daemon::run(&ctl, None).await });
    wait_for_daemon(&ctl_path).await;

    let resp = control_request(&ctl_path, Frame::KillSession { session: "999".to_string() }).await;
    assert!(
        matches!(resp, Frame::Error { .. }),
        "expected Error for nonexistent session, got {resp:?}"
    );
}

#[tokio::test]
async fn session_natural_exit_reaps_from_list() {
    let (_tmp, ctl_path) = test_ctl();

    let ctl = ctl_path.clone();
    let _daemon = tokio::spawn(async move { gritty::daemon::run(&ctl, None).await });
    wait_for_daemon(&ctl_path).await;

    let _id = create_session(&ctl_path, "reapme").await;

    // Wait for shell to start (PID > 0)
    let shell_pid;
    let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
    loop {
        tokio::time::sleep(Duration::from_millis(200)).await;
        let resp = control_request(&ctl_path, Frame::ListSessions).await;
        match &resp {
            Frame::SessionInfo { sessions } if sessions.len() == 1 && sessions[0].shell_pid > 0 => {
                shell_pid = sessions[0].shell_pid;
                break;
            }
            _ if tokio::time::Instant::now() < deadline => continue,
            other => panic!("shell did not start within 10s, got {other:?}"),
        }
    }

    // Kill the shell externally
    unsafe {
        libc::kill(shell_pid as i32, libc::SIGKILL);
    }

    // Poll list until sessions is empty
    let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
    loop {
        tokio::time::sleep(Duration::from_millis(500)).await;
        let resp = control_request(&ctl_path, Frame::ListSessions).await;
        match &resp {
            Frame::SessionInfo { sessions } if sessions.is_empty() => break,
            Frame::SessionInfo { .. } if tokio::time::Instant::now() < deadline => continue,
            Frame::SessionInfo { sessions } => {
                panic!("expected no sessions after natural exit, got {sessions:?}");
            }
            other => panic!("expected SessionInfo, got {other:?}"),
        }
    }
}

#[tokio::test]
async fn list_before_session_ready() {
    let (_tmp, ctl_path) = test_ctl();

    let ctl = ctl_path.clone();
    let _daemon = tokio::spawn(async move { gritty::daemon::run(&ctl, None).await });
    wait_for_daemon(&ctl_path).await;

    // Create session via NewSession (don't wait the usual 200ms from helper)
    let resp = control_request(
        &ctl_path,
        Frame::NewSession {
            name: "early".to_string(),
            command: String::new(),
            cwd: String::new(),
            cols: 0,
            rows: 0,
            client_name: String::new(),
        },
    )
    .await;
    let id = match resp {
        Frame::SessionCreated { id } => id.to_string(),
        other => panic!("expected SessionCreated, got {other:?}"),
    };

    // List immediately
    let resp = control_request(&ctl_path, Frame::ListSessions).await;
    match &resp {
        Frame::SessionInfo { sessions } => {
            assert_eq!(sessions.len(), 1, "session should appear in list immediately");
            assert_eq!(sessions[0].id.to_string(), id);
        }
        other => panic!("expected SessionInfo, got {other:?}"),
    }

    tokio::time::sleep(Duration::from_millis(300)).await;
    kill_cleanup(&ctl_path, &id).await;
}

#[tokio::test]
async fn kill_session_while_client_connected() {
    let (_tmp, ctl_path) = test_ctl();

    let ctl = ctl_path.clone();
    let _daemon = tokio::spawn(async move { gritty::daemon::run(&ctl, None).await });
    wait_for_daemon(&ctl_path).await;

    let id = create_session(&ctl_path, "kill-conn").await;

    // Attach a client
    let mut framed = attach_session(&ctl_path, &id).await;
    drain_data(&mut framed, Duration::from_millis(500)).await;

    // Kill the session via daemon while client is connected
    let resp = control_request(&ctl_path, Frame::KillSession { session: id.clone() }).await;
    assert_eq!(resp, Frame::Ok);

    // Client should see the stream end (drain any in-flight data/exit frames)
    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    loop {
        match timeout(Duration::from_secs(1), framed.next()).await {
            Ok(None) | Ok(Some(Err(_))) => break,
            Ok(Some(Ok(Frame::Data(_) | Frame::Exit { .. }))) => continue,
            Err(_) if tokio::time::Instant::now() >= deadline => {
                panic!("stream did not end within 5s after kill")
            }
            Err(_) => continue,
            Ok(Some(Ok(other))) => panic!("unexpected frame after session kill: {other:?}"),
        }
    }
}

#[tokio::test]
async fn session_metadata_has_pty_and_pid() {
    let (_tmp, ctl_path) = test_ctl();

    let ctl = ctl_path.clone();
    let _daemon = tokio::spawn(async move { gritty::daemon::run(&ctl, None).await });
    wait_for_daemon(&ctl_path).await;

    let id = create_session(&ctl_path, "metacheck").await;

    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    loop {
        tokio::time::sleep(Duration::from_millis(200)).await;
        let resp = control_request(&ctl_path, Frame::ListSessions).await;
        match &resp {
            Frame::SessionInfo { sessions } if sessions.len() == 1 && sessions[0].shell_pid > 0 => {
                let s = &sessions[0];
                assert!(
                    s.pty_path.starts_with("/dev/pts/") || s.pty_path.starts_with("/dev/tty"),
                    "pty_path should be a real device, got: {}",
                    s.pty_path
                );
                assert!(s.created_at > 0, "created_at should be set");
                break;
            }
            _ if tokio::time::Instant::now() < deadline => continue,
            other => panic!("expected session with valid metadata, got {other:?}"),
        }
    }

    kill_cleanup(&ctl_path, &id).await;
}

#[tokio::test]
async fn attach_to_session() {
    let (_tmp, ctl_path) = test_ctl();

    let ctl = ctl_path.clone();
    let _daemon = tokio::spawn(async move { gritty::daemon::run(&ctl, None).await });
    wait_for_daemon(&ctl_path).await;

    let id = create_session(&ctl_path, "attachme").await;

    // Attach via the daemon
    let mut framed = attach_session(&ctl_path, &id).await;
    drain_data(&mut framed, Duration::from_millis(500)).await;

    framed.send(Frame::Data(Bytes::from("echo ATTACH_OK\n"))).await.unwrap();
    let mut output = Vec::new();
    while let Ok(Some(Ok(Frame::Data(data)))) = timeout(Duration::from_secs(2), framed.next()).await
    {
        output.extend_from_slice(&data);
    }
    let output_str = String::from_utf8_lossy(&output);
    assert!(
        output_str.contains("ATTACH_OK"),
        "should be able to interact after attach, got: {output_str}"
    );

    kill_cleanup(&ctl_path, &id).await;
}

#[tokio::test]
async fn attach_by_name() {
    let (_tmp, ctl_path) = test_ctl();

    let ctl = ctl_path.clone();
    let _daemon = tokio::spawn(async move { gritty::daemon::run(&ctl, None).await });
    wait_for_daemon(&ctl_path).await;

    let id = create_session(&ctl_path, "namedattach").await;

    // Attach by name
    let mut framed = attach_session(&ctl_path, "namedattach").await;
    drain_data(&mut framed, Duration::from_millis(500)).await;

    framed.send(Frame::Data(Bytes::from("echo NAME_ATTACH_OK\n"))).await.unwrap();
    let mut output = Vec::new();
    while let Ok(Some(Ok(Frame::Data(data)))) = timeout(Duration::from_secs(2), framed.next()).await
    {
        output.extend_from_slice(&data);
    }
    let output_str = String::from_utf8_lossy(&output);
    assert!(
        output_str.contains("NAME_ATTACH_OK"),
        "should be able to attach by name, got: {output_str}"
    );

    kill_cleanup(&ctl_path, &id).await;
}

#[tokio::test]
async fn attach_nonexistent_returns_error() {
    let (_tmp, ctl_path) = test_ctl();

    let ctl = ctl_path.clone();
    let _daemon = tokio::spawn(async move { gritty::daemon::run(&ctl, None).await });
    wait_for_daemon(&ctl_path).await;

    let resp = control_request(
        &ctl_path,
        Frame::Attach {
            session: "nonexistent".to_string(),
            client_name: String::new(),
            force: false,
            no_replay: false,
            cols: 0,
            rows: 0,
            attach_token: 0,
            rendered_offset: 0,
            line_dirty: false,
        },
    )
    .await;
    assert!(
        matches!(resp, Frame::Error { .. }),
        "expected Error for nonexistent attach, got {resp:?}"
    );
}

/// Regression: a non-forced Attach that races into the session-birth window
/// -- after the creator was handed off but before the session task marks the
/// session attached -- must be rejected with AlreadyAttached, not silently
/// take the session over from the creator. The admission guard used to read
/// an `attached` flag that only the session task set (after its Env wait), so
/// the guard was a no-op during creation and a second `gritty connect` could
/// steal a brand-new session.
#[tokio::test]
async fn second_attach_during_birth_window_is_rejected() {
    let (_tmp, ctl_path) = test_ctl();

    let ctl = ctl_path.clone();
    let _daemon = tokio::spawn(async move { gritty::daemon::run(&ctl, None).await });
    wait_for_daemon(&ctl_path).await;

    // Creator A: NewSession, consume SessionCreated + AttachAck, then hold the
    // connection open WITHOUT sending Env. The session task is now parked in
    // its Env wait and has not marked the session attached itself -- this is
    // exactly the birth window the bug exploited.
    let stream_a = UnixStream::connect(&ctl_path).await.unwrap();
    let mut a = Framed::new(stream_a, FrameCodec);
    do_handshake(&mut a).await;
    a.send(Frame::NewSession {
        name: "birthrace".to_string(),
        command: String::new(),
        cwd: String::new(),
        cols: 0,
        rows: 0,
        client_name: String::new(),
    })
    .await
    .unwrap();
    let created = timeout(Duration::from_secs(3), a.next())
        .await
        .expect("timed out")
        .expect("stream ended")
        .expect("decode error");
    let id = match created {
        Frame::SessionCreated { id } => id,
        other => panic!("expected SessionCreated, got {other:?}"),
    };
    let ack = timeout(Duration::from_secs(3), a.next())
        .await
        .expect("timed out")
        .expect("stream ended")
        .expect("decode error");
    assert!(matches!(ack, Frame::AttachAck { .. }), "expected AttachAck, got {ack:?}");

    // Second client B: non-forced explicit connect (force = false,
    // attach_token = 0). It must be told the session is already attached.
    let stream_b = UnixStream::connect(&ctl_path).await.unwrap();
    let mut b = Framed::new(stream_b, FrameCodec);
    do_handshake(&mut b).await;
    b.send(Frame::Attach {
        session: id.to_string(),
        client_name: String::new(),
        force: false,
        no_replay: false,
        cols: 0,
        rows: 0,
        attach_token: 0,
        rendered_offset: 0,
        line_dirty: false,
    })
    .await
    .unwrap();
    let resp = timeout(Duration::from_secs(5), b.next())
        .await
        .expect("timed out")
        .expect("stream ended")
        .expect("decode error");
    match resp {
        Frame::Error { code, .. } => {
            assert_eq!(code, ErrorCode::AlreadyAttached, "expected AlreadyAttached, got {code:?}");
        }
        other => {
            panic!("a non-forced attach during the birth window must be rejected, got {other:?}")
        }
    }

    // The creator must not have been detached by the rejected attach.
    let a_frame = timeout(Duration::from_millis(300), a.next()).await;
    assert!(
        !matches!(a_frame, Ok(Some(Ok(Frame::Detached)))),
        "creator was wrongly detached by a rejected non-forced attach"
    );

    kill_cleanup(&ctl_path, &id.to_string()).await;
}

/// Regression: attaching to a session whose shell has exited should return Error,
/// not Ok followed by a silent disconnect.
#[tokio::test]
async fn attach_dead_session_returns_error() {
    let (_tmp, ctl_path) = test_ctl();

    let ctl = ctl_path.clone();
    let _daemon = tokio::spawn(async move { gritty::daemon::run(&ctl, None).await });
    wait_for_daemon(&ctl_path).await;

    let id = create_session(&ctl_path, "dying").await;

    // Wait for shell PID
    let shell_pid;
    let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
    loop {
        tokio::time::sleep(Duration::from_millis(200)).await;
        let resp = control_request(&ctl_path, Frame::ListSessions).await;
        match &resp {
            Frame::SessionInfo { sessions } if sessions.len() == 1 && sessions[0].shell_pid > 0 => {
                shell_pid = sessions[0].shell_pid;
                break;
            }
            _ if tokio::time::Instant::now() < deadline => continue,
            other => panic!("shell did not start within 10s, got {other:?}"),
        }
    }

    // Kill the shell externally
    unsafe {
        libc::kill(shell_pid as i32, libc::SIGKILL);
    }

    // Wait for server task to notice and exit
    tokio::time::sleep(Duration::from_secs(1)).await;

    // Attach should get an error, not Ok + disconnect
    let resp = control_request(
        &ctl_path,
        Frame::Attach {
            session: id.clone(),
            client_name: String::new(),
            force: false,
            no_replay: false,
            cols: 0,
            rows: 0,
            attach_token: 0,
            rendered_offset: 0,
            line_dirty: false,
        },
    )
    .await;
    assert!(
        matches!(resp, Frame::Error { .. }),
        "expected Error for dead session attach, got {resp:?}"
    );
}

/// Regression: killing a session whose shell has already exited should return Error,
/// not Ok for a stale entry.
#[tokio::test]
async fn kill_dead_session_returns_error() {
    let (_tmp, ctl_path) = test_ctl();

    let ctl = ctl_path.clone();
    let _daemon = tokio::spawn(async move { gritty::daemon::run(&ctl, None).await });
    wait_for_daemon(&ctl_path).await;

    let id = create_session(&ctl_path, "dying2").await;

    // Wait for shell PID
    let shell_pid;
    let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
    loop {
        tokio::time::sleep(Duration::from_millis(200)).await;
        let resp = control_request(&ctl_path, Frame::ListSessions).await;
        match &resp {
            Frame::SessionInfo { sessions } if sessions.len() == 1 && sessions[0].shell_pid > 0 => {
                shell_pid = sessions[0].shell_pid;
                break;
            }
            _ if tokio::time::Instant::now() < deadline => continue,
            other => panic!("shell did not start within 10s, got {other:?}"),
        }
    }

    // Kill the shell externally
    unsafe {
        libc::kill(shell_pid as i32, libc::SIGKILL);
    }

    // Wait for server task to notice and exit
    tokio::time::sleep(Duration::from_secs(1)).await;

    // Kill should get an error, not Ok for a stale entry
    let resp = control_request(&ctl_path, Frame::KillSession { session: id.clone() }).await;
    assert!(
        matches!(resp, Frame::Error { .. }),
        "expected Error for dead session kill, got {resp:?}"
    );
}

#[tokio::test]
async fn list_sessions_shows_heartbeat() {
    let (_tmp, ctl_path) = test_ctl();

    let ctl = ctl_path.clone();
    let _daemon = tokio::spawn(async move { gritty::daemon::run(&ctl, None).await });
    wait_for_daemon(&ctl_path).await;

    let id = create_session(&ctl_path, "hbtest").await;

    // Attach and send a Ping
    let mut framed = attach_session(&ctl_path, &id).await;
    drain_data(&mut framed, Duration::from_millis(500)).await;

    framed.send(Frame::Ping).await.unwrap();
    // Wait for Pong
    loop {
        match timeout(Duration::from_secs(3), framed.next()).await {
            Ok(Some(Ok(Frame::Pong))) => break,
            Ok(Some(Ok(Frame::Data(_)))) => continue,
            other => panic!("expected Pong, got {other:?}"),
        }
    }

    // List sessions — last_heartbeat should be > 0
    let resp = control_request(&ctl_path, Frame::ListSessions).await;
    match &resp {
        Frame::SessionInfo { sessions } => {
            assert_eq!(sessions.len(), 1);
            assert!(
                sessions[0].last_heartbeat > 0,
                "last_heartbeat should be set after Ping, got {}",
                sessions[0].last_heartbeat
            );
        }
        other => panic!("expected SessionInfo, got {other:?}"),
    }

    kill_cleanup(&ctl_path, &id).await;
}

#[tokio::test]
async fn reconnect_via_daemon_after_disconnect() {
    let (_tmp, ctl_path) = test_ctl();

    let ctl = ctl_path.clone();
    let _daemon = tokio::spawn(async move { gritty::daemon::run(&ctl, None).await });
    wait_for_daemon(&ctl_path).await;

    let id = create_session(&ctl_path, "reconn").await;

    // First attach — set an env marker
    let mut framed = attach_session(&ctl_path, &id).await;
    drain_data(&mut framed, Duration::from_millis(500)).await;
    framed.send(Frame::Data(Bytes::from("export RECONN_MARKER=persisted\n"))).await.unwrap();
    drain_data(&mut framed, Duration::from_millis(500)).await;

    // Disconnect
    drop(framed);
    tokio::time::sleep(Duration::from_millis(500)).await;

    // Re-attach via new daemon connection (raw — shell already running, no guaranteed output)
    let stream = UnixStream::connect(&ctl_path).await.unwrap();
    let mut framed = Framed::new(stream, FrameCodec);
    do_handshake(&mut framed).await;
    framed
        .send(Frame::Attach {
            session: id.clone(),
            client_name: String::new(),
            force: false,
            no_replay: false,
            cols: 0,
            rows: 0,
            attach_token: 0,
            rendered_offset: 0,
            line_dirty: false,
        })
        .await
        .unwrap();
    let resp = timeout(Duration::from_secs(3), framed.next())
        .await
        .expect("timed out")
        .expect("stream ended")
        .expect("decode error");
    assert!(
        matches!(resp, Frame::AttachAck { .. }),
        "expected AttachAck for re-attach, got {resp:?}"
    );
    framed.send(Frame::Resize { cols: 80, rows: 24 }).await.unwrap();
    drain_data(&mut framed, Duration::from_millis(500)).await;

    // Verify marker persists. Poll until the expanded value appears — the shell is a
    // separate process and may lag the kernel PTY echo under CPU contention, so a
    // gap-based collect can exit holding only the line-discipline echo.
    framed.send(Frame::Data(Bytes::from("echo MARK-$RECONN_MARKER-END\n"))).await.unwrap();
    let mut output = Vec::new();
    let deadline = tokio::time::Instant::now() + Duration::from_secs(15);
    loop {
        match timeout(Duration::from_secs(1), framed.next()).await {
            Ok(Some(Ok(Frame::Data(data)))) => {
                output.extend_from_slice(&data);
                if String::from_utf8_lossy(&output).contains("MARK-persisted-END") {
                    break;
                }
            }
            _ if tokio::time::Instant::now() >= deadline => {
                let output_str = String::from_utf8_lossy(&output);
                panic!("env marker should persist across reconnect, got: {output_str}");
            }
            _ => {}
        }
    }

    kill_cleanup(&ctl_path, &id).await;
}

#[tokio::test]
async fn reconnect_after_session_killed_returns_error() {
    let (_tmp, ctl_path) = test_ctl();

    let ctl = ctl_path.clone();
    let _daemon = tokio::spawn(async move { gritty::daemon::run(&ctl, None).await });
    wait_for_daemon(&ctl_path).await;

    let id = create_session(&ctl_path, "doomed-reconn").await;

    // Attach
    let mut framed = attach_session(&ctl_path, &id).await;
    drain_data(&mut framed, Duration::from_millis(500)).await;

    // Disconnect
    drop(framed);
    tokio::time::sleep(Duration::from_millis(300)).await;

    // Kill the session
    let resp = control_request(&ctl_path, Frame::KillSession { session: id.clone() }).await;
    assert_eq!(resp, Frame::Ok);
    tokio::time::sleep(Duration::from_millis(300)).await;

    // Try to re-attach -- should get Error
    let resp = control_request(
        &ctl_path,
        Frame::Attach {
            session: id.clone(),
            client_name: String::new(),
            force: false,
            no_replay: false,
            cols: 0,
            rows: 0,
            attach_token: 0,
            rendered_offset: 0,
            line_dirty: false,
        },
    )
    .await;
    assert!(
        matches!(resp, Frame::Error { .. }),
        "expected Error when attaching to killed session, got {resp:?}"
    );
}

#[tokio::test]
async fn tail_request() {
    let (_tmp, ctl_path) = test_ctl();

    let ctl = ctl_path.clone();
    let _daemon = tokio::spawn(async move { gritty::daemon::run(&ctl, None).await });
    wait_for_daemon(&ctl_path).await;

    let id = create_session(&ctl_path, "tailtarget").await;

    // Tail via the daemon — should get Ok
    let resp = control_request(&ctl_path, Frame::Tail { session: id.clone() }).await;
    assert_eq!(resp, Frame::Ok, "expected Ok for tail, got {resp:?}");

    kill_cleanup(&ctl_path, &id).await;
}

#[tokio::test]
async fn tail_nonexistent_returns_error() {
    let (_tmp, ctl_path) = test_ctl();

    let ctl = ctl_path.clone();
    let _daemon = tokio::spawn(async move { gritty::daemon::run(&ctl, None).await });
    wait_for_daemon(&ctl_path).await;

    let resp = control_request(&ctl_path, Frame::Tail { session: "nonexistent".to_string() }).await;
    assert!(
        matches!(resp, Frame::Error { .. }),
        "expected Error for nonexistent tail, got {resp:?}"
    );
}

#[tokio::test]
async fn daemon_rejects_non_hello_first_frame() {
    let (_tmp, ctl_path) = test_ctl();

    let ctl = ctl_path.clone();
    let _daemon = tokio::spawn(async move { gritty::daemon::run(&ctl, None).await });
    wait_for_daemon(&ctl_path).await;

    // Send a ListSessions frame without Hello first
    let stream = UnixStream::connect(&ctl_path).await.unwrap();
    let mut framed = Framed::new(stream, FrameCodec);
    framed.send(Frame::ListSessions).await.unwrap();
    let resp = timeout(Duration::from_secs(3), framed.next())
        .await
        .expect("timed out")
        .expect("stream ended")
        .expect("decode error");
    match resp {
        Frame::Error { message, .. } => {
            assert!(message.contains("Hello"), "error should mention Hello, got: {message}");
        }
        other => panic!("expected Error for non-Hello first frame, got {other:?}"),
    }

    // Daemon should still be alive for subsequent valid requests
    let resp = control_request(&ctl_path, Frame::ListSessions).await;
    assert!(
        matches!(resp, Frame::SessionInfo { .. }),
        "daemon should still work after rejecting bad client, got {resp:?}"
    );
}

#[tokio::test]
async fn daemon_rejects_purely_numeric_name() {
    let (_tmp, ctl_path) = test_ctl();

    let ctl = ctl_path.clone();
    let _daemon = tokio::spawn(async move { gritty::daemon::run(&ctl, None).await });
    wait_for_daemon(&ctl_path).await;

    let resp = control_request(
        &ctl_path,
        Frame::NewSession {
            name: "42".to_string(),
            command: String::new(),
            cwd: String::new(),
            cols: 0,
            rows: 0,
            client_name: String::new(),
        },
    )
    .await;
    match resp {
        Frame::Error { message, .. } => {
            assert!(
                message.contains("purely numeric"),
                "error should mention purely numeric, got: {message}"
            );
        }
        other => panic!("expected Error for numeric name, got {other:?}"),
    }

    // Non-numeric names with digits should still be allowed
    let id = create_session(&ctl_path, "session2").await;
    kill_cleanup(&ctl_path, &id).await;
}

#[tokio::test]
async fn rename_session_success() {
    let (_tmp, ctl_path) = test_ctl();

    let ctl = ctl_path.clone();
    let _daemon = tokio::spawn(async move { gritty::daemon::run(&ctl, None).await });
    wait_for_daemon(&ctl_path).await;

    let id = create_session(&ctl_path, "oldname").await;

    let resp = control_request(
        &ctl_path,
        Frame::RenameSession { session: "oldname".to_string(), new_name: "newname".to_string() },
    )
    .await;
    assert_eq!(resp, Frame::Ok);

    // Verify the session is now findable by new name
    let resp = control_request(&ctl_path, Frame::ListSessions).await;
    match resp {
        Frame::SessionInfo { sessions } => {
            assert_eq!(sessions.len(), 1);
            assert_eq!(sessions[0].name, "newname");
        }
        other => panic!("expected SessionInfo, got {other:?}"),
    }

    kill_cleanup(&ctl_path, &id).await;
}

#[tokio::test]
async fn rename_session_to_taken_name() {
    let (_tmp, ctl_path) = test_ctl();

    let ctl = ctl_path.clone();
    let _daemon = tokio::spawn(async move { gritty::daemon::run(&ctl, None).await });
    wait_for_daemon(&ctl_path).await;

    let id_a = create_session(&ctl_path, "alice").await;
    let id_b = create_session(&ctl_path, "bob").await;

    let resp = control_request(
        &ctl_path,
        Frame::RenameSession { session: "alice".to_string(), new_name: "bob".to_string() },
    )
    .await;
    match resp {
        Frame::Error { message, .. } => {
            assert!(
                message.contains("already exists"),
                "error should mention duplicate, got: {message}"
            );
        }
        other => panic!("expected Error for duplicate name, got {other:?}"),
    }

    kill_cleanup(&ctl_path, &id_a).await;
    kill_cleanup(&ctl_path, &id_b).await;
}

#[tokio::test]
async fn rename_session_to_numeric_name() {
    let (_tmp, ctl_path) = test_ctl();

    let ctl = ctl_path.clone();
    let _daemon = tokio::spawn(async move { gritty::daemon::run(&ctl, None).await });
    wait_for_daemon(&ctl_path).await;

    let id = create_session(&ctl_path, "myname").await;

    let resp = control_request(
        &ctl_path,
        Frame::RenameSession { session: "myname".to_string(), new_name: "42".to_string() },
    )
    .await;
    match resp {
        Frame::Error { message, .. } => {
            assert!(
                message.contains("purely numeric"),
                "error should mention numeric, got: {message}"
            );
        }
        other => panic!("expected Error for numeric rename, got {other:?}"),
    }

    kill_cleanup(&ctl_path, &id).await;
}

#[tokio::test]
async fn attach_dash_resolves_to_last_session() {
    let (_tmp, ctl_path) = test_ctl();

    let ctl = ctl_path.clone();
    let _daemon = tokio::spawn(async move { gritty::daemon::run(&ctl, None).await });
    wait_for_daemon(&ctl_path).await;

    // create_session auto-attaches, so last_attached is set to each session
    let id_a = create_session(&ctl_path, "alpha").await;
    let _id_b = create_session(&ctl_path, "beta").await;
    // After creating beta, last_attached = beta's id

    // Explicitly attach to alpha (updates last_attached to alpha)
    let resp = control_request(
        &ctl_path,
        Frame::Attach {
            session: id_a.clone(),
            client_name: String::new(),
            force: false,
            no_replay: false,
            cols: 0,
            rows: 0,
            attach_token: 0,
            rendered_offset: 0,
            line_dirty: false,
        },
    )
    .await;
    assert!(matches!(resp, Frame::AttachAck { .. }), "expected AttachAck for attach, got {resp:?}");
    tokio::time::sleep(Duration::from_millis(100)).await;

    // Now "-" should resolve to alpha (last explicitly attached)
    let resp = control_request(
        &ctl_path,
        Frame::Attach {
            session: "-".to_string(),
            client_name: String::new(),
            force: false,
            no_replay: false,
            cols: 0,
            rows: 0,
            attach_token: 0,
            rendered_offset: 0,
            line_dirty: false,
        },
    )
    .await;
    assert!(
        matches!(resp, Frame::AttachAck { .. }),
        "attach - should resolve to last attached session (alpha); got {resp:?}"
    );

    kill_cleanup(&ctl_path, &id_a).await;
    kill_cleanup(&ctl_path, &_id_b).await;
}

/// Set up a fresh daemon + create a session via NewSession and keep the
/// framed connection alive. Returns the framed connection (creator is
/// auto-attached on this framed).
async fn new_session_for_test(
    ctl_path: &std::path::Path,
    name: &str,
    client_name: &str,
    device_id: u64,
) -> Framed<UnixStream, FrameCodec> {
    let stream = UnixStream::connect(ctl_path).await.unwrap();
    let mut framed = Framed::new(stream, FrameCodec);
    do_handshake_as(&mut framed, device_id).await;
    framed
        .send(Frame::NewSession {
            name: name.to_string(),
            command: String::new(),
            cwd: String::new(),
            cols: 0,
            rows: 0,
            client_name: client_name.to_string(),
        })
        .await
        .unwrap();
    match timeout(Duration::from_secs(3), framed.next()).await.unwrap().unwrap().unwrap() {
        Frame::SessionCreated { .. } => {}
        other => panic!("expected SessionCreated, got {other:?}"),
    }
    match timeout(Duration::from_secs(3), framed.next()).await.unwrap().unwrap().unwrap() {
        Frame::AttachAck { .. } => {}
        other => panic!("expected AttachAck, got {other:?}"),
    }
    framed
        .send(Frame::Env { vars: vec![("TERM".to_string(), "xterm".to_string())] })
        .await
        .unwrap();
    drain_data(&mut framed, Duration::from_millis(500)).await;
    framed
}

#[tokio::test]
async fn different_device_rejected_with_owner_changed() {
    // Device A (id=100) creates session. Device B (id=200) force-takes-over
    // with attach_token=0 (explicit connect). Device A auto-reconnects with
    // attach_token!=0 (ownership claim) -- must be rejected because device B
    // is now the owner.
    let (_tmp, ctl_path) = test_ctl();
    let ctl = ctl_path.clone();
    let _daemon = tokio::spawn(async move { gritty::daemon::run(&ctl, None).await });
    wait_for_daemon(&ctl_path).await;

    let framed_a = new_session_for_test(&ctl_path, "alpha", "laptop-a", 100).await;

    // B forcefully takes over (explicit connect: attach_token=0).
    let resp = control_request_as(
        &ctl_path,
        Frame::Attach {
            session: "alpha".to_string(),
            client_name: "laptop-b".to_string(),
            force: true,
            no_replay: false,
            cols: 0,
            rows: 0,
            attach_token: 0,
            rendered_offset: 0,
            line_dirty: false,
        },
        200,
    )
    .await;
    assert!(matches!(resp, Frame::AttachAck { .. }), "force takeover should succeed");

    // A's auto-reconnect: attach_token != 0 signals ownership claim.
    // Hello carries device_id=100, but owner is now 200. Must reject.
    let resp = control_request_as(
        &ctl_path,
        Frame::Attach {
            session: "alpha".to_string(),
            client_name: "laptop-a".to_string(),
            force: true,
            no_replay: false,
            cols: 0,
            rows: 0,
            attach_token: 100, // non-zero = auto-reconnect
            rendered_offset: 0,
            line_dirty: false,
        },
        100,
    )
    .await;
    match resp {
        Frame::Error { code: ErrorCode::OwnerChanged, .. } => {}
        other => panic!("expected OwnerChanged for different device, got {other:?}"),
    }

    drop(framed_a);
    kill_cleanup(&ctl_path, "alpha").await;
}

#[tokio::test]
async fn same_device_reconnect_succeeds() {
    // Device A creates session, disconnects, reconnects with the same
    // device_id. Should succeed (silent reconnect).
    let (_tmp, ctl_path) = test_ctl();
    let ctl = ctl_path.clone();
    let _daemon = tokio::spawn(async move { gritty::daemon::run(&ctl, None).await });
    wait_for_daemon(&ctl_path).await;

    let framed_a = new_session_for_test(&ctl_path, "alpha", "laptop-a", 100).await;
    drop(framed_a); // simulate disconnect

    tokio::time::sleep(Duration::from_millis(100)).await;

    // Same device_id=100 reconnects with ownership claim.
    let resp = control_request_as(
        &ctl_path,
        Frame::Attach {
            session: "alpha".to_string(),
            client_name: "laptop-a".to_string(),
            force: true,
            no_replay: false,
            cols: 0,
            rows: 0,
            attach_token: 100, // non-zero = auto-reconnect
            rendered_offset: 0,
            line_dirty: false,
        },
        100,
    )
    .await;
    match resp {
        Frame::AttachAck { .. } => {}
        other => panic!("expected AttachAck for same device reconnect, got {other:?}"),
    }

    kill_cleanup(&ctl_path, "alpha").await;
}

#[tokio::test]
async fn already_attached_error_names_current_client() {
    let (_tmp, ctl_path) = test_ctl();

    let ctl = ctl_path.clone();
    let _daemon = tokio::spawn(async move { gritty::daemon::run(&ctl, None).await });
    wait_for_daemon(&ctl_path).await;

    // Create a session as "laptop-a" and keep the framed connection alive so
    // the session stays attached for the duration of the test.
    let stream = UnixStream::connect(&ctl_path).await.unwrap();
    let mut framed_a = Framed::new(stream, FrameCodec);
    do_handshake(&mut framed_a).await;
    framed_a
        .send(Frame::NewSession {
            name: "alpha".to_string(),
            command: String::new(),
            cwd: String::new(),
            cols: 0,
            rows: 0,
            client_name: "laptop-a".to_string(),
        })
        .await
        .unwrap();
    let resp = timeout(Duration::from_secs(3), framed_a.next())
        .await
        .expect("timed out")
        .expect("stream ended")
        .expect("decode error");
    let id = match resp {
        Frame::SessionCreated { id } => id.to_string(),
        other => panic!("expected SessionCreated, got {other:?}"),
    };
    // Send an Env frame so the shell spawns and the session is fully attached.
    framed_a
        .send(Frame::Env { vars: vec![("TERM".to_string(), "xterm".to_string())] })
        .await
        .unwrap();
    drain_data(&mut framed_a, Duration::from_millis(500)).await;

    // Second laptop tries to attach without force -- should get AlreadyAttached
    // with a message naming "laptop-a".
    let resp = control_request(
        &ctl_path,
        Frame::Attach {
            session: "alpha".to_string(),
            client_name: "laptop-b".to_string(),
            force: false,
            no_replay: false,
            cols: 0,
            rows: 0,
            attach_token: 0,
            rendered_offset: 0,
            line_dirty: false,
        },
    )
    .await;
    match resp {
        Frame::Error { code: ErrorCode::AlreadyAttached, message } => {
            assert!(
                message.contains("laptop-a"),
                "expected error message to name current attacher 'laptop-a', got: {message}"
            );
        }
        other => panic!("expected AlreadyAttached error, got {other:?}"),
    }

    drop(framed_a);
    kill_cleanup(&ctl_path, &id).await;
}

#[tokio::test]
async fn client_prefixed_names_do_not_collide_across_clients() {
    // Two simulated clients with different `client_name` values both
    // resolve `gritty c host:default` -- under the client-prefix rule
    // they send `laptop-a/default` and `laptop-b/default` respectively,
    // which must coexist as two distinct sessions on the same daemon.
    // Pre-prefix: both would have collided on the bare name `default`.
    let (_tmp, ctl_path) = test_ctl();
    let ctl = ctl_path.clone();
    let _daemon = tokio::spawn(async move { gritty::daemon::run(&ctl, None).await });
    wait_for_daemon(&ctl_path).await;

    let _framed_a = new_session_for_test(&ctl_path, "laptop-a/default", "laptop-a", 100).await;
    let _framed_b = new_session_for_test(&ctl_path, "laptop-b/default", "laptop-b", 200).await;

    let resp = control_request(&ctl_path, Frame::ListSessions).await;
    let sessions = match resp {
        Frame::SessionInfo { sessions } => sessions,
        other => panic!("expected SessionInfo, got {other:?}"),
    };
    assert_eq!(sessions.len(), 2, "expected two distinct sessions, got {sessions:#?}");
    let mut names: Vec<_> = sessions.iter().map(|s| s.name.clone()).collect();
    names.sort();
    assert_eq!(names, vec!["laptop-a/default".to_string(), "laptop-b/default".to_string()]);
}

#[tokio::test]
async fn slash_bearing_name_enables_cross_client_access() {
    // Client A creates `laptop-a/work`. Client B types `gritty c host:laptop-a/work`
    // -- the slash in the user-supplied name causes the client to skip its own
    // prefixing rule and pass the literal foreign name through. The daemon
    // accepts it as a valid attach (force-takeover, since A holds it). This
    // is the documented foreign-access form.
    let (_tmp, ctl_path) = test_ctl();
    let ctl = ctl_path.clone();
    let _daemon = tokio::spawn(async move { gritty::daemon::run(&ctl, None).await });
    wait_for_daemon(&ctl_path).await;

    let _framed_a = new_session_for_test(&ctl_path, "laptop-a/work", "laptop-a", 100).await;

    let resp = control_request_as(
        &ctl_path,
        Frame::Attach {
            session: "laptop-a/work".to_string(),
            client_name: "laptop-b".to_string(),
            force: true,
            no_replay: false,
            cols: 0,
            rows: 0,
            attach_token: 0,
            rendered_offset: 0,
            line_dirty: false,
        },
        200,
    )
    .await;
    assert!(
        matches!(resp, Frame::AttachAck { .. }),
        "expected cross-client foreign attach to succeed, got {resp:?}"
    );
}

#[tokio::test]
async fn attach_dash_no_previous_session() {
    let (_tmp, ctl_path) = test_ctl();

    let ctl = ctl_path.clone();
    let _daemon = tokio::spawn(async move { gritty::daemon::run(&ctl, None).await });
    wait_for_daemon(&ctl_path).await;

    // No sessions created at all, "-" should fail
    let resp = control_request(
        &ctl_path,
        Frame::Attach {
            session: "-".to_string(),
            client_name: String::new(),
            force: false,
            no_replay: false,
            cols: 0,
            rows: 0,
            attach_token: 0,
            rendered_offset: 0,
            line_dirty: false,
        },
    )
    .await;
    match resp {
        Frame::Error { message, .. } => {
            assert!(
                message.contains("no such session"),
                "expected no-such-session error, got: {message}"
            );
        }
        other => panic!("expected Error for attach -, got {other:?}"),
    }
}

// ---------------------------------------------------------------------------
// Socket self-heal: the daemon detects loss/replacement of its control socket
// (e.g. systemd wiping $XDG_RUNTIME_DIR, or /tmp age-based cleanup) and either
// re-binds in place (sessions survive) or exits cleanly -- it must never
// linger as an unreachable orphan that needs a manual `kill`.
// ---------------------------------------------------------------------------

/// Spawn a daemon with a fast socket-check interval so self-heal tests don't
/// wait for the production default.
fn spawn_fast_check_daemon(
    ctl_path: &std::path::Path,
) -> tokio::task::JoinHandle<anyhow::Result<()>> {
    let ctl = ctl_path.to_path_buf();
    tokio::spawn(async move {
        gritty::daemon::run_with_options(
            &ctl,
            None,
            gritty::daemon::DaemonOptions { socket_check_interval: Duration::from_millis(100) },
        )
        .await
    })
}

/// Extract the session list or panic.
fn expect_session_info(resp: &Frame) -> &Vec<gritty::protocol::SessionEntry> {
    match resp {
        Frame::SessionInfo { sessions } => sessions,
        other => panic!("expected SessionInfo, got {other:?}"),
    }
}

#[tokio::test]
async fn daemon_rebinds_after_socket_file_removed() {
    let (_tmp, ctl_path) = test_ctl();
    let _daemon = spawn_fast_check_daemon(&ctl_path);
    wait_for_daemon(&ctl_path).await;

    let id = create_session(&ctl_path, "survivor").await;

    // Simulate external cleanup (tmpfiles-style) deleting just the socket.
    std::fs::remove_file(&ctl_path).unwrap();

    // The daemon should notice and re-bind at the same path.
    wait_for_daemon(&ctl_path).await;

    // Re-bound, same daemon: the session must have survived.
    let resp = control_request(&ctl_path, Frame::ListSessions).await;
    let sessions = expect_session_info(&resp);
    assert!(
        sessions.iter().any(|s| s.id.to_string() == id && s.name == "survivor"),
        "session lost across rebind: {sessions:?}"
    );

    // The pid-file registration must be restored too, or doctor/refresh
    // would classify this daemon as an orphan.
    let pid_path = ctl_path.with_file_name("daemon.pid");
    let pid: u32 = std::fs::read_to_string(&pid_path)
        .expect("daemon.pid should be restored after rebind")
        .trim()
        .parse()
        .unwrap();
    assert_eq!(pid, std::process::id());

    kill_cleanup(&ctl_path, &id).await;
}

#[tokio::test]
async fn daemon_rebinds_after_socket_dir_removed() {
    let (tmp, ctl_path) = test_ctl();
    let _daemon = spawn_fast_check_daemon(&ctl_path);
    wait_for_daemon(&ctl_path).await;

    let id = create_session(&ctl_path, "dirloss").await;

    // Simulate $XDG_RUNTIME_DIR-style teardown: the whole socket dir vanishes.
    std::fs::remove_dir_all(tmp.path()).unwrap();

    // The daemon should recreate the directory and re-bind.
    wait_for_daemon(&ctl_path).await;

    let resp = control_request(&ctl_path, Frame::ListSessions).await;
    let sessions = expect_session_info(&resp);
    assert!(
        sessions.iter().any(|s| s.id.to_string() == id),
        "session lost across dir recreation: {sessions:?}"
    );

    kill_cleanup(&ctl_path, &id).await;
}

#[tokio::test]
async fn daemon_exits_when_socket_taken_over_by_live_daemon() {
    let (_tmp, ctl_path) = test_ctl();
    let daemon_a = spawn_fast_check_daemon(&ctl_path);
    wait_for_daemon(&ctl_path).await;
    let id = create_session(&ctl_path, "doomed").await;

    // A new daemon takes over the path (the post-wipe race: a client
    // auto-started a fresh daemon before the old one noticed the wipe).
    std::fs::remove_file(&ctl_path).unwrap();
    let ctl_b = ctl_path.clone();
    let _daemon_b = tokio::spawn(async move { gritty::daemon::run(&ctl_b, None).await });
    wait_for_daemon(&ctl_path).await;

    // Daemon A must notice it lost the path and exit cleanly -- never
    // linger as an unreachable orphan.
    let result = timeout(Duration::from_secs(5), daemon_a)
        .await
        .expect("daemon A did not exit after losing its socket to a live daemon")
        .expect("daemon A panicked");
    assert!(result.is_ok(), "takeover exit should be clean, got {result:?}");

    // Daemon B must be untouched: A's exit must not unlink B's socket or
    // sidecars (A's session `doomed` died with A; B starts empty).
    let resp = control_request(&ctl_path, Frame::ListSessions).await;
    let sessions = expect_session_info(&resp);
    assert!(
        !sessions.iter().any(|s| s.id.to_string() == id),
        "daemon B should not have inherited A's sessions"
    );
    let pid_path = ctl_path.with_file_name("daemon.pid");
    assert!(pid_path.exists(), "daemon B's pid file must survive A's exit");
}

#[tokio::test]
async fn daemon_exits_when_socket_dir_unrecoverable() {
    let (tmp, ctl_path) = test_ctl();
    let daemon = spawn_fast_check_daemon(&ctl_path);
    wait_for_daemon(&ctl_path).await;

    // Remove the dir and block its recreation by parking a regular file at
    // the directory path (stand-in for an unrecreatable $XDG_RUNTIME_DIR).
    let dir = tmp.path().to_path_buf();
    std::fs::remove_dir_all(&dir).unwrap();
    std::fs::write(&dir, b"blocked").unwrap();

    // The daemon cannot recover; it must exit rather than linger unreachable.
    let result = timeout(Duration::from_secs(5), daemon)
        .await
        .expect("daemon did not exit after unrecoverable socket-dir loss")
        .expect("daemon panicked");
    assert!(result.is_ok(), "unrecoverable-loss exit should be clean, got {result:?}");

    // Clean up the blocking file so TempDir::drop is happy.
    let _ = std::fs::remove_file(&dir);
}

// ---------------------------------------------------------------------------
// End-to-end protocol probe (used by `gritty refresh <host>` through the
// tunnel socket; here exercised directly against a daemon socket, which has
// identical semantics -- that's the point of socket-forwarded tunnels).
// ---------------------------------------------------------------------------

#[tokio::test]
async fn probe_socket_protocol_returns_daemon_version() {
    let (_tmp, ctl_path) = test_ctl();
    let ctl = ctl_path.clone();
    let _daemon = tokio::spawn(async move { gritty::daemon::run(&ctl, None).await });
    wait_for_daemon(&ctl_path).await;

    let version = gritty::connect::probe_socket_protocol(&ctl_path)
        .await
        .expect("probe against a live daemon should succeed");
    assert_eq!(version, PROTOCOL_VERSION);
}

#[tokio::test]
async fn probe_socket_protocol_fails_cleanly_when_no_daemon() {
    let (_tmp, ctl_path) = test_ctl();
    // No daemon bound: the probe must return Err, not hang.
    let result = timeout(Duration::from_secs(5), gritty::connect::probe_socket_protocol(&ctl_path))
        .await
        .expect("probe must not hang on a missing socket");
    assert!(result.is_err(), "probe of a nonexistent socket should fail, got {result:?}");
}
