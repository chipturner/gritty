mod commands;

use clap::{CommandFactory, Parser, Subcommand};
use commands::*;
use std::os::fd::{AsRawFd, OwnedFd};
use std::path::{Path, PathBuf};
use tracing_subscriber::EnvFilter;

fn version_string() -> &'static str {
    static VERSION: std::sync::OnceLock<String> = std::sync::OnceLock::new();
    VERSION.get_or_init(|| format!("{} ({})", env!("CARGO_PKG_VERSION"), env!("GRITTY_GIT_HASH"),))
}

// help_template left-aligned so the string content has no spurious indentation
#[derive(Parser)]
#[command(
    name = "gritty",
    version = version_string(),
    about = "Persistent TTY sessions over Unix domain sockets",
    help_template = "\
{before-help}{about}

{usage-heading} {usage}

Sessions:
  connect (c)            Attach or create a session
  list-sessions (ls)     List active sessions
  tail (t)               Read-only stream of session output
  kill-session           Kill a session
  rename                 Rename a session
  kill-server            Kill the server and all sessions

Tunnels:
  tunnels (tun)          List active SSH tunnels
  tunnel-create          Set up SSH tunnel to a remote host
  tunnel-destroy         Tear down an SSH tunnel
  bootstrap              Install gritty on a remote host

In-session (run inside a gritty session):
  local-forward (lf)     Forward a port: session to client
  remote-forward (rf)    Forward a port: client to session
  send                   Send files to a paired receiver
  receive                Receive files from a paired sender
  open                   Open a URL on the local machine

Configuration:
  info                   Show diagnostics (paths, server, tunnels)
  config-edit            Open config in $VISUAL/$EDITOR/vi

Plumbing:
  server (s)             Start the server
  completions            Generate shell completions
  socket-path            Print the default socket path
  protocol-version       Print the protocol version

Options:
{options}
See 'gritty help <command>' for details.
{after-help}"
)]
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
    // -- Sessions --
    /// Smart session: attach if exists, create if not
    #[command(display_order = 0, visible_alias = "c")]
    Connect {
        /// Target host, with optional session name (host or host:name)
        target: Option<String>,

        /// Command to run instead of login shell
        #[arg(short = 'c', long)]
        command: Option<String>,

        /// Create session but don't attach (for background jobs)
        #[arg(short = 'd', long)]
        detach: bool,

        /// Attach only, error if session doesn't exist
        #[arg(long)]
        no_create: bool,

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

        /// Disable SSH agent forwarding
        #[arg(long)]
        no_forward_agent: bool,

        /// Disable URL open forwarding
        #[arg(long)]
        no_forward_open: bool,

        /// Disable OAuth callback tunneling (part of --forward-open)
        #[arg(long)]
        no_oauth_redirect: bool,

        /// Timeout in seconds for OAuth callback tunnel (default: 180)
        #[arg(long)]
        oauth_timeout: Option<u64>,

        /// Wait indefinitely for the server instead of giving up after retries
        #[arg(short = 'w', long)]
        wait: bool,

        /// Take over an already-attached session without prompting
        #[arg(long)]
        force: bool,

        /// Always show session picker, even if unambiguous
        #[arg(long, conflicts_with = "no_pick")]
        pick: bool,

        /// Never show session picker; always target "default"
        #[arg(long, conflicts_with = "pick")]
        no_pick: bool,
    },
    /// Tail a session's output (read-only, like tail -f)
    #[command(display_order = 2, visible_alias = "t")]
    Tail {
        /// Target host and session (host:session)
        target: Option<String>,
    },
    /// List active sessions
    #[command(display_order = 1, visible_alias = "ls", visible_alias = "list")]
    ListSessions {
        /// Target host
        target: Option<String>,
    },
    /// Kill a session
    #[command(display_order = 3)]
    KillSession {
        /// Target host and session (host:session)
        target: Option<String>,
    },
    /// Kill the server and all sessions
    #[command(display_order = 5)]
    KillServer {
        /// Target host
        target: Option<String>,
    },
    /// Rename a session
    #[command(display_order = 4)]
    Rename {
        /// Target host and session (host:session)
        target: String,
        /// New name for the session
        new_name: String,
    },
    // -- In-session tools --
    /// Send files to a paired receiver
    #[command(display_order = 32)]
    Send {
        /// Session to use (host:session); auto-detected if omitted
        #[arg(long)]
        session: Option<String>,

        /// Read data from stdin instead of files (use - as shorthand)
        #[arg(long, hide = true)]
        stdin: bool,

        /// Timeout in seconds waiting for a receiver (default: 300)
        #[arg(long, default_value_t = 300)]
        timeout: u64,

        /// Wait indefinitely for a receiver
        #[arg(long)]
        no_timeout: bool,

        /// Files to send (use - for stdin)
        files: Vec<PathBuf>,
    },
    /// Receive files from a paired sender
    #[command(display_order = 33)]
    Receive {
        /// Session to use (host:session); auto-detected if omitted
        #[arg(long)]
        session: Option<String>,

        /// Write received data to stdout instead of files (use - as shorthand)
        #[arg(long, hide = true)]
        stdout: bool,

        /// Timeout in seconds waiting for a sender (default: 300)
        #[arg(long, default_value_t = 300)]
        timeout: u64,

        /// Wait indefinitely for a sender
        #[arg(long)]
        no_timeout: bool,

        /// Destination directory (default: current directory, - for stdout)
        dir: Option<PathBuf>,
    },
    /// Open a URL on the local machine (for use inside gritty sessions)
    #[command(display_order = 34)]
    Open {
        /// URL to open
        url: String,
    },
    /// Forward a port from the session to the client (listen on session, connect on client)
    #[command(display_order = 30, visible_alias = "lf")]
    LocalForward {
        /// Port spec: PORT or LISTEN_PORT:TARGET_PORT
        port: String,
    },
    /// Forward a port from the client to the session (listen on client, connect on session)
    #[command(display_order = 31, visible_alias = "rf")]
    RemoteForward {
        /// Port spec: PORT or LISTEN_PORT:TARGET_PORT
        port: String,
    },
    // -- Tunnels --
    /// Set up SSH tunnel to a remote host (backgrounds by default)
    #[command(display_order = 11)]
    TunnelCreate {
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

        /// Connect even if remote protocol version differs from local
        #[arg(long)]
        ignore_version_mismatch: bool,
    },
    /// Tear down an SSH tunnel by connection name
    #[command(display_order = 12)]
    TunnelDestroy {
        /// Connection name (as shown in `gritty tunnels`)
        name: String,
    },
    /// Install gritty on a remote host (downloads release via install script)
    #[command(display_order = 13)]
    Bootstrap {
        /// Remote destination ([user@]host[:port])
        destination: String,

        /// Remote install directory (default: ~/.local/bin)
        #[arg(long, default_value = "~/.local/bin")]
        install_dir: String,

        /// Extra SSH options (can be repeated)
        #[arg(long = "ssh-option", short = 'o')]
        ssh_options: Vec<String>,
    },
    /// List active SSH tunnels
    #[command(display_order = 10, visible_alias = "tun")]
    Tunnels,
    // -- Server & config --
    /// Start the server (backgrounds by default, use -f to stay in foreground)
    #[command(display_order = 40, visible_alias = "s")]
    Server {
        /// Run in the foreground instead of daemonizing
        #[arg(long, short = 'f')]
        foreground: bool,
    },
    /// Show diagnostics (paths, server status, tunnels)
    #[command(display_order = 20)]
    Info,
    /// Open config file in $VISUAL/$EDITOR/vi (creates from template if missing)
    #[command(display_order = 21)]
    ConfigEdit,
    /// Generate shell completions
    #[command(display_order = 41)]
    Completions {
        /// Shell to generate completions for
        shell: clap_complete::Shell,
    },
    // -- Internal/plumbing --
    /// Print the default socket path
    #[command(display_order = 42, visible_alias = "socket")]
    SocketPath,
    /// Print the protocol version number
    #[command(display_order = 43)]
    ProtocolVersion,
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
/// Double-fork daemonize: parent -> session leader -> grandchild (daemon).
///
/// Parent: blocks reading the pipe. Gets `[0x01][pid: u32 LE]` -> daemon ready, runs
/// `on_ready(pid)`, exits 0. Gets other data -> error message, exits 1. Gets EOF -> crashed.
/// Session leader (middle child): calls setsid(), forks again, exits immediately.
/// Grandchild (daemon): redirects stdio, chdir("/"), returns Ok(OwnedFd) for the pipe.
///
/// If `output_path` is `Some`, stdout/stderr are redirected to that file (O_APPEND).
/// Otherwise they go to `/dev/null`. stdin always goes to `/dev/null`.
fn daemonize(on_ready: impl FnOnce(u32), output_path: Option<&Path>) -> anyhow::Result<OwnedFd> {
    use nix::unistd::{ForkResult, fork, pipe, setsid};
    let (read_fd, write_fd) = pipe()?;

    // Safety: fork before any threads (tokio runtime not yet created)
    match unsafe { fork() }? {
        ForkResult::Parent { child } => {
            // Close write end
            drop(write_fd);

            // Reap the middle child (exits immediately after second fork)
            let _ = nix::sys::wait::waitpid(child, None);

            // Read from pipe: 0x01 + pid = daemon ready, other data = error, EOF = crashed
            let mut first = [0u8; 1];
            let mut read_file = std::fs::File::from(read_fd);
            use std::io::Read;
            match read_file.read(&mut first) {
                Ok(1) if first[0] == 0x01 => {
                    let mut pid_buf = [0u8; 4];
                    if read_file.read_exact(&mut pid_buf).is_ok() {
                        on_ready(u32::from_le_bytes(pid_buf));
                        std::process::exit(0);
                    }
                    eprintln!("error: failed to start");
                    std::process::exit(1);
                }
                Ok(1) => {
                    // Error message from child -- read the rest
                    let mut msg = vec![first[0]];
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

            // Second fork: session leader exits, grandchild can't acquire a tty
            match unsafe { fork() }? {
                ForkResult::Parent { .. } => {
                    // Middle child (session leader): exit immediately.
                    // write_fd is closed by the OS; grandchild's copy keeps the pipe alive.
                    std::process::exit(0);
                }
                ForkResult::Child => {
                    // Grandchild: not a session leader
                    let _ = std::env::set_current_dir("/");

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
    // When invoked as "gritty-open" (symlink), dispatch directly to open.
    if let Some(prog) = std::env::args().next() {
        if Path::new(&prog).file_name().and_then(|f| f.to_str()) == Some("gritty-open") {
            let url = match std::env::args().nth(1) {
                Some(u) => u,
                None => {
                    eprintln!("usage: gritty-open <url>");
                    std::process::exit(1);
                }
            };
            open_url(&url);
            return;
        }
    }

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
                match daemonize(
                    |pid| eprintln!("\x1b[32m\u{25b8} server started (pid {pid})\x1b[0m"),
                    Some(&out_path),
                ) {
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
        Command::TunnelCreate {
            destination,
            name,
            no_server_start,
            ssh_options,
            dry_run,
            foreground,
            ignore_version_mismatch,
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

            if !foreground && !dry_run {
                if let Err(e) = gritty::connect::preflight_ssh(&destination, &ssh_options) {
                    eprintln!("error: {e}");
                    std::process::exit(1);
                }
            }

            let ready_fd = if foreground || dry_run {
                None
            } else {
                let sock_display = local_sock.display().to_string();
                let conn_name = connection_name.clone();
                match daemonize(
                    move |_pid| {
                        println!("{sock_display}");
                        eprintln!("\x1b[32m\u{25b8} tunnel {conn_name} started\x1b[0m");
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

            let resolved = config.resolve_tunnel(&connection_name);

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
                ignore_version_mismatch,
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

async fn run(cli: Cli, config: gritty::config::ConfigFile) -> anyhow::Result<()> {
    match cli.command {
        Command::Server { .. } | Command::TunnelCreate { .. } => unreachable!(),
        Command::Connect {
            target,
            command,
            detach,
            no_create,
            no_redraw,
            no_escape,
            forward_agent,
            forward_open,
            no_forward_agent,
            no_forward_open,
            no_oauth_redirect,
            oauth_timeout,
            wait,
            force,
            pick,
            no_pick,
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
                forward_agent: if no_forward_agent {
                    false
                } else {
                    forward_agent || resolved.forward_agent
                },
                forward_open: if no_forward_open {
                    false
                } else {
                    forward_open || resolved.forward_open
                },
                oauth_redirect: if no_oauth_redirect { false } else { resolved.oauth_redirect },
                oauth_timeout: oauth_timeout.unwrap_or(resolved.oauth_timeout),
                heartbeat_interval: resolved.heartbeat_interval,
                heartbeat_timeout: resolved.heartbeat_timeout,
                ring_buffer_size: resolved.ring_buffer_size,
                oauth_tunnel_idle_timeout: resolved.oauth_tunnel_idle_timeout,
                client_name: resolved.client_name,
            };
            connect_session(
                session,
                command,
                detach,
                no_create,
                force,
                pick,
                no_pick,
                settings,
                ctl_path,
                auto_start_mode,
                wait,
            )
            .await
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
        Command::ListSessions { target } => {
            if target.is_none() && cli.ctl_socket.is_none() {
                list_all_sessions().await
            } else {
                let host = target.as_deref().map(|t| parse_target(t).0);
                let ctl_path = resolve_ctl_path(cli.ctl_socket, host.as_deref())?;
                list_sessions(ctl_path).await
            }
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
        Command::Rename { target, new_name } => {
            let (host, session) = parse_target(&target);
            let ctl_path = resolve_ctl_path(cli.ctl_socket, Some(&host))?;
            let session = match session {
                Some(s) => s,
                None => {
                    eprintln!("error: specify session as host:session (e.g. local:mysession)");
                    std::process::exit(1);
                }
            };
            rename_session(session, new_name, ctl_path).await
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
        Command::Send { session, stdin, timeout, no_timeout, mut files } => {
            let use_stdin = stdin || files.iter().any(|f| f.as_os_str() == "-");
            if use_stdin {
                files.retain(|f| f.as_os_str() != "-");
            }
            let timeout = if no_timeout { None } else { Some(timeout) };
            send_command(cli.ctl_socket, session, use_stdin, timeout, files).await
        }
        Command::Receive { session, stdout, timeout, no_timeout, dir } => {
            let use_stdout = stdout || dir.as_deref().is_some_and(|d| d.as_os_str() == "-");
            let dir = if use_stdout { None } else { dir };
            let timeout = if no_timeout { None } else { Some(timeout) };
            receive_command(cli.ctl_socket, session, use_stdout, timeout, dir).await
        }
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
        Command::Bootstrap { destination, install_dir, ssh_options } => {
            gritty::connect::bootstrap(&destination, &ssh_options, &install_dir).await
        }
        Command::TunnelDestroy { name } => gritty::connect::disconnect(&name).await,
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
