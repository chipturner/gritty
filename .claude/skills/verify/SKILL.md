---
name: verify
description: Build gritty and drive a change end-to-end against an isolated daemon (and, for tunnel changes, a fake-ssh tunnel) without touching the live daemon or tunnels on this machine.
---

# Verifying gritty changes by running them

Never point anything at the real socket dir -- the daemon/tunnels there serve
live sessions (see "Live daemon safety" in CLAUDE.md). Everything below is
isolated through `GRITTY_SOCKET_DIR`, kept under `target/` (execution from
`/tmp` is blocked on this machine, and the socket dir must be 0700 -- gritty
tightens it itself).

Rebuild first: `cargo build`. `cargo test`/`nextest`/`clippy` do **not**
refresh `target/debug/gritty`; driving a stale binary looks exactly like a
broken change. `doctor` only flags this when the tree is dirty (`-dirty`
build stamp), and not at all if the last build was from the same clean commit.

## Local daemon + sessions

```sh
export GRITTY_SOCKET_DIR=$PWD/target/verify-sb; mkdir -p $GRITTY_SOCKET_DIR
G=target/debug/gritty
$G server                          # or let `connect` auto-start it
$G connect -d local:a; $G ls; $G doctor; $G prune --all
$G kill-server; rm -rf $GRITTY_SOCKET_DIR
```

Interactive surfaces (picker, `~?`/`~#`/`~.`, reconnect chrome, prune
`--pick`) need a pty: use the tmux tool with a dedicated session, export
`GRITTY_SOCKET_DIR` in that shell, and capture the pane. `~` escapes are only
recognized right after Enter.

## Tunnel supervisor without a real host

`connect.rs` finds `ssh` via `PATH`, so a fake `ssh` first on `PATH` gives a
real supervisor, real sidecars, and real probe ticks (`PROBE_INTERVAL` = 30s):

```sh
V=$PWD/target/verify; mkdir -p $V/bin $V/local
cat > $V/bin/ssh <<'EOF'
#!/bin/sh
# -L <local>:<remote> becomes a socat bridge; any other invocation runs the
# "remote" command locally (preflight `true`, REMOTE_ENSURE_CMD, refresh local).
prev=""; for a in "$@"; do
  if [ "$prev" = "-L" ]; then exec socat "UNIX-LISTEN:${a%%:*},fork" "UNIX-CONNECT:${a#*:}"; fi
  prev=$a; done
for a; do last=$a; done; exec sh -c "$last"
EOF
chmod +x $V/bin/ssh
export PATH=$V/bin:$PATH GRITTY_SOCKET_DIR=$V/local
target/debug/gritty tunnel-create fakehost      # -> "tunnel fakehost started", tunnels says healthy
```

Gotchas: `remote_exec` forwards `GRITTY_SOCKET_DIR` into the remote command,
so the "remote" daemon lands in the same `$V/local` dir (fine -- `ls` shows
"same daemon"); `run_remote_gritty` (used by `refresh <host>`) does not, so
the remote-side `refresh local` reports against whatever dir the fake sees.
The remote command's `PATH` prefix puts `~/bin` first, i.e. the installed
release binary acts as the remote `gritty`. Age sidecars with
`touch -a -m -t $(date -v-7d +%Y%m%d%H%M) file` and wait one tick to check
freshness. Tear down with `tunnel-destroy fakehost`, `kill-server`, `rm -rf $V`.

## Read-only against the real daemon

`ls`, `tunnels`, `info`, `doctor` (no `--clean`), `socket-path` are safe with
`GRITTY_SOCKET_DIR` unset and are the quickest way to see real-world output
widths and states. Nothing else.
