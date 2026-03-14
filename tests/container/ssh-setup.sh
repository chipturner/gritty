#!/bin/bash
# Shared SSH setup for container tests and demo recording.
# Source this file, then call setup_ssh().

setup_ssh() {
    ssh-keygen -A 2>/dev/null
    mkdir -p /run/sshd && /usr/sbin/sshd
    mkdir -p ~/.ssh && chmod 700 ~/.ssh
    ssh-keygen -t ed25519 -f ~/.ssh/id_ed25519 -N "" -q
    cat ~/.ssh/id_ed25519.pub >> ~/.ssh/authorized_keys
    chmod 600 ~/.ssh/authorized_keys
    ssh-keyscan -q localhost >> ~/.ssh/known_hosts 2>/dev/null
    cat > ~/.ssh/config <<'SSHEOF'
Host devbox
    HostName localhost
SSHEOF
    chmod 600 ~/.ssh/config
    ssh -o BatchMode=yes localhost true || { echo "FATAL: ssh localhost failed"; exit 1; }
}
