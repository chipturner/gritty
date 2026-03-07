use std::path::{Path, PathBuf};

use super::util::{format_age, format_timestamp, server_request};
use super::{AttachError, AutoStart};

pub(crate) async fn new_session(
    name: Option<String>,
    command: Option<String>,
    detach: bool,
    settings: gritty::config::SessionSettings,
    ctl_path: PathBuf,
    auto_start_mode: AutoStart,
    wait: bool,
) -> anyhow::Result<()> {
    use futures_util::{SinkExt, StreamExt};
    use gritty::protocol::{Frame, FrameCodec};
    use tokio_util::codec::Framed;

    let session_name = name.clone().unwrap_or_default();
    let session_command = command.unwrap_or_default();

    let stream = super::util::connect_or_start(&ctl_path, &auto_start_mode, wait).await?;
    let mut framed = Framed::new(stream, FrameCodec);
    gritty::handshake(&mut framed).await?;
    framed.send(Frame::NewSession { name: session_name, command: session_command }).await?;

    match Frame::expect_from(framed.next().await)? {
        Frame::SessionCreated { id } => {
            match &name {
                Some(n) => eprintln!("\x1b[32m\u{25b8} session {n}\x1b[0m"),
                None => eprintln!("\x1b[32m\u{25b8} session {id}\x1b[0m"),
            }
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

pub(crate) async fn attach(
    target: &str,
    settings: &gritty::config::SessionSettings,
    ctl_path: &Path,
) -> Result<i32, AttachError> {
    use futures_util::{SinkExt, StreamExt};
    use gritty::protocol::{Frame, FrameCodec};
    use tokio::net::UnixStream;
    use tokio_util::codec::Framed;

    let stream = loop {
        match UnixStream::connect(ctl_path).await {
            Ok(s) => break s,
            Err(_) => {
                eprintln!("\x1b[2;33m\u{25b8} waiting for server...\x1b[0m");
                tokio::time::sleep(std::time::Duration::from_secs(1)).await;
            }
        }
    };
    let mut framed = Framed::new(stream, FrameCodec);
    gritty::handshake(&mut framed).await.map_err(AttachError::Other)?;
    framed
        .send(Frame::Attach { session: target.to_string() })
        .await
        .map_err(|e| AttachError::Other(e.into()))?;

    match Frame::expect_from(framed.next().await).map_err(AttachError::Other)? {
        Frame::Ok => {
            eprintln!("\x1b[32m\u{25b8} attached\x1b[0m");
            let code = gritty::client::run(
                target,
                framed,
                !settings.no_redraw,
                ctl_path,
                vec![],
                settings.no_escape,
                settings.forward_agent,
                settings.forward_open,
                settings.oauth_redirect,
                settings.oauth_timeout,
                settings.heartbeat_interval,
                settings.heartbeat_timeout,
            )
            .await
            .map_err(AttachError::Other)?;
            Ok(code)
        }
        Frame::Error { message } if message.starts_with("no such session:") => {
            Err(AttachError::NoSuchSession)
        }
        Frame::Error { message } => Err(AttachError::Other(anyhow::anyhow!("{message}"))),
        other => {
            Err(AttachError::Other(anyhow::anyhow!("unexpected response from server: {other:?}")))
        }
    }
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
/// command is invoked without the session part (e.g. `gritty attach local`
/// instead of `gritty attach local:session`).
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
