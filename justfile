set shell := ["zsh", "-uc"]

default:
    just --list

# Build the project
build:
    cargo build

# Clippy (strict) then full test suite — the pre-push gate
check:
    cargo clippy -- -D warnings
    cargo test

# Format all source files
fmt:
    cargo fmt

# Check formatting without modifying (CI-friendly)
fmt-check:
    cargo fmt -- --check

# Run tests (pass args to filter, e.g. `just test session_natural`)
test *args:
    cargo test {{ args }}

# Protocol codec unit tests only
test-protocol:
    cargo test --test protocol_test

# E2E session integration tests only
test-e2e:
    cargo test --test e2e_test

# Daemon integration tests only
test-daemon:
    cargo test --test daemon_test

# SSH integration tests (requires sshd + ssh localhost)
test-ssh:
    GRITTY_SSH_TEST=1 cargo test --test ssh_integration_test -- --test-threads=1

# Socat tunnel disruption tests (requires socat; skips gracefully if missing)
test-socat:
    cargo test --test socat_tunnel_test -- --test-threads=1

# Run full suite N times and report pass/fail tally
stress count="10":
    #!/usr/bin/env zsh
    pass=0 fail=0
    for i in $(seq 1 {{ count }}); do
        echo -n "Run $i/{{ count }}: "
        if cargo test 2>&1 | grep -q "FAILED"; then
            echo "FAILED"
            ((fail++))
        else
            echo "PASSED"
            ((pass++))
        fi
    done
    echo "\n$pass passed, $fail failed out of {{ count }} runs"
    [[ $fail -eq 0 ]]

# Run the binary (pass args, e.g. `just run new -t myproject`)
run *args:
    cargo run -- {{ args }}

# Run a foreground debug session
debug-session name="test":
    RUST_LOG=debug cargo run -- new-session -t {{ name }} --foreground

# Launch 3-pane tmux manual test (server + socat bridge + client)
quicktest:
    tmux -L gritty-test start-server\; source-file quicktest.tmux

# Test coverage summary
coverage:
    cargo llvm-cov

# Test coverage with HTML report
coverage-html:
    cargo llvm-cov --html
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
    cargo test

# Clean all build artifacts
clean:
    cargo clean
