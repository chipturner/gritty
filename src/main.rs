use clap::{Parser, Subcommand};
use std::os::fd::{AsRawFd, OwnedFd};
use std::path::PathBuf;
use tracing_subscriber::EnvFilter;

#[derive(Parser)]
#[command(name = "gritty", about = "Persistent TTY sessions over Unix domain sockets")]
struct Cli {
    /// Path to the server control socket (overrides default)
    #[arg(long, global = true)]
    ctl_socket: Option<PathBuf>,

    /// Enable verbose logging
    #[arg(short = 'v', long, global = true)]
    verbose: bool,

    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Start the server (backgrounds by default, use --foreground to stay in foreground)
    #[command(alias = "s")]
    Server {
        /// Run in the foreground instead of daemonizing
        #[arg(long, short = 'f')]
        foreground: bool,
    },
    /// Create a new persistent session (auto-attaches)
    #[command(alias = "new")]
    NewSession {
        /// Remote host (connection name from `gritty connect`)
        host: Option<String>,

        /// Session name (optional; sessions always get an auto-incrementing id)
        #[arg(short = 't', long = "target")]
        target: Option<String>,

        /// Disable escape sequences (~. detach, ~? help, etc.)
        #[arg(long)]
        no_escape: bool,

        /// Forward local SSH agent to the session
        #[arg(short = 'A', long)]
        forward_agent: bool,
    },
    /// Attach to an existing session (detaches other clients)
    #[command(alias = "a")]
    Attach {
        /// Remote host (connection name from `gritty connect`)
        host: Option<String>,

        /// Session id or name
        #[arg(short = 't', long = "target")]
        target: String,

        /// Don't send Ctrl-L to redraw after attaching
        #[arg(long)]
        no_redraw: bool,

        /// Disable escape sequences (~. detach, ~? help, etc.)
        #[arg(long)]
        no_escape: bool,

        /// Forward local SSH agent to the session
        #[arg(short = 'A', long)]
        forward_agent: bool,
    },
    /// List active sessions
    #[command(alias = "ls", alias = "list")]
    ListSessions {
        /// Remote host (connection name from `gritty connect`)
        host: Option<String>,
    },
    /// Kill a specific session
    KillSession {
        /// Remote host (connection name from `gritty connect`)
        host: Option<String>,

        /// Session id or name
        #[arg(short = 't', long = "target")]
        target: String,
    },
    /// Kill the server and all sessions
    KillServer {
        /// Remote host (connection name from `gritty connect`)
        host: Option<String>,
    },
    /// Print the default socket path
    #[command(alias = "socket")]
    SocketPath,
    /// SSH tunnel to a remote host (backgrounds by default, prints socket path)
    #[command(alias = "c")]
    Connect {
        /// Remote destination ([user@]host[:port])
        destination: String,

        /// Connection name (defaults to hostname from destination)
        #[arg(short = 'n', long)]
        name: Option<String>,

        /// Don't auto-start remote server
        #[arg(long)]
        no_server_start: bool,

        /// Extra SSH options (can be repeated)
        #[arg(long = "ssh-option", short = 'o')]
        ssh_options: Vec<String>,

        /// Print the SSH commands instead of running them
        #[arg(long)]
        dry_run: bool,

        /// Run in the foreground instead of backgrounding
        #[arg(long, short = 'f')]
        foreground: bool,
    },
    /// Disconnect an SSH tunnel by connection name
    #[command(alias = "dc")]
    Disconnect {
        /// Connection name (as shown in `gritty tunnels`)
        name: String,
    },
    /// List active SSH tunnels
    #[command(alias = "tun")]
    Tunnels,
}

fn init_tracing(verbose: bool) {
    let filter = if verbose && std::env::var("RUST_LOG").is_err() {
        EnvFilter::new("gritty=debug")
    } else {
        EnvFilter::from_default_env()
    };
    tracing_subscriber::fmt().with_env_filter(filter).with_writer(std::io::stderr).init();
}

/// Fork into background, returning the write end of the readiness pipe.
///
/// Parent: blocks reading the pipe. Gets a byte → child is ready, runs `on_ready`, exits 0.
/// Gets EOF → child died (exits 1).
/// Child: returns Ok(OwnedFd) for the write end of the pipe.
fn daemonize(on_ready: impl FnOnce(nix::unistd::Pid)) -> anyhow::Result<OwnedFd> {
    use nix::unistd::{ForkResult, fork, pipe, setsid};
    let (read_fd, write_fd) = pipe()?;

    // Safety: fork before any threads (tokio runtime not yet created)
    match unsafe { fork() }? {
        ForkResult::Parent { child } => {
            // Close write end
            drop(write_fd);

            // Read from pipe: one byte = child ready, EOF = child died
            let mut buf = [0u8; 1];
            let mut read_file = std::fs::File::from(read_fd);
            use std::io::Read;
            match read_file.read(&mut buf) {
                Ok(1) => {
                    on_ready(child);
                    std::process::exit(0);
                }
                _ => {
                    eprintln!("error: failed to start");
                    std::process::exit(1);
                }
            }
        }
        ForkResult::Child => {
            // Close read end
            drop(read_fd);

            // New session, detach from terminal
            setsid()?;

            // Redirect stdin/stdout/stderr to /dev/null
            let devnull = nix::fcntl::open(
                "/dev/null",
                nix::fcntl::OFlag::O_RDWR,
                nix::sys::stat::Mode::empty(),
            )?;
            unsafe {
                libc::dup2(devnull.as_raw_fd(), 0);
                libc::dup2(devnull.as_raw_fd(), 1);
                libc::dup2(devnull.as_raw_fd(), 2);
            }
            // devnull drops here, closing the original fd (always >2 post-fork)

            Ok(write_fd)
        }
    }
}

fn main() {
    let cli = Cli::parse();
    let verbose = cli.verbose;

    match cli.command {
        Command::Server { foreground } => {
            let ctl_path = cli.ctl_socket.unwrap_or_else(gritty::daemon::control_socket_path);

            let ready_fd = if foreground {
                None
            } else {
                match daemonize(|child| eprintln!("server started (pid {child})")) {
                    Ok(fd) => Some(fd),
                    Err(e) => {
                        eprintln!("error: failed to daemonize: {e}");
                        std::process::exit(1);
                    }
                }
            };

            // Init tracing AFTER fork (stderr may be /dev/null in server mode)
            init_tracing(verbose);

            let rt = match tokio::runtime::Runtime::new() {
                Ok(rt) => rt,
                Err(e) => {
                    eprintln!("error: {e}");
                    std::process::exit(1);
                }
            };
            if let Err(e) = rt.block_on(gritty::daemon::run(&ctl_path, ready_fd)) {
                eprintln!("error: {e}");
                std::process::exit(1);
            }
        }
        Command::Connect {
            destination,
            name,
            no_server_start,
            ssh_options,
            dry_run,
            foreground,
        } => {
            // Compute connection name before fork so parent can print socket path
            let connection_name = name.clone().unwrap_or_else(|| {
                // Parse destination to extract host (same logic as connect::run)
                destination.find('@').map_or_else(
                    || {
                        destination
                            .rfind(':')
                            .map_or(destination.clone(), |c| destination[..c].to_string())
                    },
                    |at| {
                        let rest = &destination[at + 1..];
                        rest.rfind(':').map_or(rest.to_string(), |c| rest[..c].to_string())
                    },
                )
            });
            let local_sock = gritty::connect::connection_socket_path(&connection_name);

            let ready_fd = if foreground || dry_run {
                None
            } else {
                let sock_display = local_sock.display().to_string();
                let conn_name = connection_name.clone();
                match daemonize(move |_child| {
                    println!("{sock_display}");
                    eprintln!("tunnel started (name: {conn_name}). to use:");
                    eprintln!("  gritty new {conn_name}");
                    eprintln!("  gritty attach {conn_name} -t <name>");
                }) {
                    Ok(fd) => Some(fd),
                    Err(e) => {
                        eprintln!("error: failed to daemonize: {e}");
                        std::process::exit(1);
                    }
                }
            };

            // In the parent process, daemonize() exits via std::process::exit.
            // If we're here, we're either the child or running in foreground.

            init_tracing(verbose);

            let rt = match tokio::runtime::Runtime::new() {
                Ok(rt) => rt,
                Err(e) => {
                    eprintln!("error: {e}");
                    std::process::exit(1);
                }
            };

            let opts = gritty::connect::ConnectOpts {
                destination,
                no_server_start,
                ssh_options,
                name,
                dry_run,
            };

            match rt.block_on(gritty::connect::run(opts, ready_fd)) {
                Ok(code) => std::process::exit(code),
                Err(e) => {
                    eprintln!("error: {e}");
                    std::process::exit(1);
                }
            }
        }
        _ => {
            init_tracing(verbose);
            let rt = match tokio::runtime::Runtime::new() {
                Ok(rt) => rt,
                Err(e) => {
                    eprintln!("error: {e}");
                    std::process::exit(1);
                }
            };
            if let Err(e) = rt.block_on(run(cli)) {
                eprintln!("error: {e}");
                std::process::exit(1);
            }
        }
    }
}

fn resolve_ctl_path(ctl_socket: Option<PathBuf>, host: Option<&str>) -> anyhow::Result<PathBuf> {
    match (ctl_socket, host) {
        (Some(_), Some(_)) => anyhow::bail!("cannot specify both --ctl-socket and a host argument"),
        (Some(p), None) => Ok(p),
        (None, Some(h)) => Ok(gritty::daemon::socket_dir().join(format!("connect-{h}.sock"))),
        (None, None) => Ok(gritty::daemon::control_socket_path()),
    }
}

async fn run(cli: Cli) -> anyhow::Result<()> {
    match cli.command {
        Command::Server { .. } | Command::Connect { .. } => unreachable!(),
        Command::NewSession { host, target, no_escape, forward_agent } => {
            let ctl_path = resolve_ctl_path(cli.ctl_socket, host.as_deref())?;
            new_session(target, no_escape, forward_agent, ctl_path).await
        }
        Command::Attach { host, target, no_redraw, no_escape, forward_agent } => {
            let ctl_path = resolve_ctl_path(cli.ctl_socket, host.as_deref())?;
            let code = attach(target, !no_redraw, no_escape, forward_agent, ctl_path).await?;
            std::process::exit(code);
        }
        Command::ListSessions { host } => {
            let ctl_path = resolve_ctl_path(cli.ctl_socket, host.as_deref())?;
            list_sessions(ctl_path).await
        }
        Command::KillSession { host, target } => {
            let ctl_path = resolve_ctl_path(cli.ctl_socket, host.as_deref())?;
            kill_session(target, ctl_path).await
        }
        Command::KillServer { host } => {
            let ctl_path = resolve_ctl_path(cli.ctl_socket, host.as_deref())?;
            kill_server(ctl_path).await
        }
        Command::SocketPath => {
            let ctl_path = cli.ctl_socket.unwrap_or_else(gritty::daemon::control_socket_path);
            println!("{}", ctl_path.display());
            Ok(())
        }
        Command::Disconnect { name } => gritty::connect::disconnect(&name).await,
        Command::Tunnels => {
            gritty::connect::list_tunnels();
            Ok(())
        }
    }
}

async fn new_session(
    name: Option<String>,
    no_escape: bool,
    forward_agent: bool,
    ctl_path: PathBuf,
) -> anyhow::Result<()> {
    use futures_util::{SinkExt, StreamExt};
    use gritty::protocol::{Frame, FrameCodec};
    use tokio::net::UnixStream;
    use tokio_util::codec::Framed;

    let session_name = name.clone().unwrap_or_default();

    let stream = UnixStream::connect(&ctl_path).await.map_err(|_| {
        anyhow::anyhow!("no server running (could not connect to {})", ctl_path.display())
    })?;
    let mut framed = Framed::new(stream, FrameCodec);
    framed.send(Frame::NewSession { name: session_name }).await?;

    match Frame::expect_from(framed.next().await)? {
        Frame::SessionCreated { id } => {
            match &name {
                Some(n) => eprintln!("session created: {n} (id {id})"),
                None => eprintln!("session created: id {id}"),
            }
            let env_vars = gritty::collect_env_vars();
            let code = gritty::client::run(
                &id,
                framed,
                false,
                &ctl_path,
                env_vars,
                no_escape,
                forward_agent,
            )
            .await?;
            std::process::exit(code);
        }
        Frame::Error { message } => anyhow::bail!("{message}"),
        other => anyhow::bail!("unexpected response from server: {other:?}"),
    }
}

async fn attach(
    target: String,
    redraw: bool,
    no_escape: bool,
    forward_agent: bool,
    ctl_path: PathBuf,
) -> anyhow::Result<i32> {
    use futures_util::{SinkExt, StreamExt};
    use gritty::protocol::{Frame, FrameCodec};
    use tokio::net::UnixStream;
    use tokio_util::codec::Framed;

    let stream = loop {
        match UnixStream::connect(&ctl_path).await {
            Ok(s) => break s,
            Err(_) => {
                eprintln!("waiting for server ({})... ctrl-c to abort", ctl_path.display());
                tokio::time::sleep(std::time::Duration::from_secs(1)).await;
            }
        }
    };
    let mut framed = Framed::new(stream, FrameCodec);
    framed.send(Frame::Attach { session: target.clone() }).await?;

    match Frame::expect_from(framed.next().await)? {
        Frame::Ok => {
            eprintln!("[attached]");
            let code = gritty::client::run(
                &target,
                framed,
                redraw,
                &ctl_path,
                vec![],
                no_escape,
                forward_agent,
            )
            .await?;
            Ok(code)
        }
        Frame::Error { message } => anyhow::bail!("{message}"),
        other => anyhow::bail!("unexpected response from server: {other:?}"),
    }
}

/// Send a control frame to the server and return the response.
async fn server_request(
    ctl_path: &PathBuf,
    frame: gritty::protocol::Frame,
) -> anyhow::Result<gritty::protocol::Frame> {
    use futures_util::{SinkExt, StreamExt};
    use gritty::protocol::{Frame, FrameCodec};
    use tokio::net::UnixStream;
    use tokio_util::codec::Framed;

    let stream = UnixStream::connect(ctl_path).await.map_err(|_| {
        anyhow::anyhow!("no server running (could not connect to {})", ctl_path.display())
    })?;
    let mut framed = Framed::new(stream, FrameCodec);
    framed.send(frame).await?;
    Frame::expect_from(framed.next().await)
}

async fn list_sessions(ctl_path: PathBuf) -> anyhow::Result<()> {
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
                let rows: Vec<_> = sessions
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
                        (s.id.clone(), name, pty, pid, created, status)
                    })
                    .collect();

                // Compute column widths
                let w_id = rows.iter().map(|r| r.0.len()).max().unwrap().max(2);
                let w_name = rows.iter().map(|r| r.1.len()).max().unwrap().max(4);
                let w_pty = rows.iter().map(|r| r.2.len()).max().unwrap().max(3);
                let w_pid = rows.iter().map(|r| r.3.len()).max().unwrap().max(3);
                let w_created = rows.iter().map(|r| r.4.len()).max().unwrap().max(7);

                println!(
                    "{:<w_id$}  {:<w_name$}  {:<w_pty$}  {:<w_pid$}  {:<w_created$}  Status",
                    "ID", "Name", "PTY", "PID", "Created",
                );
                for (id, name, pty, pid, created, status) in &rows {
                    println!(
                        "{:<w_id$}  {:<w_name$}  {:<w_pty$}  {:<w_pid$}  {:<w_created$}  {status}",
                        id, name, pty, pid, created,
                    );
                }
            }
            Ok(())
        }
        other => {
            anyhow::bail!("unexpected response from server: {other:?}");
        }
    }
}

fn format_timestamp(epoch_secs: u64) -> String {
    let time = epoch_secs as libc::time_t;
    let mut tm: libc::tm = unsafe { std::mem::zeroed() };
    let result = unsafe { libc::localtime_r(&time, &mut tm) };
    if result.is_null() {
        return "-".to_string();
    }
    format!(
        "{:04}-{:02}-{:02} {:02}:{:02}:{:02}",
        tm.tm_year + 1900,
        tm.tm_mon + 1,
        tm.tm_mday,
        tm.tm_hour,
        tm.tm_min,
        tm.tm_sec,
    )
}

async fn kill_session(target: String, ctl_path: PathBuf) -> anyhow::Result<()> {
    use gritty::protocol::Frame;

    match server_request(&ctl_path, Frame::KillSession { session: target.clone() }).await? {
        Frame::Ok => {
            eprintln!("session killed: {target}");
            Ok(())
        }
        Frame::Error { message } => anyhow::bail!("{message}"),
        other => anyhow::bail!("unexpected response from server: {other:?}"),
    }
}

async fn kill_server(ctl_path: PathBuf) -> anyhow::Result<()> {
    use gritty::protocol::Frame;

    match server_request(&ctl_path, Frame::KillServer).await? {
        Frame::Ok => {
            eprintln!("server killed");
            Ok(())
        }
        Frame::Error { message } => anyhow::bail!("{message}"),
        other => anyhow::bail!("unexpected response from server: {other:?}"),
    }
}
