#!/bin/bash
set -eou pipefail

. /tests/helpers.sh

trap run_cleanups EXIT

# ---------------------------------------------------------------------------
# 1. Tail -- read-only stream of session output
# ---------------------------------------------------------------------------
test_tail() {
    reset_state
    tmux new-session -d -s feat -x 120 -y 40
    cleanup_push "tmux kill-session -t feat 2>/dev/null"
    tmux send-keys -t feat 'gritty connect local:tailtest' Enter
    wait_for_session local "tailtest" 10 || {
        fail "tail: session created" ""
        return
    }

    # Schedule marker output AFTER we detach. The ring buffer only captures
    # PTY output while no client is connected, so the marker must be echoed
    # during the disconnect window.
    tmux send-keys -t feat '(sleep 2 && echo TAIL_MARKER_xyz) &' Enter
    sleep 1

    tmux send-keys -t feat Enter
    sleep 0.2
    tmux send-keys -t feat '~.'
    sleep 3  # wait for background echo to fire

    tmux split-window -t feat 'gritty tail local:tailtest'
    if wait_for_text TAIL_MARKER_xyz feat.1 10; then
        pass "tail captures session output"
    else
        fail "tail captures session output" "marker not found in tail pane"
    fi

    gritty kill-session local:tailtest 2>/dev/null || true
}

# ---------------------------------------------------------------------------
# 2. Local forward -- session listens, client connects out.
#
# `lf 18765:19999`: daemon binds 18765 on the session side; connections to it
# are forwarded to client:19999. We place a TCP listener at the client target
# port and connect to the session listen port to exercise the forward.
# The gritty client needs to be attached so its fwd-*.sock exists; we drive
# the lf/nc commands from the main test shell so tmux send-keys timing
# doesn't get in the way.
# ---------------------------------------------------------------------------
test_local_forward() {
    reset_state
    local session_id_name="fwdtest"

    # Client-side listener (target of the forward).
    (echo "LF_REPLY_OK" | nc -l -p 19999 -q 0) &
    local listener_pid=$!
    cleanup_push "kill ${listener_pid} 2>/dev/null"
    wait_for_port 19999 5 || {
        fail "lf: client-side listener bound" "port 19999 never opened"
        return
    }

    # Attached gritty client (creates fwd-*.sock).
    tmux new-session -d -s feat -x 120 -y 40
    cleanup_push "tmux kill-session -t feat 2>/dev/null"
    tmux send-keys -t feat "gritty connect local:${session_id_name}" Enter
    wait_for_session_attached local "${session_id_name}" 10 || {
        fail "lf: session attached" "ls=$(gritty ls local 2>&1)"
        return
    }

    gritty lf "local:${session_id_name}" 18765:19999 >/tmp/lf.out 2>&1 &
    local lf_pid=$!
    cleanup_push "kill ${lf_pid} 2>/dev/null"
    wait_for_port 18765 5 || {
        fail "lf: forwarded port" "session-side port 18765 never opened; lf output: $(cat /tmp/lf.out 2>/dev/null)"
        return
    }

    local response
    response=$(echo "hello" | nc -w 2 127.0.0.1 18765 2>/dev/null) || true
    if echo "${response}" | grep -qF "LF_REPLY_OK"; then
        pass "local forward data roundtrip (session->client)"
    else
        fail "local forward data roundtrip (session->client)" "response: '${response}' lf-output: $(cat /tmp/lf.out 2>/dev/null) listener-status: $(jobs -p | head -1)"
    fi

    gritty kill-session "local:${session_id_name}" 2>/dev/null || true
}

# ---------------------------------------------------------------------------
# 3. Remote forward -- client listens, session connects out.
#
# `rf 18766:19998`: client binds 18766; connections to it forward to
# session:19998. We place a listener at the session target port and connect
# to the client listen port.
# ---------------------------------------------------------------------------
test_remote_forward() {
    reset_state
    local session_id_name="rftest"

    # Session-side listener (target of the forward). In a single container the
    # loopback is shared, so a listener "in the session" is just a process
    # bound on 127.0.0.1:19998 from the host's shell.
    (echo "RF_REPLY_OK" | nc -l -p 19998 -q 0) &
    local listener_pid=$!
    cleanup_push "kill ${listener_pid} 2>/dev/null"
    wait_for_port 19998 5 || {
        fail "rf: session-side listener bound" "port 19998 never opened"
        return
    }

    tmux new-session -d -s feat -x 120 -y 40
    cleanup_push "tmux kill-session -t feat 2>/dev/null"
    tmux send-keys -t feat "gritty connect local:${session_id_name}" Enter
    wait_for_session_attached local "${session_id_name}" 10 || {
        fail "rf: session attached" "ls=$(gritty ls local 2>&1)"
        return
    }

    gritty rf "local:${session_id_name}" 18766:19998 >/tmp/rf.out 2>&1 &
    local rf_pid=$!
    cleanup_push "kill ${rf_pid} 2>/dev/null"
    wait_for_port 18766 5 || {
        fail "rf: client-side port" "client-bound port 18766 never opened"
        return
    }

    local response
    response=$(echo "hello" | nc -w 2 127.0.0.1 18766 2>/dev/null) || true
    if echo "${response}" | grep -qF "RF_REPLY_OK"; then
        pass "remote forward data roundtrip (client->session)"
    else
        fail "remote forward data roundtrip (client->session)" "response: '${response}' rf-output: $(cat /tmp/rf.out 2>/dev/null)"
    fi

    gritty kill-session "local:${session_id_name}" 2>/dev/null || true
}

# ---------------------------------------------------------------------------
# 4. Force takeover -- second client steals an attached session
# ---------------------------------------------------------------------------
test_force_takeover() {
    reset_state
    tmux new-session -d -s feat -x 120 -y 40
    cleanup_push "tmux kill-session -t feat 2>/dev/null"

    # Both clients share the same client_name (the hostname) so a bare
    # `connect local:takeover` from each resolves to the same wire session.
    tmux send-keys -t feat 'gritty connect local:takeover' Enter
    wait_for_session local "takeover" 10 || {
        fail "takeover: session A created" ""
        return
    }
    tmux send-keys -t feat 'echo TAKEOVER_A_HERE' Enter
    wait_for_text TAKEOVER_A_HERE feat 5 || {
        fail "takeover: A interactive" ""
        return
    }

    # Probe via the daemon API directly: a second Attach on this client name
    # without --force should fail with AlreadyAttached. We do this from the
    # main test shell so we get the error directly without tmux timing games.
    local out exit_code
    out=$(gritty connect --no-create local:takeover < /dev/null 2>&1) && exit_code=0 || exit_code=$?
    if echo "${out}" | grep -qiE "already attached|alreadyattached"; then
        pass "takeover: second client rejected without --force"
    else
        fail "takeover: second client rejected without --force" "exit=${exit_code} out='${out}'"
    fi

    # With --force, the steal should succeed from a second tmux pane.
    tmux split-window -t feat -d
    tmux send-keys -t feat.1 'gritty connect --force local:takeover' Enter
    sleep 2
    tmux send-keys -t feat.1 'echo TAKEOVER_B_HERE' Enter
    if wait_for_text TAKEOVER_B_HERE feat.1 5; then
        pass "takeover: --force succeeds"
    else
        fail "takeover: --force succeeds" "B not interactive after --force"
        tmux capture-pane -t feat.1 -p -S - | tail -15
    fi

    tmux send-keys -t feat.1 Enter
    sleep 0.2
    tmux send-keys -t feat.1 '~.'
    gritty kill-session local:takeover 2>/dev/null || true
}

# ---------------------------------------------------------------------------
# Run all tests
# ---------------------------------------------------------------------------
echo "=== gritty feature tests ==="
echo ""

test_tail
test_local_forward
test_remote_forward
test_force_takeover

gritty kill-server local 2>/dev/null || true

report_and_exit
