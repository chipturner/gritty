#!/bin/bash
set -eou pipefail

failed=0

for script in \
        /tests/lifecycle.sh \
        /tests/features.sh \
        /tests/send-receive.sh \
        /tests/ssh-tunnel.sh \
        /tests/session-over-tunnel.sh; do
    echo ""
    echo "### running ${script} ###"
    if ! "${script}"; then
        failed=1
    fi
    # Best-effort teardown between suites so leftover state from one suite
    # doesn't poison the next.
    gritty kill-server local 2>/dev/null || true
    tmux kill-server 2>/dev/null || true
    echo ""
done

exit "${failed}"
