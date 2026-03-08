# simple, quick test via an intermediate socket bridge
# usage: tmux -L gritty-test start-server\; source-file quicktest.tmux
new-session -d -s tty 'RUST_LOG=debug cargo run -- server -f'
split-window -v 'sleep 1 && cargo run -- connect local'
attach-session -t tty
