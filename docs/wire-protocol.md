# Wire Protocol Reference

Byte-level reference for the gritty wire protocol (`src/protocol.rs`).

**Read this when** changing `protocol.rs`, frame handling in `server.rs`/`client.rs`/`daemon.rs`, or anything that encodes/decodes bytes on the socket.

**Update this in the same commit** as any change to frame types, field layouts, handshake semantics, or `PROTOCOL_VERSION` (see the "Changing the protocol" checklist at the bottom).

## Framing

All frames are `[type: u8][length: u32 BE][payload]`. Payloads <= 1MB. `PROTOCOL_VERSION: u16` is currently **22**.

## Frame type codes

Handshake: `0x01` Hello, `0x02` HelloAck. Relay: `0x10` Data, `0x11` Resize, `0x12` Exit, `0x13` Detached, `0x14` Ping, `0x15` Pong, `0x16` Env, `0x17` DiagRequest, `0x18` DiagResponse, `0x19` ServerShutdown, `0x1A` Resume, `0x1B` Notice. Agent: `0x20` AgentForward, `0x21` AgentOpen, `0x22` AgentData, `0x23` AgentClose. URL/clipboard: `0x28` OpenForward, `0x29` OpenUrl, `0x2A` ClipboardSet, `0x2B` ClipboardGet, `0x2C` ClipboardData. Tunnel: `0x30` TunnelListen, `0x31` TunnelOpen, `0x32` TunnelData, `0x33` TunnelClose. Transfer: `0x38` SendOffer, `0x39` SendDone, `0x3A` SendCancel, `0x3B` SendFile. Port forward: `0x40` PFListen, `0x41` PFReady, `0x42` PFOpen, `0x43` PFData, `0x44` PFClose, `0x45` PFStop, `0x46` PortForwardRequest. Control: `0x50` NewSession, `0x51` Attach, `0x52` ListSessions, `0x53` KillSession, `0x54` KillServer, `0x55` Tail, `0x56` RenameSession. Responses: `0x60` SessionCreated, `0x61` SessionInfo, `0x62` Ok, `0x63` Error, `0x64` AttachAck. Reserved: `0x80-0xFF`.

## Handshake

`Hello`: `[version: u16][capabilities: u32][device_id: u64]`. `HelloAck`: `[version: u16][capabilities: u32][server_id: u64]`. `device_id` is a persistent per-machine identifier stored in `$XDG_STATE_HOME/gritty/device_id`; the server records it as the session owner for auto-reconnect validation. Capabilities bitfield, negotiated = client & server (bitwise AND). Defined bits: `CAP_CLIPBOARD (0x01)` -- gates clipboard frame forwarding and svc socket clipboard requests. `server_id` is an ephemeral daemon identifier picked at startup; a reconnecting client that observes a different value exits with "server restarted -- session is gone" instead of looping.

### Version mismatch is NOT a handshake error

Since v15, the daemon always replies with `HelloAck` carrying its own version even when the client's version differs, and the client decides via `require_matched_version()` whether to proceed. Under a mismatch the daemon gates the next control frame so only `KillServer` is honored (returning `Frame::Ok`); anything else gets `ErrorCode::VersionMismatch` with a message pointing at `gritty restart`. This is the recovery path for upgrading one side -- `kill-server` and `restart` both use `server_request_any_version` while every normal command uses `server_request` which bails on mismatch.

This is deliberate -- `kill-server` and `restart` need to work across a mismatched handshake so users can recover without SSH. `tunnel-create --ignore-version-mismatch` still exists for the SSH-level pre-check but its value is mostly superseded by the in-band recovery flow. `gritty refresh` is the porcelain: it reads each long-lived process's `.info` sidecar (see `runinfo`) and restarts only what is stale; `refresh local` is also what `refresh <host>` runs *on the remote* over SSH, so the remote daemon is measured against the remote's own on-disk binary (not ours), which is what works for source-built remotes where `bootstrap` doesn't apply.

## Control frames

`NewSession`: `[name_len: u16][name][cmd_len: u16][cmd][cwd_len: u16][cwd][cols: u16][rows: u16][client_name_len: u16][client_name]`. Empty cwd = `$HOME`. Zero cols/rows = default 80x24. `client_name` propagated to session metadata.

`Attach`: `[session_len: u16][session][client_name_len: u16][client_name][force: u8][no_replay: u8][cols: u16][rows: u16][attach_token: u64][rendered_offset: u64][line_dirty: u8]`. `attach_token` is an ownership claim flag: `0` = explicit connect (no ownership check; server adopts the Hello's `device_id` as new owner; also signals "fresh viewer" so the server replays scrollback context instead of an incremental resume), non-zero = auto-reconnect (server compares Hello's `device_id` against stored `owner_device_id`; mismatch → `OwnerChanged`). Server enforces: if attached and `!force`, returns `AlreadyAttached` error. `no_replay` = existence probe only (daemon replies `Ok` without session handoff). `cols`/`rows` are the client's current terminal size, applied to the PTY before reconnect replay so regenerated prompts and TUI repaints use the right winsize (0 = unknown). `rendered_offset` is how far the client has rendered into the session's PTY output stream -- the server resumes from there (see `Resume`/`Notice` and the smart-reconnect pattern in [internals.md](internals.md)). `line_dirty` = the client painted a reconnect status line, so its cursor left `rendered_offset`'s position and the server repaints the current line before resuming.

`Resume`: `[offset: u64]` (server → client, first frame after a reconnect/takeover handoff). The client sets its `rendered_offset` to `offset` and counts subsequent `Data` payload bytes up from it. `Notice`: `[bytes: remaining]` (server → client) -- terminal bytes to render that are NOT part of the PTY stream (reconnect dividers, truncation markers, alt-screen priming, partial-line repaints); unlike `Data` it does not advance `rendered_offset`.

`SessionCreated`: `[id: u32]`. Immediately followed by `AttachAck` on the same framed connection -- the creator auto-attaches.

`AttachAck`: `[token: u64][session_id: u32]`. Reply to a successful `Attach` (or auto-attach after `NewSession`). `token` echoes the `device_id` (client ignores it -- ownership is tracked by the persistent device_id, not an ephemeral token). The `session_id` lets the client use the authoritative numeric id (even when the user passed `-` or a name) for subsequent reconnect/tail/fwd-socket operations, avoiding client-side races resolving `-` via `ListSessions`.

`Error`: `[code: u16][message: remaining bytes]`. `ErrorCode`: `NoSuchSession(1)`, `NameAlreadyExists(2)`, `InvalidName(3)`, `EmptyName(4)`, `VersionMismatch(5)`, `UnexpectedFrame(6)`, `AlreadyAttached(7)`, `OwnerChanged(8)`, `Unknown(u16)`. Match on code for programmatic error handling, display message for humans. `OwnerChanged(8)` is terminal: the client's reconnect loop treats it like `ServerRestarted` and exits without retrying.

`SessionInfo`: `[count: u32][per entry: [entry_len: u32][id: u32][name: u16-len + bytes][pty_path: u16-len + bytes][shell_pid: u32][created_at: u64][attached: u8][last_heartbeat: u64][foreground_cmd: u16-len + bytes][cwd: u16-len + bytes][client_name: u16-len + bytes][agent_forwarding_active: u8][is_last_attached: u8][last_activity: u64]]`. Decoder skips unknown trailing bytes within each entry_len; new fields default gracefully when absent (older servers).

## Diagnostics and shutdown

`DiagRequest`: empty payload (client → server, during active session). `DiagResponse`: `[text: remaining bytes]` (server → client). Client sends DiagRequest on `~#`; server replies with session diagnostics (history stats + stream offset, alt screen state, channel counts, shell PID).

`ServerShutdown`: empty payload (server → client). Sent to attached and tail clients when the daemon is shutting down (`kill-server` or SIGTERM/SIGINT). Terminal: the client exits immediately with "server shut down -- session is gone" instead of entering the reconnect loop. Without it, a remote client behind a still-live tunnel would spin "reconnecting..." for minutes until the tunnel supervisor happened to restart the remote daemon with a fresh `server_id`. `daemon::shutdown()` sends `ClientConn::Shutdown` to each session and waits up to 500ms for the goodbye frames to flush before aborting stragglers.

## Forwarding and service requests

`PortForwardRequest`: `[forward_id: u32][direction: u8][listen_port: u16][target_port: u16]`. Client sends to server. Direction `0` = local-forward (server listens), `1` = remote-forward (client listens).

`SvcRequest` (svc socket dispatch, 1-byte discriminator): `OpenUrl=1`, `Send=2`, `Receive=3`, `Clipboard=5`. Clipboard sub-protocol: `[0x01][data]` = copy (client half-closes its write side; server replies one byte -- `0x01` delivered to an attached clipboard-capable client, `0x00` dropped -- so `gritty copy` fails loudly instead of exiting 0 on a silent drop; an older server sends nothing and the client degrades to a soft warning), `[0x02]` = paste (server responds with clipboard content).

File transfer manifest (svc socket, not Frame protocol): sender writes `[file_count: u32][per file: [name_len: u16][name: bytes][size: u64][mode: u32]]`. Server relays per-file headers `[name_len: u16][name: bytes][size: u64][mode: u32]` to receiver, then `size` bytes of data. Sentinel `[name_len: 0x0000]` ends transfer. `-` (stdin) spools to a temp file for size discovery.

## Changing the protocol

- **`PROTOCOL_VERSION`** -- bump whenever frame types, encoding, or `SessionEntry` fields change.
- **`expect_min_len`** -- all fixed-field decoders use `expect_min_len` (not exact length checks), so trailing bytes are tolerated for forward extensibility.
- **`Frame` enum** -- update: encoder, decoder, protocol tests, all `match frame` in server.rs, client.rs, daemon.rs, main.rs.
- **`SessionInfo`** -- entry count `u32`. Changing `SessionEntry` fields requires updating both encoder and decoder in protocol.rs.
- Update the wire format codes and field layouts in this document.
