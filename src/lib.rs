pub mod client;
pub mod config;
pub mod connect;
pub mod daemon;
pub mod protocol;
pub mod security;
pub mod server;
pub mod table;

/// Perform a protocol version handshake with the server.
///
/// Sends Hello with our PROTOCOL_VERSION, expects HelloAck with the
/// negotiated version (min of client and server).
pub async fn handshake(
    framed: &mut tokio_util::codec::Framed<tokio::net::UnixStream, protocol::FrameCodec>,
) -> anyhow::Result<u16> {
    use futures_util::{SinkExt, StreamExt};
    framed
        .send(protocol::Frame::Hello { version: protocol::PROTOCOL_VERSION, capabilities: 0 })
        .await?;
    match protocol::Frame::expect_from(framed.next().await)? {
        protocol::Frame::HelloAck { version, .. } => Ok(version),
        protocol::Frame::Error { message, .. } => anyhow::bail!("handshake rejected: {message}"),
        other => anyhow::bail!("expected HelloAck, got {other:?}"),
    }
}

/// Collect TERM/LANG/COLORTERM from the environment for forwarding to remote sessions.
pub fn collect_env_vars() -> Vec<(String, String)> {
    ["TERM", "LANG", "COLORTERM"]
        .iter()
        .filter_map(|k| std::env::var(k).ok().map(|v| (k.to_string(), v)))
        .collect()
}

/// Spawn bidirectional relay tasks for a stream channel.
///
/// Reader task reads from the stream and calls `on_data`/`on_close`.
/// Writer task drains the returned sender and writes to the stream.
/// Channel buffer size for `spawn_channel_relay` writer channels.
/// At 8KB per read, 256 entries ≈ 2MB per channel.
const CHANNEL_RELAY_BUFFER: usize = 256;

pub fn spawn_channel_relay<R, W, F, G>(
    channel_id: u32,
    read_half: R,
    write_half: W,
    on_data: F,
    on_close: G,
) -> tokio::sync::mpsc::Sender<bytes::Bytes>
where
    R: tokio::io::AsyncRead + Unpin + Send + 'static,
    W: tokio::io::AsyncWrite + Unpin + Send + 'static,
    F: Fn(u32, bytes::Bytes) -> bool + Send + 'static,
    G: Fn(u32) + Send + 'static,
{
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    let (writer_tx, mut writer_rx) =
        tokio::sync::mpsc::channel::<bytes::Bytes>(CHANNEL_RELAY_BUFFER);

    tokio::spawn(async move {
        let mut read_half = read_half;
        let mut buf = vec![0u8; 8192];
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

    writer_tx
}

#[cfg(test)]
mod tests {
    use super::*;

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

        let _writer_tx = spawn_channel_relay(
            42,
            read_half,
            write_half,
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

        let data = received.lock().unwrap();
        assert_eq!(data.len(), 1);
        assert_eq!(data[0].0, 42);
        assert_eq!(&data[0].1[..], b"hello");
        drop(data);

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

        let writer_tx = spawn_channel_relay(7, read_half, write_half, |_, _| true, |_| {});

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

        let writer_tx = spawn_channel_relay(7, read_half, write_half, |_, _| true, |_| {});

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
