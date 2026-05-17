#!/bin/bash
# File-transfer tests through the gritty CLI (not the wire protocol directly).
set -eou pipefail

. /tests/helpers.sh

trap run_cleanups EXIT

# Use a workspace dir we can clean between tests.
work=/tmp/xfer
rm -rf "${work}" && mkdir -p "${work}"

# ---------------------------------------------------------------------------
# 1. Single-file round-trip
# ---------------------------------------------------------------------------
test_single_file() {
    reset_state
    local session="local:xfer1"

    tmux new-session -d -s sr -x 120 -y 40
    cleanup_push "tmux kill-session -t sr 2>/dev/null"
    tmux send-keys -t sr "gritty connect ${session}" Enter
    wait_for_session local "xfer1" 10 || {
        fail "single file: session created" ""
        return
    }

    # Detach the interactive client; transfer uses the svc socket independently.
    tmux send-keys -t sr Enter
    sleep 0.2
    tmux send-keys -t sr '~.'
    sleep 0.5

    local src="${work}/single.bin"
    local dst_dir="${work}/single-out"
    mkdir -p "${dst_dir}"

    # 256KB random payload exercises chunking.
    head -c 262144 /dev/urandom > "${src}"
    local src_sum
    src_sum=$(sha256sum "${src}" | awk '{print $1}')

    gritty receive --session "${session}" --timeout 30 "${dst_dir}" >/dev/null &
    local recv_pid=$!
    cleanup_push "kill ${recv_pid} 2>/dev/null"

    sleep 0.3  # let receiver register before sender connects
    if ! gritty send --session "${session}" --timeout 30 "${src}" >/dev/null; then
        fail "single file: send" "send command failed"
        return
    fi
    wait "${recv_pid}" 2>/dev/null || true

    local out_sum
    out_sum=$(sha256sum "${dst_dir}/single.bin" 2>/dev/null | awk '{print $1}')
    if [ "${src_sum}" = "${out_sum}" ]; then
        pass "send/receive: single file (256K) round-trip"
    else
        fail "send/receive: single file (256K) round-trip" "checksum mismatch src=${src_sum} out=${out_sum}"
    fi

    gritty kill-session "${session}" 2>/dev/null || true
}

# ---------------------------------------------------------------------------
# 2. Multiple files in one send
# ---------------------------------------------------------------------------
test_multiple_files() {
    reset_state
    local session="local:xfer2"

    tmux new-session -d -s sr -x 120 -y 40
    cleanup_push "tmux kill-session -t sr 2>/dev/null"
    tmux send-keys -t sr "gritty connect ${session}" Enter
    wait_for_session local "xfer2" 10 || {
        fail "multi file: session created" ""
        return
    }
    tmux send-keys -t sr Enter
    sleep 0.2
    tmux send-keys -t sr '~.'
    sleep 0.5

    local src_dir="${work}/multi-in"
    local dst_dir="${work}/multi-out"
    mkdir -p "${src_dir}" "${dst_dir}"

    printf 'alpha\n' > "${src_dir}/a.txt"
    printf 'beta beta\n' > "${src_dir}/b.txt"
    printf 'gamma gamma gamma\n' > "${src_dir}/c.txt"

    gritty receive --session "${session}" --timeout 30 "${dst_dir}" >/dev/null &
    local recv_pid=$!
    cleanup_push "kill ${recv_pid} 2>/dev/null"
    sleep 0.3

    if ! gritty send --session "${session}" --timeout 30 \
            "${src_dir}/a.txt" "${src_dir}/b.txt" "${src_dir}/c.txt" >/dev/null; then
        fail "multi file: send" "send command failed"
        return
    fi
    wait "${recv_pid}" 2>/dev/null || true

    local ok=1
    for f in a.txt b.txt c.txt; do
        if ! cmp -s "${src_dir}/${f}" "${dst_dir}/${f}"; then
            ok=0
            break
        fi
    done
    if [ "${ok}" -eq 1 ]; then
        pass "send/receive: three files in one transfer"
    else
        fail "send/receive: three files in one transfer" "file content mismatch"
    fi

    gritty kill-session "${session}" 2>/dev/null || true
}

# ---------------------------------------------------------------------------
# 3. Receiver-first ordering (sender connects after receiver is waiting)
# ---------------------------------------------------------------------------
test_receiver_first() {
    reset_state
    local session="local:xfer3"

    tmux new-session -d -s sr -x 120 -y 40
    cleanup_push "tmux kill-session -t sr 2>/dev/null"
    tmux send-keys -t sr "gritty connect ${session}" Enter
    wait_for_session local "xfer3" 10 || {
        fail "receiver first: session created" ""
        return
    }
    tmux send-keys -t sr Enter
    sleep 0.2
    tmux send-keys -t sr '~.'
    sleep 0.5

    local src="${work}/rf.txt"
    local dst_dir="${work}/rf-out"
    mkdir -p "${dst_dir}"
    printf 'receiver-first payload\n' > "${src}"

    gritty receive --session "${session}" --timeout 30 "${dst_dir}" >/dev/null &
    local recv_pid=$!
    cleanup_push "kill ${recv_pid} 2>/dev/null"

    # Longer pause so the receiver is definitely parked before the sender arrives.
    sleep 1
    if ! gritty send --session "${session}" --timeout 30 "${src}" >/dev/null; then
        fail "receiver first: send" "send command failed"
        return
    fi
    wait "${recv_pid}" 2>/dev/null || true

    if cmp -s "${src}" "${dst_dir}/rf.txt"; then
        pass "send/receive: receiver-first ordering"
    else
        fail "send/receive: receiver-first ordering" "content mismatch"
    fi

    gritty kill-session "${session}" 2>/dev/null || true
}

# ---------------------------------------------------------------------------
# Run all tests
# ---------------------------------------------------------------------------
echo "=== gritty send/receive CLI tests ==="
echo ""

test_single_file
test_multiple_files
test_receiver_first

gritty kill-server local 2>/dev/null || true
rm -rf "${work}"

report_and_exit
