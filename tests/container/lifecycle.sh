#!/bin/bash
set -eou pipefail

. /tests/helpers.sh

trap run_cleanups EXIT

capture() {
    tmux capture-pane -t "${1}" -p -S - 2>/dev/null
}

# ---------------------------------------------------------------------------
# 1. Server auto-start + interactive session
# ---------------------------------------------------------------------------
test_connect_and_interactive() {
    reset_state
    tmux new-session -d -s t -x 120 -y 40
    cleanup_push "tmux kill-session -t t 2>/dev/null"
    tmux send-keys -t t 'gritty connect local:test1' Enter

    wait_for_session local "test1" 10 || {
        fail "server auto-start + interactive session" "session never created"
        capture t
        return
    }

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
    if session_exists local "test1"; then
        pass "session listing shows test1 (exact-name match)"
    else
        fail "session listing shows test1 (exact-name match)" "ls=$(gritty ls local 2>&1)"
    fi
}

# ---------------------------------------------------------------------------
# 3. Detach via escape sequence
# ---------------------------------------------------------------------------
test_detach() {
    # ~ requires a preceding newline
    tmux send-keys -t t Enter
    sleep 0.3
    tmux send-keys -t t '~.'
    sleep 1

    if session_exists local "test1"; then
        pass "detach via ~. (session persists)"
    else
        fail "detach via ~. (session persists)" "ls=$(gritty ls local 2>&1)"
    fi
}

# ---------------------------------------------------------------------------
# 4. Reattach + ring buffer
# ---------------------------------------------------------------------------
test_reattach_ring_buffer() {
    tmux send-keys -t t 'gritty connect local:test1' Enter
    sleep 1

    if wait_for_text MARKER_aaa111 t 5; then
        pass "reattach + ring buffer preserves output"
    else
        fail "reattach + ring buffer preserves output" "MARKER not found after reattach"
        capture t
    fi

    tmux send-keys -t t Enter
    sleep 0.3
    tmux send-keys -t t '~.'
    sleep 1
}

# ---------------------------------------------------------------------------
# 5. Multi-session
# ---------------------------------------------------------------------------
test_multi_session() {
    tmux send-keys -t t 'gritty connect local:test2' Enter
    wait_for_session local "test2" 10 || {
        fail "multi-session create" "second session never appeared"
        capture t
        return
    }

    tmux send-keys -t t 'echo MARKER_bbb222' Enter
    if ! wait_for_text MARKER_bbb222 t 5; then
        fail "multi-session create" "second session not interactive"
        capture t
        return
    fi

    tmux send-keys -t t Enter
    sleep 0.3
    tmux send-keys -t t '~.'
    sleep 1

    if session_exists local "test1" && session_exists local "test2"; then
        pass "multi-session (both listed, exact match)"
    else
        fail "multi-session (both listed, exact match)" "ls=$(gritty ls local 2>&1)"
    fi
}

# ---------------------------------------------------------------------------
# 6. Session rename
# ---------------------------------------------------------------------------
test_rename() {
    gritty rename local:test2 renamed 2>&1 || true
    if session_exists local "renamed" && ! session_exists local "test2"; then
        pass "session rename"
    else
        fail "session rename" "ls=$(gritty ls local 2>&1)"
    fi
}

# ---------------------------------------------------------------------------
# 7. Session kill
# ---------------------------------------------------------------------------
test_kill_session() {
    gritty kill-session local:renamed 2>&1 || true
    wait_for_session_gone local "renamed" 5 || true

    if session_exists local "test1" && ! session_exists local "renamed"; then
        pass "kill-session (test1 survives, renamed gone)"
    else
        fail "kill-session (test1 survives, renamed gone)" "ls=$(gritty ls local 2>&1)"
    fi
}

# ---------------------------------------------------------------------------
# 8. Info command
# ---------------------------------------------------------------------------
test_info() {
    local output
    output=$(gritty info 2>&1) || true
    if echo "${output}" | grep -qiE 'socket|path|version'; then
        pass "info command"
    else
        fail "info command" "output: ${output}"
    fi
}

# ---------------------------------------------------------------------------
# 9. Server kill + cleanup
# ---------------------------------------------------------------------------
test_kill_server() {
    gritty kill-server local 2>&1 || true
    sleep 1
    if gritty ls local >/dev/null 2>&1; then
        fail "kill-server (ls should fail)" "ls succeeded: $(gritty ls local 2>&1)"
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

tmux kill-server 2>/dev/null || true

report_and_exit
