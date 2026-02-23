use bytes::Bytes;
use futures_util::{SinkExt, StreamExt};
use gritty::protocol::{Frame, FrameCodec};
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

fn unique_agent_socket_path() -> PathBuf {
    let id = TEST_ID.fetch_add(1, Ordering::Relaxed);
    let pid = std::process::id();
    std::env::temp_dir().join(format!("gritty-test-agent-{pid}-{id}.sock"))
}

fn unique_open_socket_path() -> PathBuf {
    let id = TEST_ID.fetch_add(1, Ordering::Relaxed);
    let pid = std::process::id();
    std::env::temp_dir().join(format!("gritty-test-open-{pid}-{id}.sock"))
}

/// Spawn a server task connected via socketpair + channel.
/// Returns (client_tx for takeover, client-side framed, server join handle).
async fn setup_session() -> (
    mpsc::UnboundedSender<Framed<UnixStream, FrameCodec>>,
    Framed<UnixStream, FrameCodec>,
    JoinHandle<anyhow::Result<()>>,
    Arc<OnceLock<gritty::server::SessionMetadata>>,
) {
    let (client_tx, client_rx) = mpsc::unbounded_channel();
    let meta = Arc::new(OnceLock::new());
    let meta_clone = Arc::clone(&meta);
    let agent_path = unique_agent_socket_path();
    let open_path = unique_open_socket_path();
    let handle = tokio::spawn(async move {
        gritty::server::run(client_rx, meta_clone, agent_path, open_path).await
    });

    let (server_stream, client_stream) = UnixStream::pair().unwrap();
    client_tx.send(Framed::new(server_stream, FrameCodec)).unwrap();

    let framed = Framed::new(client_stream, FrameCodec);
    (client_tx, framed, handle, meta)
}

/// Spawn a server task, send an Env frame before the first Resize.
/// Returns same tuple as setup_session().
async fn setup_session_with_env(
    env_vars: Vec<(String, String)>,
) -> (
    mpsc::UnboundedSender<Framed<UnixStream, FrameCodec>>,
    Framed<UnixStream, FrameCodec>,
    JoinHandle<anyhow::Result<()>>,
    Arc<OnceLock<gritty::server::SessionMetadata>>,
) {
    let (client_tx, client_rx) = mpsc::unbounded_channel();
    let meta = Arc::new(OnceLock::new());
    let meta_clone = Arc::clone(&meta);
    let agent_path = unique_agent_socket_path();
    let open_path = unique_open_socket_path();
    let handle = tokio::spawn(async move {
        gritty::server::run(client_rx, meta_clone, agent_path, open_path).await
    });

    let (server_stream, client_stream) = UnixStream::pair().unwrap();
    client_tx.send(Framed::new(server_stream, FrameCodec)).unwrap();

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
    client_tx.send(Framed::new(server_stream, FrameCodec)).unwrap();
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
    client_tx.send(Framed::new(server_stream2, FrameCodec)).unwrap();
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
        client_tx.send(Framed::new(server_stream, FrameCodec)).unwrap();
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
    client_tx.send(Framed::new(server_stream, FrameCodec)).unwrap();
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
    client_tx.send(Framed::new(server_stream, FrameCodec)).unwrap();
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
    client_tx.send(Framed::new(server_stream, FrameCodec)).unwrap();
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
    mpsc::UnboundedSender<Framed<UnixStream, FrameCodec>>,
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
    let open_path = unique_open_socket_path();
    let handle = tokio::spawn(async move {
        gritty::server::run(client_rx, meta_clone, agent_path_clone, open_path).await
    });

    let (server_stream, client_stream) = UnixStream::pair().unwrap();
    client_tx.send(Framed::new(server_stream, FrameCodec)).unwrap();

    let framed = Framed::new(client_stream, FrameCodec);
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

/// Like setup_session but returns the open socket path for open forwarding tests.
async fn setup_session_with_open_path() -> (
    mpsc::UnboundedSender<Framed<UnixStream, FrameCodec>>,
    Framed<UnixStream, FrameCodec>,
    JoinHandle<anyhow::Result<()>>,
    Arc<OnceLock<gritty::server::SessionMetadata>>,
    PathBuf,
) {
    let (client_tx, client_rx) = mpsc::unbounded_channel();
    let meta = Arc::new(OnceLock::new());
    let meta_clone = Arc::clone(&meta);
    let agent_path = unique_agent_socket_path();
    let open_path = unique_open_socket_path();
    let open_path_clone = open_path.clone();
    let handle = tokio::spawn(async move {
        gritty::server::run(client_rx, meta_clone, agent_path, open_path_clone).await
    });

    let (server_stream, client_stream) = UnixStream::pair().unwrap();
    client_tx.send(Framed::new(server_stream, FrameCodec)).unwrap();

    let framed = Framed::new(client_stream, FrameCodec);
    (client_tx, framed, handle, meta, open_path)
}

#[tokio::test]
async fn open_forwarding_url_roundtrip() {
    let _permit = CONCURRENCY.acquire().await.unwrap();
    let (_tx, mut framed, server, _meta, open_path) = setup_session_with_open_path().await;
    wait_for_shell(&mut framed).await;
    read_available_data(&mut framed, Duration::from_secs(1)).await;

    // Client sends OpenForward to enable forwarding
    framed.send(Frame::OpenForward).await.unwrap();
    tokio::time::sleep(Duration::from_millis(100)).await;

    // Connect to open socket (simulating `gritty open` inside the session)
    let mut open_conn = UnixStream::connect(&open_path).await.unwrap();
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
    let (_tx, mut framed, server, _meta, open_path) = setup_session_with_open_path().await;
    wait_for_shell(&mut framed).await;
    read_available_data(&mut framed, Duration::from_secs(1)).await;

    // Do NOT send OpenForward — open socket file should not exist.
    assert!(!open_path.exists(), "open socket should not exist without OpenForward");

    assert!(
        UnixStream::connect(&open_path).await.is_err(),
        "connect to open socket should fail without OpenForward"
    );

    let _ = framed.send(Frame::Data(Bytes::from("exit\n"))).await;
    let _ = timeout(Duration::from_secs(3), server).await;
}
