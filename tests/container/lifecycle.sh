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
        # -S - captures full scrollback, not just visible area
        if tmux capture-pane -t "$pane" -p -S - 2>/dev/null | grep -qF "$target"; then
            return 0
        fi
        sleep 1
    done
    return 1
}

capture() {
    tmux capture-pane -t "$1" -p -S - 2>/dev/null
}

# ---------------------------------------------------------------------------
# 1. Server auto-start + interactive session
# ---------------------------------------------------------------------------
test_connect_and_interactive() {
    tmux new-session -d -s t -x 120 -y 40
    tmux send-keys -t t 'gritty connect local:test1' Enter
    sleep 3  # server auto-start + shell spawn

    # Verify we got a shell by sending a command
    tmux send-keys -t t 'echo MARKER_aaa111' Enter
    if wait_for_text MARKER_aaa111 t 5; then
        pass "server auto-start + interactive session"
    else
        fail "server auto-start + interactive session" "MARKER not found in pane output"
        capture t
    fi
}

# ---------------------------------------------------------------------------
# 2. Session listing
# ---------------------------------------------------------------------------
test_session_listing() {
    local output
    output=$(gritty ls local 2>&1) || true
    if echo "$output" | grep -q 'test1'; then
        pass "session listing shows test1"
    else
        fail "session listing shows test1" "output: $output"
    fi
}

# ---------------------------------------------------------------------------
# 3. Detach via escape sequence
# ---------------------------------------------------------------------------
test_detach() {
    # ~ requires a preceding newline
    tmux send-keys -t t Enter
    sleep 0.5
    tmux send-keys -t t '~.'
    sleep 2

    # After detach, the gritty connect process should have exited.
    # The pane should be back at a shell prompt (or show "detached").
    # Verify the session is still alive on the server.
    local output
    output=$(gritty ls local 2>&1) || true
    if echo "$output" | grep -q 'test1'; then
        pass "detach via ~. (session persists)"
    else
        fail "detach via ~. (session persists)" "session not found after detach: $output"
    fi
}

# ---------------------------------------------------------------------------
# 4. Reattach + ring buffer
# ---------------------------------------------------------------------------
test_reattach_ring_buffer() {
    tmux send-keys -t t 'gritty connect local:test1' Enter
    sleep 2

    # Ring buffer should replay previous output including our marker
    if wait_for_text MARKER_aaa111 t 5; then
        pass "reattach + ring buffer preserves output"
    else
        fail "reattach + ring buffer preserves output" "MARKER not found after reattach"
        capture t
    fi

    # Detach again for subsequent tests
    tmux send-keys -t t Enter
    sleep 0.5
    tmux send-keys -t t '~.'
    sleep 2
}

# ---------------------------------------------------------------------------
# 5. Multi-session
# ---------------------------------------------------------------------------
test_multi_session() {
    tmux send-keys -t t 'gritty connect local:test2' Enter
    sleep 3

    # Verify we're in a working session
    tmux send-keys -t t 'echo MARKER_bbb222' Enter
    if ! wait_for_text MARKER_bbb222 t 5; then
        fail "multi-session create" "second session not interactive"
        capture t
        return
    fi

    # Detach
    tmux send-keys -t t Enter
    sleep 0.5
    tmux send-keys -t t '~.'
    sleep 2

    # Both sessions should appear in listing
    local output
    output=$(gritty ls local 2>&1) || true
    if echo "$output" | grep -q 'test1' && echo "$output" | grep -q 'test2'; then
        pass "multi-session (both listed)"
    else
        fail "multi-session (both listed)" "output: $output"
    fi
}

# ---------------------------------------------------------------------------
# 6. Session rename
# ---------------------------------------------------------------------------
test_rename() {
    gritty rename local:test2 renamed 2>&1 || true
    local output
    output=$(gritty ls local 2>&1) || true
    if echo "$output" | grep -q 'renamed'; then
        pass "session rename"
    else
        fail "session rename" "output: $output"
    fi
}

# ---------------------------------------------------------------------------
# 7. Session kill
# ---------------------------------------------------------------------------
test_kill_session() {
    gritty kill-session local:renamed 2>&1 || true
    sleep 1
    local output
    output=$(gritty ls local 2>&1) || true
    if echo "$output" | grep -q 'test1' && ! echo "$output" | grep -q 'renamed'; then
        pass "kill-session (test1 survives, renamed gone)"
    else
        fail "kill-session (test1 survives, renamed gone)" "output: $output"
    fi
}

# ---------------------------------------------------------------------------
# 8. Info command
# ---------------------------------------------------------------------------
test_info() {
    local output
    output=$(gritty info 2>&1) || true
    if echo "$output" | grep -qi 'socket\|path\|version'; then
        pass "info command"
    else
        fail "info command" "output: $output"
    fi
}

# ---------------------------------------------------------------------------
# 9. Server kill + cleanup
# ---------------------------------------------------------------------------
test_kill_server() {
    gritty kill-server local 2>&1 || true
    sleep 1
    local output
    if output=$(gritty ls local 2>&1); then
        fail "kill-server (ls should fail)" "ls succeeded: $output"
    else
        pass "kill-server + cleanup"
    fi
}

# ---------------------------------------------------------------------------
# Run all tests
# ---------------------------------------------------------------------------
echo "=== gritty container lifecycle test ==="
echo ""

test_connect_and_interactive
test_session_listing
test_detach
test_reattach_ring_buffer
test_multi_session
test_rename
test_kill_session
test_info
test_kill_server

# Cleanup
tmux kill-server 2>/dev/null || true

echo ""
echo "=== $passed/$total passed, $failed failed ==="

if [ "$failed" -gt 0 ]; then
    exit 1
fi
