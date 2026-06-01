#!/bin/bash
# Shared helpers for container tests. Source this file at the top of each
# test script and call `init_test_counters` once.

# Per-script counters (test scripts read/write these).
passed=0
failed=0
total=0

pass() {
    echo "PASS: ${1}"
    passed=$((passed + 1))
    total=$((total + 1))
}

fail() {
    echo "FAIL: ${1} -- ${2}"
    failed=$((failed + 1))
    total=$((total + 1))
}

# Cleanup registry. Each test pushes commands with `cleanup_push` and they run
# in LIFO order on script exit (success or failure).
cleanup_cmds=()

cleanup_push() {
    cleanup_cmds+=("${1}")
}

run_cleanups() {
    local i
    for ((i = ${#cleanup_cmds[@]} - 1; i >= 0; i--)); do
        eval "${cleanup_cmds[${i}]}" 2>/dev/null || true
    done
    cleanup_cmds=()
}

# Run + clear the current cleanup list. Call at the start of each test so the
# previous test's resources are released (tmux sessions, tunnels, sockets),
# and again from the EXIT trap as the safety net.
reset_state() {
    run_cleanups
}

# ---------------------------------------------------------------------------
# Polling helpers -- prefer these over bare `sleep` so we react to readiness
# instead of waiting out a worst-case guess.
# ---------------------------------------------------------------------------

# Wait for a tmux pane to contain a literal substring.
wait_for_text() {
    local target="${1}" pane="${2}" timeout="${3:-5}"
    local i
    for ((i = 0; i < timeout * 10; i++)); do
        if tmux capture-pane -t "${pane}" -p -S - 2>/dev/null | grep -qF "${target}"; then
            return 0
        fi
        sleep 0.1
    done
    return 1
}

# Wait for a path (file or socket) to exist.
wait_for_file() {
    local path="${1}" timeout="${2:-10}"
    local i
    for ((i = 0; i < timeout * 10; i++)); do
        if [ -e "${path}" ]; then
            return 0
        fi
        sleep 0.1
    done
    return 1
}

# Wait for a path to NOT exist (cleanup verification).
wait_for_file_gone() {
    local path="${1}" timeout="${2:-10}"
    local i
    for ((i = 0; i < timeout * 10; i++)); do
        if [ ! -e "${path}" ]; then
            return 0
        fi
        sleep 0.1
    done
    return 1
}

# Wait for a TCP port to be in LISTEN state. Reads /proc/net/tcp directly
# instead of probing with `nc -z`, which would consume a one-shot listener.
# State 0A = TCP_LISTEN per <linux/tcp_states.h>.
wait_for_port() {
    local port="${1}" timeout="${2:-10}"
    local hex_port
    hex_port=$(printf '%04X' "${port}")
    local i
    for ((i = 0; i < timeout * 10; i++)); do
        # The local_address column is "IP_HEX:PORT_HEX"; we don't pin IP so
        # 0.0.0.0 / 127.0.0.1 / ::1 all match.
        if awk -v p=":${hex_port}" '$2 ~ p"$" && $4 == "0A" {found=1} END{exit !found}' /proc/net/tcp 2>/dev/null; then
            return 0
        fi
        if awk -v p=":${hex_port}" '$2 ~ p"$" && $4 == "0A" {found=1} END{exit !found}' /proc/net/tcp6 2>/dev/null; then
            return 0
        fi
        sleep 0.1
    done
    return 1
}

# Wait until `gritty ls <host>` exit-code is 0 (daemon reachable).
wait_for_daemon() {
    local host="${1}" timeout="${2:-10}"
    local i
    for ((i = 0; i < timeout * 10; i++)); do
        if gritty ls "${host}" >/dev/null 2>&1; then
            return 0
        fi
        sleep 0.1
    done
    return 1
}

# Wait for a named session to appear in `gritty ls <host>`. Uses an exact-name
# column match (awk on column 2) so `test1` doesn't match `test10`.
wait_for_session() {
    local host="${1}" name="${2}" timeout="${3:-10}"
    local i
    for ((i = 0; i < timeout * 10; i++)); do
        if gritty ls "${host}" 2>/dev/null | awk 'NR>1 {print $2}' | grep -qFx "${name}"; then
            return 0
        fi
        sleep 0.1
    done
    return 1
}

# Wait for a session to disappear from `gritty ls`.
wait_for_session_gone() {
    local host="${1}" name="${2}" timeout="${3:-10}"
    local i
    for ((i = 0; i < timeout * 10; i++)); do
        if ! gritty ls "${host}" 2>/dev/null | awk 'NR>1 {print $2}' | grep -qFx "${name}"; then
            return 0
        fi
        sleep 0.1
    done
    return 1
}

# Strict column-aware session match. Returns 0 if a session with the exact name
# exists in `gritty ls <host>`.
session_exists() {
    local host="${1}" name="${2}"
    gritty ls "${host}" 2>/dev/null | awk 'NR>1 {print $2}' | grep -qFx "${name}"
}

# Wait until a process is dead: gone from /proc, or lingering as a zombie.
# `kill -0` alone is the wrong check inside the test container: there is no
# init (PID 1 is the test runner's bash), so a SIGKILLed daemon stays in the
# process table as a zombie that `kill -0` still "sees". Dead is dead -- a
# zombie has already released its sockets, memory, and fds.
wait_for_process_dead() {
    local pid="${1}" timeout="${2:-5}"
    local i state
    for ((i = 0; i < timeout * 10; i++)); do
        state=$(awk '{print $3}' "/proc/${pid}/stat" 2>/dev/null || echo "gone")
        if [ -z "${state}" ] || [ "${state}" = "gone" ] || [ "${state}" = "Z" ]; then
            return 0
        fi
        sleep 0.1
    done
    return 1
}

# Wait until a session is in the "attached" state in `gritty ls`. Useful before
# probing per-session resources (fwd-*.sock) that only appear once a client has
# finished its Attach handshake. The status may carry a heartbeat suffix
# ("attached (heartbeat 3s ago)"), so match the word rather than the last field.
wait_for_session_attached() {
    local host="${1}" name="${2}" timeout="${3:-10}"
    local i
    for ((i = 0; i < timeout * 10; i++)); do
        if gritty ls "${host}" 2>/dev/null | awk -v n="${name}" 'NR>1 && $2 == n && / attached/' | grep -q .; then
            return 0
        fi
        sleep 0.1
    done
    return 1
}

# Print summary and set exit code.
report_and_exit() {
    echo ""
    echo "=== ${passed}/${total} passed, ${failed} failed ==="
    if [ "${failed}" -gt 0 ]; then
        exit 1
    fi
    exit 0
}
