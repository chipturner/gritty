# Changelog

Notable changes per release. Each entry notes the wire protocol version;
when it bumps, run `gritty refresh` after upgrading (see
[USAGE.md](USAGE.md#debugging) -- it restarts stale daemons and tunnels
everywhere in one idempotent command). Releases that don't bump the
protocol interoperate with their neighbors.

## Unreleased

- **`--color=auto|always|never`, and color is finally conditional.** gritty
  colorized unconditionally: `gritty ls > file` wrote ANSI escapes into the
  file, `NO_COLOR` and `TERM=dumb` were ignored, and the transfer progress bar
  painted its erase-line into redirected stderr. Each stream is now colorized
  only when it is a terminal, `NO_COLOR` / `CLICOLOR` / `CLICOLOR_FORCE` /
  `TERM=dumb` are honored, and `--color` overrides all of it. The progress bar
  is drawn only when stderr is a terminal (independent of `--color`).
- **Messages have a vocabulary.** A new `ui` module names the five severities
  gritty had been expressing as ad-hoc escape codes at ~90 call sites, and owns
  the palette. Errors and warnings now render consistently as `error: <msg>` /
  `warning: <msg>` wherever they come from -- previously the same severity
  looked different depending on which code path printed it. The `▸` marker falls
  back to `>` when the locale is not UTF-8 (an unset-locale container no longer
  prints mojibake).
- **Log failures are now structured.** `warn!`/`error!` sites carried the error by
  interpolating it into the message text, so the single highest-value field was
  unparseable and the message was not a stable event identity. They now emit
  `error = %e` alongside a fixed message. Two `frame decode error` sites that
  logged identical text from different phases are now distinguishable
  (`hello` vs `control`), and the peer-UID rejection on the control socket --
  previously logged as a bare `warn!("{e}")` with no message at all -- says what
  it rejected.
- **`doctor --llm` no longer confuses field values for log levels.** The filter
  choosing which historical lines to include matched `WARN`/`ERROR`/`panic`
  anywhere on the line, so a session named `ERROR`, or an invocation audit line
  quoting either word, could consume the whole 40-line budget and push the real
  failures out of the report. The level is now matched positionally, and raw
  panics in `.out` files are matched on `panicked at`.
- **Fixed: log lines from spawned tasks lost their session.** The agent, port-forward,
  svc-socket, transfer-relay, and tail tasks were started with bare `tokio::spawn`,
  which does not inherit the enclosing `session{id,name}` span. On a daemon serving
  several sessions their lines -- including the svc-socket security events
  (`peer_cred unavailable`, unknown request byte) -- were unattributable. All
  spawns in `server.rs` and `client.rs` now go through `spawn_traced`.
- **Fixed: after recovering a wiped socket dir, the daemon logged into a
  deleted file.** The self-heal path re-bound the control socket and rewrote
  its sidecars but never reopened `daemon.log`, so every subsequent line was
  appended to an unlinked inode -- invisible to `doctor`, `tail`, and
  `doctor --llm`. It now requests a reopen before the first post-recovery
  log line.
- **Fixed: a failed log reopen was silent and permanent.** `SIGUSR2` cleared
  the reopen request before attempting the open, so if the open failed
  (directory wiped, `ENOSPC`, `EACCES`) the writer kept the old file
  descriptor forever with no error and no retry. The request now survives a
  failed open and is retried on the next write.
- **Fixed: log color escapes leaked into redirected stderr.** `gritty server
  -f 2>log` and `RUST_LOG=debug gritty ls 2>log` wrote ANSI codes into the
  file; the daemon's file logger already suppressed them. stderr is now
  colored only when it is a terminal.
- **Fixed: error messages dropped their context chain.** `gritty ls`, `kill`,
  `prune`, and friends printed only the outermost error; the `.context()`
  each command layer attached was discarded. They now render the full chain,
  matching what `gritty server` and `tunnel-create` already did.
- **Fixed: a failed remote probe could poison the tunnel's forward spec.**
  When `gritty socket-path` failed on the remote (binary missing after an
  upgrade, broken PATH), the probe's `ERR:` tag was mistaken for the socket
  path and the supervisor looped respawning `ssh -L ...:ERR:` ("Bad local
  forwarding specification") forever. The probe parser now rejects any
  `ERR:`-tagged or non-absolute-path result, a poisoned `.remote-sock`
  cache is discarded on read, and `spawn_tunnel` refuses to build a
  forward from a non-absolute remote path.
- **Clearer error when the remote daemon is unreachable through a tunnel**:
  `daemon closed connection` on a connect through a tunnel socket now
  explains that ssh is up but the remote daemon isn't answering, quotes the
  most recent `channel N: open failed` line from the tunnel's `.out`, and
  points at the file plus `gritty restart <host>`.
- **`doctor` flags tunnels whose remote daemon is unreachable**: previously
  an end-to-end probe failure through a healthy-looking tunnel was silently
  ignored and the tunnel reported `healthy`; it now warns with the ssh
  `.out` evidence.
- **`gritty doctor --llm ["description"]`**: print a self-contained,
  LLM-ready diagnostic report (architecture primer, known failure modes,
  health checks, session/tunnel state, sanitized log excerpts) to paste
  into a chat or pipe into an LLM CLI. gritty never calls an LLM itself.
- **`doctor --llm` includes dead tunnels' evidence**: stale tunnels appear
  in the report's tunnel list (status `stale`) instead of being silently
  garbage-collected while gathering, and the log excerpts cover post-mortem
  `connect-<name>.log`/`.out` files left behind by tunnels that died --
  previously the report omitted exactly the logs that explain a dead tunnel.

## 0.15.1 (2026-07-04) -- protocol v23 (no change)

- **Port forwards survive reconnect**: when the attached client drops
  (network blip, detach, takeover), a running `lf`/`rf` re-places its
  forward automatically once a client is attached again. Only Ctrl-C
  stops a forward now.
- **`--json` on `ls`, `tunnels`, `info`, and `doctor`**: machine-readable
  output for scripts and status bars. Fields are append-only.
- **`gritty mangen <dir>`**: generate man pages (one per subcommand),
  mainly for packagers. `just man` writes them to `target/man`.
- MSRV lowered from 1.94 to 1.88 (the actual floor: let-chains).
- `gritty prune` with no filter now explains the filter choices instead
  of emitting clap's generic required-argument error.
- Clearer help text: `-O` semantics, config-precedence note in
  `connect --help`, and `lf`/`rf` help that leads with the ssh `-L`/`-R`
  equivalence.

## 0.15.0 (2026-06-17) -- protocol v23 (refresh after upgrade)

- **Linger timeout**: detached sessions are auto-reaped after a
  configurable timeout (`linger` in config; off by default).
- Fix: the tunnel supervisor refreshes its `.lock` mtime so `/tmp`
  age-based sweepers don't reap a live tunnel.

## 0.14.0 (2026-06-08) -- protocol v22 (no change)

- `gritty receive` auto-switches to stdout mode when output is redirected.
- While the reconnect status line is showing, any keystroke forces an
  immediate retry (useful when the OS network monitor lags reality after
  wake-from-sleep).

## 0.13.2 (2026-06-06) -- protocol v22 (no change)

- **`gritty prune`**: bulk-kill stale detached sessions with `--client` /
  `--idle` filters, `--all`, or an interactive multi-select `--pick` TUI.
  Dry-run unless `-y`.
- **`gritty doctor` audits the socket directory** against a documented
  state inventory; `--clean` removes unknown files.
- `lf`/`rf` target is optional when exactly one attached session exists;
  errors steer toward the fix.
- Client commands log to stderr at `warn` by default; log files stay at
  `info`.

## 0.13.1 (2026-06-01) -- protocol v22 (no change)

- **Host aliases**: `[host.<name>] aliases` in config makes alternate
  spellings (IPs, FQDNs, short names) address one tunnel.

## 0.13.0 (2026-06-01) -- protocol v22 (no change)

- **Lifecycle self-healing**: daemons detect socket-directory loss (the
  systemd `/run/user` wipe) and re-bind without losing sessions, or exit
  cleanly when they can't; `gritty refresh` reaps orphaned daemons from
  older releases and ends with an end-to-end protocol probe.

## 0.12.11 (2026-06-01) -- protocol v22 (refresh after upgrade)

- `gritty ls` gains an Idle column; `kill-session` accepts multiple
  targets by ID or name.

## 0.12.1 - 0.12.10 (2026-05 to 2026-06) -- protocol v21 (no change)

- **Client-prefixed session names**: short names are scoped per client
  (`mylaptop/0`), so two machines typing `gritty connect host:0` no longer
  collide. Numeric default names; auto-attach and the picker are scoped to
  your own namespace.
- Bare `gritty ls` becomes a connectivity dashboard (local by default,
  `--include-remote` to fan out).
- Static musl Linux binaries; Homebrew formula published to
  `chipturner/tap`; macOS build fixes.
- Fixes: lazy agent-socket binding (so `ssh-add` reports "no agent" when
  unforwarded), wake-from-suspend ghost-lock no longer kills sessions,
  `.bindlock` cleanup.

## 0.12.0 (2026-05-16) -- protocol v21 (refresh after upgrade)

- **Offset-based reconnect resume**: the client reports how far it
  rendered and the server replays exactly the missed bytes -- a brief blip
  resumes byte-for-byte with nothing redrawn. Overhauled reconnect status
  line.
- **`gritty refresh`**: idempotent post-upgrade restarts driven by `.info`
  sidecars (only restarts what is actually stale); precise stale-process
  detection in `doctor`.
- **macOS network-path awareness**: reconnect and tunnel respawn react to
  path changes and wake-from-suspend instead of sleeping through them.
- `ServerShutdown` frame: `kill-server` tells clients to exit cleanly
  instead of spinning the reconnect loop.
- Many tunnel-supervisor hardening fixes (flock ownership, backoff
  discipline, signal handling) -- see
  [docs/tunnel-state-machine.md](docs/tunnel-state-machine.md).
- Broad fix pass across transfer (silent data loss, pipe-mode truncation),
  config validation, logging, and CLI error messages.

## 0.11.0 (2026-04-16) and earlier -- protocol v19

Foundation releases: persistent sessions over Unix domain sockets, the
single-socket daemon, SSH tunnel supervisor (`tunnel-create`), agent
forwarding, URL/OAuth forwarding, port forwarding (`lf`/`rf`), file
transfer (`send`/`receive`/`copy`), scrollback replay, `doctor`, and
`bootstrap`.
