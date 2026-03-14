#!/usr/bin/env bash
set -euo pipefail

# Drive asciinema recordings from .demo scripts.
#
# Usage:
#   ./run-demo.sh <script.demo>                 # drive only (attach with: tmux attach -t gritty-demo)
#   ./run-demo.sh <script.demo> <output.cast>   # drive + record
#
# Demo file format (one directive per line, # comments, blank lines ignored):
#
#   type: <text>            Type text with realistic delays, press Enter
#   send: <key> [<key>...]  Send tmux key names (C-c, C-b, Enter, Escape, etc.)
#   wait: <pattern>         Wait for pattern in pane output (30s timeout)
#   wait/N: <pattern>       Wait with N-second timeout
#   sleep: <seconds>        Fixed delay (fractional OK)
#   run: <command>          Run a shell command in the driver (not the demo pane)
#   pause                   Wait for Enter in the driver terminal (manual step)

TMUX_SESSION=gritty-demo
COLS=120
ROWS=36

die() { echo "error: $*" >&2; exit 1; }

# Extract gritty hosts referenced in a demo file (for cleanup).
demo_hosts() {
    grep -ohE 'gritty (connect|tunnel-create) [^ ]+' "$1" \
        | awk '{print $NF}' | cut -d: -f1 | sort -u
}

cleanup() {
    tmux kill-session -t "$TMUX_SESSION" 2>/dev/null || true
    for host in $DEMO_HOSTS; do
        gritty kill-server "$host" 2>/dev/null || true
        gritty tunnel-destroy "$host" 2>/dev/null || true
    done
}

type_text() {
    local text=$1 i char
    for ((i = 0; i < ${#text}; i++)); do
        char="${text:i:1}"
        if [[ "$char" == ";" ]]; then
            tmux send-keys -t "$TMUX_SESSION" '\;'
        else
            tmux send-keys -t "$TMUX_SESSION" -l -- "$char"
        fi
        sleep "0.0$((RANDOM % 5 + 3))"
    done
}

type_line() {
    type_text "$1"
    sleep 0.1
    tmux send-keys -t "$TMUX_SESSION" Enter
}

wait_for() {
    local pattern=$1 timeout=${2:-30} start=$SECONDS
    while ((SECONDS - start < timeout)); do
        if tmux capture-pane -t "$TMUX_SESSION" -p 2>/dev/null | grep -qF "$pattern"; then
            return 0
        fi
        sleep 0.3
    done
    die "timed out after ${timeout}s waiting for: $pattern"
}

setup_session() {
    tmux kill-session -t "$TMUX_SESSION" 2>/dev/null || true
    tmux -f /dev/null new-session -d -s "$TMUX_SESSION" -x "$COLS" -y "$ROWS"
    tmux set-option -t "$TMUX_SESSION" status off
    # Clean prompt, no history
    tmux send-keys -t "$TMUX_SESSION" \
        "export PS1='$ ' PROMPT_COMMAND='' HISTFILE=/dev/null" Enter
    sleep 0.3
    tmux send-keys -t "$TMUX_SESSION" clear Enter
    sleep 0.5
}

drive_demo() {
    local demo_file=$1
    while IFS= read -r line || [[ -n "$line" ]]; do
        line="${line%%$'\r'}"
        [[ "$line" =~ ^[[:space:]]*($|#) ]] && continue

        if [[ "$line" =~ ^type:\ (.+)$ ]]; then
            type_line "${BASH_REMATCH[1]}"

        elif [[ "$line" =~ ^send:\ (.+)$ ]]; then
            read -ra keys <<< "${BASH_REMATCH[1]}"
            for key in "${keys[@]}"; do
                tmux send-keys -t "$TMUX_SESSION" -- "$key"
                sleep 0.05
            done

        elif [[ "$line" =~ ^wait(/([0-9]+))?:\ (.+)$ ]]; then
            wait_for "${BASH_REMATCH[3]}" "${BASH_REMATCH[2]:-30}"

        elif [[ "$line" =~ ^sleep:\ (.+)$ ]]; then
            sleep "${BASH_REMATCH[1]}"

        elif [[ "$line" =~ ^run:\ (.+)$ ]]; then
            bash -c "${BASH_REMATCH[1]}"

        elif [[ "$line" == pause ]]; then
            read -rp "PAUSED -- press Enter to continue..."

        else
            echo "warning: unknown directive: $line" >&2
        fi
    done < "$demo_file"
}

# --- Main ---

demo_file="${1:-}"
cast_file="${2:-}"

[[ -n "$demo_file" ]] || die "usage: $0 <script.demo> [output.cast]"
[[ -f "$demo_file" ]] || die "not found: $demo_file"

DEMO_HOSTS=$(demo_hosts "$demo_file")

cleanup
setup_session
trap cleanup EXIT

if [[ -n "$cast_file" ]]; then
    # Recording mode: drive in background, record in foreground.
    # The driver kills the tmux session when done, which exits tmux attach,
    # which exits asciinema. idle-time-limit caps dead time (e.g. OAuth wait).
    (sleep 2; drive_demo "$demo_file"; sleep 1; tmux kill-session -t "$TMUX_SESSION" 2>/dev/null) &
    asciinema rec --overwrite --idle-time-limit 2 \
        --command "tmux attach -t $TMUX_SESSION" \
        "$cast_file" || true
    wait 2>/dev/null || true
    echo "saved: $cast_file"
else
    # Interactive mode: user attaches in another terminal to watch.
    echo "attach to watch:  tmux attach -t $TMUX_SESSION"
    echo "starting in 3s..."
    sleep 3
    drive_demo "$demo_file"
    echo "demo complete -- press Enter to kill session"
    read -r
fi
