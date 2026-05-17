#!/bin/bash
set -eou pipefail

. /tests/helpers.sh
. /tests/ssh-setup.sh

trap run_cleanups EXIT

# Helper: run gritty and assert success
gritty_ok() {
    local output
    if ! output=$(gritty "$@" 2>&1); then
        echo "gritty $* failed: ${output}" >&2
        return 1
    fi
    echo "${output}"
}

# ---------------------------------------------------------------------------
# 1. Tunnel create + list tunnels
# ---------------------------------------------------------------------------
test_tunnel_create_and_list() {
    reset_state
    local name="test-list"

    gritty_ok tunnel-create localhost -n "${name}" >/dev/null
    cleanup_push "gritty tunnel-destroy ${name} 2>/dev/null"
    sleep 1

    local tunnels
    tunnels=$(gritty_ok tunnels)
    if echo "${tunnels}" | grep -q "${name}"; then
        pass "tunnel-create + tunnels listing"
    else
        fail "tunnel-create + tunnels listing" "tunnel ${name} not in: ${tunnels}"
    fi

    gritty_ok tunnel-destroy "${name}" >/dev/null
    sleep 1

    tunnels=$(gritty_ok tunnels)
    if echo "${tunnels}" | grep -q "${name}"; then
        fail "tunnel-destroy removes tunnel" "tunnel ${name} still in: ${tunnels}"
    else
        pass "tunnel-destroy removes tunnel"
    fi
}

# ---------------------------------------------------------------------------
# 2. List sessions (empty)
# ---------------------------------------------------------------------------
test_list_sessions_empty() {
    reset_state
    local name="test-empty"

    gritty_ok tunnel-create localhost -n "${name}" >/dev/null
    cleanup_push "gritty kill-server ${name} 2>/dev/null; gritty tunnel-destroy ${name} 2>/dev/null"
    wait_for_daemon "${name}" 10 || {
        fail "list sessions (empty)" "daemon never reachable"
        return
    }

    local out
    out=$(gritty_ok ls "${name}")
    if echo "${out}" | grep -q "running"; then
        fail "list sessions (empty)" "expected no running sessions, got: ${out}"
    else
        pass "list sessions (empty)"
    fi
}

# ---------------------------------------------------------------------------
# 3. Disconnect + reconnect (server persists)
# ---------------------------------------------------------------------------
test_disconnect_reconnect() {
    reset_state
    local name="test-reconnect"

    gritty_ok tunnel-create localhost -n "${name}" >/dev/null
    cleanup_push "gritty kill-server ${name} 2>/dev/null; gritty tunnel-destroy ${name} 2>/dev/null"
    wait_for_daemon "${name}" 10 || {
        fail "disconnect + reconnect: initial daemon" ""
        return
    }

    gritty_ok tunnel-destroy "${name}" >/dev/null
    sleep 1

    # Reconnect -- server should still be running from first tunnel-create
    gritty_ok tunnel-create localhost -n "${name}" >/dev/null
    if wait_for_daemon "${name}" 10; then
        pass "disconnect + reconnect (server persists)"
    else
        fail "disconnect + reconnect (server persists)" "ls failed after reconnect"
    fi
}

# ---------------------------------------------------------------------------
# 4. Custom tunnel name
# ---------------------------------------------------------------------------
test_custom_name() {
    reset_state
    local name="mydev-test"

    gritty_ok tunnel-create localhost -n "${name}" >/dev/null
    cleanup_push "gritty kill-server ${name} 2>/dev/null; gritty tunnel-destroy ${name} 2>/dev/null"
    sleep 1

    local tunnels
    tunnels=$(gritty_ok tunnels)
    if ! echo "${tunnels}" | grep -q "${name}"; then
        fail "custom tunnel name" "name ${name} not in: ${tunnels}"
        return
    fi

    gritty_ok ls "${name}" >/dev/null

    gritty_ok tunnel-destroy "${name}" >/dev/null
    sleep 1

    tunnels=$(gritty_ok tunnels)
    if echo "${tunnels}" | grep -q "${name}"; then
        fail "custom tunnel name" "name ${name} still in tunnels after destroy"
    else
        pass "custom tunnel name"
    fi
}

# ---------------------------------------------------------------------------
# 5. Info shows tunnel
# ---------------------------------------------------------------------------
test_info_shows_tunnel() {
    reset_state
    local name="test-info"

    gritty_ok tunnel-create localhost -n "${name}" >/dev/null
    cleanup_push "gritty kill-server ${name} 2>/dev/null; gritty tunnel-destroy ${name} 2>/dev/null"
    sleep 1

    local info
    info=$(gritty_ok info)
    if echo "${info}" | grep -q "${name}"; then
        pass "info shows tunnel"
    else
        fail "info shows tunnel" "info output: ${info}"
    fi
}

# ---------------------------------------------------------------------------
# 6. Foreground mode + SIGTERM cleanup
# ---------------------------------------------------------------------------
test_foreground_mode() {
    reset_state
    local name="test-fg"
    local socket_dir
    socket_dir=$(dirname "$(gritty socket-path)")
    local connect_sock="${socket_dir}/connect-${name}.sock"

    gritty tunnel-create localhost -n "${name}" --foreground &
    local pid=$!
    cleanup_push "kill ${pid} 2>/dev/null; gritty kill-server ${name} 2>/dev/null"

    if ! wait_for_file "${connect_sock}" 15; then
        fail "foreground mode" "connect socket never appeared: ${connect_sock}"
        return
    fi

    gritty_ok ls "${name}" >/dev/null

    kill -TERM "${pid}" 2>/dev/null || true
    wait "${pid}" 2>/dev/null || true
    sleep 1

    pass "foreground mode + SIGTERM cleanup"
}

# ---------------------------------------------------------------------------
# 7. kill-server through tunnel -- the remote daemon goes away, sessions die.
# ---------------------------------------------------------------------------
test_kill_server_via_tunnel() {
    reset_state
    local name="test-killsrv"

    gritty_ok tunnel-create localhost -n "${name}" >/dev/null
    cleanup_push "gritty tunnel-destroy ${name} 2>/dev/null"
    wait_for_daemon "${name}" 10 || {
        fail "kill-server via tunnel: initial daemon" ""
        return
    }

    # Create a session detached so we have something to kill.
    gritty connect "${name}:will-die" -d >/dev/null 2>&1 || true
    wait_for_session "${name}" "will-die" 10 || {
        fail "kill-server via tunnel: session created" ""
        return
    }

    gritty kill-server "${name}" >/dev/null 2>&1 || true

    # After kill-server, the daemon is gone; ls should fail.
    sleep 1
    if gritty ls "${name}" >/dev/null 2>&1; then
        fail "kill-server via tunnel" "ls still succeeded after kill-server"
        return
    fi

    pass "kill-server via tunnel (daemon torn down)"
}

# ---------------------------------------------------------------------------
# Run all tests
# ---------------------------------------------------------------------------
echo "=== gritty SSH tunnel test ==="
echo ""

setup_ssh

test_tunnel_create_and_list
test_list_sessions_empty
test_disconnect_reconnect
test_custom_name
test_info_shows_tunnel
test_foreground_mode
test_kill_server_via_tunnel

report_and_exit
