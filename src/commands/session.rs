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
            let code = gritty::client::run(
                &name,
                framed,
                !settings.no_redraw,
                &ctl_path,
                vec![],
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
        return pick_or_list(&host, &sessions);
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
    pick_or_list(&host, &sessions)
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
fn pick_or_list(host: &str, sessions: &[gritty::protocol::SessionEntry]) -> String {
    if std::io::IsTerminal::is_terminal(&std::io::stderr()) {
        match tui_pick_session(host, sessions) {
            Some(name) => name,
            None => std::process::exit(1),
        }
    } else {
        print_session_list(host, sessions);
        std::process::exit(1);
    }
}

/// Interactive session picker. Returns selected session name, or None on abort.
fn tui_pick_session(host: &str, sessions: &[gritty::protocol::SessionEntry]) -> Option<String> {
    use crossterm::{
        event::{self, Event, KeyCode, KeyEvent, KeyModifiers},
        terminal,
    };
    use std::io::Write;

    let mut stderr = std::io::stderr();

    // Find first detached session for initial cursor position
    let initial = sessions.iter().position(|s| !s.attached).unwrap_or(0);
    let mut cursor = initial;

    // Precompute column data
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();

    struct Row {
        name: String,
        attached: bool,
        age: String,
        cmd: String,
    }

    let rows: Vec<Row> = sessions
        .iter()
        .map(|s| Row {
            name: session_display_name(s),
            attached: s.attached,
            age: format_age(now, s.created_at),
            cmd: s.foreground_cmd.clone(),
        })
        .collect();

    let name_w = rows.iter().map(|r| r.name.len()).max().unwrap_or(0);
    let tag_w = 10; // "(attached)" is 10 chars
    let age_w = rows.iter().map(|r| r.age.len()).max().unwrap_or(0);
    let total_lines = rows.len() + 2; // +1 header, +1 hint

    // Enter raw mode
    let _ = terminal::enable_raw_mode();
    let _ = write!(stderr, "\x1b[?25l"); // hide cursor

    let render = |stderr: &mut std::io::Stderr, cursor: usize| {
        // Header: cyan bold
        let _ = write!(stderr, "\x1b[36;1mPick a session on {host}:\x1b[0m\r\n");
        for (i, row) in rows.iter().enumerate() {
            let marker = if i == cursor { "\x1b[32;1m\u{25b8}\x1b[0m" } else { " " };
            let tag = if row.attached { "(attached)" } else { "" };

            if i == cursor {
                // Selected: green bold
                let _ = write!(
                    stderr,
                    "{marker} \x1b[32;1m{:<name_w$}\x1b[0m  {:<tag_w$}  \x1b[32m{:<age_w$}\x1b[0m  \x1b[32m{}\x1b[0m\r\n",
                    row.name, tag, row.age, row.cmd,
                );
            } else if row.attached {
                // Attached: dimmed
                let _ = write!(
                    stderr,
                    "{marker} \x1b[2m{:<name_w$}  {:<tag_w$}  {:<age_w$}  {}\x1b[0m\r\n",
                    row.name, tag, row.age, row.cmd,
                );
            } else {
                // Normal
                let _ = write!(
                    stderr,
                    "{marker} {:<name_w$}  {:<tag_w$}  {:<age_w$}  {}\r\n",
                    row.name, tag, row.age, row.cmd,
                );
            }
        }
        let _ = write!(
            stderr,
            "\x1b[2m  \u{2191}/\u{2193} navigate  enter select  esc cancel\x1b[0m\r\n"
        );
        let _ = stderr.flush();
    };

    // Initial render
    render(&mut stderr, cursor);

    let result = loop {
        let Ok(ev) = event::read() else {
            break None;
        };
        match ev {
            Event::Key(KeyEvent { code: KeyCode::Up | KeyCode::Char('k'), .. }) => {
                cursor = cursor.saturating_sub(1);
            }
            Event::Key(KeyEvent { code: KeyCode::Down | KeyCode::Char('j'), .. }) => {
                if cursor + 1 < rows.len() {
                    cursor += 1;
                }
            }
            Event::Key(KeyEvent { code: KeyCode::Enter, .. }) => {
                break Some(rows[cursor].name.clone());
            }
            Event::Key(KeyEvent { code: KeyCode::Esc | KeyCode::Char('q'), .. }) => {
                break None;
            }
            Event::Key(KeyEvent {
                code: KeyCode::Char('c') | KeyCode::Char('g'),
                modifiers: KeyModifiers::CONTROL,
                ..
            }) => {
                break None;
            }
            _ => continue, // don't re-render for unknown events
        }
        // Move cursor up to re-render in place
        let _ = write!(stderr, "\x1b[{}A", total_lines);
        render(&mut stderr, cursor);
    };

    // Cleanup: erase picker lines, restore terminal
    let _ = write!(stderr, "\x1b[{}A", total_lines); // move up
    for _ in 0..total_lines {
        let _ = write!(stderr, "\x1b[2K\r\n"); // clear each line
    }
    let _ = write!(stderr, "\x1b[{}A", total_lines); // move back up
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
                            pty,
                            pid,
                            created,
                            status,
                        ]
                    })
                    .collect();

                gritty::table::print_table(
                    &["ID", "Name", "Cmd", "PTY", "PID", "Created", "Status"],
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
            &["Host", "ID", "Name", "Cmd", "PTY", "PID", "Created", "Status"],
            &rows,
        );
    } else {
        let host = &rows[0][0];
        println!("Host: {host}");
        let trimmed: Vec<Vec<String>> = rows.iter().map(|r| r[1..].to_vec()).collect();
        gritty::table::print_table(
            &["ID", "Name", "Cmd", "PTY", "PID", "Created", "Status"],
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
