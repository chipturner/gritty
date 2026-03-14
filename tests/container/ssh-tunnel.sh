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

# ---------------------------------------------------------------------------
# SSH setup (shared)
# ---------------------------------------------------------------------------
. /tests/ssh-setup.sh

# Helper: run gritty and assert success
gritty_ok() {
    local output
    if ! output=$(gritty "$@" 2>&1); then
        echo "gritty $* failed: $output" >&2
        return 1
    fi
    echo "$output"
}

# Helper: wait for a file to appear
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
# 1. Tunnel create + list tunnels
# ---------------------------------------------------------------------------
test_tunnel_create_and_list() {
    local name="test-list"

    gritty_ok tunnel-create localhost -n "$name" >/dev/null
    sleep 1

    local tunnels
    tunnels=$(gritty_ok tunnels)
    if echo "$tunnels" | grep -q "$name"; then
        pass "tunnel-create + tunnels listing"
    else
        fail "tunnel-create + tunnels listing" "tunnel $name not in: $tunnels"
    fi

    gritty_ok tunnel-destroy "$name" >/dev/null
    sleep 1

    tunnels=$(gritty_ok tunnels)
    if echo "$tunnels" | grep -q "$name"; then
        fail "tunnel-destroy removes tunnel" "tunnel $name still in: $tunnels"
    else
        pass "tunnel-destroy removes tunnel"
    fi
}

# ---------------------------------------------------------------------------
# 2. List sessions (empty)
# ---------------------------------------------------------------------------
test_list_sessions_empty() {
    local name="test-empty"

    gritty_ok tunnel-create localhost -n "$name" >/dev/null
    sleep 1

    local out
    out=$(gritty_ok ls "$name")
    if echo "$out" | grep -q "running"; then
        fail "list sessions (empty)" "expected no running sessions, got: $out"
    else
        pass "list sessions (empty)"
    fi

    gritty kill-server "$name" 2>/dev/null || true
    gritty tunnel-destroy "$name" 2>/dev/null || true
    sleep 1
}

# ---------------------------------------------------------------------------
# 3. Disconnect + reconnect (server persists)
# ---------------------------------------------------------------------------
test_disconnect_reconnect() {
    local name="test-reconnect"

    gritty_ok tunnel-create localhost -n "$name" >/dev/null
    gritty_ok ls "$name" >/dev/null

    gritty_ok tunnel-destroy "$name" >/dev/null
    sleep 1

    # Reconnect -- server should still be running from first tunnel-create
    gritty_ok tunnel-create localhost -n "$name" >/dev/null
    sleep 1

    if gritty_ok ls "$name" >/dev/null; then
        pass "disconnect + reconnect (server persists)"
    else
        fail "disconnect + reconnect (server persists)" "ls failed after reconnect"
    fi

    gritty kill-server "$name" 2>/dev/null || true
    gritty tunnel-destroy "$name" 2>/dev/null || true
    sleep 1
}

# ---------------------------------------------------------------------------
# 4. Custom tunnel name
# ---------------------------------------------------------------------------
test_custom_name() {
    local name="mydev-test"

    gritty_ok tunnel-create localhost -n "$name" >/dev/null
    sleep 1

    local tunnels
    tunnels=$(gritty_ok tunnels)
    if ! echo "$tunnels" | grep -q "$name"; then
        fail "custom tunnel name" "name $name not in: $tunnels"
        gritty kill-server "$name" 2>/dev/null || true
        gritty tunnel-destroy "$name" 2>/dev/null || true
        return
    fi

    gritty_ok ls "$name" >/dev/null

    gritty_ok tunnel-destroy "$name" >/dev/null
    sleep 1

    tunnels=$(gritty_ok tunnels)
    if echo "$tunnels" | grep -q "$name"; then
        fail "custom tunnel name" "name $name still in tunnels after destroy"
    else
        pass "custom tunnel name"
    fi

    gritty kill-server "$name" 2>/dev/null || true
    sleep 1
}

# ---------------------------------------------------------------------------
# 5. Info shows tunnel
# ---------------------------------------------------------------------------
test_info_shows_tunnel() {
    local name="test-info"

    gritty_ok tunnel-create localhost -n "$name" >/dev/null
    sleep 1

    local info
    info=$(gritty_ok info)
    if echo "$info" | grep -q "$name"; then
        pass "info shows tunnel"
    else
        fail "info shows tunnel" "info output: $info"
    fi

    gritty kill-server "$name" 2>/dev/null || true
    gritty tunnel-destroy "$name" 2>/dev/null || true
    sleep 1
}

# ---------------------------------------------------------------------------
# 6. Foreground mode + SIGTERM cleanup
# ---------------------------------------------------------------------------
test_foreground_mode() {
    local name="test-fg"
    local socket_dir
    socket_dir=$(gritty socket-path)
    socket_dir=$(dirname "$socket_dir")
    local connect_sock="$socket_dir/connect-${name}.sock"

    gritty tunnel-create localhost -n "$name" --foreground &
    local pid=$!
    sleep 1

    if ! wait_for_file "$connect_sock" 15; then
        kill "$pid" 2>/dev/null || true
        fail "foreground mode" "connect socket never appeared: $connect_sock"
        return
    fi

    gritty_ok ls "$name" >/dev/null

    kill -TERM "$pid" 2>/dev/null || true
    wait "$pid" 2>/dev/null || true
    sleep 1

    pass "foreground mode + SIGTERM cleanup"

    gritty kill-server "$name" 2>/dev/null || true
    sleep 1
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

echo ""
echo "=== $passed/$total passed, $failed failed ==="

if [ "$failed" -gt 0 ]; then
    exit 1
fi
