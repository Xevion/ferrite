# ferrite — development commands

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

# Run clippy + fmt check + unused deps
check *args:
    @if echo "{{args}}" | grep -q -- '--fix'; then \
        cargo fmt; \
        cargo clippy -- -D warnings; \
    else \
        cargo clippy -- -D warnings; \
        cargo fmt --check; \
    fi
    cargo machete
    cargo deny check advisories sources 2>&1 || echo "warning: cargo deny found issues (non-blocking)"

# Run tests
test:
    cargo nextest run --no-fail-fast

# Clippy only
lint:
    cargo clippy -- -D warnings

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
