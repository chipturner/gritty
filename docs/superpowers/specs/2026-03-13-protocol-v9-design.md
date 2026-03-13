# Protocol v9: Final Stabilization for 1.0

## Context

Protocol v8 cleaned up the wire format (error codes, length-prefixed SessionEntry, u32 IDs, capabilities field, frame byte reorganization). This follow-up addresses the remaining issues found during 1.0 review: in-band signaling hacks in the Env frame, fixed-field frames that can't be extended without a version bump, and an unused capabilities bitfield.

## Changes

### 1. Add `client_name` to NewSession

**Problem:** `client_name` is in `Frame::Attach` but not `Frame::NewSession`. For the first client (new session), the server extracts `client_name` from a `GRITTY_CLIENT` key in the `Env` frame -- a hack that overloads Env's purpose ("set these in the shell") with session metadata.

**Fix:**
- Add `client_name: String` field to `Frame::NewSession`
- Wire: append `[client_name_len: u16][client_name]` after `[rows: u16]`
- Update `expect_min_len` minimum for NewSession from 10 to 12 (10 + 2 for the empty client_name length prefix)
- Server uses `client_name` from NewSession directly for first-client metadata
- Remove `GRITTY_CLIENT` from `ALLOWED_ENV_KEYS` in server.rs. Add explicit `cmd.env("GRITTY_CLIENT", &client_name)` at shell spawn to preserve the env var for user scripts.
- Remove both server-side `GRITTY_CLIENT` lookups (first-client Env parse at server.rs:1377-1381 and reconnect Env handler at server.rs:918-921)
- Remove `GRITTY_CLIENT` from client Env construction in commands/session.rs (hard version gate means no mixed versions)

### 2. Move BROWSER symlink to OpenForward handler

**Problem:** The client sends both a `BROWSER` key in the Env frame and a `Frame::OpenForward`. The server creates the `gritty-open` symlink and sets `BROWSER` in the shell on the Env path. This is redundant and races (Env arrives before OpenForward).

**Fix:**
- Move the `gritty-open` symlink creation and `cmd.env("BROWSER", &open_link)` logic from the Env handler to a new deferred setup triggered by `Frame::OpenForward`
- Since OpenForward arrives *after* shell spawn (it's a relay frame, not a pre-spawn frame), the BROWSER env var must be set differently. Two options:
  - (a) Create the symlink eagerly at session start (always), set BROWSER to it unconditionally. The symlink exists but only works if a client has sent OpenForward. This is simplest.
  - (b) Store the open_link path, set BROWSER at spawn time unconditionally, and the `gritty-open` binary already checks for a connected client before trying to forward.

**Chosen approach:** (a) -- always create the symlink at session start and set BROWSER to it. The `gritty-open` binary sends to the svc socket which checks `open_forward_enabled`. If no client has OpenForward enabled, the server already responds with 0x00 (no client). This is already the behavior -- the only change is removing the `BROWSER` key from Env frame handling and creating the symlink unconditionally.

- Remove `BROWSER` env var construction from client in commands/session.rs (the `if settings.forward_open` block that adds `BROWSER` to the Env frame). Note: `collect_env_vars()` in lib.rs does NOT include BROWSER -- it's added in session.rs.
- Server creates `gritty-open` symlink and sets `BROWSER` env var unconditionally at shell spawn
- Remove the `k == "BROWSER"` branch from the Env handler in server.rs

### 3. All fixed-field frames use `expect_min_len`

**Problem:** Fixed-field frame decoders use `expect_len` (exact match). Adding a field to Hello, Resize, SessionCreated, etc. requires a protocol version bump even if the new field has a sensible default.

**Fix:** Replace all `expect_len` calls with `expect_min_len`. Decoders read known fields from the payload and ignore any trailing bytes. This applies to:

- `Hello` (6 bytes min)
- `HelloAck` (6 bytes min)
- `Resize` (4 bytes min)
- `Exit` (4 bytes min)
- `AgentOpen` (4 bytes min)
- `AgentClose` (4 bytes min)
- `TunnelListen` (2 bytes min)
- `TunnelOpen` (4 bytes min)
- `TunnelClose` (4 bytes min)
- `SendOffer` (12 bytes min)
- `PortForwardListen` (8 bytes min)
- `PortForwardReady` (4 bytes min)
- `PortForwardOpen` (10 bytes min)
- `PortForwardClose` (4 bytes min)
- `PortForwardStop` (4 bytes min)
- `SessionCreated` (4 bytes min)

The `expect_len` function can be removed entirely since no callers will remain.

### 4. CAP_CLIPBOARD capability bit

**Problem:** `Hello { capabilities: u32 }` exists but is always 0. The bitfield is dead weight without at least one defined bit, and clipboard forwarding has no negotiation -- the server blindly sends ClipboardSet/ClipboardGet.

**Fix:**
- Define `pub const CAP_CLIPBOARD: u32 = 0x01` in protocol.rs
- Client sets `CAP_CLIPBOARD` in `Hello.capabilities` (always -- all v9 clients support it)
- Server sets `CAP_CLIPBOARD` in `HelloAck.capabilities`
- Negotiated capabilities = `client_caps & server_caps`, stored in a session-level variable
- `handshake()` in lib.rs returns negotiated capabilities (or stores them)
- Server checks negotiated capabilities before:
  - Accepting `SvcRequest::Clipboard` in the svc acceptor (drop if not negotiated)
  - Sending `ClipboardSet`/`ClipboardGet` frames in the relay
- Daemon passes negotiated capabilities to server via a new field or through the existing channel

**Propagation path:** The daemon performs the handshake via `connection_handshake()` in daemon.rs. Currently it destructures `Frame::Hello { version, .. }`, discarding capabilities, and sends `HelloAck { capabilities: 0 }`. Changes needed:
- `connection_handshake()` captures `capabilities` from Hello, sends HelloAck with server caps, computes `negotiated = client_caps & server_caps`
- The channel from `connection_handshake` to `dispatch_control` (currently `(Frame, Framed<...>)`) must carry the negotiated capabilities -- becomes `(Frame, Framed<...>, u32)`
- `ClientConn::Active { framed, client_name, capabilities }` carries negotiated caps to the session server
- Server stores negotiated caps in a local variable, updates on every reconnect/takeover (new ClientConn::Active arrives with fresh capabilities)

### 5. ClipboardGet timeout

**Problem:** Server sends ClipboardGet, stores a oneshot in `pending_paste`, waits forever. If the client never replies, the svc-side paste caller hangs.

**Fix:**
- Add a `paste_deadline: Option<tokio::time::Instant>` to ServerRelay (alongside `pending_paste`)
- When ClipboardGet is sent, set deadline to `Instant::now() + 5s`
- Add a select branch for the deadline in the relay loop
- When it fires, resolve `pending_paste` with `None` and clear the deadline
- Clear both on ClipboardData receipt
- Clear both on client disconnect/takeover (same sites where `pending_paste.take()` already happens)

## Files to modify

| File | Changes |
|------|---------|
| `src/protocol.rs` | NewSession gains `client_name`, `CAP_CLIPBOARD` const, `expect_len` removed, all decoders use `expect_min_len`, bump PROTOCOL_VERSION to 9 |
| `src/server.rs` | BROWSER symlink unconditional, remove GRITTY_CLIENT/BROWSER from Env handler, ClientConn gains capabilities, clipboard gating, paste timeout |
| `src/daemon.rs` | Capture capabilities in `connection_handshake()`, thread through channel, pass via ClientConn::Active |
| `src/client.rs` | Remove BROWSER from env vars, set CAP_CLIPBOARD in Hello (via handshake) |
| `src/lib.rs` | handshake() sets CAP_CLIPBOARD, returns/stores negotiated caps |
| `src/commands/session.rs` | Add client_name to NewSession construction, remove BROWSER and GRITTY_CLIENT from Env frame |
| `tests/protocol_test.rs` | Update NewSession roundtrips, update expect_len error tests |
| `tests/e2e_test.rs` | Update NewSession constructions, ClientConn::Active |
| `tests/daemon_test.rs` | Update handshake, NewSession, ClientConn |
| `tests/socat_bridge_test.rs` | Update handshake |
| `tests/socat_tunnel_test.rs` | Update handshake |
| `CLAUDE.md` | Wire format, server::run() notes, capabilities docs |

## Verification

1. `cargo check` after each logical step
2. `just fmt && just check` as final gate
3. Manual smoke test: `cargo run -- server && cargo run -- connect local:test`
