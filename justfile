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

# Container tests: lifecycle + SSH tunnel (requires Docker; Linux only)
test-container:
    cargo build
    docker build -t gritty-container-test -f tests/container/Dockerfile .
    docker run --rm gritty-container-test

# Record demos in Docker (repeatable, no local deps beyond Docker)
record-demos:
    cargo build
    docker build -t gritty-demo -f tests/container/Dockerfile.demo .
    mkdir -p docs/demos/output
    docker run --rm -v "$(pwd)/docs/demos/output:/demos/output" gritty-demo

# Record demos locally (uses your SSH config + installed tools)
record-demos-local:
    docs/demos/record-all.sh

# Clean all build artifacts
clean:
    cargo clean
