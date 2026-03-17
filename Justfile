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

# Run clippy + fmt check
check *args:
    @if echo "{{args}}" | grep -q -- '--fix'; then \
        cargo fmt; \
        cargo clippy -- -D warnings; \
    else \
        cargo clippy -- -D warnings; \
        cargo fmt --check; \
    fi

# Run tests
test:
    cargo nextest run

# Clippy only
lint:
    cargo clippy -- -D warnings

# Format code
format:
    cargo fmt
