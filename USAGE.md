# gritty usage

Complete command and flag reference. For an overview and quick start, see [README.md](README.md).

## Commands

| Command | Aliases | Description |
|---------|---------|-------------|
| `gritty connect [host[:name]]` | `c` | Smart session: attach if exists, create if not |
| `gritty list-sessions [host] [--json]` | `ls`, `list` | List sessions. Bare `gritty ls` shows every known host -- local + all tunnels -- grouped by daemon (tunnels reaching the same daemon are merged). With a host, lists just that host. Your own sessions are bold and sorted first; foreign sessions group by client (foreground process shown on Linux only). The `Idle` column shows time since the session's last terminal activity (output or keystrokes); detached sessions also show how long ago a client was last attached |
| `gritty tail [host:session]` | `t` | Read-only stream of session output |
| `gritty kill-session [targets...]` | `kill` | Kill one or more sessions. Each target is `host:session`, or a bare session name/ID killed on `local` (so after `gritty ls`, `gritty kill 3 5 work` reaps by ID or name). A bare target naming a known host lists that host's sessions instead. Numeric targets match your own namespace name first, then fall back to the raw session ID |
| `gritty prune [host]` | | Bulk-kill stale detached sessions. Select with `--client <name>` (sessions created by that client, repeatable -- e.g. a laptop you know rebooted) and/or `--idle <duration>` (no terminal activity for at least that long: `90s`, `30m`, `12h`, `7d`) -- the two AND together -- or `--all` (every detached session; excludes the other filters). Or pick interactively with `--pick` (TUI: space marks, `a` marks all, `1`-`9` toggle, enter kills the marked set after a y/n confirm; `--client`/`--idle` narrow the candidate list; not combinable with `--all` or `-y`). Dry run by default: prints the selection and stops; pass `-y` to kill it. Attached sessions are never touched |
| `gritty rename <host:session> <name>` | | Rename a session |
| `gritty kill-server [host]` | | Kill the server and all sessions (works across a protocol version mismatch) |
| `gritty restart [host]` | | Kill + restart server (and tunnel, for remote hosts). One-shot upgrade recovery |
| `gritty refresh [host]` | | Restart only processes running stale code, reap orphaned daemons, and (for remote hosts) verify protocol compatibility end to end (idempotent). No args = local + all tunnels |
| `gritty tunnels [--json]` | `tun` | List active SSH tunnels |
| `gritty tunnel-create <destination>` | | Set up SSH tunnel to remote host |
| `gritty tunnel-destroy <name>` | | Tear down an SSH tunnel |
| `gritty bootstrap <destination>` | | Install gritty on a remote host |
| `gritty local-forward [target] <port>` | `lf` | Make a local (client-side) port reachable inside the session (like ssh `-R`) |
| `gritty remote-forward [target] <port>` | `rf` | Bring a remote (session-side) port to the client (like ssh `-L`) |
| `gritty send [files...]` | | Send files to a paired receiver |
| `gritty receive [dir]` | | Receive files from a paired sender |
| `gritty open <url>` | | Open a URL on the local machine (for use inside gritty sessions) |
| `gritty copy` | | Copy stdin to the client clipboard (for use inside gritty sessions) |
| `gritty info [--json]` | | Show diagnostics (paths, server status, device id, tunnels) |
| `gritty config` | | Open config in `$VISUAL`/`$EDITOR`/vi (creates from template if missing) |
| `gritty doctor [--clean \| --json \| --llm [desc]]` | | Show key paths and check for common issues (stale processes, orphaned daemons, orphaned sockets, config errors); `--clean` removes socket-dir files this version doesn't recognize; `--llm` prints an LLM-ready diagnostic report instead (mutually exclusive with `--clean`/`--json`; `--log-lines <N>` adjusts log excerpt size -- see [Debugging](#debugging)) |
| `gritty server` | `s` | Start the server (backgrounds by default; `-f` for foreground) |
| `gritty completions <shell>` | | Generate shell completions (bash, zsh, fish, elvish, powershell) |
| `gritty mangen <dir>` | | Write man pages (one per subcommand) into `<dir>` -- for packagers |
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

`local` is a reserved connection name for a server on this machine (no SSH tunnel). Omitting the host entirely targets `local`: bare `gritty connect`, `gritty tail`, `gritty prune --pick`, `gritty kill-server`, and `gritty restart` all address the local daemon. The exceptions are `gritty ls` and `gritty refresh`, where no host means every known host.

A `[host.<name>] aliases` config entry makes alternate spellings resolve to the same connection name, so `gritty connect devbox.example.com:work` and `gritty connect devbox:work` address the same tunnel and sessions (see [Configuration](#configuration)).

**`session`** is a name you choose so you can run several sessions per host. Rules:

- Omitted: `connect` looks only at sessions in your own namespace (`<client>/*`). It attaches the sole detached one, shows a picker when the choice is ambiguous, and falls back to `<client>/0` when your namespace is empty. Auto-created sessions get the next free integer slot in your namespace (`0`, `1`, `2`, ...). Foreign-namespace and legacy unprefixed sessions are ignored -- reach those with the explicit slash-bearing form (`gritty connect host:other/name`).
- `-`: refers to the last-attached session, e.g. `gritty connect devbox:-`.
- Purely numeric *wire* names are rejected by the server (they would collide with auto-assigned session IDs). Typing `0` as a short name still works -- the client prefix turns it into `<client>/0`, which contains a slash and so is not purely numeric.

**Client namespacing.** Every short name (no `/`) you type is silently scoped to your client's namespace. With `client-name = "mylaptop"` (the default is your hostname), `gritty connect devbox:work` resolves to the wire name `mylaptop/work`. Two laptops typing the same short name no longer collide -- each lands in its own session. To address a session in another client's namespace (or a deliberately-shared session), type the full slash-bearing form: `gritty connect devbox:laptop2/work` is taken literally with no prefix added. `gritty ls` strips your own prefix for readability and shows foreign prefixes intact.

`connect` auto-starts the server and tunnel as needed. `send`/`receive` auto-detect the session across all active servers; use `--session host:session` to target a specific one.

## Options

### Global options

- `-v` / `--verbose`: enable debug logging
- `--ctl-socket <path>`: override the server socket path

### Session options (`connect`)

Flag defaults come from config, with precedence CLI > `[host.<name>]` > `[defaults]` > built-in. That's why on-by-default features still have an enable flag: it overrides a config-file `false` for one invocation.

- `-A` / `--forward-agent`: forward your local SSH agent (off by default)
- `--no-forward-agent`: never forward the agent, even if `forward-agent = true` in config
- `-O` / `--forward-open`: forward URL opens to local machine (on by default; `-O` overrides a `forward-open = false` in config, `--no-forward-open` disables for this connect)
- `-c <cmd>` / `--command`: run a command instead of a login shell (when creating)
- `-d` / `--detach`: create session without attaching (background jobs)
- `--force`: take over an already-attached session without prompting
- `--pick`: always show session picker (interactive when in a terminal)
- `--no-pick`: never show session list; always target session `0`
- `-n` / `--new`: skip the picker and create the next free integer-slot session in your namespace (`0`, `1`, `2`, ...)
- `--no-create`: attach only, error if session doesn't exist
- `--linger <duration>`: how long the session survives with no client attached before the server reaps it (e.g. `30m`, `1h`, `never`); overrides the `linger`/`linger-unnamed` config (default: `never`)
- `--no-escape`: disable escape sequence processing
- `--no-oauth-redirect`: disable OAuth callback tunneling (part of `-O`)
- `--oauth-timeout <seconds>`: OAuth callback accept timeout (default: 180)
- `-w` / `--wait`: wait indefinitely for the server

### Tunnel options (`tunnel-create`)

- `-n <name>` / `--name`: override connection name (defaults to hostname)
- `-o <option>` / `--ssh-option`: extra SSH options (repeatable, e.g., `-o "ProxyJump=bastion"`)
- `--no-server-start`: don't auto-start the remote server
- `--dry-run`: print SSH commands instead of running them
- `-f` / `--foreground`: run in the foreground instead of backgrounding
- `--ignore-version-mismatch`: connect even if the remote protocol version differs from local

### Bootstrap options (`bootstrap`)

- `--install-dir <dir>`: remote install directory (default: `~/.local/bin`)
- `-o <option>` / `--ssh-option`: extra SSH options (repeatable)

### Send/receive options

- `--session host:session`: target a specific session
- `-r` / `--recursive` (`send`): send directories recursively (preserves structure, skips symlinks)
- `-` (`send`): read data from stdin; (`receive`): write data to stdout. `receive` with no destination also auto-switches to stdout when its stdout is redirected (e.g. `gritty receive > foo` or piped); pass a directory to force file mode
- `--timeout <seconds>`: deadline for pairing with a receiver/sender (default 300; `--no-timeout` waits indefinitely)

File permissions are preserved. Directories can be sent with `-r`, or via tar for compression:

```
laptop$ gritty send -r mydir                 # recursive (preserves directory structure)
# or with tar for compression:
devbox$ tar czf - mydir | gritty send -
laptop$ gritty receive - | tar xzf -
```

## Session environment

**Set inside sessions:** `GRITTY_SOCK` (svc socket for `gritty open`/`send`/`receive`/port forwarding), `GRITTY_SESSION` (session ID), `GRITTY_SESSION_NAME` (if named), and `GRITTY_CLIENT` (the creating client's namespace prefix) are set in the shell environment. Useful for prompt customization or scripts that need to know which session they're in. `BROWSER` points at the `gritty-open` helper (URL forwarding), and `SSH_AUTH_SOCK` points at the session's agent socket.

**Agent socket without `-A`:** `SSH_AUTH_SOCK` is always exported, but the underlying socket only has a listener while an `-A` client is attached. Without agent forwarding, a tool connecting to it (e.g. `ssh-add -l`) gets "cannot connect to agent" (exit 2) -- so a login script that checks whether an agent is reachable will correctly conclude there is none and can start its own. Probe reachability (`ssh-add -l`; exit 2 means no agent) rather than mere presence of `SSH_AUTH_SOCK`.

**Forwarded to sessions:** `TERM`, `COLORTERM`, `LANG`, and the `LC_*` locale categories are carried from the client to the session's login shell (mirroring SSH's default `SendEnv LANG LC_*`), so a remote session renders UTF-8 correctly even when the remote daemon's own environment lacks your locale.

## Port forwarding

`gritty lf [target] <port>` and `gritty rf [target] <port>` where `<target>` is a `host:session` specifier (e.g. `devbox:work`). Port spec is `PORT` (same on both ends) or `LISTEN_PORT:TARGET_PORT` (the first number is always the new listening port, the second the existing service).

The commands are named for where the *service* lives, which is the opposite of SSH's listen-side `-L`/`-R` convention: `rf` brings a **r**emote (session-side) port to the client (`gritty rf 3000`, then browse `localhost:3000` -- the common case, SSH's `-L`), and `lf` makes a **l**ocal (client-side) port reachable inside the session (`gritty lf 5432` to let the session reach local postgres -- SSH's `-R`).

`<target>` may be omitted when exactly one session is attached from this machine: `gritty rf 3000` forwards to it. With zero attached sessions the command errors (forwards need an attached `gritty connect` client -- always run `lf`/`rf` on the client machine, not inside the session); with several, it lists them and asks for an explicit target.

Port forwards are client-initiated -- they communicate with the client process through a local forward socket, and the client sends `PortForwardRequest` frames to the server. A compromised server cannot initiate port forwards. All forwarding binds to `127.0.0.1` only -- there is no bind-address option (unlike SSH's `-L`/`-R`).

Forwards survive disconnects the way sessions do: when the attached client drops (network blip, detach, takeover), the `lf`/`rf` process re-places the forward automatically as soon as a client is attached again, retrying with backoff until then. Ctrl-C stops the forward -- that's the only thing that does. A rejection on the first attempt (e.g. the listen port is busy) is still an immediate error.

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
# linger = "never"               # how long a session survives with no client
                                  # attached before the server reaps it; applies
                                  # to sessions you explicitly named (`host:foo`).
                                  # Accepts e.g. "30m", "1h", "7d", or "never".
# linger-unnamed = "never"       # same, for sessions where you omitted the name
                                  # (`host` -> auto-numbered slot). Setting just
                                  # this key gives throwaway shells a fuse while
                                  # named ones stay permanent. Precedence:
                                  # host.linger-unnamed > host.linger >
                                  # defaults.linger-unnamed > defaults.linger --
                                  # so `[host.prod] linger = "never"` shields prod's
                                  # unnamed sessions too.
# client-name = "my-laptop"      # prefix applied to session names; default: hostname.
                                  # Sessions you create are named <client-name>/<short>
                                  # on the wire so multi-laptop clients don't collide.
                                  # Must be non-empty and contain no `/`, whitespace,
                                  # or control characters (invalid -> falls back to
                                  # "unknown"). Use `gritty connect host:foo/bar` to
                                  # bypass prefixing (foreign or shared sessions).

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
[host.devbox]
# Alternate names: typing an alias as the host part of any target resolves to
# this connection name, so `gritty c devbox.example.com:work` and
# `gritty c devbox:work` address the same tunnel (and this config section).
# The FIRST alias is also used as the SSH destination when no tunnel exists
# yet, so `gritty c devbox` cold-starts without a prior tunnel-create or an
# ~/.ssh/config entry.
aliases = ["devbox.example.com"]

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

**Aliases:** real names always win -- `local` and exact `[host.<name>]` keys are never remapped, and an alias claimed by more than one host warns and is used literally. `aliases` is only meaningful under `[host.<name>]`, not `[defaults]`. A live tunnel's recorded destination (from `tunnel-create`) takes priority over the first-alias destination, so an explicit `user@host:port` is preserved across restarts.

`ring-buffer-size` and `oauth-tunnel-idle-timeout` are resolved by the daemon, which has no per-connection context -- they take effect only from `[defaults]`, not `[host.<name>]`.

`linger`/`linger-unnamed` are resolved client-side and sent at session creation; they take effect from `[host.<name>]` and `[defaults]`. When a session is reaped, the shell's process group gets `SIGHUP` -- the same as closing an ssh window -- so anything started under `nohup`/`disown`/`setsid` survives.

A missing config file uses built-in defaults. A malformed config file (a typo'd key or section -- keys are kebab-case) is rejected in full and the built-in defaults are used; `gritty info` and `gritty doctor` both report it as invalid.

## Escape Sequences

After a newline (or at session start), `~` enters escape mode:

| Sequence | Action |
|----------|--------|
| `~.` | Detach from session (clean exit, no auto-reconnect) |
| `~R` | Force reconnect |
| `~K` | Pin this session: set its linger to `never` so it's never auto-reaped |
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

## Scripting (`--json`)

`ls`, `tunnels`, `info`, and `doctor` accept `--json` for machine-readable output -- status bars, prompts, and scripts should parse this instead of the human tables. `gritty ls --json` emits an array of host groups, each with `hosts` (name, destination, tunnel_status), an `error` (probe failure) or a `sessions` array (`id`, `name` = wire name to pass back to gritty, `display_name` = prefix-elided form shown in tables, `attached`, `idle_secs`, `foreground_cmd`, `cwd`, ...). Fields are append-only: new keys may appear, existing ones won't be renamed or removed.

```
laptop$ gritty ls --json | jq -r '.[].sessions[] | select(.attached | not) | .name'
laptop$ gritty tunnels --json | jq -r '.[] | select(.status != "healthy") | .name'
laptop$ gritty doctor --json | jq '.failures'
```

## Man Pages

`gritty mangen <dir>` writes roff man pages -- `gritty.1` plus one page per subcommand -- into `<dir>`. Intended for packagers; locally, `gritty mangen ~/.local/share/man/man1` puts them on most default `MANPATH`s.

## Debugging

`gritty doctor` is the first stop: it prints the key paths (config file, socket dir, logs, device id) and checks for stale processes, orphaned sockets, and config errors. It also flags any file in the socket dir that this gritty version doesn't recognize -- litter from a release whose artifact set differed; `gritty doctor --clean` removes such files (never directories or sockets something is actively serving). `gritty info` prints the same paths plus live server/tunnel status.

**Asking an LLM for help:** `gritty doctor --llm "describe what's going wrong"` prints a self-contained diagnostic report -- a primer on gritty's architecture and known failure modes, your description, doctor's checks, session/tunnel state, and sanitized excerpts from the daemon and tunnel logs -- formatted to paste into a chat or pipe into an LLM CLI:

```
laptop$ gritty doctor --llm "sessions to devbox drop every few minutes" | claude -p
```

gritty never contacts an LLM itself; it only produces the report. Review before sharing: it contains hostnames, paths, session and command names, and log lines. The description is optional (`gritty doctor --llm` reports general health) and `--log-lines <N>` adjusts how much of each log is included. The report covers this machine only -- for a suspect remote host, run the same command there over ssh.

**Log levels:** log files (daemon, tunnel) default to `info`; client commands logging to the terminal default to `warn` so routine telemetry stays out of interactive output (`server -f` and `tunnel-create -f` keep `info` on stderr -- foreground is a diagnostic mode). `-v` enables debug logging. `RUST_LOG=gritty=trace` enables the most verbose output (protocol-level frame tracing, alt-screen state machine transitions). `RUST_LOG=gritty::server=debug,gritty=info` enables debug logging for the server module only.

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

**Forcing a reconnect attempt:** While the `reconnecting Ns` or `waiting for network` status line is showing, any key skips the rest of the backoff sleep and attempts immediately -- the line flips to `retrying now` when the attempt starts. This also overrides `waiting for network`, which trusts the OS path monitor and can lag reality after a wake-from-sleep; if you know the network is back, hit a key. Forced attempts never escalate the retry backoff, so tapping a key cannot delay the automatic schedule. Keys only become retry commands once the outage has been visible for a second -- anything typed during a blip too brief to show the status line is delivered to the session after it resumes. `^C` exits instead of retrying.

**Log file locations:** `gritty doctor` and `gritty info` both show the paths to `daemon.log` and `daemon.out`. `doctor` also warns if a log grows past 50 MB.

**Protocol version mismatch after upgrade:** pre-1.0, any gritty release may bump the wire protocol; mismatched sides refuse to talk rather than misbehave, and `gritty refresh` is the one-command recovery. Concretely: after upgrading gritty on one side the other side's daemon still speaks the old protocol, so session operations fail with `protocol version mismatch`. Upgrade both binaries to matching versions (`gritty protocol-version` prints the local version), then run `gritty refresh` to restart whatever is stale -- the local daemon, each tunnel supervisor, and (over SSH) each remote daemon. `refresh` is idempotent: it reads each process's `.info` sidecar and only restarts what is actually behind the binary on disk, so a second run is a no-op. For remote hosts, `refresh` finishes with an end-to-end Hello/HelloAck probe through the tunnel: if the remote daemon still speaks a different protocol after the per-process checks pass, the remote *binary* itself is a different release -- refresh fails with instructions to run `gritty bootstrap <host>` and then `gritty refresh <host>` again. `gritty restart <host>` is the scorched-earth variant that restarts unconditionally; both tolerate the mismatched handshake so neither falls back to raw SSH. `gritty doctor` shows which processes are stale without touching anything.

**Orphaned daemons (sessions vanish, stray `gritty server` processes):** systemd wipes `/run/user/<uid>` when your last login session on a host ends (and `/tmp` gets age-based sweeps), which deletes the socket directory out from under a running daemon. Current daemons self-heal: the daemon notices within a few seconds, re-binds its socket, and rewrites its registration -- sessions survive. When recovery is impossible (the directory cannot be recreated, or a newer daemon already took the path), the daemon shuts down cleanly instead of lingering as an unreachable orphan. Daemons from older gritty releases can do neither and keep running invisibly; `gritty doctor` reports them (it cross-checks the process table against on-disk registrations, Linux only) and `gritty refresh` reaps them after a grace period that lets a self-healing daemon recover. To prevent the wipe in the first place on hosts you only reach over SSH, enable lingering: `loginctl enable-linger`.

See [docs/tunnel-state-machine.md](docs/tunnel-state-machine.md) for the `healthy` / `reconnecting` / `stale` tunnel state definitions and the full supervisor state diagram.
