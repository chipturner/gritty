pub mod alt_screen;
pub mod client;
pub mod config;
pub mod connect;
pub mod daemon;
pub mod logging;
pub mod naming;
pub mod net_watch;
pub mod procscan;
pub mod protocol;
pub mod runinfo;
pub mod scrollback;
pub mod security;
pub mod server;
pub mod table;
pub mod ui;

/// `tokio::spawn`, but the task inherits the caller's current tracing span.
///
/// Bare `tokio::spawn` does *not*: the spawned future is polled by another
/// worker with no span entered, so every line it logs is emitted outside the
/// `session{id,name}` / `client{session}` span its parent runs in. On a daemon
/// serving several sessions those lines -- which include the svc-socket
/// security events (`peer_cred unavailable`, unknown request byte) -- become
/// unattributable.
///
/// Attaching `Span::current()` is never worse than the status quo: outside any
/// span it resolves to the disabled current span and costs nothing.
pub fn spawn_traced<F>(future: F) -> tokio::task::JoinHandle<F::Output>
where
    F: Future<Output: Send + 'static> + Send + 'static,
{
    use tracing::Instrument;
    tokio::spawn(future.instrument(tracing::Span::current()))
}

/// Parse a compact duration into seconds: bare seconds (`"90"`) or a number
/// with a unit suffix (`"90s"`, `"30m"`, `"12h"`, `"7d"`) -- what `gritty ls`
/// prints in the Idle column is valid input here.
pub fn parse_duration(s: &str) -> anyhow::Result<u64> {
    let err = || anyhow::anyhow!("invalid duration: {s:?} (use e.g. 90s, 30m, 12h, 7d)");
    let (number, multiplier) = match s.chars().last() {
        Some('s') => (&s[..s.len() - 1], 1),
        Some('m') => (&s[..s.len() - 1], 60),
        Some('h') => (&s[..s.len() - 1], 3600),
        Some('d') => (&s[..s.len() - 1], 86400),
        Some(c) if c.is_ascii_digit() => (s, 1),
        _ => return Err(err()),
    };
    let n: u64 = number.parse().map_err(|_| err())?;
    n.checked_mul(multiplier).ok_or_else(err)
}

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
///
/// The result is memoized for the life of the process. This is load-bearing,
/// not an optimization: the daemon records the Hello's `device_id` as the
/// session owner, and the client must auto-reconnect with the *same* value or
/// the server rejects it with a spurious `OwnerChanged`. The id is requested
/// from several independent call sites (the Hello handshake and the client
/// config, per connect). If the backing file cannot be persisted (read-only
/// or full `$HOME`, container, sandbox) each un-memoized call would generate a
/// fresh random id, so the owner recorded at handshake and the value used for
/// the first auto-reconnect would differ -- silently breaking reconnect
/// exactly where it is needed most. Caching once per process closes that gap.
pub fn get_or_create_device_id() -> u64 {
    static CACHED: std::sync::OnceLock<u64> = std::sync::OnceLock::new();
    *CACHED.get_or_init(|| load_or_create_device_id(&device_id_path()))
}

/// Read an existing device id from `path`, or generate a new non-zero random
/// one and persist it best-effort.
///
/// Not memoized: a failed persist makes successive calls return *different*
/// ids, which is why [`get_or_create_device_id`] wraps this in a process-wide
/// cache. Kept separate so the read/persist contract is unit-testable without
/// touching the real state directory.
fn load_or_create_device_id(path: &std::path::Path) -> u64 {
    // Try reading an existing ID first.
    if let Ok(contents) = std::fs::read_to_string(path)
        && let Ok(id) = contents.trim().parse::<u64>()
        && id != 0
    {
        return id;
    }

    // Generate a new random ID (non-zero).
    let mut buf = [0u8; 8];
    getrandom::fill(&mut buf).expect("getrandom failed");
    let id = u64::from_ne_bytes(buf) | 1;

    // Persist -- best-effort; failure is non-fatal (the process-wide cache in
    // get_or_create_device_id keeps us consistent until the next run).
    if let Some(dir) = path.parent() {
        let _ = security::secure_create_dir_all(dir);
    }
    if std::fs::write(path, format!("{id}\n")).is_ok() {
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let _ = std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600));
        }
    }

    id
}

/// Environment variables forwarded from the client to a remote session's login
/// shell. `TERM`/`COLORTERM` drive terminal rendering; `LANG` and the `LC_*`
/// locale categories stop a remote that lacks the user's locale (a daemon
/// started over non-interactive SSH, where `LC_*` is often unset) from falling
/// back to `C`/`POSIX` and producing UTF-8 mojibake. Mirrors SSH's default
/// `SendEnv LANG LC_*`.
///
/// Shared by the client's [`collect_env_vars`] and the server's env allowlist
/// so the two can never drift apart.
pub const FORWARDED_ENV_KEYS: &[&str] = &[
    "TERM",
    "COLORTERM",
    "LANG",
    "LC_ALL",
    "LC_CTYPE",
    "LC_COLLATE",
    "LC_MESSAGES",
    "LC_MONETARY",
    "LC_NUMERIC",
    "LC_TIME",
    "LC_PAPER",
    "LC_NAME",
    "LC_ADDRESS",
    "LC_TELEPHONE",
    "LC_MEASUREMENT",
    "LC_IDENTIFICATION",
];

/// Collect the [`FORWARDED_ENV_KEYS`] that are set, for forwarding to remote
/// sessions.
pub fn collect_env_vars() -> Vec<(String, String)> {
    FORWARDED_ENV_KEYS
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

    /// Each task reports the span it sees *at poll time*. The entered guard is
    /// released before either handle is awaited, which is what the daemon
    /// actually does -- the spawning future is not being polled while its child
    /// runs. Hold the guard across the await and the child observes the parent's
    /// span through the thread-local, and the test passes vacuously.
    #[tokio::test]
    async fn spawn_traced_carries_the_callers_span_and_bare_spawn_does_not() {
        fn current_span_name() -> Option<&'static str> {
            tracing::Span::current().metadata().map(|m| m.name())
        }

        let _guard = tracing::subscriber::set_default(
            tracing_subscriber::FmtSubscriber::builder().with_writer(std::io::sink).finish(),
        );
        let span = tracing::info_span!("session", id = 7);

        let traced = {
            let _entered = span.enter();
            assert_eq!(current_span_name(), Some("session"), "caller must be inside the span");
            spawn_traced(async { current_span_name() })
        };
        // Negative control: the defect `spawn_traced` exists to prevent.
        let bare = {
            let _entered = span.enter();
            tokio::spawn(async { current_span_name() })
        };

        assert_eq!(traced.await.unwrap(), Some("session"), "spawn_traced must carry the span");
        assert_eq!(bare.await.unwrap(), None, "bare tokio::spawn drops the span");
    }

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
            assert!(FORWARDED_ENV_KEYS.contains(&k.as_str()), "unexpected key: {k}");
        }
    }

    // The locale categories must be forwarded, or a remote login shell lacking
    // the user's LC_* falls back to C/POSIX and renders UTF-8 as mojibake.
    #[test]
    fn forwarded_env_keys_include_locale_categories() {
        for k in ["LANG", "LC_ALL", "LC_CTYPE"] {
            assert!(FORWARDED_ENV_KEYS.contains(&k), "missing locale key: {k}");
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

    #[test]
    fn device_id_round_trips_through_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("device_id");
        let first = load_or_create_device_id(&path);
        assert_ne!(first, 0, "generated id must be non-zero");
        assert!(path.exists(), "id must be persisted on first call");
        // A second call must read the persisted id back, not regenerate.
        assert_eq!(load_or_create_device_id(&path), first);
    }

    #[test]
    fn device_id_prefers_existing_valid_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("device_id");
        std::fs::write(&path, "424242\n").unwrap();
        assert_eq!(load_or_create_device_id(&path), 424242);
    }

    #[test]
    fn device_id_regenerates_on_invalid_or_zero_file() {
        let dir = tempfile::tempdir().unwrap();
        for bad in ["0", "0\n", "not-a-number", ""] {
            let path = dir.path().join("device_id");
            std::fs::write(&path, bad).unwrap();
            let id = load_or_create_device_id(&path);
            assert_ne!(id, 0, "invalid/zero file {bad:?} must regenerate a non-zero id");
        }
    }

    // Regression for the split-brain device id: the Hello handshake and the
    // client config each asked for the device id independently, so a failed
    // persist made the two ids diverge and the first auto-reconnect hit a
    // spurious OwnerChanged. The public entry point must be process-stable.
    #[test]
    fn get_or_create_device_id_is_memoized() {
        let first = get_or_create_device_id();
        assert_ne!(first, 0);
        for _ in 0..5 {
            assert_eq!(get_or_create_device_id(), first);
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
