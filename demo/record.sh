#!/usr/bin/env bash
set -euo pipefail

# Record demo GIFs from vhs tapes against an isolated gritty environment.
#
# Usage:
#   demo/record.sh [tape...]    # default: every .tape in this directory
#
# Requires: vhs and tmux on PATH (https://github.com/charmbracelet/vhs)
# and a release build of gritty (cargo build --release). GIFs land in
# demo/out/.
#
# Everything lives under /tmp/gritty-demo: its own socket dir, config
# (client-name "laptop" instead of the real hostname), and a pristine HOME
# whose .bash_profile gives session shells a deterministic prompt showing
# the session name. Recordings never touch your real daemon or sessions.
# vhs inherits this environment, so tapes need no Env lines of their own.
# Split-pane tapes run tmux on its own server socket (-L gritty-demo) so
# they never touch a real tmux server either.
#
# State is reset before every tape (fresh daemon + reseeded sessions and
# files), so each tape records against the same fixture regardless of what
# earlier tapes created or killed.

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
REPO_ROOT="$(dirname "$SCRIPT_DIR")"

DEMO_DIR=/tmp/gritty-demo

die() { echo "error: $*" >&2; exit 1; }

command -v vhs >/dev/null || die "vhs not found on PATH"
command -v tmux >/dev/null || die "tmux not found on PATH"
[[ -x "$REPO_ROOT/target/release/gritty" ]] || die "no release binary; run: cargo build --release"
export PATH="$REPO_ROOT/target/release:$PATH"

export GRITTY_SOCKET_DIR="$DEMO_DIR/sock"
export XDG_CONFIG_HOME="$DEMO_DIR/config"
export HOME="$DEMO_DIR/home"
export SHELL=/bin/bash

# This script may itself be running inside a gritty session (dogfooding).
# Scrub that session's context so demo shells don't inherit it -- a leaked
# GRITTY_SOCK would make demo-side send/receive pair with the real session.
unset GRITTY_SESSION GRITTY_SESSION_NAME GRITTY_SOCK GRITTY_CLIENT BROWSER

# Fresh daemon + fixture sessions and files. The socket dir itself is
# created by gritty (the security module owns its permissions).
reset_env() {
    tmux -L gritty-demo kill-server 2>/dev/null || true
    gritty kill-server >/dev/null 2>&1 || true
    rm -rf "$DEMO_DIR"
    mkdir -p "$XDG_CONFIG_HOME/gritty" "$HOME/project" "$HOME/site"

    cat > "$XDG_CONFIG_HOME/gritty/config.toml" <<'EOF'
[defaults]
client-name = "laptop"
EOF

    # The PATH export must live here, not just in this script's env: login
    # shells (gritty sessions, tmux panes) run /etc/profile, which rebuilds
    # PATH and would drop the release binary.
    cat > "$HOME/.bash_profile" <<EOF
export PATH="$REPO_ROOT/target/release:\$PATH"
EOF
    cat >> "$HOME/.bash_profile" <<'EOF'
if [[ -n "${GRITTY_SESSION_NAME:-}" ]]; then
    PS1="\[\e[1;35m\][${GRITTY_SESSION_NAME#*/}]\[\e[0m\] \[\e[36m\]\w\[\e[0m\] \$ "
else
    PS1="\[\e[36m\]\w\[\e[0m\] \$ "
fi
EOF

    cat > "$HOME/.tmux.conf" <<'EOF'
set -g status off
set -g pane-border-style "fg=colour238"
set -g pane-active-border-style "fg=colour135"
EOF

    # Fixture files for the transfer and forward tapes.
    cat > "$HOME/project/results.csv" <<'EOF'
region,revenue
west,1200
east,3400
EOF
    echo '<h1>hello from the session</h1>' > "$HOME/site/index.html"

    # Seed detached sessions so `gritty ls` looks lived-in.
    gritty connect -d local:build -c 'sleep 600' >/dev/null 2>&1
    gritty connect -d local:scratch >/dev/null 2>&1
}

tapes=()
for tape in "$@"; do
    tapes+=("$(realpath "$tape")")
done
[[ ${#tapes[@]} -gt 0 ]] || tapes=("$SCRIPT_DIR"/*.tape)

cd "$SCRIPT_DIR"
mkdir -p out
for tape in "${tapes[@]}"; do
    echo "==> recording $(basename "$tape")"
    reset_env
    vhs "$tape"
done

tmux -L gritty-demo kill-server 2>/dev/null || true
gritty kill-server >/dev/null 2>&1 || true
echo "done: GIFs in $SCRIPT_DIR/out/"
