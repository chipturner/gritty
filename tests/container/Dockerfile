FROM debian:trixie-slim
RUN apt-get update && apt-get install -y --no-install-recommends tmux && rm -rf /var/lib/apt/lists/*
COPY target/debug/gritty /usr/local/bin/gritty
COPY tests/container/lifecycle.sh /lifecycle.sh
RUN chmod +x /lifecycle.sh /usr/local/bin/gritty
CMD ["/lifecycle.sh"]
