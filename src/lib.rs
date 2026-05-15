pub mod alt_screen;
pub mod client;
pub mod config;
pub mod connect;
pub mod daemon;
pub mod logging;
pub mod net_watch;
pub mod protocol;
pub mod runinfo;
pub mod scrollback;
pub mod security;
pub mod server;
pub mod table;

/// Result of a successful handshake. The `version` field carries what the
/// server advertised, which may differ from `PROTOCOL_VERSION` -- callers
/// that need session-level compatibility must verify via
/// [`require_matched_version`]. The only caller that legitimately ignores
/// this is `kill-server`, which is the recovery path for upgrading across
/// a mismatch.
#[derive(Debug, Clone, Copy)]
pub struct HandshakeInfo {
    pub version: u16,
    pub capabilities: u32,
    /// Ephemeral server identifier. Compared across reconnects to detect a
    /// daemon that crashed and was respawned (in which case the original
    /// session is gone forever).
    pub server_id: u64,
}

/// Perform a protocol version handshake with the server.
///
/// Sends Hello with our `PROTOCOL_VERSION` and returns whatever `HelloAck`
/// the server replied with. A mismatched version is *not* a handshake error:
/// the server always sends `HelloAck` (since protocol v15) and gates
/// non-`KillServer` frames on its side, while the client decides whether to
/// proceed by calling `require_matched_version`. Returning the server's
/// version unconditionally lets recovery commands act across the mismatch.
pub async fn handshake(
    framed: &mut tokio_util::codec::Framed<tokio::net::UnixStream, protocol::FrameCodec>,
    device_id: u64,
) -> anyhow::Result<HandshakeInfo> {
    use futures_util::{SinkExt, StreamExt};
    framed
        .send(protocol::Frame::Hello {
            version: protocol::PROTOCOL_VERSION,
            capabilities: protocol::CAP_CLIPBOARD,
            device_id,
        })
        .await?;
    // 10s gives headroom for a 300-500ms RTT link with one TCP retransmit.
    // The reconnect loop wraps this in RECONNECT_ATTEMPT_TIMEOUT (15s) so the
    // overall bound is unchanged there; this only loosens the initial connect
    // and server_request paths on marginal links.
    let reply = tokio::time::timeout(std::time::Duration::from_secs(10), framed.next())
        .await
        .map_err(|_| anyhow::anyhow!("handshake timed out after 10s"))?;
    match protocol::Frame::expect_from(reply)? {
        protocol::Frame::HelloAck { version, capabilities, server_id } => Ok(HandshakeInfo {
            version,
            capabilities: protocol::CAP_CLIPBOARD & capabilities,
            server_id,
        }),
        // Legacy: a pre-v15 server may still reject the handshake with
        // `Error { VersionMismatch, .. }` before sending HelloAck. Surface
        // it as a normal handshake error so the reconnect loop treats it
        // as terminal.
        protocol::Frame::Error { message, .. } => anyhow::bail!("handshake rejected: {message}"),
        other => anyhow::bail!("expected HelloAck, got {other:?}"),
    }
}

/// Bail with an actionable error if the remote's protocol version does not
/// match ours. Every caller except `kill-server` (the upgrade recovery path)
/// should invoke this right after `handshake()`.
pub fn require_matched_version(info: &HandshakeInfo) -> anyhow::Result<()> {
    if info.version != protocol::PROTOCOL_VERSION {
        anyhow::bail!(
            "protocol version mismatch: local={} remote={}; run `gritty refresh` to update",
            protocol::PROTOCOL_VERSION,
            info.version,
        );
    }
    Ok(())
}

/// Path to the persistent device-id file.
///
/// `$XDG_STATE_HOME/gritty/device_id`, typically `~/.local/state/gritty/device_id`.
pub fn device_id_path() -> std::path::PathBuf {
    use std::path::PathBuf;
    let dir = if let Some(proj) = directories::ProjectDirs::from("", "", "gritty")
        && let Some(state) = proj.state_dir()
    {
        state.to_path_buf()
    } else {
        let home = std::env::var("HOME").unwrap_or_else(|_| ".".into());
        PathBuf::from(home).join(".local/state/gritty")
    };
    dir.join("device_id")
}

/// Return a persistent per-machine device identifier, creating one on first use.
///
/// Stored in `$XDG_STATE_HOME/gritty/device_id` (typically `~/.local/state/gritty/device_id`).
/// The value is a random non-zero `u64` generated via `getrandom`.
pub fn get_or_create_device_id() -> u64 {
    let path = device_id_path();

    // Try reading an existing ID first.
    if let Ok(contents) = std::fs::read_to_string(&path)
        && let Ok(id) = contents.trim().parse::<u64>()
        && id != 0
    {
        return id;
    }

    // Generate a new random ID (non-zero).
    let mut buf = [0u8; 8];
    getrandom::fill(&mut buf).expect("getrandom failed");
    let id = u64::from_ne_bytes(buf) | 1;

    // Persist -- best-effort; failure is non-fatal (we'll regenerate next time).
    if let Some(dir) = path.parent() {
        let _ = security::secure_create_dir_all(dir);
    }
    if std::fs::write(&path, format!("{id}\n")).is_ok() {
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let _ = std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600));
        }
    }

    id
}

/// Collect TERM/LANG/COLORTERM from the environment for forwarding to remote sessions.
pub fn collect_env_vars() -> Vec<(String, String)> {
    ["TERM", "LANG", "COLORTERM"]
        .iter()
        .filter_map(|k| std::env::var(k).ok().map(|v| (k.to_string(), v)))
        .collect()
}

/// Writer-side mpsc depth for `spawn_channel_relay`. At 32KB per read,
/// 64 entries caps buffered bytes at ~2MB per channel.
const CHANNEL_RELAY_BUFFER: usize = 64;

/// Read chunk size for the relay reader task. Matches SSH's channel packet
/// size so a forwarded response isn't shredded into many tiny frames.
const CHANNEL_RELAY_READ_SIZE: usize = 32 * 1024;

/// Construct the writer-side channel for `spawn_channel_relay`. Split out so
/// accept loops can enqueue their `Accepted` event (carrying the sender)
/// *before* the reader task is spawned -- otherwise on a multi-thread
/// runtime the reader can win the race and enqueue `Data` for a channel the
/// select loop hasn't registered yet, which gets dropped.
pub fn relay_writer_channel()
-> (tokio::sync::mpsc::Sender<bytes::Bytes>, tokio::sync::mpsc::Receiver<bytes::Bytes>) {
    tokio::sync::mpsc::channel(CHANNEL_RELAY_BUFFER)
}

/// Spawn bidirectional relay tasks for a stream channel.
///
/// Reader task reads from the stream and calls `on_data`/`on_close`.
/// Writer task drains `writer_rx` and writes to the stream. Callers obtain
/// `writer_rx` from `relay_writer_channel()`; doing that (and registering the
/// returned sender) *before* calling this function guarantees no `on_data`
/// can fire for an unregistered channel.
pub fn spawn_channel_relay<R, W, F, G>(
    channel_id: u32,
    read_half: R,
    write_half: W,
    writer_rx: tokio::sync::mpsc::Receiver<bytes::Bytes>,
    on_data: F,
    on_close: G,
) where
    R: tokio::io::AsyncRead + Unpin + Send + 'static,
    W: tokio::io::AsyncWrite + Unpin + Send + 'static,
    F: Fn(u32, bytes::Bytes) -> bool + Send + 'static,
    G: Fn(u32) + Send + 'static,
{
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    let mut writer_rx = writer_rx;
    tokio::spawn(async move {
        let mut write_half = write_half;
        while let Some(data) = writer_rx.recv().await {
            if write_half.write_all(&data).await.is_err() {
                break;
            }
        }
        // Graceful half-close: send FIN instead of RST
        let _ = write_half.shutdown().await;
    });

    tokio::spawn(async move {
        let mut read_half = read_half;
        let mut buf = vec![0u8; CHANNEL_RELAY_READ_SIZE];
        loop {
            match read_half.read(&mut buf).await {
                Ok(0) | Err(_) => {
                    on_close(channel_id);
                    break;
                }
                Ok(n) => {
                    if !on_data(channel_id, bytes::Bytes::copy_from_slice(&buf[..n])) {
                        break;
                    }
                }
            }
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn require_matched_version_accepts_same_version() {
        let info =
            HandshakeInfo { version: protocol::PROTOCOL_VERSION, capabilities: 0, server_id: 0 };
        assert!(require_matched_version(&info).is_ok());
    }

    // The mismatch guidance must point at the idempotent `gritty refresh`
    // (the documented recovery), not the scorched-earth `restart`.
    #[test]
    fn require_matched_version_points_at_refresh() {
        let info = HandshakeInfo {
            version: protocol::PROTOCOL_VERSION.wrapping_add(1),
            capabilities: 0,
            server_id: 0,
        };
        let err = require_matched_version(&info).unwrap_err().to_string();
        assert!(err.contains("gritty refresh"), "got: {err}");
        assert!(!err.contains("restart"), "must not steer users to restart: {err}");
    }

    #[test]
    fn collect_env_vars_only_known_keys() {
        let vars = collect_env_vars();
        for (k, _) in &vars {
            assert!(["TERM", "LANG", "COLORTERM"].contains(&k.as_str()), "unexpected key: {k}");
        }
    }

    #[test]
    fn collect_env_vars_no_duplicates() {
        let vars = collect_env_vars();
        let keys: Vec<&str> = vars.iter().map(|(k, _)| k.as_str()).collect();
        let mut deduped = keys.clone();
        deduped.sort();
        deduped.dedup();
        assert_eq!(keys.len(), deduped.len());
    }

    #[test]
    fn collect_env_vars_includes_term_if_set() {
        if std::env::var("TERM").is_ok() {
            let vars = collect_env_vars();
            assert!(vars.iter().any(|(k, _)| k == "TERM"));
        }
    }

    #[tokio::test]
    async fn spawn_channel_relay_on_data_and_close() {
        use std::sync::{Arc, Mutex};
        use tokio::io::AsyncWriteExt;
        use tokio::net::UnixStream;

        // Two pairs: one for read side, one for write side
        let (read_stream, mut feed_stream) = UnixStream::pair().unwrap();
        let (write_stream, _drain_stream) = UnixStream::pair().unwrap();
        let (read_half, _) = read_stream.into_split();
        let (_, write_half) = write_stream.into_split();

        let received = Arc::new(Mutex::new(Vec::<(u32, bytes::Bytes)>::new()));
        let received_clone = received.clone();
        let closed = Arc::new(Mutex::new(false));
        let closed_clone = closed.clone();

        let (_writer_tx, writer_rx) = relay_writer_channel();
        spawn_channel_relay(
            42,
            read_half,
            write_half,
            writer_rx,
            move |ch, data| {
                received_clone.lock().unwrap().push((ch, data));
                true
            },
            move |ch| {
                assert_eq!(ch, 42);
                *closed_clone.lock().unwrap() = true;
            },
        );

        // Write data to the relay's read side
        feed_stream.write_all(b"hello").await.unwrap();
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;

        {
            let data = received.lock().unwrap();
            assert_eq!(data.len(), 1);
            assert_eq!(data[0].0, 42);
            assert_eq!(&data[0].1[..], b"hello");
        }

        // Close the feed stream -> triggers on_close
        drop(feed_stream);
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        assert!(*closed.lock().unwrap());
    }

    #[tokio::test]
    async fn spawn_channel_relay_writer_sends_data() {
        use tokio::io::AsyncReadExt;
        use tokio::net::UnixStream;

        // Two pairs: one for read side, one for write side
        let (read_stream, _feed_stream) = UnixStream::pair().unwrap();
        let (write_stream, mut drain_stream) = UnixStream::pair().unwrap();
        let (read_half, _) = read_stream.into_split();
        let (_, write_half) = write_stream.into_split();

        let (writer_tx, writer_rx) = relay_writer_channel();
        spawn_channel_relay(7, read_half, write_half, writer_rx, |_, _| true, |_| {});

        writer_tx.try_send(bytes::Bytes::from_static(b"hello")).unwrap();

        let mut buf = vec![0u8; 32];
        let n =
            tokio::time::timeout(std::time::Duration::from_secs(2), drain_stream.read(&mut buf))
                .await
                .unwrap()
                .unwrap();

        assert_eq!(&buf[..n], b"hello");
    }

    #[tokio::test]
    async fn spawn_channel_relay_writer_half_close() {
        use tokio::io::AsyncReadExt;
        use tokio::net::UnixStream;

        let (read_stream, _feed_stream) = UnixStream::pair().unwrap();
        let (write_stream, mut drain_stream) = UnixStream::pair().unwrap();
        let (read_half, _) = read_stream.into_split();
        let (_, write_half) = write_stream.into_split();

        let (writer_tx, writer_rx) = relay_writer_channel();
        spawn_channel_relay(7, read_half, write_half, writer_rx, |_, _| true, |_| {});

        // Send data then drop the sender (triggers half-close)
        writer_tx.try_send(bytes::Bytes::from_static(b"request")).unwrap();
        drop(writer_tx);

        // Read the data
        let mut buf = vec![0u8; 32];
        let n =
            tokio::time::timeout(std::time::Duration::from_secs(2), drain_stream.read(&mut buf))
                .await
                .unwrap()
                .unwrap();
        assert_eq!(&buf[..n], b"request");

        // Read again -- should get EOF (graceful shutdown), not error
        let n =
            tokio::time::timeout(std::time::Duration::from_secs(2), drain_stream.read(&mut buf))
                .await
                .unwrap()
                .unwrap();
        assert_eq!(n, 0, "expected EOF from graceful half-close");
    }
}
