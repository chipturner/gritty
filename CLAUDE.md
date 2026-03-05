# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

## What is gritty

Persistent TTY sessions over Unix domain sockets. Single binary, tmux-like CLI. Similar to Eternal Terminal but socket-based. Sessions survive client disconnect; a background server manages multiple sessions over a single socket.

### Commands

| Command | Alias | Description |
|---------|-------|-------------|
| `server` | `s` | Start server (self-daemonizes, `-f` for foreground) |
| `new-session <host[:name]>` | `new` | Create session and auto-attach |
| `attach <host:session>` | `a` | Attach to session (detaches other clients) |
| `tail <host:session>` | `t` | Read-only stream of session output |
| `send <files...>` | | Send files to paired receiver |
| `receive [dir]` | | Receive files from paired sender |
| `open <url>` | | Open URL on local machine (inside `-O` sessions) |
| `local-forward <port>` | `lf` | Forward TCP port: session to client |
| `remote-forward <port>` | `rf` | Forward TCP port: client to session |
| `connect <dest>` | `c` | SSH tunnel to remote server |
| `disconnect <name>` | `dc` | Tear down SSH tunnel |
| `tunnels` | `tun` | List active SSH tunnels |
| `list-sessions [host]` | `ls` | List sessions (all daemons if no host) |
| `kill-session <host:session>` | | Kill a session |
| `rename <host:session> <new>` | | Rename a session |
| `kill-server <host>` | | Kill server and all sessions |
| `info` | | Show diagnostics |
| `config-edit` | | Edit config file |
| `completions <shell>` | | Generate shell completions |

Sessions have auto-incrementing IDs with optional names (`host:name`). Numeric names rejected. `-` = last-attached session. `<host>` is `local` or a connection name from `connect`. Global: `--ctl-socket <path>`, `-v`. See `--help` for per-command flags.

## Build & Test

Rust edition 2024, MSRV 1.85. Uses `just` as the task runner. Tests via `cargo-nextest` (concurrency in `.config/nextest.toml`).

```bash
just check                           # clippy + full test suite (pre-push gate)
just fmt                             # format all source files
just test                            # all tests (pass args to filter: just test session)
just test-protocol                   # codec unit tests only
just test-daemon                     # daemon integration tests only
just test-e2e                        # e2e session tests only
just test-ssh                        # SSH integration tests (auto-detects ssh localhost)
just test-socat                      # socat tunnel disruption tests (requires socat)
just test-socat-bridge               # socat bridge tests (requires socat)
just stress 10                       # run full suite N times, report pass/fail tally
just coverage                        # test coverage summary
just coverage-html                   # HTML coverage report
```

```bash
cargo run -- server                   # start server (self-backgrounds, prints PID)
cargo run -- new local:myproject      # create named session
cargo run -- attach local:myproject   # attach by name
cargo run -- ls local                 # list active sessions
RUST_LOG=debug cargo run -- server -f # debug mode (foreground)
just quicktest                        # manual 3-pane tmux test
```

## Architecture

Single-socket: all communication (control + session relay) through one Unix domain socket per server. Hello/HelloAck version handshake, then control frame declares intent, server routes accordingly.

Eight modules behind a lib crate (`src/lib.rs` hosts `collect_env_vars()`, `spawn_channel_relay()`, `handshake()`) with thin binary entry (`src/main.rs`):

- **`security`** -- Socket/dir creation with 0700/0600 perms, ownership validation, symlink rejection, `SO_PEERCRED`. **All socket binding and dir creation MUST go through this module.**
- **`config`** -- TOML config (`$XDG_CONFIG_HOME/gritty/config.toml`). `[defaults]` + `[host.<name>]`. Precedence: CLI > host > defaults > built-in.
- **`protocol`** -- `Frame` enum, `Encoder`/`Decoder`, wire `[type: u8][length: u32 BE][payload]`. `PROTOCOL_VERSION: u16`. `SessionEntry` for list metadata. `SvcRequest` enum for svc socket dispatch.
- **`daemon`** -- Accept loop on `ctl.sock`. Handshake, control frame, route. `HashMap<u32, SessionState>`. Hands off `Framed<UnixStream>` to session tasks via `mpsc`.
- **`server`** -- Per-session: PTY, client relay, ring buffer, forwarding (agent/URL/tunnel/port), file transfer, tail broadcast. Per-session sockets: `agent-{id}.sock` + `svc-{id}.sock`.
- **`connect`** -- Self-backgrounding SSH tunnel. Monitor respawns on transient failure (backoff 1s to 60s, resets after 30s healthy). Per-tunnel files: `.sock`, `.pid`, `.lock`, `.dest`, `.log`, `.out`. `ConnectGuard` Drop cleans up.
- **`table`** -- `print_table()` for tabular output.
- **`client`** -- Raw mode, escape processor, heartbeat (5s ping / 15s timeout), auto-reconnect, forwarding relay. `tail()` is read-only variant.

### Wire format

Handshake: `0x16` Hello, `0x24` HelloAck. Relay: `0x01` Data, `0x02` Resize, `0x03` Exit, `0x04` Detached, `0x05` Ping, `0x06` Pong, `0x07` Env, `0x08` AgentForward, `0x09` AgentOpen, `0x0A` AgentData, `0x0B` AgentClose, `0x0C` OpenForward, `0x0D` OpenUrl, `0x0E` TunnelListen, `0x0F` TunnelOpen, `0x17` TunnelData, `0x18` TunnelClose, `0x19` SendOffer, `0x1A` SendDone, `0x1B` SendCancel, `0x1C` PortForwardListen, `0x1D` PortForwardReady, `0x1E` PortForwardOpen, `0x1F` PortForwardData, `0x26` PortForwardClose, `0x27` PortForwardStop. Control: `0x10` NewSession, `0x11` Attach, `0x12` ListSessions, `0x13` KillSession, `0x14` KillServer, `0x15` Tail, `0x25` SendFile, `0x28` RenameSession. Responses: `0x20` SessionCreated, `0x21` SessionInfo, `0x22` Ok, `0x23` Error.

`SessionInfo`: `[count: u32][per entry: [id: u16-len + bytes][name: u16-len + bytes][pty_path: u16-len + bytes][shell_pid: u32][created_at: u64][attached: u8][last_heartbeat: u64][foreground_cmd: u16-len + bytes]]`.

`SvcRequest`: `OpenUrl=1`, `Send=2`, `Receive=3`, `PortForward=4` (1-byte discriminator).

## Key Patterns

- **Connection handoff**: Daemon transfers `Framed<UnixStream>` to session task via `mpsc`. Daemon exits the data path.
- **AsyncFd + try_io**: PTY master and stdin are raw fds in `AsyncFd`. `guard.try_io()` with would-block continuation.
- **Deferred shell spawn**: PTY allocated early, shell waits for first client's `Env` frame (TERM/LANG/COLORTERM). Spawns login shell with `CWD=$HOME`. First client feeds directly into relay (no outer-loop re-wait).
- **Ring buffer**: Client disconnect breaks inner relay; outer loop drains PTY into `VecDeque<Bytes>` (default 1MB). On reconnect, dropped-bytes marker if overflow, then flush.
- **Client takeover**: `client_rx.recv()` in relay select. New client causes `Detached` to old, then switch.
- **Self-daemonizing**: Fork before tokio runtime. Parent waits on pipe for readiness. PID file at `socket_dir()/daemon.pid`.
- **Lockfile-based liveness**: `flock()` on `connect-{name}.lock`. Non-blocking probe distinguishes live vs dead tunnels.
- **Multi-channel forwarding**: Agent, tunnel, and port forwarding use `channel_id: u32` + `spawn_channel_relay<R, W>()`. State cleared on disconnect/takeover.
- **Terminal guards**: `RawModeGuard` + `NonBlockGuard`. Drop order matters: `NonBlockGuard` must outlive `AsyncFd`.
- **Auto-start**: `new-session` auto-starts server on failure (`local` runs `gritty server`, others run `gritty connect`). `attach` waits indefinitely instead. Other commands fail immediately.
- **Host routing**: `parse_target()` splits `host:session`. `resolve_ctl_path()`: `--ctl-socket` > `"local"` > connect socket. `"local"` reserved keyword.
- **Escape sequences**: `~.` detach, `~R` reconnect, `~#` status, `~^Z` suspend, `~?` help, `~~` literal. 3-state machine (Normal/AfterNewline/AfterTilde). `--no-escape` disables.
- **Security**: `umask(0o077)`, sockets 0600, dirs 0700, `SO_PEERCRED` on all accepts, payloads <= 1MB, resize 1..=10000.
- **URL/OAuth**: Client calls `opener::open()`. OAuth tunnel: multi-channel reverse TCP with idle timeout (default 5s, configurable). Disable with `--no-oauth-redirect`.

## Development Notes

### Critical invariants
- **`security` module is load-bearing** -- never use `UnixListener::bind` or `create_dir_all` directly.
- **Reap before lookup** -- `reap_sessions()` MUST precede Attach/KillSession/ListSessions. Stale sessions cause silent failures.
- **Channel closed check** -- before `Frame::Ok` for Attach, check `client_tx.is_closed()` (session died between reap and lookup).
- **`Stdio::from(OwnedFd)`** -- don't reintroduce `FromRawFd` in server.rs.
- **Fork before tokio** -- `daemonize()` MUST fork before creating the tokio runtime. `main()` is sync (no `#[tokio::main]`).

### Changing protocol/signatures
- **`Frame` enum** -- update: encoder, decoder, protocol tests, all `match frame` in server.rs, client.rs, daemon.rs, main.rs.
- **`SessionInfo`** -- entry count `u32`. Changing `SessionEntry` fields requires updating both encoder and decoder in protocol.rs.
- **`server::run()`** -- takes `(client_rx, metadata, agent_path, svc_path, session_id, session_name, command, ring_buffer_cap, oauth_tunnel_idle_timeout)`. Called by e2e tests + daemon; update both.

### Testing
- **E2e**: `UnixStream::pair()` + channel to `server::run()`. No socket files.
- **Daemon**: real socket in `tempfile::tempdir()`. `do_handshake()` + `wait_for_daemon()`.
- **Nextest**: e2e + daemon capped at 2 concurrent; socat/SSH serial; 2 retries for flaky tests. Per-process isolation.
- **SSH/socat**: auto-detect availability, skip gracefully. `GRITTY_SSH_TEST=0` to force-skip.

### Style
- `main()` returns `()`. Errors via `eprintln!("error: ...")`. Never `-> anyhow::Result` on `main()`.
