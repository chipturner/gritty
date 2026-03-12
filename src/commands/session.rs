use std::path::{Path, PathBuf};

use super::AutoStart;
use super::util::{format_age, format_timestamp, server_request};

#[allow(clippy::too_many_arguments)]
pub(crate) async fn connect_session(
    session: Option<String>,
    command: Option<String>,
    detach: bool,
    no_create: bool,
    force: bool,
    pick: bool,
    no_pick: bool,
    settings: gritty::config::SessionSettings,
    ctl_path: PathBuf,
    auto_start_mode: AutoStart,
    wait: bool,
) -> anyhow::Result<()> {
    use futures_util::{SinkExt, StreamExt};
    use gritty::protocol::{Frame, FrameCodec};
    use tokio_util::codec::Framed;

    let name = match session {
        Some(name) => name,
        None => pick_session(pick, no_pick, &ctl_path).await,
    };
    let session_command = command.unwrap_or_default();

    // If not forcing, check whether the target session is already attached
    if !force {
        if let Some(entry) = find_session(&name, &ctl_path).await? {
            if entry.attached {
                let host = host_from_ctl_path(&ctl_path);
                eprintln!(
                    "error: session {name} is already attached (heartbeat {}s ago)",
                    std::time::SystemTime::now()
                        .duration_since(std::time::UNIX_EPOCH)
                        .unwrap_or_default()
                        .as_secs()
                        .saturating_sub(entry.last_heartbeat)
                );
                eprintln!("  gritty connect {host}:{name} --force   to take over",);
                std::process::exit(1);
            }
        }
    }

    let stream = super::util::connect_or_start(&ctl_path, &auto_start_mode, wait).await?;
    let mut framed = Framed::new(stream, FrameCodec);
    gritty::handshake(&mut framed).await?;

    // Try attach first
    framed.send(Frame::Attach { session: name.clone() }).await?;

    match Frame::expect_from(framed.next().await)? {
        Frame::Ok => {
            // Attached to existing session
            if detach {
                eprintln!("\x1b[32m\u{25b8} session {name} exists (not attaching, -d)\x1b[0m");
                return Ok(());
            }
            eprintln!("\x1b[32m\u{25b8} attached {name}\x1b[0m");
            let mut env_vars = vec![];
            if !settings.client_name.is_empty() {
                env_vars.push(("GRITTY_CLIENT".into(), settings.client_name.clone()));
            }
            let code = gritty::client::run(
                &name,
                framed,
                !settings.no_redraw,
                &ctl_path,
                env_vars,
                settings.no_escape,
                settings.forward_agent,
                settings.forward_open,
                settings.oauth_redirect,
                settings.oauth_timeout,
                settings.heartbeat_interval,
                settings.heartbeat_timeout,
            )
            .await?;
            std::process::exit(code);
        }
        Frame::Error { message } if message.starts_with("no such session:") => {
            if no_create {
                anyhow::bail!("no such session: {name}");
            }
            // Fall through to create
        }
        Frame::Error { message } => anyhow::bail!("{message}"),
        other => anyhow::bail!("unexpected response from server: {other:?}"),
    }

    // Create new session -- need a fresh connection since the previous one
    // was consumed by the failed attach
    let stream = super::util::connect_or_start(&ctl_path, &auto_start_mode, wait).await?;
    let mut framed = Framed::new(stream, FrameCodec);
    gritty::handshake(&mut framed).await?;
    framed.send(Frame::NewSession { name: name.clone(), command: session_command }).await?;

    match Frame::expect_from(framed.next().await)? {
        Frame::SessionCreated { id } => {
            eprintln!("\x1b[32m\u{25b8} session {name}\x1b[0m");

            // Alert about other detached sessions
            alert_detached_sessions(&name, &ctl_path).await;

            if detach {
                return Ok(());
            }
            let mut env_vars = gritty::collect_env_vars();
            if settings.forward_open {
                env_vars.push(("BROWSER".into(), "gritty open".into()));
            }
            if !settings.client_name.is_empty() {
                env_vars.push(("GRITTY_CLIENT".into(), settings.client_name.clone()));
            }
            let code = gritty::client::run(
                &id,
                framed,
                false, // no redraw on new session -- nothing to redraw
                &ctl_path,
                env_vars,
                settings.no_escape,
                settings.forward_agent,
                settings.forward_open,
                settings.oauth_redirect,
                settings.oauth_timeout,
                settings.heartbeat_interval,
                settings.heartbeat_timeout,
            )
            .await?;
            std::process::exit(code);
        }
        Frame::Error { message } => anyhow::bail!("{message}"),
        other => anyhow::bail!("unexpected response from server: {other:?}"),
    }
}

/// Resolve session name when none was explicitly given.
async fn pick_session(pick: bool, no_pick: bool, ctl_path: &Path) -> String {
    use gritty::protocol::Frame;

    if no_pick {
        return "default".to_string();
    }

    let sessions = match server_request(&ctl_path.to_path_buf(), Frame::ListSessions).await {
        Ok(Frame::SessionInfo { sessions }) => sessions,
        _ => return "default".to_string(),
    };

    if sessions.is_empty() {
        return "default".to_string();
    }

    let host = host_from_ctl_path(ctl_path);

    if pick {
        return pick_or_list(&host, &sessions, ctl_path).await;
    }

    let detached: Vec<_> = sessions.iter().filter(|s| !s.attached).collect();

    // One session, detached: attach directly
    if sessions.len() == 1 && detached.len() == 1 {
        return session_display_name(&sessions[0]);
    }

    // Multiple sessions, exactly one detached: attach to the detached one
    if detached.len() == 1 {
        return session_display_name(detached[0]);
    }

    // Ambiguous (multiple detached) or all attached: show picker
    pick_or_list(&host, &sessions, ctl_path).await
}

fn session_display_name(s: &gritty::protocol::SessionEntry) -> String {
    if s.name.is_empty() { s.id.clone() } else { s.name.clone() }
}

fn print_session_list(host: &str, sessions: &[gritty::protocol::SessionEntry]) {
    if sessions.len() == 1 {
        eprintln!("error: session on {host} is already attached:");
    } else {
        eprintln!("error: multiple sessions on {host} -- specify one:");
    }
    for s in sessions {
        let name = session_display_name(s);
        let suffix = if s.attached { "     (attached)" } else { "" };
        eprintln!("  gritty connect {host}:{name}{suffix}");
    }
}

/// Show picker (TUI if stderr is a TTY, static list otherwise).
/// Returns selected name or exits on abort/non-TTY.
async fn pick_or_list(
    host: &str,
    sessions: &[gritty::protocol::SessionEntry],
    ctl_path: &Path,
) -> String {
    if std::io::IsTerminal::is_terminal(&std::io::stderr()) {
        match tui_pick_session(host, sessions, ctl_path).await {
            Some(name) => name,
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

fn build_rows(sessions: &[gritty::protocol::SessionEntry]) -> Vec<Row> {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();
    let home = std::env::var("HOME").unwrap_or_default();
    sessions
        .iter()
        .enumerate()
        .map(|(i, s)| Row {
            name: session_display_name(s),
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

/// Interactive session picker. Returns selected session name, or None on abort.
async fn tui_pick_session(
    host: &str,
    sessions: &[gritty::protocol::SessionEntry],
    ctl_path: &Path,
) -> Option<String> {
    use crossterm::{
        event::{self, Event, KeyCode, KeyEvent, KeyModifiers},
        terminal,
    };
    use std::io::Write;

    let mut stderr = std::io::stderr();

    // Find first detached session for initial cursor position
    let initial = sessions.iter().position(|s| !s.attached).unwrap_or(0);
    let mut cursor = initial;

    let mut rows = build_rows(sessions);
    let mut has_default = sessions.iter().any(|s| s.name == "default" || s.id == "default");

    enum Mode {
        Pick,
        Input { buf: String, cursor_pos: usize, rename_of: Option<String> },
        ConfirmKill { name: String },
    }

    let mut mode = Mode::Pick;
    let mut prev_total_lines: usize = 0;

    // Enter raw mode
    let _ = terminal::enable_raw_mode();
    let _ = write!(stderr, "\x1b[?25l"); // hide cursor

    let render = |stderr: &mut std::io::Stderr,
                  rows: &[Row],
                  cursor: usize,
                  mode: &Mode,
                  has_default: bool,
                  prev_total_lines: usize| {
        let name_w = rows.iter().map(|r| r.name.len()).max().unwrap_or(0).max(3);
        let tag_w = 10; // "(attached)" is 10 chars
        let age_w = rows.iter().map(|r| r.age.len()).max().unwrap_or(0);
        let cmd_w = rows.iter().map(|r| r.cmd.len()).max().unwrap_or(0);
        let client_w = rows.iter().map(|r| r.client.len()).max().unwrap_or(0);
        let total_lines = rows.len() + 3; // header + rows + new-session + hint

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

            if i == cursor && matches!(mode, Mode::Pick | Mode::ConfirmKill { .. }) {
                let tag = if row.attached { "(attached)" } else { "" };
                let _ = write!(
                    stderr,
                    "{marker} \x1b[2m{hk}\x1b[0m \x1b[32;1m{:<name_w$}\x1b[0m  {:<tag_w$}  \x1b[32m{:<age_w$}\x1b[0m  \x1b[32m{:<cmd_w$}\x1b[0m  \x1b[32m{:<client_w$}\x1b[0m  \x1b[32m{}\x1b[0m\r\n",
                    row.name, tag, row.age, row.cmd, row.client, row.cwd,
                );
            } else if row.attached {
                let _ = write!(
                    stderr,
                    "{marker} \x1b[2m{hk} {:<name_w$}  {:<tag_w$}  {:<age_w$}  {:<cmd_w$}  {:<client_w$}  {}\x1b[0m\r\n",
                    row.name, "(attached)", row.age, row.cmd, row.client, row.cwd,
                );
            } else {
                let _ = write!(
                    stderr,
                    "{marker} \x1b[2m{hk}\x1b[0m {:<name_w$}  {:<tag_w$}  {:<age_w$}  {:<cmd_w$}  {:<client_w$}  {}\r\n",
                    row.name, "", row.age, row.cmd, row.client, row.cwd,
                );
            }
        }
        // "New session" row / input line
        match mode {
            Mode::Pick | Mode::ConfirmKill { .. } => {
                let marker = if cursor == rows.len() { "\x1b[32;1m\u{25b8}\x1b[0m" } else { " " };
                if cursor == rows.len() {
                    let _ = write!(
                        stderr,
                        "{marker} \x1b[2m+)\x1b[0m \x1b[32;1mnew session\x1b[0m\r\n"
                    );
                } else {
                    let _ = write!(stderr, "{marker} \x1b[2m+) new session\x1b[0m\r\n");
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
                let mut h = String::from("1-9 jump  enter select  c/n new  r rename  x kill");
                if has_default {
                    h.push_str("  d default");
                }
                h.push_str("  esc quit");
                h
            }
            Mode::Input { rename_of: Some(_), .. } => "enter rename  esc back".to_string(),
            Mode::Input { .. } => "enter create  esc back".to_string(),
            Mode::ConfirmKill { name } => format!("kill {name}? y/n"),
        };
        let _ = write!(stderr, "\x1b[2m  {hints}\x1b[0m\r\n");
        let _ = stderr.flush();
        total_lines
    };

    // Initial render
    prev_total_lines = render(&mut stderr, &rows, cursor, &mode, has_default, prev_total_lines);

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
                        break Some(rows[cursor].name.clone());
                    }
                    mode = Mode::Input { buf: String::new(), cursor_pos: 0, rename_of: None };
                }
                // Hotkeys 1-9
                Event::Key(KeyEvent {
                    code: KeyCode::Char(ch @ '1'..='9'),
                    modifiers: KeyModifiers::NONE,
                    ..
                }) => {
                    let idx = (ch as u8 - b'1') as usize;
                    if idx < rows.len() {
                        break Some(rows[idx].name.clone());
                    }
                }
                // 'd' -> select "default"
                Event::Key(KeyEvent {
                    code: KeyCode::Char('d'),
                    modifiers: KeyModifiers::NONE,
                    ..
                }) if has_default => {
                    break Some("default".to_string());
                }
                // 'c' or 'n' -> new session input
                Event::Key(KeyEvent {
                    code: KeyCode::Char('c') | KeyCode::Char('n'),
                    modifiers: KeyModifiers::NONE,
                    ..
                }) => {
                    mode = Mode::Input { buf: String::new(), cursor_pos: 0, rename_of: None };
                }
                // '+' -> new session
                Event::Key(KeyEvent {
                    code: KeyCode::Char('+'),
                    modifiers: KeyModifiers::NONE,
                    ..
                }) => {
                    mode = Mode::Input { buf: String::new(), cursor_pos: 0, rename_of: None };
                }
                // 'x' or Delete -> kill selected session
                Event::Key(KeyEvent {
                    code: KeyCode::Char('x') | KeyCode::Delete,
                    modifiers: KeyModifiers::NONE,
                    ..
                }) => {
                    if cursor < rows.len() {
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
                        mode = Mode::Input {
                            buf: name.clone(),
                            cursor_pos: len,
                            rename_of: Some(name),
                        };
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
                    let _ = server_request(
                        &ctl,
                        gritty::protocol::Frame::KillSession { session: kill_name },
                    )
                    .await;
                    // Refresh session list
                    if let Ok(gritty::protocol::Frame::SessionInfo { sessions: fresh }) =
                        server_request(&ctl, gritty::protocol::Frame::ListSessions).await
                    {
                        if fresh.is_empty() {
                            break Some("default".to_string());
                        }
                        has_default =
                            fresh.iter().any(|s| s.name == "default" || s.id == "default");
                        rows = build_rows(&fresh);
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
                    let new_name = buf.trim().to_string();
                    if let Some(old_name) = rename_of.take() {
                        // Rename mode
                        if new_name.is_empty() || new_name == old_name {
                            mode = Mode::Pick;
                        } else {
                            let ctl = ctl_path.to_path_buf();
                            let _ = server_request(
                                &ctl,
                                gritty::protocol::Frame::RenameSession {
                                    session: old_name,
                                    new_name,
                                },
                            )
                            .await;
                            // Refresh
                            if let Ok(gritty::protocol::Frame::SessionInfo { sessions: fresh }) =
                                server_request(&ctl, gritty::protocol::Frame::ListSessions).await
                            {
                                has_default =
                                    fresh.iter().any(|s| s.name == "default" || s.id == "default");
                                rows = build_rows(&fresh);
                                cursor = cursor.min(rows.len().saturating_sub(1));
                            }
                            mode = Mode::Pick;
                        }
                    } else if new_name.is_empty() {
                        mode = Mode::Pick;
                        cursor = rows.len();
                    } else {
                        break Some(new_name);
                    }
                }
                Event::Key(KeyEvent { code: KeyCode::Esc, .. }) => {
                    let back_to_new = rename_of.is_none();
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
                        buf.remove(*cursor_pos - 1);
                        *cursor_pos -= 1;
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
                    *cursor_pos = cursor_pos.saturating_sub(1);
                }
                // Right arrow / Ctrl+F
                Event::Key(KeyEvent { code: KeyCode::Right, .. })
                | Event::Key(KeyEvent {
                    code: KeyCode::Char('f'),
                    modifiers: KeyModifiers::CONTROL,
                    ..
                }) => {
                    if *cursor_pos < buf.len() {
                        *cursor_pos += 1;
                    }
                }
                // Typing
                Event::Key(KeyEvent {
                    code: KeyCode::Char(ch),
                    modifiers: KeyModifiers::NONE | KeyModifiers::SHIFT,
                    ..
                }) => {
                    buf.insert(*cursor_pos, ch);
                    *cursor_pos += 1;
                }
                _ => continue,
            },
        }
        prev_total_lines = render(&mut stderr, &rows, cursor, &mode, has_default, prev_total_lines);
    };

    // Cleanup: erase picker lines, restore terminal
    if prev_total_lines > 0 {
        let _ = write!(stderr, "\x1b[{}A", prev_total_lines);
        for _ in 0..prev_total_lines {
            let _ = write!(stderr, "\x1b[2K\r\n");
        }
        let _ = write!(stderr, "\x1b[{}A", prev_total_lines);
    }
    let _ = write!(stderr, "\x1b[?25h"); // show cursor
    let _ = stderr.flush();
    let _ = terminal::disable_raw_mode();

    result
}

/// Query the daemon for a specific session by name, returning its entry if found.
/// Returns Ok(None) if the server isn't running or the session doesn't exist.
async fn find_session(
    name: &str,
    ctl_path: &Path,
) -> anyhow::Result<Option<gritty::protocol::SessionEntry>> {
    use gritty::protocol::Frame;

    let resp = match server_request(&ctl_path.to_path_buf(), Frame::ListSessions).await {
        Ok(r) => r,
        Err(_) => return Ok(None),
    };
    let Frame::SessionInfo { sessions } = resp else {
        return Ok(None);
    };
    Ok(sessions.into_iter().find(|s| {
        let display = if s.name.is_empty() { &s.id } else { &s.name };
        display == name
    }))
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
/// sessions the user might have forgotten about.
async fn alert_detached_sessions(current_name: &str, ctl_path: &Path) {
    use gritty::protocol::Frame;

    let ctl_path_buf = ctl_path.to_path_buf();
    let Ok(resp) = server_request(&ctl_path_buf, Frame::ListSessions).await else {
        return;
    };
    let Frame::SessionInfo { sessions } = resp else {
        return;
    };
    let detached: Vec<_> = sessions
        .iter()
        .filter(|s| {
            !s.attached && {
                let display = if s.name.is_empty() { &s.id } else { &s.name };
                display != current_name
            }
        })
        .collect();
    if detached.is_empty() {
        return;
    }
    let names: Vec<_> = detached
        .iter()
        .map(|s| if s.name.is_empty() { s.id.clone() } else { s.name.clone() })
        .collect();
    eprintln!("\x1b[2;33m\u{25b8} detached sessions: {}\x1b[0m", names.join(", "));
}

pub(crate) async fn tail_session(target: String, ctl_path: PathBuf) -> anyhow::Result<i32> {
    use futures_util::{SinkExt, StreamExt};
    use gritty::protocol::{Frame, FrameCodec};
    use tokio::net::UnixStream;
    use tokio_util::codec::Framed;

    let stream = UnixStream::connect(&ctl_path).await.map_err(|_| {
        anyhow::anyhow!("no server running (could not connect to {})", ctl_path.display())
    })?;
    let mut framed = Framed::new(stream, FrameCodec);
    gritty::handshake(&mut framed).await?;
    framed.send(Frame::Tail { session: target.clone() }).await?;

    match Frame::expect_from(framed.next().await)? {
        Frame::Ok => {
            eprintln!("\x1b[2;33m\u{25b8} tailing {target}\x1b[0m");
            gritty::client::tail(&target, framed, &ctl_path).await
        }
        Frame::Error { message } => anyhow::bail!("{message}"),
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
        Frame::Error { message } => anyhow::bail!("{message}"),
        other => anyhow::bail!("unexpected response from server: {other:?}"),
    }
}

pub(crate) async fn kill_session(target: String, ctl_path: PathBuf) -> anyhow::Result<()> {
    use gritty::protocol::Frame;

    match server_request(&ctl_path, Frame::KillSession { session: target.clone() }).await? {
        Frame::Ok => {
            eprintln!("\x1b[32m\u{25b8} session {target} killed\x1b[0m");
            Ok(())
        }
        Frame::Error { message } => anyhow::bail!("{message}"),
        other => anyhow::bail!("unexpected response from server: {other:?}"),
    }
}

pub(crate) async fn kill_server(ctl_path: PathBuf) -> anyhow::Result<()> {
    use gritty::protocol::Frame;

    match server_request(&ctl_path, Frame::KillServer).await? {
        Frame::Ok => {
            eprintln!("\x1b[32m\u{25b8} server killed\x1b[0m");
            Ok(())
        }
        Frame::Error { message } => anyhow::bail!("{message}"),
        other => anyhow::bail!("unexpected response from server: {other:?}"),
    }
}

pub(crate) async fn list_sessions(ctl_path: PathBuf) -> anyhow::Result<()> {
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

                // Build row data
                let rows: Vec<Vec<String>> = sessions
                    .iter()
                    .map(|s| {
                        let name = if s.name.is_empty() { "-".to_string() } else { s.name.clone() };
                        let (pty, pid, created, status) = if s.shell_pid == 0 {
                            (
                                "-".to_string(),
                                "-".to_string(),
                                "-".to_string(),
                                "starting".to_string(),
                            )
                        } else {
                            let status = if s.attached {
                                if s.last_heartbeat > 0 {
                                    let ago = now.saturating_sub(s.last_heartbeat);
                                    format!("attached (heartbeat {ago}s ago)")
                                } else {
                                    "attached".to_string()
                                }
                            } else {
                                "detached".to_string()
                            };
                            (
                                s.pty_path.clone(),
                                s.shell_pid.to_string(),
                                format_timestamp(s.created_at),
                                status,
                            )
                        };
                        vec![
                            s.id.clone(),
                            name,
                            s.foreground_cmd.clone(),
                            s.cwd.clone(),
                            s.client_name.clone(),
                            pty,
                            pid,
                            created,
                            status,
                        ]
                    })
                    .collect();

                gritty::table::print_table(
                    &["ID", "Name", "Cmd", "CWD", "Client", "PTY", "PID", "Created", "Status"],
                    &rows,
                );
            }
            Ok(())
        }
        other => {
            anyhow::bail!("unexpected response from server: {other:?}");
        }
    }
}

pub(crate) async fn list_all_sessions() -> anyhow::Result<()> {
    use gritty::protocol::{Frame, FrameCodec, SessionEntry};

    let mut probes: Vec<(String, PathBuf)> = Vec::new();
    let local = gritty::daemon::control_socket_path();
    if local.exists() {
        probes.push(("local".to_string(), local));
    }
    for info in gritty::connect::get_tunnel_info() {
        if info.status == "healthy" {
            probes.push((info.name.clone(), gritty::connect::connection_socket_path(&info.name)));
        }
    }

    if probes.is_empty() {
        anyhow::bail!("no server running");
    }

    let futures: Vec<_> = probes
        .into_iter()
        .map(|(host, path)| async move {
            let result = tokio::time::timeout(std::time::Duration::from_secs(2), async {
                let stream = tokio::net::UnixStream::connect(&path).await.ok()?;
                let mut framed = tokio_util::codec::Framed::new(stream, FrameCodec);
                gritty::handshake(&mut framed).await.ok()?;
                futures_util::SinkExt::send(&mut framed, Frame::ListSessions).await.ok()?;
                match Frame::expect_from(futures_util::StreamExt::next(&mut framed).await) {
                    Ok(Frame::SessionInfo { sessions }) => Some(sessions),
                    _ => None,
                }
            })
            .await;
            let sessions: Vec<SessionEntry> = result.ok().flatten().unwrap_or_default();
            (host, sessions)
        })
        .collect();

    let results: Vec<(String, Vec<SessionEntry>)> = futures_util::future::join_all(futures).await;

    let all_empty = results.iter().all(|(_, s)| s.is_empty());
    if all_empty {
        println!("no active sessions");
        return Ok(());
    }

    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();

    let multi_host = results.iter().filter(|(_, s)| !s.is_empty()).count() > 1;

    // Build row data: [host, id, name, pty, pid, created, status]
    let rows: Vec<Vec<String>> = results
        .iter()
        .flat_map(|(host, sessions)| {
            sessions.iter().map(move |s| {
                let name = if s.name.is_empty() { "-".to_string() } else { s.name.clone() };
                let (pty, pid, created, status) = if s.shell_pid == 0 {
                    ("-".to_string(), "-".to_string(), "-".to_string(), "starting".to_string())
                } else {
                    let status = if s.attached {
                        if s.last_heartbeat > 0 {
                            let ago = now.saturating_sub(s.last_heartbeat);
                            format!("attached (heartbeat {ago}s ago)")
                        } else {
                            "attached".to_string()
                        }
                    } else {
                        "detached".to_string()
                    };
                    (
                        s.pty_path.clone(),
                        s.shell_pid.to_string(),
                        format_timestamp(s.created_at),
                        status,
                    )
                };
                vec![
                    host.clone(),
                    s.id.clone(),
                    name,
                    s.foreground_cmd.clone(),
                    s.cwd.clone(),
                    s.client_name.clone(),
                    pty,
                    pid,
                    created,
                    status,
                ]
            })
        })
        .collect();

    if multi_host {
        gritty::table::print_table(
            &["Host", "ID", "Name", "Cmd", "CWD", "Client", "PTY", "PID", "Created", "Status"],
            &rows,
        );
    } else {
        let host = &rows[0][0];
        println!("Host: {host}");
        let trimmed: Vec<Vec<String>> = rows.iter().map(|r| r[1..].to_vec()).collect();
        gritty::table::print_table(
            &["ID", "Name", "Cmd", "CWD", "Client", "PTY", "PID", "Created", "Status"],
            &trimmed,
        );
    }
    Ok(())
}

/// Print available sessions and exit with an error when a session-requiring
/// command is invoked without the session part (e.g. `gritty tail local`
/// instead of `gritty tail local:session`).
pub(crate) async fn suggest_session(cmd: &str, host: &str, ctl_path: &Path) -> anyhow::Result<()> {
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
                let name = if s.name.is_empty() { "-".to_string() } else { s.name.clone() };
                let age = format_age(now, s.created_at);
                msg.push_str(&format!("  {}   {:<8} {}\n", s.id, name, age));
            }
            anyhow::bail!("{msg}");
        }
        _ => anyhow::bail!("specify a session: gritty {cmd} {host}:<session>"),
    }
}
