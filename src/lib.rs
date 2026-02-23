pub mod client;
pub mod connect;
pub mod daemon;
pub mod protocol;
pub mod security;
pub mod server;

/// Collect TERM/LANG/COLORTERM from the environment for forwarding to remote sessions.
pub fn collect_env_vars() -> Vec<(String, String)> {
    ["TERM", "LANG", "COLORTERM"]
        .iter()
        .filter_map(|k| std::env::var(k).ok().map(|v| (k.to_string(), v)))
        .collect()
}

/// Spawn bidirectional relay tasks for a Unix stream channel.
///
/// Reader task reads from the stream and calls `on_data`/`on_close`.
/// Writer task drains the returned sender and writes to the stream.
pub fn spawn_channel_relay<F, G>(
    channel_id: u32,
    read_half: tokio::net::unix::OwnedReadHalf,
    write_half: tokio::net::unix::OwnedWriteHalf,
    on_data: F,
    on_close: G,
) -> tokio::sync::mpsc::UnboundedSender<bytes::Bytes>
where
    F: Fn(u32, bytes::Bytes) -> bool + Send + 'static,
    G: Fn(u32) + Send + 'static,
{
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    let (writer_tx, mut writer_rx) = tokio::sync::mpsc::unbounded_channel::<bytes::Bytes>();

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
    });

    writer_tx
}
