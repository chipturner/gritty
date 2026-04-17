# Tunnel state machine

Gritty's SSH tunnel supervisor (`src/connect.rs`) has enough moving parts --
lockfile ownership, ssh child lifecycle, app-layer probing, exponential backoff,
remote server re-priming -- that the behavior is easier to reason about as an
explicit state machine than by reading the code top-to-bottom.

This document is the source of truth for that state machine. When you change
`connect.rs`, update the diagram and the state notes below **in the same
commit**.

## Scope

Covered:

- `tunnel-create` startup path (`connect::run`).
- Supervisor loop (`connect::tunnel_monitor`): ssh child lifecycle, app-layer
  probing, respawn/backoff, remote-ready re-priming.
- Shutdown path (`ConnectGuard::drop`, `disconnect`).
- Externally observable status (`probe_tunnel_status` -> `TunnelStatus`).

Not covered (separate concerns, documented elsewhere):

- Client reconnect loop (see `ARCHITECTURE.md`, `## Self-Healing Reconnect`).
- Session-layer takeover and ownership (`CLAUDE.md`, Device-based ownership).

## On-disk artifacts

A tunnel named `NAME` owns these files in `socket_dir()`:

| File                    | Writer                                   | Purpose |
|-------------------------|------------------------------------------|---------|
| `connect-NAME.lock`     | `try_acquire_lock` (flock exclusive)     | Liveness: flock held == supervisor alive. |
| `connect-NAME.pid`      | `run()` immediately after lock acquired  | Target for `disconnect`'s SIGTERM. |
| `connect-NAME.sock`     | `ssh -L` bind target                     | Client connects here to reach remote `ctl.sock`. |
| `connect-NAME.dest`     | `run()` after socket is up               | Original destination string, for `restart` / auto-start recovery. |
| `connect-NAME.log`      | tracing subscriber                       | Supervisor's own structured logs. |
| `connect-NAME.out`      | daemonize stderr redirection             | ssh child's stderr (forward-setup errors, etc.). |

Invariant: the flock held on the `.lock` *inode* is the single liveness truth.
Everything else is advisory -- if the inode's flock is free, any other leftover
file is stale by definition. `ConnectGuard::drop` releases the flock **before**
unlinking the lock file, so a racing `try_acquire_lock` never observes a
"file unlinked but inode still locked" window in which it could `O_CREAT` a
new inode at the path and end up holding a valid flock concurrently with the
departing supervisor.

## State diagram

```mermaid
stateDiagram-v2
    direction LR

    [*] --> Absent

    Absent --> WaitingForPeer: try_acquire_lock fails\n(peer supervisor alive)
    Absent --> Starting: try_acquire_lock ok\n(write .pid, clean stale files)

    WaitingForPeer --> [*]: wait_for_socket ok\n(print socket path, exit 0)
    WaitingForPeer --> Failed: wait_for_socket timeout
    WaitingForPeer --> Failed: peer released flock\n(supervisor died mid-startup)

    state Starting {
        direction TB
        [*] --> Preflight
        Preflight --> Preflight: ensure_remote_ready\n(ssh + socket-path + server-start)
        Preflight --> SpawnChild: got remote_sock + version\n(version-check; bail unless --ignore-version-mismatch)
        SpawnChild --> WaitForSocket: ssh spawned\n(stderr drained to .out)
        WaitForSocket --> Ready: UnixStream::connect ok
        WaitForSocket --> Aborted: child exited before socket\nor wait_for_socket timeout
    }

    Starting --> Running: Ready\n(write .dest, signal_ready,\nspawn tunnel_monitor)
    Starting --> Failed: Aborted

    state Running {
        direction LR
        [*] --> Alive
        Alive --> Alive: probe_tunnel_alive ok\n(Hello -> HelloAck -> ListSessions,\nreset failure counter)
        Alive --> ProbeFailing: probe failed\n(counter++)
        ProbeFailing --> Alive: next probe ok
        ProbeFailing --> KillingChild: counter >= 2\n(kill ssh, reset counter)
        Alive --> ChildExited: child.wait() returned
        KillingChild --> ChildExited: child.wait() observes kill

        ChildExited --> NonTransient: exit code in 1..=254\nexcluding 128..=159
        ChildExited --> Backoff: exit code 255,\nsignal death (128..=159),\nor no code (local signal)
        NonTransient --> [*]: warn, monitor returns
        Backoff --> Backoff: sleep(backoff)\nreset to 1s if child_spawned_at\nelapsed >=30s,\nelse double to <=60s
        Backoff --> EnsureRemoteRetry: timer fires
        EnsureRemoteRetry --> Backoff: ensure_remote_ready err\n(retry from top)
        EnsureRemoteRetry --> SpawnRetry: got fresh remote_sock
        SpawnRetry --> Alive: ssh respawned
        SpawnRetry --> Backoff: spawn_tunnel err
    }

    Running --> Stopping: SIGTERM / SIGINT\n(run() select)
    Running --> Stopping: monitor returned\n(NonTransient exit)
    Running --> Stopping: stop.cancelled()\n(ConnectGuard drop)

    state Stopping {
        direction TB
        [*] --> CancelMonitor: stop.cancel()
        CancelMonitor --> KillChild: monitor awaits child.kill()
        KillChild --> CleanupFiles: ConnectGuard::drop\n(SIGTERM ssh, rm sock/pid/lock/dest)
        CleanupFiles --> [*]: flock released
    }

    Stopping --> [*]

    Failed --> [*]: bail with diagnostic
```

## External observability

From another process (e.g. `gritty tunnels`, `gritty info`, the client's
reconnect grace window), status is projected to three externally visible
values via `probe_tunnel_status(name) -> TunnelStatus`:

| Observed                                     | `TunnelStatus`  | Internal states that produce it |
|----------------------------------------------|-----------------|---------------------------------|
| lock held + `.sock` connectable              | `Healthy`       | `Running.Alive`, `Running.ProbeFailing` (socket is still up during probe) |
| lock held + `.sock` not connectable          | `Reconnecting`  | `Starting.*`, `Running.KillingChild`, `Running.ChildExited`, `Running.Backoff`, `Running.EnsureRemoteRetry`, `Running.SpawnRetry` |
| lock free                                    | `Stale`         | Supervisor absent / dead; `.sock`/`.pid` orphaned. `get_tunnel_info` GCs stale files as a side effect. |

The client's reconnect loop (`src/client.rs`) uses `is_lock_held` directly,
not `TunnelStatus`: a held lock means "supervisor is alive and may be
respawning" -- keep retrying past `SOCKET_GONE_GRACE` -- while a free lock
means "tunnel is gone, give up". This distinction is what lets a 1..60s ssh
backoff not trip the client's short socket-gone grace.

## Transition details

### Startup (`connect::run`)

1. **Absent -> WaitingForPeer** -- `try_acquire_lock` returned `Err`. Another
   supervisor holds the flock. This process must not spawn a second ssh
   child: the socket path is shared and the invocation is expected to be
   idempotent (`auto_start` relies on "`tunnel-create` exit 0 ==> socket is
   ready"). We fall through to `wait_for_socket` to absorb the startup race,
   then signal ready and exit. The wait races against a concurrent
   `is_lock_held` poll so a peer supervisor that crashes during its own
   startup surfaces as a fast, diagnosable error instead of forcing us to
   wait the full `socket_wait_deadline`.
2. **Absent -> Starting** -- `try_acquire_lock` returned `Ok`. We own the
   supervisor role. Clean any stale `.sock`/`.pid`/`.dest` (we don't remove
   the lock we just acquired), then write the `.pid` file **immediately**
   so `disconnect` can find us during the startup window.
3. **Starting.Preflight -> Starting.SpawnChild** -- `ensure_remote_ready`
   returns `(remote_sock, remote_version)`. Version mismatch bails unless
   `--ignore-version-mismatch`; this predates the in-band v15 mismatch
   recovery in the daemon and is still used as a pre-flight for
   `tunnel-create` since we can't talk to the remote daemon without ssh.
4. **Starting.SpawnChild -> Starting.WaitForSocket** -- ssh spawned with
   `exec sleep 2147483647` as its remote command. Not `-N`: a mux client
   with `-N` exits 0 immediately after the master accepts the forward. A
   blocking remote command keeps a session channel open so the child's
   lifetime tracks the forward in both mux and standalone modes. Stderr is
   drained to our stderr (== `.out` in daemonized mode) so mux errors like
   `mux_client_forward: forwarding request failed` surface without waiting
   for `wait_for_socket` to time out.
5. **Starting.WaitForSocket -> Running** -- `wait_for_socket` raced against
   `child.wait()`. On child exit first, bail with a diagnostic pointing at
   `.out`. On socket ready first, write `.dest`, call `signal_ready` so the
   parent `tunnel-create` process exits, and hand the ssh child to
   `tunnel_monitor`.

### Supervisor loop (`tunnel_monitor`)

The monitor runs a `tokio::select!` with three arms:

- **`stop.cancelled()`** -- kill ssh child and return.
- **`probe_ticker.tick()`** -- every 30s, run `probe_tunnel_alive` against
  the local socket. The probe does `Hello -> HelloAck -> ListSessions` with
  a 3s outer timeout and 1s inner timeouts. This catches remote-daemon death
  (OOM, crash, manual kill) that ssh can't see: ssh's `ServerAliveInterval`
  only covers TCP-layer liveness. Two consecutive probe failures kill the
  ssh child to force a respawn that re-runs `ensure_remote_ready`. One
  transient probe failure is recoverable and does not kill ssh.
- **`child.wait()`** -- ssh exited. Classify the exit code:
  - codes `1..=254` except `128..=159` are non-transient (auth/config
    errors); log and return without retry.
  - code `255` (ssh connection error), codes `128..=159` (signal death
    from the remote side: reboot, OOM, SIGTERM during shutdown), and `None`
    (local signal-kill, typically our own `child.kill()` from the probe
    arm) are all transient -- sleep the current `backoff`, then retry.
- Whenever a respawn succeeds, the "healthy threshold" logic
  (`spawned_at.elapsed() >= 30s` at the *next* exit) resets `backoff` back
  to 1s. That means a tunnel that dies five minutes into a stable run waits
  1s before retrying, not whatever the last `min(backoff * 2, 60s)` was.

### Re-priming the remote (`ensure_remote_ready` on respawn)

Between `Backoff` and `SpawnRetry`, we call `ensure_remote_ready` again.
Without this, a respawn after a remote reboot succeeds at the ssh/forward
layer but nothing is listening on the far end of the forward, so the first
client Hello hits EOF. The re-prime runs `gritty ls local || (gritty server
&& sleep 0.3)` so a rebooted host gets its daemon started before we point
ssh at its ctl socket. If `ensure_remote_ready` itself fails (ssh auth
problem, remote unreachable, etc.), we go back to `Backoff` -- we don't
try to spawn ssh against a stale `remote_sock`.

### Shutdown

Two entry points into `Stopping`:

- **`ConnectGuard::drop`** (normal path) -- `run()` received SIGTERM/SIGINT,
  or fell out of the main `select!` because the monitor returned (e.g.
  non-transient exit). `Drop` cancels the stop token, SIGTERMs the ssh
  child directly as a belt-and-braces (the monitor also kills on cancel),
  then removes `.sock`, `.pid`, `.lock`, `.dest`. The flock is released
  last when `_lock` drops.
- **`disconnect(name)`** (external command) -- reads `.pid`, sends SIGTERM,
  polls `is_lock_held` for up to 2s. If still held, escalates to SIGKILL
  plus `killpg` to catch any detached ssh children. Then calls
  `cleanup_stale_files(name, true)` which removes the lock file too.

## Invariants

These must hold; violating them has in the past caused specific, nasty bugs:

1. **The flock is the ground truth for liveness.** Never infer "tunnel is
   up" from the presence of `.sock` or `.pid`. The supervisor can be mid-
   respawn (flock held, socket gone) for up to 60s.
2. **Write `.pid` before any slow startup step.** `ensure_remote_ready` +
   `spawn_tunnel` + `wait_for_socket` can take tens of seconds on WAN
   links. `disconnect` needs the PID immediately -- before v0.11.0 the
   PID was written only after socket-up, so `disconnect` during startup
   saw "lock held but no PID" and failed.
3. **`remote_sock` must be re-fetched on every respawn.** See above;
   otherwise a remote reboot / upgrade leaves the tunnel pointing at a
   dead daemon with no way to recover short of `tunnel-destroy`.
4. **Non-transient exit codes must not retry.** Auth failure, host-key
   mismatch, remote config error (`ExitOnForwardFailure=yes` tripping on
   a bad forward spec) all exit in `1..=254 \ {255, 128..=159}`. Retrying
   these hammers the remote and buries the real error in a loop.
5. **Stderr drain must start immediately.** If ssh fills its stderr pipe
   buffer (~64KB) with forward-setup errors while we're blocked in
   `wait_for_socket`, ssh wedges and we never see the error. See
   `drain_stderr` in `connect.rs`.
6. **`stop.cancel()` must happen before `ConnectGuard` drops the child.**
   Otherwise the monitor's `child.wait()` arm may race the guard's SIGTERM
   and try to respawn a dying supervisor.

## Timing constants

| Constant                              | Value          | Location             | Rationale |
|---------------------------------------|----------------|----------------------|-----------|
| `ServerAliveInterval` / `CountMax`    | 3s / 2 (=6s)   | `TUNNEL_SSH_OPTS`    | TCP-layer dead-peer detection inside ssh. |
| `PROBE_INTERVAL`                      | 30s            | `tunnel_monitor`     | App-layer Hello handshake cadence. Longer than `ServerAliveInterval` so ssh catches TCP failures first. |
| `PROBE_FAILURES_BEFORE_RESPAWN`       | 2              | `tunnel_monitor`     | One missed probe can be a transient network blip; two in a row (60s window) is a confident "remote daemon is dead". |
| Probe outer timeout                   | 3s             | `probe_tunnel_alive` | Runs inside the supervisor select; slow probe blocks everything. |
| Probe inner (HelloAck) timeout        | 1s             | `probe_tunnel_alive` | Same. |
| Backoff min / max                     | 1s / 60s       | `tunnel_monitor`     | Aggressive first retry (usual case: transient ssh exit 255); cap at 60s to avoid hammering. |
| `HEALTHY_THRESHOLD`                   | 30s            | `tunnel_monitor`     | Tunnel alive this long before its next death resets backoff to 1s. |
| `socket_wait_deadline(ct)`            | `max(5, ct)+10`s (60s if ct==0) | `wait_for_socket` | Bounds wait-for-socket polling; leaves headroom for ProxyCommand startup and forward setup. |
| `remote_exec` outer timeout           | 60s            | `remote_exec`        | Wall-clock ceiling on the whole ssh invocation; ServerAlive covers TCP hangs but not stuck shell profiles. |
| `disconnect` graceful deadline        | 2s             | `disconnect`         | SIGTERM -> poll for flock release. Escalates to SIGKILL + killpg after. |

## Client-observer coupling

The client's auto-reconnect loop reads the supervisor's flock via
`connect::is_lock_held` (through `ctl_socket_lock_path`). This is the only
cross-module coupling to the state machine, so it's worth stating the
contract explicitly:

> While the supervisor's flock is held, the client MUST assume the tunnel
> is alive-but-possibly-respawning, and keep retrying past its normal
> `SOCKET_GONE_GRACE` window. Only a free flock (== `TunnelStatus::Stale`)
> authorizes the client to give up.

Changing supervisor behavior in a way that holds the flock without being
respawn-capable (for example, holding it in `Failed`) will break this
contract and cause clients to hang forever on destroyed tunnels.
