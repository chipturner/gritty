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
# 10. Socket self-heal -- external wipe of the socket dir; the daemon must
#     re-bind in place (same pid, sessions preserved) instead of becoming an
#     unreachable orphan.
# ---------------------------------------------------------------------------
test_socket_wipe_self_heal() {
    reset_state
    # Fast self-heal interval so the test doesn't wait the production default.
    GRITTY_SOCKET_CHECK_SECS=1 gritty server >/dev/null 2>&1 || true
    wait_for_daemon local 10 || {
        fail "self-heal: server started" ""
        return
    }
    local sock_dir pid
    sock_dir=$(dirname "$(gritty socket-path)")
    pid=$(cat "${sock_dir}/daemon.pid")

    # External cleanup (tmpfiles/systemd-style) deletes socket + registration.
    rm -f "${sock_dir}/ctl.sock" "${sock_dir}/daemon.pid" "${sock_dir}/daemon.info"

    # The daemon should notice within ~1s and re-bind at the same path: ls
    # works again and the registered pid is unchanged (same process).
    if wait_for_daemon local 10 && [ "$(cat "${sock_dir}/daemon.pid" 2>/dev/null)" = "${pid}" ]; then
        pass "self-heal: daemon re-binds after external socket wipe"
    else
        fail "self-heal: daemon re-binds after external socket wipe" \
            "registered pid: $(cat "${sock_dir}/daemon.pid" 2>&1), original: ${pid}"
    fi
    gritty kill-server local 2>/dev/null || true
}

# ---------------------------------------------------------------------------
# 11. Orphan detection + reaping -- a daemon that cannot self-heal (stand-in
#     for an old-binary daemon) must be reported by doctor and reaped by
#     refresh after the confirm window.
# ---------------------------------------------------------------------------
test_orphan_reaping() {
    reset_state
    # Park self-heal so the wiped daemon stays orphaned, like an old binary.
    GRITTY_SOCKET_CHECK_SECS=3600 gritty server >/dev/null 2>&1 || true
    wait_for_daemon local 10 || {
        fail "orphan: server started" ""
        return
    }
    local sock_dir orphan_pid
    sock_dir=$(dirname "$(gritty socket-path)")
    orphan_pid=$(cat "${sock_dir}/daemon.pid")
    cleanup_push "kill -9 ${orphan_pid} 2>/dev/null"

    # Simulate systemd wiping the runtime dir out from under the daemon.
    rm -rf "${sock_dir}"

    if ! kill -0 "${orphan_pid}" 2>/dev/null; then
        fail "orphan: daemon still running after wipe" "daemon died unexpectedly"
        return
    fi

    # Capture first: doctor exits 1 when it finds problems (the orphan), and
    # pipefail would otherwise sink the grep result.
    local doctor_out
    doctor_out=$(gritty doctor 2>&1) || true
    if echo "${doctor_out}" | grep -q "orphaned daemon"; then
        pass "orphan: doctor reports orphaned daemon"
    else
        fail "orphan: doctor reports orphaned daemon" "$(echo "${doctor_out}" | tail -5)"
    fi

    # refresh reaps it (includes the ~7s self-heal grace window).
    gritty refresh local 2>&1 || true
    if wait_for_process_dead "${orphan_pid}" 5; then
        pass "orphan: refresh reaps unrecoverable orphan"
    else
        fail "orphan: refresh reaps unrecoverable orphan" \
            "pid ${orphan_pid} still alive: state=$(awk '{print $3}' "/proc/${orphan_pid}/stat" 2>/dev/null)"
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
test_socket_wipe_self_heal
test_orphan_reaping

tmux kill-server 2>/dev/null || true

report_and_exit
