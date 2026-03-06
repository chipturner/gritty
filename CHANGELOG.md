# Changelog

## 0.9.1

- Fix `rename --help` crash
- Hard-gate protocol version mismatch; add `--ignore-version-mismatch` to `connect`

## 0.9.0

- Rename session support
- Config file editing (`config-edit`)
- Session environment variables (`GRITTY_SESSION`, `GRITTY_SESSION_NAME`)
- Foreground process display in `list-sessions` (Linux only)
- OAuth callback tunneling with idle timeout
- Preflight SSH connectivity check before daemonizing `connect`
- Port forwarding (`local-forward`, `remote-forward`)
- File transfer (`send`, `receive`) with `--stdin`/`--stdout` pipe mode and `-r` recursive directory support
- Send/receive timeout flag
- `--command` and `--detach` flags for `new-session`
- `attach -` to reattach last session
- `attach -c` to create session if missing
- `ls` with no args lists sessions across all daemons
- `~#` escape sequence for session status and RTT
- Configurable heartbeat interval/timeout and ring buffer size
- Comparison table in README

## 0.7.0

- Shell completions (`bash`, `zsh`, `fish`, `elvish`, `powershell`)
- Auto-start server on `new-session`; add `--wait` flag
- Version handshake, send timeouts, truncation marker
- Release binaries for Linux and macOS

## 0.6.0

- `gritty tail` for read-only session output streaming
- TOML configuration file with per-host overrides (`config-edit` added in 0.9.0)

## 0.5.0

- SSH agent forwarding (`-A`)
- URL open forwarding (`-O`)
- Escape sequences (`~.` detach, `~R` reconnect, `~^Z` suspend, `~?` help)
- Ring buffer drain while client disconnected
- `gritty info` diagnostics command
- Self-healing SSH tunnel with backoff and respawn
