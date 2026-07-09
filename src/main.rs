mod commands;

use clap::{CommandFactory, Parser, Subcommand};
use commands::*;
use std::os::fd::{AsRawFd, OwnedFd};
use std::path::{Path, PathBuf};

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
  kill-session (kill)    Kill one or more sessions
  prune                  Bulk-kill stale detached sessions (dry-run unless -y)
  rename                 Rename a session
  kill-server            Kill the server and all sessions
  restart                Kill + restart the server (upgrade recovery)
  refresh                Restart only stale processes (idempotent)

Tunnels:
  tunnels (tun)          List active SSH tunnels
  tunnel-create          Set up SSH tunnel to a remote host
  tunnel-destroy         Tear down an SSH tunnel
  bootstrap              Install gritty on a remote host

Forwarding & transfer:
  local-forward (lf)     Expose a local port inside the session (like ssh -R)
  remote-forward (rf)    Bring a session port to this machine (like ssh -L)
  send                   Send files to a paired receiver
  receive                Receive files from a paired sender

In-session (run inside a gritty session):
  open                   Open a URL on the local machine
  copy                   Copy stdin to the client clipboard

Configuration:
  info                   Show diagnostics (paths, server, tunnels)
  config                 Open config in $VISUAL/$EDITOR/vi
  doctor                 Check for common issues

Plumbing:
  server (s)             Start the server
  completions            Generate shell completions
  mangen                 Generate man pages
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
    #[command(
        display_order = 0,
        visible_alias = "c",
        after_help = "Flag defaults come from config, with precedence CLI > [host.<name>] > \
                      [defaults] > built-in. Enable flags for on-by-default features (-O) exist \
                      to override a config-file `false` for one invocation; --no-* flags win \
                      over everything."
    )]
    Connect {
        /// Target host, with optional session name (host or host:name);
        /// host defaults to `local` when omitted
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

        /// Disable escape sequences (~. detach, ~? help, etc.)
        #[arg(long)]
        no_escape: bool,

        /// Forward local SSH agent to the session
        #[arg(short = 'A', long)]
        forward_agent: bool,

        /// Forward URL opens to the local machine (default: on; overrides
        /// a config-file `forward-open = false`)
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

        /// Never show session picker; always target session `0`
        #[arg(long, conflicts_with = "pick")]
        no_pick: bool,

        /// Skip the picker and create a new session in the next free
        /// integer slot in your namespace (`0` if unused, else the lowest
        /// free `N`).
        #[arg(short = 'n', long = "new", conflicts_with_all = ["pick", "no_pick"])]
        new_session: bool,

        /// How long the session survives with no client attached before
        /// the server reaps it (e.g. `30m`, `1h`, `never`). Overrides the
        /// `linger` / `linger-unnamed` config.
        #[arg(long, value_name = "DURATION")]
        linger: Option<String>,
    },
    /// Tail a session's output (read-only, like tail -f)
    #[command(display_order = 2, visible_alias = "t")]
    Tail {
        /// Target host and session (host:session); host defaults to `local`
        target: Option<String>,
    },
    /// List active sessions (no host = all known hosts: local + tunnels)
    #[command(display_order = 1, visible_alias = "ls", visible_alias = "list")]
    ListSessions {
        /// Target host (omit to list every known host)
        target: Option<String>,

        /// Machine-readable output (array of host groups with sessions)
        #[arg(long)]
        json: bool,
    },
    /// Kill one or more sessions
    #[command(display_order = 3, visible_alias = "kill")]
    KillSession {
        /// Sessions to kill: `host:session`, or a bare name/ID resolved on
        /// `local` (IDs and names both work; bare known host names list that
        /// host's sessions instead)
        targets: Vec<String>,
    },
    /// Bulk-kill stale detached sessions (dry-run unless -y)
    // The filter group is validated by hand (`ensure_prune_filter`) instead of
    // `required(true)` so a bare `gritty prune` gets a steering error rather
    // than clap's generic required-group message.
    #[command(display_order = 4, group = clap::ArgGroup::new("prune_filter").multiple(true))]
    Prune {
        /// Target host (defaults to `local`)
        target: Option<String>,

        /// Only sessions created by this client (the `name/` prefix shown in
        /// `gritty ls`). Repeatable.
        #[arg(long = "client", value_name = "NAME", group = "prune_filter")]
        clients: Vec<String>,

        /// Only sessions with no terminal activity for at least this long
        /// (e.g. 90s, 30m, 12h, 7d)
        #[arg(long, value_name = "DURATION", group = "prune_filter")]
        idle: Option<String>,

        /// Select every detached session
        #[arg(long, group = "prune_filter", conflicts_with_all = ["clients", "idle"])]
        all: bool,

        /// Pick victims interactively (TUI): space marks, enter kills the
        /// marked set after a y/n confirm. `--client`/`--idle` narrow the
        /// candidate list
        #[arg(long, group = "prune_filter", conflicts_with_all = ["all", "yes"])]
        pick: bool,

        /// Actually kill the selection (without this, prune only prints it)
        #[arg(short = 'y', long)]
        yes: bool,
    },
    /// Kill the server and all sessions
    #[command(display_order = 6)]
    KillServer {
        /// Target host (defaults to `local`)
        target: Option<String>,
    },
    /// Restart the server (and tunnel, for remote hosts). One-shot recovery
    /// for protocol version upgrades: kills the daemon (tolerant of a
    /// mismatched handshake), tears down the tunnel, and starts both back up.
    #[command(display_order = 7)]
    Restart {
        /// Target host (defaults to `local`)
        target: Option<String>,
    },
    /// Restart only the processes running stale code (daemon, tunnel
    /// supervisor, remote daemon). Idempotent: a second run is a no-op.
    /// Use after upgrading the gritty binary to pick it up everywhere
    /// without the scorched-earth `restart`.
    #[command(display_order = 8)]
    Refresh {
        /// Target host (defaults to `local` plus all active tunnels)
        target: Option<String>,
    },
    /// Rename a session
    #[command(display_order = 5)]
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

        /// Send directories recursively
        #[arg(short, long)]
        recursive: bool,

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

        /// Destination directory (default: current directory, - for stdout;
        /// stdout is implied when omitted and stdout is redirected)
        dir: Option<PathBuf>,
    },
    /// Open a URL on the local machine (for use inside gritty sessions)
    #[command(display_order = 34)]
    Open {
        /// URL to open
        url: String,
    },
    /// Copy stdin to the client clipboard (for use inside gritty sessions)
    #[command(display_order = 35)]
    Copy,
    /// Make a local (client-side) port reachable inside the session (like ssh -R)
    ///
    /// Named for where the service lives: `gritty lf 5432` lets processes
    /// in the session reach the postgres on your local machine. Listens in
    /// the session, connects on the client.
    #[command(display_order = 30, visible_alias = "lf")]
    LocalForward {
        /// Target session (host[:session]); omit to use the only attached session
        target: Option<String>,
        /// Port spec: PORT or LISTEN_PORT:TARGET_PORT
        port: Option<String>,
    },
    /// Bring a remote (session-side) port to the client (like ssh -L)
    ///
    /// Named for where the service lives: `gritty rf 3000` lets you browse
    /// the session's :3000 at localhost:3000 -- the common dev-server case.
    /// Listens on the client, connects in the session.
    #[command(display_order = 31, visible_alias = "rf")]
    RemoteForward {
        /// Target session (host[:session]); omit to use the only attached session
        target: Option<String>,
        /// Port spec: PORT or LISTEN_PORT:TARGET_PORT
        port: Option<String>,
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
    Tunnels {
        /// Machine-readable output
        #[arg(long)]
        json: bool,
    },
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
    Info {
        /// Machine-readable output
        #[arg(long)]
        json: bool,
    },
    /// Open config file in $VISUAL/$EDITOR/vi (creates from template if missing)
    #[command(display_order = 21)]
    Config,
    /// Check for common issues (stale processes, orphaned sockets, config errors)
    #[command(display_order = 22)]
    Doctor {
        /// Remove socket-dir files this gritty version doesn't recognize
        #[arg(long)]
        clean: bool,

        /// Machine-readable output (paths, check groups, summary)
        #[arg(long)]
        json: bool,

        /// Print an LLM-ready diagnostic report (primer, checks, state, log
        /// excerpts) to paste into a chat or pipe into an LLM CLI; gritty
        /// never calls an LLM itself. Optionally describe the problem.
        #[arg(
            long,
            value_name = "DESCRIPTION",
            num_args = 0..=1,
            default_missing_value = "",
            conflicts_with_all = ["json", "clean"],
        )]
        llm: Option<String>,

        /// Tail length per log excerpt in the --llm report
        #[arg(long, value_name = "N", requires = "llm")]
        log_lines: Option<usize>,
    },
    /// Generate shell completions
    #[command(display_order = 41)]
    Completions {
        /// Shell to generate completions for
        shell: clap_complete::Shell,
    },
    /// Generate man pages (one per subcommand, for packagers)
    #[command(display_order = 44)]
    Mangen {
        /// Directory to write the man pages into (created if missing)
        dir: PathBuf,
    },
    // -- Internal/plumbing --
    /// Print the default socket path
    #[command(display_order = 42, visible_alias = "socket")]
    SocketPath,
    /// Print the protocol version number
    #[command(display_order = 43)]
    ProtocolVersion,
}

fn init_tracing(verbose: bool, log_path: Option<&Path>, stderr_default: &'static str) {
    gritty::logging::init_tracing(verbose, log_path, stderr_default);
    let argv: Vec<String> = std::env::args().collect();
    // Audit line for log files (which command/pid started this process, which
    // client invocations attached over a tunnel). Filtered out on an
    // interactive terminal by the client's `gritty=warn` stderr default.
    tracing::info!(cmd = %argv.join(" "), pid = std::process::id(), "gritty invoked");
}

/// For long-lived client commands against a tunnel host, route tracing to
/// that tunnel's log file so client-side reconnect/link-down events land
/// alongside the supervisor's entries instead of spraying stderr into a
/// raw-mode terminal. One-shot commands, `local`, and explicit
/// `--ctl-socket` keep stderr.
fn client_log_path(
    config: &gritty::config::ConfigFile,
    cmd: &Command,
    ctl_socket_override: bool,
) -> Option<PathBuf> {
    if ctl_socket_override {
        return None;
    }
    let target = match cmd {
        Command::Connect { target, .. } | Command::Tail { target } => target.as_deref()?,
        _ => return None,
    };
    // Quiet alias resolution: run() canonicalizes the same target right after
    // and owns any ambiguity warning.
    let (host, _) = split_target(target);
    let host = config.canonical_host_quiet(&host);
    if host == "local" {
        return None;
    }
    Some(gritty::connect::connect_log_path(&host))
}

/// Apply the client-name prefix rule to the session part of a `host:session`
/// target string, leaving the host part untouched. Used by `send` / `receive`
/// which take an opaque `--session host:session` string and pass it down to
/// the transfer helpers (which don't know about config).
fn resolve_target_session(
    config: &gritty::config::ConfigFile,
    target: Option<String>,
) -> Option<String> {
    let target = target?;
    let (host, session) = parse_target(config, &target);
    let session = session?;
    let client_name = config.resolve_session(Some(&host)).client_name;
    let wire = gritty::naming::resolve_session_name(&session, &client_name);
    Some(format!("{host}:{wire}"))
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
                    // Grandchild: not a session leader. Become our own process-group
                    // leader so `killpg(our_pid)` (e.g. from `tunnel-destroy`) reaches
                    // us and any children we spawn.
                    let _ = nix::unistd::setpgid(
                        nix::unistd::Pid::from_raw(0),
                        nix::unistd::Pid::from_raw(0),
                    );
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
    let msg = format!("error: {e:#}");
    if let Some(fd) = error_pipe {
        let _ = nix::unistd::write(fd, msg.as_bytes());
    }
    eprintln!("{msg}");
    std::process::exit(1);
}

fn main() {
    // When invoked as "gritty-open" (symlink), dispatch directly to open.
    if let Some(prog) = std::env::args().next()
        && Path::new(&prog).file_name().and_then(|f| f.to_str()) == Some("gritty-open")
    {
        let url = match std::env::args().nth(1) {
            Some(u) => u,
            None => {
                eprintln!("usage: gritty-open <url>");
                std::process::exit(1);
            }
        };
        open_url(&url, true);
        return;
    }

    let cli = Cli::parse();
    let verbose = cli.verbose;
    // Record verbosity so auto_start can forward --verbose to daemons it spawns.
    set_verbose(verbose);
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

            // Init tracing AFTER fork: file in daemon mode, stderr in foreground.
            // Foreground is a diagnostic mode -- keep info on stderr.
            init_tracing(
                verbose,
                if ready_fd.is_some() { Some(&log_path) } else { None },
                "gritty=info",
            );

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
                        eprintln!("error: {e:#}");
                        std::process::exit(1);
                    }
                },
            };
            // Canonicalize through `[host.*] aliases` so `tunnel-create
            // FOO.BAR.COM` lands on the same connection name (and config
            // section) as `connect FOO`.
            let connection_name = config.canonical_host(&connection_name);
            if connection_name == "local" {
                eprintln!(
                    "error: 'local' is reserved for the local server; \
                     use 'localhost.' to SSH to this machine"
                );
                std::process::exit(1);
            }
            let local_sock = gritty::connect::connection_socket_path(&connection_name);
            let resolved = config.resolve_tunnel(&connection_name);

            let socket_dir = gritty::daemon::socket_dir();
            if let Err(e) = gritty::security::secure_create_dir_all(&socket_dir) {
                eprintln!("error: {e}");
                std::process::exit(1);
            }
            let out_path = gritty::connect::connect_out_path(&connection_name);
            let log_path = gritty::connect::connect_log_path(&connection_name);

            // Merge CLI -o options with config-layer options so preflight
            // sees the same SSH option set the real tunnel will use.
            // Running preflight against only the CLI options produced
            // false-negative "cannot connect non-interactively" when the
            // config supplied IdentityFile / ProxyJump / etc.
            let merged_ssh_options =
                gritty::connect::merge_ssh_options(&ssh_options, &resolved.ssh_options);

            if !foreground
                && !dry_run
                && let Err(e) = gritty::connect::preflight_ssh(
                    &destination,
                    &merged_ssh_options,
                    resolved.connect_timeout,
                )
            {
                eprintln!("error: {e:#}");
                std::process::exit(1);
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

            // Foreground is a diagnostic mode -- keep info on stderr.
            init_tracing(
                verbose,
                if ready_fd.is_some() { Some(&log_path) } else { None },
                "gritty=info",
            );

            let rt = match tokio::runtime::Runtime::new() {
                Ok(rt) => rt,
                Err(e) => {
                    eprintln!("error: {e}");
                    std::process::exit(1);
                }
            };

            let opts = gritty::connect::ConnectOpts {
                destination,
                no_server_start: no_server_start || resolved.no_server_start,
                ssh_options: merged_ssh_options,
                // Persist only the pre-merge CLI -o options: tunnel-create
                // re-resolves config ssh-options on replay, so storing the
                // merged set would double them.
                cli_ssh_options: ssh_options,
                name,
                dry_run,
                foreground,
                ignore_version_mismatch,
                isolate_control_path: resolved.isolate_control_path,
                connect_timeout: resolved.connect_timeout,
            };

            let error_pipe = dup_ready_fd(&ready_fd);
            match rt.block_on(gritty::connect::run(opts, ready_fd)) {
                Ok(code) => std::process::exit(code),
                Err(e) => report_error(&error_pipe, &e),
            }
        }
        _ => {
            let log_path = client_log_path(&config, &cli.command, cli.ctl_socket.is_some());
            // Client commands log telemetry, not UI: keep stderr quiet (warn)
            // so e.g. `gritty ls` doesn't print its own invocation audit line.
            init_tracing(verbose, log_path.as_deref(), "gritty=warn");
            let rt = match tokio::runtime::Runtime::new() {
                Ok(rt) => rt,
                Err(e) => {
                    eprintln!("error: {e}");
                    std::process::exit(1);
                }
            };
            if let Err(e) = rt.block_on(run(cli, config)) {
                // `{e:#}` renders anyhow's full cause chain, matching
                // `report_error`. Plain `{e}` drops every `.context()` the
                // command layer attached.
                eprintln!("error: {e:#}");
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
            new_session,
            linger,
        } => {
            let (host, session) = split_optional_target(&config, target.as_deref());
            let auto_start_mode = match (&cli.ctl_socket, host.as_str()) {
                (Some(_), _) => AutoStart::None,
                (None, "local") => AutoStart::Server,
                (None, h) => AutoStart::Tunnel {
                    name: h.to_string(),
                    config_dest: config.alias_destination(h),
                },
            };
            let ctl_path = resolve_ctl_path(cli.ctl_socket, Some(&host))?;
            let resolved = config.resolve_session(Some(&host));
            let settings = gritty::config::SessionSettings {
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
                linger: resolved.linger,
                linger_unnamed: resolved.linger_unnamed,
            };
            // Resolve the linger duration to send in NewSession. `--linger`
            // wins; otherwise an omitted session name (`host` -> auto-slot)
            // uses `linger-unnamed`, and a typed name (`host:foo`) uses
            // `linger`. The server just stores and enforces the result.
            let linger_from_cli = linger.is_some();
            let linger_secs = match linger.as_deref().map(gritty::config::parse_linger) {
                Some(Ok(secs)) => secs,
                Some(Err(e)) => anyhow::bail!("--linger: {e}"),
                None if session.is_none() => settings.linger_unnamed,
                None => settings.linger,
            };
            // Prefix the user-supplied session into this client's namespace
            // (e.g. `work` -> `mylaptop/work`). Names containing `/` pass
            // through literally -- the foreign-access / shared-session form.
            let session =
                session.map(|s| gritty::naming::resolve_session_name(&s, &settings.client_name));
            connect_session(
                session,
                command,
                ConnectFlags {
                    detach,
                    no_create,
                    force,
                    pick,
                    no_pick,
                    new_session,
                    wait,
                    linger_secs,
                    linger_from_cli,
                },
                settings,
                ctl_path,
                auto_start_mode,
            )
            .await
        }
        Command::Tail { target } => {
            let (host, session) = split_optional_target(&config, target.as_deref());
            let ctl_path = resolve_ctl_path(cli.ctl_socket, Some(&host))?;
            let client_name = config.resolve_session(Some(&host)).client_name;
            let session = match session {
                Some(s) => gritty::naming::resolve_session_name(&s, &client_name),
                None => {
                    suggest_session("tail", &host, &ctl_path, &client_name).await?;
                    unreachable!()
                }
            };
            let code = tail_session(session, ctl_path).await?;
            std::process::exit(code);
        }
        Command::ListSessions { target, json } => {
            if target.is_none() && cli.ctl_socket.is_none() {
                list_all_sessions(&config, json).await
            } else {
                let host = target.as_deref().map(|t| parse_target(&config, t).0);
                let ctl_path = resolve_ctl_path(cli.ctl_socket, host.as_deref())?;
                let client_name = config.resolve_session(host.as_deref()).client_name;
                list_sessions(ctl_path, host.as_deref().unwrap_or("local"), &client_name, json)
                    .await
            }
        }
        Command::KillSession { targets } => {
            if targets.is_empty() {
                // No targets: same as the old no-target form -- needs a host
                // (or --ctl-socket), then lists its sessions to pick from.
                let ctl_path = resolve_ctl_path(cli.ctl_socket, None)?;
                let client_name = config.resolve_session(None).client_name;
                suggest_session("kill-session", "host", &ctl_path, &client_name).await?;
                unreachable!()
            }
            kill_sessions(&targets, cli.ctl_socket.as_deref(), &config).await
        }
        Command::Prune { target, clients, idle, all, pick, yes } => {
            ensure_prune_filter(&clients, idle.as_deref(), all, pick)?;
            let host = parse_host_or_local(&config, target.as_deref());
            let ctl_path = resolve_ctl_path(cli.ctl_socket, Some(&host))?;
            let client_name = config.resolve_session(Some(&host)).client_name;
            prune_sessions(ctl_path, &client_name, &clients, idle.as_deref(), all, pick, yes).await
        }
        Command::Rename { target, new_name } => {
            let (host, session) = parse_target(&config, &target);
            let ctl_path = resolve_ctl_path(cli.ctl_socket, Some(&host))?;
            let client_name = config.resolve_session(Some(&host)).client_name;
            let session = match session {
                Some(s) => gritty::naming::resolve_session_name(&s, &client_name),
                None => {
                    eprintln!("error: specify session as host:session (e.g. local:mysession)");
                    std::process::exit(1);
                }
            };
            let new_name = gritty::naming::resolve_session_name(&new_name, &client_name);
            rename_session(session, new_name, ctl_path).await
        }
        Command::KillServer { target } => {
            let host = parse_host_or_local(&config, target.as_deref());
            let ctl_path = resolve_ctl_path(cli.ctl_socket, Some(&host))?;
            let kill_result = kill_server(ctl_path).await;
            if host == "local" {
                kill_result
            } else {
                // Always tear down the tunnel supervisor: if the remote
                // daemon is already down it would otherwise reconnect
                // forever. A kill_server error here usually just means the
                // remote daemon was already gone -- downgrade it to a
                // warning and let the tunnel teardown decide the outcome.
                if let Err(e) = &kill_result {
                    eprintln!("\x1b[2;33m\u{25b8} {host}: {e}\x1b[0m");
                }
                gritty::connect::disconnect(&host).await
            }
        }
        Command::Restart { target } => {
            let host = target.as_deref().map(|t| parse_target(&config, t).0);
            restart(host, cli.ctl_socket, &config).await
        }
        Command::Refresh { target } => {
            let host = target.as_deref().map(|t| parse_target(&config, t).0);
            refresh(host, cli.ctl_socket, &config).await
        }
        Command::SocketPath => {
            let ctl_path = cli.ctl_socket.unwrap_or_else(gritty::daemon::control_socket_path);
            println!("{}", ctl_path.display());
            Ok(())
        }
        Command::Send { session, stdin, timeout, no_timeout, recursive, mut files } => {
            let use_stdin = stdin || files.iter().any(|f| f.as_os_str() == "-");
            if use_stdin {
                files.retain(|f| f.as_os_str() != "-");
            }
            let timeout = if no_timeout { None } else { Some(timeout) };
            let session = resolve_target_session(&config, session);
            send_command(cli.ctl_socket, session, use_stdin, timeout, recursive, files).await
        }
        Command::Receive { session, stdout, timeout, no_timeout, dir } => {
            use std::io::IsTerminal;
            let (use_stdout, dir, auto) =
                resolve_receive_output(stdout, dir, std::io::stdout().is_terminal());
            if auto {
                eprintln!(
                    "\x1b[2;33m\u{25b8} stdout is redirected; streaming data to it (pass a directory to receive files instead)\x1b[0m"
                );
            }
            let timeout = if no_timeout { None } else { Some(timeout) };
            let session = resolve_target_session(&config, session);
            receive_command(cli.ctl_socket, session, use_stdout, timeout, dir).await
        }
        Command::Open { url } => {
            open_url(&url, false);
            Ok(())
        }
        Command::Copy => {
            clipboard_copy();
            Ok(())
        }
        Command::LocalForward { target, port } => {
            port_forward_command(&config, cli.ctl_socket, target, port, 0).await
        }
        Command::RemoteForward { target, port } => {
            port_forward_command(&config, cli.ctl_socket, target, port, 1).await
        }
        Command::Bootstrap { destination, install_dir, ssh_options } => {
            // Canonicalize so a `[host.FOO]` section (ssh-options etc.)
            // applies when bootstrapping via an alias destination.
            let host = config.canonical_host(&gritty::connect::parse_host(&destination)?);
            let resolved = config.resolve_tunnel(&host);
            // Merge configured ssh-options with the CLI ones (CLI first, SSH
            // first-match wins) -- every other SSH path does this, so a host
            // reachable only via a configured ProxyJump/IdentityFile/Port
            // must work for `bootstrap` too.
            let merged_ssh_options =
                gritty::connect::merge_ssh_options(&ssh_options, &resolved.ssh_options);
            gritty::connect::bootstrap(
                &destination,
                &merged_ssh_options,
                &install_dir,
                resolved.connect_timeout,
            )
            .await
        }
        Command::TunnelDestroy { name } => gritty::connect::disconnect(&name).await,
        Command::Tunnels { json } => {
            gritty::connect::list_tunnels(json);
            Ok(())
        }
        Command::Info { json } => info(cli.ctl_socket, json).await,
        Command::Config => config_edit(),
        Command::Doctor { clean, json, llm, log_lines } => match llm {
            Some(description) => {
                llm_report(
                    cli.ctl_socket.as_deref(),
                    &description,
                    log_lines.unwrap_or(DEFAULT_TAIL_LINES),
                )
                .await
            }
            None => doctor(cli.ctl_socket, clean, json).await,
        },
        Command::ProtocolVersion => {
            println!("{}", gritty::protocol::PROTOCOL_VERSION);
            Ok(())
        }
        Command::Completions { shell } => {
            clap_complete::generate(shell, &mut Cli::command(), "gritty", &mut std::io::stdout());
            Ok(())
        }
        Command::Mangen { dir } => {
            std::fs::create_dir_all(&dir)?;
            clap_mangen::generate_to(Cli::command(), &dir)?;
            println!("man pages written to {}", dir.display());
            Ok(())
        }
    }
}

/// A bare `gritty prune` selects nothing rather than defaulting to
/// everything; require an explicit filter and steer toward the choices.
fn ensure_prune_filter(
    clients: &[String],
    idle: Option<&str>,
    all: bool,
    pick: bool,
) -> anyhow::Result<()> {
    if all || pick || idle.is_some() || !clients.is_empty() {
        return Ok(());
    }
    anyhow::bail!(
        "prune needs a filter: --all (every detached session), --idle <duration> \
         (e.g. --idle 2h), --client <name>, or --pick (choose interactively). \
         Dry-run by default; add -y to actually kill."
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn clap_config_is_valid() {
        Cli::command().debug_assert();
    }

    // A bare `gritty prune` must parse (no clap required-group error) so the
    // hand-written steering error in `ensure_prune_filter` is what users see.
    #[test]
    fn prune_without_filters_parses_then_fails_validation() {
        let cli = Cli::try_parse_from(["gritty", "prune"]).expect("bare prune should parse");
        let Command::Prune { clients, idle, all, pick, .. } = cli.command else {
            panic!("expected Prune");
        };
        let err = ensure_prune_filter(&clients, idle.as_deref(), all, pick)
            .expect_err("no filter must be rejected");
        for hint in ["--all", "--idle", "--client", "--pick"] {
            assert!(err.to_string().contains(hint), "error should mention {hint}");
        }
    }

    #[test]
    fn prune_filters_pass_validation_and_combine() {
        // Each filter alone satisfies the check.
        assert!(ensure_prune_filter(&[], None, true, false).is_ok());
        assert!(ensure_prune_filter(&[], None, false, true).is_ok());
        assert!(ensure_prune_filter(&[], Some("2h"), false, false).is_ok());
        assert!(ensure_prune_filter(&["laptop".into()], None, false, false).is_ok());
        // --client and --idle still combine at the clap level.
        assert!(Cli::try_parse_from(["gritty", "prune", "--client", "x", "--idle", "2h"]).is_ok());
        // --all and --pick still conflict at the clap level.
        assert!(Cli::try_parse_from(["gritty", "prune", "--all", "--pick"]).is_err());
    }

    // The top-level `gritty --help` uses a hand-written help_template with no
    // {subcommands} placeholder, so it can silently drift from the Command
    // enum. Guard it: every subcommand must be named in the rendered help.
    #[test]
    fn help_template_lists_every_subcommand() {
        let help = Cli::command().render_help().to_string();
        for sub in Cli::command().get_subcommands() {
            let name = sub.get_name();
            assert!(
                help.contains(name),
                "top-level --help omits subcommand `{name}` -- help_template has \
                 drifted from the Command enum"
            );
        }
    }
}
