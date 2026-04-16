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
| `kill-server [host]` | | Kill the server and all sessions (tolerates version mismatch) |
| `restart [host]` | | Kill + restart server (and tunnel, for remote hosts) -- upgrade recovery |
| `tunnels` | `tun` | List active SSH tunnels |
| `tunnel-create <destination>` | | SSH tunnel to remote host |
| `tunnel-destroy <name>` | | Tear down SSH tunnel |
| `bootstrap <destination>` | | Install gritty on a remote host |
| `local-forward <target> <port>` | `lf` | Forward TCP port: session to client |
| `remote-forward <target> <port>` | `rf` | Forward TCP port: client to session |
| `send [files...]` | | Send files to a paired receiver (`-r` for directories) |
| `receive [dir]` | | Receive files from a paired sender |
| `copy` | | Copy stdin to the client clipboard |
| `paste` | | Paste client clipboard to stdout |
| `open <url>` | | Open a URL on the local machine (for use inside gritty sessions) |
| `info` | | Show diagnostics |
| `config` | | Open config in `$VISUAL`/`$EDITOR`/vi |
| `doctor` | | Check for common issues |
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

Nine modules behind a lib crate (`src/lib.rs` hosts `collect_env_vars()`, `spawn_channel_relay()`, `handshake()`, `get_or_create_device_id()`) with thin binary entry (`src/main.rs`):

- **`security`** -- Socket/dir creation with 0700/0600 perms, ownership validation, symlink rejection, `SO_PEERCRED`. **All socket binding and dir creation MUST go through this module.**
- **`config`** -- TOML config (`$XDG_CONFIG_HOME/gritty/config.toml`). `[defaults]` + `[host.<name>]`. Precedence: CLI > host > defaults > built-in.
- **`protocol`** -- `Frame` enum, `Encoder`/`Decoder`, wire `[type: u8][length: u32 BE][payload]`. `PROTOCOL_VERSION: u16`. `SessionEntry` for list metadata. `SvcRequest` enum for svc socket dispatch.
- **`daemon`** -- Accept loop on `ctl.sock`. Handshake, control frame, route. `HashMap<u32, SessionState>`. Hands off `Framed<UnixStream>` to session tasks via `mpsc`.
- **`server`** -- Per-session: PTY, client relay, ring buffer, forwarding (agent/URL/tunnel/port), file transfer, tail broadcast. Per-session sockets: `agent-{id}.sock` + `svc-{id}.sock`. Client-side forward socket: `fwd-{host}-{id}.sock` (created by the client, used by `gritty lf`/`gritty rf` to request port forwards).
- **`connect`** (module, implements `tunnel-create` CLI) -- Self-backgrounding SSH tunnel. Monitor respawns on transient failure (backoff 1s to 60s, resets after 30s healthy); respawn preserves the original `foreground` flag and re-runs `ensure_remote_ready` so a rebooted remote gets its `gritty server` started again before SSH forwards bytes. Per-tunnel files: `.sock`, `.pid` (written immediately on lock acquisition), `.lock`, `.dest`, `.log`, `.out`. `ConnectGuard` Drop cleans up.
- **`alt_screen`** -- `AltScreenTracker`: byte-scanning state machine that detects alternate screen mode (`?1049`, `?1047`, `?47`). Used by server for smart reconnect.
- **`scrollback`** -- `ScrollbackBuffer`: tracks last 50 lines of PTY output for replay on main-screen reconnect.
- **`table`** -- `print_table()` for tabular output.
- **`logging`** -- Tracing subscriber setup with `reload::Layer` for runtime log-level switching (SIGUSR1 cycles info/debug/trace) and `ReopenableWriter` for log-file rotation (SIGUSR2 reopens the file). `init_tracing()` configures file output (daemon mode) or stderr (foreground/client).
- **`client`** -- Raw mode, escape processor, idle heartbeat (Ping fires when the client has sent nothing for `heartbeat_interval` / 10s default; 60s idle-timeout off inbound), clock-skew suspend detection with 5s post-resume probe deadline, auto-reconnect (5s attempt timeout), forwarding relay. Ping cadence keys off `last_outbound_at` (not `last_activity`) so steady inbound server output doesn't suppress probes -- the server uses client frames as liveness for its idle-evict, and inbound data doesn't prove the client can still send. `tail()` is read-only variant.

### Wire format

Handshake: `0x01` Hello, `0x02` HelloAck. Relay: `0x10` Data, `0x11` Resize, `0x12` Exit, `0x13` Detached, `0x14` Ping, `0x15` Pong, `0x16` Env, `0x17` DiagRequest, `0x18` DiagResponse. Agent: `0x20` AgentForward, `0x21` AgentOpen, `0x22` AgentData, `0x23` AgentClose. URL/clipboard: `0x28` OpenForward, `0x29` OpenUrl, `0x2A` ClipboardSet, `0x2B` ClipboardGet, `0x2C` ClipboardData. Tunnel: `0x30` TunnelListen, `0x31` TunnelOpen, `0x32` TunnelData, `0x33` TunnelClose. Transfer: `0x38` SendOffer, `0x39` SendDone, `0x3A` SendCancel, `0x3B` SendFile. Port forward: `0x40` PFListen, `0x41` PFReady, `0x42` PFOpen, `0x43` PFData, `0x44` PFClose, `0x45` PFStop, `0x46` PortForwardRequest. Control: `0x50` NewSession, `0x51` Attach, `0x52` ListSessions, `0x53` KillSession, `0x54` KillServer, `0x55` Tail, `0x56` RenameSession. Responses: `0x60` SessionCreated, `0x61` SessionInfo, `0x62` Ok, `0x63` Error, `0x64` AttachAck. Reserved: `0x80-0xFF`.

`Hello`: `[version: u16][capabilities: u32][device_id: u64]`. `HelloAck`: `[version: u16][capabilities: u32][server_id: u64]`. `device_id` is a persistent per-machine identifier stored in `$XDG_STATE_HOME/gritty/device_id`; the server records it as the session owner for auto-reconnect validation. Capabilities bitfield, negotiated = client & server (bitwise AND). Defined bits: `CAP_CLIPBOARD (0x01)` -- gates clipboard frame forwarding and svc socket clipboard requests. `server_id` is an ephemeral daemon identifier picked at startup; a reconnecting client that observes a different value exits with "server restarted -- session is gone" instead of looping. **Version mismatch is NOT a handshake error:** since v15, the daemon always replies with `HelloAck` carrying its own version even when the client's version differs, and the client decides via `require_matched_version()` whether to proceed. Under a mismatch the daemon gates the next control frame so only `KillServer` is honored (returning `Frame::Ok`); anything else gets `ErrorCode::VersionMismatch` with a message pointing at `gritty restart`. This is the recovery path for upgrading one side -- `kill-server` and `restart` both use `server_request_any_version` while every normal command uses `server_request` which bails on mismatch.

`NewSession`: `[name_len: u16][name][cmd_len: u16][cmd][cwd_len: u16][cwd][cols: u16][rows: u16][client_name_len: u16][client_name]`. Empty cwd = `$HOME`. Zero cols/rows = default 80x24. `client_name` propagated to session metadata.

`Attach`: `[session_len: u16][session][client_name_len: u16][client_name][force: u8][no_replay: u8][cols: u16][rows: u16][attach_token: u64]`. `attach_token` is an ownership claim flag: `0` = explicit connect (no ownership check; server adopts the Hello's `device_id` as new owner), non-zero = auto-reconnect (server compares Hello's `device_id` against stored `owner_device_id`; mismatch → `OwnerChanged`). Server enforces: if attached and `!force`, returns `AlreadyAttached` error. `no_replay` = existence probe only (daemon replies `Ok` without session handoff). `cols`/`rows` are the client's current terminal size, applied to the PTY before reconnect replay so regenerated prompts and TUI repaints use the right winsize (0 = unknown).

`SessionCreated`: `[id: u32]`. Immediately followed by `AttachAck` on the same framed connection -- the creator auto-attaches.

`AttachAck`: `[token: u64][session_id: u32]`. Reply to a successful `Attach` (or auto-attach after `NewSession`). `token` echoes the `device_id` (client ignores it -- ownership is tracked by the persistent device_id, not an ephemeral token). The `session_id` lets the client use the authoritative numeric id (even when the user passed `-` or a name) for subsequent reconnect/tail/fwd-socket operations, avoiding client-side races resolving `-` via `ListSessions`.

`Error`: `[code: u16][message: remaining bytes]`. `ErrorCode`: `NoSuchSession(1)`, `NameAlreadyExists(2)`, `InvalidName(3)`, `EmptyName(4)`, `VersionMismatch(5)`, `UnexpectedFrame(6)`, `AlreadyAttached(7)`, `OwnerChanged(8)`, `Unknown(u16)`.

`SessionInfo`: `[count: u32][per entry: [entry_len: u32][id: u32][name: u16-len + bytes][pty_path: u16-len + bytes][shell_pid: u32][created_at: u64][attached: u8][last_heartbeat: u64][foreground_cmd: u16-len + bytes][cwd: u16-len + bytes][client_name: u16-len + bytes][agent_forwarding_active: u8][is_last_attached: u8]]`. Decoder skips unknown trailing bytes within each entry_len; new fields default gracefully when absent (older servers).

`SvcRequest`: `OpenUrl=1`, `Send=2`, `Receive=3`, `Clipboard=5` (1-byte discriminator). Clipboard sub-protocol: `[0x01][data]` = copy, `[0x02]` = paste (server responds with clipboard content).

`PortForwardRequest`: `[forward_id: u32][direction: u8][listen_port: u16][target_port: u16]`. Client sends to server. Direction `0` = local-forward (server listens), `1` = remote-forward (client listens).

File transfer manifest (svc socket, not Frame protocol): sender writes `[file_count: u32][per file: [name_len: u16][name: bytes][size: u64][mode: u32]]`. Server relays per-file headers `[name_len: u16][name: bytes][size: u64][mode: u32]` to receiver, then `size` bytes of data. Sentinel `[name_len: 0x0000]` ends transfer. `-` (stdin) spools to a temp file for size discovery.

`DiagRequest`: empty payload (client → server, during active session). `DiagResponse`: `[text: remaining bytes]` (server → client). Client sends DiagRequest on `~#`; server replies with session diagnostics (ring buffer stats, alt screen state, channel counts, shell PID).

## Key Patterns

- **Connection handoff**: Daemon transfers `Framed<UnixStream>` to session task via `mpsc`. Daemon exits the data path.
- **AsyncFd + try_io**: PTY master and stdin are raw fds in `AsyncFd`. `guard.try_io()` with would-block continuation.
- **Deferred shell spawn**: PTY allocated early (with initial window size from `NewSession` cols/rows when > 0), shell waits for first client's `Env` frame (TERM/LANG/COLORTERM). Spawns login shell with CWD from `NewSession` (or `$HOME` if empty). First client feeds directly into relay (no outer-loop re-wait).
- **Ring buffer**: Client disconnect breaks inner relay; outer loop drains PTY into `VecDeque<Bytes>` (default 1MB). On reconnect, dropped-bytes marker if overflow, then flush.
- **Smart reconnect**: Server tracks alternate screen mode (`\x1b[?1049h`/`l`, `?47`, `?1047`) via `AltScreenTracker` in `alt_screen.rs`. The `Attach` frame carries the client's current `cols`/`rows`, which the server applies to the PTY (via `TIOCSWINSZ` + `SIGWINCH`) BEFORE any replay, so bytes are regenerated at the right winsize. On reconnect into alternate screen: the server discards the ring buffer (stale alt-screen deltas), sends a priming `\x1b[?1049h\x1b[H\x1b[2J` so a fresh client terminal enters a clean alt-screen, then does a SIGWINCH toggle (rows-1 then rows, each signaled) via `force_tui_redraw()` -- which drains and forwards PTY output between the two ioctls so the TUI doesn't block in `write()` and actually observes the intermediate size. Main screen shows `[gritty: reconnected]` then replays the last 50 lines from `ScrollbackBuffer` via `lines_and_partial()` (which includes the in-progress partial line so the current prompt shows up), followed by any output produced while disconnected.
- **Client takeover**: `client_rx.recv()` in relay select. New client causes `Detached` to old, then switch. Capability check (500ms deadline) warns if reconnecting client is missing `-A`/`-O` that the session expects.
- **Device-based ownership (`device_id`)**: Each machine has a persistent random `u64` stored in `$XDG_STATE_HOME/gritty/device_id` (generated once via `get_or_create_device_id()`). The Hello handshake carries the device_id; the server stores it per session as `owner_device_id` in `SessionMetadata`. Auto-reconnect (inside `client::run()`) sends `attach_token = device_id` (non-zero = ownership claim); the server compares the Hello's `device_id` against `owner_device_id` and rejects with `OwnerChanged` on mismatch. Explicit connect (from `commands/session.rs`) sends `attach_token = 0` (no ownership check; server adopts the new device_id). This prevents a flaky laptop A from silently stealing back a session that laptop B legitimately force-took-over while A was disconnected, and survives client restarts and reboots since the device_id is persistent. Server restart is detected separately via `server_id` in `HelloAck`.
- **Reconnect timings**: `RECONNECT_ATTEMPT_TIMEOUT` (15s) bounds a single connect + handshake + Attach cycle; generous enough for 300ms cellular RTT with retransmits. `SUSPEND_PROBE_DEADLINE` (15s) is the grace window after a detected laptop wake for the server's Pong to arrive before the link is declared dead. Between failed reconnect attempts the client uses `next_reconnect_delay(prev)` exponential backoff 1s..10s (resets on success) -- see `client::tests::reconnect_backoff_*`. Compare to the SSH tunnel supervisor in `connect.rs`, which uses 1s..60s with a 30s healthy reset.
- **Self-daemonizing**: Fork before tokio runtime. Parent waits on pipe for readiness. PID file at `socket_dir()/daemon.pid`.
- **Signal handlers (daemon)**: SIGTERM/SIGINT = shutdown. SIGUSR1 = cycle log level (info -> debug -> trace -> info) without restart. SIGUSR2 = reopen log file (for external logrotate). `kill -USR1 $(cat $SOCKET_DIR/daemon.pid)` to toggle debug logging on a running daemon.
- **Lockfile-based liveness**: `flock()` on `connect-{name}.lock`. Non-blocking probe distinguishes live vs dead tunnels.
- **Multi-channel forwarding**: Agent, tunnel, and port forwarding use `channel_id: u32` + `spawn_channel_relay<R, W>()`. State cleared on disconnect/takeover.
- **Terminal guards**: `RawModeGuard` + `NonBlockGuard`. Drop order matters: `NonBlockGuard` must outlive `AsyncFd`.
- **Auto-start**: `connect` auto-starts server on failure (`local` runs `gritty server`, others run `gritty tunnel-create`). Other commands fail immediately.
- **Host routing**: `parse_target()` splits `host:session`. `resolve_ctl_path()`: `--ctl-socket` > `"local"` > connect socket. `"local"` reserved keyword.
- **Escape sequences**: `~.` detach, `~R` reconnect, `~#` status (client + server diagnostics via `DiagRequest`/`DiagResponse`), `~^Z` suspend, `~?` help, `~~` literal. 3-state machine (Normal/AfterNewline/AfterTilde). `--no-escape` disables.
- **Security**: `umask(0o077)`, sockets 0600, dirs 0700, `SO_PEERCRED` on all accepts, payloads <= 1MB, resize 1..=10000.
- **URL/OAuth**: Client calls `opener::open()`. OAuth tunnel: multi-channel reverse TCP with idle timeout (default 5s, configurable). Disable with `--no-oauth-redirect`.
- **BROWSER setup**: Server creates a `gritty-open` symlink (pointing to `current_exe()`) in the socket dir unconditionally at shell spawn and sets `BROWSER` to that path. The binary detects `argv[0] == "gritty-open"` and dispatches directly to the open logic, so `$BROWSER` is a single path with no spaces.
- **Capability negotiation**: `Hello` and `HelloAck` carry a `capabilities: u32` bitfield. Negotiated capabilities = client & server (bitwise AND). `CAP_CLIPBOARD (0x01)` gates clipboard frame forwarding. Capabilities propagate from daemon `connection_handshake()` through `ClientConn::Active` to the session server, refreshed on each reconnect/takeover. Clipboard paste has a 5-second timeout -- if the client doesn't reply with `ClipboardData`, the pending paste is resolved with `None`.
- **Client-initiated port forwarding**: Port forwards are requested by the client via `PortForwardRequest` frames through the session connection, not through the svc socket. The `lf`/`rf` commands communicate with the client process through a client-side forward socket (`{socket_dir}/fwd-{host}-{id}.sock`). A compromised server cannot initiate port forwards.
- **Port forwarding is loopback-only**: All `TcpListener::bind` and `TcpStream::connect` in forwarding use `127.0.0.1`. No bind-address specification (unlike SSH `-L`/`-R`).
- **Client-side security gates**: URL opening gated by `forward_open` on the client (rate limited 2/30s). Clipboard is push-only -- server can push `ClipboardSet` (rate limited 5/30s) but `ClipboardGet` returns empty. Agent forwarding defaults to off (opt-in via `-A`). `TunnelListen` rate limited 2/30s, `AgentOpen` rate limited 10/30s. Audit logging at info/warn level for all security-sensitive operations.

## Development Notes

### Critical invariants
- **`security` module is load-bearing** -- never use `UnixListener::bind` or `create_dir_all` directly. Client-side connects to ctl/daemon sockets MUST go through `security::connect_verified()` (connect + `SO_PEERCRED` check).
- **Reap before lookup** -- `reap_sessions()` MUST precede Attach/KillSession/ListSessions. Stale sessions cause silent failures.
- **Channel closed check** -- before `Frame::Ok` for Attach, check `client_tx.is_closed()` (session died between reap and lookup).
- **`Stdio::from(OwnedFd)`** -- don't reintroduce `FromRawFd` in server.rs.
- **Fork before tokio** -- `daemonize()` MUST fork before creating the tokio runtime. `main()` is sync (no `#[tokio::main]`).

### Changing protocol/signatures
- **`PROTOCOL_VERSION`** -- bump whenever frame types, encoding, or `SessionEntry` fields change. Currently v19. Version mismatch is **not** a hard gate at the handshake layer: the daemon always sends `HelloAck` with its own version so the client can see it, then rejects any non-`KillServer` follow-up frame with `ErrorCode::VersionMismatch`. This is deliberate -- `kill-server` and `restart` need to work across a mismatched handshake so users can recover without SSH. `tunnel-create --ignore-version-mismatch` still exists for the SSH-level pre-check but its value is mostly superseded by the in-band recovery flow.
- **`expect_min_len`** -- all fixed-field decoders use `expect_min_len` (not exact length checks), so trailing bytes are tolerated for forward extensibility.
- **`Frame` enum** -- update: encoder, decoder, protocol tests, all `match frame` in server.rs, client.rs, daemon.rs, main.rs.
- **`SessionInfo`** -- entry count `u32`. Changing `SessionEntry` fields requires updating both encoder and decoder in protocol.rs.
- **`server::run()`** -- takes `(client_rx, metadata_slot, SessionConfig { agent_socket_path, svc_socket_path, session_id, session_name, command, ring_buffer_cap, oauth_tunnel_idle_timeout, initial_cols, initial_rows, cwd, initial_device_id })`. `SessionConfig` has `Default` (1MB ring, 5s oauth timeout). Called by e2e tests + daemon; update both.
- **`client::run()`** -- takes `(framed, ClientConfig { session, session_id, ctl_path, env_vars, no_escape, forward_agent, forward_open, oauth_redirect, oauth_timeout, heartbeat_interval, heartbeat_timeout, client_name, expected_server_id, device_id })`. `device_id` is the persistent machine identifier from `get_or_create_device_id()`.
- **`ClientConn::Active`** -- struct variant `Active { framed, client_name, capabilities, cols, rows }`. `client_name` propagated from `Attach`/`NewSession` frame to session metadata. `capabilities` is the negotiated bitfield from handshake. `cols`/`rows` carry the reconnecting client's current terminal size (0 = unknown, used for probe-only attaches and NewSession auto-attach) and are applied to the PTY before reconnect replay.
- **`ErrorCode`** -- `Frame::Error` carries a `code: ErrorCode` enum + `message: String`. Match on code for programmatic error handling, display message for humans. `OwnerChanged(8)` is terminal: the client's reconnect loop treats it like `ServerRestarted` and exits without retrying.

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
