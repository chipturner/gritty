#!/usr/bin/env bash
set -euo pipefail

# Record demo GIFs from vhs tapes against an isolated gritty environment.
#
# Usage:
#   demo/record.sh [tape...]    # default: every .tape in this directory
#
# Requires: vhs on PATH (https://github.com/charmbracelet/vhs) and a
# release build of gritty (cargo build --release). GIFs land in demo/out/.
#
# Everything lives under /tmp/gritty-demo: its own socket dir, config
# (client-name "laptop" instead of the real hostname), and a pristine HOME
# whose .bash_profile gives session shells a deterministic prompt showing
# the session name. Recordings never touch your real daemon or sessions.
# vhs inherits this environment, so tapes need no Env lines of their own.
#
# State is reset before every tape (fresh daemon + reseeded sessions), so
# each tape records against the same fixture regardless of what earlier
# tapes created or killed.

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
REPO_ROOT="$(dirname "$SCRIPT_DIR")"

DEMO_DIR=/tmp/gritty-demo

die() { echo "error: $*" >&2; exit 1; }

command -v vhs >/dev/null || die "vhs not found on PATH"
[[ -x "$REPO_ROOT/target/release/gritty" ]] || die "no release binary; run: cargo build --release"
export PATH="$REPO_ROOT/target/release:$PATH"

export GRITTY_SOCKET_DIR="$DEMO_DIR/sock"
export XDG_CONFIG_HOME="$DEMO_DIR/config"
export HOME="$DEMO_DIR/home"
export SHELL=/bin/bash

# Fresh daemon + fixture sessions. The socket dir itself is created by
# gritty (the security module owns its permissions).
reset_env() {
    gritty kill-server >/dev/null 2>&1 || true
    rm -rf "$DEMO_DIR"
    mkdir -p "$XDG_CONFIG_HOME/gritty" "$HOME"

    cat > "$XDG_CONFIG_HOME/gritty/config.toml" <<'EOF'
[defaults]
client-name = "laptop"
EOF

    cat > "$HOME/.bash_profile" <<'EOF'
if [[ -n "${GRITTY_SESSION_NAME:-}" ]]; then
    PS1="\[\e[1;35m\][${GRITTY_SESSION_NAME#*/}]\[\e[0m\] \[\e[36m\]\w\[\e[0m\] \$ "
else
    PS1="\[\e[36m\]\w\[\e[0m\] \$ "
fi
EOF

    # Seed detached sessions so `gritty ls` looks lived-in.
    gritty connect -d local:build -c 'sleep 600' >/dev/null
    gritty connect -d local:scratch >/dev/null
}

tapes=("$@")
[[ ${#tapes[@]} -gt 0 ]] || tapes=("$SCRIPT_DIR"/*.tape)

cd "$SCRIPT_DIR"
mkdir -p out
for tape in "${tapes[@]}"; do
    echo "==> recording $(basename "$tape")"
    reset_env
    vhs "$tape"
done

gritty kill-server >/dev/null 2>&1 || true
echo "done: GIFs in $SCRIPT_DIR/out/"
