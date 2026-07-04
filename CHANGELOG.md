# Changelog

Notable changes per release. Each entry notes the wire protocol version;
when it bumps, run `gritty refresh` after upgrading (see
[USAGE.md](USAGE.md#debugging) -- it restarts stale daemons and tunnels
everywhere in one idempotent command). Releases that don't bump the
protocol interoperate with their neighbors.

## Unreleased

- **`gritty mangen <dir>`**: generate man pages (one per subcommand),
  mainly for packagers. `just man` writes them to `target/man`.
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
