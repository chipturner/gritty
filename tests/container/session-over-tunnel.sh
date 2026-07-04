#!/bin/bash
# End-to-end "remote session over SSH tunnel" tests. This exercises gritty's
# core promise -- a real PTY session that survives the network dying -- via
# the user-facing CLI, not the wire protocol directly.
set -eou pipefail

. /tests/helpers.sh
. /tests/ssh-setup.sh

setup_ssh

trap run_cleanups EXIT

# ---------------------------------------------------------------------------
# 1. Create + attach + run command via tunnel
# ---------------------------------------------------------------------------
test_session_over_tunnel() {
    reset_state
    local tunnel="t-basic"
    local session="${tunnel}:basic"

    gritty tunnel-create localhost -n "${tunnel}" >/dev/null
    cleanup_push "gritty kill-server ${tunnel} 2>/dev/null; gritty tunnel-destroy ${tunnel} 2>/dev/null"
    wait_for_daemon "${tunnel}" 10 || {
        fail "session over tunnel: daemon reachable" "ls ${tunnel} never succeeded"
        return
    }

    tmux new-session -d -s sot -x 120 -y 40
    cleanup_push "tmux kill-session -t sot 2>/dev/null"
    tmux send-keys -t sot "gritty connect ${session}" Enter
    wait_for_session "${tunnel}" "basic" 10 || {
        fail "session over tunnel: session created" "session never appeared"
        return
    }

    tmux send-keys -t sot 'echo SOT_MARKER_111' Enter
    if wait_for_text SOT_MARKER_111 sot 5; then
        pass "remote session: interactive command roundtrip"
    else
        fail "remote session: interactive command roundtrip" "marker not seen"
        tmux capture-pane -t sot -p -S -
    fi
}

# ---------------------------------------------------------------------------
# 2. Kill tunnel mid-session -- supervisor respawns, client reconnects, output
#    that arrived while disconnected is replayed.
# ---------------------------------------------------------------------------
test_tunnel_death_resume() {
    reset_state
    local tunnel="t-resume"
    local session="${tunnel}:resume"
    local socket_dir
    socket_dir=$(dirname "$(gritty socket-path)")

    gritty tunnel-create localhost -n "${tunnel}" >/dev/null
    cleanup_push "gritty kill-server ${tunnel} 2>/dev/null; gritty tunnel-destroy ${tunnel} 2>/dev/null"
    wait_for_daemon "${tunnel}" 10 || {
        fail "tunnel resume: daemon reachable" "ls ${tunnel} never succeeded"
        return
    }

    tmux new-session -d -s sot2 -x 120 -y 40
    cleanup_push "tmux kill-session -t sot2 2>/dev/null"
    tmux send-keys -t sot2 "gritty connect ${session}" Enter
    wait_for_session "${tunnel}" "resume" 10 || {
        fail "tunnel resume: session created" "session never appeared"
        return
    }

    # Schedule a marker to fire *after* the tunnel dies so it lands in the
    # server's ring buffer and must be replayed on reconnect.
    tmux send-keys -t sot2 '(sleep 3 && echo RESUME_MARKER_222) &' Enter
    sleep 1

    # Kill the SSH tunnel supervisor by removing its flock + signalling its PID.
    # The supervisor will respawn (1s backoff) and the client will reconnect.
    local tunnel_pid
    tunnel_pid=$(cat "${socket_dir}/connect-${tunnel}.pid" 2>/dev/null || true)
    if [ -z "${tunnel_pid}" ]; then
        fail "tunnel resume: supervisor pid file" "no pidfile"
        return
    fi
    kill -KILL "${tunnel_pid}" 2>/dev/null || true

    # Wait for the supervisor to be gone, then bring a fresh tunnel back up
    # (mimicking what `restart` does, and what auto-start does on next connect).
    wait_for_file_gone "${socket_dir}/connect-${tunnel}.sock" 10 || true
    gritty tunnel-create localhost -n "${tunnel}" >/dev/null
    wait_for_daemon "${tunnel}" 15 || {
        fail "tunnel resume: daemon reachable after respawn" ""
        return
    }

    # The client's reconnect loop should rebind and replay the buffered marker.
    if wait_for_text RESUME_MARKER_222 sot2 20; then
        pass "remote session: tunnel death + reconnect + replay"
    else
        fail "remote session: tunnel death + reconnect + replay" "marker not replayed"
        tmux capture-pane -t sot2 -p -S -
    fi
}

# ---------------------------------------------------------------------------
# 3. List sessions through tunnel matches local view
# ---------------------------------------------------------------------------
test_list_matches() {
    reset_state
    local tunnel="t-list"

    gritty tunnel-create localhost -n "${tunnel}" >/dev/null
    cleanup_push "gritty kill-server ${tunnel} 2>/dev/null; gritty tunnel-destroy ${tunnel} 2>/dev/null"
    wait_for_daemon "${tunnel}" 10 || {
        fail "list matches: daemon reachable" ""
        return
    }

    tmux new-session -d -s sot3 -x 120 -y 40
    cleanup_push "tmux kill-session -t sot3 2>/dev/null"
    tmux send-keys -t sot3 "gritty connect ${tunnel}:listme" Enter
    wait_for_session "${tunnel}" "listme" 10 || {
        fail "list matches: session created" ""
        return
    }

    # Detach so we can compare ls output cleanly.
    tmux send-keys -t sot3 Enter
    sleep 0.3
    tmux send-keys -t sot3 '~.'
    wait_for_session "${tunnel}" "listme" 5 || true

    # Compare wire names (`.name`), which are vantage-independent -- the
    # display column elides the viewer's own client prefix.
    local remote_view
    remote_view=$(gritty ls "${tunnel}" --json | jq -r '.[].sessions[].name' | sort)

    # SSH to the same host (over the existing tunnel's underlying connection)
    # and inspect the local view -- they should agree on session names.
    local local_view
    local_view=$(ssh -o BatchMode=yes localhost "$(command -v gritty) ls local --json" 2>/dev/null | jq -r '.[].sessions[].name' | sort)

    if [ "${remote_view}" = "${local_view}" ]; then
        pass "remote ls matches local ls on same host"
    else
        fail "remote ls matches local ls on same host" "remote='${remote_view}' local='${local_view}'"
    fi
}

# ---------------------------------------------------------------------------
# Run all tests
# ---------------------------------------------------------------------------
echo "=== gritty session-over-tunnel tests ==="
echo ""

test_session_over_tunnel
test_tunnel_death_resume
test_list_matches

# Final cleanup catches anything not popped by per-test cleanups.
gritty kill-server local 2>/dev/null || true

report_and_exit
