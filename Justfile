default:
	just --list

# Build and run with release profile
run *args:
    cargo run --release -- {{args}}

# Build release binary (output: ./target/release/ferrite)
build:
    cargo build --release

# Build then run as root (avoids sudo needing to find cargo)
sudo-run *args:
    cargo build --release -q
    sudo ./target/release/ferrite {{args}}

# Run clippy + fmt check + unused deps (runs all checks, reports failures at end)
check *args:
    #!/usr/bin/env bash
    set +e
    exit_code=0
    if echo "{{args}}" | grep -q -- '--fix'; then
        cargo fmt
        cargo clippy --all-targets --all-features -- -D warnings || exit_code=1
    else
        cargo clippy --all-targets --all-features -- -D warnings || exit_code=1
        cargo fmt --check || exit_code=1
    fi
    cargo machete || exit_code=1
    cargo deny check advisories sources 2>&1 || echo "warning: cargo deny found issues (non-blocking)"
    exit $exit_code

# Run tests
test:
    cargo nextest run --no-fail-fast

# Clippy only
lint:
    cargo clippy --all-targets --all-features -- -D warnings

# Format code
format:
    cargo fmt

# Security audit
audit:
    cargo deny check advisories sources

# Run tests with coverage (requires cargo-llvm-cov + nightly)
coverage:
    cargo +nightly llvm-cov nextest --no-fail-fast --hide-progress-bar
    cargo +nightly llvm-cov report --html --output-dir coverage/html
    cargo +nightly llvm-cov report --lcov --output-path coverage/lcov.info
