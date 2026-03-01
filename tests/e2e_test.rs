use bytes::Bytes;
use futures_util::{SinkExt, StreamExt};
use gritty::protocol::{Frame, FrameCodec};
use gritty::server::ClientConn;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, LazyLock, OnceLock};
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::UnixStream;
use tokio::sync::{Semaphore, mpsc};
use tokio::task::JoinHandle;
use tokio::time::timeout;
use tokio_util::codec::Framed;

/// Limit concurrent e2e tests to avoid PTY/CPU exhaustion under parallel load.
static CONCURRENCY: LazyLock<Semaphore> = LazyLock::new(|| Semaphore::new(4));

static TEST_ID: AtomicU32 = AtomicU32::new(0);

/// Shared temp directory for all e2e test sockets (isolated from /tmp).
static TEST_DIR: LazyLock<tempfile::TempDir> = LazyLock::new(|| tempfile::tempdir().unwrap());

fn unique_agent_socket_path() -> PathBuf {
    let id = TEST_ID.fetch_add(1, Ordering::Relaxed);
    let pid = std::process::id();
    TEST_DIR.path().join(format!("agent-{pid}-{id}.sock"))
}

fn unique_svc_socket_path() -> PathBuf {
    let id = TEST_ID.fetch_add(1, Ordering::Relaxed);
    let pid = std::process::id();
    TEST_DIR.path().join(format!("svc-{pid}-{id}.sock"))
}

/// Spawn a server task connected via socketpair + channel.
/// Returns (client_tx for takeover, client-side framed, server join handle).
async fn setup_session() -> (
    mpsc::UnboundedSender<ClientConn>,
    Framed<UnixStream, FrameCodec>,
    JoinHandle<anyhow::Result<()>>,
    Arc<OnceLock<gritty::server::SessionMetadata>>,
) {
    let (client_tx, client_rx) = mpsc::unbounded_channel();
    let meta = Arc::new(OnceLock::new());
    let meta_clone = Arc::clone(&meta);
    let agent_path = unique_agent_socket_path();
    let svc_path = unique_svc_socket_path();
    let handle = tokio::spawn(async move {
        gritty::server::run(client_rx, meta_clone, agent_path, svc_path).await
    });

    let (server_stream, client_stream) = UnixStream::pair().unwrap();
    client_tx.send(ClientConn::Active(Framed::new(server_stream, FrameCodec))).unwrap();

    let mut framed = Framed::new(client_stream, FrameCodec);
    // Send empty Env frame so server doesn't wait for the Env timeout before spawning shell
    framed.send(Frame::Env { vars: vec![] }).await.unwrap();

    (client_tx, framed, handle, meta)
}

/// Like setup_session but also returns the service socket path.
async fn setup_session_with_svc_path() -> (
    mpsc::UnboundedSender<ClientConn>,
    Framed<UnixStream, FrameCodec>,
    JoinHandle<anyhow::Result<()>>,
    Arc<OnceLock<gritty::server::SessionMetadata>>,
    PathBuf,
) {
    let (client_tx, client_rx) = mpsc::unbounded_channel();
    let meta = Arc::new(OnceLock::new());
    let meta_clone = Arc::clone(&meta);
    let agent_path = unique_agent_socket_path();
    let svc_path = unique_svc_socket_path();
    let svc_path_clone = svc_path.clone();
    let handle = tokio::spawn(async move {
        gritty::server::run(client_rx, meta_clone, agent_path, svc_path).await
    });

    let (server_stream, client_stream) = UnixStream::pair().unwrap();
    client_tx.send(ClientConn::Active(Framed::new(server_stream, FrameCodec))).unwrap();

    let mut framed = Framed::new(client_stream, FrameCodec);
    framed.send(Frame::Env { vars: vec![] }).await.unwrap();

    (client_tx, framed, handle, meta, svc_path_clone)
}

/// Spawn a server task, send an Env frame before the first Resize.
/// Returns same tuple as setup_session().
async fn setup_session_with_env(
    env_vars: Vec<(String, String)>,
) -> (
    mpsc::UnboundedSender<ClientConn>,
    Framed<UnixStream, FrameCodec>,
    JoinHandle<anyhow::Result<()>>,
    Arc<OnceLock<gritty::server::SessionMetadata>>,
) {
    let (client_tx, client_rx) = mpsc::unbounded_channel();
    let meta = Arc::new(OnceLock::new());
    let meta_clone = Arc::clone(&meta);
    let agent_path = unique_agent_socket_path();
    let svc_path = unique_svc_socket_path();
    let handle = tokio::spawn(async move {
        gritty::server::run(client_rx, meta_clone, agent_path, svc_path).await
    });

    let (server_stream, client_stream) = UnixStream::pair().unwrap();
    client_tx.send(ClientConn::Active(Framed::new(server_stream, FrameCodec))).unwrap();

    let mut framed = Framed::new(client_stream, FrameCodec);
    // Send Env frame so server reads it before spawning shell
    framed.send(Frame::Env { vars: env_vars }).await.unwrap();

    (client_tx, framed, handle, meta)
}

/// Wait for shell to produce initial output (confirms it's alive).
async fn wait_for_shell(framed: &mut Framed<UnixStream, FrameCodec>) {
    // Send initial resize
    framed.send(Frame::Resize { cols: 80, rows: 24 }).await.unwrap();

    let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
    loop {
        match timeout(Duration::from_secs(1), framed.next()).await {
            Ok(Some(Ok(Frame::Data(_)))) => break,
            _ if tokio::time::Instant::now() >= deadline => {
                panic!("shell did not produce output within 10s")
            }
            _ => continue,
        }
    }
}

/// Drain all available Data frames within a timeout, return concatenated bytes.
async fn read_available_data(
    framed: &mut Framed<UnixStream, FrameCodec>,
    wait: Duration,
) -> Vec<u8> {
    let mut out = Vec::new();
    while let Ok(Some(Ok(Frame::Data(data)))) = timeout(wait, framed.next()).await {
        out.extend_from_slice(&data);
    }
    out
}

/// Read frames until we see an Exit frame or timeout.
async fn expect_exit_frame(
    framed: &mut Framed<UnixStream, FrameCodec>,
    wait: Duration,
) -> Option<i32> {
    loop {
        match timeout(wait, framed.next()).await {
            Ok(Some(Ok(Frame::Exit { code }))) => return Some(code),
            Ok(Some(Ok(Frame::Data(_)))) => continue,
            _ => return None,
        }
    }
}

#[tokio::test]
async fn server_spawns_shell_and_relays_output() {
    let _permit = CONCURRENCY.acquire().await.unwrap();
    let (_tx, mut framed, server, _meta) = setup_session().await;
    wait_for_shell(&mut framed).await;
    read_available_data(&mut framed, Duration::from_millis(500)).await;

    let _ = framed.send(Frame::Data(Bytes::from("exit\n"))).await;
    let _ = timeout(Duration::from_secs(3), server).await;
}

#[tokio::test]
async fn server_relays_command_output() {
    let _permit = CONCURRENCY.acquire().await.unwrap();
    let (_tx, mut framed, server, _meta) = setup_session().await;
    wait_for_shell(&mut framed).await;
    read_available_data(&mut framed, Duration::from_secs(1)).await;

    framed.send(Frame::Data(Bytes::from("echo TTYLEPORT_TEST_OK\n"))).await.unwrap();

    let output = read_available_data(&mut framed, Duration::from_secs(2)).await;
    let output_str = String::from_utf8_lossy(&output);
    assert!(
        output_str.contains("TTYLEPORT_TEST_OK"),
        "expected command output in relay, got: {output_str}"
    );

    let _ = framed.send(Frame::Data(Bytes::from("exit\n"))).await;
    let _ = timeout(Duration::from_secs(3), server).await;
}

#[tokio::test]
async fn server_sends_exit_frame_on_shell_exit() {
    let _permit = CONCURRENCY.acquire().await.unwrap();
    let (_tx, mut framed, _server, _meta) = setup_session().await;
    wait_for_shell(&mut framed).await;
    read_available_data(&mut framed, Duration::from_secs(1)).await;

    framed.send(Frame::Data(Bytes::from("exit 42\n"))).await.unwrap();

    let code = expect_exit_frame(&mut framed, Duration::from_secs(5)).await;
    assert_eq!(code, Some(42), "expected exit code 42");
}

#[tokio::test]
async fn reconnect_preserves_shell_session() {
    let _permit = CONCURRENCY.acquire().await.unwrap();
    let (client_tx, mut framed, server, _meta) = setup_session().await;
    wait_for_shell(&mut framed).await;
    read_available_data(&mut framed, Duration::from_secs(1)).await;

    framed.send(Frame::Data(Bytes::from("export TTYLEPORT_MARKER=alive\n"))).await.unwrap();
    read_available_data(&mut framed, Duration::from_millis(500)).await;

    // Disconnect by dropping the framed stream
    drop(framed);

    // Give server time to notice disconnect
    tokio::time::sleep(Duration::from_millis(300)).await;

    // Second connection via socketpair through channel
    let (server_stream, client_stream) = UnixStream::pair().unwrap();
    client_tx.send(ClientConn::Active(Framed::new(server_stream, FrameCodec))).unwrap();
    let mut framed = Framed::new(client_stream, FrameCodec);
    framed.send(Frame::Resize { cols: 80, rows: 24 }).await.unwrap();

    // Drain any buffered output
    read_available_data(&mut framed, Duration::from_secs(1)).await;

    framed.send(Frame::Data(Bytes::from("echo $TTYLEPORT_MARKER\n"))).await.unwrap();

    let output = read_available_data(&mut framed, Duration::from_secs(2)).await;
    let output_str = String::from_utf8_lossy(&output);
    assert!(
        output_str.contains("alive"),
        "shell session should persist across reconnect, got: {output_str}"
    );

    let _ = framed.send(Frame::Data(Bytes::from("exit\n"))).await;
    let _ = timeout(Duration::from_secs(3), server).await;
}

#[tokio::test]
async fn server_exits_when_shell_dies_while_disconnected() {
    let _permit = CONCURRENCY.acquire().await.unwrap();
    let (_tx, mut framed, server, _meta) = setup_session().await;
    wait_for_shell(&mut framed).await;
    read_available_data(&mut framed, Duration::from_secs(1)).await;

    framed.send(Frame::Data(Bytes::from("sleep 0.5 && exit 0\n"))).await.unwrap();
    read_available_data(&mut framed, Duration::from_millis(200)).await;

    // Disconnect
    drop(framed);

    // Server should exit once the shell dies
    let result = timeout(Duration::from_secs(5), server).await;
    assert!(result.is_ok(), "server should exit after shell dies while disconnected");
}

#[tokio::test]
async fn second_client_detaches_first() {
    let _permit = CONCURRENCY.acquire().await.unwrap();
    let (client_tx, mut client1, server, _meta) = setup_session().await;
    wait_for_shell(&mut client1).await;
    read_available_data(&mut client1, Duration::from_secs(1)).await;

    // Second client connects via channel — should take over the session
    let (server_stream2, client_stream2) = UnixStream::pair().unwrap();
    client_tx.send(ClientConn::Active(Framed::new(server_stream2, FrameCodec))).unwrap();
    let mut client2 = Framed::new(client_stream2, FrameCodec);
    client2.send(Frame::Resize { cols: 80, rows: 24 }).await.unwrap();

    // First client should receive Detached
    let mut got_detached = false;
    loop {
        match timeout(Duration::from_secs(3), client1.next()).await {
            Ok(Some(Ok(Frame::Detached))) => {
                got_detached = true;
                break;
            }
            Ok(Some(Ok(Frame::Data(_)))) => continue,
            _ => break,
        }
    }
    assert!(got_detached, "first client should receive Detached frame");

    // Second client should be able to interact with the shell
    read_available_data(&mut client2, Duration::from_secs(1)).await;

    client2.send(Frame::Data(Bytes::from("echo TAKEOVER_OK\n"))).await.unwrap();

    let output = read_available_data(&mut client2, Duration::from_secs(2)).await;
    let output_str = String::from_utf8_lossy(&output);
    assert!(
        output_str.contains("TAKEOVER_OK"),
        "second client should be able to use the session, got: {output_str}"
    );

    let _ = client2.send(Frame::Data(Bytes::from("exit\n"))).await;
    let _ = timeout(Duration::from_secs(3), server).await;
}

#[tokio::test]
async fn exit_code_zero_sends_exit_frame() {
    let _permit = CONCURRENCY.acquire().await.unwrap();
    let (_tx, mut framed, _server, _meta) = setup_session().await;
    wait_for_shell(&mut framed).await;
    read_available_data(&mut framed, Duration::from_secs(1)).await;

    framed.send(Frame::Data(Bytes::from("exit 0\n"))).await.unwrap();

    let code = expect_exit_frame(&mut framed, Duration::from_secs(5)).await;
    assert_eq!(code, Some(0), "expected exit code 0");
}

#[tokio::test]
async fn exit_code_signal_death() {
    let _permit = CONCURRENCY.acquire().await.unwrap();
    let (_tx, mut framed, _server, meta) = setup_session().await;
    wait_for_shell(&mut framed).await;
    read_available_data(&mut framed, Duration::from_secs(1)).await;

    // Wait for metadata
    tokio::time::sleep(Duration::from_millis(100)).await;
    let shell_pid = meta.get().map(|m| m.shell_pid).unwrap_or(0);
    assert!(shell_pid > 0, "should have shell PID in metadata");

    unsafe {
        libc::kill(shell_pid as i32, libc::SIGKILL);
    }

    let code = expect_exit_frame(&mut framed, Duration::from_secs(5)).await;
    assert!(code.is_some(), "expected Exit frame after SIGKILL");
}

#[tokio::test]
async fn rapid_reconnect_cycles() {
    let _permit = CONCURRENCY.acquire().await.unwrap();
    let (client_tx, mut framed, server, _meta) = setup_session().await;
    wait_for_shell(&mut framed).await;
    read_available_data(&mut framed, Duration::from_secs(1)).await;

    framed.send(Frame::Data(Bytes::from("export RAPID_TEST_MARKER=survived\n"))).await.unwrap();
    read_available_data(&mut framed, Duration::from_millis(500)).await;

    // Rapidly disconnect and reconnect 3 times
    for _i in 0..3 {
        drop(framed);
        tokio::time::sleep(Duration::from_millis(200)).await;

        let (server_stream, client_stream) = UnixStream::pair().unwrap();
        client_tx.send(ClientConn::Active(Framed::new(server_stream, FrameCodec))).unwrap();
        framed = Framed::new(client_stream, FrameCodec);
        framed.send(Frame::Resize { cols: 80, rows: 24 }).await.unwrap();
        read_available_data(&mut framed, Duration::from_millis(500)).await;
    }

    framed.send(Frame::Data(Bytes::from("echo $RAPID_TEST_MARKER\n"))).await.unwrap();
    let output = read_available_data(&mut framed, Duration::from_secs(2)).await;
    let output_str = String::from_utf8_lossy(&output);
    assert!(
        output_str.contains("survived"),
        "shell should survive rapid reconnect cycles, got: {output_str}"
    );

    let _ = framed.send(Frame::Data(Bytes::from("exit\n"))).await;
    let _ = timeout(Duration::from_secs(3), server).await;
}

#[tokio::test]
async fn control_frame_on_session_is_ignored() {
    let _permit = CONCURRENCY.acquire().await.unwrap();
    let (_tx, mut framed, server, _meta) = setup_session().await;
    wait_for_shell(&mut framed).await;
    read_available_data(&mut framed, Duration::from_secs(1)).await;

    // Send various control frames that make no sense on a session connection
    framed.send(Frame::ListSessions).await.unwrap();
    framed.send(Frame::KillServer).await.unwrap();
    framed.send(Frame::NewSession { name: "bogus".to_string() }).await.unwrap();

    tokio::time::sleep(Duration::from_millis(200)).await;

    framed.send(Frame::Data(Bytes::from("echo STILL_ALIVE\n"))).await.unwrap();

    let output = read_available_data(&mut framed, Duration::from_secs(2)).await;
    let output_str = String::from_utf8_lossy(&output);
    assert!(
        output_str.contains("STILL_ALIVE"),
        "server should survive control frames on session, got: {output_str}"
    );

    let _ = framed.send(Frame::Data(Bytes::from("exit\n"))).await;
    let _ = timeout(Duration::from_secs(3), server).await;
}

#[tokio::test]
async fn pty_buffer_saturation_and_resume() {
    let _permit = CONCURRENCY.acquire().await.unwrap();
    let (client_tx, mut framed, server, _meta) = setup_session().await;
    wait_for_shell(&mut framed).await;
    read_available_data(&mut framed, Duration::from_secs(1)).await;

    framed
        .send(Frame::Data(Bytes::from("{ sleep 0.3; seq 1 2000; echo PTY_DRAINED_OK; } &\n")))
        .await
        .unwrap();
    read_available_data(&mut framed, Duration::from_millis(200)).await;

    // Disconnect
    drop(framed);

    // Wait for output to start filling the PTY buffer
    tokio::time::sleep(Duration::from_secs(1)).await;

    // Reconnect via socketpair
    let (server_stream, client_stream) = UnixStream::pair().unwrap();
    client_tx.send(ClientConn::Active(Framed::new(server_stream, FrameCodec))).unwrap();
    let mut framed = Framed::new(client_stream, FrameCodec);
    framed.send(Frame::Resize { cols: 80, rows: 24 }).await.unwrap();

    let output = read_available_data(&mut framed, Duration::from_secs(3)).await;
    let output_str = String::from_utf8_lossy(&output);
    assert!(
        output_str.contains("PTY_DRAINED_OK"),
        "shell should resume after PTY buffer drain, got last 200 chars: {}",
        &output_str[output_str.len().saturating_sub(200)..]
    );

    let _ = framed.send(Frame::Data(Bytes::from("exit\n"))).await;
    let _ = timeout(Duration::from_secs(3), server).await;
}

#[tokio::test]
async fn pty_ring_buffer_drains_during_disconnect() {
    let _permit = CONCURRENCY.acquire().await.unwrap();
    let (client_tx, mut framed, server, _meta) = setup_session().await;
    wait_for_shell(&mut framed).await;
    read_available_data(&mut framed, Duration::from_secs(1)).await;

    // Start a background job that outputs more than 4KB (kernel PTY buffer)
    // but less than 1MB (ring buffer cap)
    framed
        .send(Frame::Data(Bytes::from("{ sleep 0.3; seq 1 5000; echo RING_BUF_OK; } &\n")))
        .await
        .unwrap();
    read_available_data(&mut framed, Duration::from_millis(200)).await;

    // Disconnect
    drop(framed);

    // Wait for output to complete (would stall without ring buffer)
    tokio::time::sleep(Duration::from_secs(3)).await;

    // Reconnect
    let (server_stream, client_stream) = UnixStream::pair().unwrap();
    client_tx.send(ClientConn::Active(Framed::new(server_stream, FrameCodec))).unwrap();
    let mut framed = Framed::new(client_stream, FrameCodec);
    framed.send(Frame::Resize { cols: 80, rows: 24 }).await.unwrap();

    // Should get buffered output including the marker
    let output = read_available_data(&mut framed, Duration::from_secs(3)).await;
    let output_str = String::from_utf8_lossy(&output);
    assert!(
        output_str.contains("RING_BUF_OK"),
        "ring buffer should have captured output during disconnect, got last 200 chars: {}",
        &output_str[output_str.len().saturating_sub(200)..]
    );

    let _ = framed.send(Frame::Data(Bytes::from("exit\n"))).await;
    let _ = timeout(Duration::from_secs(3), server).await;
}

#[tokio::test]
async fn pty_ring_buffer_caps_at_limit() {
    let _permit = CONCURRENCY.acquire().await.unwrap();
    let (client_tx, mut framed, server, _meta) = setup_session().await;
    wait_for_shell(&mut framed).await;
    read_available_data(&mut framed, Duration::from_secs(1)).await;

    // Generate ~2MB of output (exceeds 1MB ring buffer cap)
    framed
        .send(Frame::Data(Bytes::from(
            "{ sleep 0.3; dd if=/dev/zero bs=1024 count=2048 2>/dev/null | base64; echo CAP_TEST_DONE; } &\n",
        )))
        .await
        .unwrap();
    read_available_data(&mut framed, Duration::from_millis(200)).await;

    // Disconnect and wait for output to complete
    drop(framed);
    tokio::time::sleep(Duration::from_secs(5)).await;

    // Reconnect
    let (server_stream, client_stream) = UnixStream::pair().unwrap();
    client_tx.send(ClientConn::Active(Framed::new(server_stream, FrameCodec))).unwrap();
    let mut framed = Framed::new(client_stream, FrameCodec);
    framed.send(Frame::Resize { cols: 80, rows: 24 }).await.unwrap();

    // Should get the tail of the output (ring buffer dropped old data)
    let output = read_available_data(&mut framed, Duration::from_secs(3)).await;
    let output_str = String::from_utf8_lossy(&output);
    assert!(
        output_str.contains("CAP_TEST_DONE"),
        "tail of output should be preserved even when buffer overflows cap, got last 200 chars: {}",
        &output_str[output_str.len().saturating_sub(200)..]
    );
    // Verify we didn't get all 2MB+ (some was dropped)
    assert!(output.len() < 1_500_000, "ring buffer should cap output, got {} bytes", output.len());

    let _ = framed.send(Frame::Data(Bytes::from("exit\n"))).await;
    let _ = timeout(Duration::from_secs(3), server).await;
}

#[tokio::test]
async fn resize_propagates_to_pty() {
    let _permit = CONCURRENCY.acquire().await.unwrap();
    let (_tx, mut framed, server, _meta) = setup_session().await;
    wait_for_shell(&mut framed).await;
    read_available_data(&mut framed, Duration::from_secs(1)).await;

    framed.send(Frame::Resize { cols: 132, rows: 43 }).await.unwrap();
    tokio::time::sleep(Duration::from_millis(100)).await;

    framed.send(Frame::Data(Bytes::from("stty size\n"))).await.unwrap();
    let output = read_available_data(&mut framed, Duration::from_secs(2)).await;
    let output_str = String::from_utf8_lossy(&output);
    assert!(
        output_str.contains("43 132"),
        "PTY should reflect resize (43 rows, 132 cols), got: {output_str}"
    );

    let _ = framed.send(Frame::Data(Bytes::from("exit\n"))).await;
    let _ = timeout(Duration::from_secs(3), server).await;
}

#[tokio::test]
async fn metadata_reflects_attached_state() {
    let _permit = CONCURRENCY.acquire().await.unwrap();
    let (client_tx, mut framed, server, meta) = setup_session().await;

    // Wait for metadata to be set
    tokio::time::sleep(Duration::from_millis(300)).await;
    let m = meta.get().expect("metadata should be set after server starts");

    // The first client was already sent via setup_session, so attached should be true after shell starts
    wait_for_shell(&mut framed).await;
    read_available_data(&mut framed, Duration::from_millis(500)).await;
    assert!(
        m.attached.load(std::sync::atomic::Ordering::Relaxed),
        "should be attached after client connects"
    );

    // Disconnect
    drop(framed);
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert!(
        !m.attached.load(std::sync::atomic::Ordering::Relaxed),
        "should not be attached after client disconnects"
    );

    // Reconnect and clean up
    let (server_stream, client_stream) = UnixStream::pair().unwrap();
    client_tx.send(ClientConn::Active(Framed::new(server_stream, FrameCodec))).unwrap();
    let mut framed = Framed::new(client_stream, FrameCodec);
    framed.send(Frame::Resize { cols: 80, rows: 24 }).await.unwrap();
    let _ = framed.send(Frame::Data(Bytes::from("exit\n"))).await;
    let _ = timeout(Duration::from_secs(3), server).await;
}

#[tokio::test]
async fn client_explicit_exit_frame() {
    let _permit = CONCURRENCY.acquire().await.unwrap();
    let (client_tx, mut framed, server, _meta) = setup_session().await;
    wait_for_shell(&mut framed).await;
    read_available_data(&mut framed, Duration::from_secs(1)).await;

    // Send Exit frame from client side
    framed.send(Frame::Exit { code: 0 }).await.unwrap();

    // Give server time to notice
    tokio::time::sleep(Duration::from_millis(300)).await;

    // Server should still be running (it treats Exit as client disconnect).
    // Reconnect to verify shell is alive.
    let (server_stream, client_stream) = UnixStream::pair().unwrap();
    client_tx.send(ClientConn::Active(Framed::new(server_stream, FrameCodec))).unwrap();
    let mut framed = Framed::new(client_stream, FrameCodec);
    framed.send(Frame::Resize { cols: 80, rows: 24 }).await.unwrap();
    read_available_data(&mut framed, Duration::from_secs(1)).await;

    framed.send(Frame::Data(Bytes::from("echo EXIT_FRAME_OK\n"))).await.unwrap();
    let output = read_available_data(&mut framed, Duration::from_secs(2)).await;
    let output_str = String::from_utf8_lossy(&output);
    assert!(
        output_str.contains("EXIT_FRAME_OK"),
        "shell should survive client Exit frame, got: {output_str}"
    );

    let _ = framed.send(Frame::Data(Bytes::from("exit\n"))).await;
    let _ = timeout(Duration::from_secs(3), server).await;
}

#[tokio::test]
async fn high_throughput_data_relay() {
    let _permit = CONCURRENCY.acquire().await.unwrap();
    let (_tx, mut framed, server, _meta) = setup_session().await;
    wait_for_shell(&mut framed).await;
    read_available_data(&mut framed, Duration::from_secs(1)).await;

    framed
        .send(Frame::Data(Bytes::from(
            "head -c 2000000 /dev/urandom | base64; echo THROUGHPUT_DONE\n",
        )))
        .await
        .unwrap();

    let mut total = Vec::new();
    let deadline = tokio::time::Instant::now() + Duration::from_secs(30);
    loop {
        match timeout(Duration::from_secs(2), framed.next()).await {
            Ok(Some(Ok(Frame::Data(data)))) => total.extend_from_slice(&data),
            _ => {
                if tokio::time::Instant::now() >= deadline {
                    break;
                }
                let s = String::from_utf8_lossy(&total);
                if s.contains("THROUGHPUT_DONE") {
                    break;
                }
                continue;
            }
        }
    }

    let output_str = String::from_utf8_lossy(&total);
    assert!(
        output_str.contains("THROUGHPUT_DONE"),
        "expected throughput marker, got {} bytes (last 100: {})",
        total.len(),
        &output_str[output_str.len().saturating_sub(100)..],
    );

    assert!(total.len() > 1_000_000, "expected >1MB of output, got {} bytes", total.len(),);

    let _ = framed.send(Frame::Data(Bytes::from("exit\n"))).await;
    let _ = timeout(Duration::from_secs(3), server).await;
}

#[tokio::test]
async fn ping_pong_heartbeat() {
    let _permit = CONCURRENCY.acquire().await.unwrap();
    let (_tx, mut framed, server, meta) = setup_session().await;
    wait_for_shell(&mut framed).await;
    read_available_data(&mut framed, Duration::from_secs(1)).await;

    // Send Ping, expect Pong back
    framed.send(Frame::Ping).await.unwrap();
    let mut got_pong = false;
    loop {
        match timeout(Duration::from_secs(3), framed.next()).await {
            Ok(Some(Ok(Frame::Pong))) => {
                got_pong = true;
                break;
            }
            Ok(Some(Ok(Frame::Data(_)))) => continue,
            _ => break,
        }
    }
    assert!(got_pong, "server should reply with Pong to Ping");

    // Verify last_heartbeat was updated in metadata
    tokio::time::sleep(Duration::from_millis(100)).await;
    let m = meta.get().expect("metadata should be set");
    let hb = m.last_heartbeat.load(std::sync::atomic::Ordering::Relaxed);
    assert!(hb > 0, "last_heartbeat should be updated after Ping");

    let _ = framed.send(Frame::Data(Bytes::from("exit\n"))).await;
    let _ = timeout(Duration::from_secs(3), server).await;
}

#[tokio::test]
async fn env_vars_forwarded() {
    let _permit = CONCURRENCY.acquire().await.unwrap();
    let (_tx, mut framed, server, _meta) =
        setup_session_with_env(vec![("TERM".to_string(), "xterm-test-42".to_string())]).await;
    wait_for_shell(&mut framed).await;
    read_available_data(&mut framed, Duration::from_secs(1)).await;

    framed.send(Frame::Data(Bytes::from("echo $TERM\n"))).await.unwrap();

    let output = read_available_data(&mut framed, Duration::from_secs(2)).await;
    let output_str = String::from_utf8_lossy(&output);
    assert!(
        output_str.contains("xterm-test-42"),
        "expected forwarded TERM in output, got: {output_str}"
    );

    let _ = framed.send(Frame::Data(Bytes::from("exit\n"))).await;
    let _ = timeout(Duration::from_secs(3), server).await;
}

#[tokio::test]
async fn disallowed_env_vars_rejected() {
    let _permit = CONCURRENCY.acquire().await.unwrap();
    let (_tx, mut framed, server, _meta) = setup_session_with_env(vec![
        ("TERM".to_string(), "xterm-test-env".to_string()),
        ("LD_PRELOAD".to_string(), "/tmp/evil.so".to_string()),
    ])
    .await;
    wait_for_shell(&mut framed).await;
    read_available_data(&mut framed, Duration::from_secs(1)).await;

    // TERM should be forwarded (allowed)
    framed.send(Frame::Data(Bytes::from("echo TERM=$TERM\n"))).await.unwrap();
    let output = read_available_data(&mut framed, Duration::from_secs(2)).await;
    let output_str = String::from_utf8_lossy(&output);
    assert!(
        output_str.contains("TERM=xterm-test-env"),
        "expected TERM to be forwarded, got: {output_str}"
    );

    // LD_PRELOAD should NOT be forwarded (disallowed)
    framed.send(Frame::Data(Bytes::from("echo LD_PRELOAD=${LD_PRELOAD:-unset}\n"))).await.unwrap();
    let output = read_available_data(&mut framed, Duration::from_secs(2)).await;
    let output_str = String::from_utf8_lossy(&output);
    assert!(
        output_str.contains("LD_PRELOAD=unset"),
        "expected LD_PRELOAD to be unset, got: {output_str}"
    );

    let _ = framed.send(Frame::Data(Bytes::from("exit\n"))).await;
    let _ = timeout(Duration::from_secs(3), server).await;
}

#[tokio::test]
async fn login_shell_starts_in_home() {
    let _permit = CONCURRENCY.acquire().await.unwrap();
    let (_tx, mut framed, server, _meta) = setup_session().await;
    wait_for_shell(&mut framed).await;
    read_available_data(&mut framed, Duration::from_secs(1)).await;

    framed.send(Frame::Data(Bytes::from("pwd\n"))).await.unwrap();

    let output = read_available_data(&mut framed, Duration::from_secs(2)).await;
    let output_str = String::from_utf8_lossy(&output);
    let home = std::env::var("HOME").unwrap();
    assert!(output_str.contains(&home), "expected CWD to be $HOME ({home}), got: {output_str}");

    let _ = framed.send(Frame::Data(Bytes::from("exit\n"))).await;
    let _ = timeout(Duration::from_secs(3), server).await;
}

/// Like setup_session but returns the agent socket path for agent forwarding tests.
async fn setup_session_with_agent_path() -> (
    mpsc::UnboundedSender<ClientConn>,
    Framed<UnixStream, FrameCodec>,
    JoinHandle<anyhow::Result<()>>,
    Arc<OnceLock<gritty::server::SessionMetadata>>,
    PathBuf,
) {
    let (client_tx, client_rx) = mpsc::unbounded_channel();
    let meta = Arc::new(OnceLock::new());
    let meta_clone = Arc::clone(&meta);
    let agent_path = unique_agent_socket_path();
    let agent_path_clone = agent_path.clone();
    let svc_path = unique_svc_socket_path();
    let handle = tokio::spawn(async move {
        gritty::server::run(client_rx, meta_clone, agent_path_clone, svc_path).await
    });

    let (server_stream, client_stream) = UnixStream::pair().unwrap();
    client_tx.send(ClientConn::Active(Framed::new(server_stream, FrameCodec))).unwrap();

    let mut framed = Framed::new(client_stream, FrameCodec);
    framed.send(Frame::Env { vars: vec![] }).await.unwrap();

    (client_tx, framed, handle, meta, agent_path)
}

#[tokio::test]
async fn agent_forwarding_data_roundtrip() {
    let _permit = CONCURRENCY.acquire().await.unwrap();
    let (_tx, mut framed, server, _meta, agent_path) = setup_session_with_agent_path().await;
    wait_for_shell(&mut framed).await;
    read_available_data(&mut framed, Duration::from_secs(1)).await;

    // Client sends AgentForward to enable forwarding
    framed.send(Frame::AgentForward).await.unwrap();
    tokio::time::sleep(Duration::from_millis(100)).await;

    // Connect to agent socket (simulating a process inside the session using SSH_AUTH_SOCK)
    let mut agent_conn = UnixStream::connect(&agent_path).await.unwrap();
    tokio::time::sleep(Duration::from_millis(100)).await;

    // Server should send AgentOpen
    let cid = loop {
        match timeout(Duration::from_secs(3), framed.next()).await {
            Ok(Some(Ok(Frame::AgentOpen { channel_id }))) => break channel_id,
            Ok(Some(Ok(Frame::Data(_)))) => continue,
            other => panic!("expected AgentOpen, got: {other:?}"),
        }
    };

    // Server should send AgentData with whatever the remote process wrote
    agent_conn.write_all(b"hello-agent").await.unwrap();
    tokio::time::sleep(Duration::from_millis(100)).await;

    loop {
        match timeout(Duration::from_secs(3), framed.next()).await {
            Ok(Some(Ok(Frame::AgentData { channel_id, data }))) => {
                assert_eq!(channel_id, cid);
                assert_eq!(&data[..], b"hello-agent");
                break;
            }
            Ok(Some(Ok(Frame::Data(_)))) => continue,
            other => panic!("expected AgentData, got: {other:?}"),
        }
    }

    // Client sends AgentData back (simulating a response from local agent)
    framed
        .send(Frame::AgentData { channel_id: cid, data: Bytes::from("agent-response") })
        .await
        .unwrap();
    tokio::time::sleep(Duration::from_millis(100)).await;

    // The remote agent connection should receive the data
    let mut response_buf = vec![0u8; 64];
    let n =
        timeout(Duration::from_secs(3), agent_conn.read(&mut response_buf)).await.unwrap().unwrap();
    assert_eq!(&response_buf[..n], b"agent-response");

    // Clean up
    drop(agent_conn);
    let _ = framed.send(Frame::Data(Bytes::from("exit\n"))).await;
    let _ = timeout(Duration::from_secs(3), server).await;
}

#[tokio::test]
async fn agent_close_on_remote_disconnect() {
    let _permit = CONCURRENCY.acquire().await.unwrap();
    let (_tx, mut framed, server, _meta, agent_path) = setup_session_with_agent_path().await;
    wait_for_shell(&mut framed).await;
    read_available_data(&mut framed, Duration::from_secs(1)).await;

    // Enable agent forwarding
    framed.send(Frame::AgentForward).await.unwrap();
    tokio::time::sleep(Duration::from_millis(100)).await;

    // Connect to agent socket
    let agent_conn = UnixStream::connect(&agent_path).await.unwrap();
    tokio::time::sleep(Duration::from_millis(100)).await;

    // Consume AgentOpen
    let cid = loop {
        match timeout(Duration::from_secs(3), framed.next()).await {
            Ok(Some(Ok(Frame::AgentOpen { channel_id }))) => break channel_id,
            Ok(Some(Ok(Frame::Data(_)))) => continue,
            other => panic!("expected AgentOpen, got: {other:?}"),
        }
    };

    // Drop the agent connection — server should send AgentClose
    drop(agent_conn);
    tokio::time::sleep(Duration::from_millis(200)).await;

    let mut got_close = false;
    loop {
        match timeout(Duration::from_secs(3), framed.next()).await {
            Ok(Some(Ok(Frame::AgentClose { channel_id }))) => {
                assert_eq!(channel_id, cid);
                got_close = true;
                break;
            }
            Ok(Some(Ok(Frame::Data(_)))) => continue,
            _ => break,
        }
    }
    assert!(got_close, "should receive AgentClose when remote agent connection closes");

    let _ = framed.send(Frame::Data(Bytes::from("exit\n"))).await;
    let _ = timeout(Duration::from_secs(3), server).await;
}

#[tokio::test]
async fn agent_not_forwarded_without_flag() {
    let _permit = CONCURRENCY.acquire().await.unwrap();
    let (_tx, mut framed, server, _meta, agent_path) = setup_session_with_agent_path().await;
    wait_for_shell(&mut framed).await;
    read_available_data(&mut framed, Duration::from_secs(1)).await;

    // Do NOT send AgentForward — agent socket file should not exist.
    assert!(!agent_path.exists(), "agent socket should not exist without AgentForward");

    // Connecting should fail
    assert!(
        UnixStream::connect(&agent_path).await.is_err(),
        "connect to agent socket should fail without AgentForward"
    );

    let _ = framed.send(Frame::Data(Bytes::from("exit\n"))).await;
    let _ = timeout(Duration::from_secs(3), server).await;
}

#[tokio::test]
async fn open_forwarding_url_roundtrip() {
    let _permit = CONCURRENCY.acquire().await.unwrap();
    let (_tx, mut framed, server, _meta, svc_path) = setup_session_with_svc_path().await;
    wait_for_shell(&mut framed).await;
    read_available_data(&mut framed, Duration::from_secs(1)).await;

    // Client sends OpenForward to enable forwarding
    framed.send(Frame::OpenForward).await.unwrap();
    tokio::time::sleep(Duration::from_millis(100)).await;

    // Connect to svc socket (simulating `gritty open` inside the session)
    let mut open_conn = UnixStream::connect(&svc_path).await.unwrap();
    open_conn.write_all(&[gritty::protocol::SvcRequest::OpenUrl.to_byte()]).await.unwrap();
    open_conn.write_all(b"https://example.com\n").await.unwrap();
    drop(open_conn);
    tokio::time::sleep(Duration::from_millis(200)).await;

    // Server should send OpenUrl to client
    loop {
        match timeout(Duration::from_secs(3), framed.next()).await {
            Ok(Some(Ok(Frame::OpenUrl { url }))) => {
                assert_eq!(url, "https://example.com");
                break;
            }
            Ok(Some(Ok(Frame::Data(_)))) => continue,
            other => panic!("expected OpenUrl, got: {other:?}"),
        }
    }

    let _ = framed.send(Frame::Data(Bytes::from("exit\n"))).await;
    let _ = timeout(Duration::from_secs(3), server).await;
}

#[tokio::test]
async fn open_forwarding_not_enabled() {
    let _permit = CONCURRENCY.acquire().await.unwrap();
    let (_tx, mut framed, server, _meta, svc_path) = setup_session_with_svc_path().await;
    wait_for_shell(&mut framed).await;
    read_available_data(&mut framed, Duration::from_secs(1)).await;

    // Wait for svc socket to be bound
    for _ in 0..50 {
        if svc_path.exists() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }

    // Svc socket IS always bound, but without OpenForward, URLs are silently ignored.
    let mut open_conn = UnixStream::connect(&svc_path).await.unwrap();
    open_conn.write_all(&[gritty::protocol::SvcRequest::OpenUrl.to_byte()]).await.unwrap();
    open_conn.write_all(b"https://example.com\n").await.unwrap();
    drop(open_conn);
    tokio::time::sleep(Duration::from_millis(200)).await;

    // Should NOT receive OpenUrl (forwarding disabled)
    match timeout(Duration::from_millis(500), framed.next()).await {
        Ok(Some(Ok(Frame::OpenUrl { .. }))) => {
            panic!("should not receive OpenUrl without OpenForward")
        }
        Ok(Some(Ok(Frame::Data(_)))) => {}
        _ => {}
    }

    let _ = framed.send(Frame::Data(Bytes::from("exit\n"))).await;
    let _ = timeout(Duration::from_secs(3), server).await;
}

#[tokio::test]
async fn ping_pong_response_is_fast() {
    let _permit = CONCURRENCY.acquire().await.unwrap();
    let (_tx, mut framed, server, _meta) = setup_session().await;
    wait_for_shell(&mut framed).await;
    read_available_data(&mut framed, Duration::from_secs(1)).await;

    let start = std::time::Instant::now();
    framed.send(Frame::Ping).await.unwrap();

    loop {
        match timeout(Duration::from_secs(3), framed.next()).await {
            Ok(Some(Ok(Frame::Pong))) => break,
            Ok(Some(Ok(Frame::Data(_)))) => continue,
            other => panic!("expected Pong, got: {other:?}"),
        }
    }

    let elapsed = start.elapsed();
    assert!(elapsed < Duration::from_secs(1), "Pong should arrive in <1s, took {elapsed:?}");

    let _ = framed.send(Frame::Data(Bytes::from("exit\n"))).await;
    let _ = timeout(Duration::from_secs(3), server).await;
}

#[tokio::test]
async fn multiple_pings_get_multiple_pongs() {
    let _permit = CONCURRENCY.acquire().await.unwrap();
    let (_tx, mut framed, server, _meta) = setup_session().await;
    wait_for_shell(&mut framed).await;
    read_available_data(&mut framed, Duration::from_secs(1)).await;

    // Send 3 Pings rapidly
    for _ in 0..3 {
        framed.send(Frame::Ping).await.unwrap();
    }

    // Collect exactly 3 Pongs
    let mut pong_count = 0;
    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    while pong_count < 3 {
        match timeout(Duration::from_secs(3), framed.next()).await {
            Ok(Some(Ok(Frame::Pong))) => pong_count += 1,
            Ok(Some(Ok(Frame::Data(_)))) => continue,
            other => panic!("expected Pong #{}, got: {other:?}", pong_count + 1),
        }
        if tokio::time::Instant::now() >= deadline {
            break;
        }
    }

    assert_eq!(pong_count, 3, "expected 3 Pongs for 3 Pings");

    let _ = framed.send(Frame::Data(Bytes::from("exit\n"))).await;
    let _ = timeout(Duration::from_secs(3), server).await;
}

#[tokio::test]
async fn ring_buffer_overflow_shows_truncation_marker() {
    let _permit = CONCURRENCY.acquire().await.unwrap();
    let (client_tx, mut framed, server, _meta) = setup_session().await;
    wait_for_shell(&mut framed).await;
    read_available_data(&mut framed, Duration::from_secs(1)).await;

    // Generate ~2MB of output (exceeds 1MB ring buffer cap)
    framed
        .send(Frame::Data(Bytes::from(
            "{ sleep 0.3; dd if=/dev/zero bs=1024 count=2048 2>/dev/null | base64; echo TRUNC_DONE; } &\n",
        )))
        .await
        .unwrap();
    read_available_data(&mut framed, Duration::from_millis(200)).await;

    // Disconnect and wait for output to complete
    drop(framed);
    tokio::time::sleep(Duration::from_secs(5)).await;

    // Reconnect
    let (server_stream, client_stream) = UnixStream::pair().unwrap();
    client_tx.send(ClientConn::Active(Framed::new(server_stream, FrameCodec))).unwrap();
    let mut framed = Framed::new(client_stream, FrameCodec);
    framed.send(Frame::Resize { cols: 80, rows: 24 }).await.unwrap();

    // First Data frame should contain the truncation marker
    let output = read_available_data(&mut framed, Duration::from_secs(3)).await;
    let output_str = String::from_utf8_lossy(&output);
    assert!(
        output_str.contains("bytes of output dropped"),
        "reconnect should show truncation marker when ring buffer overflows, got first 200 chars: {}",
        &output_str[..output_str.len().min(200)]
    );
    assert!(output_str.contains("TRUNC_DONE"), "tail of output should still be preserved");

    let _ = framed.send(Frame::Data(Bytes::from("exit\n"))).await;
    let _ = timeout(Duration::from_secs(3), server).await;
}

#[tokio::test]
async fn tail_receives_output() {
    let _permit = CONCURRENCY.acquire().await.unwrap();
    let (client_tx, mut framed, server, _meta) = setup_session().await;
    wait_for_shell(&mut framed).await;
    read_available_data(&mut framed, Duration::from_secs(1)).await;

    // Connect a tail client via channel
    let (server_stream, client_stream) = UnixStream::pair().unwrap();
    client_tx.send(ClientConn::Tail(Framed::new(server_stream, FrameCodec))).unwrap();
    let mut tail = Framed::new(client_stream, FrameCodec);

    tokio::time::sleep(Duration::from_millis(200)).await;

    // Send command via active client
    framed.send(Frame::Data(Bytes::from("echo TAIL_TEST_OK\n"))).await.unwrap();

    // Tail client should see the output
    let mut output = Vec::new();
    while let Ok(Some(Ok(Frame::Data(data)))) = timeout(Duration::from_secs(3), tail.next()).await {
        output.extend_from_slice(&data);
        if String::from_utf8_lossy(&output).contains("TAIL_TEST_OK") {
            break;
        }
    }
    let output_str = String::from_utf8_lossy(&output);
    assert!(
        output_str.contains("TAIL_TEST_OK"),
        "tail client should receive PTY output, got: {output_str}"
    );

    let _ = framed.send(Frame::Data(Bytes::from("exit\n"))).await;
    let _ = timeout(Duration::from_secs(3), server).await;
}

#[tokio::test]
async fn tail_does_not_detach_active_client() {
    let _permit = CONCURRENCY.acquire().await.unwrap();
    let (client_tx, mut framed, server, _meta) = setup_session().await;
    wait_for_shell(&mut framed).await;
    read_available_data(&mut framed, Duration::from_secs(1)).await;

    // Connect a tail client
    let (server_stream, client_stream) = UnixStream::pair().unwrap();
    client_tx.send(ClientConn::Tail(Framed::new(server_stream, FrameCodec))).unwrap();
    let mut _tail = Framed::new(client_stream, FrameCodec);

    tokio::time::sleep(Duration::from_millis(200)).await;

    // Active client should still be connected (no Detached frame)
    framed.send(Frame::Data(Bytes::from("echo STILL_ACTIVE\n"))).await.unwrap();
    let output = read_available_data(&mut framed, Duration::from_secs(2)).await;
    let output_str = String::from_utf8_lossy(&output);
    assert!(
        output_str.contains("STILL_ACTIVE"),
        "active client should not be detached by tail, got: {output_str}"
    );

    let _ = framed.send(Frame::Data(Bytes::from("exit\n"))).await;
    let _ = timeout(Duration::from_secs(3), server).await;
}

#[tokio::test]
async fn tail_receives_exit_on_shell_exit() {
    let _permit = CONCURRENCY.acquire().await.unwrap();
    let (client_tx, mut framed, _server, _meta) = setup_session().await;
    wait_for_shell(&mut framed).await;
    read_available_data(&mut framed, Duration::from_secs(1)).await;

    // Connect a tail client
    let (server_stream, client_stream) = UnixStream::pair().unwrap();
    client_tx.send(ClientConn::Tail(Framed::new(server_stream, FrameCodec))).unwrap();
    let mut tail = Framed::new(client_stream, FrameCodec);

    tokio::time::sleep(Duration::from_millis(200)).await;

    // Exit the shell
    framed.send(Frame::Data(Bytes::from("exit 42\n"))).await.unwrap();

    // Tail client should receive Exit frame
    let code = expect_exit_frame(&mut tail, Duration::from_secs(5)).await;
    assert_eq!(code, Some(42), "tail should receive exit code");
}

#[tokio::test]
async fn tunnel_forwarding_roundtrip() {
    let _permit = CONCURRENCY.acquire().await.unwrap();
    let (_tx, mut framed, server, _meta, svc_path) = setup_session_with_svc_path().await;
    wait_for_shell(&mut framed).await;
    read_available_data(&mut framed, Duration::from_secs(1)).await;

    // Bind a TCP listener simulating the remote program waiting for OAuth callback
    let callback_listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let port = callback_listener.local_addr().unwrap().port();
    callback_listener.set_nonblocking(true).ok();
    let callback_listener = tokio::net::TcpListener::from_std(callback_listener).unwrap();

    // Enable open forwarding
    framed.send(Frame::OpenForward).await.unwrap();
    tokio::time::sleep(Duration::from_millis(100)).await;

    // Send URL with redirect_uri pointing to the callback port
    let url = format!(
        "https://accounts.example.com/auth?redirect_uri=http://localhost:{port}/callback&client_id=test"
    );
    let mut open_conn = UnixStream::connect(&svc_path).await.unwrap();
    open_conn.write_all(&[gritty::protocol::SvcRequest::OpenUrl.to_byte()]).await.unwrap();
    open_conn.write_all(format!("{url}\n").as_bytes()).await.unwrap();
    drop(open_conn);

    // Should receive TunnelListen then OpenUrl
    let mut got_tunnel_listen = false;
    loop {
        match timeout(Duration::from_secs(5), framed.next()).await {
            Ok(Some(Ok(Frame::TunnelListen { port: p }))) => {
                assert_eq!(p, port);
                got_tunnel_listen = true;
            }
            Ok(Some(Ok(Frame::OpenUrl { url: u }))) => {
                assert!(u.contains("accounts.example.com"));
                break;
            }
            Ok(Some(Ok(Frame::Data(_)))) => continue,
            other => panic!("expected TunnelListen/OpenUrl, got: {other:?}"),
        }
    }
    assert!(got_tunnel_listen, "should have received TunnelListen before OpenUrl");

    // Send TunnelOpen (simulating client accepted a connection)
    framed.send(Frame::TunnelOpen).await.unwrap();
    tokio::time::sleep(Duration::from_millis(100)).await;

    // Accept connection on the callback listener (simulating the remote program)
    let (mut callback_conn, _) = callback_listener.accept().await.unwrap();

    // Send data from client -> server -> callback program
    let request = b"GET /callback?code=abc123 HTTP/1.1\r\n\r\n";
    framed.send(Frame::TunnelData(Bytes::from_static(request))).await.unwrap();

    // Read it from the callback connection
    let mut buf = vec![0u8; 4096];
    let n = timeout(Duration::from_secs(3), callback_conn.read(&mut buf)).await.unwrap().unwrap();
    assert_eq!(&buf[..n], request);

    // Send response back: callback program -> server -> client
    let response = b"HTTP/1.1 200 OK\r\n\r\nSuccess";
    callback_conn.write_all(response).await.unwrap();

    // Read TunnelData back from the server
    loop {
        match timeout(Duration::from_secs(3), framed.next()).await {
            Ok(Some(Ok(Frame::TunnelData(data)))) => {
                assert_eq!(&data[..], response);
                break;
            }
            Ok(Some(Ok(Frame::Data(_)))) => continue,
            other => panic!("expected TunnelData, got: {other:?}"),
        }
    }

    // Drop the callback connection, should get TunnelClose
    drop(callback_conn);
    loop {
        match timeout(Duration::from_secs(3), framed.next()).await {
            Ok(Some(Ok(Frame::TunnelClose))) => break,
            Ok(Some(Ok(Frame::Data(_) | Frame::TunnelData(_)))) => continue,
            other => panic!("expected TunnelClose, got: {other:?}"),
        }
    }

    let _ = framed.send(Frame::Data(Bytes::from("exit\n"))).await;
    let _ = timeout(Duration::from_secs(3), server).await;
}

#[tokio::test]
async fn tunnel_not_created_without_redirect_uri() {
    let _permit = CONCURRENCY.acquire().await.unwrap();
    let (_tx, mut framed, server, _meta, svc_path) = setup_session_with_svc_path().await;
    wait_for_shell(&mut framed).await;
    read_available_data(&mut framed, Duration::from_secs(1)).await;

    // Enable open forwarding
    framed.send(Frame::OpenForward).await.unwrap();
    tokio::time::sleep(Duration::from_millis(100)).await;

    // Send URL without redirect_uri
    let mut open_conn = UnixStream::connect(&svc_path).await.unwrap();
    open_conn.write_all(&[gritty::protocol::SvcRequest::OpenUrl.to_byte()]).await.unwrap();
    open_conn.write_all(b"https://example.com/page\n").await.unwrap();
    drop(open_conn);

    // Should receive only OpenUrl, no TunnelListen
    loop {
        match timeout(Duration::from_secs(3), framed.next()).await {
            Ok(Some(Ok(Frame::OpenUrl { url }))) => {
                assert_eq!(url, "https://example.com/page");
                break;
            }
            Ok(Some(Ok(Frame::TunnelListen { .. }))) => {
                panic!("should not receive TunnelListen without redirect_uri");
            }
            Ok(Some(Ok(Frame::Data(_)))) => continue,
            other => panic!("expected OpenUrl, got: {other:?}"),
        }
    }

    let _ = framed.send(Frame::Data(Bytes::from("exit\n"))).await;
    let _ = timeout(Duration::from_secs(3), server).await;
}

#[tokio::test]
async fn tunnel_not_created_when_port_not_listening() {
    let _permit = CONCURRENCY.acquire().await.unwrap();
    let (_tx, mut framed, server, _meta, svc_path) = setup_session_with_svc_path().await;
    wait_for_shell(&mut framed).await;
    read_available_data(&mut framed, Duration::from_secs(1)).await;

    // Enable open forwarding
    framed.send(Frame::OpenForward).await.unwrap();
    tokio::time::sleep(Duration::from_millis(100)).await;

    // Find a port that's NOT in use by binding and immediately dropping
    let temp_listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let unused_port = temp_listener.local_addr().unwrap().port();
    drop(temp_listener);

    // Send URL with redirect_uri pointing to the unused port
    let url = format!(
        "https://auth.example.com/authorize?redirect_uri=http://localhost:{unused_port}/callback"
    );
    let mut open_conn = UnixStream::connect(&svc_path).await.unwrap();
    open_conn.write_all(&[gritty::protocol::SvcRequest::OpenUrl.to_byte()]).await.unwrap();
    open_conn.write_all(format!("{url}\n").as_bytes()).await.unwrap();
    drop(open_conn);

    // Should receive only OpenUrl, no TunnelListen (nothing listening on the port)
    loop {
        match timeout(Duration::from_secs(3), framed.next()).await {
            Ok(Some(Ok(Frame::OpenUrl { url: u }))) => {
                assert!(u.contains("auth.example.com"));
                break;
            }
            Ok(Some(Ok(Frame::TunnelListen { .. }))) => {
                panic!("should not receive TunnelListen when port is not in use");
            }
            Ok(Some(Ok(Frame::Data(_)))) => continue,
            other => panic!("expected OpenUrl, got: {other:?}"),
        }
    }

    let _ = framed.send(Frame::Data(Bytes::from("exit\n"))).await;
    let _ = timeout(Duration::from_secs(3), server).await;
}

// =============================================================================
// File transfer tests
// =============================================================================

/// Helper: connect as sender, write manifest, wait for go signal, stream files.
async fn send_files(svc_path: &std::path::Path, files: &[(&str, &[u8])]) {
    let mut stream = UnixStream::connect(svc_path).await.unwrap();

    // SvcRequest discriminator
    stream.write_all(&[gritty::protocol::SvcRequest::Send.to_byte()]).await.unwrap();

    // Manifest
    let file_count = files.len() as u32;
    stream.write_all(&file_count.to_be_bytes()).await.unwrap();
    for (name, data) in files {
        let name_bytes = name.as_bytes();
        stream.write_all(&(name_bytes.len() as u16).to_be_bytes()).await.unwrap();
        stream.write_all(name_bytes).await.unwrap();
        stream.write_all(&(data.len() as u64).to_be_bytes()).await.unwrap();
    }

    // Wait for go signal
    let mut go = [0u8; 1];
    stream.read_exact(&mut go).await.unwrap();
    assert_eq!(go[0], 0x01);

    // Stream file data
    for (_name, data) in files {
        stream.write_all(data).await.unwrap();
    }
}

/// Helper: connect as receiver, read files, return Vec<(name, data)>.
async fn receive_files(svc_path: &std::path::Path) -> Vec<(String, Vec<u8>)> {
    let mut stream = UnixStream::connect(svc_path).await.unwrap();

    // SvcRequest discriminator
    stream.write_all(&[gritty::protocol::SvcRequest::Receive.to_byte()]).await.unwrap();

    // Dest dir (empty = cwd)
    stream.write_all(b"\n").await.unwrap();

    // Read file_count
    let mut buf4 = [0u8; 4];
    stream.read_exact(&mut buf4).await.unwrap();
    let file_count = u32::from_be_bytes(buf4);

    let mut result = Vec::new();
    loop {
        let mut buf2 = [0u8; 2];
        stream.read_exact(&mut buf2).await.unwrap();
        let name_len = u16::from_be_bytes(buf2) as usize;
        if name_len == 0 {
            break;
        }
        let mut name_buf = vec![0u8; name_len];
        stream.read_exact(&mut name_buf).await.unwrap();
        let name = String::from_utf8(name_buf).unwrap();

        let mut buf8 = [0u8; 8];
        stream.read_exact(&mut buf8).await.unwrap();
        let file_size = u64::from_be_bytes(buf8);

        let mut data = vec![0u8; file_size as usize];
        stream.read_exact(&mut data).await.unwrap();
        result.push((name, data));
    }
    assert_eq!(result.len(), file_count as usize);
    result
}

#[tokio::test]
async fn send_receive_single_file() {
    let _permit = CONCURRENCY.acquire().await.unwrap();
    let (_client_tx, mut framed, server, _meta, svc_path) = setup_session_with_svc_path().await;
    wait_for_shell(&mut framed).await;

    // Wait for send socket to be bound
    for _ in 0..50 {
        if svc_path.exists() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    assert!(svc_path.exists(), "send socket not bound");

    let file_data = b"hello world\n";

    // Spawn sender and receiver concurrently (sender first)
    let sp = svc_path.clone();
    let sender = tokio::spawn(async move {
        send_files(&sp, &[("test.txt", file_data)]).await;
    });
    // Small delay so sender arrives first
    tokio::time::sleep(Duration::from_millis(50)).await;
    let sp = svc_path.clone();
    let receiver = tokio::spawn(async move { receive_files(&sp).await });

    let (send_result, recv_result) = tokio::join!(sender, receiver);
    send_result.unwrap();
    let files = recv_result.unwrap();

    assert_eq!(files.len(), 1);
    assert_eq!(files[0].0, "test.txt");
    assert_eq!(files[0].1, file_data);

    // Check for SendOffer + SendDone notification frames to the active client
    let mut got_offer = false;
    let mut got_done = false;
    loop {
        match timeout(Duration::from_secs(3), framed.next()).await {
            Ok(Some(Ok(Frame::SendOffer { file_count, total_bytes }))) => {
                assert_eq!(file_count, 1);
                assert_eq!(total_bytes, file_data.len() as u64);
                got_offer = true;
            }
            Ok(Some(Ok(Frame::SendDone))) => {
                got_done = true;
                break;
            }
            Ok(Some(Ok(Frame::Data(_)))) => continue,
            Ok(Some(Ok(other))) => panic!("unexpected frame: {other:?}"),
            _ => break,
        }
    }
    assert!(got_offer, "did not receive SendOffer");
    assert!(got_done, "did not receive SendDone");

    let _ = framed.send(Frame::Data(Bytes::from("exit\n"))).await;
    let _ = timeout(Duration::from_secs(3), server).await;
}

#[tokio::test]
async fn send_receive_multiple_files() {
    let _permit = CONCURRENCY.acquire().await.unwrap();
    let (_client_tx, mut framed, server, _meta, svc_path) = setup_session_with_svc_path().await;
    wait_for_shell(&mut framed).await;

    for _ in 0..50 {
        if svc_path.exists() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }

    let files_to_send: Vec<(&str, &[u8])> = vec![
        ("a.txt", b"alpha"),
        ("b.bin", &[0u8, 1, 2, 3, 4, 5]),
        ("c.txt", b"gamma delta epsilon"),
    ];

    let sp = svc_path.clone();
    let f = files_to_send.clone();
    let sender = tokio::spawn(async move {
        send_files(&sp, &f).await;
    });
    tokio::time::sleep(Duration::from_millis(50)).await;
    let sp = svc_path.clone();
    let receiver = tokio::spawn(async move { receive_files(&sp).await });

    let (s, r) = tokio::join!(sender, receiver);
    s.unwrap();
    let received = r.unwrap();

    assert_eq!(received.len(), 3);
    assert_eq!(received[0].0, "a.txt");
    assert_eq!(received[0].1, b"alpha");
    assert_eq!(received[1].0, "b.bin");
    assert_eq!(received[1].1, &[0, 1, 2, 3, 4, 5]);
    assert_eq!(received[2].0, "c.txt");
    assert_eq!(received[2].1, b"gamma delta epsilon");

    let _ = framed.send(Frame::Data(Bytes::from("exit\n"))).await;
    let _ = timeout(Duration::from_secs(3), server).await;
}

#[tokio::test]
async fn send_receive_receiver_first() {
    let _permit = CONCURRENCY.acquire().await.unwrap();
    let (_client_tx, mut framed, server, _meta, svc_path) = setup_session_with_svc_path().await;
    wait_for_shell(&mut framed).await;

    for _ in 0..50 {
        if svc_path.exists() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }

    let file_data = b"receiver-first test data";

    // Spawn receiver first this time
    let sp = svc_path.clone();
    let receiver = tokio::spawn(async move { receive_files(&sp).await });
    tokio::time::sleep(Duration::from_millis(50)).await;
    let sp = svc_path.clone();
    let sender = tokio::spawn(async move {
        send_files(&sp, &[("recv_first.dat", file_data)]).await;
    });

    let (r, s) = tokio::join!(receiver, sender);
    s.unwrap();
    let files = r.unwrap();

    assert_eq!(files.len(), 1);
    assert_eq!(files[0].0, "recv_first.dat");
    assert_eq!(files[0].1, file_data);

    let _ = framed.send(Frame::Data(Bytes::from("exit\n"))).await;
    let _ = timeout(Duration::from_secs(3), server).await;
}

#[tokio::test]
async fn send_cancel_on_sender_disconnect() {
    let _permit = CONCURRENCY.acquire().await.unwrap();
    let (_client_tx, mut framed, server, _meta, svc_path) = setup_session_with_svc_path().await;
    wait_for_shell(&mut framed).await;

    for _ in 0..50 {
        if svc_path.exists() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }

    // Connect sender with a 1MB file but disconnect before sending data
    let sp = svc_path.clone();
    let sender = tokio::spawn(async move {
        let mut stream = UnixStream::connect(&sp).await.unwrap();
        stream.write_all(&[gritty::protocol::SvcRequest::Send.to_byte()]).await.unwrap();
        // 1 file, 1MB
        stream.write_all(&1u32.to_be_bytes()).await.unwrap();
        let name = b"big.bin";
        stream.write_all(&(name.len() as u16).to_be_bytes()).await.unwrap();
        stream.write_all(name).await.unwrap();
        stream.write_all(&(1024u64 * 1024).to_be_bytes()).await.unwrap();

        // Wait for go signal
        let mut go = [0u8; 1];
        stream.read_exact(&mut go).await.unwrap();
        // Drop stream without sending file data
        drop(stream);
    });

    tokio::time::sleep(Duration::from_millis(50)).await;
    let sp = svc_path.clone();
    let receiver = tokio::spawn(async move {
        let mut stream = UnixStream::connect(&sp).await.unwrap();
        stream.write_all(&[gritty::protocol::SvcRequest::Receive.to_byte()]).await.unwrap();
        stream.write_all(b"\n").await.unwrap();
        // Try to read -- should fail when sender disconnects
        let mut buf4 = [0u8; 4];
        let _ = stream.read_exact(&mut buf4).await;
    });

    let _ = tokio::join!(sender, receiver);

    // Should get SendCancel notification
    let mut got_cancel = false;
    loop {
        match timeout(Duration::from_secs(3), framed.next()).await {
            Ok(Some(Ok(Frame::SendCancel { reason }))) => {
                assert!(reason.contains("sender disconnected"), "got: {reason}");
                got_cancel = true;
                break;
            }
            Ok(Some(Ok(Frame::SendOffer { .. }))) => continue,
            Ok(Some(Ok(Frame::Data(_)))) => continue,
            _ => break,
        }
    }
    assert!(got_cancel, "did not receive SendCancel");

    let _ = framed.send(Frame::Data(Bytes::from("exit\n"))).await;
    let _ = timeout(Duration::from_secs(3), server).await;
}

#[tokio::test]
async fn send_filename_sanitized() {
    let _permit = CONCURRENCY.acquire().await.unwrap();
    let (_client_tx, mut framed, server, _meta, svc_path) = setup_session_with_svc_path().await;
    wait_for_shell(&mut framed).await;

    for _ in 0..50 {
        if svc_path.exists() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }

    // Send a file with a path separator in the name -- server should sanitize to basename
    let file_data = b"sanitized";

    let sp = svc_path.clone();
    let sender = tokio::spawn(async move {
        let mut stream = UnixStream::connect(&sp).await.unwrap();
        stream.write_all(&[gritty::protocol::SvcRequest::Send.to_byte()]).await.unwrap();
        stream.write_all(&1u32.to_be_bytes()).await.unwrap();
        let name = b"../../etc/passwd";
        stream.write_all(&(name.len() as u16).to_be_bytes()).await.unwrap();
        stream.write_all(name).await.unwrap();
        stream.write_all(&(file_data.len() as u64).to_be_bytes()).await.unwrap();
        let mut go = [0u8; 1];
        stream.read_exact(&mut go).await.unwrap();
        stream.write_all(file_data).await.unwrap();
    });
    tokio::time::sleep(Duration::from_millis(50)).await;
    let sp = svc_path.clone();
    let receiver = tokio::spawn(async move { receive_files(&sp).await });

    let (s, r) = tokio::join!(sender, receiver);
    s.unwrap();
    let files = r.unwrap();

    assert_eq!(files.len(), 1);
    // Should be sanitized to just "passwd"
    assert_eq!(files[0].0, "passwd");
    assert_eq!(files[0].1, b"sanitized");

    let _ = framed.send(Frame::Data(Bytes::from("exit\n"))).await;
    let _ = timeout(Duration::from_secs(3), server).await;
}

#[tokio::test]
async fn stale_receiver_does_not_poison_next_transfer() {
    let _permit = CONCURRENCY.acquire().await.unwrap();
    let (_client_tx, mut framed, server, _meta, svc_path) = setup_session_with_svc_path().await;
    wait_for_shell(&mut framed).await;

    for _ in 0..50 {
        if svc_path.exists() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    assert!(svc_path.exists(), "svc socket not bound");

    // Step 1: Connect a receiver and immediately drop it.
    // This simulates the local side's auto-detect connecting to multiple sessions
    // and dropping the unpaired ones after select_first_ready picks a different session.
    {
        let mut stream = UnixStream::connect(&svc_path).await.unwrap();
        stream.write_all(&[gritty::protocol::SvcRequest::Receive.to_byte()]).await.unwrap();
        stream.write_all(b"/tmp\n").await.unwrap();
        // Drop stream -- server enters WaitingForSender with a dead receiver
    }

    // Give server time to process ReceiverArrived
    tokio::time::sleep(Duration::from_millis(200)).await;

    // Step 2: Now do a real transfer. The sender arrives and should NOT pair
    // with the stale dead receiver. Instead it should wait, then pair with the
    // real receiver that arrives next.
    let file_data = b"stale test data\n";
    let sp = svc_path.clone();
    let sender = tokio::spawn(async move {
        send_files(&sp, &[("stale_test.txt", file_data)]).await;
    });

    tokio::time::sleep(Duration::from_millis(50)).await;

    let sp = svc_path.clone();
    let receiver = tokio::spawn(async move { receive_files(&sp).await });

    let (send_result, recv_result) = tokio::join!(sender, receiver);
    send_result.unwrap();
    let files = recv_result.unwrap();

    assert_eq!(files.len(), 1);
    assert_eq!(files[0].0, "stale_test.txt");
    assert_eq!(files[0].1, file_data);

    let _ = framed.send(Frame::Data(Bytes::from("exit\n"))).await;
    let _ = timeout(Duration::from_secs(3), server).await;
}

#[tokio::test]
async fn stale_sender_does_not_poison_next_transfer() {
    let _permit = CONCURRENCY.acquire().await.unwrap();
    let (_client_tx, mut framed, server, _meta, svc_path) = setup_session_with_svc_path().await;
    wait_for_shell(&mut framed).await;

    for _ in 0..50 {
        if svc_path.exists() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    assert!(svc_path.exists(), "svc socket not bound");

    // Step 1: Connect a sender with manifest and immediately drop it.
    {
        let mut stream = UnixStream::connect(&svc_path).await.unwrap();
        stream.write_all(&[gritty::protocol::SvcRequest::Send.to_byte()]).await.unwrap();
        // Manifest: 1 file, "ghost.txt", 100 bytes
        stream.write_all(&1u32.to_be_bytes()).await.unwrap();
        let name = b"ghost.txt";
        stream.write_all(&(name.len() as u16).to_be_bytes()).await.unwrap();
        stream.write_all(name).await.unwrap();
        stream.write_all(&100u64.to_be_bytes()).await.unwrap();
        // Drop stream -- server enters WaitingForReceiver with a dead sender
    }

    // Give server time to process SenderArrived
    tokio::time::sleep(Duration::from_millis(200)).await;

    // Step 2: Real transfer should succeed despite stale sender state.
    let file_data = b"real data\n";
    let sp = svc_path.clone();
    let sender = tokio::spawn(async move {
        send_files(&sp, &[("real.txt", file_data)]).await;
    });

    tokio::time::sleep(Duration::from_millis(50)).await;

    let sp = svc_path.clone();
    let receiver = tokio::spawn(async move { receive_files(&sp).await });

    let (send_result, recv_result) = tokio::join!(sender, receiver);
    send_result.unwrap();
    let files = recv_result.unwrap();

    assert_eq!(files.len(), 1);
    assert_eq!(files[0].0, "real.txt");
    assert_eq!(files[0].1, file_data);

    let _ = framed.send(Frame::Data(Bytes::from("exit\n"))).await;
    let _ = timeout(Duration::from_secs(3), server).await;
}
