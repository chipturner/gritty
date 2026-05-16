# gritty usage

Complete command and flag reference. For an overview and quick start, see [README.md](README.md).

## Commands

| Command | Aliases | Description |
|---------|---------|-------------|
| `gritty connect [host[:name]]` | `c` | Smart session: attach if exists, create if not |
| `gritty list-sessions [host]` | `ls`, `list` | List sessions (no args = all servers; foreground process shown on Linux only) |
| `gritty tail [host:session]` | `t` | Read-only stream of session output |
| `gritty kill-session [host:session]` | | Kill a session |
| `gritty rename <host:session> <name>` | | Rename a session |
| `gritty kill-server [host]` | | Kill the server and all sessions (works across a protocol version mismatch) |
| `gritty restart [host]` | | Kill + restart server (and tunnel, for remote hosts). One-shot upgrade recovery |
| `gritty refresh [host]` | | Restart only processes running stale code (idempotent). No args = local + all tunnels |
| `gritty tunnels` | `tun` | List active SSH tunnels |
| `gritty tunnel-create <destination>` | | Set up SSH tunnel to remote host |
| `gritty tunnel-destroy <name>` | | Tear down an SSH tunnel |
| `gritty bootstrap <destination>` | | Install gritty on a remote host |
| `gritty local-forward <target> <port>` | `lf` | Forward a TCP port from session to client |
| `gritty remote-forward <target> <port>` | `rf` | Forward a TCP port from client to session |
| `gritty send [files...]` | | Send files to a paired receiver |
| `gritty receive [dir]` | | Receive files from a paired sender |
| `gritty open <url>` | | Open a URL on the local machine (for use inside gritty sessions) |
| `gritty copy` | | Copy stdin to the client clipboard (for use inside gritty sessions) |
| `gritty info` | | Show diagnostics (paths, server status, device id, tunnels) |
| `gritty config` | | Open config in `$VISUAL`/`$EDITOR`/vi (creates from template if missing) |
| `gritty doctor` | | Show key paths and check for common issues (stale processes, orphaned sockets, config errors) |
| `gritty server` | `s` | Start the server (backgrounds by default; `-f` for foreground) |
| `gritty completions <shell>` | | Generate shell completions (bash, zsh, fish, elvish, powershell) |
| `gritty socket-path` | `socket` | Print the default socket path |
| `gritty protocol-version` | | Print the protocol version number |

## Addressing sessions

A session is addressed as `host:session`.

**`host`** is a **connection name**. By default it is simply the SSH hostname -- the name you'd pass to `ssh` -- which is what nearly everyone uses:

```
laptop$ gritty connect devbox:work          # "devbox" = the host you ssh to
```

It is technically the name assigned by `gritty tunnel-create`, which defaults to the hostname but can be remapped with `-n`. Remapping is only useful when the SSH destination and the name you want to type differ:

```
laptop$ gritty tunnel-create user@10.0.0.5 -n devbox
laptop$ gritty connect devbox:work          # now "devbox" routes to user@10.0.0.5
```

`local` is a reserved connection name for a server on this machine (no SSH tunnel).

**`session`** is a name you choose so you can run several sessions per host. Rules:

- Omitted: `connect` attaches the sole detached session, shows a picker when the choice is ambiguous, and falls back to a session named `default` only when the server has no sessions.
- `-`: refers to the last-attached session, e.g. `gritty connect devbox:-`.
- Numeric-only names are rejected (they would collide with auto-assigned session IDs).

`connect` auto-starts the server and tunnel as needed. `send`/`receive` auto-detect the session across all active servers; use `--session host:session` to target a specific one.

## Options

### Global options

- `-v` / `--verbose`: enable debug logging
- `--ctl-socket <path>`: override the server socket path

### Session options (`connect`)

- `-A` / `--forward-agent`: forward your local SSH agent (off by default)
- `--no-forward-agent`: never forward the agent, even if `forward-agent = true` in config
- `-O` / `--forward-open`: forward URL opens to local machine (on by default; disable with `--no-forward-open`)
- `-c <cmd>` / `--command`: run a command instead of a login shell (when creating)
- `-d` / `--detach`: create session without attaching (background jobs)
- `--force`: take over an already-attached session without prompting
- `--pick`: always show session picker (interactive when in a terminal)
- `--no-pick`: never show session list; always target `default`
- `-n` / `--new`: skip the picker and create the next auto-named session (`default` or `session-N`)
- `--no-create`: attach only, error if session doesn't exist
- `--no-escape`: disable escape sequence processing
- `--no-oauth-redirect`: disable OAuth callback tunneling (part of `-O`)
- `--oauth-timeout <seconds>`: OAuth callback accept timeout (default: 180)
- `-w` / `--wait`: wait indefinitely for the server

### Tunnel options (`tunnel-create`)

- `-n <name>`: override connection name (defaults to hostname)
- `-o <option>` / `--ssh-option`: extra SSH options (repeatable, e.g., `-o "ProxyJump=bastion"`)
- `--no-server-start`: don't auto-start the remote server
- `--dry-run`: print SSH commands instead of running them
- `-f` / `--foreground`: run in the foreground instead of backgrounding
- `--ignore-version-mismatch`: connect even if the remote protocol version differs from local

### Send/receive options

- `--session host:session`: target a specific session
- `-r` / `--recursive` (`send`): send directories recursively (preserves structure, skips symlinks)
- `-` (`send`): read data from stdin; (`receive`): write data to stdout
- `--timeout <seconds>`: deadline for pairing with a receiver/sender (default 300; `--no-timeout` waits indefinitely)

File permissions are preserved. Directories can be sent with `-r`, or via tar for compression:

```
laptop$ gritty send -r mydir                 # recursive (preserves directory structure)
# or with tar for compression:
devbox$ tar czf - mydir | gritty send -
laptop$ gritty receive - | tar xzf -
```

## Session environment

**Set inside sessions:** `GRITTY_SOCK` (svc socket for `gritty open`/`send`/`receive`/port forwarding), `GRITTY_SESSION` (session ID), and `GRITTY_SESSION_NAME` (if named) are set in the shell environment. Useful for prompt customization or scripts that need to know which session they're in.

**Forwarded to sessions:** `TERM`, `COLORTERM`, `LANG`, and the `LC_*` locale categories are carried from the client to the session's login shell (mirroring SSH's default `SendEnv LANG LC_*`), so a remote session renders UTF-8 correctly even when the remote daemon's own environment lacks your locale.

## Port forwarding

`gritty lf <target> <port>` and `gritty rf <target> <port>` where `<target>` is a `host:session` specifier (e.g. `devbox:work`). Port spec is `PORT` (same on both ends) or `LISTEN_PORT:TARGET_PORT`.

Port forwards are client-initiated -- they communicate with the client process through a local forward socket, and the client sends `PortForwardRequest` frames to the server. A compromised server cannot initiate port forwards. Ctrl-C stops the forward. All forwarding binds to `127.0.0.1` only -- there is no bind-address option (unlike SSH's `-L`/`-R`).

These are transient, on-demand forwards -- great for quick checks during development. For always-on port forwarding, configure it on the SSH tunnel instead:

```
laptop$ gritty tunnel-create devbox -o "LocalForward=8080 localhost:8080"
```

or add it to `ssh-options` in your config file.

## Configuration

gritty works out of the box with no config file. Optionally, set persistent defaults in the config file: `~/.config/gritty/config.toml` on Linux (honors `$XDG_CONFIG_HOME`), `~/Library/Application Support/gritty/config.toml` on macOS. Run `gritty config` to create and open it at the right path, or `gritty info` to print it.

```toml
# Global defaults for all sessions/connections.
[defaults]
# forward-agent = false
# forward-open = true
# no-escape = false
# oauth-redirect = true
# oauth-timeout = 180
# heartbeat-interval = 10
# heartbeat-timeout = 60
# ring-buffer-size = 1048576
# oauth-tunnel-idle-timeout = 5
# client-name = "my-laptop"      # label shown in session lists; default: hostname

# Tunnel-specific global defaults (for tunnel-create).
[defaults.tunnel]
# ssh-options = []
# no-server-start = false
# isolate-control-path = true   # ControlPath=none on the tunnel ssh; set false to ride an existing ControlMaster mux
# connect-timeout = 30          # ssh -o ConnectTimeout (seconds); 0 = defer to ssh_config

# Per-host overrides, keyed by connection name.
# Connection name = hostname from destination, or -n override.
# A connection name with dots (an FQDN) must be quoted, or TOML reads the
# dots as table separators: [host."prod-db.example.com"].
[host.devbox.tunnel]
ssh-options = ["IdentityFile=~/.ssh/devbox_tunnel_key"]

[host.prod]
forward-agent = false
forward-open = false
no-escape = true

[host.prod.tunnel]
no-server-start = true
```

**Precedence:** CLI flag > `[host.<name>]` > `[defaults]` > built-in default. For `ssh-options`, values are appended (CLI first, then host, then defaults; SSH first-match gives earlier options priority).

`ring-buffer-size` and `oauth-tunnel-idle-timeout` are resolved by the daemon, which has no per-connection context -- they take effect only from `[defaults]`, not `[host.<name>]`.

A missing config file uses built-in defaults. A malformed config file (a typo'd key or section -- keys are kebab-case) is rejected in full and the built-in defaults are used; `gritty info` and `gritty doctor` both report it as invalid.

## Escape Sequences

After a newline (or at session start), `~` enters escape mode:

| Sequence | Action |
|----------|--------|
| `~.` | Detach from session (clean exit, no auto-reconnect) |
| `~R` | Force reconnect |
| `~#` | Session status, RTT, and server-side diagnostics |
| `~^Z` | Suspend the client (SIGTSTP) |
| `~?` | Print help |
| `~~` | Send a literal `~` |

Escape processing can be disabled with `--no-escape`.

## Shell Completions

```
# Bash
laptop$ gritty completions bash > /etc/bash_completion.d/gritty

# Zsh -- put in fpath and ensure compinit runs after:
laptop$ mkdir -p ~/.zfunc
laptop$ gritty completions zsh > ~/.zfunc/_gritty
# Add to .zshrc (before compinit):  fpath=(~/.zfunc $fpath)
# Then: rm -f ~/.zcompdump && exec zsh

# Fish
laptop$ gritty completions fish > ~/.config/fish/completions/gritty.fish
```

## Debugging

`gritty doctor` is the first stop: it prints the key paths (config file, socket dir, logs, device id) and checks for stale processes, orphaned sockets, and config errors. `gritty info` prints the same paths plus live server/tunnel status.

**Log levels:** `-v` enables debug logging. `RUST_LOG=gritty=trace` enables the most verbose output (protocol-level frame tracing, alt-screen state machine transitions). `RUST_LOG=gritty::server=debug,gritty=info` enables debug logging for the server module only.

**Runtime log-level adjustment:** Send SIGUSR1 to the daemon to cycle through log levels (info -> debug -> trace -> info) without restarting and losing sessions:

```
laptop$ kill -USR1 $(cat $(gritty socket-path | xargs dirname)/daemon.pid)
```

SIGUSR1 cycling is disabled when the daemon was started with `RUST_LOG` set -- that filter takes priority and is left untouched. `-v` passed to `connect`/`restart`/`refresh` is forwarded to any daemon or tunnel they auto-start.

**Log rotation:** Send SIGUSR2 to reopen the log file, compatible with external logrotate:

```
laptop$ kill -USR2 $(cat $(gritty socket-path | xargs dirname)/daemon.pid)
```

**In-session diagnostics:** Press `~#` during a session to see both client-side status (RTT, uptime, bytes relayed) and server-side diagnostics (output history state + stream offset, alt-screen mode, channel counts, shell PID).

**Log file locations:** `gritty doctor` and `gritty info` both show the paths to `daemon.log` and `daemon.out`. `doctor` also warns if a log grows past 50 MB.

**Protocol version mismatch after upgrade:** after upgrading gritty on one side the other side's daemon still speaks the old protocol, so session operations fail with `protocol version mismatch`. Upgrade both binaries to matching versions (`gritty protocol-version` prints the local version), then run `gritty refresh` to restart whatever is stale -- the local daemon, each tunnel supervisor, and (over SSH) each remote daemon. `refresh` is idempotent: it reads each process's `.info` sidecar and only restarts what is actually behind the binary on disk, so a second run is a no-op. `gritty restart <host>` is the scorched-earth variant that restarts unconditionally; both tolerate the mismatched handshake so neither falls back to raw SSH. `gritty doctor` shows which processes are stale without touching anything.

See [docs/tunnel-state-machine.md](docs/tunnel-state-machine.md) for the `healthy` / `reconnecting` / `stale` tunnel state definitions and the full supervisor state diagram.
