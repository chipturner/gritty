#!/bin/bash
set -euo pipefail

failed=0

for script in /tests/lifecycle.sh /tests/features.sh /tests/ssh-tunnel.sh; do
    echo ""
    if ! "$script"; then
        failed=1
    fi
    echo ""
done

exit "$failed"
