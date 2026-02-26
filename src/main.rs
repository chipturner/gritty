use clap::{CommandFactory, Parser, Subcommand};
use std::os::fd::{AsRawFd, OwnedFd};
use std::path::{Path, PathBuf};
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

        /// Don't send Ctrl-L to redraw after the shell starts
        #[arg(long)]
        no_redraw: bool,

        /// Disable escape sequences (~. detach, ~? help, etc.)
        #[arg(long)]
        no_escape: bool,

        /// Forward local SSH agent to the session
        #[arg(short = 'A', long)]
        forward_agent: bool,

        /// Forward URL open requests back to the local machine
        #[arg(short = 'O', long)]
        forward_open: bool,

        /// Wait indefinitely for the server instead of giving up after retries
        #[arg(short = 'w', long)]
        wait: bool,
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

        /// Forward URL open requests back to the local machine
        #[arg(short = 'O', long)]
        forward_open: bool,

        /// Wait indefinitely for the server instead of giving up after retries
        #[arg(short = 'w', long)]
        wait: bool,
    },
    /// Tail a session's output (read-only, like tail -f)
    #[command(alias = "t")]
    Tail {
        /// Remote host (connection name from `gritty connect`)
        host: Option<String>,

        /// Session id or name
        #[arg(short = 't', long = "target")]
        target: String,
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
    /// Open a URL on the local machine (for use inside gritty sessions)
    Open {
        /// URL to open
        url: String,
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
    /// Show diagnostics (paths, server status, tunnels)
    Info,
    /// Open config file in $EDITOR (creates from template if missing)
    ConfigEdit,
    /// Print the protocol version number
    ProtocolVersion,
    /// Generate shell completions
    Completions {
        /// Shell to generate completions for
        shell: clap_complete::Shell,
    },
}

fn init_tracing(verbose: bool, log_path: Option<&Path>) {
    let filter = if std::env::var("RUST_LOG").is_ok() {
        EnvFilter::from_default_env()
    } else if verbose {
        EnvFilter::new("gritty=debug")
    } else {
        EnvFilter::new("gritty=info")
    };

    match log_path.and_then(open_log_file) {
        Some(file) => {
            tracing_subscriber::fmt()
                .with_env_filter(filter)
                .with_writer(std::sync::Mutex::new(file))
                .with_ansi(false)
                .with_line_number(true)
                .with_file(true)
                .with_target(true)
                .init();
        }
        None => {
            tracing_subscriber::fmt().with_env_filter(filter).with_writer(std::io::stderr).init();
        }
    }
}

fn open_log_file(path: &Path) -> Option<std::fs::File> {
    use std::os::unix::fs::OpenOptionsExt;
    std::fs::OpenOptions::new().create(true).append(true).mode(0o600).open(path).ok()
}

/// Fork into background, returning the write end of the readiness pipe.
///
/// Parent: blocks reading the pipe. Gets a byte → child is ready, runs `on_ready`, exits 0.
/// Gets EOF → child died (exits 1).
/// Child: returns Ok(OwnedFd) for the write end of the pipe.
///
/// If `output_path` is `Some`, stdout/stderr are redirected to that file (O_APPEND).
/// Otherwise they go to `/dev/null`. stdin always goes to `/dev/null`.
fn daemonize(
    on_ready: impl FnOnce(nix::unistd::Pid),
    output_path: Option<&Path>,
) -> anyhow::Result<OwnedFd> {
    use nix::unistd::{ForkResult, fork, pipe, setsid};
    let (read_fd, write_fd) = pipe()?;

    // Safety: fork before any threads (tokio runtime not yet created)
    match unsafe { fork() }? {
        ForkResult::Parent { child } => {
            // Close write end
            drop(write_fd);

            // Read from pipe: 0x01 = child ready, other data = error message, EOF = crashed
            let mut buf = [0u8; 1];
            let mut read_file = std::fs::File::from(read_fd);
            use std::io::Read;
            match read_file.read(&mut buf) {
                Ok(1) if buf[0] == 0x01 => {
                    on_ready(child);
                    std::process::exit(0);
                }
                Ok(1) => {
                    // Error message from child — read the rest
                    let mut msg = vec![buf[0]];
                    let _ = read_file.read_to_end(&mut msg);
                    eprintln!("{}", String::from_utf8_lossy(&msg).trim());
                    std::process::exit(1);
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

            // stdin always to /dev/null
            let devnull = nix::fcntl::open(
                "/dev/null",
                nix::fcntl::OFlag::O_RDWR,
                nix::sys::stat::Mode::empty(),
            )?;
            unsafe {
                libc::dup2(devnull.as_raw_fd(), 0);
            }

            // stdout/stderr: to output file if provided, else /dev/null
            let out_fd = match output_path {
                Some(path) => nix::fcntl::open(
                    path,
                    nix::fcntl::OFlag::O_WRONLY
                        | nix::fcntl::OFlag::O_CREAT
                        | nix::fcntl::OFlag::O_APPEND,
                    nix::sys::stat::Mode::from_bits_truncate(0o600),
                )?,
                None => nix::fcntl::open(
                    "/dev/null",
                    nix::fcntl::OFlag::O_RDWR,
                    nix::sys::stat::Mode::empty(),
                )?,
            };
            unsafe {
                libc::dup2(out_fd.as_raw_fd(), 1);
                libc::dup2(out_fd.as_raw_fd(), 2);
            }
            // devnull and out_fd drop here, closing the original fds (always >2 post-fork)

            Ok(write_fd)
        }
    }
}

/// Dup the ready_fd so we can send errors back to the parent after `run()` consumes it.
fn dup_ready_fd(ready_fd: &Option<OwnedFd>) -> Option<OwnedFd> {
    ready_fd.as_ref().and_then(|fd| gritty::security::checked_dup(fd.as_raw_fd()).ok())
}

/// Write error to the readiness pipe (so the parent displays it), print to stderr, and exit.
fn report_error(error_pipe: &Option<OwnedFd>, e: &anyhow::Error) -> ! {
    let msg = format!("error: {e}");
    if let Some(fd) = error_pipe {
        let _ = nix::unistd::write(fd, msg.as_bytes());
    }
    eprintln!("{msg}");
    std::process::exit(1);
}

fn main() {
    let cli = Cli::parse();
    let verbose = cli.verbose;
    let config = gritty::config::ConfigFile::load();

    match cli.command {
        Command::Server { foreground } => {
            let ctl_path = cli.ctl_socket.unwrap_or_else(gritty::daemon::control_socket_path);
            let socket_dir = ctl_path.parent().unwrap_or(Path::new("."));
            if let Err(e) = gritty::security::secure_create_dir_all(socket_dir) {
                eprintln!("error: {e}");
                std::process::exit(1);
            }
            let out_path = socket_dir.join("daemon.out");
            let log_path = socket_dir.join("daemon.log");

            let ready_fd = if foreground {
                None
            } else {
                match daemonize(|child| eprintln!("server started (pid {child})"), Some(&out_path))
                {
                    Ok(fd) => Some(fd),
                    Err(e) => {
                        eprintln!("error: failed to daemonize: {e}");
                        std::process::exit(1);
                    }
                }
            };

            // Init tracing AFTER fork: file in daemon mode, stderr in foreground
            init_tracing(verbose, if ready_fd.is_some() { Some(&log_path) } else { None });

            let rt = match tokio::runtime::Runtime::new() {
                Ok(rt) => rt,
                Err(e) => {
                    eprintln!("error: {e}");
                    std::process::exit(1);
                }
            };
            let error_pipe = dup_ready_fd(&ready_fd);
            if let Err(e) = rt.block_on(gritty::daemon::run(&ctl_path, ready_fd)) {
                report_error(&error_pipe, &e);
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
            let connection_name = match name.clone() {
                Some(n) => n,
                None => match gritty::connect::parse_host(&destination) {
                    Ok(h) => h,
                    Err(e) => {
                        eprintln!("error: {e}");
                        std::process::exit(1);
                    }
                },
            };
            let local_sock = gritty::connect::connection_socket_path(&connection_name);

            let socket_dir = gritty::daemon::socket_dir();
            if let Err(e) = gritty::security::secure_create_dir_all(&socket_dir) {
                eprintln!("error: {e}");
                std::process::exit(1);
            }
            let out_path = socket_dir.join(format!("connect-{connection_name}.out"));
            let log_path = socket_dir.join(format!("connect-{connection_name}.log"));

            let ready_fd = if foreground || dry_run {
                None
            } else {
                let sock_display = local_sock.display().to_string();
                let conn_name = connection_name.clone();
                match daemonize(
                    move |_child| {
                        println!("{sock_display}");
                        eprintln!("tunnel started (name: {conn_name}). to use:");
                        eprintln!("  gritty new {conn_name}");
                        eprintln!("  gritty attach {conn_name} -t <name>");
                    },
                    Some(&out_path),
                ) {
                    Ok(fd) => Some(fd),
                    Err(e) => {
                        eprintln!("error: failed to daemonize: {e}");
                        std::process::exit(1);
                    }
                }
            };

            // In the parent process, daemonize() exits via std::process::exit.
            // If we're here, we're either the child or running in foreground.

            init_tracing(verbose, if ready_fd.is_some() { Some(&log_path) } else { None });

            let rt = match tokio::runtime::Runtime::new() {
                Ok(rt) => rt,
                Err(e) => {
                    eprintln!("error: {e}");
                    std::process::exit(1);
                }
            };

            let resolved = config.resolve_connect(&connection_name);

            let opts = gritty::connect::ConnectOpts {
                destination,
                no_server_start: no_server_start || resolved.no_server_start,
                ssh_options: {
                    let mut opts = ssh_options;
                    opts.extend(resolved.ssh_options);
                    opts
                },
                name,
                dry_run,
                foreground,
            };

            let error_pipe = dup_ready_fd(&ready_fd);
            match rt.block_on(gritty::connect::run(opts, ready_fd)) {
                Ok(code) => std::process::exit(code),
                Err(e) => report_error(&error_pipe, &e),
            }
        }
        _ => {
            init_tracing(verbose, None);
            let rt = match tokio::runtime::Runtime::new() {
                Ok(rt) => rt,
                Err(e) => {
                    eprintln!("error: {e}");
                    std::process::exit(1);
                }
            };
            if let Err(e) = rt.block_on(run(cli, config)) {
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

async fn run(cli: Cli, config: gritty::config::ConfigFile) -> anyhow::Result<()> {
    match cli.command {
        Command::Server { .. } | Command::Connect { .. } => unreachable!(),
        Command::NewSession {
            host,
            target,
            no_redraw,
            no_escape,
            forward_agent,
            forward_open,
            wait,
        } => {
            let is_default_path = cli.ctl_socket.is_none() && host.is_none();
            let ctl_path = resolve_ctl_path(cli.ctl_socket, host.as_deref())?;
            let resolved = config.resolve_session(host.as_deref());
            new_session(
                target,
                !(no_redraw || resolved.no_redraw),
                no_escape || resolved.no_escape,
                forward_agent || resolved.forward_agent,
                forward_open || resolved.forward_open,
                ctl_path,
                is_default_path,
                wait,
            )
            .await
        }
        Command::Tail { host, target } => {
            let ctl_path = resolve_ctl_path(cli.ctl_socket, host.as_deref())?;
            let code = tail_session(target, ctl_path).await?;
            std::process::exit(code);
        }
        Command::Attach {
            host,
            target,
            no_redraw,
            no_escape,
            forward_agent,
            forward_open,
            wait,
        } => {
            let is_default_path = cli.ctl_socket.is_none() && host.is_none();
            let ctl_path = resolve_ctl_path(cli.ctl_socket, host.as_deref())?;
            let resolved = config.resolve_session(host.as_deref());
            let code = attach(
                target,
                !(no_redraw || resolved.no_redraw),
                no_escape || resolved.no_escape,
                forward_agent || resolved.forward_agent,
                forward_open || resolved.forward_open,
                ctl_path,
                is_default_path,
                wait,
            )
            .await?;
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
            kill_server(ctl_path).await?;
            if let Some(host) = &host {
                gritty::connect::disconnect(host).await?;
            }
            Ok(())
        }
        Command::SocketPath => {
            let ctl_path = cli.ctl_socket.unwrap_or_else(gritty::daemon::control_socket_path);
            println!("{}", ctl_path.display());
            Ok(())
        }
        Command::Open { url } => {
            open_url(&url);
            Ok(())
        }
        Command::Disconnect { name } => gritty::connect::disconnect(&name).await,
        Command::Tunnels => {
            gritty::connect::list_tunnels();
            Ok(())
        }
        Command::Info => info(&config).await,
        Command::ConfigEdit => config_edit(),
        Command::ProtocolVersion => {
            println!("{}", gritty::protocol::PROTOCOL_VERSION);
            Ok(())
        }
        Command::Completions { shell } => {
            clap_complete::generate(shell, &mut Cli::command(), "gritty", &mut std::io::stdout());
            Ok(())
        }
    }
}

/// Start the server by running the current binary with `["server"]` args.
/// The server self-daemonizes and returns after the socket is ready.
fn auto_start_server() -> anyhow::Result<()> {
    let exe = std::env::current_exe().unwrap_or_else(|_| PathBuf::from("gritty"));
    let status = std::process::Command::new(&exe).arg("server").status()?;
    if !status.success() {
        anyhow::bail!("failed to auto-start server (exit {})", status);
    }
    Ok(())
}

/// Try to connect to the control socket. On failure, optionally auto-start the
/// server (if `can_auto_start`) and retry with a bounded loop. With `--wait`,
/// retries indefinitely instead.
async fn connect_to_server(
    ctl_path: &Path,
    can_auto_start: bool,
    wait: bool,
) -> anyhow::Result<tokio::net::UnixStream> {
    use tokio::net::UnixStream;

    match UnixStream::connect(ctl_path).await {
        Ok(s) => return Ok(s),
        Err(_) if can_auto_start => {
            eprintln!("no server running, starting one...");
            auto_start_server()?;
        }
        Err(_) if wait => {}
        Err(_) => {
            anyhow::bail!("no server running (could not connect to {})", ctl_path.display());
        }
    }

    // Retry loop: bounded (10 retries, 500ms apart) or indefinite (--wait)
    let max_retries = if wait { u32::MAX } else { 10 };
    for _ in 0..max_retries {
        match UnixStream::connect(ctl_path).await {
            Ok(s) => return Ok(s),
            Err(_) => {
                if wait {
                    eprintln!("waiting for server ({})... ctrl-c to abort", ctl_path.display());
                }
                tokio::time::sleep(std::time::Duration::from_millis(500)).await;
            }
        }
    }
    anyhow::bail!("server did not become ready ({})", ctl_path.display())
}

#[allow(clippy::too_many_arguments)]
async fn new_session(
    name: Option<String>,
    redraw: bool,
    no_escape: bool,
    forward_agent: bool,
    forward_open: bool,
    ctl_path: PathBuf,
    is_default_path: bool,
    wait: bool,
) -> anyhow::Result<()> {
    use futures_util::{SinkExt, StreamExt};
    use gritty::protocol::{Frame, FrameCodec};
    use tokio_util::codec::Framed;

    let session_name = name.clone().unwrap_or_default();

    let stream = connect_to_server(&ctl_path, is_default_path, wait).await?;
    let mut framed = Framed::new(stream, FrameCodec);
    gritty::handshake(&mut framed).await?;
    framed.send(Frame::NewSession { name: session_name }).await?;

    match Frame::expect_from(framed.next().await)? {
        Frame::SessionCreated { id } => {
            match &name {
                Some(n) => eprintln!("session created: {n} (id {id})"),
                None => eprintln!("session created: id {id}"),
            }
            let mut env_vars = gritty::collect_env_vars();
            if forward_open {
                let exe = std::env::current_exe()
                    .ok()
                    .and_then(|p| p.to_str().map(String::from))
                    .unwrap_or_else(|| "gritty".into());
                env_vars.push(("BROWSER".into(), format!("{exe} open")));
            }
            let code = gritty::client::run(
                &id,
                framed,
                redraw,
                &ctl_path,
                env_vars,
                no_escape,
                forward_agent,
                forward_open,
            )
            .await?;
            std::process::exit(code);
        }
        Frame::Error { message } => anyhow::bail!("{message}"),
        other => anyhow::bail!("unexpected response from server: {other:?}"),
    }
}

#[allow(clippy::too_many_arguments)]
async fn attach(
    target: String,
    redraw: bool,
    no_escape: bool,
    forward_agent: bool,
    forward_open: bool,
    ctl_path: PathBuf,
    is_default_path: bool,
    wait: bool,
) -> anyhow::Result<i32> {
    use futures_util::{SinkExt, StreamExt};
    use gritty::protocol::{Frame, FrameCodec};
    use tokio_util::codec::Framed;

    let stream = connect_to_server(&ctl_path, is_default_path, wait).await?;
    let mut framed = Framed::new(stream, FrameCodec);
    gritty::handshake(&mut framed).await?;
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
                forward_open,
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
    gritty::handshake(&mut framed).await?;
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

fn open_url(url: &str) {
    let sock_path = match std::env::var("GRITTY_OPEN_SOCK") {
        Ok(p) => p,
        Err(_) => {
            eprintln!(
                "error: GRITTY_OPEN_SOCK not set (are you inside a gritty session with --forward-open?)"
            );
            std::process::exit(1);
        }
    };
    match std::os::unix::net::UnixStream::connect(&sock_path) {
        Ok(mut stream) => {
            use std::io::Write;
            let _ = stream.write_all(url.as_bytes());
            let _ = stream.write_all(b"\n");
        }
        Err(e) => {
            eprintln!("error: could not connect to open socket ({sock_path}): {e}");
            std::process::exit(1);
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

async fn tail_session(target: String, ctl_path: PathBuf) -> anyhow::Result<i32> {
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
            eprintln!("[tailing session {target}]");
            gritty::client::tail(&target, framed, &ctl_path).await
        }
        Frame::Error { message } => anyhow::bail!("{message}"),
        other => anyhow::bail!("unexpected response from server: {other:?}"),
    }
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

fn config_edit() -> anyhow::Result<()> {
    let path = gritty::config::config_path();
    if !path.exists() {
        if let Some(parent) = path.parent() {
            gritty::security::secure_create_dir_all(parent)?;
        }
        std::fs::write(&path, gritty::config::DEFAULT_CONFIG)?;
        eprintln!("created {}", path.display());
    }
    let editor = std::env::var("VISUAL")
        .or_else(|_| std::env::var("EDITOR"))
        .unwrap_or_else(|_| "vi".into());
    let status = std::process::Command::new(&editor).arg(&path).status()?;
    if !status.success() {
        anyhow::bail!("{editor} exited with {status}");
    }
    Ok(())
}

async fn info(config: &gritty::config::ConfigFile) -> anyhow::Result<()> {
    use gritty::protocol::Frame;

    println!("gritty {}", env!("CARGO_PKG_VERSION"));
    println!();

    let cfg_path = gritty::config::config_path();
    let cfg_status = if cfg_path.exists() {
        if config.host.is_empty() {
            "loaded".to_string()
        } else {
            let n = config.host.len();
            let s = if n == 1 { "" } else { "s" };
            format!("loaded, {n} host{s}")
        }
    } else {
        "not found".to_string()
    };
    println!("config:         {} ({cfg_status})", cfg_path.display());

    let socket_dir = canonicalize_or_raw(gritty::daemon::socket_dir());
    let ctl_path = socket_dir.join("ctl.sock");

    println!("socket dir:     {}", socket_dir.display());
    println!("server socket:  {}", ctl_path.display());

    // Probe server status via server_request (which includes handshake)
    let pid_path = gritty::daemon::pid_file_path(&ctl_path);
    let pid = std::fs::read_to_string(&pid_path).ok().and_then(|s| s.trim().parse::<u32>().ok());

    match server_request(&ctl_path, Frame::ListSessions).await {
        Ok(Frame::SessionInfo { sessions }) => {
            let n = sessions.len();
            match pid {
                Some(p) => {
                    let s = if n == 1 { "" } else { "s" };
                    println!("server status:  running (pid {p}, {n} session{s})");
                }
                None => println!("server status:  running"),
            }
        }
        _ => {
            println!("server status:  not running");
        }
    }

    let log_path = socket_dir.join("daemon.log");
    let out_path = socket_dir.join("daemon.out");
    print_path("server log:    ", &log_path);
    print_path("server output: ", &out_path);

    // Tunnels
    let tunnels = gritty::connect::get_tunnel_info();
    if !tunnels.is_empty() {
        println!();
        println!("tunnels:");
        for t in &tunnels {
            let pid_str = match t.pid {
                Some(p) => format!(" (pid {p})"),
                None => String::new(),
            };
            println!("  {:<14}{}{pid_str}", t.name, t.status);
            print_path("                log:", &canonicalize_or_raw(t.log_path.clone()));
        }
    }

    Ok(())
}

fn print_path(label: &str, path: &Path) {
    if path.exists() {
        println!("{label} {}", path.display());
    } else {
        println!("{label} {} (not found)", path.display());
    }
}

/// Resolve symlinks in the path (e.g. /tmp → /private/tmp on macOS).
fn canonicalize_or_raw(path: PathBuf) -> PathBuf {
    std::fs::canonicalize(&path).unwrap_or(path)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resolve_ctl_path_both_args_errors() {
        let result = resolve_ctl_path(Some(PathBuf::from("/tmp/x.sock")), Some("myhost"));
        assert!(result.is_err());
        let msg = result.unwrap_err().to_string();
        assert!(msg.contains("cannot specify both"), "got: {msg}");
    }

    #[test]
    fn resolve_ctl_path_ctl_socket_only() {
        let p = PathBuf::from("/tmp/custom.sock");
        let result = resolve_ctl_path(Some(p.clone()), None).unwrap();
        assert_eq!(result, p);
    }

    #[test]
    fn resolve_ctl_path_host_only() {
        let result = resolve_ctl_path(None, Some("devbox")).unwrap();
        let s = result.to_string_lossy();
        assert!(s.contains("connect-devbox.sock"), "got: {s}");
    }

    #[test]
    fn resolve_ctl_path_neither() {
        let result = resolve_ctl_path(None, None).unwrap();
        assert_eq!(result, gritty::daemon::control_socket_path());
    }

    #[test]
    fn format_timestamp_epoch_zero() {
        let s = format_timestamp(0);
        assert_eq!(s.len(), 19, "got: {s}");
        // Could be 1970 (UTC) or 1969 (negative UTC offset)
        assert!(s.contains("1970") || s.contains("1969"), "got: {s}");
    }

    #[test]
    fn format_timestamp_recent() {
        let s = format_timestamp(1_700_000_000);
        assert_eq!(s.len(), 19, "got: {s}");
        assert!(s.starts_with("202"), "got: {s}");
    }
}
