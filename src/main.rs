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
    #[command(visible_alias = "s")]
    Server {
        /// Run in the foreground instead of daemonizing
        #[arg(long, short = 'f')]
        foreground: bool,
    },
    /// Create a new persistent session (auto-attaches)
    #[command(visible_alias = "new")]
    NewSession {
        /// Target host, with optional session name (host or host:name)
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

        /// Disable OAuth callback tunneling (part of --forward-open)
        #[arg(long)]
        no_oauth_redirect: bool,

        /// Timeout in seconds for OAuth callback tunnel (default: 180)
        #[arg(long)]
        oauth_timeout: Option<u64>,

        /// Wait indefinitely for the server instead of giving up after retries
        #[arg(short = 'w', long)]
        wait: bool,
    },
    /// Attach to an existing session (detaches other clients)
    #[command(visible_alias = "a")]
    Attach {
        /// Target host and session (host:session)
        target: Option<String>,

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

        /// Disable OAuth callback tunneling (part of --forward-open)
        #[arg(long)]
        no_oauth_redirect: bool,

        /// Timeout in seconds for OAuth callback tunnel (default: 180)
        #[arg(long)]
        oauth_timeout: Option<u64>,
    },
    /// Tail a session's output (read-only, like tail -f)
    #[command(visible_alias = "t")]
    Tail {
        /// Target host and session (host:session)
        target: Option<String>,
    },
    /// List active sessions
    #[command(visible_alias = "ls", visible_alias = "list")]
    ListSessions {
        /// Target host
        target: Option<String>,
    },
    /// Kill a specific session
    KillSession {
        /// Target host and session (host:session)
        target: Option<String>,
    },
    /// Kill the server and all sessions
    KillServer {
        /// Target host
        target: Option<String>,
    },
    /// Send files to a paired receiver
    Send {
        /// Session to use (host:session); auto-detected if omitted
        #[arg(long)]
        session: Option<String>,

        /// Files to send
        #[arg(required = true)]
        files: Vec<PathBuf>,
    },
    /// Receive files from a paired sender
    Receive {
        /// Session to use (host:session); auto-detected if omitted
        #[arg(long)]
        session: Option<String>,

        /// Destination directory (default: current directory)
        dir: Option<PathBuf>,
    },
    /// Open a URL on the local machine (for use inside gritty sessions)
    Open {
        /// URL to open
        url: String,
    },
    /// Forward a port from the session to the client (listen on session, connect on client)
    #[command(visible_alias = "lf")]
    LocalForward {
        /// Port spec: PORT or LISTEN_PORT:TARGET_PORT
        port: String,
    },
    /// Forward a port from the client to the session (listen on client, connect on session)
    #[command(visible_alias = "rf")]
    RemoteForward {
        /// Port spec: PORT or LISTEN_PORT:TARGET_PORT
        port: String,
    },
    /// Print the default socket path
    #[command(visible_alias = "socket")]
    SocketPath,
    /// SSH tunnel to a remote host (backgrounds by default, prints socket path)
    #[command(visible_alias = "c")]
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
    #[command(visible_alias = "dc")]
    Disconnect {
        /// Connection name (as shown in `gritty tunnels`)
        name: String,
    },
    /// List active SSH tunnels
    #[command(visible_alias = "tun")]
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
            if connection_name == "local" {
                eprintln!(
                    "error: 'local' is reserved for the local server; \
                     use 'localhost.' to SSH to this machine"
                );
                std::process::exit(1);
            }
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

/// Parse a `host[:session]` target string. Splits on the first `:`.
fn parse_target(s: &str) -> (String, Option<String>) {
    match s.split_once(':') {
        Some((host, session)) if !session.is_empty() => {
            (host.to_string(), Some(session.to_string()))
        }
        Some((host, _)) => (host.to_string(), None),
        None => (s.to_string(), None),
    }
}

fn resolve_ctl_path(ctl_socket: Option<PathBuf>, host: Option<&str>) -> anyhow::Result<PathBuf> {
    if let Some(p) = ctl_socket {
        return Ok(p);
    }
    match host {
        Some("local") => Ok(gritty::daemon::control_socket_path()),
        Some(h) => Ok(gritty::daemon::socket_dir().join(format!("connect-{h}.sock"))),
        None => anyhow::bail!("specify a host or use --ctl-socket"),
    }
}

async fn run(cli: Cli, config: gritty::config::ConfigFile) -> anyhow::Result<()> {
    match cli.command {
        Command::Server { .. } | Command::Connect { .. } => unreachable!(),
        Command::NewSession {
            target,
            no_redraw,
            no_escape,
            forward_agent,
            forward_open,
            no_oauth_redirect,
            oauth_timeout,
            wait,
        } => {
            let (host, session) = match &target {
                Some(t) => {
                    let (h, s) = parse_target(t);
                    (Some(h), s)
                }
                None => (None, None),
            };
            let auto_start_mode = match (&cli.ctl_socket, host.as_deref()) {
                (Some(_), _) => AutoStart::None,
                (None, Some("local")) => AutoStart::Server,
                (None, Some(h)) => AutoStart::Tunnel(h.to_string()),
                (None, None) => anyhow::bail!("specify a host or use --ctl-socket"),
            };
            let ctl_path = resolve_ctl_path(cli.ctl_socket, host.as_deref())?;
            let resolved = config.resolve_session(host.as_deref());
            let settings = gritty::config::SessionSettings {
                no_redraw: no_redraw || resolved.no_redraw,
                no_escape: no_escape || resolved.no_escape,
                forward_agent: forward_agent || resolved.forward_agent,
                forward_open: forward_open || resolved.forward_open,
                oauth_redirect: if no_oauth_redirect { false } else { resolved.oauth_redirect },
                oauth_timeout: oauth_timeout.unwrap_or(resolved.oauth_timeout),
            };
            new_session(session, settings, ctl_path, auto_start_mode, wait).await
        }
        Command::Tail { target } => {
            let (host, session) = match &target {
                Some(t) => {
                    let (h, s) = parse_target(t);
                    (Some(h), s)
                }
                None => (None, None),
            };
            let ctl_path = resolve_ctl_path(cli.ctl_socket, host.as_deref())?;
            let session = match session {
                Some(s) => s,
                None => {
                    suggest_session("tail", host.as_deref().unwrap_or("host"), &ctl_path).await?;
                    unreachable!()
                }
            };
            let code = tail_session(session, ctl_path).await?;
            std::process::exit(code);
        }
        Command::Attach {
            target,
            no_redraw,
            no_escape,
            forward_agent,
            forward_open,
            no_oauth_redirect,
            oauth_timeout,
        } => {
            let (host, session) = match &target {
                Some(t) => {
                    let (h, s) = parse_target(t);
                    (Some(h), s)
                }
                None => (None, None),
            };
            let ctl_path = resolve_ctl_path(cli.ctl_socket, host.as_deref())?;
            let resolved = config.resolve_session(host.as_deref());
            let settings = gritty::config::SessionSettings {
                no_redraw: no_redraw || resolved.no_redraw,
                no_escape: no_escape || resolved.no_escape,
                forward_agent: forward_agent || resolved.forward_agent,
                forward_open: forward_open || resolved.forward_open,
                oauth_redirect: if no_oauth_redirect { false } else { resolved.oauth_redirect },
                oauth_timeout: oauth_timeout.unwrap_or(resolved.oauth_timeout),
            };
            let session = match session {
                Some(s) => s,
                None => {
                    suggest_session("attach", host.as_deref().unwrap_or("host"), &ctl_path).await?;
                    unreachable!()
                }
            };
            let code = attach(session, settings, ctl_path).await?;
            std::process::exit(code);
        }
        Command::ListSessions { target } => {
            let host = target.as_deref().map(|t| parse_target(t).0);
            let ctl_path = resolve_ctl_path(cli.ctl_socket, host.as_deref())?;
            list_sessions(ctl_path).await
        }
        Command::KillSession { target } => {
            let (host, session) = match &target {
                Some(t) => {
                    let (h, s) = parse_target(t);
                    (Some(h), s)
                }
                None => (None, None),
            };
            let ctl_path = resolve_ctl_path(cli.ctl_socket, host.as_deref())?;
            let session = match session {
                Some(s) => s,
                None => {
                    suggest_session("kill-session", host.as_deref().unwrap_or("host"), &ctl_path)
                        .await?;
                    unreachable!()
                }
            };
            kill_session(session, ctl_path).await
        }
        Command::KillServer { target } => {
            let host = target.as_deref().map(|t| parse_target(t).0);
            let ctl_path = resolve_ctl_path(cli.ctl_socket, host.as_deref())?;
            kill_server(ctl_path).await?;
            if let Some(h) = host.as_deref() {
                if h != "local" {
                    gritty::connect::disconnect(h).await?;
                }
            }
            Ok(())
        }
        Command::SocketPath => {
            let ctl_path = cli.ctl_socket.unwrap_or_else(gritty::daemon::control_socket_path);
            println!("{}", ctl_path.display());
            Ok(())
        }
        Command::Send { session, files } => send_command(cli.ctl_socket, session, files).await,
        Command::Receive { session, dir } => receive_command(cli.ctl_socket, session, dir).await,
        Command::Open { url } => {
            open_url(&url);
            Ok(())
        }
        Command::LocalForward { port } => {
            let (listen_port, target_port) = parse_port_spec(&port)?;
            port_forward_command(0, listen_port, target_port).await
        }
        Command::RemoteForward { port } => {
            let (listen_port, target_port) = parse_port_spec(&port)?;
            port_forward_command(1, listen_port, target_port).await
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

/// What to auto-start when new-session can't connect.
enum AutoStart {
    /// Explicit --ctl-socket: no auto-start
    None,
    /// Default path, no host: start local server
    Server,
    /// Host-routed: start SSH tunnel via `gritty connect <host>`
    Tunnel(String),
}

/// Run the current binary with the given args. Both `gritty server` and
/// `gritty connect <host>` self-daemonize and return after the socket is ready.
fn auto_start(args: &[&str]) -> anyhow::Result<()> {
    let exe = std::env::current_exe().unwrap_or_else(|_| PathBuf::from("gritty"));
    let status = std::process::Command::new(&exe).args(args).status()?;
    if !status.success() {
        anyhow::bail!("failed to start `gritty {}` (exit {})", args.join(" "), status);
    }
    Ok(())
}

/// Try to connect to the control socket. On failure, auto-start the
/// appropriate process and retry with a bounded loop (or indefinitely
/// with `--wait`).
async fn connect_or_start(
    ctl_path: &Path,
    auto_start_mode: &AutoStart,
    wait: bool,
) -> anyhow::Result<tokio::net::UnixStream> {
    use tokio::net::UnixStream;

    match UnixStream::connect(ctl_path).await {
        Ok(s) => return Ok(s),
        Err(_) => match auto_start_mode {
            AutoStart::Server => {
                eprintln!("no server running, starting one...");
                auto_start(&["server"])?;
            }
            AutoStart::Tunnel(host) => {
                eprintln!("no tunnel running for {host}, starting one...");
                auto_start(&["connect", host])?;
            }
            AutoStart::None if wait => {}
            AutoStart::None => {
                anyhow::bail!("no server running (could not connect to {})", ctl_path.display());
            }
        },
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

async fn new_session(
    name: Option<String>,
    settings: gritty::config::SessionSettings,
    ctl_path: PathBuf,
    auto_start_mode: AutoStart,
    wait: bool,
) -> anyhow::Result<()> {
    use futures_util::{SinkExt, StreamExt};
    use gritty::protocol::{Frame, FrameCodec};
    use tokio_util::codec::Framed;

    let session_name = name.clone().unwrap_or_default();

    let stream = connect_or_start(&ctl_path, &auto_start_mode, wait).await?;
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
    settings: gritty::config::SessionSettings,
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
    gritty::handshake(&mut framed).await?;
    framed.send(Frame::Attach { session: target.clone() }).await?;

    match Frame::expect_from(framed.next().await)? {
        Frame::Ok => {
            eprintln!("[attached]");
            let code = gritty::client::run(
                &target,
                framed,
                !settings.no_redraw,
                &ctl_path,
                vec![],
                settings.no_escape,
                settings.forward_agent,
                settings.forward_open,
                settings.oauth_redirect,
                settings.oauth_timeout,
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

/// Parse a port spec: "PORT" or "LISTEN_PORT:TARGET_PORT".
fn parse_port_spec(spec: &str) -> anyhow::Result<(u16, u16)> {
    if let Some((a, b)) = spec.split_once(':') {
        let listen: u16 = a.parse().map_err(|_| anyhow::anyhow!("invalid listen port: {a}"))?;
        let target: u16 = b.parse().map_err(|_| anyhow::anyhow!("invalid target port: {b}"))?;
        Ok((listen, target))
    } else {
        let port: u16 = spec.parse().map_err(|_| anyhow::anyhow!("invalid port: {spec}"))?;
        Ok((port, port))
    }
}

/// Run a port forward command. Connects to GRITTY_SOCK, sends the request,
/// reads the response, prints status, and blocks until SIGINT or EOF.
async fn port_forward_command(
    direction: u8,
    listen_port: u16,
    target_port: u16,
) -> anyhow::Result<()> {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    let sock_path = match std::env::var("GRITTY_SOCK") {
        Ok(p) => p,
        Err(_) => {
            anyhow::bail!("GRITTY_SOCK not set (are you inside a gritty session?)");
        }
    };

    let mut stream = tokio::net::UnixStream::connect(&sock_path).await?;

    // Write: [discriminator][direction][listen_port BE][target_port BE]
    let mut header = [0u8; 6];
    header[0] = gritty::protocol::SvcRequest::PortForward.to_byte();
    header[1] = direction;
    header[2..4].copy_from_slice(&listen_port.to_be_bytes());
    header[4..6].copy_from_slice(&target_port.to_be_bytes());
    stream.write_all(&header).await?;

    // Read response: 0x01 = success, 0x02 + message = error
    let mut resp = [0u8; 1];
    stream.read_exact(&mut resp).await?;
    if resp[0] == 0x02 {
        let mut msg = Vec::new();
        stream.read_to_end(&mut msg).await?;
        let msg = String::from_utf8_lossy(&msg);
        anyhow::bail!("{msg}");
    }
    if resp[0] != 0x01 {
        anyhow::bail!("unexpected response: 0x{:02x}", resp[0]);
    }

    let dir_str = if direction == 0 { "local" } else { "remote" };
    let port_str = if listen_port == target_port {
        format!("{listen_port}")
    } else {
        format!("{listen_port}:{target_port}")
    };
    eprintln!("{dir_str}-forward {port_str} active (ctrl-c to stop)");

    // Block until SIGINT or stream EOF
    let mut sigint = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::interrupt())?;
    let mut sigterm = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())?;
    let mut buf = [0u8; 1];
    tokio::select! {
        _ = sigint.recv() => {}
        _ = sigterm.recv() => {}
        _ = stream.read(&mut buf) => {}
    }
    // Stream drop closes the connection, triggering server-side cleanup
    Ok(())
}

fn open_url(url: &str) {
    let sock_path = match std::env::var("GRITTY_SOCK") {
        Ok(p) => p,
        Err(_) => {
            eprintln!(
                "error: GRITTY_SOCK not set (are you inside a gritty session with --forward-open?)"
            );
            std::process::exit(1);
        }
    };
    match std::os::unix::net::UnixStream::connect(&sock_path) {
        Ok(mut stream) => {
            use std::io::Write;
            let _ = stream.write_all(&[gritty::protocol::SvcRequest::OpenUrl.to_byte()]);
            let _ = stream.write_all(url.as_bytes());
            let _ = stream.write_all(b"\n");
        }
        Err(e) => {
            eprintln!("error: could not connect to service socket ({sock_path}): {e}");
            std::process::exit(1);
        }
    }
}

fn format_age(now: u64, created_at: u64) -> String {
    let secs = now.saturating_sub(created_at);
    if secs < 60 {
        format!("{secs}s ago")
    } else if secs < 3600 {
        format!("{}m ago", secs / 60)
    } else if secs < 86400 {
        format!("{}h ago", secs / 3600)
    } else {
        format!("{}d ago", secs / 86400)
    }
}

/// Print available sessions and exit with an error when a session-requiring
/// command is invoked without the session part (e.g. `gritty attach local`
/// instead of `gritty attach local:session`).
async fn suggest_session(cmd: &str, host: &str, ctl_path: &Path) -> anyhow::Result<()> {
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
    framed.send(Frame::SendFile { session: session.to_string(), role }).await?;

    match Frame::expect_from(framed.next().await)? {
        Frame::Ok => {}
        Frame::Error { message } => anyhow::bail!("{message}"),
        other => anyhow::bail!("unexpected response: {other:?}"),
    }

    let mut stream = framed.into_inner();
    stream.write_all(&[role]).await?;
    Ok(stream)
}

/// Connect to service sockets for transfer. Returns one or more streams.
/// In-session or explicit --session returns one; auto-detect returns all.
async fn connect_send_sockets(
    ctl_socket: Option<PathBuf>,
    session_flag: Option<String>,
    role: u8,
) -> anyhow::Result<Vec<tokio::net::UnixStream>> {
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
        return Ok(vec![stream]);
    }

    // Explicit --session flag
    if let Some(target) = session_flag {
        let (host, session) = parse_target(&target);
        let session = session
            .ok_or_else(|| anyhow::anyhow!("--session requires host:session (e.g. local:0)"))?;
        let ctl_path = resolve_ctl_path(ctl_socket, Some(&host))?;
        let stream = send_file_handshake(&ctl_path, &session, role).await?;
        return Ok(vec![stream]);
    }

    // Auto-detect: connect to ALL sessions
    let sessions = discover_all_sessions(ctl_socket.as_deref()).await?;
    let mut streams = Vec::new();
    for s in &sessions {
        if let Ok(stream) = send_file_handshake(&s.ctl_path, &s.session_id, role).await {
            streams.push(stream);
        }
    }
    if streams.is_empty() {
        anyhow::bail!("no active sessions");
    }
    Ok(streams)
}

/// Wait for the first stream to become readable, return it (drop the rest).
async fn select_first_ready(
    streams: Vec<tokio::net::UnixStream>,
) -> anyhow::Result<tokio::net::UnixStream> {
    use futures_util::future::select_all;

    let futs: Vec<_> = streams
        .into_iter()
        .map(|stream| {
            Box::pin(async move {
                stream.readable().await?;
                Ok::<_, std::io::Error>(stream)
            })
        })
        .collect();

    let (result, _, _) = select_all(futs).await;
    Ok(result?)
}

async fn write_send_manifest(
    stream: &mut tokio::net::UnixStream,
    entries: &[(String, u64, PathBuf)],
) -> anyhow::Result<()> {
    use tokio::io::AsyncWriteExt;
    let file_count = entries.len() as u32;
    stream.write_all(&file_count.to_be_bytes()).await?;
    for (name, size, _) in entries {
        let name_bytes = name.as_bytes();
        stream.write_all(&(name_bytes.len() as u16).to_be_bytes()).await?;
        stream.write_all(name_bytes).await?;
        stream.write_all(&size.to_be_bytes()).await?;
    }
    Ok(())
}

fn print_progress(name: &str, transferred: u64, total: u64) {
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

async fn send_command(
    ctl_socket: Option<PathBuf>,
    session: Option<String>,
    files: Vec<PathBuf>,
) -> anyhow::Result<()> {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    // Validate files exist and collect metadata
    let mut entries: Vec<(String, u64, PathBuf)> = Vec::with_capacity(files.len());
    for path in &files {
        let meta =
            std::fs::metadata(path).map_err(|e| anyhow::anyhow!("{}: {e}", path.display()))?;
        if !meta.is_file() {
            anyhow::bail!("{}: not a regular file", path.display());
        }
        let basename = sanitize_basename(&path.to_string_lossy())?;
        entries.push((basename, meta.len(), path.clone()));
    }

    let mut streams =
        connect_send_sockets(ctl_socket, session, gritty::protocol::SvcRequest::Send.to_byte())
            .await?;

    // Write manifest on all streams
    for stream in &mut streams {
        write_send_manifest(stream, &entries).await?;
    }

    // Wait for go signal -- first stream to get paired wins
    eprintln!("\x1b[2mwaiting for receiver...\x1b[0m");
    let mut stream = if streams.len() == 1 {
        streams.into_iter().next().unwrap()
    } else {
        select_first_ready(streams).await?
    };

    let mut go = [0u8; 1];
    stream.read_exact(&mut go).await.map_err(|_| anyhow::anyhow!("no receiver connected"))?;
    if go[0] != 0x01 {
        anyhow::bail!("unexpected signal from server: 0x{:02x}", go[0]);
    }

    // Stream file data
    let total_bytes: u64 = entries.iter().map(|(_, s, _)| s).sum();
    let total_str = gritty::client::format_size(total_bytes);
    let s = if entries.len() == 1 { "" } else { "s" };
    eprintln!("sending {} file{s} ({total_str})", entries.len());

    let mut buf = vec![0u8; 64 * 1024];
    for (name, size, path) in &entries {
        let mut file = tokio::fs::File::open(path).await?;
        let mut remaining = *size;
        let mut transferred = 0u64;
        while remaining > 0 {
            let to_read = (remaining as usize).min(buf.len());
            let n = file.read(&mut buf[..to_read]).await?;
            if n == 0 {
                anyhow::bail!("unexpected EOF reading {name}");
            }
            stream.write_all(&buf[..n]).await?;
            remaining -= n as u64;
            transferred += n as u64;
            print_progress(name, transferred, *size);
        }
        eprintln!();
    }

    eprintln!("\x1b[32mdone\x1b[0m");
    Ok(())
}

async fn receive_command(
    ctl_socket: Option<PathBuf>,
    session: Option<String>,
    dir: Option<PathBuf>,
) -> anyhow::Result<()> {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    let dest_dir = dir.unwrap_or_else(|| PathBuf::from("."));
    if !dest_dir.is_dir() {
        anyhow::bail!("{}: not a directory", dest_dir.display());
    }

    let mut streams =
        connect_send_sockets(ctl_socket, session, gritty::protocol::SvcRequest::Receive.to_byte())
            .await?;

    // Write dest dir on all streams
    let dest_str = dest_dir.to_string_lossy();
    for stream in &mut streams {
        stream.write_all(dest_str.as_bytes()).await?;
        stream.write_all(b"\n").await?;
    }

    // Wait for file data -- first stream to get paired wins
    eprintln!("\x1b[2mwaiting for sender...\x1b[0m");
    let mut stream = if streams.len() == 1 {
        streams.into_iter().next().unwrap()
    } else {
        select_first_ready(streams).await?
    };

    // Read: file_count (u32 BE)
    let mut buf4 = [0u8; 4];
    stream.read_exact(&mut buf4).await.map_err(|_| anyhow::anyhow!("no sender connected"))?;
    let file_count = u32::from_be_bytes(buf4);

    // Read per-file metadata and data
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
        let name = sanitize_basename(&name)?;

        // Read file_size (u64 BE)
        let mut buf8 = [0u8; 8];
        stream.read_exact(&mut buf8).await?;
        let file_size = u64::from_be_bytes(buf8);

        let s = if file_count == 1 { "" } else { "s" };
        if received == 0 {
            eprintln!("receiving {file_count} file{s}");
        }

        // Write file data
        let file_path = dest_dir.join(&name);
        let mut file = tokio::fs::OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .mode(0o600)
            .open(&file_path)
            .await?;
        let mut remaining = file_size;
        let mut transferred = 0u64;
        while remaining > 0 {
            let to_read = (remaining as usize).min(buf.len());
            stream.read_exact(&mut buf[..to_read]).await?;
            file.write_all(&buf[..to_read]).await?;
            remaining -= to_read as u64;
            transferred += to_read as u64;
            print_progress(&name, transferred, file_size);
        }
        eprintln!();
        received += 1;
    }

    if received == 0 {
        eprintln!("no files received");
    } else {
        let s = if received == 1 { "" } else { "s" };
        eprintln!("\x1b[32mreceived {received} file{s}\x1b[0m");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_target_host_only() {
        let (host, session) = parse_target("local");
        assert_eq!(host, "local");
        assert_eq!(session, None);
    }

    #[test]
    fn parse_target_host_and_session() {
        let (host, session) = parse_target("local:work");
        assert_eq!(host, "local");
        assert_eq!(session, Some("work".to_string()));
    }

    #[test]
    fn parse_target_remote_and_id() {
        let (host, session) = parse_target("devbox:0");
        assert_eq!(host, "devbox");
        assert_eq!(session, Some("0".to_string()));
    }

    #[test]
    fn parse_target_colon_in_session_name() {
        let (host, session) = parse_target("local:my:weird:name");
        assert_eq!(host, "local");
        assert_eq!(session, Some("my:weird:name".to_string()));
    }

    #[test]
    fn parse_target_empty_session() {
        let (host, session) = parse_target("local:");
        assert_eq!(host, "local");
        assert_eq!(session, None);
    }

    #[test]
    fn resolve_ctl_path_ctl_socket_wins() {
        let p = PathBuf::from("/tmp/x.sock");
        let result = resolve_ctl_path(Some(p.clone()), Some("myhost")).unwrap();
        assert_eq!(result, p);
    }

    #[test]
    fn resolve_ctl_path_ctl_socket_no_host() {
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
    fn resolve_ctl_path_local() {
        let result = resolve_ctl_path(None, Some("local")).unwrap();
        assert_eq!(result, gritty::daemon::control_socket_path());
    }

    #[test]
    fn resolve_ctl_path_none_none_errors() {
        assert!(resolve_ctl_path(None, None).is_err());
    }

    #[test]
    fn format_age_seconds() {
        assert_eq!(format_age(100, 70), "30s ago");
    }

    #[test]
    fn format_age_minutes() {
        assert_eq!(format_age(1000, 700), "5m ago");
    }

    #[test]
    fn format_age_hours() {
        assert_eq!(format_age(10000, 0), "2h ago");
    }

    #[test]
    fn format_age_days() {
        assert_eq!(format_age(200000, 0), "2d ago");
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

    #[test]
    fn parse_port_spec_single() {
        let (l, t) = parse_port_spec("8080").unwrap();
        assert_eq!(l, 8080);
        assert_eq!(t, 8080);
    }

    #[test]
    fn parse_port_spec_pair() {
        let (l, t) = parse_port_spec("9090:3000").unwrap();
        assert_eq!(l, 9090);
        assert_eq!(t, 3000);
    }

    #[test]
    fn parse_port_spec_invalid() {
        assert!(parse_port_spec("abc").is_err());
        assert!(parse_port_spec("80:xyz").is_err());
        assert!(parse_port_spec("99999").is_err());
    }
}
