# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

## What is gritty

Persistent TTY sessions over Unix domain sockets. Single binary, tmux-like CLI. Similar to Eternal Terminal but socket-based. Sessions survive client disconnect; a background server manages multiple sessions over a single socket.

The full command table and all flags live in [USAGE.md](USAGE.md). Session addressing essentials:

- Sessions are addressed `host:session`. `<host>` is `local` or a connection name from `tunnel-create`. `-` = last-attached session. Session name defaults to the next free integer slot in your namespace (`0`, `1`, ...) when omitted. Omitted host = `local` (`commands/util.rs::split_optional_target` / `parse_host_or_local`); exceptions: `ls` and `refresh`, where no host means all known hosts.
- **Host aliases** (`config` module): `[host.<name>] aliases` makes alternate spellings canonicalize to one connection name -- every typed host goes through `commands/util.rs::parse_target()` (config-aware; raw core `split_target()`). The first alias is the SSH-destination fallback when no `.dest` sidecar exists. Real names win: `local` and exact `[host.*]` keys never remap.
- **Client-prefixed names** (`naming` module): every short name (no `/`) the user types is silently rewritten to `<client_name>/<name>` on the wire, so two laptops typing `0` end up in distinct sessions. A name containing `/` is taken literally -- the foreign-access / shared-session form. The daemon is oblivious; names are opaque strings to it. `gritty ls` elides your own prefix for readability.
- Purely numeric wire names are rejected by the server (ambiguous with session IDs) -- harmless in practice because the client prefix makes typed `0` resolve to `<client>/0`.

## Build & Test

Rust edition 2024, MSRV 1.94. Uses `just` as the task runner. Tests via `cargo-nextest` (concurrency in `.config/nextest.toml`).

```bash
just check                           # clippy + full test suite (pre-push gate)
just fmt                             # format all source files
just test                            # all tests (pass args to filter: just test session)
just test-protocol                   # codec unit tests only
just test-daemon                     # daemon integration tests only
just test-e2e                        # e2e session tests only
just test-container                  # container tests (lifecycle + SSH tunnel + features)
just test-socat                      # socat tunnel disruption tests (requires socat)
just test-socat-bridge               # socat bridge tests (requires socat)
just stress 10                       # run full suite N times, report pass/fail tally
just coverage                        # test coverage summary
just coverage-html                   # HTML coverage report
```

```bash
cargo run -- server                   # start server (self-backgrounds, prints PID)
cargo run -- connect local:myproject  # create or attach to named session
cargo run -- connect local            # create or attach to session `0`
cargo run -- ls local                 # list active sessions
RUST_LOG=debug cargo run -- server -f # debug mode (foreground)
just quicktest                        # manual 3-pane tmux test
```

## Architecture

Single-socket: all communication (control + session relay) through one Unix domain socket per server. Hello/HelloAck version handshake, then control frame declares intent, server routes accordingly.

Sixteen modules behind a lib crate (`src/lib.rs` hosts shared helpers + `FORWARDED_ENV_KEYS`) with thin binary entry (`src/main.rs`):

| Module | Responsibility |
|--------|----------------|
| `security` | Socket/dir creation (0700/0600), ownership validation, `SO_PEERCRED`. **All socket binding and dir creation MUST go through this module** |
| `config` | TOML config: `[defaults]` + `[host.<name>]`. Precedence: CLI > host > defaults > built-in. Host aliases (`canonical_host`, `alias_destination`) |
| `protocol` | `Frame` enum, encoder/decoder, `PROTOCOL_VERSION`, `IDLE_EVICT_TIMEOUT` contract |
| `daemon` | Accept loop on `ctl.sock`; handshake, route, hand off `Framed<UnixStream>` to session tasks. Periodic socket self-check: re-binds (sessions survive) or exits cleanly if the socket dir is wiped externally |
| `server` | Per-session: PTY, client relay, offset-indexed `History`, forwarding, file transfer, tail |
| `connect` | Self-backgrounding SSH tunnel supervisor (implements `tunnel-create`) |
| `net_watch` | macOS network path-change notifications (advisory; inert stub elsewhere) |
| `alt_screen` | `AltScreenTracker`: detects alternate screen mode for smart reconnect |
| `runinfo` | `.info` sidecars (protocol version + git hash) so `doctor`/`refresh` detect stale processes |
| `procscan` | Process-table scan for orphaned daemons (running but unregistered); Linux only, inert stub elsewhere |
| `scrollback` | Last-50-lines buffer replayed for fresh viewers |
| `table` | `print_table()` for tabular output |
| `logging` | Tracing setup, SIGUSR1 log-level cycling, SIGUSR2 log reopen |
| `naming` | Pure helpers for the client-prefixed session-name rule |
| `client` | Raw mode, escape processor, heartbeat, auto-reconnect, forwarding relay |
| `commands` | CLI command implementations (`session`, `util`, `doctor`, `refresh`, `transfer`) |

Detailed module descriptions, the on-disk state inventory (every socket/sidecar/lock file, its writer, readers, and lifecycle -- keep `doctor.rs::is_known_artifact()` in sync with it), key patterns (reconnect/replay, ownership, locking, forwarding, security gates), and core signatures: **[docs/internals.md](docs/internals.md)**.

Wire format (frame codes, byte layouts, handshake/version-mismatch semantics): **[docs/wire-protocol.md](docs/wire-protocol.md)**.

SSH tunnel supervisor state machine: **[docs/tunnel-state-machine.md](docs/tunnel-state-machine.md)**.

## Development Notes

### Critical invariants
- **`security` module is load-bearing** -- never use `UnixListener::bind` or `create_dir_all` directly. Client-side connects to ctl/daemon sockets MUST go through `security::connect_verified()` (connect + `SO_PEERCRED` check).
- **Reap before lookup** -- `reap_sessions()` MUST precede Attach/KillSession/ListSessions. Stale sessions cause silent failures.
- **Channel closed check** -- before `Frame::Ok` for Attach, check `client_tx.is_closed()` (session died between reap and lookup).
- **`Stdio::from(OwnedFd)`** -- don't reintroduce `FromRawFd` in server.rs.
- **Fork before tokio** -- `daemonize()` MUST fork before creating the tokio runtime. `main()` is sync (no `#[tokio::main]`).
- **Orphans get SIGKILL, never SIGTERM** -- an orphaned daemon's SIGTERM handler runs its normal shutdown, which unlinks whatever is at its old socket path; by reap time that path may belong to a newer daemon. Same reason the daemon's own lost-socket exit path (`drain_sessions`) removes no files.
- **Reap only after the confirm delay** -- `procscan::confirm_and_reap` must wait longer than `daemon::SOCKET_CHECK_INTERVAL` so a self-healing daemon is never killed mid-recovery.

### Changing the protocol
- Bump `PROTOCOL_VERSION` whenever frame types, encoding, or `SessionEntry` fields change (currently v22).
- Version mismatch is **not** a hard handshake gate -- the daemon replies `HelloAck` with its own version and only honors `KillServer` afterward, so `kill-server`/`restart` work across mismatches. Details in [docs/wire-protocol.md](docs/wire-protocol.md).
- `Frame` enum changes require updating: encoder, decoder, protocol tests, all `match frame` in server.rs, client.rs, daemon.rs, main.rs.
- `server::run()` / `client::run()` / `ClientConn::Active` signatures are documented in [docs/internals.md](docs/internals.md) -- they are shared by the daemon and tests; update both.

### Testing
- **E2e**: `UnixStream::pair()` + channel to `server::run()`. No socket files.
- **Daemon**: real socket in `tempfile::tempdir()`. `do_handshake()` + `wait_for_daemon()`.
- **Protocol**: codec unit tests + property tests (`tests/protocol_proptest.rs`).
- **Nextest**: e2e + daemon capped at 2 concurrent; socat/SSH serial; 2 retries for flaky tests. Per-process isolation.
- **SSH/socat**: auto-detect availability, skip gracefully. `GRITTY_SSH_TEST=0` to force-skip.

### Workflow
- Run `just fmt` after making code changes.
- Run `just check` (clippy + full test suite) before finishing work.
- When changing code, update docs **in the same commit**. Files to check:
  - **README.md** -- overview, install, quick start, features, comparison, short config pointer, common troubleshooting, security model
  - **USAGE.md** -- full command table, all flags, `host:session` addressing, session env vars, full config reference, escape sequences, shell completions, debugging
  - **CLAUDE.md** -- module map, build/test commands, critical invariants, doc pointers
  - **ARCHITECTURE.md** -- high-level feature descriptions, diagrams
  - **docs/internals.md** -- detailed module descriptions, on-disk state inventory (any new socket/sidecar/lock file also goes in `doctor.rs::is_known_artifact()`), key patterns, core signatures (`server::run()`, `client::run()`, `ClientConn::Active`)
  - **docs/wire-protocol.md** -- frame types, byte layouts, handshake semantics, `PROTOCOL_VERSION`
  - **docs/tunnel-state-machine.md** -- any change to `connect.rs` supervisor behavior (states, transitions, timing constants, exit-code classification, `TunnelStatus` projection, flock/client-observer contract)

### Style
- `main()` returns `()`. Errors via `eprintln!("error: ...")`. Never `-> anyhow::Result` on `main()`.
