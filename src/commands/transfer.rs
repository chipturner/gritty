use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};

use super::util::{parse_target, resolve_ctl_path};

/// Sanitize a filename to its basename, rejecting ".." and empty names.
fn sanitize_basename(name: &str) -> anyhow::Result<String> {
    let basename = Path::new(name).file_name().and_then(|n| n.to_str()).unwrap_or(name);
    if basename.is_empty() || basename == ".." {
        anyhow::bail!("invalid filename: {name}");
    }
    Ok(basename.to_string())
}

struct DiscoveredSession {
    session_id: String,
    ctl_path: PathBuf,
}

/// Probe a single daemon for its sessions.
async fn probe_daemon_sessions(ctl_path: &Path) -> Vec<DiscoveredSession> {
    use futures_util::{SinkExt, StreamExt};
    use gritty::protocol::{Frame, FrameCodec};
    use tokio_util::codec::Framed;

    let stream = match tokio::net::UnixStream::connect(ctl_path).await {
        Ok(s) => s,
        Err(_) => return vec![],
    };
    let mut framed = Framed::new(stream, FrameCodec);
    if gritty::handshake(&mut framed).await.is_err() {
        return vec![];
    }
    if framed.send(Frame::ListSessions).await.is_err() {
        return vec![];
    }
    match Frame::expect_from(framed.next().await) {
        Ok(Frame::SessionInfo { sessions }) => sessions
            .into_iter()
            .map(|s| DiscoveredSession {
                session_id: if s.name.is_empty() { s.id } else { s.name },
                ctl_path: ctl_path.to_path_buf(),
            })
            .collect(),
        _ => vec![],
    }
}

/// Discover all sessions across all known daemons.
async fn discover_all_sessions(
    ctl_socket: Option<&Path>,
) -> anyhow::Result<Vec<DiscoveredSession>> {
    let mut probes: Vec<PathBuf> = Vec::new();

    if let Some(p) = ctl_socket {
        probes.push(p.to_path_buf());
    } else {
        let local = gritty::daemon::control_socket_path();
        if local.exists() {
            probes.push(local);
        }
        for info in gritty::connect::get_tunnel_info() {
            if info.status == "healthy" {
                probes.push(gritty::connect::connection_socket_path(&info.name));
            }
        }
    }

    if probes.is_empty() {
        anyhow::bail!("no server running");
    }

    let futures: Vec<_> = probes
        .into_iter()
        .map(|path| async move {
            tokio::time::timeout(std::time::Duration::from_secs(2), probe_daemon_sessions(&path))
                .await
                .unwrap_or_default()
        })
        .collect();

    let results: Vec<DiscoveredSession> =
        futures_util::future::join_all(futures).await.into_iter().flatten().collect();

    if results.is_empty() {
        anyhow::bail!("no active sessions");
    }
    Ok(results)
}

/// Connect to the daemon, handshake, send SendFile, extract raw stream.
/// The `role` byte is written on the raw stream after framing is stripped,
/// so the session's `handle_send_stream` can route to Send or Receive.
async fn send_file_handshake(
    ctl_path: &Path,
    session: &str,
    role: u8,
) -> anyhow::Result<tokio::net::UnixStream> {
    use futures_util::{SinkExt, StreamExt};
    use gritty::protocol::{Frame, FrameCodec};
    use tokio::io::AsyncWriteExt;
    use tokio_util::codec::Framed;

    let stream = tokio::net::UnixStream::connect(ctl_path).await.map_err(|_| {
        anyhow::anyhow!("no server running (could not connect to {})", ctl_path.display())
    })?;
    let mut framed = Framed::new(stream, FrameCodec);
    gritty::handshake(&mut framed).await?;
    framed.send(Frame::SendFile { session: session.to_string() }).await?;

    match Frame::expect_from(framed.next().await)? {
        Frame::Ok => {}
        Frame::Error { message } => anyhow::bail!("{message}"),
        other => anyhow::bail!("unexpected response: {other:?}"),
    }

    let mut stream = framed.into_inner();
    stream.write_all(&[role]).await?;
    Ok(stream)
}

/// A stream tagged with the session it belongs to.
struct TaggedStream {
    stream: tokio::net::UnixStream,
    /// Human-readable session label (e.g. "local:work").
    label: Option<String>,
}

/// Connect to service sockets for transfer. Returns one or more tagged streams.
/// In-session or explicit --session returns one; auto-detect returns all.
async fn connect_send_sockets(
    ctl_socket: Option<PathBuf>,
    session_flag: Option<String>,
    role: u8,
) -> anyhow::Result<Vec<TaggedStream>> {
    // In-session: GRITTY_SOCK is set
    if let Ok(sock_path) = std::env::var("GRITTY_SOCK") {
        if session_flag.is_some() {
            anyhow::bail!("cannot specify --session inside a session");
        }
        let mut stream = tokio::net::UnixStream::connect(&sock_path).await.map_err(|e| {
            anyhow::anyhow!("could not connect to service socket ({sock_path}): {e}")
        })?;
        use tokio::io::AsyncWriteExt;
        stream.write_all(&[role]).await?;
        return Ok(vec![TaggedStream { stream, label: None }]);
    }

    // Explicit --session flag
    if let Some(target) = session_flag {
        let (host, session) = parse_target(&target);
        let session = session
            .ok_or_else(|| anyhow::anyhow!("--session requires host:session (e.g. local:0)"))?;
        let ctl_path = resolve_ctl_path(ctl_socket, Some(&host))?;
        let stream = send_file_handshake(&ctl_path, &session, role).await?;
        let label = format!("{host}:{session}");
        return Ok(vec![TaggedStream { stream, label: Some(label) }]);
    }

    // Auto-detect: connect to ALL sessions
    let sessions = discover_all_sessions(ctl_socket.as_deref()).await?;
    let mut streams = Vec::new();
    for s in &sessions {
        if let Ok(stream) = send_file_handshake(&s.ctl_path, &s.session_id, role).await {
            // Derive host from ctl_path (connect-*.sock -> host, ctl.sock -> local)
            let host = s
                .ctl_path
                .file_name()
                .and_then(|f| f.to_str())
                .and_then(|f| f.strip_prefix("connect-").and_then(|r| r.strip_suffix(".sock")))
                .unwrap_or("local");
            let label = format!("{host}:{}", s.session_id);
            streams.push(TaggedStream { stream, label: Some(label) });
        }
    }
    if streams.is_empty() {
        anyhow::bail!("no active sessions");
    }
    Ok(streams)
}

/// Wait for the first stream to become readable, return it (drop the rest).
async fn select_first_ready(streams: Vec<TaggedStream>) -> anyhow::Result<TaggedStream> {
    use futures_util::future::select_all;

    let futs: Vec<_> = streams
        .into_iter()
        .map(|ts| {
            Box::pin(async move {
                ts.stream.readable().await?;
                Ok::<_, std::io::Error>(ts)
            })
        })
        .collect();

    let (result, _, _) = select_all(futs).await;
    Ok(result?)
}

async fn write_send_manifest(
    stream: &mut tokio::net::UnixStream,
    entries: &[(String, u64, u32, PathBuf)],
) -> anyhow::Result<()> {
    use tokio::io::AsyncWriteExt;
    let file_count = entries.len() as u32;
    stream.write_all(&file_count.to_be_bytes()).await?;
    for (name, size, mode, _) in entries {
        let name_bytes = name.as_bytes();
        stream.write_all(&(name_bytes.len() as u16).to_be_bytes()).await?;
        stream.write_all(name_bytes).await?;
        stream.write_all(&size.to_be_bytes()).await?;
        stream.write_all(&mode.to_be_bytes()).await?;
    }
    Ok(())
}

fn print_progress(name: &str, transferred: u64, total: u64, last_render: &mut std::time::Instant) {
    let now = std::time::Instant::now();
    if transferred < total && now.duration_since(*last_render).as_millis() < 50 {
        return;
    }
    *last_render = now;
    let pct = if total == 0 { 100 } else { (transferred * 100 / total).min(100) };
    let bar_width = 20usize;
    let filled = (pct as usize * bar_width / 100).min(bar_width);
    let empty = bar_width - filled;
    let transferred_str = gritty::client::format_size(transferred);
    let total_str = gritty::client::format_size(total);
    eprint!(
        "\x1b[2K\r  {name}  \x1b[32m{}\x1b[2m{}\x1b[0m  {pct}%  {transferred_str}/{total_str}",
        "=".repeat(filled),
        "-".repeat(empty),
    );
}

pub(crate) async fn send_command(
    ctl_socket: Option<PathBuf>,
    session: Option<String>,
    use_stdin: bool,
    timeout: Option<u64>,
    files: Vec<PathBuf>,
) -> anyhow::Result<()> {
    use tokio::io::{AsyncReadExt, AsyncSeekExt, AsyncWriteExt};

    if use_stdin && !files.is_empty() {
        anyhow::bail!("--stdin cannot be used with file arguments");
    }
    if !use_stdin && files.is_empty() {
        anyhow::bail!("either provide files or use --stdin");
    }

    // Spool stdin to a temp file so we know the size without buffering in RAM
    let stdin_temp = if use_stdin {
        let std_file = tempfile::tempfile()?;
        let mut temp = tokio::fs::File::from_std(std_file);
        let size = tokio::io::copy(&mut tokio::io::stdin(), &mut temp).await?;
        temp.seek(std::io::SeekFrom::Start(0)).await?;
        Some((temp, size))
    } else {
        None
    };

    // Validate files exist and collect metadata
    let mut entries: Vec<(String, u64, u32, PathBuf)> = Vec::with_capacity(files.len());
    for path in &files {
        let meta =
            std::fs::metadata(path).map_err(|e| anyhow::anyhow!("{}: {e}", path.display()))?;
        if meta.is_dir() {
            anyhow::bail!(
                "{}: is a directory (use tar: tar czf - dir | gritty send --stdin)",
                path.display()
            );
        } else if meta.is_file() {
            let basename = sanitize_basename(&path.to_string_lossy())?;
            let mode = meta.permissions().mode();
            entries.push((basename, meta.len(), mode, path.clone()));
        } else {
            anyhow::bail!("{}: not a regular file", path.display());
        }
    }
    if !use_stdin && entries.is_empty() {
        anyhow::bail!("no files to send");
    }

    let mut tagged =
        connect_send_sockets(ctl_socket, session, gritty::protocol::SvcRequest::Send.to_byte())
            .await?;

    // Write manifest on all streams
    if let Some((_, size)) = &stdin_temp {
        let stdin_entries = vec![("stdin".to_string(), *size, 0o644u32, PathBuf::new())];
        for ts in &mut tagged {
            write_send_manifest(&mut ts.stream, &stdin_entries).await?;
        }
    } else {
        for ts in &mut tagged {
            write_send_manifest(&mut ts.stream, &entries).await?;
        }
    }

    // Wait for go signal -- first stream to get paired wins
    eprintln!("\x1b[2;33m\u{25b8} waiting for receiver...\x1b[0m");
    let wait_for_pair = async {
        let ts = if tagged.len() == 1 {
            tagged.into_iter().next().unwrap()
        } else {
            select_first_ready(tagged).await?
        };
        if let Some(ref label) = ts.label {
            eprintln!("\x1b[32m\u{25b8} paired with session {label}\x1b[0m");
        }
        let mut stream = ts.stream;

        let mut go = [0u8; 1];
        stream.read_exact(&mut go).await.map_err(|_| anyhow::anyhow!("no receiver connected"))?;
        if go[0] != 0x01 {
            anyhow::bail!("unexpected signal from server: 0x{:02x}", go[0]);
        }
        Ok::<_, anyhow::Error>(stream)
    };
    let mut stream = if let Some(secs) = timeout {
        tokio::time::timeout(std::time::Duration::from_secs(secs), wait_for_pair)
            .await
            .map_err(|_| anyhow::anyhow!("timed out waiting for receiver"))??
    } else {
        wait_for_pair.await?
    };

    // Stream data
    if let Some((mut temp, size)) = stdin_temp {
        let total_str = gritty::client::format_size(size);
        eprintln!("\x1b[2;33m\u{25b8} sending stdin ({total_str})\x1b[0m");
        tokio::io::copy(&mut temp, &mut stream).await?;
    } else {
        let total_bytes: u64 = entries.iter().map(|(_, s, _, _)| s).sum();
        let total_str = gritty::client::format_size(total_bytes);
        let s = if entries.len() == 1 { "" } else { "s" };
        eprintln!("\x1b[2;33m\u{25b8} sending {} file{s} ({total_str})\x1b[0m", entries.len());

        let mut buf = vec![0u8; 64 * 1024];
        for (name, size, _mode, path) in &entries {
            let mut file = tokio::fs::File::open(path).await?;
            let mut remaining = *size;
            let mut transferred = 0u64;
            let mut last_render = std::time::Instant::now();
            while remaining > 0 {
                let to_read = (remaining as usize).min(buf.len());
                let n = file.read(&mut buf[..to_read]).await?;
                if n == 0 {
                    anyhow::bail!("unexpected EOF reading {name}");
                }
                stream.write_all(&buf[..n]).await?;
                remaining -= n as u64;
                transferred += n as u64;
                print_progress(name, transferred, *size, &mut last_render);
            }
            eprintln!();
        }
    }

    eprintln!("\x1b[32m\u{25b8} done\x1b[0m");
    Ok(())
}

pub(crate) async fn receive_command(
    ctl_socket: Option<PathBuf>,
    session: Option<String>,
    use_stdout: bool,
    timeout: Option<u64>,
    dir: Option<PathBuf>,
) -> anyhow::Result<()> {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    let dest_dir = dir.unwrap_or_else(|| PathBuf::from("."));
    if !use_stdout && !dest_dir.is_dir() {
        anyhow::bail!("{}: not a directory", dest_dir.display());
    }

    let mut tagged =
        connect_send_sockets(ctl_socket, session, gritty::protocol::SvcRequest::Receive.to_byte())
            .await?;

    // Write dest dir on all streams
    let dest_str = dest_dir.to_string_lossy();
    for ts in &mut tagged {
        ts.stream.write_all(dest_str.as_bytes()).await?;
        ts.stream.write_all(b"\n").await?;
    }

    // Wait for file data -- first stream to get paired wins
    eprintln!("\x1b[2;33m\u{25b8} waiting for sender...\x1b[0m");
    let wait_for_pair = async {
        let ts = if tagged.len() == 1 {
            tagged.into_iter().next().unwrap()
        } else {
            select_first_ready(tagged).await?
        };
        if let Some(ref label) = ts.label {
            eprintln!("\x1b[32m\u{25b8} paired with session {label}\x1b[0m");
        }
        let mut stream = ts.stream;

        // Read: file_count (u32 BE)
        let mut buf4 = [0u8; 4];
        stream.read_exact(&mut buf4).await.map_err(|_| anyhow::anyhow!("no sender connected"))?;
        let file_count = u32::from_be_bytes(buf4);
        Ok::<_, anyhow::Error>((stream, file_count))
    };
    let (mut stream, file_count) = if let Some(secs) = timeout {
        tokio::time::timeout(std::time::Duration::from_secs(secs), wait_for_pair)
            .await
            .map_err(|_| anyhow::anyhow!("timed out waiting for sender"))??
    } else {
        wait_for_pair.await?
    };

    // Read per-file metadata and data
    let mut received = 0u32;
    let mut buf = vec![0u8; 64 * 1024];
    let mut stdout = if use_stdout { Some(tokio::io::stdout()) } else { None };
    loop {
        // Read filename_len (u16 BE)
        let mut buf2 = [0u8; 2];
        stream.read_exact(&mut buf2).await?;
        let name_len = u16::from_be_bytes(buf2) as usize;
        if name_len == 0 {
            break; // sentinel
        }

        // Read filename
        let mut name_buf = vec![0u8; name_len];
        stream.read_exact(&mut name_buf).await?;
        let name = String::from_utf8(name_buf)?;
        let name = sanitize_basename(&name)?;

        // Read file_size (u64 BE) and mode (u32 BE)
        let mut buf8 = [0u8; 8];
        stream.read_exact(&mut buf8).await?;
        let file_size = u64::from_be_bytes(buf8);
        let mut buf4 = [0u8; 4];
        stream.read_exact(&mut buf4).await?;
        let mode = u32::from_be_bytes(buf4);

        if let Some(ref mut out) = stdout {
            // Write data to stdout
            let mut remaining = file_size;
            while remaining > 0 {
                let to_read = (remaining as usize).min(buf.len());
                stream.read_exact(&mut buf[..to_read]).await?;
                out.write_all(&buf[..to_read]).await?;
                remaining -= to_read as u64;
            }
        } else {
            let s = if file_count == 1 { "" } else { "s" };
            if received == 0 {
                eprintln!("\x1b[2;33m\u{25b8} receiving {file_count} file{s}\x1b[0m");
            }

            // Write file data (create parent dirs for nested paths)
            let file_path = dest_dir.join(&name);
            if let Some(parent) = file_path.parent() {
                tokio::fs::create_dir_all(parent).await?;
            }
            let recv_mode = if mode == 0 { 0o644 } else { mode & 0o7777 };
            let mut file = tokio::fs::OpenOptions::new()
                .write(true)
                .create(true)
                .truncate(true)
                .mode(recv_mode)
                .open(&file_path)
                .await?;
            let mut remaining = file_size;
            let mut transferred = 0u64;
            let mut last_render = std::time::Instant::now();
            while remaining > 0 {
                let to_read = (remaining as usize).min(buf.len());
                stream.read_exact(&mut buf[..to_read]).await?;
                file.write_all(&buf[..to_read]).await?;
                remaining -= to_read as u64;
                transferred += to_read as u64;
                print_progress(&name, transferred, file_size, &mut last_render);
            }
            eprintln!();
        }
        received += 1;
    }

    if !use_stdout {
        if received == 0 {
            eprintln!("\x1b[2;33m\u{25b8} no files received\x1b[0m");
        } else {
            let s = if received == 1 { "" } else { "s" };
            eprintln!("\x1b[32m\u{25b8} received {received} file{s}\x1b[0m");
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sanitize_basename_simple() {
        assert_eq!(sanitize_basename("foo.txt").unwrap(), "foo.txt");
    }

    #[test]
    fn sanitize_basename_strips_path() {
        assert_eq!(sanitize_basename("/a/b/foo.txt").unwrap(), "foo.txt");
    }

    #[test]
    fn sanitize_basename_rejects_dotdot() {
        assert!(sanitize_basename("..").is_err());
    }
}
