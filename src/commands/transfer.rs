use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};

use super::util::{resolve_ctl_path, split_target};
use gritty::protocol::{
    TRANSFER_GO_BUSY as GO_BUSY, TRANSFER_GO_PAIRED as GO_PAIRED,
    TRANSFER_GO_SUPERSEDED as GO_SUPERSEDED,
};
use gritty::ui;

/// The pairing race gave up on every session: why, per session when the
/// server said (see the `TRANSFER_GO_*` bytes), else the generic causes.
fn unpaired_message(peer: &str, reasons: &[String]) -> String {
    if reasons.is_empty() {
        return format!(
            "no {peer} paired: every offered session closed the offer (a newer `gritty send` in \
             the same session replaces an earlier one; a session mid-transfer takes no new offers)"
        );
    }
    format!("no {peer} paired: {}", reasons.join("; "))
}

fn timeout_message(peer: &str, secs: u64) -> String {
    format!(
        "no {peer} paired within {secs}s (raise --timeout, or --no-timeout to wait indefinitely)"
    )
}

#[derive(Debug, PartialEq, Eq)]
enum GoOutcome {
    Paired,
    /// This session is out of the race; the text goes into [`unpaired_message`].
    Skipped(String),
    /// A byte outside the protocol: something is badly wrong, abort.
    Protocol(String),
}

/// Interpret what a waiting sender read from one session's svc socket:
/// `Ok(byte)`, or `Err(())` for EOF. `label` is `None` when `send` runs
/// inside the session itself (one unlabeled stream via `GRITTY_SOCK`), where
/// naming the session would only be noise.
fn classify_go(read: Result<u8, ()>, label: Option<&str>) -> GoOutcome {
    let at = label.map(|l| format!("{l}: ")).unwrap_or_default();
    let session = if label.is_some() { "that session" } else { "this session" };
    match read {
        Ok(GO_PAIRED) => GoOutcome::Paired,
        Ok(GO_SUPERSEDED) => {
            GoOutcome::Skipped(format!("{at}replaced by a newer gritty send in {session}"))
        }
        Ok(GO_BUSY) => GoOutcome::Skipped(format!("{at}busy with another transfer")),
        Ok(other) => {
            GoOutcome::Protocol(format!("{at}unexpected signal from server: 0x{other:02x}"))
        }
        Err(()) => GoOutcome::Skipped(format!("{at}closed before pairing")),
    }
}

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

/// The wire name `send -` gives its payload; a directory receiver writes a
/// file of this name, a `receive -` never sees it.
const STDIN_WIRE_NAME: &str = "stdin";

struct DiscoveredSession {
    /// The name to put in `SendFile` (an unnamed session's is its id).
    wire_name: String,
    ctl_path: PathBuf,
}

/// One file queued for sending: (wire name, size, mode, disk path).
type SendEntry = (String, u64, u32, PathBuf);

/// Outcome of [`uniquify_wire_names`]: the final entries plus an exact record
/// of what changed, so the sender can announce it.
#[derive(Debug)]
struct Uniquified {
    entries: Vec<SendEntry>,
    /// (disk path, new wire name) for every entry whose name changed.
    renames: Vec<(PathBuf, String)>,
    /// (dropped spelling, kept spelling) for arguments that turned out to be
    /// the same file twice.
    dropped: Vec<(PathBuf, PathBuf)>,
}

/// Directory prefixes of a wire name: `a/b/c` -> `a`, `a/b`.
fn dir_prefixes(name: &str) -> impl Iterator<Item = &str> {
    name.match_indices('/').map(|(i, _)| &name[..i])
}

/// True if any two names are equal, or one names a directory of another --
/// either way the receiver cannot materialize both.
fn names_conflict(names: &[&str]) -> bool {
    let set: std::collections::HashSet<&str> = names.iter().copied().collect();
    set.len() != names.len() || names.iter().any(|n| dir_prefixes(n).any(|p| set.contains(p)))
}

/// Resolve wire-name conflicts with each name's shortest unique path suffix
/// (uniquify style): the receiver opens each file with `truncate(true)`, so
/// equal names (e.g. `send a/x.txt b/x.txt`) would silently overwrite -- and a
/// name that is a directory prefix of another breaks the receiver's
/// `create_dir_all`. Prepend parent directory components until the names form
/// a consistent tree. Two spellings of the same file collapse to one entry;
/// distinct files with no distinguishing parent directories are an error.
fn uniquify_wire_names(entries: Vec<SendEntry>) -> anyhow::Result<Uniquified> {
    // Fast path: no conflicts (the overwhelmingly common case) means no
    // per-entry scaffolding and the entries pass through untouched.
    {
        let names: Vec<&str> = entries.iter().map(|(n, ..)| n.as_str()).collect();
        if !names_conflict(&names) {
            return Ok(Uniquified { entries, renames: Vec::new(), dropped: Vec::new() });
        }
    }

    struct Work {
        entry: SendEntry,
        orig_name: String,
        /// Trailing run of plain components of the disk path -- the only ones
        /// a wire name may contain (`sanitize_path` rejects `..` and absolute
        /// paths). The wire name is always a suffix of this list: `used`
        /// components.
        avail: Vec<String>,
        used: usize,
    }
    let mut work: Vec<Work> = entries
        .into_iter()
        .map(|entry| {
            let mut avail: Vec<String> = entry
                .3
                .components()
                .rev()
                .map_while(|c| match c {
                    std::path::Component::Normal(c) => Some(c.to_string_lossy().into_owned()),
                    _ => None,
                })
                .collect();
            avail.reverse();
            let orig_name = entry.0.clone();
            let used = entry.0.split('/').count();
            debug_assert!(
                used <= avail.len(),
                "wire name `{}` is not a suffix of disk path `{}`",
                entry.0,
                entry.3.display()
            );
            Work { entry, orig_name, avail, used }
        })
        .collect();

    let mut dropped: Vec<(PathBuf, PathBuf)> = Vec::new();
    loop {
        // Two conflict shapes, re-derived each round: entries sharing a wire
        // name, and an entry whose wire name is a directory prefix of
        // another's (the receiver writes file dest/a, then create_dir_all
        // for a/ref.md fails on it). Extension can synthesize the second
        // shape from the first, so both are checked every round.
        let mut eq_groups: Vec<Vec<usize>> = Vec::new();
        let mut prefix_groups: Vec<Vec<usize>> = Vec::new();
        {
            let mut by_name: std::collections::HashMap<&str, Vec<usize>> =
                std::collections::HashMap::new();
            for (i, w) in work.iter().enumerate() {
                by_name.entry(w.entry.0.as_str()).or_default().push(i);
            }
            eq_groups.extend(by_name.values().filter(|g| g.len() > 1).cloned());
            for (i, w) in work.iter().enumerate() {
                for p in dir_prefixes(&w.entry.0) {
                    if let Some(owners) = by_name.get(p) {
                        let mut g = owners.clone();
                        g.push(i);
                        prefix_groups.push(g);
                    }
                }
            }
        }
        if eq_groups.is_empty() && prefix_groups.is_empty() {
            let mut renames = Vec::new();
            let entries = work
                .into_iter()
                .map(|w| {
                    if w.entry.0 != w.orig_name {
                        renames.push((w.entry.3.clone(), w.entry.0.clone()));
                    }
                    w.entry
                })
                .collect();
            return Ok(Uniquified { entries, renames, dropped });
        }

        let mut to_extend = std::collections::HashSet::new();
        let mut drop = std::collections::HashSet::new();
        for group in eq_groups {
            // The same file spelled twice collapses before any extension, so
            // the survivor keeps its short name. Only canonicalize-proven
            // sameness collapses; anything else stays in the group.
            let canon: Vec<_> =
                group.iter().map(|&i| std::fs::canonicalize(&work[i].entry.3).ok()).collect();
            let mut kept: Vec<usize> = Vec::new(); // positions within group
            for (gi, &i) in group.iter().enumerate() {
                let twin = canon[gi]
                    .as_ref()
                    .and_then(|c| kept.iter().copied().find(|&kg| canon[kg].as_ref() == Some(c)));
                match twin {
                    Some(kg) => {
                        drop.insert(i);
                        dropped.push((work[i].entry.3.clone(), work[group[kg]].entry.3.clone()));
                    }
                    None => kept.push(gi),
                }
            }
            if kept.len() < 2 {
                continue;
            }
            let extendable: Vec<usize> = kept
                .iter()
                .map(|&gi| group[gi])
                .filter(|&i| work[i].used < work[i].avail.len())
                .collect();
            if extendable.is_empty() {
                let list = kept
                    .iter()
                    .map(|&gi| work[group[gi]].entry.3.display().to_string())
                    .collect::<Vec<_>>()
                    .join(", ");
                anyhow::bail!(
                    "duplicate file name `{}` ({list}): the receiver would overwrite it -- \
                     rename a file or send them in separate transfers",
                    work[group[kept[0]]].entry.0
                );
            }
            to_extend.extend(extendable);
        }
        for group in prefix_groups {
            let live: Vec<usize> = group.iter().copied().filter(|i| !drop.contains(i)).collect();
            if live.len() < 2 {
                continue;
            }
            let extendable: Vec<usize> =
                live.iter().copied().filter(|&i| work[i].used < work[i].avail.len()).collect();
            if extendable.is_empty() {
                let list = live
                    .iter()
                    .map(|&i| format!("`{}` ({})", work[i].entry.0, work[i].entry.3.display()))
                    .collect::<Vec<_>>()
                    .join(", ");
                anyhow::bail!(
                    "file/directory name collision: {list} -- \
                     rename a file or send them in separate transfers"
                );
            }
            to_extend.extend(extendable);
        }
        for i in to_extend {
            let w = &mut work[i];
            w.used += 1;
            w.entry.0 = w.avail[w.avail.len() - w.used..].join("/");
        }
        if !drop.is_empty() {
            work = work
                .into_iter()
                .enumerate()
                .filter(|(i, _)| !drop.contains(i))
                .map(|(_, w)| w)
                .collect();
        }
    }
}

/// Result of probing one daemon -- distinguishes a protocol-version mismatch
/// from a plain unreachable daemon so discovery can give an actionable hint.
enum ProbeOutcome {
    /// The daemon answered (the list may still be empty).
    Sessions { server_id: u64, sessions: Vec<DiscoveredSession> },
    /// The daemon is up but speaks a different protocol version.
    VersionMismatch,
    /// The daemon could not be reached or did not answer.
    Unavailable,
}

/// Collapse probe results that reached the same daemon, keeping the first.
///
/// A daemon reachable through two connect sockets (two tunnel names for one
/// host) is probed twice and reports the same ephemeral `server_id`. Keeping
/// both copies would open two transfer streams per session -- and the
/// duplicate sender arriving while a relay is active is what corrupted
/// receive-first transfers in the field.
fn dedupe_by_server_id(results: Vec<(u64, Vec<DiscoveredSession>)>) -> Vec<DiscoveredSession> {
    let mut seen = std::collections::HashSet::new();
    results
        .into_iter()
        .filter(|(id, _)| seen.insert(*id))
        .flat_map(|(_, sessions)| sessions)
        .collect()
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
        Ok(Frame::SessionInfo { sessions }) => ProbeOutcome::Sessions {
            server_id: info.server_id,
            sessions: sessions
                .into_iter()
                .map(|s| DiscoveredSession {
                    wire_name: if s.name.is_empty() { s.id.to_string() } else { s.name },
                    ctl_path: ctl_path.to_path_buf(),
                })
                .collect(),
        },
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
        anyhow::bail!(
            "no sessions to offer the transfer to: neither a local server nor any tunnel is \
             running (`gritty connect [host]` starts one)"
        );
    }

    let futures: Vec<_> = probes
        .into_iter()
        .map(|path| async move {
            tokio::time::timeout(std::time::Duration::from_secs(2), probe_daemon_sessions(&path))
                .await
                .unwrap_or(ProbeOutcome::Unavailable)
        })
        .collect();

    let mut probed: Vec<(u64, Vec<DiscoveredSession>)> = Vec::new();
    let mut saw_version_mismatch = false;
    for outcome in futures_util::future::join_all(futures).await {
        match outcome {
            ProbeOutcome::Sessions { server_id, sessions } => probed.push((server_id, sessions)),
            ProbeOutcome::VersionMismatch => saw_version_mismatch = true,
            ProbeOutcome::Unavailable => {}
        }
    }
    let results = dedupe_by_server_id(probed);

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
    /// Human-readable session label (e.g. "devbox:work"); `None` in-session.
    label: Option<String>,
}

/// `host:session` as `ls` would show it: the wire name with that host's own
/// client prefix elided, so the label is both recognizable and typeable as a
/// `--session` argument.
fn session_label(config: &gritty::config::ConfigFile, host: &str, wire_name: &str) -> String {
    let client_name = config.resolve_session(Some(host)).client_name;
    format!("{host}:{}", gritty::naming::display_session_name(wire_name, &client_name))
}

/// The "waiting" status: which sessions the offer went out to and what to
/// run on the other end. In-session (no labels) the peer is the client side;
/// otherwise the offer is live in every listed session at once and whichever
/// one answers first gets the transfer.
fn waiting_line(peer: &str, peer_command: &str, labels: &[&str]) -> String {
    match labels {
        [] => format!("waiting for {peer} -- run `{peer_command}` on the client side"),
        [one] => format!("waiting for {peer} in {one} -- run `{peer_command}` there"),
        many => format!(
            "waiting for {peer} in {} sessions ({}) -- run `{peer_command}` in one of them",
            many.len(),
            many.join(", ")
        ),
    }
}

fn labels_of(streams: &[TaggedStream]) -> Vec<&str> {
    streams.iter().filter_map(|t| t.label.as_deref()).collect()
}

/// Connect to service sockets for transfer. Returns one or more tagged streams.
/// In-session or explicit --session returns one; auto-detect returns all.
async fn connect_send_sockets(
    config: &gritty::config::ConfigFile,
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
        let label = session_label(config, &host, &session);
        return Ok(vec![TaggedStream { stream, label: Some(label) }]);
    }

    // Auto-detect: connect to ALL sessions
    let sessions = discover_all_sessions(ctl_socket.as_deref()).await?;
    let mut streams = Vec::new();
    for s in &sessions {
        if let Ok(stream) = send_file_handshake(&s.ctl_path, &s.wire_name, role).await {
            let host = super::util::host_from_ctl_path(&s.ctl_path);
            let label = session_label(config, &host, &s.wire_name);
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
    entries: &[SendEntry],
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
fn print_progress(
    name: &str,
    name_width: usize,
    transferred: u64,
    total: u64,
    last_render: &mut std::time::Instant,
) {
    if !ui::stderr_is_interactive() {
        return;
    }
    let now = std::time::Instant::now();
    if transferred < total && now.duration_since(*last_render).as_millis() < 50 {
        return;
    }
    *last_render = now;
    eprint!(
        "\x1b[2K\r{}",
        progress_line(name, name_width, transferred, total, ui::stderr_is_colored())
    );
}

/// One rendering of the bar. `name_width` is the widest name of the batch,
/// so consecutive files' bars line up in one column.
fn progress_line(
    name: &str,
    name_width: usize,
    transferred: u64,
    total: u64,
    colored: bool,
) -> String {
    use gritty::ui::sgr::{DIM, GREEN, RESET};

    let pct = (transferred * 100).checked_div(total).map_or(100, |v| v.min(100));
    let bar_width = 20usize;
    let filled = (pct as usize * bar_width / 100).min(bar_width);
    let empty = bar_width - filled;
    let (green, dim, reset) = if colored { (GREEN, DIM, RESET) } else { ("", "", "") };
    format!(
        "  {name:<name_width$}  {green}{}{dim}{}{reset}  {pct}%  {}/{}",
        "=".repeat(filled),
        "-".repeat(empty),
        gritty::client::format_size(transferred),
        gritty::client::format_size(total),
    )
}

/// End-of-file line for the progress display: paint the completed bar (also
/// the only line a zero-byte file ever gets, since the copy loop never runs
/// for it) and move off the row. Interactive-only, like [`print_progress`]
/// -- an unconditional newline here put one blank line per file into every
/// piped/redirected stderr, where no bar was ever drawn.
fn finish_progress(name: &str, name_width: usize, total: u64) {
    if !ui::stderr_is_interactive() {
        return;
    }
    // transferred == total bypasses the render throttle, so any instant does.
    print_progress(name, name_width, total, total, &mut std::time::Instant::now());
    eprintln!();
}

/// Recursively walk a directory, collecting regular files with paths relative to `base`.
fn walk_dir(dir: &Path, base: &Path, entries: &mut Vec<SendEntry>) -> anyhow::Result<()> {
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
            // Keep only plain components: `send -r .` yields `./x`-style rel
            // paths whose `.` the daemon's sanitize_filename rejects.
            let wire_name = rel
                .components()
                .filter_map(|c| match c {
                    std::path::Component::Normal(c) => Some(c.to_string_lossy()),
                    _ => None,
                })
                .collect::<Vec<_>>()
                .join("/");
            let meta = std::fs::metadata(&path)?;
            entries.push((wire_name, meta.len(), meta.permissions().mode(), path));
        }
    }
    Ok(())
}

pub(crate) async fn send_command(
    config: &gritty::config::ConfigFile,
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
    let mut entries: Vec<SendEntry> = Vec::with_capacity(files.len());
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
    let uniq = uniquify_wire_names(entries)?;
    for (dup, kept) in &uniq.dropped {
        ui::status(&format!("skipping {}: same file as {}", dup.display(), kept.display()));
    }
    if !uniq.renames.is_empty() {
        let list = uniq
            .renames
            .iter()
            .map(|(path, name)| format!("{} as {name}", path.display()))
            .collect::<Vec<_>>()
            .join(", ");
        ui::status(&format!("duplicate names disambiguated: sending {list}"));
    }
    let entries = uniq.entries;

    let tagged = connect_send_sockets(
        config,
        ctl_socket,
        session,
        gritty::protocol::SvcRequest::Send.to_byte(),
    )
    .await?;

    // Write the manifest on every discovered stream, best-effort: a stale or
    // broken session must not abort the whole transfer and discard healthy
    // sessions that were ready to pair ("first receiver wins").
    let manifest = match &stdin_temp {
        Some((_, size)) => {
            ui::status(&format!(
                "stdin will arrive as a file named `{STDIN_WIRE_NAME}` (a `gritty receive -` \
                 streams it to stdout instead)"
            ));
            vec![(STDIN_WIRE_NAME.to_string(), *size, 0o644u32, PathBuf::new())]
        }
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
    ui::status(&waiting_line("receiver", "gritty receive", &labels_of(&tagged)));
    let reasons: std::sync::Arc<std::sync::Mutex<Vec<String>>> = Default::default();
    let select = race_first_ready(tagged, |mut ts| {
        let reasons = std::sync::Arc::clone(&reasons);
        Box::pin(async move {
            let mut go = [0u8; 1];
            let read = ts.stream.read_exact(&mut go).await.map(|_| go[0]).map_err(|_| ());
            match classify_go(read, ts.label.as_deref()) {
                GoOutcome::Paired => Some(Ok(ts)),
                GoOutcome::Skipped(why) => {
                    reasons.lock().unwrap_or_else(|p| p.into_inner()).push(why);
                    None // out of the race; keep waiting on the survivors
                }
                GoOutcome::Protocol(msg) => Some(Err(anyhow::anyhow!(msg))),
            }
        })
    });
    let ts = if let Some(secs) = timeout {
        tokio::time::timeout(std::time::Duration::from_secs(secs), select)
            .await
            .map_err(|_| anyhow::anyhow!("{}", timeout_message("receiver", secs)))??
    } else {
        select.await?
    }
    .ok_or_else(|| {
        let reasons = reasons.lock().unwrap_or_else(|p| p.into_inner());
        anyhow::anyhow!("{}", unpaired_message("receiver", &reasons))
    })?;
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
        ui::status(&format!("sending {} ({total_str})", ui::count(entries.len(), "file")));

        let name_width = entries.iter().map(|(name, ..)| name.chars().count()).max().unwrap_or(0);
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
                print_progress(name, name_width, transferred, *size, &mut last_render);
            }
            finish_progress(name, name_width, *size);
        }
    }

    ui::success("done");
    Ok(())
}

/// One entry of the receive stream: `(wire name, size, mode)`, or `None` at
/// the end-of-transfer sentinel. Shared by both receive modes so they accept
/// and reject exactly the same inputs.
async fn read_entry_header<R>(reader: &mut R) -> anyhow::Result<Option<(String, u64, u32)>>
where
    R: tokio::io::AsyncRead + Unpin,
{
    use tokio::io::AsyncReadExt;

    let mut buf2 = [0u8; 2];
    reader.read_exact(&mut buf2).await?;
    let name_len = u16::from_be_bytes(buf2) as usize;
    if name_len == 0 {
        return Ok(None);
    }
    let mut name_buf = vec![0u8; name_len];
    reader.read_exact(&mut name_buf).await?;
    let name = sanitize_path(&String::from_utf8(name_buf)?)?;
    let mut buf8 = [0u8; 8];
    reader.read_exact(&mut buf8).await?;
    let size = u64::from_be_bytes(buf8);
    let mut buf4 = [0u8; 4];
    reader.read_exact(&mut buf4).await?;
    Ok(Some((name, size, u32::from_be_bytes(buf4))))
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
    while let Some((_name, file_size, _mode)) = read_entry_header(reader).await? {
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

/// A file being received, written beside its final path and renamed into
/// place only once complete. Dropping it uncommitted removes the partial, so
/// an interrupted transfer leaves neither a truncated target nor a stray
/// temporary -- and a pre-existing file at the target survives untouched
/// until the replacement is whole.
struct PartialFile {
    tmp: PathBuf,
    committed: bool,
}

impl PartialFile {
    fn tmp_path_for(target: &Path) -> PathBuf {
        let mut name = std::ffi::OsString::from(".");
        name.push(target.file_name().unwrap_or_default());
        name.push(".gritty-partial");
        target.with_file_name(name)
    }

    async fn commit(mut self, target: &Path) -> std::io::Result<()> {
        tokio::fs::rename(&self.tmp, target).await?;
        self.committed = true;
        Ok(())
    }
}

impl Drop for PartialFile {
    fn drop(&mut self) {
        if !self.committed {
            let _ = std::fs::remove_file(&self.tmp);
        }
    }
}

/// Receive every entry of the stream into `dest_dir` (returning the count).
/// `file_count` is only used for the opening status line.
async fn receive_to_dir<R>(reader: &mut R, dest_dir: &Path, file_count: u32) -> anyhow::Result<u32>
where
    R: tokio::io::AsyncRead + Unpin,
{
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    let mut received = 0u32;
    // Headers arrive one file at a time, so the receiver can only align to
    // the widest name seen so far (the sender knows the whole batch up front).
    let mut name_width = 0usize;
    let mut buf = vec![0u8; 64 * 1024];
    while let Some((name, file_size, mode)) = read_entry_header(reader).await? {
        if received == 0 {
            ui::status(&format!("receiving {}", ui::count(file_count as usize, "file")));
        }
        name_width = name_width.max(name.chars().count());
        let target = dest_dir.join(&name);
        if let Some(parent) = target.parent() {
            tokio::fs::create_dir_all(parent)
                .await
                .map_err(|e| anyhow::anyhow!("{}: {e}", parent.display()))?;
        }
        // Expected often enough (re-sending a file you just edited) that it
        // is information, not a warning -- but never silent.
        if tokio::fs::symlink_metadata(&target).await.is_ok() {
            ui::status(&format!("replacing existing {name}"));
        }

        let partial = PartialFile { tmp: PartialFile::tmp_path_for(&target), committed: false };
        let recv_mode = if mode == 0 { 0o644 } else { mode & 0o7777 };
        let mut file = tokio::fs::OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .mode(recv_mode)
            .open(&partial.tmp)
            .await
            .map_err(|e| anyhow::anyhow!("{}: {e}", partial.tmp.display()))?;
        // `.mode()` only applies on create and is masked by the umask; set
        // the sender's mode explicitly so a re-received file ends up exactly
        // as sent.
        file.set_permissions(std::fs::Permissions::from_mode(recv_mode)).await?;

        let mut transferred = 0u64;
        let mut last_render = std::time::Instant::now();
        let copy = async {
            // `read`, not `read_exact`: bytes are credited as they arrive, so
            // the progress bar moves on slow links and an interruption
            // reports how far it actually got.
            while transferred < file_size {
                let want = ((file_size - transferred) as usize).min(buf.len());
                let n = reader.read(&mut buf[..want]).await?;
                if n == 0 {
                    return Err(std::io::Error::from(std::io::ErrorKind::UnexpectedEof));
                }
                file.write_all(&buf[..n]).await?;
                transferred += n as u64;
                print_progress(&name, name_width, transferred, file_size, &mut last_render);
            }
            file.flush().await
        };
        if let Err(e) = copy.await {
            // `partial` drops here and removes the temporary.
            anyhow::bail!(
                "transfer of {name} interrupted after {} of {}: {e}",
                gritty::client::format_size(transferred),
                gritty::client::format_size(file_size)
            );
        }
        drop(file);
        partial.commit(&target).await.map_err(|e| anyhow::anyhow!("{}: {e}", target.display()))?;
        finish_progress(&name, name_width, file_size);
        received += 1;
    }
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
    config: &gritty::config::ConfigFile,
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

    let tagged = connect_send_sockets(
        config,
        ctl_socket,
        session,
        gritty::protocol::SvcRequest::Receive.to_byte(),
    )
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
    ui::status(&waiting_line("sender", "gritty send <files>", &labels_of(&tagged)));
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
            .map_err(|_| anyhow::anyhow!("{}", timeout_message("sender", secs)))??
    } else {
        select.await?
    }
    .ok_or_else(|| anyhow::anyhow!("{}", unpaired_message("sender", &[])))?;
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

    let received = receive_to_dir(&mut stream, &dest_dir, file_count).await?;
    if received == 0 {
        ui::status("no files received");
    } else {
        ui::success(&format!(
            "received {} into {}",
            ui::count(received as usize, "file"),
            dest_dir.display()
        ));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn timeout_message_names_the_budget_and_both_ways_out() {
        let msg = timeout_message("receiver", 300);
        assert_eq!(
            msg,
            "no receiver paired within 300s (raise --timeout, or --no-timeout to wait indefinitely)"
        );
        assert!(timeout_message("sender", 5).starts_with("no sender paired within 5s"));
    }

    #[test]
    fn unpaired_message_explains_each_closed_offer() {
        // Nothing specific known: the generic explanation of what closes an
        // offer before pairing.
        let msg = unpaired_message("receiver", &[]);
        assert!(
            msg.starts_with("no receiver paired: every offered session closed the offer"),
            "{msg}"
        );
        assert!(msg.contains("newer `gritty send`"), "{msg}");

        let msg = unpaired_message(
            "receiver",
            &[
                "local:work: replaced by a newer gritty send".to_string(),
                "devbox:x: busy with another transfer".to_string(),
            ],
        );
        assert_eq!(
            msg,
            "no receiver paired: local:work: replaced by a newer gritty send; devbox:x: busy with another transfer"
        );
    }

    #[test]
    fn go_signal_outcomes() {
        let work = Some("local:work");
        assert_eq!(classify_go(Ok(GO_PAIRED), work), GoOutcome::Paired);
        assert_eq!(
            classify_go(Ok(GO_SUPERSEDED), work),
            GoOutcome::Skipped(
                "local:work: replaced by a newer gritty send in that session".into()
            )
        );
        assert_eq!(
            classify_go(Ok(GO_BUSY), work),
            GoOutcome::Skipped("local:work: busy with another transfer".into())
        );
        // EOF: the session went away (or an older server dropped us); no claim made.
        assert_eq!(
            classify_go(Err(()), work),
            GoOutcome::Skipped("local:work: closed before pairing".into())
        );
        assert!(matches!(classify_go(Ok(0x7f), work), GoOutcome::Protocol(_)));
        // Run inside the session (no label): no "?:" prefix, and "this session".
        assert_eq!(
            classify_go(Ok(GO_SUPERSEDED), None),
            GoOutcome::Skipped("replaced by a newer gritty send in this session".into())
        );
    }

    #[test]
    fn progress_lines_pad_the_name_to_the_batch_width() {
        let width = "longer-name".len();
        let a = progress_line("a", width, 50, 100, false);
        let b = progress_line("longer-name", width, 100, 100, false);
        // Both bars start in the same column: the name cell is padded.
        assert_eq!(a.find('=').unwrap(), b.find('=').unwrap(), "{a:?} vs {b:?}");
        assert!(a.contains("50%") && a.contains("50 B/100 B"), "{a}");
        assert!(a.starts_with("  a          "), "{a:?}");
        assert!(b.contains("100%"), "{b}");
    }

    #[test]
    fn waiting_line_names_where_the_offer_went() {
        assert_eq!(
            waiting_line("receiver", "gritty receive", &[]),
            "waiting for receiver -- run `gritty receive` on the client side"
        );
        assert_eq!(
            waiting_line("receiver", "gritty receive", &["devbox:work"]),
            "waiting for receiver in devbox:work -- run `gritty receive` there"
        );
        assert_eq!(
            waiting_line("sender", "gritty send <files>", &["devbox:work", "local:0"]),
            "waiting for sender in 2 sessions (devbox:work, local:0) -- run `gritty send <files>` in one of them"
        );
    }

    #[test]
    fn session_label_matches_ls_display_form() {
        let config = gritty::config::ConfigFile::default();
        let me = config.resolve_session(Some("devbox")).client_name;
        assert_eq!(session_label(&config, "devbox", &format!("{me}/work")), "devbox:work");
        assert_eq!(session_label(&config, "devbox", "pat/work"), "devbox:pat/work");
        assert_eq!(session_label(&config, "local", "7"), "local:7");
    }

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

    fn ds(wire_name: &str, ctl_path: &str) -> DiscoveredSession {
        DiscoveredSession { wire_name: wire_name.into(), ctl_path: PathBuf::from(ctl_path) }
    }

    // Regression: a daemon reachable through two tunnel sockets was probed
    // twice, so every session got two sender connections -- the duplicate
    // arriving mid-relay aborted receive-first transfers.
    #[test]
    fn dedupe_by_server_id_drops_second_route_to_same_daemon() {
        let deduped = dedupe_by_server_id(vec![
            (7, vec![ds("a/0", "/t/connect-fate.sock"), ds("a/1", "/t/connect-fate.sock")]),
            (7, vec![ds("a/0", "/t/connect-fate2.sock"), ds("a/1", "/t/connect-fate2.sock")]),
            (9, vec![ds("b/0", "/t/ctl.sock")]),
        ]);
        let got: Vec<_> =
            deduped.iter().map(|d| (d.wire_name.as_str(), d.ctl_path.to_str().unwrap())).collect();
        assert_eq!(
            got,
            vec![
                ("a/0", "/t/connect-fate.sock"),
                ("a/1", "/t/connect-fate.sock"),
                ("b/0", "/t/ctl.sock"),
            ]
        );
    }

    #[test]
    fn dedupe_by_server_id_keeps_distinct_daemons() {
        let deduped = dedupe_by_server_id(vec![
            (1, vec![ds("a/0", "/t/ctl.sock")]),
            (2, vec![ds("a/0", "/t/connect-other.sock")]),
        ]);
        assert_eq!(deduped.len(), 2);
    }

    fn entry(name: &str, path: impl Into<PathBuf>) -> SendEntry {
        (name.to_string(), 0, 0o644, path.into())
    }

    fn wire_names(entries: &[SendEntry]) -> Vec<&str> {
        entries.iter().map(|(n, ..)| n.as_str()).collect()
    }

    #[test]
    fn uniquify_leaves_distinct_names_alone() {
        let out = uniquify_wire_names(vec![
            entry("a.txt", "x/a.txt"),
            entry("b.txt", "x/b.txt"),
            entry("dir/a.txt", "p/dir/a.txt"),
        ])
        .unwrap();
        assert_eq!(wire_names(&out.entries), ["a.txt", "b.txt", "dir/a.txt"]);
        assert!(out.renames.is_empty() && out.dropped.is_empty());
    }

    #[test]
    fn uniquify_prefixes_parent_dir_on_collision() {
        // `send docs/a/reference.md docs/b/reference.md`
        let out = uniquify_wire_names(vec![
            entry("reference.md", "docs/a/reference.md"),
            entry("reference.md", "docs/b/reference.md"),
        ])
        .unwrap();
        assert_eq!(wire_names(&out.entries), ["a/reference.md", "b/reference.md"]);
    }

    #[test]
    fn uniquify_reports_exact_renames() {
        let out = uniquify_wire_names(vec![
            entry("reference.md", "docs/a/reference.md"),
            entry("reference.md", "docs/b/reference.md"),
        ])
        .unwrap();
        let renames: Vec<(String, String)> =
            out.renames.iter().map(|(p, n)| (p.display().to_string(), n.clone())).collect();
        assert_eq!(
            renames,
            [
                ("docs/a/reference.md".to_string(), "a/reference.md".to_string()),
                ("docs/b/reference.md".to_string(), "b/reference.md".to_string()),
            ]
        );
    }

    #[test]
    fn uniquify_extends_until_names_diverge() {
        // One parent level is not enough: the shared x/ must be walked past.
        let out =
            uniquify_wire_names(vec![entry("ref.md", "a/x/ref.md"), entry("ref.md", "b/x/ref.md")])
                .unwrap();
        assert_eq!(wire_names(&out.entries), ["a/x/ref.md", "b/x/ref.md"]);
    }

    #[test]
    fn uniquify_keeps_exhausted_name_when_sibling_extends() {
        // `send ref.md a/ref.md` -- the bare name has no parents to add.
        let out = uniquify_wire_names(vec![entry("ref.md", "ref.md"), entry("ref.md", "a/ref.md")])
            .unwrap();
        assert_eq!(wire_names(&out.entries), ["ref.md", "a/ref.md"]);
    }

    #[test]
    fn uniquify_disambiguates_recursive_walks() {
        // `send -r x/data y/data` -- both walks emit data/f.txt.
        let out = uniquify_wire_names(vec![
            entry("data/f.txt", "x/data/f.txt"),
            entry("data/f.txt", "y/data/f.txt"),
        ])
        .unwrap();
        assert_eq!(wire_names(&out.entries), ["x/data/f.txt", "y/data/f.txt"]);
    }

    #[test]
    fn uniquify_dedupe_keeps_the_short_name() {
        // `send docs/notes.txt docs/notes.txt` must arrive as notes.txt --
        // the survivor must not grow a docs/ prefix on its way out.
        let tmp = tempfile::tempdir().unwrap();
        std::fs::create_dir(tmp.path().join("docs")).unwrap();
        let p = tmp.path().join("docs/notes.txt");
        std::fs::write(&p, "x").unwrap();
        let out =
            uniquify_wire_names(vec![entry("notes.txt", &p), entry("notes.txt", &p)]).unwrap();
        assert_eq!(wire_names(&out.entries), ["notes.txt"]);
        assert!(out.renames.is_empty());
        assert_eq!(out.dropped.len(), 1);
    }

    #[test]
    fn uniquify_dedupes_rel_and_abs_spellings_of_one_file() {
        // nextest runs one process per test, so chdir is safe here.
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(tmp.path().join("f.txt"), "x").unwrap();
        std::env::set_current_dir(tmp.path()).unwrap();
        let out = uniquify_wire_names(vec![
            entry("f.txt", "f.txt"),
            entry("f.txt", tmp.path().join("f.txt")),
        ])
        .unwrap();
        assert_eq!(wire_names(&out.entries), ["f.txt"]);
        assert_eq!(out.dropped.len(), 1);
    }

    #[test]
    fn uniquify_resolves_file_vs_directory_prefix() {
        // `a` (a plain file) and `a/ref.md` cannot both materialize at the
        // receiver: writing dest/a then create_dir_all(dest/a) fails.
        let out =
            uniquify_wire_names(vec![entry("a", "notes/a"), entry("a/ref.md", "docs/a/ref.md")])
                .unwrap();
        assert_eq!(wire_names(&out.entries), ["notes/a", "docs/a/ref.md"]);
    }

    #[test]
    fn uniquify_resolves_synthesized_directory_prefix_conflicts() {
        // `send notes/a docs/a/ref.md docs/b/ref.md`: disambiguation invents
        // a/ref.md, which collides with the plain file `a` as a directory.
        let out = uniquify_wire_names(vec![
            entry("a", "notes/a"),
            entry("ref.md", "docs/a/ref.md"),
            entry("ref.md", "docs/b/ref.md"),
        ])
        .unwrap();
        assert_eq!(wire_names(&out.entries), ["notes/a", "docs/a/ref.md", "b/ref.md"]);
    }

    #[test]
    fn uniquify_errors_on_unresolvable_prefix_conflict() {
        let err =
            uniquify_wire_names(vec![entry("a", "a"), entry("a/ref.md", "a/ref.md")]).unwrap_err();
        assert!(err.to_string().contains("collision"), "{err}");
    }

    #[test]
    fn uniquify_error_names_the_conflicting_disk_paths() {
        // `..` cannot appear in a wire name, so neither path has usable
        // parent components -- and neither file exists, so canonicalization
        // cannot prove they are the same file. Refusing is the safe default,
        // and the error must name the disk paths, not a synthesized wire name.
        let tmp = tempfile::tempdir().unwrap();
        let a = tmp.path().join("a/../ref.md");
        let b = tmp.path().join("b/../ref.md");
        let err = uniquify_wire_names(vec![entry("ref.md", &a), entry("ref.md", &b)]).unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("duplicate file name"), "{msg}");
        assert!(msg.contains(&a.display().to_string()), "{msg}");
        assert!(msg.contains(&b.display().to_string()), "{msg}");
    }

    #[test]
    fn walk_dir_normalizes_dot_prefixed_names() {
        // `send -r .` -- base becomes "" and rel paths keep their leading
        // `./`, which the daemon's sanitize_filename rejects. nextest runs
        // one process per test, so chdir is safe here.
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(tmp.path().join("f.txt"), "x").unwrap();
        std::fs::create_dir(tmp.path().join("sub")).unwrap();
        std::fs::write(tmp.path().join("sub/b.txt"), "y").unwrap();
        std::env::set_current_dir(tmp.path()).unwrap();

        let dot = PathBuf::from(".");
        let mut entries = Vec::new();
        walk_dir(&dot, dot.parent().unwrap(), &mut entries).unwrap();
        let mut names = wire_names(&entries);
        names.sort();
        assert_eq!(names, ["f.txt", "sub/b.txt"]);
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

    /// Encode a file entry with an explicit mode and no sentinel, so streams
    /// of several files (or truncated ones) can be assembled.
    fn encode_entry(name: &str, payload: &[u8], mode: u32) -> Vec<u8> {
        let mut v = Vec::new();
        v.extend_from_slice(&(name.len() as u16).to_be_bytes());
        v.extend_from_slice(name.as_bytes());
        v.extend_from_slice(&(payload.len() as u64).to_be_bytes());
        v.extend_from_slice(&mode.to_be_bytes());
        v.extend_from_slice(payload);
        v
    }

    fn dir_entries(dir: &Path) -> Vec<String> {
        let mut names: Vec<String> = std::fs::read_dir(dir)
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        names.sort();
        names
    }

    #[tokio::test]
    async fn receive_to_dir_writes_files_and_leaves_no_temporaries() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let mut input = encode_entry("a.txt", b"aaa", 0o600);
        input.extend(encode_entry("sub/b.sh", b"#!/bin/sh\n", 0o755));
        input.extend_from_slice(&0u16.to_be_bytes());
        let mut reader: &[u8] = &input;

        let n = receive_to_dir(&mut reader, dir.path(), 2).await.unwrap();

        assert_eq!(n, 2);
        assert_eq!(std::fs::read(dir.path().join("a.txt")).unwrap(), b"aaa");
        assert_eq!(std::fs::read(dir.path().join("sub/b.sh")).unwrap(), b"#!/bin/sh\n");
        let mode =
            |p: &str| std::fs::metadata(dir.path().join(p)).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode("a.txt"), 0o600);
        assert_eq!(mode("sub/b.sh"), 0o755);
        assert_eq!(dir_entries(dir.path()), ["a.txt", "sub"], "no temp files left behind");
    }

    #[tokio::test]
    async fn receive_to_dir_replaces_an_existing_file_including_its_mode() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("a.txt");
        std::fs::write(&target, b"old contents, longer than the new ones").unwrap();
        std::fs::set_permissions(&target, std::fs::Permissions::from_mode(0o777)).unwrap();
        let mut input = encode_entry("a.txt", b"new", 0o644);
        input.extend_from_slice(&0u16.to_be_bytes());
        let mut reader: &[u8] = &input;

        receive_to_dir(&mut reader, dir.path(), 1).await.unwrap();

        assert_eq!(std::fs::read(&target).unwrap(), b"new");
        // Previously the mode was only applied on create, so a re-received
        // file silently kept whatever mode it had.
        assert_eq!(std::fs::metadata(&target).unwrap().permissions().mode() & 0o777, 0o644);
        assert_eq!(dir_entries(dir.path()), ["a.txt"]);
    }

    #[tokio::test]
    async fn interrupted_receive_keeps_the_old_file_and_names_the_casualty() {
        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("big.bin");
        std::fs::write(&target, b"previous version").unwrap();
        // Header promises 1000 bytes; the sender dies after 10.
        let mut input = encode_entry("big.bin", &[7u8; 1000], 0o644);
        input.truncate(input.len() - 990);
        let mut reader: &[u8] = &input;

        let err = receive_to_dir(&mut reader, dir.path(), 1).await.unwrap_err();

        let msg = format!("{err:#}");
        assert!(msg.contains("big.bin"), "{msg}");
        assert!(msg.contains("10 B") && msg.contains("1000 B"), "must say how far it got: {msg}");
        assert_eq!(std::fs::read(&target).unwrap(), b"previous version", "target untouched");
        assert_eq!(dir_entries(dir.path()), ["big.bin"], "partial removed");
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
