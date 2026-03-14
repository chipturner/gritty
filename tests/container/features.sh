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

wait_for_file() {
    local path=$1 timeout=${2:-10}
    for i in $(seq 1 "$timeout"); do
        if [ -e "$path" ]; then
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
    # Start server + session
    gritty server >/dev/null 2>&1
    sleep 1

    tmux new-session -d -s feat -x 120 -y 40
    tmux send-keys -t feat 'gritty connect local:tailtest' Enter
    sleep 3

    # Echo a marker inside the session
    tmux send-keys -t feat 'echo TAIL_MARKER_xyz' Enter
    sleep 1

    # Run tail in background and capture output
    local tailout=/tmp/tail-output.txt
    gritty tail local:tailtest > "$tailout" 2>&1 &
    local tail_pid=$!
    sleep 2
    kill "$tail_pid" 2>/dev/null || true
    wait "$tail_pid" 2>/dev/null || true

    if grep -qF TAIL_MARKER_xyz "$tailout"; then
        pass "tail captures session output"
    else
        fail "tail captures session output" "marker not in tail output: $(cat "$tailout")"
    fi

    # Detach
    tmux send-keys -t feat Enter
    sleep 0.5
    tmux send-keys -t feat '~.'
    sleep 2
    gritty kill-session local:tailtest 2>/dev/null || true
    tmux kill-session -t feat 2>/dev/null || true
}

# ---------------------------------------------------------------------------
# 2. Send + Receive -- file transfer between sessions
# ---------------------------------------------------------------------------
test_send_receive() {
    # Create a file to send
    echo "gritty-transfer-test-content-42" > /tmp/send-test.txt

    tmux new-session -d -s feat -x 120 -y 40
    tmux send-keys -t feat 'gritty connect local:xfer' Enter
    sleep 3

    # Start receiver inside the session
    tmux send-keys -t feat 'gritty receive /tmp/recv-dir &' Enter
    sleep 2

    # Send the file from outside
    gritty send --session local:xfer /tmp/send-test.txt 2>&1 || true
    sleep 2

    # Check if file was received
    if [ -f /tmp/recv-dir/send-test.txt ] && grep -qF "gritty-transfer-test-content-42" /tmp/recv-dir/send-test.txt; then
        pass "send + receive file transfer"
    else
        fail "send + receive file transfer" "received file missing or wrong content"
    fi

    tmux send-keys -t feat Enter
    sleep 0.5
    tmux send-keys -t feat '~.'
    sleep 2
    gritty kill-session local:xfer 2>/dev/null || true
    tmux kill-session -t feat 2>/dev/null || true
    rm -rf /tmp/send-test.txt /tmp/recv-dir
}

# ---------------------------------------------------------------------------
# 3. Local forward -- TCP port forwarding from session to client
# ---------------------------------------------------------------------------
test_local_forward() {
    tmux new-session -d -s feat -x 120 -y 40
    tmux send-keys -t feat 'gritty connect local:fwdtest' Enter
    sleep 3

    # Start a TCP echo server inside the session on a known port
    tmux send-keys -t feat 'while true; do echo "ECHO_REPLY" | nc -l -p 18765 -q 0; done &' Enter
    sleep 1

    # Request local-forward from outside
    gritty lf --session local:fwdtest 18765 &
    local fwd_pid=$!
    sleep 2

    # Connect to the forwarded port and check response
    local response
    response=$(echo "hello" | nc -w 2 127.0.0.1 18765 2>/dev/null) || true

    if echo "$response" | grep -qF "ECHO_REPLY"; then
        pass "local forward data roundtrip"
    else
        fail "local forward data roundtrip" "response: $response"
    fi

    kill "$fwd_pid" 2>/dev/null || true
    wait "$fwd_pid" 2>/dev/null || true
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
test_send_receive
test_local_forward

# Cleanup
gritty kill-server local 2>/dev/null || true
tmux kill-server 2>/dev/null || true

echo ""
echo "=== $passed/$total passed, $failed failed ==="

if [ "$failed" -gt 0 ]; then
    exit 1
fi
