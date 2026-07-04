# Contributing to gritty

Patches welcome. This file is the short orientation; the deeper maps live
in [CLAUDE.md](CLAUDE.md) (module table, invariants),
[docs/internals.md](docs/internals.md) (module details, on-disk state
inventory, key patterns), and
[docs/wire-protocol.md](docs/wire-protocol.md) (frame formats).

## Setup

Rust edition 2024 (MSRV in `Cargo.toml` -- use [rustup](https://rustup.rs/)
if your distro's Rust is older), plus:

```bash
cargo install just cargo-nextest   # task runner + test runner
```

## Build, test, iterate

```bash
just check            # clippy + full test suite -- the pre-push gate
just fmt              # format everything
just test session     # run a filtered subset
cargo run -- server   # start a server; connect with: cargo run -- connect local:dev
```

Test tiers (all under `tests/`): protocol codec unit/property tests,
daemon integration tests (real sockets in tempdirs), e2e session tests
(socketpair straight into `server::run()`, no files), and optional
container / socat / SSH suites (`just test-container`, `just test-socat`)
that skip gracefully when their tooling is absent.

Development is test-driven: red-green-refactor. Prefer adding a test over
verifying by hand.

## Ground rules

- **Run `just fmt` and `just check` before pushing.**
- **Docs update in the same commit as the code.** The list of files to
  check per change is in [CLAUDE.md](CLAUDE.md) under "Workflow" --
  README/USAGE for user-visible behavior, docs/internals.md for module or
  on-disk-state changes, docs/wire-protocol.md + a `PROTOCOL_VERSION` bump
  for any frame change, CHANGELOG.md for anything a user would notice.
- **Read the "Critical invariants" section of [CLAUDE.md](CLAUDE.md)**
  before touching `security`, `daemon`, `server`, or `connect` -- several
  load-bearing rules there (socket creation goes through the `security`
  module, reap-before-lookup, fork-before-tokio, SIGKILL-only for
  orphans) exist because violating them loses user sessions.
- Commit messages: descriptive, focused, single-purpose --
  `feat(scope): ...` / `fix(scope): ...` as in `git log`.
- `main()` returns `()`; errors are reported via `eprintln!("error: ...")`.

## Changing the wire protocol

Bump `PROTOCOL_VERSION` in `src/protocol.rs` whenever frame types,
encoding, or `SessionEntry` fields change, update the encoder/decoder and
every `match frame` site (server.rs, client.rs, daemon.rs, main.rs), the
protocol tests, and [docs/wire-protocol.md](docs/wire-protocol.md). Note
the bump in CHANGELOG.md so users know to `gritty refresh`.
