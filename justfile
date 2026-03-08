set shell := ["zsh", "-uc"]

default:
    just --list

# Build the project
build:
    cargo build

# Clippy (strict) then full test suite — the pre-push gate
check:
    cargo clippy -- -D warnings
    cargo nextest run

# Format all source files
fmt:
    cargo fmt

# Check formatting without modifying (CI-friendly)
fmt-check:
    cargo fmt -- --check

# Run tests (pass args to filter, e.g. `just test session_natural`)
test *args:
    cargo nextest run {{ args }}

# Protocol codec unit tests only
test-protocol:
    cargo nextest run --test protocol_test

# E2E session integration tests only
test-e2e:
    cargo nextest run --test e2e_test

# Daemon integration tests only
test-daemon:
    cargo nextest run --test daemon_test

# SSH integration tests (requires sshd + ssh localhost; skips gracefully if missing)
test-ssh:
    cargo nextest run --test ssh_integration_test -j 1

# Socat tunnel disruption tests (requires socat; skips gracefully if missing)
test-socat:
    cargo nextest run --test socat_tunnel_test -j 1

# Socat bridge integration tests (requires socat; skips gracefully if missing)
test-socat-bridge:
    cargo nextest run --test socat_bridge_test -j 1

# Run full suite N times and report pass/fail tally
stress count="10":
    #!/usr/bin/env zsh
    pass=0 fail=0
    for i in $(seq 1 {{ count }}); do
        echo -n "Run $i/{{ count }}: "
        if ! cargo nextest run &>/dev/null; then
            echo "FAILED"
            ((fail++))
        else
            echo "PASSED"
            ((pass++))
        fi
    done
    echo "\n$pass passed, $fail failed out of {{ count }} runs"
    [[ $fail -eq 0 ]]

# Run the binary (pass args, e.g. `just run connect local:myproject`)
run *args:
    cargo run -- {{ args }}

# Run a foreground debug session
debug-session name="test":
    RUST_LOG=debug cargo run -- server -f

# Launch 3-pane tmux manual test (server + socat bridge + client)
quicktest:
    tmux -L gritty-test start-server\; source-file quicktest.tmux

# Test coverage summary
coverage:
    cargo llvm-cov nextest

# Test coverage with HTML report
coverage-html:
    cargo llvm-cov nextest --html
    @echo "Report: target/llvm-cov/html/index.html"

# Clean coverage artifacts
coverage-clean:
    cargo llvm-cov clean --workspace
    rm -rf coverage/
    rm -f lcov.info coverage.json coverage.xml
    rm -f **/*.profraw(N) **/*.profdata(N)

# Upgrade dependencies, update lockfile, and validate
cargo-upgrade *args:
    cargo-upgrade upgrade {{ args }}
    cargo update
    cargo clippy -- -D warnings
    cargo nextest run

# Clean all build artifacts
clean:
    cargo clean
