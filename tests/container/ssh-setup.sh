#!/bin/bash
# Shared SSH setup for container tests and demo recording.
# Idempotent -- safe to source/call from multiple scripts in one container run.
# Source this file, then call setup_ssh().

setup_ssh() {
    if [ -f /var/run/gritty-ssh-setup-done ]; then
        return 0
    fi

    ssh-keygen -A 2>/dev/null
    mkdir -p /run/sshd
    pgrep -x sshd >/dev/null || /usr/sbin/sshd
    mkdir -p ~/.ssh && chmod 700 ~/.ssh
    [ -f ~/.ssh/id_ed25519 ] || ssh-keygen -t ed25519 -f ~/.ssh/id_ed25519 -N "" -q
    if ! grep -qFf ~/.ssh/id_ed25519.pub ~/.ssh/authorized_keys 2>/dev/null; then
        cat ~/.ssh/id_ed25519.pub >> ~/.ssh/authorized_keys
    fi
    chmod 600 ~/.ssh/authorized_keys
    ssh-keyscan -q localhost >> ~/.ssh/known_hosts 2>/dev/null
    cat > ~/.ssh/config <<'SSHEOF'
Host devbox
    HostName localhost
SSHEOF
    chmod 600 ~/.ssh/config
    ssh -o BatchMode=yes localhost true || { echo "FATAL: ssh localhost failed"; exit 1; }

    touch /var/run/gritty-ssh-setup-done
}
