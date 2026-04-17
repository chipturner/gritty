# Liveness / heartbeat follow-ups

Deferred from the 2026-04 heartbeat review. Each is independently landable.

## Pong-deadline fast-fail (medium value, ~30 lines)

Currently a Ping that never gets a Pong is only noticed via the generic 60s
inbound-idle timeout. Arm a `pong_deadline = now + max(5s, 3 * last_rtt)` when
a Ping is sent; if it expires before any inbound frame, return `Disconnected`.
Cuts blackhole detection (wifi→cellular handover with a still-ESTABLISHED TCP
socket) from ~60s to ~15s. Needs a dedicated test that withholds Pong.

If this lands, also switch `last_outbound_at` from `Instant` to `SystemTime`
with the same backward-clamp as `wall_elapsed`, so the deadline math is
consistent across suspend.

## Tighten IDLE_EVICT_TIMEOUT to 60s (aggressive)

90s was chosen because it leaves the client defaults (interval=10, timeout=60)
untouched. Going to 60s would halve the ghost-attached window after lid-close
but requires also lowering `DEFAULT_HEARTBEAT_TIMEOUT` to ~40s so the derived
config clamp still passes defaults. Do this only if field reports show 90s is
still annoyingly long for force-takeover.

## Split SEND_TIMEOUT per frame class

`SEND_TIMEOUT` (10s) currently guards both keystroke `Data` frames and bulk
`PFData`/`TunnelData`/`AgentData`. The stdin short-circuit already makes the
keystroke-after-wake case fast, so there's no urgency. If bulk-forward stalls
on congested cellular uplinks become a reported problem, consider a longer
timeout for channel-data frames and a shorter (5s) one for interactive frames.

## 1s staleness poll arm

A cheap `tokio::time::interval(1s)` select arm that checks `link_is_stale()`
would make wake detection near-instant even without user input. The stdin
short-circuit covers the case that matters (user opened the lid *to type*), so
this is only worth it if "open lid, stare at frozen terminal for 10s without
typing" turns out to be a real complaint.

## Configurable SSH ServerAlive knobs

`ServerAliveInterval=3/Count=2` is hardcoded in the tunnel. Very lossy links
(satellite) may prefer `10/3` to avoid spurious respawns. The `ssh-options`
escape hatch covers this today; a dedicated `[defaults.tunnel]` knob would be
more discoverable but is low priority.
