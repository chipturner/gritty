FROM debian:trixie-slim
RUN apt-get update && apt-get install -y --no-install-recommends \
        tmux openssh-server openssh-client netcat-openbsd \
    && rm -rf /var/lib/apt/lists/*
COPY target/debug/gritty /usr/local/bin/gritty
COPY tests/container/lifecycle.sh tests/container/ssh-tunnel.sh tests/container/ssh-setup.sh \
     tests/container/features.sh tests/container/run-all.sh /tests/
RUN chmod +x /tests/*.sh /usr/local/bin/gritty
CMD ["/tests/run-all.sh"]
