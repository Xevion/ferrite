set shell := ["bash", "-uc"]

default:
	just --list

alias c := check
alias t := test
alias l := lint

# Run all checks (format, lint, unused deps, tests, security audit)
check *args:
    tempo check {{args}}

# Run tests (mirrors the tempo/CI test command)
test *args:
    cargo nextest run --no-fail-fast --hide-progress-bar --failure-output final {{args}}

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

# Build and run with dev profile (use sudo-run for release)
run *args:
    cargo run -- {{args}}

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

# --max-time caps slow cells; --sort location keeps sizes in ascending order.
bench_flags := "--max-time 2 --sort location"
bench_targets := "--bench patterns --bench alloc --bench simd"

# Run wall-clock benchmarks (patterns, alloc, SIMD). FILTER (optional) matches
# benchmark names by substring; alloc runs unprivileged when RLIMIT_MEMLOCK
# covers the size, else those sizes skip individually.
bench filter="":
    cargo bench -p ferrite --features bench {{bench_targets}} -- {{bench_flags}} {{filter}}

# Save current bench results as a named baseline (default: "main")
bench-baseline name="main":
    mkdir -p crates/ferrite-linux/benches/baselines
    cargo bench -p ferrite --features bench {{bench_targets}} -- {{bench_flags}} | tee crates/ferrite-linux/benches/baselines/{{name}}.txt

# Compare current bench results against a saved baseline
bench-compare name="main":
    cargo bench -p ferrite --features bench {{bench_targets}} -- {{bench_flags}} | tee /tmp/ferrite-bench-current.txt
    diff crates/ferrite-linux/benches/baselines/{{name}}.txt /tmp/ferrite-bench-current.txt || true

# Run instruction-count benchmarks via Gungraun (requires valgrind + cargo install gungraun-runner)
bench-ci:
    cargo bench -p ferrite --features bench --bench instructions

ovmf_code := "/usr/share/OVMF/OVMF_CODE_4M.fd"
ovmf_vars := "/usr/share/OVMF/OVMF_VARS_4M.fd"
efi := "target/x86_64-unknown-uefi/debug/ferrite-uefi.efi"
display := "gtk"
# e1000 has a UNDI driver in OVMF; user-mode net gives a built-in DHCP server
# (guest 10.0.2.15) and forwards host :8080 to the guest's HTTP :80.
netdev := "-netdev user,id=net0,hostfwd=tcp::8080-:80 -device e1000,netdev=net0"


# type-check without producing a binary
uefi-check:
    cargo check -p ferrite-uefi --target x86_64-unknown-uefi

# run the shared lib's unit tests on the host
uefi-test:
    cargo test -p ferrite-uefi --lib --target x86_64-unknown-linux-gnu

# build the .efi (debug)
uefi-build:
    cargo build -p ferrite-uefi --target x86_64-unknown-uefi

# build the optimized .efi
build-release:
    cargo build -p ferrite-uefi --target x86_64-unknown-uefi --release

# assemble the virtual EFI system partition under ./esp
esp: uefi-build
    mkdir -p esp/efi/boot
    cp -f {{ efi }} esp/efi/boot/bootx64.efi
    # writable copy of the NVRAM store; created once, then reused
    test -f OVMF_VARS.fd || cp {{ ovmf_vars }} OVMF_VARS.fd

# boot the spike in QEMU with a graphical display (headful)
qemu: esp
    qemu-system-x86_64 \
        -enable-kvm \
        -machine q35 \
        -m 512M \
        -drive if=pflash,format=raw,readonly=on,file={{ ovmf_code }} \
        -drive if=pflash,format=raw,file=OVMF_VARS.fd \
        -drive format=raw,file=fat:rw:esp \
        {{ netdev }} \
        -display {{ display }}

# boot headless with the console mirrored to your terminal via serial
qemu-serial: esp
    qemu-system-x86_64 \
        -enable-kvm \
        -machine q35 \
        -m 512M \
        -drive if=pflash,format=raw,readonly=on,file={{ ovmf_code }} \
        -drive if=pflash,format=raw,file=OVMF_VARS.fd \
        -drive format=raw,file=fat:rw:esp \
        {{ netdev }} \
        -nographic

# stage an arbitrary bin (e.g. pciscan) as the ESP bootloader
stage bin:
    cargo build -p ferrite-uefi --target x86_64-unknown-uefi --bin {{ bin }}
    mkdir -p esp/efi/boot
    cp -f target/x86_64-unknown-uefi/debug/{{ bin }}.efi esp/efi/boot/bootx64.efi
    test -f OVMF_VARS.fd || cp {{ ovmf_vars }} OVMF_VARS.fd

# boot the PCI "touch the chip" scanner in a graphical window
pciscan: (stage "pciscan")
    qemu-system-x86_64 \
        -enable-kvm \
        -machine q35 \
        -m 512M \
        -drive if=pflash,format=raw,readonly=on,file={{ ovmf_code }} \
        -drive if=pflash,format=raw,file=OVMF_VARS.fd \
        -drive format=raw,file=fat:rw:esp \
        {{ netdev }} \
        -display {{ display }}

# boot the PCI scanner headless, console over serial
pciscan-serial: (stage "pciscan")
    qemu-system-x86_64 \
        -enable-kvm \
        -machine q35 \
        -m 512M \
        -drive if=pflash,format=raw,readonly=on,file={{ ovmf_code }} \
        -drive if=pflash,format=raw,file=OVMF_VARS.fd \
        -drive format=raw,file=fat:rw:esp \
        {{ netdev }} \
        -nographic

# boot the xHCI USB-controller recon tool in a graphical window
xhciscan: (stage "xhciscan")
    qemu-system-x86_64 \
        -enable-kvm \
        -machine q35 \
        -m 512M \
        -drive if=pflash,format=raw,readonly=on,file={{ ovmf_code }} \
        -drive if=pflash,format=raw,file=OVMF_VARS.fd \
        -drive format=raw,file=fat:rw:esp \
        -device qemu-xhci \
        -display {{ display }}

# boot the xHCI recon tool headless, console over serial
xhciscan-serial: (stage "xhciscan")
    qemu-system-x86_64 \
        -enable-kvm \
        -machine q35 \
        -m 512M \
        -drive if=pflash,format=raw,readonly=on,file={{ ovmf_code }} \
        -drive if=pflash,format=raw,file=OVMF_VARS.fd \
        -drive format=raw,file=fat:rw:esp \
        -device qemu-xhci \
        -nographic

# boot the xHCI Debug Capability bring-up app in a graphical window
dbcping: (stage "dbcping")
    qemu-system-x86_64 \
        -enable-kvm \
        -machine q35 \
        -m 512M \
        -drive if=pflash,format=raw,readonly=on,file={{ ovmf_code }} \
        -drive if=pflash,format=raw,file=OVMF_VARS.fd \
        -drive format=raw,file=fat:rw:esp \
        -device qemu-xhci \
        -display {{ display }}

# boot the DbC bring-up app headless, console over serial
dbcping-serial: (stage "dbcping")
    qemu-system-x86_64 \
        -enable-kvm \
        -machine q35 \
        -m 512M \
        -drive if=pflash,format=raw,readonly=on,file={{ ovmf_code }} \
        -drive if=pflash,format=raw,file=OVMF_VARS.fd \
        -drive format=raw,file=fat:rw:esp \
        -device qemu-xhci \
        -nographic

# copy the built ESP tree onto a mounted FAT32 USB (pass the mount point)
# e.g. `just stage-usb /run/media/$USER/FERRITE`
stage-usb mount: esp
    mkdir -p "{{ mount }}/efi/boot"
    cp -f esp/efi/boot/bootx64.efi "{{ mount }}/efi/boot/bootx64.efi"
    @echo "staged bootx64.efi -> {{ mount }}/efi/boot/"
