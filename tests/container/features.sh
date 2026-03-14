#!/bin/bash
set -euo pipefail

passed=0
failed=0
total=0

pass() {
    echo "PASS: $1"
    passed=$((passed + 1))
    total=$((total + 1))
}

fail() {
    echo "FAIL: $1 -- $2"
    failed=$((failed + 1))
    total=$((total + 1))
}

wait_for_text() {
    local target=$1 pane=$2 timeout=${3:-5}
    for i in $(seq 1 "$timeout"); do
        if tmux capture-pane -t "$pane" -p -S - 2>/dev/null | grep -qF "$target"; then
            return 0
        fi
        sleep 1
    done
    return 1
}

# ---------------------------------------------------------------------------
# 1. Tail -- read-only stream of session output
# ---------------------------------------------------------------------------
test_tail() {
    # Create session (server auto-starts via connect, like lifecycle.sh)
    tmux new-session -d -s feat -x 120 -y 40
    tmux send-keys -t feat 'gritty connect local:tailtest' Enter
    sleep 3

    # Schedule marker output AFTER we detach. The ring buffer only captures
    # PTY output while no client is connected, so the marker must be echoed
    # during the disconnect window.
    tmux send-keys -t feat '(sleep 2 && echo TAIL_MARKER_xyz) &' Enter
    sleep 1

    # Detach so PTY output goes to ring buffer
    tmux send-keys -t feat Enter
    sleep 0.5
    tmux send-keys -t feat '~.'
    sleep 4  # wait for background echo to fire

    # Tail replays the ring buffer in a tmux pane (avoids stdout buffering)
    tmux split-window -t feat "gritty tail local:tailtest"
    sleep 3
    if tmux capture-pane -t feat.1 -p -S - 2>/dev/null | grep -qF TAIL_MARKER_xyz; then
        pass "tail captures session output"
    else
        fail "tail captures session output" "marker not found in tail pane"
    fi

    gritty kill-session local:tailtest 2>/dev/null || true
    tmux kill-session -t feat 2>/dev/null || true
}

# ---------------------------------------------------------------------------
# 2. Local forward -- TCP port forwarding
# ---------------------------------------------------------------------------
test_local_forward() {
    tmux new-session -d -s feat -x 120 -y 40
    tmux send-keys -t feat 'gritty connect local:fwdtest' Enter
    sleep 3

    # Start a TCP listener inside the session that replies with a marker
    tmux send-keys -t feat 'while true; do echo "FWD_REPLY_OK" | nc -l -p 18765 -q 0 2>/dev/null || break; done &' Enter
    sleep 1

    # Request local-forward from inside the session (lf works on local server)
    tmux send-keys -t feat 'gritty lf 18765 &' Enter
    sleep 2

    # Connect to the forwarded port from outside and check response
    local response
    response=$(echo "hello" | nc -w 2 127.0.0.1 18765 2>/dev/null) || true

    if echo "$response" | grep -qF "FWD_REPLY_OK"; then
        pass "local forward data roundtrip"
    else
        fail "local forward data roundtrip" "response: $response"
    fi

    tmux send-keys -t feat Enter
    sleep 0.5
    tmux send-keys -t feat '~.'
    sleep 2
    gritty kill-session local:fwdtest 2>/dev/null || true
    tmux kill-session -t feat 2>/dev/null || true
}

# ---------------------------------------------------------------------------
# Run all tests
# ---------------------------------------------------------------------------
echo "=== gritty feature tests ==="
echo ""

test_tail
test_local_forward

# Cleanup
gritty kill-server local 2>/dev/null || true
tmux kill-server 2>/dev/null || true

echo ""
echo "=== $passed/$total passed, $failed failed ==="

if [ "$failed" -gt 0 ]; then
    exit 1
fi
