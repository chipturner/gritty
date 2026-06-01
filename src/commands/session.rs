use std::path::{Path, PathBuf};
use tracing::Instrument;

use super::AutoStart;
use super::util::{
    DaemonProbe, discover_daemon_probes, format_age, format_timestamp, resolve_session_id,
    server_request,
};

/// The shared session-table column headers (see [`session_status_cols`]).
const SESSION_TABLE_HEADERS: [&str; 10] =
    ["ID", "Name", "Cmd", "CWD", "Client", "PTY", "PID", "Created", "Idle", "Status"];

fn client_config(
    name: &str,
    session_id: u32,
    ctl_path: &Path,
    settings: &gritty::config::SessionSettings,
    server_id: u64,
) -> gritty::client::ClientConfig {
    gritty::client::ClientConfig {
        session: name.to_string(),
        session_id,
        ctl_path: ctl_path.to_path_buf(),
        env_vars: vec![],
        no_escape: settings.no_escape,
        forward_agent: settings.forward_agent,
        forward_open: settings.forward_open,
        oauth_redirect: settings.oauth_redirect,
        oauth_timeout: settings.oauth_timeout,
        heartbeat_interval: settings.heartbeat_interval,
        heartbeat_timeout: settings.heartbeat_timeout,
        client_name: settings.client_name.clone(),
        expected_server_id: server_id,
        device_id: gritty::get_or_create_device_id(),
    }
}

/// Boolean options for [`connect_session`], grouped into one struct because
/// seven adjacent bare `bool` parameters are silently transposable at the call
/// site (mirrors the existing `SessionSettings` grouping pattern).
pub(crate) struct ConnectFlags {
    pub(crate) detach: bool,
    pub(crate) no_create: bool,
    pub(crate) force: bool,
    pub(crate) pick: bool,
    pub(crate) no_pick: bool,
    pub(crate) new_session: bool,
    pub(crate) wait: bool,
}

pub(crate) async fn connect_session(
    session: Option<String>,
    command: Option<String>,
    flags: ConnectFlags,
    settings: gritty::config::SessionSettings,
    ctl_path: PathBuf,
    auto_start_mode: AutoStart,
) -> anyhow::Result<()> {
    use futures_util::{SinkExt, StreamExt};
    use gritty::protocol::{Frame, FrameCodec};
    use tokio_util::codec::Framed;

    let ConnectFlags { detach, no_create, force, pick, no_pick, new_session, wait } = flags;

    // Resolve the session name. An explicit name or the interactive picker
    // can be resolved up front; -n/--new must wait until connect_or_start has
    // proven the socket reachable.
    let (preliminary_name, picker_force) = match &session {
        Some(name) => (Some(name.clone()), false),
        None if new_session => (None, false),
        None => {
            let (n, _picked, pf) =
                pick_session(pick, no_pick, &ctl_path, &settings.client_name).await;
            (Some(n), pf)
        }
    };
    let force = force || picker_force;
    let session_command = command.unwrap_or_default();

    let (stream, _auto_started) =
        super::util::connect_or_start(&ctl_path, &auto_start_mode, wait).await?;

    // -n/--new: resolve the auto-name only now. Resolving it before
    // connect_or_start collapses to `<client>/0` whenever the local tunnel
    // socket is down (e.g. after a laptop reboot), and the attach-first path
    // would then silently attach to a pre-existing remote `0` instead
    // of creating a fresh integer-slot session -- violating the --new contract.
    let name = match preliminary_name {
        Some(n) => n,
        None => auto_new_session_name(&ctl_path, &settings.client_name).await,
    };

    let mut framed = Framed::new(stream, FrameCodec);
    let info = gritty::handshake(&mut framed, gritty::get_or_create_device_id()).await?;
    gritty::require_matched_version(&info)?;
    let server_id = info.server_id;

    // Carry current terminal size so the server can resize the PTY before
    // replaying scrollback/ring buffer on reconnect. Zero for probe-only.
    let (attach_cols, attach_rows) =
        if detach { (0, 0) } else { gritty::client::get_terminal_size() };

    // Try attach first
    framed
        .send(Frame::Attach {
            session: name.clone(),
            client_name: settings.client_name.clone(),
            force,
            no_replay: detach,
            cols: attach_cols,
            rows: attach_rows,
            attach_token: 0,
            // Explicit connect: no prior stream position. attach_token == 0
            // already signals "fresh viewer" to the server, so it replays
            // scrollback context rather than an incremental resume.
            rendered_offset: 0,
            line_dirty: false,
        })
        .await?;

    match Frame::expect_from(framed.next().await)? {
        Frame::Ok if detach => {
            // Probe succeeded (connect -d): existence confirmed, not attaching.
            eprintln!("\x1b[32m\u{25b8} session {name} exists (not attaching, -d)\x1b[0m");
            return Ok(());
        }
        Frame::AttachAck { token: _, session_id } => {
            eprintln!("\x1b[32m\u{25b8} attached {name}\x1b[0m");
            let client_span = tracing::info_span!("client", session = %name, session_id);
            let code = gritty::client::run(
                framed,
                client_config(&name, session_id, &ctl_path, &settings, server_id),
            )
            .instrument(client_span)
            .await?;
            std::process::exit(code);
        }
        Frame::Error { code: gritty::protocol::ErrorCode::NoSuchSession, .. } => {
            if name == "-" {
                // `-` means "last-attached session"; creating one named `-`
                // is reserved and would fail with a misleading error.
                let host = host_from_ctl_path(&ctl_path);
                anyhow::bail!("no previously-attached session on {host}");
            }
            if no_create {
                anyhow::bail!("no such session: {name}");
            }
            // Fall through to create
        }
        Frame::Error { code: gritty::protocol::ErrorCode::AlreadyAttached, message, .. } => {
            let host = host_from_ctl_path(&ctl_path);
            eprintln!("error: {message}");
            eprintln!("  gritty connect {host}:{name} --force   to take over");
            std::process::exit(1);
        }
        Frame::Error { message, .. } => anyhow::bail!("{message}"),
        other => anyhow::bail!("unexpected response from server: {other:?}"),
    }

    // Create new session -- need a fresh connection since the previous one
    // was consumed by the failed attach
    let (stream, _) = super::util::connect_or_start(&ctl_path, &auto_start_mode, wait).await?;
    let mut framed = Framed::new(stream, FrameCodec);
    let info = gritty::handshake(&mut framed, gritty::get_or_create_device_id()).await?;
    gritty::require_matched_version(&info)?;
    let server_id = info.server_id;
    // Get terminal size for initial PTY dimensions
    let (cols, rows) = crossterm::terminal::size().unwrap_or((0, 0));
    framed
        .send(Frame::NewSession {
            name: name.clone(),
            command: session_command,
            cwd: String::new(),
            cols,
            rows,
            client_name: settings.client_name.clone(),
        })
        .await?;

    match Frame::expect_from(framed.next().await)? {
        Frame::SessionCreated { id } => {
            eprintln!("\x1b[32m\u{25b8} session {name}\x1b[0m");

            // Daemon emits SessionCreated immediately followed by AttachAck.
            // The token echoes the device_id (client ignores it -- ownership
            // is tracked by the persistent device_id, not an ephemeral token).
            match Frame::expect_from(framed.next().await)? {
                Frame::AttachAck { .. } => {}
                other => anyhow::bail!("unexpected response from server: {other:?}"),
            }

            // Send Env immediately so the server's 2s deferred-spawn deadline
            // is satisfied before -d returns or alert_detached_sessions runs
            // its own multi-RTT round-trip.
            framed.send(Frame::Env { vars: gritty::collect_env_vars() }).await?;

            if detach {
                return Ok(());
            }

            // Alert about other detached sessions
            alert_detached_sessions(&name, &ctl_path, &settings.client_name).await;

            let client_span = tracing::info_span!("client", session = %name, session_id = id);
            let code = gritty::client::run(
                framed,
                client_config(&name, id, &ctl_path, &settings, server_id),
            )
            .instrument(client_span)
            .await?;
            std::process::exit(code);
        }
        Frame::Error { message, .. } => anyhow::bail!("{message}"),
        other => anyhow::bail!("unexpected response from server: {other:?}"),
    }
}

/// Resolve `-n`/`--new` to the next auto-named session slot: the first free
/// integer slot in this client's namespace (`<client>/0`, `<client>/1`, ...).
/// Falls back to `<client>/0` if the server can't be reached -- the connect
/// flow auto-starts the daemon and will re-attempt anyway.
async fn auto_new_session_name(ctl_path: &Path, client_name: &str) -> String {
    use gritty::protocol::Frame;

    let sessions = match server_request(ctl_path, Frame::ListSessions).await {
        Ok(Frame::SessionInfo { sessions }) => sessions,
        _ => return gritty::naming::resolve_session_name("0", client_name),
    };
    suggest_name(&build_rows(&sessions, client_name), client_name)
}

async fn pick_session(
    pick: bool,
    no_pick: bool,
    ctl_path: &Path,
    client_name: &str,
) -> (String, bool, bool) {
    use gritty::protocol::Frame;

    let default_wire = gritty::naming::resolve_session_name("0", client_name);

    if no_pick {
        return (default_wire, false, false);
    }

    let sessions = match server_request(ctl_path, Frame::ListSessions).await {
        Ok(Frame::SessionInfo { sessions }) => sessions,
        _ => return (default_wire, false, false),
    };

    let host = host_from_ctl_path(ctl_path);

    if pick {
        return pick_or_list(&host, &sessions, ctl_path, client_name).await;
    }

    match auto_attach_target(&sessions, client_name) {
        Some(name) => (name, false, false),
        None => pick_or_list(&host, &sessions, ctl_path, client_name).await,
    }
}

/// Decide the auto-attach target for `gritty connect host` (no session name).
/// Returns `Some(wire_name)` when the choice is unambiguous, `None` when the
/// caller should fall through to the picker.
///
/// Only sessions in `<client_name>/*` count -- foreign-namespace sessions
/// (other clients' or legacy unprefixed names) are ignored entirely. That
/// means a stale `default` left by an older gritty doesn't block creating
/// `<client>/0`, and a teammate's `pat/work` doesn't get silently adopted.
/// Reach those explicitly with the literal slash-bearing form
/// (`gritty c host:other/name`).
fn auto_attach_target(
    sessions: &[gritty::protocol::SessionEntry],
    client_name: &str,
) -> Option<String> {
    let prefix = format!("{client_name}/");
    let mine: Vec<&gritty::protocol::SessionEntry> =
        sessions.iter().filter(|s| s.name.starts_with(&prefix)).collect();

    if mine.is_empty() {
        return Some(gritty::naming::resolve_session_name("0", client_name));
    }

    let detached: Vec<&gritty::protocol::SessionEntry> =
        mine.iter().filter(|s| !s.attached).copied().collect();

    if detached.len() == 1 {
        return Some(session_wire_name(detached[0]));
    }

    None
}

/// The wire name of a session: its `name` field, or the numeric id as a string
/// for an unnamed session. Always passed back to the server verbatim, never
/// elided.
fn session_wire_name(s: &gritty::protocol::SessionEntry) -> String {
    if s.name.is_empty() { s.id.to_string() } else { s.name.clone() }
}

/// Suggest a wire name for a new session in this client's namespace. Returns
/// the lowest-numbered free slot `<client>/N` starting at 0. Non-integer
/// names (e.g. user-given labels, legacy `default` / `session-N`) do not
/// occupy integer slots, so they are ignored by the scan.
fn suggest_name(rows: &[Row], client_name: &str) -> String {
    for n in 0u32.. {
        let candidate = gritty::naming::resolve_session_name(&n.to_string(), client_name);
        if !rows.iter().any(|r| r.name == candidate) {
            return candidate;
        }
    }
    unreachable!()
}

fn print_session_list(host: &str, sessions: &[gritty::protocol::SessionEntry]) {
    if sessions.len() == 1 {
        eprintln!("error: session on {host} is already attached:");
    } else {
        eprintln!("error: multiple sessions on {host} -- specify one:");
    }
    for s in sessions {
        let wire = session_wire_name(s);
        let suffix = if s.attached { "     (attached)" } else { "" };
        eprintln!("  gritty connect {host}:{wire}{suffix}");
    }
}

/// Show picker (TUI if stderr is a TTY, static list otherwise).
/// Returns `(name, true)` for interactive pick, or exits on abort/non-TTY.
async fn pick_or_list(
    host: &str,
    sessions: &[gritty::protocol::SessionEntry],
    ctl_path: &Path,
    client_name: &str,
) -> (String, bool, bool) {
    if std::io::IsTerminal::is_terminal(&std::io::stderr()) {
        match tui_pick_session(host, sessions, ctl_path, client_name).await {
            Some((name, force)) => (name, true, force),
            None => std::process::exit(1),
        }
    } else {
        print_session_list(host, sessions);
        std::process::exit(1);
    }
}

struct Row {
    name: String,
    attached: bool,
    age: String,
    cmd: String,
    cwd: String,
    client: String,
    hotkey: Option<char>, // '1'-'9' for first 9 rows
}

fn build_rows(sessions: &[gritty::protocol::SessionEntry], client_name: &str) -> Vec<Row> {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();
    let home = std::env::var("HOME").unwrap_or_default();
    // Sort own-namespace sessions first so the picker (and its `1`-`9` hotkeys)
    // surface your own sessions ahead of foreign/legacy ones. Stable so the
    // server's id-order survives within each group.
    let prefix = format!("{client_name}/");
    let mut ordered: Vec<&gritty::protocol::SessionEntry> = sessions.iter().collect();
    ordered.sort_by_key(|s| !s.name.starts_with(&prefix));
    ordered
        .iter()
        .enumerate()
        .map(|(i, s)| Row {
            name: session_wire_name(s),
            attached: s.attached,
            age: format_age(now, s.created_at),
            cmd: s.foreground_cmd.clone(),
            cwd: shorten_home(&s.cwd, &home),
            client: s.client_name.clone(),
            hotkey: if i < 9 { Some((b'1' + i as u8) as char) } else { None },
        })
        .collect()
}

fn shorten_home(path: &str, home: &str) -> String {
    if !home.is_empty() && path.starts_with(home) {
        let rest = &path[home.len()..];
        if rest.is_empty() || rest.starts_with('/') {
            return format!("~{rest}");
        }
    }
    path.to_string()
}

/// Interactive session picker. Returns selected session name (wire form), or
/// None on abort. `client_name` is the ambient prefix used to elide own-namespace
/// names in display and to namespace newly-suggested names.
async fn tui_pick_session(
    host: &str,
    sessions: &[gritty::protocol::SessionEntry],
    ctl_path: &Path,
    client_name: &str,
) -> Option<(String, bool)> {
    use crossterm::{
        event::{self, Event, KeyCode, KeyEvent, KeyModifiers},
        terminal,
    };
    use std::io::Write;

    let mut stderr = std::io::stderr();

    // Find first detached session for initial cursor position
    let initial = sessions.iter().position(|s| !s.attached).unwrap_or(0);
    let mut cursor = initial;

    let mut rows = build_rows(sessions, client_name);

    enum Mode {
        Pick,
        Input { buf: String, cursor_pos: usize, rename_of: Option<String> },
        ConfirmKill { name: String },
    }

    let mut mode = Mode::Pick;
    // Last server error from an in-picker rename/kill, shown until the next
    // attempt. Without it a rejected rename/kill is a silent no-op.
    let mut status: Option<String> = None;
    let mut prev_total_lines: usize = 0;

    struct PickerTermGuard;
    impl Drop for PickerTermGuard {
        fn drop(&mut self) {
            use std::io::Write;
            let _ = write!(std::io::stderr(), "\x1b[?25h");
            let _ = std::io::stderr().flush();
            let _ = crossterm::terminal::disable_raw_mode();
        }
    }
    // Enter raw mode
    let _ = terminal::enable_raw_mode();
    let _ = write!(stderr, "\x1b[?25l"); // hide cursor
    let _term_guard = PickerTermGuard;

    fn prev_char_boundary(s: &str, mut pos: usize) -> usize {
        while pos > 0 {
            pos -= 1;
            if s.is_char_boundary(pos) {
                break;
            }
        }
        pos
    }
    fn next_char_boundary(s: &str, pos: usize) -> usize {
        s[pos..].chars().next().map_or(pos, |c| pos + c.len_utf8())
    }

    let render = |stderr: &mut std::io::Stderr,
                  rows: &[Row],
                  cursor: usize,
                  mode: &Mode,
                  status: Option<&str>,
                  prev_total_lines: usize| {
        // Show the full wire name (`<client>/<suffix>`) in the picker so
        // the namespace is visible at a glance, distinguishing your own
        // sessions from foreign ones without scanning the CLIENT column.
        // `gritty ls` still elides the ambient prefix (it has a separate
        // CLIENT column doing the same job).
        let displayed: Vec<&str> = rows.iter().map(|r| r.name.as_str()).collect();
        let name_w = displayed.iter().map(|s| s.len()).max().unwrap_or(0).max(3);
        let tag_w = 10; // "(attached)" is 10 chars
        let age_w = rows.iter().map(|r| r.age.len()).max().unwrap_or(0);
        let cmd_w = rows.iter().map(|r| r.cmd.len()).max().unwrap_or(0);
        let client_w = rows.iter().map(|r| r.client.len()).max().unwrap_or(0);
        // header + rows + new-session + hint, plus an optional status line.
        let total_lines = rows.len() + 3 + usize::from(status.is_some());

        // If we drew before, erase old output first
        if prev_total_lines > 0 {
            let _ = write!(stderr, "\x1b[{}A", prev_total_lines);
            for _ in 0..prev_total_lines {
                let _ = write!(stderr, "\x1b[2K\r\n");
            }
            let _ = write!(stderr, "\x1b[{}A", prev_total_lines);
        }

        // Show/hide cursor based on mode
        match mode {
            Mode::Pick | Mode::ConfirmKill { .. } => {
                let _ = write!(stderr, "\x1b[?25l");
            }
            Mode::Input { .. } => {
                let _ = write!(stderr, "\x1b[?25h");
            }
        }

        // Header
        let _ = write!(stderr, "\x1b[36;1mPick a session on {host}:\x1b[0m\r\n");
        for (i, row) in rows.iter().enumerate() {
            let marker = if i == cursor && matches!(mode, Mode::Pick | Mode::ConfirmKill { .. }) {
                "\x1b[32;1m\u{25b8}\x1b[0m"
            } else {
                " "
            };
            let hk = row.hotkey.map_or(String::from("  "), |c| format!("{c})"));
            let shown = displayed[i];

            if i == cursor && matches!(mode, Mode::Pick | Mode::ConfirmKill { .. }) {
                let tag = if row.attached { "(attached)" } else { "" };
                let _ = write!(
                    stderr,
                    "{marker} \x1b[2m{hk}\x1b[0m \x1b[32;1m{:<name_w$}\x1b[0m  {:<tag_w$}  \x1b[32m{:<age_w$}\x1b[0m  \x1b[32m{:<cmd_w$}\x1b[0m  \x1b[32m{:<client_w$}\x1b[0m  \x1b[32m{}\x1b[0m\r\n",
                    shown, tag, row.age, row.cmd, row.client, row.cwd,
                );
            } else if row.attached {
                let _ = write!(
                    stderr,
                    "{marker} \x1b[2m{hk} {:<name_w$}  {:<tag_w$}  {:<age_w$}  {:<cmd_w$}  {:<client_w$}  {}\x1b[0m\r\n",
                    shown, "(attached)", row.age, row.cmd, row.client, row.cwd,
                );
            } else {
                let _ = write!(
                    stderr,
                    "{marker} \x1b[2m{hk}\x1b[0m {:<name_w$}  {:<tag_w$}  {:<age_w$}  {:<cmd_w$}  {:<client_w$}  {}\r\n",
                    shown, "", row.age, row.cmd, row.client, row.cwd,
                );
            }
        }
        // "New session" row / input line
        let suggested_wire = suggest_name(rows, client_name);
        let suggested = suggested_wire.as_str();
        match mode {
            Mode::Pick | Mode::ConfirmKill { .. } => {
                let marker = if cursor == rows.len() { "\x1b[32;1m\u{25b8}\x1b[0m" } else { " " };
                if cursor == rows.len() {
                    let _ = write!(
                        stderr,
                        "{marker} \x1b[2m+)\x1b[0m \x1b[32;1mnew session \x1b[2m({suggested})\x1b[0m\r\n"
                    );
                } else {
                    let _ =
                        write!(stderr, "{marker} \x1b[2m+) new session ({suggested})\x1b[0m\r\n");
                }
            }
            Mode::Input { buf, cursor_pos, rename_of } => {
                let prefix = if rename_of.is_some() { "r)" } else { "+)" };
                let (before, after) = buf.split_at(*cursor_pos);
                let cursor_ch = after.chars().next().unwrap_or(' ');
                let rest = if after.is_empty() { "" } else { &after[cursor_ch.len_utf8()..] };
                let _ = write!(
                    stderr,
                    "\x1b[32;1m\u{25b8}\x1b[0m \x1b[2m{prefix}\x1b[0m \x1b[32;1m{before}\x1b[7m{cursor_ch}\x1b[27m{rest}\x1b[0m\r\n"
                );
            }
        }
        // Hint line
        let hints = match mode {
            Mode::Pick => {
                "1-9 jump  enter select  f force  n new  c/+ new (named)  r rename  x kill  esc quit"
                    .to_string()
            }
            Mode::Input { rename_of: Some(_), .. } => "enter rename  esc back".to_string(),
            Mode::Input { .. } => "enter create  esc back".to_string(),
            Mode::ConfirmKill { name } => format!("kill {name}? y/n"),
        };
        let _ = write!(stderr, "\x1b[2m  {hints}\x1b[0m\r\n");
        if let Some(msg) = status {
            let _ = write!(stderr, "\x1b[31m  {msg}\x1b[0m\r\n");
        }
        let _ = stderr.flush();
        total_lines
    };

    // Initial render
    prev_total_lines =
        render(&mut stderr, &rows, cursor, &mode, status.as_deref(), prev_total_lines);

    let result = loop {
        // Poll-based loop so we can yield to the async runtime
        if !event::poll(std::time::Duration::from_millis(100)).unwrap_or(false) {
            tokio::task::yield_now().await;
            continue;
        }
        let Ok(ev) = event::read() else {
            break None;
        };
        match &mut mode {
            Mode::Pick => match ev {
                Event::Key(KeyEvent {
                    code: KeyCode::Up | KeyCode::Char('k'),
                    modifiers: KeyModifiers::NONE,
                    ..
                }) => {
                    cursor = cursor.saturating_sub(1);
                }
                Event::Key(KeyEvent {
                    code: KeyCode::Down | KeyCode::Char('j'),
                    modifiers: KeyModifiers::NONE,
                    ..
                }) => {
                    if cursor < rows.len() {
                        cursor += 1;
                    }
                }
                Event::Key(KeyEvent { code: KeyCode::Enter, .. }) => {
                    if cursor < rows.len() {
                        break Some((rows[cursor].name.clone(), false));
                    }
                    // On the "new session" row: create immediately with suggested name
                    break Some((suggest_name(&rows, client_name), false));
                }
                // Hotkeys 1-9
                Event::Key(KeyEvent {
                    code: KeyCode::Char(ch @ '1'..='9'),
                    modifiers: KeyModifiers::NONE,
                    ..
                }) => {
                    let idx = (ch as u8 - b'1') as usize;
                    if idx < rows.len() {
                        break Some((rows[idx].name.clone(), false));
                    }
                }
                // 'n' -> create new session immediately with the suggested name
                Event::Key(KeyEvent {
                    code: KeyCode::Char('n'),
                    modifiers: KeyModifiers::NONE,
                    ..
                }) => {
                    break Some((suggest_name(&rows, client_name), false));
                }
                // 'c' or '+' -> new session input, prompting to edit the name
                Event::Key(KeyEvent {
                    code: KeyCode::Char('c') | KeyCode::Char('+'),
                    modifiers: KeyModifiers::NONE,
                    ..
                }) => {
                    // Pre-fill the input with the suggested name in display
                    // form (e.g. `0` instead of `mylaptop/0`) so the user
                    // edits the part they care about; the prefix is
                    // re-applied via resolve_session_name on submit.
                    let suggested = suggest_name(&rows, client_name);
                    let name =
                        gritty::naming::display_session_name(&suggested, client_name).to_string();
                    let len = name.len();
                    cursor = rows.len();
                    status = None;
                    mode = Mode::Input { buf: name, cursor_pos: len, rename_of: None };
                }
                // 'x' or Delete -> kill selected session
                Event::Key(KeyEvent {
                    code: KeyCode::Char('x') | KeyCode::Delete,
                    modifiers: KeyModifiers::NONE,
                    ..
                }) => {
                    if cursor < rows.len() {
                        status = None;
                        mode = Mode::ConfirmKill { name: rows[cursor].name.clone() };
                    }
                }
                // 'r' -> rename selected session
                Event::Key(KeyEvent {
                    code: KeyCode::Char('r'),
                    modifiers: KeyModifiers::NONE,
                    ..
                }) => {
                    if cursor < rows.len() {
                        let name = rows[cursor].name.clone();
                        let len = name.len();
                        status = None;
                        mode = Mode::Input {
                            buf: name.clone(),
                            cursor_pos: len,
                            rename_of: Some(name),
                        };
                    }
                }
                // 'f' -> force-attach (take over) selected session
                Event::Key(KeyEvent {
                    code: KeyCode::Char('f'),
                    modifiers: KeyModifiers::NONE,
                    ..
                }) => {
                    if cursor < rows.len() {
                        break Some((rows[cursor].name.clone(), true));
                    }
                }
                Event::Key(KeyEvent {
                    code: KeyCode::Esc | KeyCode::Char('q'),
                    modifiers: KeyModifiers::NONE,
                    ..
                }) => {
                    break None;
                }
                Event::Key(KeyEvent {
                    code: KeyCode::Char('c') | KeyCode::Char('g'),
                    modifiers: KeyModifiers::CONTROL,
                    ..
                }) => {
                    break None;
                }
                _ => continue,
            },
            Mode::ConfirmKill { name } => match ev {
                Event::Key(KeyEvent { code: KeyCode::Char('y'), .. }) => {
                    let kill_name = name.clone();
                    let ctl = ctl_path.to_path_buf();
                    match server_request(
                        &ctl,
                        gritty::protocol::Frame::KillSession { session: kill_name },
                    )
                    .await
                    {
                        Ok(gritty::protocol::Frame::Ok) => status = None,
                        Ok(gritty::protocol::Frame::Error { message, .. }) => {
                            status = Some(format!("kill failed: {message}"));
                        }
                        other => {
                            status = Some(format!("kill failed: unexpected response {other:?}"));
                        }
                    }
                    // Refresh session list
                    if let Ok(gritty::protocol::Frame::SessionInfo { sessions: fresh }) =
                        server_request(&ctl, gritty::protocol::Frame::ListSessions).await
                    {
                        if fresh.is_empty() {
                            break Some((
                                gritty::naming::resolve_session_name("0", client_name),
                                false,
                            ));
                        }
                        rows = build_rows(&fresh, client_name);
                        cursor = cursor.min(rows.len().saturating_sub(1));
                    }
                    mode = Mode::Pick;
                }
                _ => {
                    mode = Mode::Pick;
                }
            },
            Mode::Input { buf, cursor_pos, rename_of } => match ev {
                Event::Key(KeyEvent { code: KeyCode::Enter, .. }) => {
                    let user_input = buf.trim().to_string();
                    // Re-prefix the user's input with our namespace -- the
                    // input was pre-filled with the elided display form.
                    let new_wire = gritty::naming::resolve_session_name(&user_input, client_name);
                    // clone, don't take: on a rejected rename we stay in Input
                    // mode, and `rename_of` must still say this is a rename.
                    if let Some(old_name) = rename_of.clone() {
                        // Rename mode
                        if user_input.is_empty() || new_wire == old_name {
                            mode = Mode::Pick;
                        } else {
                            let ctl = ctl_path.to_path_buf();
                            match server_request(
                                &ctl,
                                gritty::protocol::Frame::RenameSession {
                                    session: old_name,
                                    new_name: new_wire,
                                },
                            )
                            .await
                            {
                                Ok(gritty::protocol::Frame::Ok) => {
                                    status = None;
                                    // Refresh
                                    if let Ok(gritty::protocol::Frame::SessionInfo {
                                        sessions: fresh,
                                    }) =
                                        server_request(&ctl, gritty::protocol::Frame::ListSessions)
                                            .await
                                    {
                                        rows = build_rows(&fresh, client_name);
                                        cursor = cursor.min(rows.len().saturating_sub(1));
                                    }
                                    mode = Mode::Pick;
                                }
                                // Rejected (name collision, invalid name): keep
                                // the user in rename mode with the message.
                                Ok(gritty::protocol::Frame::Error { message, .. }) => {
                                    status = Some(format!("rename failed: {message}"));
                                }
                                other => {
                                    status = Some(format!(
                                        "rename failed: unexpected response {other:?}"
                                    ));
                                }
                            }
                        }
                    } else if user_input.is_empty() {
                        mode = Mode::Pick;
                        cursor = rows.len();
                    } else {
                        break Some((new_wire, false));
                    }
                }
                Event::Key(KeyEvent { code: KeyCode::Esc, .. }) => {
                    let back_to_new = rename_of.is_none();
                    status = None;
                    mode = Mode::Pick;
                    if back_to_new {
                        cursor = rows.len();
                    }
                }
                Event::Key(KeyEvent {
                    code: KeyCode::Char('c') | KeyCode::Char('g'),
                    modifiers: KeyModifiers::CONTROL,
                    ..
                }) => {
                    let back_to_new = rename_of.is_none();
                    status = None;
                    mode = Mode::Pick;
                    if back_to_new {
                        cursor = rows.len();
                    }
                }
                // Readline: Ctrl+A -> beginning
                Event::Key(KeyEvent {
                    code: KeyCode::Char('a'),
                    modifiers: KeyModifiers::CONTROL,
                    ..
                })
                | Event::Key(KeyEvent { code: KeyCode::Home, .. }) => {
                    *cursor_pos = 0;
                }
                // Readline: Ctrl+E -> end
                Event::Key(KeyEvent {
                    code: KeyCode::Char('e'),
                    modifiers: KeyModifiers::CONTROL,
                    ..
                })
                | Event::Key(KeyEvent { code: KeyCode::End, .. }) => {
                    *cursor_pos = buf.len();
                }
                // Readline: Ctrl+U -> kill line
                Event::Key(KeyEvent {
                    code: KeyCode::Char('u'),
                    modifiers: KeyModifiers::CONTROL,
                    ..
                }) => {
                    buf.drain(..*cursor_pos);
                    *cursor_pos = 0;
                }
                // Readline: Ctrl+W -> kill word backward
                Event::Key(KeyEvent {
                    code: KeyCode::Char('w'),
                    modifiers: KeyModifiers::CONTROL,
                    ..
                }) => {
                    let before = &buf[..*cursor_pos];
                    let trimmed = before.trim_end();
                    let word_start = trimmed.rfind(' ').map_or(0, |i| i + 1);
                    buf.drain(word_start..*cursor_pos);
                    *cursor_pos = word_start;
                }
                // Backspace
                Event::Key(KeyEvent { code: KeyCode::Backspace, .. }) => {
                    if *cursor_pos > 0 {
                        let prev = prev_char_boundary(buf, *cursor_pos);
                        buf.remove(prev);
                        *cursor_pos = prev;
                    }
                }
                // Delete
                Event::Key(KeyEvent { code: KeyCode::Delete, .. }) => {
                    if *cursor_pos < buf.len() {
                        buf.remove(*cursor_pos);
                    }
                }
                // Left arrow / Ctrl+B
                Event::Key(KeyEvent { code: KeyCode::Left, .. })
                | Event::Key(KeyEvent {
                    code: KeyCode::Char('b'),
                    modifiers: KeyModifiers::CONTROL,
                    ..
                }) => {
                    *cursor_pos = prev_char_boundary(buf, *cursor_pos);
                }
                // Right arrow / Ctrl+F
                Event::Key(KeyEvent { code: KeyCode::Right, .. })
                | Event::Key(KeyEvent {
                    code: KeyCode::Char('f'),
                    modifiers: KeyModifiers::CONTROL,
                    ..
                }) => {
                    *cursor_pos = next_char_boundary(buf, *cursor_pos);
                }
                // Typing
                Event::Key(KeyEvent {
                    code: KeyCode::Char(ch),
                    modifiers: KeyModifiers::NONE | KeyModifiers::SHIFT,
                    ..
                }) => {
                    buf.insert(*cursor_pos, ch);
                    *cursor_pos += ch.len_utf8();
                }
                _ => continue,
            },
        }
        prev_total_lines =
            render(&mut stderr, &rows, cursor, &mode, status.as_deref(), prev_total_lines);
    };

    // Cleanup: erase picker lines (terminal restore handled by PickerTermGuard)
    if prev_total_lines > 0 {
        let _ = write!(stderr, "\x1b[{}A", prev_total_lines);
        for _ in 0..prev_total_lines {
            let _ = write!(stderr, "\x1b[2K\r\n");
        }
        let _ = write!(stderr, "\x1b[{}A", prev_total_lines);
    }
    let _ = stderr.flush();

    result
}

/// Extract a display-friendly host name from a ctl socket path.
fn host_from_ctl_path(ctl_path: &Path) -> String {
    // Tunnel sockets: .../connect-<host>.sock -> host is <host>
    // Local daemon: .../ctl.sock -> host is "local"
    let file = ctl_path.file_stem().and_then(|s| s.to_str()).unwrap_or("local");
    if let Some(host) = file.strip_prefix("connect-") {
        host.to_string()
    } else {
        "local".to_string()
    }
}

/// After creating a new session, show a hint if there are other detached
/// sessions the user might have forgotten about. Names are shown with our
/// own client prefix elided.
async fn alert_detached_sessions(current_name: &str, ctl_path: &Path, client_name: &str) {
    use gritty::protocol::Frame;

    let ctl_path_buf = ctl_path.to_path_buf();
    let Ok(resp) = server_request(&ctl_path_buf, Frame::ListSessions).await else {
        return;
    };
    let Frame::SessionInfo { sessions } = resp else {
        return;
    };
    let detached: Vec<_> =
        sessions.iter().filter(|s| !s.attached && session_wire_name(s) != current_name).collect();
    if detached.is_empty() {
        return;
    }
    let names: Vec<_> = detached
        .iter()
        .map(|s| {
            let wire = session_wire_name(s);
            gritty::naming::display_session_name(&wire, client_name).to_string()
        })
        .collect();
    eprintln!("\x1b[2;33m\u{25b8} detached sessions: {}\x1b[0m", names.join(", "));
}

pub(crate) async fn tail_session(target: String, ctl_path: PathBuf) -> anyhow::Result<i32> {
    use futures_util::{SinkExt, StreamExt};
    use gritty::protocol::Frame;

    // Resolve the target to a numeric id before opening the tail stream so
    // reconnect can reuse that id (the original target string may be `-`
    // or a name that can shift while we're tailing).
    let session_id = resolve_session_id(&ctl_path, &target).await?;

    let (mut framed, info) = super::util::connect_handshaked(&ctl_path, true).await?;
    let server_id = info.server_id;
    framed.send(Frame::Tail { session: session_id.to_string() }).await?;

    match Frame::expect_from(framed.next().await)? {
        Frame::Ok => {
            eprintln!("\x1b[2;33m\u{25b8} tailing {target}\x1b[0m");
            gritty::client::tail(
                session_id,
                framed,
                &ctl_path,
                server_id,
                gritty::get_or_create_device_id(),
            )
            .await
        }
        Frame::Error { message, .. } => anyhow::bail!("{message}"),
        other => anyhow::bail!("unexpected response from server: {other:?}"),
    }
}

pub(crate) async fn rename_session(
    target: String,
    new_name: String,
    ctl_path: PathBuf,
) -> anyhow::Result<()> {
    use gritty::protocol::Frame;

    match server_request(
        &ctl_path,
        Frame::RenameSession { session: target.clone(), new_name: new_name.clone() },
    )
    .await?
    {
        Frame::Ok => {
            eprintln!("\x1b[32m\u{25b8} renamed {target} -> {new_name}\x1b[0m");
            Ok(())
        }
        Frame::Error { message, .. } => anyhow::bail!("{message}"),
        other => anyhow::bail!("unexpected response from server: {other:?}"),
    }
}

/// How a single `kill-session` argument resolves: a session to kill on a
/// host, or a bare host name (no session part).
#[derive(Debug, PartialEq, Eq)]
enum KillTarget {
    Session { host: String, session: String },
    HostOnly(String),
}

/// Parse one `kill-session` argument. `host:session` splits as usual; a bare
/// word (no `:`) is a host when it names a known one (`local`, an existing
/// tunnel, or a configured alias), otherwise it's a session on `local` -- so
/// reaping after a bare `gritty ls` is just `gritty kill-session 3 5 work`.
fn parse_kill_target(
    config: &gritty::config::ConfigFile,
    target: &str,
    known_tunnels: &[String],
) -> KillTarget {
    if target.contains(':') {
        let (host, session) = super::util::parse_target(config, target);
        return match session {
            Some(session) => KillTarget::Session { host, session },
            None => KillTarget::HostOnly(host),
        };
    }
    // A bare word that resolves through an alias is unambiguously a host --
    // aliases are only ever configured for connections.
    let canonical = config.canonical_host_quiet(target);
    if canonical != target || target == "local" || known_tunnels.iter().any(|t| t == target) {
        return KillTarget::HostOnly(canonical);
    }
    KillTarget::Session { host: "local".to_string(), session: target.to_string() }
}

/// Kill one session. The input is namespace-resolved first (`3` ->
/// `mylaptop/3`, matching `connect` semantics); when that doesn't exist and
/// the input is purely numeric, it falls back to the raw session ID -- the ID
/// column in `gritty ls` -- which the daemon resolves directly.
async fn kill_one(user_session: &str, client_name: &str, ctl_path: &Path) -> anyhow::Result<()> {
    use gritty::protocol::{ErrorCode, Frame};

    let wire = gritty::naming::resolve_session_name(user_session, client_name);
    let mut resp = server_request(ctl_path, Frame::KillSession { session: wire.clone() }).await?;
    if matches!(resp, Frame::Error { code: ErrorCode::NoSuchSession, .. })
        && wire != user_session
        && user_session.parse::<u32>().is_ok()
    {
        resp = server_request(ctl_path, Frame::KillSession { session: user_session.to_string() })
            .await?;
    }
    match resp {
        Frame::Ok => {
            eprintln!("\x1b[32m\u{25b8} session {user_session} killed\x1b[0m");
            Ok(())
        }
        Frame::Error { message, .. } => anyhow::bail!("{message}"),
        other => anyhow::bail!("unexpected response from server: {other:?}"),
    }
}

/// Kill one or more sessions. Each target is parsed by [`parse_kill_target`];
/// a bare known host name lists that host's sessions instead of killing
/// anything (same as `kill-session` with no arguments). Failures don't stop
/// the remaining targets -- they're reported per target and summarized at the
/// end.
pub(crate) async fn kill_sessions(
    targets: &[String],
    ctl_socket: Option<&Path>,
    config: &gritty::config::ConfigFile,
) -> anyhow::Result<()> {
    let known_tunnels = gritty::connect::enumerate_tunnels();
    let mut failed = 0usize;
    for target in targets {
        let result = match parse_kill_target(config, target, &known_tunnels) {
            KillTarget::HostOnly(host) => {
                let ctl_path =
                    super::util::resolve_ctl_path(ctl_socket.map(Path::to_path_buf), Some(&host))?;
                let client_name = config.resolve_session(Some(&host)).client_name;
                // Always bails with the host's session listing.
                return suggest_session("kill-session", &host, &ctl_path, &client_name).await;
            }
            KillTarget::Session { host, session } => {
                let ctl_path =
                    super::util::resolve_ctl_path(ctl_socket.map(Path::to_path_buf), Some(&host))?;
                let client_name = config.resolve_session(Some(&host)).client_name;
                kill_one(&session, &client_name, &ctl_path).await
            }
        };
        if let Err(e) = result {
            eprintln!("error: {target}: {e:#}");
            failed += 1;
        }
    }
    if failed > 0 {
        anyhow::bail!("failed to kill {failed} of {} session(s)", targets.len());
    }
    Ok(())
}

pub(crate) async fn kill_server(ctl_path: PathBuf) -> anyhow::Result<()> {
    use gritty::protocol::Frame;

    // Deliberately tolerant of protocol version mismatch: killing the server
    // is the first step of the upgrade recovery ritual, so it MUST work
    // across a mismatched handshake.
    match super::util::server_request_any_version(&ctl_path, Frame::KillServer).await? {
        Frame::Ok => {
            eprintln!("\x1b[32m\u{25b8} server killed\x1b[0m");
            Ok(())
        }
        Frame::Error { message, .. } => anyhow::bail!("{message}"),
        other => anyhow::bail!("unexpected response from server: {other:?}"),
    }
}

/// Orchestrate a full daemon (+ tunnel, for remote hosts) restart. The
/// canonical recovery ritual after upgrading the `gritty` binary on one or
/// both sides: old daemon gets killed across the version mismatch, tunnel
/// is torn down and respawned (which bootstraps the remote daemon with the
/// new binary), and the local server is started if we targeted `local`.
pub(crate) async fn restart(
    host: Option<String>,
    ctl_socket: Option<PathBuf>,
    config: &gritty::config::ConfigFile,
) -> anyhow::Result<()> {
    use gritty::protocol::Frame;

    let host = host.unwrap_or_else(|| "local".to_string());
    // Snapshot the override before resolve_ctl_path consumes it -- the local
    // respawn below must land on the same socket, not the default path.
    let ctl_socket_arg = ctl_socket.as_ref().map(|p| p.to_string_lossy().into_owned());
    let ctl_path = super::util::resolve_ctl_path(ctl_socket, Some(&host))?;

    // Step 1: kill-server, tolerant of both "nothing running" and
    // protocol-version mismatch. We don't want to fail the whole restart
    // just because the daemon was already gone.
    match super::util::server_request_any_version(&ctl_path, Frame::KillServer).await {
        Ok(Frame::Ok) => {
            eprintln!("\x1b[32m\u{25b8} server killed\x1b[0m");
        }
        Ok(Frame::Error { message, .. }) => {
            eprintln!("\x1b[2;33m\u{25b8} kill-server: {message} (continuing)\x1b[0m");
        }
        Ok(other) => {
            eprintln!(
                "\x1b[2;33m\u{25b8} kill-server: unexpected response {other:?} (continuing)\x1b[0m"
            );
        }
        Err(_) => {
            // Connect failed -- no daemon to kill, that's fine.
            eprintln!("\x1b[2;33m\u{25b8} no server running\x1b[0m");
        }
    }

    if host == "local" {
        // For local, kick off a fresh `gritty server`.
        eprintln!("\x1b[2;33m\u{25b8} starting server...\x1b[0m");
        super::util::auto_start(&super::util::server_auto_start_args(ctl_socket_arg.as_deref()))?;
        eprintln!("\x1b[32m\u{25b8} server restarted\x1b[0m");
    } else {
        // Remote: capture the original destination from the .dest sidecar
        // before disconnect wipes it. Using just `host` here would collapse
        // `user@server.example.com:2222` down to the friendly connection
        // name and break SSH.
        let destination =
            gritty::connect::resolve_destination(&host, config.alias_destination(&host).as_deref());
        // Capture the recreate args (destination + persisted CLI -o options)
        // *before* disconnect wipes the sidecar files.
        let recreate = gritty::connect::tunnel_recreate_args(&host, &destination);
        // Tear down the tunnel (the supervisor may already be exiting
        // because the ctl socket vanished when the daemon died, but
        // `disconnect` is idempotent for the "already stopped" case).
        gritty::connect::disconnect(&host).await?;
        eprintln!("\x1b[2;33m\u{25b8} starting tunnel {host}...\x1b[0m");
        let recreate: Vec<&str> = recreate.iter().map(String::as_str).collect();
        super::util::auto_start(&recreate)?;
        eprintln!("\x1b[32m\u{25b8} {host} restarted\x1b[0m");
    }
    Ok(())
}

/// The ten shared session-table columns (ID..Status) for one row. Identical
/// between `list_sessions` and `list_all_sessions`; the latter just prepends a
/// Host column. Single-sourced so the "starting"/attached/heartbeat/detached
/// status logic and column order cannot drift between the two listings.
///
/// `ambient_client_name` elides that prefix from the displayed NAME column so
/// your own sessions read as `work` rather than `mylaptop/work`; pass an empty
/// string to skip elision (e.g. `--full`).
fn session_status_cols(
    s: &gritty::protocol::SessionEntry,
    now: u64,
    ambient_client_name: &str,
) -> Vec<String> {
    use super::util::{format_duration, format_idle};

    let name = if s.name.is_empty() {
        "-".to_string()
    } else {
        gritty::naming::display_session_name(&s.name, ambient_client_name).to_string()
    };
    let (pty, pid, created, idle, status) = if s.shell_pid == 0 {
        let dash = || "-".to_string();
        (dash(), dash(), dash(), dash(), "starting".to_string())
    } else {
        // Idle = time since terminal activity (PTY output / keystrokes).
        // The status parenthetical = time since client presence (attach /
        // heartbeat / detach). Both matter when deciding what to reap: a
        // detached session running a build is idle by presence but not by
        // activity.
        let status = if s.attached {
            if s.last_heartbeat > 0 {
                let ago = now.saturating_sub(s.last_heartbeat);
                format!("attached (heartbeat {ago}s ago)")
            } else {
                "attached".to_string()
            }
        } else if s.last_heartbeat > 0 {
            format!("detached ({} ago)", format_duration(now.saturating_sub(s.last_heartbeat)))
        } else {
            "detached".to_string()
        };
        (
            s.pty_path.clone(),
            s.shell_pid.to_string(),
            format_timestamp(s.created_at),
            format_idle(now, s.last_activity),
            status,
        )
    };
    vec![
        s.id.to_string(),
        name,
        s.foreground_cmd.clone(),
        s.cwd.clone(),
        s.client_name.clone(),
        pty,
        pid,
        created,
        idle,
        status,
    ]
}

/// Order sessions for the `ls` table and flag which belong to the ambient
/// client: own-namespace sessions first, then foreign sessions grouped by
/// namespace, legacy unprefixed names last. The sort is stable so the server's
/// id-order survives within each group. "Own" uses the same rule as the picker
/// and name elision -- the wire name starts with `<ambient_client_name>/`.
fn order_sessions<'a>(
    sessions: &'a [gritty::protocol::SessionEntry],
    ambient_client_name: &str,
) -> Vec<(&'a gritty::protocol::SessionEntry, bool)> {
    let prefix = format!("{ambient_client_name}/");
    let mut ordered: Vec<(&gritty::protocol::SessionEntry, bool)> = sessions
        .iter()
        .map(|s| (s, !ambient_client_name.is_empty() && s.name.starts_with(&prefix)))
        .collect();
    ordered.sort_by_key(|(s, own)| {
        let namespace = s.name.split_once('/').map(|(ns, _)| ns.to_string());
        (!own, namespace.is_none(), namespace.unwrap_or_default())
    });
    ordered
}

/// Print the session table: ordered by [`order_sessions`], own-client rows in
/// bold (only when stdout is a terminal -- scripts parsing `ls` output must
/// not see ANSI codes), every line prefixed with `indent`.
fn print_session_table(
    sessions: &[gritty::protocol::SessionEntry],
    now: u64,
    client_name: &str,
    indent: &str,
) {
    use std::io::IsTerminal;

    let ordered = order_sessions(sessions, client_name);
    let rows: Vec<Vec<String>> =
        ordered.iter().map(|(s, _)| session_status_cols(s, now, client_name)).collect();
    let lines = gritty::table::format_table(&SESSION_TABLE_HEADERS, &rows);
    let bold_ok = std::io::stdout().is_terminal();

    // lines[0] is the header; lines[i + 1] renders ordered[i].
    println!("{indent}{}", lines[0]);
    for (i, line) in lines[1..].iter().enumerate() {
        if bold_ok && ordered[i].1 {
            println!("{indent}\x1b[1m{line}\x1b[0m");
        } else {
            println!("{indent}{line}");
        }
    }
}

pub(crate) async fn list_sessions(ctl_path: PathBuf, client_name: &str) -> anyhow::Result<()> {
    use gritty::protocol::Frame;

    let resp = server_request(&ctl_path, Frame::ListSessions).await?;
    match resp {
        Frame::SessionInfo { sessions } => {
            if sessions.is_empty() {
                println!("no active sessions");
            } else {
                let now = std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .unwrap_or_default()
                    .as_secs();

                print_session_table(&sessions, now, client_name, "");
            }
            Ok(())
        }
        other => {
            anyhow::bail!("unexpected response from server: {other:?}");
        }
    }
}

/// One probed daemon endpoint: its host name, tunnel display info (when
/// reached through an SSH tunnel), and the probe outcome -- the daemon's
/// ephemeral `server_id` plus its sessions, or an error string.
struct ProbedHost {
    host: String,
    /// `(destination, status)` from the tunnel sidecar files; `None` for the
    /// local daemon and orphaned socket files.
    tunnel: Option<(String, String)>,
    outcome: Result<(u64, Vec<gritty::protocol::SessionEntry>), String>,
}

/// Hosts that resolved to the same daemon, merged for display.
struct HostGroup {
    /// `(host_name, tunnel_info)` for each member, in discovery order.
    members: Vec<(String, Option<(String, String)>)>,
    result: Result<Vec<gritty::protocol::SessionEntry>, String>,
}

/// Group probed hosts by daemon identity: successful probes that returned the
/// same `server_id` collapse into one group (two tunnel names pointing at the
/// same remote daemon would otherwise list every session twice). Failed probes
/// never merge -- without a `server_id` there is no identity to merge on.
/// First-appearance order is preserved, so `local` (discovered first) leads.
fn group_by_daemon(probed: Vec<ProbedHost>) -> Vec<HostGroup> {
    let mut keyed: Vec<(Option<u64>, HostGroup)> = Vec::new();
    for p in probed {
        let member = (p.host, p.tunnel);
        match p.outcome {
            Ok((server_id, sessions)) => {
                if let Some((_, group)) = keyed.iter_mut().find(|(id, _)| *id == Some(server_id)) {
                    group.members.push(member);
                } else {
                    keyed.push((
                        Some(server_id),
                        HostGroup { members: vec![member], result: Ok(sessions) },
                    ));
                }
            }
            Err(e) => {
                keyed.push((None, HostGroup { members: vec![member], result: Err(e) }));
            }
        }
    }
    keyed.into_iter().map(|(_, g)| g).collect()
}

/// Build the header for one host group: the joined host names plus an optional
/// parenthetical annotation (informative destinations, a "same daemon" marker
/// for merged groups, and the tunnel status). Returns `(names, annotation)`;
/// the caller styles the annotation.
fn group_header(group: &HostGroup) -> (String, Option<String>) {
    let names: Vec<&str> = group.members.iter().map(|(h, _)| h.as_str()).collect();
    let mut parts: Vec<String> = Vec::new();

    // Destinations that add information (differ from every displayed name).
    for (_, tunnel) in &group.members {
        if let Some((dest, _)) = tunnel
            && !names.contains(&dest.as_str())
            && !parts.contains(dest)
        {
            parts.push(dest.clone());
        }
    }
    if group.members.len() > 1 {
        parts.push("same daemon".to_string());
    }
    // Tunnel status: "reconnecting" anywhere wins over "healthy" -- it is the
    // state a user needs to act on. Local-only groups have no status.
    let statuses: Vec<&str> = group
        .members
        .iter()
        .filter_map(|(_, t)| t.as_ref().map(|(_, status)| status.as_str()))
        .collect();
    if !statuses.is_empty() {
        let worst = if statuses.iter().any(|s| *s != "healthy") {
            statuses.iter().find(|s| **s != "healthy").unwrap()
        } else {
            "healthy"
        };
        parts.push(worst.to_string());
    }

    let annotation = if parts.is_empty() { None } else { Some(parts.join(", ")) };
    (names.join(", "), annotation)
}

/// Probe one daemon endpoint: handshake (capturing `server_id` for identity),
/// then `ListSessions`. Bounded by a 2s timeout so one dead tunnel cannot
/// stall the whole listing.
async fn probe_host(probe: DaemonProbe) -> ProbedHost {
    use gritty::protocol::{Frame, FrameCodec};

    let DaemonProbe { host, socket, tunnel } = probe;
    let result = tokio::time::timeout(std::time::Duration::from_secs(2), async {
        let stream = gritty::security::connect_verified(&socket)
            .await
            .map_err(|e| format!("connect: {e}"))?;
        let mut framed = tokio_util::codec::Framed::new(stream, FrameCodec);
        let info = gritty::handshake(&mut framed, gritty::get_or_create_device_id())
            .await
            .map_err(|e| e.to_string())?;
        gritty::require_matched_version(&info).map_err(|e| e.to_string())?;
        futures_util::SinkExt::send(&mut framed, Frame::ListSessions)
            .await
            .map_err(|e| format!("send ListSessions: {e}"))?;
        match Frame::expect_from(futures_util::StreamExt::next(&mut framed).await) {
            Ok(Frame::SessionInfo { sessions }) => Ok((info.server_id, sessions)),
            Ok(Frame::Error { message, .. }) => Err(message),
            Ok(other) => Err(format!("unexpected response: {other:?}")),
            Err(e) => Err(e.to_string()),
        }
    })
    .await;
    let outcome = match result {
        Ok(inner) => inner,
        Err(_) => Err("probe timed out after 2s".to_string()),
    };
    ProbedHost { host, tunnel, outcome }
}

/// Whether a probed endpoint belongs on the dashboard. A bare socket file
/// (no live tunnel supervisor, not the local daemon) that refused the probe
/// is litter from a dead tunnel, not a connection -- a section for it would
/// dress up junk as state. The local daemon and live tunnels always show,
/// even when broken: those are real endpoints the user needs to act on.
fn is_listable(p: &ProbedHost) -> bool {
    p.host == "local" || p.tunnel.is_some() || p.outcome.is_ok()
}

/// The bare `gritty ls` connectivity dashboard: every known daemon (local +
/// all tunnels), grouped by daemon identity, one section per daemon. Outbound
/// connections are the tunnel sections; inbound connections show up in the
/// local section's Client column.
pub(crate) async fn list_all_sessions(config: &gritty::config::ConfigFile) -> anyhow::Result<()> {
    let probes = discover_daemon_probes();

    if probes.is_empty() {
        anyhow::bail!("no server running and no tunnels found");
    }

    let probed: Vec<ProbedHost> =
        futures_util::future::join_all(probes.into_iter().map(probe_host)).await;
    let probed: Vec<ProbedHost> = probed.into_iter().filter(is_listable).collect();
    if probed.is_empty() {
        anyhow::bail!("no server running and no tunnels found");
    }
    let groups = group_by_daemon(probed);

    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();

    for (i, group) in groups.iter().enumerate() {
        if i > 0 {
            println!();
        }
        let (names, annotation) = group_header(group);
        match annotation {
            Some(a) => println!("{names}  \x1b[2m({a})\x1b[0m"),
            None => println!("{names}"),
        }
        match &group.result {
            Ok(sessions) if sessions.is_empty() => println!("  \x1b[2m(no sessions)\x1b[0m"),
            Ok(sessions) => {
                // The client_name elision applied to the NAME column uses the
                // *resolved* client_name for this host -- a host with a
                // per-host `client-name` override (different from
                // `[defaults].client-name`) gets its own elision.
                let host = &group.members[0].0;
                let client_name = config.resolve_session(Some(host)).client_name;
                print_session_table(sessions, now, &client_name, "  ");
            }
            Err(e) => println!("  \x1b[2;33m\u{26a0} {e}\x1b[0m"),
        }
    }
    Ok(())
}

/// Print available sessions and exit with an error when a session-requiring
/// command is invoked without the session part (e.g. `gritty tail local`
/// instead of `gritty tail local:session`).
pub(crate) async fn suggest_session(
    cmd: &str,
    host: &str,
    ctl_path: &Path,
    client_name: &str,
) -> anyhow::Result<()> {
    use gritty::protocol::Frame;

    let ctl_path_buf = ctl_path.to_path_buf();
    let resp = match server_request(&ctl_path_buf, Frame::ListSessions).await {
        Ok(resp) => resp,
        Err(_) => {
            anyhow::bail!("specify a session: gritty {cmd} {host}:<session>");
        }
    };

    match resp {
        Frame::SessionInfo { sessions } if sessions.is_empty() => {
            anyhow::bail!("no active sessions on {host}");
        }
        Frame::SessionInfo { sessions } => {
            let now = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .as_secs();

            let mut msg = format!("specify a session: gritty {cmd} {host}:<session>\n\n");
            msg.push_str("  ID  Name     Age\n");
            for s in &sessions {
                let name = if s.name.is_empty() {
                    "-".to_string()
                } else {
                    gritty::naming::display_session_name(&s.name, client_name).to_string()
                };
                let age = format_age(now, s.created_at);
                msg.push_str(&format!("  {}   {:<8} {}\n", s.id, name, age));
            }
            anyhow::bail!("{msg}");
        }
        _ => anyhow::bail!("specify a session: gritty {cmd} {host}:<session>"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn row(name: &str) -> Row {
        Row {
            name: name.to_string(),
            attached: false,
            age: String::new(),
            cmd: String::new(),
            cwd: String::new(),
            client: String::new(),
            hotkey: None,
        }
    }

    #[test]
    fn suggest_name_first_call_returns_zero() {
        assert_eq!(suggest_name(&[], "mylaptop"), "mylaptop/0");
        // A foreign session in another namespace doesn't occupy our slot 0.
        assert_eq!(suggest_name(&[row("laptop2/0")], "mylaptop"), "mylaptop/0");
        // A legacy `default` in our namespace doesn't occupy slot 0 either --
        // the integer scan ignores non-integer names.
        assert_eq!(suggest_name(&[row("mylaptop/default")], "mylaptop"), "mylaptop/0");
    }

    #[test]
    fn suggest_name_increments_past_existing_integers() {
        assert_eq!(suggest_name(&[row("mylaptop/0")], "mylaptop"), "mylaptop/1");
        assert_eq!(suggest_name(&[row("mylaptop/0"), row("mylaptop/1")], "mylaptop"), "mylaptop/2");
    }

    #[test]
    fn suggest_name_fills_first_free_slot() {
        // 1 is missing in our namespace -- pick it first, not max+1.
        assert_eq!(suggest_name(&[row("mylaptop/0"), row("mylaptop/2")], "mylaptop"), "mylaptop/1");
    }

    #[test]
    fn suggest_name_ignores_non_integer_legacy_names() {
        // `default` and `session-2` from before the change shouldn't shift
        // our scan -- we always start at 0.
        let rows = vec![row("mylaptop/default"), row("mylaptop/session-2")];
        assert_eq!(suggest_name(&rows, "mylaptop"), "mylaptop/0");
    }

    fn entry() -> gritty::protocol::SessionEntry {
        gritty::protocol::SessionEntry {
            id: 3,
            name: String::new(),
            pty_path: "/dev/pts/7".to_string(),
            shell_pid: 1234,
            created_at: 0,
            attached: false,
            last_heartbeat: 0,
            foreground_cmd: "vim".to_string(),
            cwd: "/home/x".to_string(),
            client_name: "laptop".to_string(),
            agent_forwarding_active: false,
            is_last_attached: false,
            last_activity: 0,
        }
    }

    fn auto_entry(name: &str, attached: bool) -> gritty::protocol::SessionEntry {
        let mut e = entry();
        e.name = name.to_string();
        e.attached = attached;
        e
    }

    #[test]
    fn auto_attach_no_sessions_creates_zero() {
        assert_eq!(auto_attach_target(&[], "defiant"), Some("defiant/0".to_string()));
    }

    #[test]
    fn build_rows_puts_own_namespace_first() {
        // Server returns sessions sorted by id, mixing foreign and own
        // namespaces. The picker should surface our own first so the `1`-`9`
        // hotkeys land on them.
        let sessions = vec![
            auto_entry("laptop2/work", false),
            auto_entry("defiant/a", false),
            auto_entry("default", false),
            auto_entry("defiant/b", false),
        ];
        let rows = build_rows(&sessions, "defiant");
        let names: Vec<&str> = rows.iter().map(|r| r.name.as_str()).collect();
        assert_eq!(names, vec!["defiant/a", "defiant/b", "laptop2/work", "default"]);
        // Hotkeys follow the displayed order.
        assert_eq!(rows[0].hotkey, Some('1'));
        assert_eq!(rows[1].hotkey, Some('2'));
        assert_eq!(rows[2].hotkey, Some('3'));
    }

    #[test]
    fn build_rows_preserves_server_order_within_groups() {
        // Within each group (own / foreign), the server's id-order survives.
        let sessions = vec![
            auto_entry("defiant/b", false),
            auto_entry("laptop2/x", false),
            auto_entry("defiant/a", false),
            auto_entry("laptop2/y", false),
        ];
        let rows = build_rows(&sessions, "defiant");
        let names: Vec<&str> = rows.iter().map(|r| r.name.as_str()).collect();
        assert_eq!(names, vec!["defiant/b", "defiant/a", "laptop2/x", "laptop2/y"]);
    }

    #[test]
    fn auto_attach_ignores_legacy_unprefixed_name() {
        // A `default` from a pre-namespace gritty must not block us from
        // creating `defiant/0`.
        let s = vec![auto_entry("default", false)];
        assert_eq!(auto_attach_target(&s, "defiant"), Some("defiant/0".to_string()));
    }

    #[test]
    fn auto_attach_ignores_foreign_namespace() {
        let s = vec![auto_entry("laptop2/work", false)];
        assert_eq!(auto_attach_target(&s, "defiant"), Some("defiant/0".to_string()));
    }

    #[test]
    fn auto_attach_picks_lone_in_namespace_detached() {
        let s = vec![auto_entry("defiant/work", false)];
        assert_eq!(auto_attach_target(&s, "defiant"), Some("defiant/work".to_string()));
    }

    #[test]
    fn auto_attach_shows_picker_when_lone_in_namespace_attached() {
        let s = vec![auto_entry("defiant/work", true)];
        assert_eq!(auto_attach_target(&s, "defiant"), None);
    }

    #[test]
    fn auto_attach_picks_single_detached_among_in_namespace() {
        let s = vec![auto_entry("defiant/work", true), auto_entry("defiant/play", false)];
        assert_eq!(auto_attach_target(&s, "defiant"), Some("defiant/play".to_string()));
    }

    #[test]
    fn auto_attach_shows_picker_for_multiple_detached_in_namespace() {
        let s = vec![auto_entry("defiant/work", false), auto_entry("defiant/play", false)];
        assert_eq!(auto_attach_target(&s, "defiant"), None);
    }

    #[test]
    fn auto_attach_in_namespace_wins_over_foreign_detached() {
        let s = vec![
            auto_entry("defiant/work", false),
            auto_entry("laptop2/foo", false),
            auto_entry("default", false),
        ];
        assert_eq!(auto_attach_target(&s, "defiant"), Some("defiant/work".to_string()));
    }

    #[test]
    fn session_status_cols_starting_when_no_shell() {
        let mut s = entry();
        s.shell_pid = 0;
        let cols = session_status_cols(&s, 100, "");
        // id, name(-), cmd, cwd, client, pty(-), pid(-), created(-), idle(-), status
        assert_eq!(cols[0], "3");
        assert_eq!(cols[1], "-"); // empty name renders as "-"
        assert_eq!(cols[5], "-"); // pty
        assert_eq!(cols[6], "-"); // pid
        assert_eq!(cols[8], "-"); // idle
        assert_eq!(cols[9], "starting");
    }

    #[test]
    fn session_status_cols_attached_reports_heartbeat_age() {
        let mut s = entry();
        s.attached = true;
        s.last_heartbeat = 90;
        let cols = session_status_cols(&s, 100, "");
        assert_eq!(cols[9], "attached (heartbeat 10s ago)");
        assert_eq!(cols[6], "1234"); // pid
    }

    #[test]
    fn session_status_cols_detached_and_attached_no_heartbeat() {
        let s = entry();
        assert_eq!(session_status_cols(&s, 100, "")[9], "detached");
        let mut s2 = entry();
        s2.attached = true;
        assert_eq!(session_status_cols(&s2, 100, "")[9], "attached");
    }

    #[test]
    fn session_status_cols_detached_shows_presence_age() {
        // A detached session with a known last client presence (attach /
        // heartbeat / detach time) reports how long ago that was.
        let mut s = entry();
        s.last_heartbeat = 10000 - 7200; // 2h before now
        assert_eq!(session_status_cols(&s, 10000, "")[9], "detached (2h ago)");
    }

    #[test]
    fn session_status_cols_idle_from_last_activity() {
        // Idle column = time since last terminal activity, compact format.
        let mut s = entry();
        s.last_activity = 100 - 60;
        assert_eq!(session_status_cols(&s, 100, "")[8], "1m");
    }

    #[test]
    fn session_status_cols_idle_unknown_is_dash() {
        // last_activity == 0 (older server) renders as "-", not a huge age.
        let s = entry();
        assert_eq!(session_status_cols(&s, 100, "")[8], "-");
    }

    #[test]
    fn session_status_cols_elides_ambient_client_prefix() {
        let mut s = entry();
        s.name = "mylaptop/work".to_string();
        let cols = session_status_cols(&s, 100, "mylaptop");
        assert_eq!(cols[1], "work"); // own prefix elided
    }

    #[test]
    fn session_status_cols_keeps_foreign_prefix() {
        let mut s = entry();
        s.name = "laptop2/work".to_string();
        let cols = session_status_cols(&s, 100, "mylaptop");
        assert_eq!(cols[1], "laptop2/work"); // foreign prefix kept
    }

    // -- parse_kill_target (kill-session argument resolution) --

    fn session_target(host: &str, session: &str) -> KillTarget {
        KillTarget::Session { host: host.to_string(), session: session.to_string() }
    }

    fn no_aliases() -> gritty::config::ConfigFile {
        gritty::config::ConfigFile::default()
    }

    #[test]
    fn kill_target_host_session_splits() {
        let cfg = no_aliases();
        assert_eq!(parse_kill_target(&cfg, "local:3", &[]), session_target("local", "3"));
        assert_eq!(parse_kill_target(&cfg, "remote:work", &[]), session_target("remote", "work"));
    }

    #[test]
    fn kill_target_bare_word_is_local_session() {
        let cfg = no_aliases();
        // Bare IDs and names go to `local` -- the reap-after-`ls` path.
        assert_eq!(parse_kill_target(&cfg, "3", &[]), session_target("local", "3"));
        assert_eq!(parse_kill_target(&cfg, "work", &[]), session_target("local", "work"));
    }

    #[test]
    fn kill_target_bare_known_host_stays_host() {
        let cfg = no_aliases();
        // `local` and known tunnel names keep the "list that host" behavior.
        let tunnels = vec!["devbox".to_string()];
        assert_eq!(
            parse_kill_target(&cfg, "local", &tunnels),
            KillTarget::HostOnly("local".to_string())
        );
        assert_eq!(
            parse_kill_target(&cfg, "devbox", &tunnels),
            KillTarget::HostOnly("devbox".to_string())
        );
        // Unknown bare word is still a local session even when tunnels exist.
        assert_eq!(parse_kill_target(&cfg, "work", &tunnels), session_target("local", "work"));
    }

    #[test]
    fn kill_target_bare_alias_is_host() {
        // A bare word that resolves through a configured alias is a host even
        // with no live tunnel -- aliases are only configured for connections.
        let cfg: gritty::config::ConfigFile =
            toml::from_str("[host.foo]\naliases = [\"foo.bar.com\"]\n").unwrap();
        assert_eq!(
            parse_kill_target(&cfg, "foo.bar.com", &[]),
            KillTarget::HostOnly("foo".to_string())
        );
        // And the host part of host:session resolves too.
        assert_eq!(parse_kill_target(&cfg, "foo.bar.com:3", &[]), session_target("foo", "3"));
    }

    #[test]
    fn kill_target_trailing_colon_is_host_only() {
        // `host:` (empty session) means the host itself, like parse_target.
        assert_eq!(
            parse_kill_target(&no_aliases(), "devbox:", &[]),
            KillTarget::HostOnly("devbox".to_string())
        );
    }

    #[test]
    fn kill_target_foreign_namespace_passes_through() {
        // A `/`-qualified name stays a session string; namespace resolution
        // happens later in kill_one (it's passed through literally).
        assert_eq!(
            parse_kill_target(&no_aliases(), "local:laptop2/work", &[]),
            session_target("local", "laptop2/work")
        );
    }

    // -- order_sessions (own-client highlighting and sort) --

    #[test]
    fn order_sessions_own_namespace_first_and_flagged() {
        let sessions = vec![
            auto_entry("laptop2/work", false),
            auto_entry("mylaptop/build", false),
            auto_entry("default", false),
            auto_entry("mylaptop/edit", false),
        ];
        let ordered = order_sessions(&sessions, "mylaptop");
        let names: Vec<&str> = ordered.iter().map(|(s, _)| s.name.as_str()).collect();
        assert_eq!(names, vec!["mylaptop/build", "mylaptop/edit", "laptop2/work", "default"]);
        let own_flags: Vec<bool> = ordered.iter().map(|(_, own)| *own).collect();
        assert_eq!(own_flags, vec![true, true, false, false]);
    }

    #[test]
    fn order_sessions_groups_foreign_by_namespace() {
        // Foreign sessions interleaved by the server's id-order regroup by
        // namespace; id-order survives within each namespace (stable sort).
        let sessions = vec![
            auto_entry("zeta/1", false),
            auto_entry("alpha/9", false),
            auto_entry("zeta/0", false),
            auto_entry("alpha/2", false),
        ];
        let ordered = order_sessions(&sessions, "mylaptop");
        let names: Vec<&str> = ordered.iter().map(|(s, _)| s.name.as_str()).collect();
        assert_eq!(names, vec!["alpha/9", "alpha/2", "zeta/1", "zeta/0"]);
    }

    #[test]
    fn order_sessions_empty_ambient_marks_nothing_own() {
        // An empty ambient client name (no elision context) must not flag
        // every session as own -- `"".starts_with("/")` style bugs.
        let sessions = vec![auto_entry("laptop2/work", false), auto_entry("default", false)];
        let ordered = order_sessions(&sessions, "");
        assert!(ordered.iter().all(|(_, own)| !own));
    }

    // -- group_by_daemon / group_header (bare `gritty ls` dashboard) --

    fn probed_ok(host: &str, dest: &str, server_id: u64) -> ProbedHost {
        ProbedHost {
            host: host.to_string(),
            tunnel: Some((dest.to_string(), "healthy".to_string())),
            outcome: Ok((server_id, vec![entry()])),
        }
    }

    fn probed_local(server_id: u64) -> ProbedHost {
        ProbedHost { host: "local".to_string(), tunnel: None, outcome: Ok((server_id, vec![])) }
    }

    fn probed_err(host: &str, dest: &str, status: &str, err: &str) -> ProbedHost {
        ProbedHost {
            host: host.to_string(),
            tunnel: Some((dest.to_string(), status.to_string())),
            outcome: Err(err.to_string()),
        }
    }

    #[test]
    fn group_by_daemon_merges_same_server_id() {
        // Two tunnel names pointing at the same remote daemon collapse into
        // one group -- the duplicate-listing fix.
        let groups = group_by_daemon(vec![
            probed_ok("fate", "fate", 42),
            probed_ok("fate.x.pattern.net", "fate.x.pattern.net", 42),
        ]);
        assert_eq!(groups.len(), 1);
        let names: Vec<&str> = groups[0].members.iter().map(|(h, _)| h.as_str()).collect();
        assert_eq!(names, vec!["fate", "fate.x.pattern.net"]);
        // Sessions kept once, not concatenated twice.
        assert_eq!(groups[0].result.as_ref().unwrap().len(), 1);
    }

    #[test]
    fn group_by_daemon_distinct_daemons_stay_separate() {
        let groups =
            group_by_daemon(vec![probed_local(1), probed_ok("devbox", "devbox.example.com", 2)]);
        assert_eq!(groups.len(), 2);
        assert_eq!(groups[0].members[0].0, "local"); // discovery order preserved
        assert_eq!(groups[1].members[0].0, "devbox");
    }

    #[test]
    fn group_by_daemon_failed_probes_never_merge() {
        // Two failures (no server_id) must not merge with each other -- there
        // is no identity to merge on.
        let groups = group_by_daemon(vec![
            probed_err("a", "a.example.com", "reconnecting", "probe timed out after 2s"),
            probed_err("b", "b.example.com", "reconnecting", "probe timed out after 2s"),
        ]);
        assert_eq!(groups.len(), 2);
    }

    #[test]
    fn group_header_local_has_no_annotation() {
        let groups = group_by_daemon(vec![probed_local(1)]);
        assert_eq!(group_header(&groups[0]), ("local".to_string(), None));
    }

    #[test]
    fn group_header_shows_informative_destination_and_status() {
        // Tunnel name `fate` with destination `fate.x.pattern.net`: the
        // destination adds information, so it appears before the status.
        let groups = group_by_daemon(vec![probed_ok("fate", "fate.x.pattern.net", 1)]);
        let (names, annotation) = group_header(&groups[0]);
        assert_eq!(names, "fate");
        assert_eq!(annotation.as_deref(), Some("fate.x.pattern.net, healthy"));
    }

    #[test]
    fn group_header_elides_redundant_destination() {
        // Destination identical to the tunnel name adds nothing.
        let groups = group_by_daemon(vec![probed_ok("fate", "fate", 1)]);
        let (names, annotation) = group_header(&groups[0]);
        assert_eq!(names, "fate");
        assert_eq!(annotation.as_deref(), Some("healthy"));
    }

    #[test]
    fn group_header_merged_group_says_same_daemon() {
        let groups = group_by_daemon(vec![
            probed_ok("fate", "fate", 42),
            probed_ok("fate.x.pattern.net", "fate.x.pattern.net", 42),
        ]);
        let (names, annotation) = group_header(&groups[0]);
        assert_eq!(names, "fate, fate.x.pattern.net");
        assert_eq!(annotation.as_deref(), Some("same daemon, healthy"));
    }

    #[test]
    fn is_listable_drops_only_dead_orphaned_sockets() {
        // A dead orphaned socket file (no tunnel, not local, probe failed) is
        // litter, not a connection.
        let orphan_dead = ProbedHost {
            host: "old-tunnel".to_string(),
            tunnel: None,
            outcome: Err("connect: Connection refused".to_string()),
        };
        assert!(!is_listable(&orphan_dead));

        // ...but an orphaned socket that answers is a live daemon -- show it.
        let orphan_live =
            ProbedHost { host: "old-tunnel".to_string(), tunnel: None, outcome: Ok((9, vec![])) };
        assert!(is_listable(&orphan_live));

        // A broken local daemon and a broken live tunnel both stay visible.
        let local_dead = ProbedHost {
            host: "local".to_string(),
            tunnel: None,
            outcome: Err("connect: Connection refused".to_string()),
        };
        assert!(is_listable(&local_dead));
        assert!(is_listable(&probed_err("devbox", "devbox", "reconnecting", "timed out")));
    }

    #[test]
    fn group_header_reconnecting_wins_over_healthy() {
        // A failed probe whose flock says "reconnecting" surfaces that status
        // -- it is the state the user needs to act on.
        let groups =
            group_by_daemon(vec![probed_err("devbox", "devbox", "reconnecting", "timed out")]);
        let (_, annotation) = group_header(&groups[0]);
        assert_eq!(annotation.as_deref(), Some("reconnecting"));
    }
}
