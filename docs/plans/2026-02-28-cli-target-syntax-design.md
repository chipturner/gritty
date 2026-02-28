# CLI Target Syntax Redesign

## Problem

The current CLI requires a separate `-t`/`--target` flag for session identification alongside a positional `host` arg. This makes common commands verbose (`gritty new local -t work`) and the `-t` flag name is unclear.

## Design

### Unified `host[:session]` positional arg

Replace the separate `host` positional + `-t` flag with a single positional that uses colon syntax. Split on the first `:` -- left side is host, right side (if present) is session.

### Before / After

| Command | Before | After |
|---------|--------|-------|
| New named session | `gritty new local -t work` | `gritty new local:work` |
| New unnamed session | `gritty new local` | `gritty new local` |
| Attach by name | `gritty attach local -t work` | `gritty attach local:work` |
| Attach by id | `gritty attach local -t 0` | `gritty attach local:0` |
| Tail remote | `gritty tail devbox -t 0` | `gritty tail devbox:0` |
| List sessions | `gritty ls local` | `gritty ls local` |
| Kill session | `gritty kill-session local -t work` | `gritty kill-session local:work` |
| Kill server | `gritty kill-server local` | `gritty kill-server local` |
| Send (outside) | `gritty send local -t work f.txt` | `gritty send local f.txt` |
| Send (in-session) | `gritty send f.txt` | `gritty send f.txt` |

### Target parsing

`parse_target(s: &str) -> (String, Option<String>)` splits on the first `:`.

- `"local"` -> `("local", None)`
- `"local:work"` -> `("local", Some("work"))`
- `"devbox:0"` -> `("devbox", Some("0"))`
- `"local:my:weird:name"` -> `("local", Some("my:weird:name"))` (first colon only)

### Clap structure

Each command gets a single `target: String` positional (or `Option<String>` for send/receive):

- `NewSession { target: String, ... }` -- "local" or "local:work"
- `Attach { target: String, ... }` -- "local:0" or "local:work"
- `Tail { target: String, ... }`
- `ListSessions { target: String, ... }` -- host only, session part ignored
- `KillSession { target: String, ... }`
- `KillServer { target: String, ... }` -- host only
- `Send { target: Option<String>, files: Vec<PathBuf>, ... }` -- optional (in-session)
- `Receive { target: Option<String>, dir: Option<PathBuf>, ... }` -- optional

### Friendly errors for missing session

Commands requiring a session (attach, tail, kill-session) that receive only a host:

1. Connect to the server
2. Send ListSessions
3. Print formatted error with available sessions
4. Exit non-zero

Example output:

```
error: specify a session -- gritty attach local:<session>

  ID  Name     Created
  0   work     2m ago
  1   debug    30s ago
```

### Send/receive session inference

When target has no session part: list sessions, auto-pick if exactly one, error with list if ambiguous. When target is omitted (in-session): use GRITTY_SEND_SOCK as today.

### `--ctl-socket` interaction

`--ctl-socket` overrides host for socket resolution. Session part is still extracted from target. So `gritty attach --ctl-socket /tmp/my.sock ignored:work` targets session "work" on the given socket.

### `-t` flag

Dropped entirely. Clean break, no deprecation period.
