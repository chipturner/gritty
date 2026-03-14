#!/bin/bash
set -euo pipefail
SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
OUTPUT_DIR="${GRITTY_DEMO_OUTPUT:-$SCRIPT_DIR/output}"
mkdir -p "$OUTPUT_DIR"

# Docker mode: set up SSH localhost with devbox alias
if [ -f /setup/ssh-setup.sh ]; then
    echo "Docker mode: setting up SSH..."
    . /setup/ssh-setup.sh
    setup_ssh
fi

# Record each demo (skip files requiring browser in Docker)
for demo in "$SCRIPT_DIR"/*.demo; do
    name=$(basename "$demo" .demo)
    if [ -f /setup/ssh-setup.sh ] && head -5 "$demo" | grep -q "requires:.*browser"; then
        echo "Skipping $name (requires browser, Docker mode)"
        continue
    fi
    echo "Recording: $name"
    "$SCRIPT_DIR/run-demo.sh" "$demo" "$OUTPUT_DIR/$name.cast" || {
        echo "WARNING: $name recording failed" >&2
    }
done
echo "Recordings saved to: $OUTPUT_DIR"
