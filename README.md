# gritty

[![CI](https://github.com/chipturner/gritty/actions/workflows/ci.yml/badge.svg)](https://github.com/chipturner/gritty/actions/workflows/ci.yml)
[![Crates.io](https://img.shields.io/crates/v/gritty-cli)](https://crates.io/crates/gritty-cli)
[![License: MIT OR Apache-2.0](https://img.shields.io/crates/l/gritty-cli)](LICENSE)

Persistent remote shells that bring your local tools with them.

### The problem

You SSH into a devbox and run `gh auth login`. It prints a URL. You copy it, paste it into your laptop browser, complete the flow... and the callback goes to `localhost:38291` *on your laptop*, not the remote box. It hangs.

Same story for `gcloud auth login`, `aws sso login`, anything OAuth.

### With gritty

```bash
gritty connect devbox:work
gh auth login                       # browser opens locally, callback tunnels back. Done.
```

Close your laptop, change wifi, open it back up -- `gritty connect devbox:work` and you're exactly where you left off. Agent forwarding, URL forwarding, and OAuth tunneling all work out of the box.

It works by forwarding Unix domain sockets over SSH -- no custom protocol, no open ports, no certificates, no configuration. If you can `ssh` to a host, you can use gritty.

### Install

**Prebuilt binaries** (Linux x86_64/ARM64, macOS x86_64/ARM64):

```bash
# Download the install script, review it, then run:
curl -sSfL https://raw.githubusercontent.com/chipturner/gritty/main/install.sh -o install.sh
less install.sh
sh install.sh

# Or via cargo-binstall:
cargo binstall gritty-cli
```

**From source:**

```bash
cargo install gritty-cli   # binary name: gritty, requires Rust 1.85+
```

Install on **both your laptop and the remote host**.

### Quick Start

Make sure you can `ssh devbox` first (to accept the host key / enter your password), then:

```bash
gritty connect devbox:work
```

That's it. gritty auto-starts the SSH tunnel and remote server. Agent forwarding and URL/OAuth forwarding are on by default. If the session already exists, `connect` reattaches to it.

Transfer files through the session (run one side locally, one remotely):

```bash
gritty send file1.txt file2.txt     # auto-detects which session to use
gritty receive /tmp/dest

command | gritty send --stdin        # pipe mode
gritty receive --stdout | command
```

Detach and reattach from anywhere:

```bash
# Detach with ~. or just close your terminal

gritty connect devbox:work          # reattach from any terminal, any machine
gritty ls devbox                    # list sessions
gritty tunnels                      # list active tunnels
```

Local-only sessions (`gritty connect local:scratch`) are available for testing but aren't the typical workflow.

## Features

- **Self-healing connections** -- heartbeat detection, automatic tunnel respawn, transparent reconnect
- **Persistent sessions** -- shells survive disconnect, network failure, laptop sleep; `connect` reattaches or creates; multiple named sessions
- **SSH agent forwarding** -- `git push`, `ssh`, and other agent-dependent commands work remotely using your local keys (on by default); survives reconnects without stale sockets
- **URL open forwarding** -- `$BROWSER` requests forwarded to your local machine, with automatic OAuth callback tunneling (on by default)
- **Port forwarding** -- `gritty lf 8080` to quick-check a remote web server locally, `gritty rf 5432` to let the session reach local postgres
- **File transfer** -- `gritty send` / `gritty receive` through the session connection, preserving permissions; pipe mode with `--stdin`/`--stdout` for composing with `tar` etc.
- **Single binary, no network protocol** -- Unix domain sockets locally, SSH handles encryption and auth; optional TOML config for per-host defaults

## Commands

| Command | Aliases | Description |
|---------|---------|-------------|
| `gritty connect [host[:name]]` | `c` | Smart session: attach if exists, create if not |
| `gritty list-sessions [host]` | `ls`, `list` | List sessions (no args = all servers; foreground process shown on Linux only) |
| `gritty tail [host:session]` | `t` | Read-only stream of session output |
| `gritty kill-session [host:session]` | | Kill a session |
| `gritty rename <host:session> <name>` | | Rename a session |
| `gritty kill-server [host]` | | Kill the server and all sessions |
| `gritty tunnels` | `tun` | List active SSH tunnels |
| `gritty tunnel-create <destination>` | | Set up SSH tunnel to remote host |
| `gritty tunnel-destroy <name>` | | Tear down an SSH tunnel |
| `gritty local-forward <port>` | `lf` | Forward a TCP port from session to client |
| `gritty remote-forward <port>` | `rf` | Forward a TCP port from client to session |
| `gritty send [files...]` | | Send files to a paired receiver |
| `gritty receive [dir]` | | Receive files from a paired sender |
| `gritty open <url>` | | Open a URL on the local machine (for use inside gritty sessions) |
| `gritty info` | | Show diagnostics (paths, server status, tunnels) |
| `gritty config-edit` | | Open config in `$VISUAL`/`$EDITOR`/vi (creates from template if missing) |
| `gritty server` | `s` | Start the server (backgrounds by default; `-f` for foreground) |
| `gritty completions <shell>` | | Generate shell completions (bash, zsh, fish, elvish, powershell) |
| `gritty socket-path` | `socket` | Print the default socket path |
| `gritty protocol-version` | | Print the protocol version number |

The `<host>` in `host:session` is a **connection name**, not an SSH destination. It's the name assigned by `gritty tunnel-create` -- by default the hostname, overridable with `-n`. `local` is the reserved name for the local server. For example, `gritty tunnel-create user@mybox.example.com -n devbox` creates connection name `devbox`, so you'd use `gritty connect devbox:work`. If the session name is omitted, it defaults to `default`. The special session name `-` refers to the last-attached session (e.g. `gritty connect devbox:-`). `connect` auto-starts server/tunnel as needed. `send`/`receive` auto-detect the session across all active servers; use `--session host:session` to target a specific one.

**Global options:**
- `-v` / `--verbose`: enable debug logging
- `--ctl-socket <path>`: override the server socket path

**Session options** (`connect`):
- `-A` / `--forward-agent`: forward your local SSH agent (on by default; disable with `--no-forward-agent`)
- `-O` / `--forward-open`: forward URL opens to local machine (on by default; disable with `--no-forward-open`)
- `-c <cmd>` / `--command`: run a command instead of a login shell (when creating)
- `-d` / `--detach`: create session without attaching (background jobs)
- `--force`: take over an already-attached session without prompting
- `--pick`: always show session picker (interactive when in a terminal)
- `--no-pick`: never show session list; always target `default`
- `--no-create`: attach only, error if session doesn't exist
- `--no-redraw`: don't send Ctrl-L after connecting
- `--no-escape`: disable escape sequence processing
- `--no-oauth-redirect`: disable OAuth callback tunneling (part of `-O`)
- `--oauth-timeout <seconds>`: OAuth callback accept timeout (default: 180)
- `-w` / `--wait`: wait indefinitely for the server

**Tunnel options** (`tunnel-create`):
- `-n <name>`: override connection name (defaults to hostname)
- `-o <option>` / `--ssh-option`: extra SSH options (repeatable, e.g., `-o "ProxyJump=bastion"`)
- `--no-server-start`: don't auto-start the remote server
- `--dry-run`: print SSH commands instead of running them
- `-f` / `--foreground`: run in the foreground instead of backgrounding
- `--ignore-version-mismatch`: connect even if the remote protocol version differs from local

**Send/receive options:**
- `--session host:session`: target a specific session
- `--stdin` (`send`): read data from stdin instead of files
- `--stdout` (`receive`): write data to stdout instead of files
- `--timeout <seconds>`: deadline for pairing with a receiver/sender

File permissions are preserved. For directories, use tar with pipe mode:

```bash
# sender (remote)
tar czf - mydir | gritty send --stdin
# receiver (local)
gritty receive --stdout | tar xzf -
```

**Environment inside sessions:** `GRITTY_SOCK` (svc socket for `gritty open`/`send`/`receive`/port forwarding), `GRITTY_SESSION` (session ID), and `GRITTY_SESSION_NAME` (if named) are set in the shell environment. Useful for prompt customization or scripts that need to know which session they're in.

**Port forwarding:** port spec is `PORT` (same on both ends) or `LISTEN_PORT:TARGET_PORT`. Runs inside a session (`GRITTY_SOCK` required). Ctrl-C stops the forward. All forwarding binds to `127.0.0.1` only -- there is no bind-address option (unlike SSH's `-L`/`-R`). These are transient, on-demand forwards -- great for quick checks during development. For always-on port forwarding, configure it on the SSH tunnel instead: `gritty tunnel-create devbox -o "LocalForward=8080 localhost:8080"` or add it to `ssh-options` in your config file.

## Comparison

|  | **gritty** | [**mosh**](https://mosh.org/) | [**ET**](https://eternalterminal.dev/) | **autossh + tmux** |
|--|:--:|:--:|:--:|:--:|
| Survives network change | yes | yes | yes | yes |
| Survives client reboot | yes | no | no | yes |
| Auto-reconnect | yes | yes | yes | autossh only |
| SSH agent forwarding | ✨ yes | [no](https://github.com/mobile-shell/mosh/issues/120) | [no](https://github.com/MisterTea/EternalTerminal/issues/41) | [stale socket](https://werat.dev/blog/happy-ssh-agent-forwarding/) |
| Browser / URL forwarding | ✨ yes | no | no | no |
| OAuth callback tunneling | ✨ yes | no | no | no |
| Port forwarding | yes | no | yes | SSH -L/-R |
| File transfer | yes | no | no | scp/rsync |
| Predictive local echo | no | yes | no | no |
| Scroll-back / panes | no | no | no | tmux |
| No extra ports / firewall | yes | no (UDP) | no (TCP) | yes |
| IP roaming (mobile) | reconnect | seamless | reconnect | reconnect |
| Windows client | no | no | no | yes |
| Maturity | 0.9.3 (new) | mature | mature | mature |

**Where gritty wins:** seamless local-tool integration. SSH agent forwarding that survives reconnects without stale sockets. Browser opens and OAuth flows that just work remotely. Port forwarding and file transfer multiplexed over the session -- no extra tunnels or tools. Stateless client -- reboot your laptop, `gritty connect` picks up where you left off.

**Where gritty loses:** no predictive local echo (mosh is unbeatable on high-latency links), no scroll-back or window management (use tmux inside gritty), no Windows support, and it's early-stage software.

**gritty + tmux** is the ideal pairing. gritty handles the connection -- self-healing tunnels, agent forwarding, auto-reconnect -- while tmux handles the workspace -- splits, windows, copy-mode, scroll-back. Run tmux inside a gritty session and close your laptop, change wifi, open it back up: your tmux splits are exactly where you left them, no re-SSH and `tmux attach` required. gritty replaces the fragile SSH pipe underneath tmux, not tmux itself.

## Configuration

gritty works out of the box with no config file. Optionally, set persistent defaults in `$XDG_CONFIG_HOME/gritty/config.toml` (default: `~/.config/gritty/config.toml`). Run `gritty config-edit` to create and open the config file.

```toml
# Global defaults for all sessions/connections.
[defaults]
# forward-agent = true
# forward-open = true
# no-escape = false
# no-redraw = false
# oauth-redirect = true
# oauth-timeout = 180
# heartbeat-interval = 5
# heartbeat-timeout = 15
# ring-buffer-size = 1048576
# oauth-tunnel-idle-timeout = 5

# Tunnel-specific global defaults (for tunnel-create).
[defaults.tunnel]
# ssh-options = []
# no-server-start = false

# Per-host overrides, keyed by connection name.
# Connection name = hostname from destination, or -n override.
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

A missing or malformed config file is silently ignored. Use `gritty info` to check config status.

## Escape Sequences

After a newline (or at session start), `~` enters escape mode:

| Sequence | Action |
|----------|--------|
| `~.` | Detach from session (clean exit, no auto-reconnect) |
| `~R` | Force reconnect |
| `~#` | Session status and RTT |
| `~^Z` | Suspend the client (SIGTSTP) |
| `~?` | Print help |
| `~~` | Send a literal `~` |

## Shell Completions

```bash
# Bash
gritty completions bash > /etc/bash_completion.d/gritty

# Zsh -- put in fpath and ensure compinit runs after:
mkdir -p ~/.zfunc
gritty completions zsh > ~/.zfunc/_gritty
# Add to .zshrc (before compinit):  fpath=(~/.zfunc $fpath)
# Then: rm -f ~/.zcompdump && exec zsh

# Fish
gritty completions fish > ~/.config/fish/completions/gritty.fish
```

## Troubleshooting

**"gritty not found on remote host"** -- gritty must be installed on the remote host too. Install it with `curl -sSfL https://raw.githubusercontent.com/chipturner/gritty/main/install.sh | sh` (or `cargo install gritty-cli`), and ensure it's in `$HOME/bin`, `$HOME/.local/bin`, `$HOME/.cargo/bin`, or another standard path.

**First connect hangs or fails** -- gritty backgrounds the SSH tunnel, so it can't prompt for a password or host key. Make sure `ssh <destination>` works first, then use `gritty connect`.

**"[reconnecting...]" forever** -- the SSH tunnel is down and not coming back. Check `gritty tunnels` for tunnel status. If the tunnel shows as stale, `gritty tunnel-destroy <name>` to clean it up and `gritty tunnel-create <dest>` to re-establish. Check `gritty info` for log file paths if you need to dig deeper.

**Protocol version mismatch after upgrade** -- if you upgrade gritty on one side but not the other, connections will be rejected with a version mismatch error. Upgrade both sides to the same version. `gritty protocol-version` shows the local version. If you need to connect temporarily before upgrading, use `gritty tunnel-create --ignore-version-mismatch`.

## Design

gritty contains zero networking code. Sessions live on Unix domain sockets; for remote access, you forward the socket over SSH -- the same SSH that already handles your keys, `.ssh/config`, bastion hosts, and MFA. No ports to open, no firewall rules, no TLS certificates, no authentication system to trust beyond the one you already use.

All communication -- control and session relay -- flows through a single server socket. When a client connects, the server hands off the raw connection and gets out of the loop. The PTY and shell keep running when the client disconnects; output drains into a ring buffer so the shell never blocks. On reconnect, buffered output is flushed before the relay resumes.

Locally, the socket is `0600`, the directory is `0700`, and every `accept()` verifies the peer UID. The attack surface is small because there's very little to attack.

See [ARCHITECTURE.md](ARCHITECTURE.md) for diagrams and detailed protocol description.

## Status

Early stage. Works on Linux and macOS. Expect rough edges -- patches welcome.

## License

MIT OR Apache-2.0
