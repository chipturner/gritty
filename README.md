# gritty

Persistent remote shells that bring your local tools with them.

> **Early stage.** Works on Linux and macOS. Available on [crates.io](https://crates.io/crates/gritty-cli). Expect rough edges -- patches welcome.

```bash
gritty new devbox:work -A -O        # connect, forward SSH agent + browser/OAuth

# Inside the session -- your local tools just work:
git push                            # uses your local SSH keys via -A
gh auth login                       # OAuth opens in your local browser via -O
gritty lf 8080                      # quick-check a remote web server locally
gritty rf 5432                      # let the session reach local postgres
```

Close your laptop, change wifi, open it back up: you're exactly where you left off.

It works by forwarding Unix domain sockets over SSH -- no custom protocol, no open ports, no certificates, no configuration. If you can `ssh` to a host, you can use gritty.

### Install

Install on **both your laptop and the remote host**:

```bash
cargo install gritty-cli
```

### Quick Start

For first-time setup, use `--foreground` so you can accept the host key and enter your password if needed:

```bash
gritty connect --foreground devbox   # set up tunnel (Ctrl-C when done)
```

After that, one command creates a session and connects -- auto-starting the tunnel and remote server:

```bash
gritty new devbox:work -A -O
```

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

gritty attach devbox:work           # reattach from any terminal, any machine
gritty ls devbox                    # list sessions
gritty tunnels                      # list active tunnels
```

For local sessions (useful for testing): `gritty new local:scratch`

## Features

- **Self-healing connections** -- heartbeat detection, automatic tunnel respawn, transparent reconnect
- **Persistent sessions** -- shells survive disconnect, network failure, laptop sleep; reattach from any terminal or machine; multiple named sessions
- **SSH agent forwarding** (`-A`) -- `git push`, `ssh`, and other agent-dependent commands work remotely
- **URL open forwarding** (`-O`) -- `$BROWSER` requests forwarded to your local machine, with automatic OAuth callback tunneling
- **Port forwarding** -- `gritty local-forward` / `gritty remote-forward` for transient TCP forwards through the session
- **File transfer** -- `gritty send` / `gritty receive` through the session connection, with `--stdin`/`--stdout` pipe mode
- **Single binary, no network protocol** -- Unix domain sockets locally, SSH handles encryption and auth; optional TOML config for per-host defaults

## Commands

| Command | Aliases | Description |
|---------|---------|-------------|
| `gritty new-session <host[:name]>` | `new` | Create a session and auto-attach |
| `gritty attach <host:session>` | `a` | Attach to a session (`-c` creates if missing) |
| `gritty tail <host:session>` | `t` | Read-only stream of session output |
| `gritty list-sessions [host]` | `ls`, `list` | List sessions (no args = all daemons; foreground process shown on Linux only) |
| `gritty kill-session <host:session>` | | Kill a session |
| `gritty rename <host:session> <name>` | | Rename a session |
| `gritty kill-server <host>` | | Kill the server and all sessions |
| `gritty connect <destination>` | `c` | Set up SSH tunnel to remote host |
| `gritty disconnect <name>` | `dc` | Tear down an SSH tunnel |
| `gritty tunnels` | `tun` | List active SSH tunnels |
| `gritty send [-r] [files...]` | | Send files/directories to a paired receiver |
| `gritty receive [dir]` | | Receive files from a paired sender |
| `gritty local-forward <port>` | `lf` | Forward a TCP port from session to client |
| `gritty remote-forward <port>` | `rf` | Forward a TCP port from client to session |
| `gritty open <url>` | | Open a URL on the local machine (inside sessions) |
| `gritty info` | | Show diagnostics (version, config, server status, tunnels) |
| `gritty config-edit` | | Open config in `$VISUAL`/`$EDITOR` (creates from template if missing) |
| `gritty completions <shell>` | | Generate shell completions (bash, zsh, fish, elvish, powershell) |

The `<host>` in `host:session` is a **connection name**, not an SSH destination. It's the name assigned by `gritty connect` -- by default the hostname, overridable with `-n`. `local` is the reserved name for the local server. For example, `gritty connect user@mybox.example.com -n devbox` creates connection name `devbox`, so you'd use `gritty new devbox:work`. The special session name `-` refers to the last-attached session (e.g. `gritty attach devbox:-`). Auto-starts server/tunnel on `new`; `attach` waits for an existing server. `send`/`receive` auto-detect the session across all active daemons; use `--session host:session` to target a specific one.

**Global options:**
- `-v` / `--verbose`: enable debug logging
- `--ctl-socket <path>`: override the server socket path

**Session options** (`new`/`attach`):
- `-A` / `--forward-agent`: forward your local SSH agent
- `-O` / `--forward-open`: forward URL opens to local machine
- `-c <cmd>` / `--command` (`new` only): run a command instead of a login shell
- `-d` / `--detach` (`new` only): create session without attaching (background jobs)
- `--no-redraw`: don't send Ctrl-L after connecting
- `--no-escape`: disable escape sequence processing
- `--no-oauth-redirect`: disable OAuth callback tunneling (part of `-O`)
- `--oauth-timeout <seconds>`: OAuth callback accept timeout (default: 180)
- `-w` / `--wait` (`new` only): wait indefinitely for the server

**Connect options:**
- `-n <name>`: override connection name (defaults to hostname)
- `-o <option>` / `--ssh-option`: extra SSH options (repeatable, e.g., `-o "ProxyJump=bastion"`)
- `--no-server-start`: don't auto-start the remote server
- `--dry-run`: print SSH commands instead of running them
- `-f` / `--foreground`: run in the foreground instead of backgrounding

**Send/receive options:**
- `--session host:session`: target a specific session
- `--stdin` (`send`): read data from stdin instead of files
- `--stdout` (`receive`): write data to stdout instead of files
- `-r` / `--recursive` (`send`): send directories recursively
- `--timeout <seconds>`: deadline for pairing with a receiver/sender

**Environment inside sessions:** `GRITTY_SOCK` (svc socket for `gritty open`/`send`/`receive`/port forwarding), `GRITTY_SESSION` (session ID), and `GRITTY_SESSION_NAME` (if named) are set in the shell environment. Useful for prompt customization or scripts that need to know which session they're in.

**Port forwarding:** port spec is `PORT` (same on both ends) or `LISTEN:TARGET`. Runs inside a session (`GRITTY_SOCK` required). Ctrl-C stops the forward. These are transient, on-demand forwards -- great for quick checks during development. For always-on port forwarding, configure it on the SSH tunnel instead: `gritty connect devbox -o "LocalForward=8080 localhost:8080"` or add it to `ssh-options` in your config file.

## Comparison

|  | **gritty** | [**mosh**](https://mosh.org/) | [**ET**](https://eternalterminal.dev/) | **autossh + tmux** |
|--|:--:|:--:|:--:|:--:|
| Survives network change | yes | yes | yes | yes |
| Survives client reboot | yes | no | no | yes |
| Auto-reconnect | yes | yes | yes | autossh only |
| SSH agent forwarding | yes | [no](https://github.com/mobile-shell/mosh/issues/120) | [no](https://github.com/MisterTea/EternalTerminal/issues/41) | [stale socket](https://werat.dev/blog/happy-ssh-agent-forwarding/) |
| Browser / URL forwarding | yes | no | no | no |
| OAuth callback tunneling | yes | no | no | no |
| Port forwarding | yes | no | yes | SSH -L/-R |
| File transfer | yes | no | no | scp/rsync |
| Predictive local echo | no | yes | no | no |
| Scroll-back / panes | no | no | no | tmux |
| No extra ports / firewall | yes | no (UDP) | no (TCP) | yes |
| IP roaming (mobile) | reconnect | seamless | reconnect | reconnect |
| Windows client | no | no | no | yes |
| Maturity | early | mature | mature | mature |

**Where gritty wins:** seamless local-tool integration. SSH agent forwarding that survives reconnects without stale sockets. Browser opens and OAuth flows that just work remotely. Port forwarding and file transfer multiplexed over the session -- no extra tunnels or tools. Stateless client -- reboot your laptop, `gritty attach` picks up where you left off.

**Where gritty loses:** no predictive local echo (mosh is unbeatable on high-latency links), no scroll-back or window management (use tmux inside gritty), no Windows support, and it's early-stage software.

**gritty + tmux** is the ideal pairing. gritty handles the connection -- self-healing tunnels, agent forwarding, auto-reconnect -- while tmux handles the workspace -- splits, windows, copy-mode, scroll-back. Run tmux inside a gritty session and close your laptop, change wifi, open it back up: your tmux splits are exactly where you left them, no re-SSH and `tmux attach` required. gritty replaces the fragile SSH pipe underneath tmux, not tmux itself.

## Configuration

gritty works out of the box with no config file. Optionally, set persistent defaults in `$XDG_CONFIG_HOME/gritty/config.toml` (default: `~/.config/gritty/config.toml`). Run `gritty config-edit` to create and open the config file.

```toml
# Global defaults for all sessions/connections.
[defaults]
# forward-agent = false
# forward-open = false
# no-escape = false
# no-redraw = false
# oauth-redirect = true
# oauth-timeout = 180
# heartbeat-interval = 5
# heartbeat-timeout = 15
# ring-buffer-size = 1048576
# oauth-tunnel-idle-timeout = 5

# Connect-specific global defaults.
[defaults.connect]
# ssh-options = []
# no-server-start = false

# Per-host overrides, keyed by connection name.
# Connection name = hostname from destination, or -n override.
[host.devbox]
forward-agent = true
forward-open = true

[host.devbox.connect]
ssh-options = ["IdentityFile=~/.ssh/devbox_tunnel_key"]

[host.prod]
forward-open = true
no-escape = true

[host.prod.connect]
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

**"gritty not found on remote host"** -- gritty must be installed on the remote host too. Run `cargo install gritty-cli` there, or ensure it's in `$HOME/bin`, `$HOME/.local/bin`, `$HOME/.cargo/bin`, or another standard path.

**First connect hangs or fails** -- if SSH needs a password or host key confirmation, the background process can't prompt you. Use `gritty connect --foreground <dest>` the first time to handle the interactive prompt, then use the normal backgrounding flow afterwards.

**"[reconnecting...]" forever** -- the SSH tunnel is down and not coming back. Check `gritty tunnels` for tunnel status. If the tunnel shows as stale, `gritty disconnect <name>` to clean it up and `gritty connect <dest>` to re-establish. Check `gritty info` for log file paths if you need to dig deeper.

**Protocol version mismatch after upgrade** -- if you upgrade gritty on one side but not the other, the version handshake negotiates down to the older protocol. This usually works, but if you see unexpected errors, upgrade both sides to the same version. `gritty protocol-version` shows the local version; `gritty info` shows tunnel status.

## Design

gritty contains zero networking code. Sessions live on Unix domain sockets; for remote access, you forward the socket over SSH -- the same SSH that already handles your keys, `.ssh/config`, bastion hosts, and MFA. No ports to open, no firewall rules, no TLS certificates, no authentication system to trust beyond the one you already use.

All communication -- control and session relay -- flows through a single server socket. When a client connects, the server hands off the raw connection and gets out of the loop. The PTY and shell keep running when the client disconnects; output drains into a ring buffer so the shell never blocks. On reconnect, buffered output is flushed before the relay resumes.

Locally, the socket is `0600`, the directory is `0700`, and every `accept()` verifies the peer UID. The attack surface is small because there's very little to attack.

See [ARCHITECTURE.md](ARCHITECTURE.md) for diagrams and detailed protocol description.

## License

MIT OR Apache-2.0
