<!-- Embedded verbatim into `gritty doctor --llm` reports via include_str!
     (src/commands/report.rs). Written for an LLM reader, not a human browsing
     the repo. Keep it short: it ships inside every diagnostic report. -->

## What gritty is

gritty provides persistent TTY sessions over Unix domain sockets -- similar
to Eternal Terminal or `autossh + tmux`, but socket-based with zero
networking code of its own. Sessions survive client disconnects, network
changes, and laptop reboots; the client auto-reconnects and replays exactly
the missed output. Remote access works by forwarding a Unix socket over
plain SSH.

Source, docs, and issue tracker: https://github.com/chipturner/gritty
(see USAGE.md for the full command reference, docs/internals.md for module
internals, docs/tunnel-state-machine.md for the SSH tunnel supervisor,
docs/wire-protocol.md for the frame protocol).

## Architecture in one minute

Three kinds of processes cooperate:

- **daemon / server** (`gritty server`, one per host): owns every session's
  PTY and shell. Listens on `ctl.sock` in the socket dir. Keeps an
  offset-indexed history of output so reconnecting clients resume
  byte-for-byte. Writes `daemon.log` (tracing) and `daemon.out` (raw
  stdout/stderr) next to its socket.
- **client** (`gritty connect`): attaches a terminal to a session. Sends
  heartbeats; on link loss it enters a reconnect loop with backoff and
  re-attaches automatically. When connecting to a tunnel host, client
  tracing (reconnects, link-down events) is routed into that tunnel's
  `connect-<name>.log`; for local sessions the client logs to stderr only.
- **tunnel supervisor** (`gritty tunnel-create`, one per remote host):
  keeps an `ssh -L <local.sock>:<remote ctl.sock>` process alive, respawning
  with backoff on failure. Per-tunnel sidecars in the socket dir:
  `connect-<name>.sock/.pid/.lock/.dest/.info/.ssh-opts/.remote-sock/.log/.out`.

Sessions are addressed `host:session`. Session names are silently prefixed
with the client's name (`laptop/work`) to prevent cross-machine collisions.
The wire protocol is versioned; both sides must match (pre-1.0, any release
may bump it).

## Known failure modes

- **systemd `/run/user/<uid>` wipe**: logging out of the last session on a
  host deletes the socket dir under the daemon. Current daemons self-heal
  (re-bind within seconds, sessions survive) or exit cleanly; daemons from
  old releases linger as unreachable orphans. Symptoms: sessions "vanish",
  stray `gritty server` processes. Fix: `gritty refresh` reaps orphans;
  `loginctl enable-linger` on the host prevents the wipe.
- **Protocol version mismatch after upgrading gritty**: operations fail
  with an explicit version-mismatch error. Fix: upgrade both sides, then
  `gritty refresh` (restarts only stale processes, idempotent).
- **Stale binary (same protocol)**: a daemon/tunnel still running code from
  before a rebuild. Doctor warns "running build X but binary on disk is Y".
  Fix: `gritty refresh`. Caution: restarting a stale daemon kills its
  sessions; refresh refuses when clients are attached unless run with `-y`.
  Do not recommend `-y` (or `restart`) casually -- attached sessions mean a
  human is using them right now.
- **Tunnel down / not recovering**: `reconnecting...` forever in the
  client. Check `gritty tunnels` and the tunnel's `connect-<name>.log`
  (ssh's own stderr lands in it). Common causes: ssh host unreachable, host
  key or auth prompts (the background tunnel cannot answer prompts -- plain
  `ssh <host>` must work first), remote gritty missing or too old.
- **Tunnel up but remote daemon unreachable**: clients get
  `daemon closed connection` on every connect while `gritty tunnels` says
  healthy. ssh's `-L` listener accepts locally but the remote-side connect
  fails; the diagnostic is `channel N: open failed: ...` in
  `connect-<name>.out`. Causes: remote daemon dead, or the forward targets
  a stale remote socket path. Doctor flags this; fix:
  `gritty restart <host>`.
- **Wake-from-suspend**: transient handshake EOFs and stale-looking locks
  right after the laptop wakes; gritty has grace periods for these, so
  brief noise in logs around a wake is normal. A key press forces an
  immediate reconnect attempt past a lagging "waiting for network".
- **Heartbeat loss**: server-side, a silent client is detached after the
  idle-evict timeout; the session itself keeps running detached.

## How to respond

You will find below: the user's problem description, environment details,
doctor's check results, current sessions and tunnels, and log excerpts
(timestamps are the machine's local time; the report header gives the
current time for correlation). Log excerpts include post-mortem logs of
tunnels that have died -- when a tunnel is the problem, its final log
lines are usually the best evidence here.

- Correlate timestamps across daemon and tunnel logs around the failure.
- Suggest the most likely causes, ranked, each tied to specific evidence.
- Suggest concrete next commands, preferring: `gritty refresh`,
  `gritty doctor` / `doctor --clean`, `gritty tunnels`,
  `gritty tunnel-destroy <name>` + `tunnel-create <dest>`,
  `RUST_LOG=debug gritty server -f` (foreground debug daemon), `-v` on any
  command, `kill -USR1 <daemon pid>` (cycle daemon log level live).
- Client-side events for tunnel hosts land in the tunnel's
  `connect-<name>.log`, which IS excerpted in this report -- look there for
  reconnect/link-down evidence. Only local-session client logs are absent
  (stderr-only; ask the user to reproduce with
  `gritty -v connect ... 2>client.log` if local client behavior is in
  question). Also not in this report: remote daemons' logs (this report is
  from one machine; ask the user to run `gritty doctor --llm` on the remote
  host over ssh if the remote side is suspect).
- If the evidence is insufficient, say precisely what to run or paste next.
