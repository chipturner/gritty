# gritty

[![CI](https://github.com/chipturner/gritty/actions/workflows/ci.yml/badge.svg)](https://github.com/chipturner/gritty/actions/workflows/ci.yml)
[![Crates.io](https://img.shields.io/crates/v/gritty-cli)](https://crates.io/crates/gritty-cli)
[![License: MIT OR Apache-2.0](https://img.shields.io/crates/l/gritty-cli)](LICENSE)

Persistent remote shells that just work.

Connect once with gritty. Close your laptop, change wifi, lose the network -- it reconnects itself and you're exactly where you left off, with your SSH agent, port forwards, and in-progress work all intact. No stale sockets, no `tmux attach`, no re-auth. And even if your laptop reboots, your sessions are still there to resume.

```
laptop$ gritty connect devbox:work          # creates or reattaches -- one command, always
```

Sessions are persistent and self-healing. The tunnel respawns on failure, the client auto-reconnects, and buffered output bridges the gap so nothing is lost. Your local tools come with you: `git push` uses your local SSH keys, `gh auth login` opens your local browser and tunnels the OAuth callback, `gritty send`/`receive` moves files through the session without scp. It feels like a local shell that happens to be remote.

gritty works by forwarding Unix domain sockets over SSH -- no custom protocol, no open ports, no certificates, no configuration. If you can `ssh` to a host, you can use gritty.

### Install

**Prebuilt binaries** (Linux x86_64/ARM64, macOS x86_64/ARM64). Linux builds are statically linked (musl), so they run on any Linux host regardless of glibc version:

```
# Download the install script, review it, then run:
laptop$ curl -sSfL https://raw.githubusercontent.com/chipturner/gritty/main/install.sh -o install.sh
laptop$ less install.sh
laptop$ sh install.sh

# Or via cargo-binstall:
laptop$ cargo binstall gritty-cli

# Or via Homebrew:
laptop$ brew install chipturner/tap/gritty
```

**From source:**

```
laptop$ cargo install gritty-cli   # binary name: gritty, requires Rust 1.94+
```

Install on **both your laptop and the remote host**. `gritty bootstrap <destination>` can do the remote side for you over SSH.

### Quick Start

First make sure plain `ssh devbox` works -- enough to accept the host key and enter any password. gritty backgrounds the SSH tunnel, so it can't prompt for either. Then, from your laptop:

```
laptop$ gritty connect devbox:work
```

That's it. gritty auto-starts the SSH tunnel and the remote server and drops you into a shell on `devbox`. URL/OAuth forwarding is on by default; agent forwarding is opt-in with `-A`. Run the same command again -- from this terminal or any other machine -- and it reattaches instead of creating a new session:

```
laptop$ gritty connect devbox:work          # reattach from anywhere
laptop$ gritty ls                            # all hosts, all sessions, tunnel health
laptop$ gritty ls devbox                     # list sessions on devbox only
laptop$ gritty tunnels                        # list active tunnels
```

Detach with `~.` or just close the terminal -- the remote shell keeps running.

**Move files through the session**, no scp -- run one side on each machine:

```
laptop$ gritty send report.pdf
devbox$ gritty receive .
```

Send a directory with `-r`, or pipe through `tar` for compression (either direction works):

```
laptop$ gritty send -r mydir                 # recursive, preserves structure
devbox$ tar czf - logs/ | gritty send -      # pipe mode
laptop$ gritty receive - | tar xzf -
```

In `devbox:work`, the part before the colon is **the host's name** and the part after is a **session name** you choose (so you can keep several going). By default the host name is just the SSH hostname -- `gritty connect devbox:work` targets the machine you reach as `devbox`. You can remap it (`gritty tunnel-create user@10.0.0.5 -n devbox`) when the SSH destination and the name you'd rather type differ. The reserved host `local` runs a server on this machine -- handy for testing, but remote sessions are the point.

Session names are scoped to your client: `gritty connect devbox:work` from your laptop lands in a session named `mylaptop/work` on the wire, so when you log in from another machine you don't silently land in (and clobber) each other's sessions. A name containing `/` is taken literally -- use that form to reach another client's session (`devbox:laptop2/work`) or to create a deliberately-shared one. The client prefix defaults to your hostname; set `client-name` in `~/.config/gritty/config.toml` to override.

See **[USAGE.md](USAGE.md)** for every command and flag, configuration, escape sequences, completions, and debugging.

## Features

- **Self-healing connections** -- heartbeat detection, automatic tunnel respawn, transparent reconnect
- **Persistent sessions** -- shells survive disconnect, network failure, laptop sleep; `connect` reattaches or creates; multiple named sessions; opt-in `linger` to auto-reap detached throwaway sessions after a timeout (`~K` to pin one you decide to keep)
- **SSH agent forwarding** -- `git push`, `ssh`, and other agent-dependent commands work remotely using your local keys (opt-in via `-A`); survives reconnects without stale sockets
- **URL open forwarding** -- `$BROWSER` requests forwarded to your local machine, with automatic OAuth callback tunneling (on by default; disable with `--no-forward-open`)
- **Port forwarding** -- `gritty rf 8080` to quick-check a remote web server locally, `gritty lf 5432` to let the session reach local postgres (the target defaults to your attached session; name one with `gritty rf devbox:work 8080`); client-initiated only (a compromised server cannot open forwards)
- **File transfer** -- `gritty send` / `gritty receive` through the session connection, preserving permissions; `-r` for recursive directory transfer; pipe mode with `-` (implied when stdout is redirected) for composing with `tar` etc.
- **Clipboard forwarding** -- `gritty copy` pushes clipboard content from a remote session to your local machine (uses `pbcopy` on macOS, `wl-copy`/`xclip`/`xsel` on Linux); copy-only by design (the server cannot read the client clipboard)
- **Single binary, no network protocol** -- Unix domain sockets locally, SSH handles encryption and auth; optional TOML config for per-host defaults

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
| Maturity | pre-1.0 (new) | mature | mature | mature |

**Advantages:** seamless local-tool integration -- SSH agent forwarding that survives reconnects without stale sockets, browser opens and OAuth flows that just work remotely, and port forwarding plus file transfer multiplexed over the session with no extra tunnels or tools. The client is stateless: reboot your laptop and `gritty connect` picks up where you left off.

**Trade-offs:** no predictive local echo, so mosh still feels better on high-latency links; no built-in scroll-back or window management (run tmux inside gritty for that); no Windows support; and it's early-stage software.

**gritty + tmux** is the ideal pairing. gritty handles the connection -- self-healing tunnels, agent forwarding, auto-reconnect -- while tmux handles the workspace -- splits, windows, copy-mode, scroll-back. Run tmux inside a gritty session and close your laptop, change wifi, open it back up: your tmux splits are exactly where you left them, no re-SSH and `tmux attach` required. gritty replaces the fragile SSH pipe underneath tmux, not tmux itself.

## Configuration

gritty works out of the box with no config file. To set persistent defaults -- forwarding behavior, per-host SSH options, host aliases, heartbeat timings -- put them in `config.toml`:

```
laptop$ gritty config        # create and open it at the right path
laptop$ gritty info          # print its location and load status
```

The file lives at `~/.config/gritty/config.toml` on Linux (honors `$XDG_CONFIG_HOME`) or `~/Library/Application Support/gritty/config.toml` on macOS. Precedence is CLI flag > `[host.<name>]` > `[defaults]` > built-in default. See [USAGE.md](USAGE.md#configuration) for the full reference and examples.

## Troubleshooting

**"gritty not found on remote host"** -- gritty must be installed on the remote too. Use `gritty bootstrap <destination>`, `cargo install gritty-cli`, or the review-first install flow above, and make sure it lands on your `$PATH` (`$HOME/bin`, `$HOME/.local/bin`, `$HOME/.cargo/bin`, etc.).

**First connect hangs or fails** -- gritty backgrounds the SSH tunnel, so it can't prompt for a password or host key. Make sure `ssh <destination>` works first, then retry.

**"reconnecting..." forever** -- the SSH tunnel is down and not recovering. Check `gritty tunnels`; if a tunnel is stale, `gritty tunnel-destroy <name>` then `gritty tunnel-create <dest>` to rebuild it. `gritty doctor` reports what's wrong and where the logs are. While the status line is up, any key forces an immediate retry (including past a stale `waiting for network`); `^C` gives up.

**Protocol version mismatch after upgrade** -- after upgrading one side, run `gritty refresh` to restart whatever is running stale code (local daemon, tunnel supervisors, and remote daemons). It's idempotent and works across the mismatch without falling back to raw SSH. For remote hosts it ends with an end-to-end protocol probe: if the remote *binary* itself is an older release, refresh says so and points at `gritty bootstrap <host>`. `gritty doctor` shows what's stale; see [USAGE.md](USAGE.md#debugging) for details.

**Sessions vanished / stray `gritty server` processes on a remote host** -- systemd wipes `/run/user/<uid>` when your last login session on a host ends, deleting the socket directory out from under the daemon. Current daemons self-heal (re-bind within seconds; sessions survive) or exit cleanly when they can't. Daemons from older releases linger as unreachable orphans: `gritty doctor` reports them and `gritty refresh` reaps them. Prevent the wipe entirely with `loginctl enable-linger` on the remote. See [USAGE.md](USAGE.md#debugging).

## Design

gritty contains zero networking code. Sessions live on Unix domain sockets; for remote access, you forward the socket over SSH -- the same SSH that already handles your keys, `.ssh/config`, bastion hosts, and MFA. No ports to open, no firewall rules, no TLS certificates, no authentication system to trust beyond the one you already use.

All communication -- control and session relay -- flows through a single server socket. When a client connects, the server hands off the raw connection and gets out of the loop. The PTY and shell keep running when the client disconnects; the server keeps an offset-indexed byte history of output so the shell never blocks. On reconnect the client reports how far it rendered and the server replays exactly the bytes it missed -- a brief blip resumes byte-for-byte with nothing redrawn.

Locally, the socket is `0600`, the directory is `0700`, and every `accept()` verifies the peer UID. The attack surface is small because there's very little to attack.

## Security Model

gritty is designed so that a compromised remote server cannot leverage the session connection to attack your local machine. The client gates all sensitive operations:

- **Port forwarding is client-initiated only.** The `lf`/`rf` commands talk to the client process through a local forward socket and the client sends `PortForwardRequest` frames to the server. The server cannot initiate port forwards on its own.
- **URL opening is opt-out and rate limited.** `OpenUrl` frames are forwarded by default; disable with `--no-forward-open` (or `forward-open = false` in config). Rate limited to 2 opens per 30 seconds.
- **Clipboard is push-only.** The server can push to the client clipboard (`ClipboardSet`, rate limited to 5 per 30s), but cannot read it -- `ClipboardGet` always returns empty.
- **Agent forwarding is opt-in.** SSH agent forwarding defaults to off; enable with `-A` or `forward-agent = true` in config. `AgentOpen` is rate limited to 10 per 30 seconds.
- **Tunnel creation is rate limited.** `TunnelListen` frames are rate limited to 2 per 30 seconds.
- **Audit logging.** All security-sensitive operations (forwarding requests, clipboard access, agent connections) are logged at info/warn level on both client and server.

**Recommendations for untrusted hosts:** pass `--no-forward-open` and `--no-forward-agent` to minimize the trust surface. `--no-forward-agent` is needed because agent forwarding can also be switched on by `forward-agent = true` in `[defaults]`, where omitting `-A` is not enough to disable it. Port forwarding is always safe since it requires explicit client-side action.

## Documentation

- **[USAGE.md](USAGE.md)** -- complete command and flag reference, configuration, escape sequences, shell completions, and debugging
- **[ARCHITECTURE.md](ARCHITECTURE.md)** -- diagrams and protocol details
- **[docs/tunnel-state-machine.md](docs/tunnel-state-machine.md)** -- SSH tunnel supervisor state machine

## Status

Early stage. Works on Linux and macOS. Expect rough edges -- patches welcome.

**Compatibility policy (pre-1.0):** any release may change the wire protocol, and mismatched sides refuse to talk rather than misbehave. This is a managed break, not a stranding: after upgrading, one `gritty refresh` restarts everything running stale code -- the local daemon, tunnel supervisors, and remote daemons -- across all your hosts, without touching live sessions on hosts that are already current. Sessions on a restarted daemon do not survive the restart, so finish what matters before refreshing. A frozen, append-only protocol is the plan for 1.0.

## License

MIT OR Apache-2.0
