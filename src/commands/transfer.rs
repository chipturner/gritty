use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};

use super::util::{resolve_ctl_path, split_target};
use gritty::ui;

/// Sanitize a filename to its basename, rejecting ".." and empty names.
fn sanitize_basename(name: &str) -> anyhow::Result<String> {
    let basename = Path::new(name).file_name().and_then(|n| n.to_str()).unwrap_or(name);
    if basename.is_empty() || basename == ".." {
        anyhow::bail!("invalid filename: {name}");
    }
    Ok(basename.to_string())
}

/// Sanitize a relative path, allowing `/` separators but rejecting `..` components and absolute paths.
fn sanitize_path(name: &str) -> anyhow::Result<String> {
    let p = Path::new(name);
    if p.is_absolute() {
        anyhow::bail!("absolute path not allowed: {name}");
    }
    for component in p.components() {
        match component {
            std::path::Component::ParentDir => anyhow::bail!("'..' not allowed in path: {name}"),
            std::path::Component::RootDir => anyhow::bail!("absolute path not allowed: {name}"),
            _ => {}
        }
    }
    if name.is_empty() {
        anyhow::bail!("empty path");
    }
    Ok(name.to_string())
}

struct DiscoveredSession {
    session_id: String,
    ctl_path: PathBuf,
}

/// Error if any two send entries share a wire name: the receiver opens each
/// file with `truncate(true)`, so a collision (e.g. `send a/x.txt b/x.txt`)
/// silently overwrites an earlier file while still counting every entry as
/// received.
fn reject_duplicate_names<'a>(names: impl IntoIterator<Item = &'a str>) -> anyhow::Result<()> {
    let mut seen = std::collections::HashSet::new();
    for name in names {
        if !seen.insert(name) {
            anyhow::bail!(
                "duplicate file name `{name}`: the receiver would overwrite it -- \
                 rename a file or send them in separate transfers"
            );
        }
    }
    Ok(())
}

/// Result of probing one daemon -- distinguishes a protocol-version mismatch
/// from a plain unreachable daemon so discovery can give an actionable hint.
enum ProbeOutcome {
    /// The daemon answered (the list may still be empty).
    Sessions(Vec<DiscoveredSession>),
    /// The daemon is up but speaks a different protocol version.
    VersionMismatch,
    /// The daemon could not be reached or did not answer.
    Unavailable,
}

/// Probe a single daemon for its sessions.
async fn probe_daemon_sessions(ctl_path: &Path) -> ProbeOutcome {
    use futures_util::{SinkExt, StreamExt};
    use gritty::protocol::{Frame, FrameCodec};
    use tokio_util::codec::Framed;

    let stream = match gritty::security::connect_verified(ctl_path).await {
        Ok(s) => s,
        Err(_) => return ProbeOutcome::Unavailable,
    };
    let mut framed = Framed::new(stream, FrameCodec);
    let info = match gritty::handshake(&mut framed, gritty::get_or_create_device_id()).await {
        Ok(i) => i,
        Err(_) => return ProbeOutcome::Unavailable,
    };
    if gritty::require_matched_version(&info).is_err() {
        return ProbeOutcome::VersionMismatch;
    }
    if framed.send(Frame::ListSessions).await.is_err() {
        return ProbeOutcome::Unavailable;
    }
    match Frame::expect_from(framed.next().await) {
        Ok(Frame::SessionInfo { sessions }) => ProbeOutcome::Sessions(
            sessions
                .into_iter()
                .map(|s| DiscoveredSession {
                    session_id: if s.name.is_empty() { s.id.to_string() } else { s.name },
                    ctl_path: ctl_path.to_path_buf(),
                })
                .collect(),
        ),
        _ => ProbeOutcome::Unavailable,
    }
}

/// Discover all sessions across all known daemons.
async fn discover_all_sessions(
    ctl_socket: Option<&Path>,
) -> anyhow::Result<Vec<DiscoveredSession>> {
    let probes: Vec<PathBuf> = if let Some(p) = ctl_socket {
        vec![p.to_path_buf()]
    } else {
        let discovered = super::util::discover_daemon_probes();
        discovered.into_iter().map(|p| p.socket).collect()
    };

    if probes.is_empty() {
        anyhow::bail!("no server running");
    }

    let futures: Vec<_> = probes
        .into_iter()
        .map(|path| async move {
            tokio::time::timeout(std::time::Duration::from_secs(2), probe_daemon_sessions(&path))
                .await
                .unwrap_or(ProbeOutcome::Unavailable)
        })
        .collect();

    let mut results: Vec<DiscoveredSession> = Vec::new();
    let mut saw_version_mismatch = false;
    for outcome in futures_util::future::join_all(futures).await {
        match outcome {
            ProbeOutcome::Sessions(s) => results.extend(s),
            ProbeOutcome::VersionMismatch => saw_version_mismatch = true,
            ProbeOutcome::Unavailable => {}
        }
    }

    if results.is_empty() {
        if saw_version_mismatch {
            anyhow::bail!(
                "no active sessions (a daemon has a protocol version mismatch -- run `gritty refresh`)"
            );
        }
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
    use gritty::protocol::Frame;
    use tokio::io::AsyncWriteExt;

    let (mut framed, _info) = super::util::connect_handshaked(ctl_path, true).await?;
    framed.send(Frame::SendFile { session: session.to_string() }).await?;

    match Frame::expect_from(framed.next().await)? {
        Frame::Ok => {}
        Frame::Error { message, .. } => anyhow::bail!("{message}"),
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
        // Raw split: main's `resolve_target_session` already rebuilt this
        // target with the canonical (alias-resolved) host.
        let (host, session) = split_target(&target);
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

/// Race `probe` across all `items`; return the first that yields `Some(Ok)`.
///
/// An item whose probe yields `None` is a dead/EOF candidate: it is discarded
/// and the race continues over the *remaining* items rather than aborting.
/// This is the correctness contract for transfer pairing -- a Unix socket
/// whose peer closed is reported readable and its probe fails instantly, so a
/// naive `select_all` would let a dead sibling session beat a live-but-waiting
/// one and abort the whole transfer (violating the best-effort invariant). A
/// probe yielding `Some(Err)` is a hard protocol error and aborts. `Ok(None)`
/// means every candidate died.
#[allow(clippy::type_complexity)]
async fn race_first_ready<S, T>(
    items: Vec<S>,
    probe: impl Fn(S) -> std::pin::Pin<Box<dyn std::future::Future<Output = Option<anyhow::Result<T>>>>>,
) -> anyhow::Result<Option<T>> {
    use futures_util::future::select_all;

    let mut futs: Vec<_> = items.into_iter().map(&probe).collect();
    while !futs.is_empty() {
        let (result, _idx, rest) = select_all(futs).await;
        futs = rest;
        match result {
            Some(Ok(v)) => return Ok(Some(v)),
            Some(Err(e)) => return Err(e),
            None => {} // dead candidate: keep racing the survivors
        }
    }
    Ok(None)
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

/// Repaint the transfer progress bar in place.
///
/// Skipped entirely when stderr is not a terminal: the bar's `\x1b[2K` erase-line
/// and carriage returns are line noise in a log file, and an `anstream` sink
/// cannot rescue it -- the sink would strip the erase-line along with the color
/// and leave the bar stacking one line per repaint. Color is a separate
/// question, so `--color=never` on a terminal still gets a bar.
fn print_progress(name: &str, transferred: u64, total: u64, last_render: &mut std::time::Instant) {
    use gritty::ui::sgr::{DIM, GREEN, RESET};

    if !ui::stderr_is_interactive() {
        return;
    }
    let now = std::time::Instant::now();
    if transferred < total && now.duration_since(*last_render).as_millis() < 50 {
        return;
    }
    *last_render = now;
    let pct = (transferred * 100).checked_div(total).map_or(100, |v| v.min(100));
    let bar_width = 20usize;
    let filled = (pct as usize * bar_width / 100).min(bar_width);
    let empty = bar_width - filled;
    let transferred_str = gritty::client::format_size(transferred);
    let total_str = gritty::client::format_size(total);
    let (green, dim, reset) =
        if ui::stderr_is_colored() { (GREEN, DIM, RESET) } else { ("", "", "") };
    eprint!(
        "\x1b[2K\r  {name}  {green}{}{dim}{}{reset}  {pct}%  {transferred_str}/{total_str}",
        "=".repeat(filled),
        "-".repeat(empty),
    );
}

/// Recursively walk a directory, collecting regular files with paths relative to `base`.
fn walk_dir(
    dir: &Path,
    base: &Path,
    entries: &mut Vec<(String, u64, u32, PathBuf)>,
) -> anyhow::Result<()> {
    for entry in std::fs::read_dir(dir).map_err(|e| anyhow::anyhow!("{}: {e}", dir.display()))? {
        let entry = entry?;
        let path = entry.path();
        let ft = entry.file_type()?;
        if ft.is_symlink() {
            continue;
        }
        if ft.is_dir() {
            walk_dir(&path, base, entries)?;
        } else if ft.is_file() {
            let rel = path.strip_prefix(base).unwrap_or(&path);
            let wire_name = rel.to_string_lossy().to_string();
            let meta = std::fs::metadata(&path)?;
            entries.push((wire_name, meta.len(), meta.permissions().mode(), path));
        }
    }
    Ok(())
}

pub(crate) async fn send_command(
    ctl_socket: Option<PathBuf>,
    session: Option<String>,
    use_stdin: bool,
    timeout: Option<u64>,
    recursive: bool,
    files: Vec<PathBuf>,
) -> anyhow::Result<()> {
    use tokio::io::{AsyncReadExt, AsyncSeekExt, AsyncWriteExt};

    if use_stdin && !files.is_empty() {
        anyhow::bail!("cannot send stdin (`-`) together with file arguments");
    }
    if !use_stdin && files.is_empty() {
        anyhow::bail!("provide files to send (use - for stdin)");
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
    // entries: (wire_name, size, mode, disk_path)
    let mut entries: Vec<(String, u64, u32, PathBuf)> = Vec::with_capacity(files.len());
    for path in &files {
        let meta =
            std::fs::metadata(path).map_err(|e| anyhow::anyhow!("{}: {e}", path.display()))?;
        if meta.is_dir() {
            if !recursive {
                anyhow::bail!(
                    "{}: is a directory (use -r to send recursively, or tar: tar czf - dir | gritty send -)",
                    path.display()
                );
            }
            let base = path.parent().unwrap_or(Path::new(""));
            walk_dir(path, base, &mut entries)?;
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
    reject_duplicate_names(entries.iter().map(|(n, ..)| n.as_str()))?;

    let tagged =
        connect_send_sockets(ctl_socket, session, gritty::protocol::SvcRequest::Send.to_byte())
            .await?;

    // Write the manifest on every discovered stream, best-effort: a stale or
    // broken session must not abort the whole transfer and discard healthy
    // sessions that were ready to pair ("first receiver wins").
    let manifest = match &stdin_temp {
        Some((_, size)) => vec![("stdin".to_string(), *size, 0o644u32, PathBuf::new())],
        None => entries.clone(),
    };
    let mut live = Vec::with_capacity(tagged.len());
    for mut ts in tagged {
        match write_send_manifest(&mut ts.stream, &manifest).await {
            Ok(()) => live.push(ts),
            Err(e) => {
                ui::status(&format!("skipping session {}: {e}", ts.label.as_deref().unwrap_or("?")))
            }
        }
    }
    if live.is_empty() {
        anyhow::bail!("no reachable receiver sessions");
    }
    let tagged = live;

    // Wait for go signal -- first stream to get paired wins. A sibling session
    // that closes before pairing is skipped (its socket reads EOF), not
    // treated as a failure of the whole transfer.
    ui::status("waiting for receiver...");
    let select = race_first_ready(tagged, |mut ts| {
        Box::pin(async move {
            let mut go = [0u8; 1];
            match ts.stream.read_exact(&mut go).await {
                Ok(_) if go[0] == 0x01 => Some(Ok(ts)),
                Ok(_) => {
                    Some(Err(anyhow::anyhow!("unexpected signal from server: 0x{:02x}", go[0])))
                }
                Err(_) => None, // this session closed before pairing -- skip it
            }
        })
    });
    let ts = if let Some(secs) = timeout {
        tokio::time::timeout(std::time::Duration::from_secs(secs), select)
            .await
            .map_err(|_| anyhow::anyhow!("timed out waiting for receiver"))??
    } else {
        select.await?
    }
    .ok_or_else(|| anyhow::anyhow!("no receiver connected"))?;
    if let Some(ref label) = ts.label {
        ui::success(&format!("paired with session {label}"));
    }
    let mut stream = ts.stream;

    // Stream data
    if let Some((mut temp, size)) = stdin_temp {
        let total_str = gritty::client::format_size(size);
        ui::status(&format!("sending stdin ({total_str})"));
        tokio::io::copy(&mut temp, &mut stream).await?;
    } else {
        let total_bytes: u64 = entries.iter().map(|(_, s, _, _)| s).sum();
        let total_str = gritty::client::format_size(total_bytes);
        let s = if entries.len() == 1 { "" } else { "s" };
        ui::status(&format!("sending {} file{s} ({total_str})", entries.len()));

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

    ui::success("done");
    Ok(())
}

/// Stream the received file protocol from `reader`, writing every payload to
/// `out`, and flush `out` before returning (returning the file count).
///
/// The flush is load-bearing: `receive -` writes to `tokio::io::stdout()`,
/// whose blocking `LineWriter` is not flushed at process exit, so without an
/// explicit flush the tail of a non-newline-terminated payload (the canonical
/// `gritty receive - | tar xzf -` case) is silently truncated.
async fn receive_to_writer<R, W>(reader: &mut R, out: &mut W) -> anyhow::Result<u32>
where
    R: tokio::io::AsyncRead + Unpin,
    W: tokio::io::AsyncWrite + Unpin,
{
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    let mut received = 0u32;
    let mut buf = vec![0u8; 64 * 1024];
    loop {
        let mut buf2 = [0u8; 2];
        reader.read_exact(&mut buf2).await?;
        let name_len = u16::from_be_bytes(buf2) as usize;
        if name_len == 0 {
            break; // sentinel
        }
        let mut name_buf = vec![0u8; name_len];
        reader.read_exact(&mut name_buf).await?;
        let name = String::from_utf8(name_buf)?;
        // Validate even though stdout mode ignores the name, to keep the
        // accepted/rejected inputs identical to the receive-to-dir path.
        sanitize_path(&name)?;

        let mut buf8 = [0u8; 8];
        reader.read_exact(&mut buf8).await?;
        let file_size = u64::from_be_bytes(buf8);
        let mut buf4 = [0u8; 4];
        reader.read_exact(&mut buf4).await?;
        let _mode = u32::from_be_bytes(buf4);

        let mut remaining = file_size;
        while remaining > 0 {
            let to_read = (remaining as usize).min(buf.len());
            reader.read_exact(&mut buf[..to_read]).await?;
            out.write_all(&buf[..to_read]).await?;
            remaining -= to_read as u64;
        }
        received += 1;
    }
    out.flush().await?;
    Ok(received)
}

/// Resolve receive's output mode from the CLI args plus whether stdout is a
/// terminal. Stdout mode when: `--stdout`, a `-` destination, or no
/// destination at all while stdout is redirected -- a bare
/// `gritty receive > foo` almost certainly wants the data on stdout, not a
/// `./stdin` file in the cwd. Returns `(use_stdout, dest_dir, auto_switched)`;
/// `auto_switched` lets the caller announce the implicit mode change.
pub(crate) fn resolve_receive_output(
    stdout_flag: bool,
    dir: Option<PathBuf>,
    stdout_is_tty: bool,
) -> (bool, Option<PathBuf>, bool) {
    let dash = dir.as_deref().is_some_and(|d| d.as_os_str() == "-");
    let auto = !stdout_flag && dir.is_none() && !stdout_is_tty;
    let use_stdout = stdout_flag || dash || auto;
    (use_stdout, if use_stdout { None } else { dir }, auto)
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

    let tagged =
        connect_send_sockets(ctl_socket, session, gritty::protocol::SvcRequest::Receive.to_byte())
            .await?;

    // Write the dest dir on every discovered stream, best-effort: one broken
    // session must not abort the transfer ("first sender wins").
    let dest_str = dest_dir.to_string_lossy();
    let mut live = Vec::with_capacity(tagged.len());
    for mut ts in tagged {
        let wrote = async {
            ts.stream.write_all(dest_str.as_bytes()).await?;
            ts.stream.write_all(b"\n").await
        }
        .await;
        match wrote {
            Ok(()) => live.push(ts),
            Err(e) => {
                ui::status(&format!("skipping session {}: {e}", ts.label.as_deref().unwrap_or("?")))
            }
        }
    }
    if live.is_empty() {
        anyhow::bail!("no reachable sender sessions");
    }
    let tagged = live;

    // Wait for file data -- first stream to get paired wins. A sibling session
    // that closes before pairing is skipped (its socket reads EOF), not
    // treated as a failure of the whole transfer.
    ui::status("waiting for sender...");
    let select = race_first_ready(tagged, |mut ts| {
        Box::pin(async move {
            // Read: file_count (u32 BE). EOF here means this session never
            // paired -- drop it and keep racing the rest.
            let mut buf4 = [0u8; 4];
            match ts.stream.read_exact(&mut buf4).await {
                Ok(_) => Some(Ok((ts, u32::from_be_bytes(buf4)))),
                Err(_) => None,
            }
        })
    });
    let (ts, file_count) = if let Some(secs) = timeout {
        tokio::time::timeout(std::time::Duration::from_secs(secs), select)
            .await
            .map_err(|_| anyhow::anyhow!("timed out waiting for sender"))??
    } else {
        select.await?
    }
    .ok_or_else(|| anyhow::anyhow!("no sender connected"))?;
    if let Some(ref label) = ts.label {
        ui::success(&format!("paired with session {label}"));
    }
    let mut stream = ts.stream;

    // Pipe mode: stream every payload straight to stdout and flush (see
    // receive_to_writer -- the flush prevents silent tail truncation).
    if use_stdout {
        let mut out = tokio::io::stdout();
        receive_to_writer(&mut stream, &mut out).await?;
        return Ok(());
    }

    // Read per-file metadata and data into the destination directory.
    let mut received = 0u32;
    let mut buf = vec![0u8; 64 * 1024];
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
        let name = sanitize_path(&name)?;

        // Read file_size (u64 BE) and mode (u32 BE)
        let mut buf8 = [0u8; 8];
        stream.read_exact(&mut buf8).await?;
        let file_size = u64::from_be_bytes(buf8);
        let mut buf4 = [0u8; 4];
        stream.read_exact(&mut buf4).await?;
        let mode = u32::from_be_bytes(buf4);

        let s = if file_count == 1 { "" } else { "s" };
        if received == 0 {
            ui::status(&format!("receiving {file_count} file{s}"));
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
        received += 1;
    }

    if received == 0 {
        ui::status("no files received");
    } else {
        let s = if received == 1 { "" } else { "s" };
        ui::success(&format!("received {received} file{s}"));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn resolve(flag: bool, dir: Option<&str>, tty: bool) -> (bool, Option<PathBuf>, bool) {
        resolve_receive_output(flag, dir.map(PathBuf::from), tty)
    }

    #[test]
    fn receive_output_stdout_flag_wins() {
        assert_eq!(resolve(true, None, true), (true, None, false));
    }

    #[test]
    fn receive_output_dash_means_stdout() {
        assert_eq!(resolve(false, Some("-"), true), (true, None, false));
    }

    #[test]
    fn receive_output_bare_tty_is_dir_mode() {
        assert_eq!(resolve(false, None, true), (false, None, false));
    }

    #[test]
    fn receive_output_bare_redirected_auto_switches() {
        assert_eq!(resolve(false, None, false), (true, None, true));
    }

    #[test]
    fn receive_output_explicit_dir_redirected_stays_dir_mode() {
        assert_eq!(resolve(false, Some("out"), false), (false, Some(PathBuf::from("out")), false));
    }

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

    #[test]
    fn sanitize_path_allows_nested() {
        assert_eq!(sanitize_path("a/b/foo.txt").unwrap(), "a/b/foo.txt");
    }

    #[test]
    fn sanitize_path_allows_simple() {
        assert_eq!(sanitize_path("foo.txt").unwrap(), "foo.txt");
    }

    #[test]
    fn sanitize_path_rejects_dotdot() {
        assert!(sanitize_path("a/../b").is_err());
        assert!(sanitize_path("..").is_err());
    }

    #[test]
    fn sanitize_path_rejects_absolute() {
        assert!(sanitize_path("/etc/passwd").is_err());
    }

    #[test]
    fn sanitize_path_rejects_empty() {
        assert!(sanitize_path("").is_err());
    }

    #[test]
    fn walk_dir_collects_files() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().join("mydir");
        std::fs::create_dir_all(root.join("sub")).unwrap();
        std::fs::write(root.join("a.txt"), "hello").unwrap();
        std::fs::write(root.join("sub/b.txt"), "world").unwrap();

        let mut entries = Vec::new();
        walk_dir(&root, tmp.path(), &mut entries).unwrap();
        let mut names: Vec<_> = entries.iter().map(|(n, _, _, _)| n.clone()).collect();
        names.sort();
        assert_eq!(names, vec!["mydir/a.txt", "mydir/sub/b.txt"]);
    }

    #[test]
    fn walk_dir_skips_symlinks() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().join("d");
        std::fs::create_dir(&root).unwrap();
        std::fs::write(root.join("real.txt"), "data").unwrap();
        std::os::unix::fs::symlink(root.join("real.txt"), root.join("link.txt")).unwrap();

        let mut entries = Vec::new();
        walk_dir(&root, tmp.path(), &mut entries).unwrap();
        let names: Vec<_> = entries.iter().map(|(n, _, _, _)| n.clone()).collect();
        assert_eq!(names, vec!["d/real.txt"]);
    }

    #[test]
    fn reject_duplicate_names_allows_distinct() {
        assert!(reject_duplicate_names(["a.txt", "b.txt", "dir/a.txt"]).is_ok());
    }

    #[test]
    fn reject_duplicate_names_rejects_collision() {
        // `send a/README.md b/README.md` -- both reduce to README.md.
        let err = reject_duplicate_names(["README.md", "README.md"]).unwrap_err();
        assert!(err.to_string().contains("duplicate file name"), "{err}");
    }

    /// Writer that models `tokio::io::Stdout`'s hazard: bytes sit in an
    /// unflushed buffer and only reach the "output" on an explicit
    /// flush/shutdown. A receiver that forgets to flush loses the tail.
    #[derive(Default)]
    struct BufferedSink {
        pending: Vec<u8>,
        flushed: Vec<u8>,
    }

    impl tokio::io::AsyncWrite for BufferedSink {
        fn poll_write(
            mut self: std::pin::Pin<&mut Self>,
            _: &mut std::task::Context<'_>,
            buf: &[u8],
        ) -> std::task::Poll<std::io::Result<usize>> {
            self.pending.extend_from_slice(buf);
            std::task::Poll::Ready(Ok(buf.len()))
        }

        fn poll_flush(
            mut self: std::pin::Pin<&mut Self>,
            _: &mut std::task::Context<'_>,
        ) -> std::task::Poll<std::io::Result<()>> {
            let p = std::mem::take(&mut self.pending);
            self.flushed.extend_from_slice(&p);
            std::task::Poll::Ready(Ok(()))
        }

        fn poll_shutdown(
            self: std::pin::Pin<&mut Self>,
            cx: &mut std::task::Context<'_>,
        ) -> std::task::Poll<std::io::Result<()>> {
            self.poll_flush(cx)
        }
    }

    fn encode_one_file(name: &str, payload: &[u8]) -> Vec<u8> {
        let mut v = Vec::new();
        v.extend_from_slice(&(name.len() as u16).to_be_bytes());
        v.extend_from_slice(name.as_bytes());
        v.extend_from_slice(&(payload.len() as u64).to_be_bytes());
        v.extend_from_slice(&0u32.to_be_bytes()); // mode
        v.extend_from_slice(payload);
        v.extend_from_slice(&0u16.to_be_bytes()); // end sentinel
        v
    }

    // Regression for bug_020: a dead sibling (probe resolves None immediately)
    // must not beat a slower live stream, and must not abort the race.
    #[tokio::test]
    async fn race_first_ready_skips_dead_and_picks_live() {
        let items = vec![0u8, 1u8];
        let res = race_first_ready(items, |i| {
            Box::pin(async move {
                if i == 0 {
                    None // dead sibling: closed before pairing
                } else {
                    tokio::time::sleep(std::time::Duration::from_millis(20)).await;
                    Some(Ok::<u32, anyhow::Error>(42))
                }
            })
        })
        .await
        .unwrap();
        assert_eq!(res, Some(42));
    }

    #[tokio::test]
    async fn race_first_ready_all_dead_returns_none() {
        let items = vec![0u8, 0u8, 0u8];
        let res: Option<u32> =
            race_first_ready(items, |_| Box::pin(async move { None })).await.unwrap();
        assert_eq!(res, None);
    }

    #[tokio::test]
    async fn race_first_ready_propagates_hard_error() {
        let items = vec![7u8];
        let err = race_first_ready(items, |_| {
            Box::pin(async move {
                Some(Err::<u32, anyhow::Error>(anyhow::anyhow!("unexpected signal")))
            })
        })
        .await
        .unwrap_err();
        assert!(err.to_string().contains("unexpected signal"));
    }

    // Regression for bug_022: the stdout receive path must flush at end of
    // transfer, or the tail of a non-newline-terminated payload (a tar/gzip
    // stream) is silently dropped.
    #[tokio::test]
    async fn receive_to_writer_flushes_tail() {
        let payload = [0xDEu8, 0xAD, 0xBE, 0xEF]; // no trailing newline
        let input = encode_one_file("a.bin", &payload);
        let mut reader: &[u8] = &input;
        let mut sink = BufferedSink::default();

        let n = receive_to_writer(&mut reader, &mut sink).await.unwrap();

        assert_eq!(n, 1);
        assert!(sink.pending.is_empty(), "bytes left unflushed -- tail would be lost");
        assert_eq!(sink.flushed, payload, "flushed output must be the full payload");
    }

    #[tokio::test]
    async fn receive_to_writer_concats_multiple_files() {
        let mut input = encode_one_file("a", b"hello");
        // Second file: reuse encoder but strip its leading-in-nothing; just
        // build a two-file stream manually to exercise the loop.
        input.truncate(input.len() - 2); // drop first sentinel
        input.extend_from_slice(&(1u16).to_be_bytes());
        input.extend_from_slice(b"b");
        input.extend_from_slice(&(5u64).to_be_bytes());
        input.extend_from_slice(&0u32.to_be_bytes());
        input.extend_from_slice(b"world");
        input.extend_from_slice(&0u16.to_be_bytes());

        let mut reader: &[u8] = &input;
        let mut sink = BufferedSink::default();
        let n = receive_to_writer(&mut reader, &mut sink).await.unwrap();
        assert_eq!(n, 2);
        assert_eq!(sink.flushed, b"helloworld");
    }
}
