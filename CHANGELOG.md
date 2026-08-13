# Changelog

Notable changes per release. Each entry notes the wire protocol version;
when it bumps, run `gritty refresh` after upgrading (see
[USAGE.md](USAGE.md#debugging) -- it restarts stale daemons and tunnels
everywhere in one idempotent command). Releases that don't bump the
protocol interoperate with their neighbors.

## Unreleased

- **`tail` and down hosts.** `tail` now prints `session exited [with code
  N]` when the session it is watching ends (its output used to just stop,
  the same as a quiet session) and exits with that code. Commands other
  than `connect` never start anything, and their error for a down host now
  says what is down and how to bring it up -- `tunnel devbox is not
  running … -- gritty connect devbox brings it up`, `tunnel devbox is
  reconnecting … retry in a moment`, or `no local server running … --
  gritty connect starts one` -- instead of "no server running" for all
  three; `send`/`receive` with nothing running at all say so in the same
  terms.
- **Transfer messages.** When an offer ends without pairing, `send` now
  says why per session (`local:work: replaced by a newer gritty send in
  that session`, `…: busy with another transfer`, `…: closed before
  pairing` -- the server tells it, via two new svc go bytes) instead of
  "no receiver connected"; a pairing timeout names the budget and both ways
  to extend it; `send -` names its payload `stdin-YYYYMMDD-HHMMSS` (it was a
  file literally called `stdin`, so a second pipe overwrote the first) and
  prints that name up front unless the receiver uses `-`; per-file progress bars
  in a batch line up in one column; and the notice painted into the
  attached terminal reads `transfer started: 3 files (1.2 MiB)` -- it
  used to say "receiving" even when the session was the sending side.
- **Fixed: `gritty copy` of more than 512 KiB was silently truncated and
  acknowledged as success.** The server now refuses an oversize payload
  (new svc reply byte `0x02`; an older `copy` binary against a new server fails on the closed socket instead), and
  `copy` itself checks the limit first so the error quotes the size
  (`clipboard payload is 1.50 MiB (limit 512 KiB); … use gritty send`).
- **Breaking: `gritty completions <shell>` is replaced by dynamic
  completions.** Put `source <(COMPLETE=zsh gritty)` (or the bash/fish/
  elvish equivalent, see USAGE) in your shell rc and delete any generated
  `_gritty`/`gritty.bash` file. Besides flags, TAB now completes host
  names, tunnel names for `tunnel-destroy`, and -- after the colon --
  the live sessions of that host for `connect`, `tail`, `rename`, `kill`,
  `lf`/`rf` and `--session`, and it stays in step with the installed
  binary automatically.
- **`ls` fits on a normal terminal.** The table now shows `Name Cmd CWD
  Client Idle Linger Status` (CWD home-relative); it used to run ~130
  columns wide with ID, PTY, PID and a full timestamp on every row. Those
  diagnostic columns are behind `ls --full`, and `--json` still carries
  every field. The session picker's CWD column shortens remote homes too
  (it only recognized the local `$HOME` before).
- **`ls <host>` and bare `ls` are the same listing.** A single host now
  renders as its dashboard section (header with tunnel destination/status,
  same columns; `--json` fills in `destination`/`tunnel_status` instead of
  `null`), and probe failures read like every other command's ("no server
  running (could not connect to ...)", the refresh hint on a mismatch)
  rather than a raw errno. Hosts are ordered `local` first, then tunnels by
  name -- `tunnels`, `doctor`, `info` and `refresh` pick up the same order
  instead of directory order. Empty states are consistent for scripts: the
  bare dashboard with nothing running prints a hint (or `[]` under
  `--json`) and exits 0; naming a host that is down still fails, but
  `--json` now emits the group with its `error` field rather than a bare
  message.
- **Message fixes.** Every counted noun goes through one helper, so
  `session(s)`, `target(s)` and friends are gone (`failed to kill 2 of 3
  sessions`); `kill` of a single session reports its error once instead of
  `work: no such session: work` plus a `failed to kill 1 of 1` trailer;
  `refresh` with no host says how many hosts it tried; and
  `tunnel-create --ignore-version-mismatch` now prints the mismatch it is
  ignoring on your terminal (the warning used to go only to the
  supervisor's log, since by then the supervisor has backgrounded -- it
  travels back through the readiness pipe like a startup error does), with
  the refusal message rewritten to name both versions and `bootstrap`.
- **Fixed: a tunnel that fails after ssh is spawned reports the failure at
  once.** The daemonize readiness pipe was inherited by everything the
  supervisor and daemon exec'd, so `tunnel-create`'s error read waited for
  EOF until that ssh (or, for the daemon, a session shell) happened to
  exit; the pipe is now close-on-exec.
- **Gone sockets are noticed promptly.** If a tunnel's local socket file
  disappears (a `/tmp` sweeper, an external `rm`), the supervisor now
  respawns ssh on the next 30s tick instead of counting the missing file as
  two 30s strikes; and an attached client whose server socket has been
  removed exits when its 3s grace is up, rather than after whatever
  reconnect backoff happened to be scheduled (observed: ~8s).
- **`info` and `doctor` read better.** `info` now says `local server:
  not running` (on a laptop that only talks to remote hosts that isn't a
  problem), omits log paths that don't exist instead of printing a page of
  `(not found)`, and shows tunnels as a table with their destinations.
  `doctor`'s healthy-tunnel lines report what is behind the tunnel
  (`3 sessions, 2 attached`) instead of restating the protocol version,
  and the Clients group is one line per host instead of one per session.
  `tail`/`kill` invoked without a session now show the host's sessions in
  the same table `ls` prints (typeable names; the old ID/Name/Age table
  misaligned on long names).
- **Tunnel bookkeeping.** `tunnel-destroy` of a name gritty has never seen
  is now an error listing the tunnels it does know (it used to say "already
  stopped" and exit 0, so a typo left the real tunnel running). A tunnel's
  remembered destination and CLI `-o` options now survive `tunnel-destroy`
  and supervisor death, like the remote-socket cache already did, so
  `connect devbox` after tearing down `tunnel-create user@10.0.0.5 -n
  devbox` goes back to `user@10.0.0.5` instead of trying to ssh to
  `devbox` (`doctor` no longer counts those two files as residue).
  `tunnel-create` prints its socket path on stdout only when stdout is not
  a terminal -- scripts still get it, and a `connect` that auto-starts a
  tunnel no longer drops a raw path above your shell. `bootstrap` installs
  this binary's release by default (`--release latest` for the newest),
  then confirms the remote's protocol version and prints the `connect`
  command to run next; it used to install `latest` and exit silently.
- **Help cleanup.** Each command's `--help` lists `--ctl-socket`/`-v`/
  `--color` under a separate "Global options" heading instead of shuffled
  in with its own flags; `--ctl-socket` is rejected (usage error) by the
  commands that never read it, instead of being silently ignored; the
  top-level command listing shows every alias (`list`, `socket` were
  missing) and uses the same one-line descriptions as the commands
  themselves (a test now pins the two together); `lf`/`rf` show
  `[TARGET] <PORT>` and keep the compact help layout; duplicated
  `(default: …)` text is gone; `connect -c` conflicts with `--no-create`.
- **`receive` is safe to interrupt, and both sides say who they are
  waiting for.** Files now land via a `.<name>.gritty-partial` sibling
  renamed into place when complete: a sender dying mid-file leaves the
  previous copy intact and no partial behind, and the error says which file
  and how far it got (`transfer of big.bin interrupted after 1.2 MiB of
  40 MiB`); replacing an existing file is announced and now also applies
  the sent permissions (before, only newly created files got them). The
  final line names the directory. `waiting for receiver…` now lists the
  sessions the offer went to (`waiting for receiver in 2 sessions
  (devbox:work, local:0) -- run gritty receive in one of them`), since a
  transfer without `--session` is offered everywhere at once.
- **A failed first connect now says what ssh said.** The tunnel preflight
  used to discard ssh's stderr and blame "a password or host key" for
  everything (including a typo'd hostname), and `connect` then spent five
  seconds retrying before adding a generic "server did not become ready".
  Now: `ssh to devbox failed: <ssh's own last line>` plus a remedy matched
  to it (accept the host key / fix unattended auth / check by hand, each
  with the exact interactive ssh command including your `-o` options), and
  a failed auto-start ends immediately with `tunnel devbox did not start`
  (`--wait` still waits). Later tunnel failures point at the `.out` file
  and the interactive ssh command instead of suggesting `--foreground`.
- **Connecting to a session someone else holds now asks.** `session work
  is already attached by laptop2 -- take it over? [y/N]` replaces the
  error-plus-retype-with-`--force` dance (which is what the flag's help
  always implied); scripts and non-terminals still get the error and the
  `--force` hint.
- **`connect -d` behaves like a create command.** Unnamed `-d` creates the
  next free slot (it used to pick an existing session and do nothing);
  `-c` on a session that already exists now warns that the command was
  ignored instead of silently attaching or reporting green success; and
  `-d` waits until the daemon lists the new session as detached before
  exiting, closing a few-millisecond window where the next command in a
  script saw it as attached.
- **Picker polish.** Both pickers (`connect`, `prune --pick`) now have a
  column header and show `ls`'s columns -- the connect picker gains `Idle`
  (it showed the session's age, unlabeled) and reserves the `(attached)`
  column only when something is attached; rows are styled the same way in
  both; the hint lines list `q` and the arrow keys, and the new-session row
  is `n)` with `n`/`c` as the two ways to create (the `+` binding is gone).
  Killing the last session from the picker leaves you in the picker on the
  new-session row instead of silently creating a session.
- **Session names are shown one way everywhere.** `attached
  mylaptop/work`, picker rows, rename pre-fill, transfer pairing labels,
  `doctor`, and the suggested `gritty connect host:mylaptop/work` commands
  all used the raw wire name while `ls` elided your own prefix; every
  surface now uses `ls`'s form (`work`; foreign sessions keep their
  prefix), which is also exactly what you type. `tail`'s status line names
  the resolved session (not `-`) and says it is read-only; the non-tty
  session list says when every candidate is attached and includes
  `--force` in those lines.
- **Sessions are addressed by name only.** With the ID column gone from
  `ls`, bare `kill 3` no longer falls back to daemon id 3 when you have no
  session named `3` -- on hosts with auto-numbered sessions the two usually
  disagreed, so the fallback killed the wrong session. `-` now means the
  same thing in `tail`/`lf`/`rf` as in `connect` (the daemon's last-attached
  session, or an error), instead of guessing the most recently heartbeated
  one.
- **Fixed: `/tmp` sweepers no longer strip a long-lived tunnel of its
  sidecars.** 0.15.0 taught the supervisor to keep `connect-<name>.lock`
  fresh, but `.pid`, `.info`, `.dest`, `.ssh-opts`, `.remote-sock`, `.log`
  and `.out` were still written once and never touched again, so after a
  few days macOS's `tmp_cleaner`/`dirhelper` removed them out from under a
  healthy tunnel: `gritty tunnels` and `gritty ls` showed the destination
  as `-`, `restart`/auto-start forgot the original `user@host` and `-o`
  options, and -- because a live supervisor with no `.info` reads as
  "version unknown" -- every `gritty refresh` restarted every tunnel while
  `gritty doctor` insisted nothing was wrong. The supervisor now re-touches
  the whole set on every probe tick, and `doctor` reports the
  version-unknown case with the same verdict `refresh` acts on. Existing
  tunnels pick this up on their next `tunnel-create`/`refresh`. No
  protocol change.
- **`:name` now means `local:name`.** An empty host part used to flow
  through as a tunnel literally named `` -- `gritty connect :work` printed
  `starting tunnel ...`, ran `tunnel-create --name '' ''`, and left a
  `connect-.log` behind. It now follows the omitted-host rule and addresses
  the local daemon, in every command that takes a target.
- **Fixed: the `connect` picker's cursor started on the wrong row.** The
  initial position was computed over the daemon's id order but the rows are
  displayed with your own sessions first, so whenever a foreign session had
  a lower id the cursor could land on an attached row (Enter then failed
  with "already attached"). It now starts on your first detached session
  as displayed, or on the "new session" row when everything is attached.
- **Fixed: `tunnel-create` for a tunnel that is already up says so.** It
  used to run the ssh preflight, fork, and print `tunnel X started` even
  though the child had merely found the existing supervisor (its "already
  running" line went to the `.out` file). It now reports
  `tunnel X already running (pid N)` up front without touching ssh. Also
  fixed: `tunnel-create <alias-spelling>` (no `-n`) announced the
  canonical name but created the tunnel's files under the spelling you
  typed, so a following `connect <name>` started a second tunnel.
- **Fixed: `doctor`'s Clients lines named sessions by id.** `client on
  this machine holds devbox:3` looked like a target, but `devbox:3` is how
  you address the session *named* `3` -- on a host with auto-numbered
  sessions that is usually a different one. Doctor now prints the same
  typeable `host:name` labels `lf`/`rf` use (`devbox (session #3)` when the
  daemon can't be asked).
- **Fixed: `send`/`receive` with stderr redirected emitted one blank line
  per file** (the newline that ends the progress bar was printed even when
  no bar was drawn); interactively, zero-byte files now get a completed bar
  line like every other file instead of a bare blank line.
- **In-session helpers.** `gritty open` in a *detached* session now fails
  immediately (and the `$BROWSER` shim prints the URL for manual opening)
  instead of waiting out a 2s timeout and then guessing "server may be
  older" -- the daemon answers detached opens the way it already answered
  detached copies. Its error no longer claims "no client is connected with
  --forward-open" when a client is connected; and `GRITTY_SOCK not set` no
  longer suggests `--forward-open` (the variable is set regardless).
  `gritty copy` run outside a session errors before reading stdin rather
  than after you press ^D. `send`/`receive --session <host>` (no session
  part) is rejected instead of silently auto-detecting across all hosts.
- **Message fixes.** A failed auto-start / ssh / bootstrap no longer reports
  `(exit exit status: 1)`. `connect host:-` says `attached <name>` (and
  `-d` says `<name> exists`) instead of a literal `-`, and can no longer
  race into creating a session. Bare `gritty kill` lists `local`'s
  sessions instead of demanding a host or `--ctl-socket`. `refresh`'s
  outdated-remote error tells you to `bootstrap` the tunnel's ssh
  destination rather than its connection name (they differ for `-n`
  tunnels), and the all-hosts form no longer drops the cause chain from
  per-host errors.
- **Fixed: the "server shut down" exit line no longer erases your last line.**
  When the daemon was killed under an attached client (or a `tail`), the
  red exit message was painted over the current row -- the sequence meant
  for replacing the reconnect spinner -- so the prompt or output you were
  looking at vanished under it. Fatal exit lines now open a fresh row unless
  a reconnect status line is actually on screen. No protocol change.
- **Fixed: `connect` without a terminal no longer leaves a session behind.**
  Run from a script (stdin a pipe or `/dev/null`), `connect` created or
  attached the session and only then died with `ENODEV: No such device`
  when it tried to put stdin into raw mode -- so every accidental scripted
  `connect` littered a fresh session. It now refuses up front, before
  talking to the daemon, and points at `connect -d`, which remains the
  non-interactive form. No protocol change.
- **`send` disambiguates duplicate file names instead of erroring.**
  `gritty send docs/a/reference.md docs/b/reference.md` used to refuse
  ("duplicate file name `reference.md`"); it now sends them as
  `a/reference.md` and `b/reference.md` -- each colliding name grows its
  shortest unique path suffix (uniquify style), and the receiver already
  creates subdirectories for nested names. File-vs-directory conflicts
  (a file `a` next to a nested `a/ref.md`) are resolved the same way. The
  same file spelled two ways collapses to one copy (announced, and the
  survivor keeps its short name); the error remains only for distinct
  files that genuinely cannot be told apart, and it names the conflicting
  paths. Names are opaque strings on the wire, so no protocol change.
- **Fixed: `send -r .` works now.** The recursive walk emitted `./`-prefixed
  wire names the daemon rejects, so the sender stalled and died with a
  misleading "no receiver connected". Wire names are now normalized to
  plain components.
- **Fixed: exiting a session no longer disturbs the terminal's main screen.**
  Every client exit path (session exit, detach, `-c` completion, tail)
  unconditionally emitted `\x1b[?1049l` (leave alternate screen) to rescue
  the terminal from a TUI that died mid-session. When no TUI was involved
  that sequence is not the assumed no-op: its implied cursor restore can
  jump the cursor to a stale position, and some terminals restore an empty
  saved buffer -- visibly clearing your output, most noticeably after
  `connect -c`. The client now tracks alt-screen state and emits `?1049l`
  only when the session actually left the terminal in the alternate screen;
  the SGR reset and cursor-show remain unconditional. No protocol change.
- **`connect`/`tail` telemetry always goes to a log file now.** Local
  attaches and `--ctl-socket` overrides used to log to stderr -- invisible
  until a warning (or a widened `RUST_LOG`) sprayed the attached terminal.
  Clients of the local daemon now append to `client.log` in the socket dir;
  clients of a tunnel keep appending to that tunnel's `connect-<name>.log`.
  No protocol change.
- **A log file that fails to open now says so.** `init_tracing` used to fall
  back to stderr silently -- keeping the file's wider `info` filter -- when
  the log file could not be opened, which made "why is telemetry on my
  terminal?" needlessly forensic. The fallback now prints a warning naming
  the path and the cause, and takes the quieter stderr filter with it. No
  protocol change.
- **Fixed: stderr log lines no longer smear across the terminal mid-session.**
  Tracing events that reach stderr while attached (e.g. `RUST_LOG=info` while
  debugging a reconnect) used to staircase diagonally -- raw mode disables the
  kernel's LF->CRLF translation -- and splice into the "reconnecting..."
  status line, which parks the cursor mid-line. The stderr writer now erases
  the current line and emits CRLF while the client owns the terminal. No
  protocol change.
- **Fixed: tunnels could sit out a full 60s backoff after lid-open.** The
  supervisor's wake-from-suspend detector only watched for suspends that
  happened *during* a backoff sleep. The common lid-open case -- ssh notices
  its dead connection ~6s after wake and exits 255 -- started a fresh sleep
  *after* the wake, so a backoff that had climbed to 60s across dark wakes ran
  in full with nothing to interrupt it. The suspend check now also runs across
  the dead ssh child's lifetime (wall-clock vs monotonic uptime): an exit that
  follows a suspend skips the first backoff sleep for one immediate attempt,
  keeping the climbed backoff so a dark wake with a locked Keychain still
  costs only one failed auth. No protocol change.
- **Pressing a key while disconnected now nudges the tunnel supervisor too.**
  Keystrokes in the "reconnecting..." state used to force only the *client's*
  next attempt, which went nowhere while the supervisor was mid-backoff with
  the forward socket unbound. The client now also touches a
  `connect-{name}.kick` file; the supervisor's backoff wait polls for it every
  2s, consumes it, and respawns ssh immediately. No protocol change.
- **Fixed: receive-first transfers died with "superseded by new sender" + "early
  eof".** `gritty send` fans out one connection per discovered session, and a
  daemon reachable through two tunnel sockets (two connection names for one
  host) was discovered twice -- so the receiving session saw the same sender
  arrive twice, and the second copy aborted the relay the first had just
  started. Send-first worked by luck: the duplicate landed before pairing,
  where replacing a parked sender is harmless. Two fixes: an active relay now
  always wins (a duplicate sender/receiver is dropped, and the client's
  pairing race skips it as a dead sibling), and transfer discovery dedupes
  daemons by the `server_id` they report in `HelloAck`. No protocol change.
- **Fixed: visual artifacts on shell prompt lines after a slow auto-reconnect.**
  When a reconnect dragged past 1s (status line shown), the server "repaired"
  the prompt line by clearing it and replaying the raw byte transcript since
  the last newline -- but a shell's current line is an edited region, not
  text: replayed cursor-forward/delete-char/autosuggestion sequences only make
  sense against the cells that existed when first played, so the repaint left
  gaps, ghost fragments, truncated-escape garbage, and duplicated wrapped
  lines. The line on screen was never damaged in the first place (the status
  line lives on the row below and is erased on success), so the server now
  restores only what was actually disturbed: the cursor column and SGR state,
  computed by a new `line_shadow` emulator (autowrap- and wide-char-aware)
  over the retained output history. Nothing is cleared or repainted. No
  protocol change.
- **`refresh` refuses to kill attached sessions without `-y`**: restarting a
  stale daemon kills every session it hosts; refresh now counts attached
  clients first and asks for `-y` instead of proceeding silently (a routine
  `refresh local` against a rebuilt dev binary once took down 7 live
  clients). A protocol-stale daemon can't report its sessions, so the
  post-upgrade recovery path is unaffected. `refresh <host>` forwards `-y`
  to the remote `gritty refresh local`; a remote binary that predates the
  flag rejects it -- update the remote first, or use `gritty restart <host>`.
- **The daemon logs who killed it**: `kill-server received` now records the
  sender's pid and cmdline (Linux, via `SO_PEERCRED`), so "what nuked my
  sessions" is answerable from `daemon.log` instead of a forensic dig.
- **Fixed: `ls` columns misaligned on CJK and emoji.** Column widths were measured
  in bytes while the padding counted characters -- neither is a terminal column.
  A session whose `Cmd` or `CWD` held wide characters pushed every column to its
  right out of line. Widths are now measured and padded in display columns.
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
