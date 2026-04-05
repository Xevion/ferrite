default:
	just --list

alias c := check
alias t := test
alias l := lint

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

# Profile with samply — opens Firefox Profiler for visual/interactive analysis.
# No sudo needed up to ~4G (RLIMIT_MEMLOCK). Open the printed URL in Firefox or Chrome.
profile *args:
    cargo build --profile profiling -q
    samply record --no-open -- ./target/profiling/ferrite {{args}}

# Profile with perf (frame-pointer unwinding) — leaves perf.data for CLI analysis.
# Walking-ones/checkerboard test data produces false [unknown] frames — this is expected.
# Use `just profile-report`, `just profile-svg`, or `just profile-annotate` after this.
profile-perf size="1G" passes="1":
    cargo build --profile profiling -q
    perf record -g -F 997 -o perf.data -- ./target/profiling/ferrite --size {{size}} --passes {{passes}} --tui never

# Profile with DWARF callchain unwinding — cleaner call chains, ~5-10x larger perf.data.
# Use when frame-pointer unwinding loses callers (e.g. verify_avx512's caller shows as [unknown]).
profile-perf-deep size="1G" passes="1":
    cargo build --profile profiling -q
    perf record --call-graph dwarf -F 997 -o perf.data -- ./target/profiling/ferrite --size {{size}} --passes {{passes}} --tui never

# Print hot-path summary filtered to ferrite symbols (hides unresolved kernel/test-data noise)
profile-report:
    perf report --stdio -n --hide-unresolved --dsos ferrite -g graph,0.5,caller,function,percent

# Annotate a symbol with per-instruction sample percentages (default: fill_nt)
profile-annotate symbol="ferrite::simd::fill_nt":
    perf annotate --stdio --symbol {{symbol}}

# Generate flamegraph SVG from last perf run
profile-svg:
    perf script | inferno-collapse-perf | inferno-flamegraph > flamegraph.svg

# Run mutation testing (requires cargo-mutants)
mutants:
    cargo mutants --profile mutant --test-tool nextest

# Run tests with coverage (requires cargo-llvm-cov + nightly)
coverage:
    RUSTFLAGS="--cfg coverage_nightly" cargo +nightly llvm-cov nextest --no-fail-fast --hide-progress-bar
    RUSTFLAGS="--cfg coverage_nightly" cargo +nightly llvm-cov report --html --output-dir coverage/html
    RUSTFLAGS="--cfg coverage_nightly" cargo +nightly llvm-cov report --lcov --output-path coverage/lcov.info
    RUSTFLAGS="--cfg coverage_nightly" cargo +nightly llvm-cov report --json --output-path coverage/coverage.json

# Run wall-clock benchmarks (patterns, alloc, SIMD) — alloc requires root for mlock
bench:
    cargo bench --features bench --bench patterns --bench alloc --bench simd

# Save current bench results as a named baseline (default: "main")
bench-baseline name="main":
    mkdir -p benches/baselines
    cargo bench --features bench --bench patterns --bench alloc --bench simd | tee benches/baselines/{{name}}.txt

# Compare current bench results against a saved baseline
bench-compare name="main":
    cargo bench --features bench --bench patterns --bench alloc --bench simd | tee /tmp/ferrite-bench-current.txt
    diff benches/baselines/{{name}}.txt /tmp/ferrite-bench-current.txt || true

# Run instruction-count benchmarks via Gungraun (requires valgrind + cargo install gungraun-runner)
bench-ci:
    cargo bench --features bench --bench instructions
