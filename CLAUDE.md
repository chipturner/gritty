# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

## What is gritty

Persistent TTY sessions over Unix domain sockets. Single binary, tmux-like CLI. Similar to Eternal Terminal but socket-based. Sessions survive client disconnect; a background server manages multiple sessions over a single socket.

### Commands

| Command | Alias | Description |
|---------|-------|-------------|
| `connect [host[:name]]` | `c` | Smart session: attach if exists, create if not |
| `list-sessions [host]` | `ls`, `list` | List sessions (all servers if no host) |
| `tail [host:session]` | `t` | Read-only stream of session output |
| `kill-session [host:session]` | | Kill a session |
| `rename <host:session> <name>` | | Rename a session |
| `kill-server [host]` | | Kill the server and all sessions |
| `tunnels` | `tun` | List active SSH tunnels |
| `tunnel-create <destination>` | | SSH tunnel to remote host |
| `tunnel-destroy <name>` | | Tear down SSH tunnel |
| `bootstrap <destination>` | | Install gritty on a remote host |
| `local-forward <port>` | `lf` | Forward TCP port: session to client |
| `remote-forward <port>` | `rf` | Forward TCP port: client to session |
| `send [files...]` | | Send files to a paired receiver (`-r` for directories) |
| `receive [dir]` | | Receive files from a paired sender |
| `copy` | | Copy stdin to the client clipboard |
| `paste` | | Paste client clipboard to stdout |
| `open <url>` | | Open a URL on the local machine (for use inside gritty sessions) |
| `info` | | Show diagnostics |
| `config-edit` | | Open config in `$VISUAL`/`$EDITOR`/vi |
| `server` | `s` | Start the server (backgrounds by default, `-f` for foreground) |
| `completions <shell>` | | Generate shell completions |
| `socket-path` | `socket` | Print the default socket path |
| `protocol-version` | | Print the protocol version number |

Sessions have auto-incrementing IDs with optional names (`host:name`). Numeric names rejected. `-` = last-attached session. `<host>` is `local` or a connection name from `tunnel-create`. Session name defaults to `default` if omitted. Global: `--ctl-socket <path>`, `-v`. See `--help` for per-command flags.

## Build & Test

Rust edition 2024, MSRV 1.94. Uses `just` as the task runner. Tests via `cargo-nextest` (concurrency in `.config/nextest.toml`).

```bash
just check                           # clippy + full test suite (pre-push gate)
just fmt                             # format all source files
just test                            # all tests (pass args to filter: just test session)
just test-protocol                   # codec unit tests only
just test-daemon                     # daemon integration tests only
just test-e2e                        # e2e session tests only
just test-container                   # container tests (lifecycle + SSH tunnel + features)
just test-socat                      # socat tunnel disruption tests (requires socat)
just test-socat-bridge               # socat bridge tests (requires socat)
just stress 10                       # run full suite N times, report pass/fail tally
just coverage                        # test coverage summary
just coverage-html                   # HTML coverage report
```

```bash
cargo run -- server                   # start server (self-backgrounds, prints PID)
cargo run -- connect local:myproject  # create or attach to named session
cargo run -- connect local            # create or attach to "default" session
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
- **`connect`** (module, implements `tunnel-create` CLI) -- Self-backgrounding SSH tunnel. Monitor respawns on transient failure (backoff 1s to 60s, resets after 30s healthy). Per-tunnel files: `.sock`, `.pid`, `.lock`, `.dest`, `.log`, `.out`. `ConnectGuard` Drop cleans up.
- **`table`** -- `print_table()` for tabular output.
- **`client`** -- Raw mode, escape processor, heartbeat (5s ping / 15s timeout), auto-reconnect, forwarding relay. `tail()` is read-only variant.

### Wire format

Handshake: `0x01` Hello, `0x02` HelloAck. Relay: `0x10` Data, `0x11` Resize, `0x12` Exit, `0x13` Detached, `0x14` Ping, `0x15` Pong, `0x16` Env. Agent: `0x20` AgentForward, `0x21` AgentOpen, `0x22` AgentData, `0x23` AgentClose. URL/clipboard: `0x28` OpenForward, `0x29` OpenUrl, `0x2A` ClipboardSet, `0x2B` ClipboardGet, `0x2C` ClipboardData. Tunnel: `0x30` TunnelListen, `0x31` TunnelOpen, `0x32` TunnelData, `0x33` TunnelClose. Transfer: `0x38` SendOffer, `0x39` SendDone, `0x3A` SendCancel, `0x3B` SendFile. Port forward: `0x40` PFListen, `0x41` PFReady, `0x42` PFOpen, `0x43` PFData, `0x44` PFClose, `0x45` PFStop. Control: `0x50` NewSession, `0x51` Attach, `0x52` ListSessions, `0x53` KillSession, `0x54` KillServer, `0x55` Tail, `0x56` RenameSession. Responses: `0x60` SessionCreated, `0x61` SessionInfo, `0x62` Ok, `0x63` Error. Reserved: `0x80-0xFF`.

`Hello`/`HelloAck`: `[version: u16][capabilities: u32]`. Capabilities bitfield, negotiated = client & server (bitwise AND). Defined bits: `CAP_CLIPBOARD (0x01)` -- gates clipboard frame forwarding and svc socket clipboard requests.

`NewSession`: `[name_len: u16][name][cmd_len: u16][cmd][cwd_len: u16][cwd][cols: u16][rows: u16][client_name_len: u16][client_name]`. Empty cwd = `$HOME`. Zero cols/rows = default 80x24. `client_name` propagated to session metadata.

`Attach`: `[session_len: u16][session][client_name_len: u16][client_name][force: u8]`. Server enforces: if attached and `!force`, returns `AlreadyAttached` error.

`SessionCreated`: `[id: u32]`.

`Error`: `[code: u16][message: remaining bytes]`. `ErrorCode`: `NoSuchSession(1)`, `NameAlreadyExists(2)`, `InvalidName(3)`, `EmptyName(4)`, `VersionMismatch(5)`, `UnexpectedFrame(6)`, `AlreadyAttached(7)`, `Unknown(u16)`.

`SessionInfo`: `[count: u32][per entry: [entry_len: u32][id: u32][name: u16-len + bytes][pty_path: u16-len + bytes][shell_pid: u32][created_at: u64][attached: u8][last_heartbeat: u64][foreground_cmd: u16-len + bytes][cwd: u16-len + bytes][client_name: u16-len + bytes]]`. Decoder skips unknown trailing bytes within each entry_len.

`SvcRequest`: `OpenUrl=1`, `Send=2`, `Receive=3`, `PortForward=4`, `Clipboard=5` (1-byte discriminator). Clipboard sub-protocol: `[0x01][data]` = copy, `[0x02]` = paste (server responds with clipboard content).

File transfer manifest (svc socket, not Frame protocol): sender writes `[file_count: u32][per file: [name_len: u16][name: bytes][size: u64][mode: u32]]`. Server relays per-file headers `[name_len: u16][name: bytes][size: u64][mode: u32]` to receiver, then `size` bytes of data. Sentinel `[name_len: 0x0000]` ends transfer. `-` (stdin) spools to a temp file for size discovery.

## Key Patterns

- **Connection handoff**: Daemon transfers `Framed<UnixStream>` to session task via `mpsc`. Daemon exits the data path.
- **AsyncFd + try_io**: PTY master and stdin are raw fds in `AsyncFd`. `guard.try_io()` with would-block continuation.
- **Deferred shell spawn**: PTY allocated early (with initial window size from `NewSession` cols/rows when > 0), shell waits for first client's `Env` frame (TERM/LANG/COLORTERM). Spawns login shell with CWD from `NewSession` (or `$HOME` if empty). First client feeds directly into relay (no outer-loop re-wait).
- **Ring buffer**: Client disconnect breaks inner relay; outer loop drains PTY into `VecDeque<Bytes>` (default 1MB). On reconnect, dropped-bytes marker if overflow, then flush.
- **Client takeover**: `client_rx.recv()` in relay select. New client causes `Detached` to old, then switch. Capability check (500ms deadline) warns if reconnecting client is missing `-A`/`-O` that the session expects.
- **Self-daemonizing**: Fork before tokio runtime. Parent waits on pipe for readiness. PID file at `socket_dir()/daemon.pid`.
- **Lockfile-based liveness**: `flock()` on `connect-{name}.lock`. Non-blocking probe distinguishes live vs dead tunnels.
- **Multi-channel forwarding**: Agent, tunnel, and port forwarding use `channel_id: u32` + `spawn_channel_relay<R, W>()`. State cleared on disconnect/takeover.
- **Terminal guards**: `RawModeGuard` + `NonBlockGuard`. Drop order matters: `NonBlockGuard` must outlive `AsyncFd`.
- **Auto-start**: `connect` auto-starts server on failure (`local` runs `gritty server`, others run `gritty tunnel-create`). Other commands fail immediately.
- **Host routing**: `parse_target()` splits `host:session`. `resolve_ctl_path()`: `--ctl-socket` > `"local"` > connect socket. `"local"` reserved keyword.
- **Escape sequences**: `~.` detach, `~R` reconnect, `~#` status, `~^Z` suspend, `~?` help, `~~` literal. 3-state machine (Normal/AfterNewline/AfterTilde). `--no-escape` disables.
- **Security**: `umask(0o077)`, sockets 0600, dirs 0700, `SO_PEERCRED` on all accepts, payloads <= 1MB, resize 1..=10000.
- **URL/OAuth**: Client calls `opener::open()`. OAuth tunnel: multi-channel reverse TCP with idle timeout (default 5s, configurable). Disable with `--no-oauth-redirect`.
- **BROWSER setup**: Server creates a `gritty-open` symlink (pointing to `current_exe()`) in the socket dir unconditionally at shell spawn and sets `BROWSER` to that path. The binary detects `argv[0] == "gritty-open"` and dispatches directly to the open logic, so `$BROWSER` is a single path with no spaces.
- **Capability negotiation**: `Hello` and `HelloAck` carry a `capabilities: u32` bitfield. Negotiated capabilities = client & server (bitwise AND). `CAP_CLIPBOARD (0x01)` gates clipboard frame forwarding. Capabilities propagate from daemon `connection_handshake()` through `ClientConn::Active` to the session server, refreshed on each reconnect/takeover. Clipboard paste has a 5-second timeout -- if the client doesn't reply with `ClipboardData`, the pending paste is resolved with `None`.
- **Port forwarding is loopback-only**: All `TcpListener::bind` and `TcpStream::connect` in forwarding use `127.0.0.1`. No bind-address specification (unlike SSH `-L`/`-R`).

## Development Notes

### Critical invariants
- **`security` module is load-bearing** -- never use `UnixListener::bind` or `create_dir_all` directly.
- **Reap before lookup** -- `reap_sessions()` MUST precede Attach/KillSession/ListSessions. Stale sessions cause silent failures.
- **Channel closed check** -- before `Frame::Ok` for Attach, check `client_tx.is_closed()` (session died between reap and lookup).
- **`Stdio::from(OwnedFd)`** -- don't reintroduce `FromRawFd` in server.rs.
- **Fork before tokio** -- `daemonize()` MUST fork before creating the tokio runtime. `main()` is sync (no `#[tokio::main]`).

### Changing protocol/signatures
- **`PROTOCOL_VERSION`** -- bump whenever frame types, encoding, or `SessionEntry` fields change. Version mismatch is a hard gate: daemon rejects clients, `tunnel-create` aborts tunnel setup. Currently v9.
- **`expect_min_len`** -- all fixed-field decoders use `expect_min_len` (not exact length checks), so trailing bytes are tolerated for forward extensibility.
- **`Frame` enum** -- update: encoder, decoder, protocol tests, all `match frame` in server.rs, client.rs, daemon.rs, main.rs.
- **`SessionInfo`** -- entry count `u32`. Changing `SessionEntry` fields requires updating both encoder and decoder in protocol.rs.
- **`server::run()`** -- takes `(client_rx, metadata, agent_path, svc_path, session_id, session_name, command, ring_buffer_cap, oauth_tunnel_idle_timeout, initial_cols, initial_rows, cwd)`. Called by e2e tests + daemon; update both.
- **`ClientConn::Active`** -- struct variant `Active { framed, client_name, capabilities }`. `client_name` propagated from `Attach`/`NewSession` frame to session metadata. `capabilities` is the negotiated bitfield from handshake.
- **`ErrorCode`** -- `Frame::Error` carries a `code: ErrorCode` enum + `message: String`. Match on code for programmatic error handling, display message for humans.

### Testing
- **E2e**: `UnixStream::pair()` + channel to `server::run()`. No socket files.
- **Daemon**: real socket in `tempfile::tempdir()`. `do_handshake()` + `wait_for_daemon()`.
- **Nextest**: e2e + daemon capped at 2 concurrent; socat/SSH serial; 2 retries for flaky tests. Per-process isolation.
- **SSH/socat**: auto-detect availability, skip gracefully. `GRITTY_SSH_TEST=0` to force-skip.

### Workflow
- Run `just fmt` after making code changes.
- Run `just check` (clippy + full test suite) before finishing work.
- When changing code, update docs **in the same commit**. Files to check:
  - **README.md** -- command table, flags, session env vars, config defaults, escape sequences
  - **CLAUDE.md** -- module descriptions, wire format codes, `server::run()` signature, key patterns
  - **ARCHITECTURE.md** -- high-level feature descriptions, diagrams

### Style
- `main()` returns `()`. Errors via `eprintln!("error: ...")`. Never `-> anyhow::Result` on `main()`.
