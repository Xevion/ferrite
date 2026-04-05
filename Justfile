default:
	just --list

# Run all checks (format, lint, unused deps, tests, security audit)
check *args:
    tempo check {{args}}

# Run tests
test:
    tempo check ferrite:test

# Clippy only
lint:
    tempo lint

# Format code
format:
    tempo fmt

# Security audit
audit:
    tempo check security

# Build release binary (output: ./target/release/ferrite)
build:
    cargo build --release

# Build and run with release profile
run *args:
    cargo run --release -- {{args}}

# Build then run as root (avoids sudo needing to find cargo)
sudo-run *args:
    cargo build --release -q
    sudo ./target/release/ferrite {{args}}

# Run tests with coverage (requires cargo-llvm-cov + nightly)
coverage:
    cargo +nightly llvm-cov nextest --no-fail-fast --hide-progress-bar
    cargo +nightly llvm-cov report --html --output-dir coverage/html
    cargo +nightly llvm-cov report --lcov --output-path coverage/lcov.info
