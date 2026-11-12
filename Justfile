set shell := ["bash", "-uc"]

uefi := "x86_64-unknown-uefi"
ovmf_code := "/usr/share/OVMF/OVMF_CODE_4M.fd"
ovmf_vars := "/usr/share/OVMF/OVMF_VARS_4M.fd"

# e1000 has a UNDI driver in OVMF. `user` gives a built-in DHCP server (guest
# 10.0.2.15) and forwards host :8080 to the guest's :80, but carries no multicast.
# `tap` joins tap-ferrite on libvirt's NAT bridge virbr0, where mDNS works.
user_net := "-netdev user,id=net0,hostfwd=tcp::8080-:80"
tap_net := "-netdev tap,id=net0,ifname=tap-ferrite,script=no,downscript=no"

alias c := check
alias t := test
alias l := lint

# list recipes
default:
    @just --list

# type-check the host crates and the UEFI apps
check: vendor
    cargo check --all-targets
    cargo efi-check

# clippy over the host crates and the UEFI apps, plus unused-dependency check
lint: vendor
    cargo clippy --all-targets
    cargo clippy -p ferrite-uefi -p ferrite-recon --lib --tests
    cargo efi-clippy
    cargo machete

# run the unit tests on the host
test *args: vendor
    cargo nextest run {{ args }}
    cargo nextest run -p ferrite-uefi -p ferrite-recon --lib {{ args }}

format:
    cargo fmt --all

# desktop dev tool: `just dev log`, `just dev bench`, `just dev run dbcping`
dev *args:
    cargo run -q -p ferrite-dev -- {{ args }}

# fetch the pinned Linux release and regenerate the untracked snapshot ferrite-wifi-sys compiles; `just vendor --force` redoes it
vendor *args:
    cargo run -q -p ferrite-dev -- vendor {{ args }}

# build every UEFI app (debug, or `just build release`)
build profile="dev": vendor
    cargo efi-build --profile {{ profile }}

# build one UEFI app and stage it as the ESP bootloader under ./esp
stage app="ferrite-uefi": vendor
    cargo build --target {{ uefi }} --workspace --bin {{ app }}
    mkdir -p esp/efi/boot
    cp -f target/{{ uefi }}/debug/{{ app }}.efi esp/efi/boot/bootx64.efi
    test -f OVMF_VARS.fd || cp {{ ovmf_vars }} OVMF_VARS.fd

# boot an app in QEMU: `just qemu`, `just qemu xhciscan serial`, `just qemu ferrite-uefi serial tap`
qemu app="ferrite-uefi" mode="gui" net="user": (stage app)
    qemu-system-x86_64 \
        -enable-kvm \
        -machine q35 \
        -m 512M \
        -drive if=pflash,format=raw,readonly=on,file={{ ovmf_code }} \
        -drive if=pflash,format=raw,file=OVMF_VARS.fd \
        -drive format=raw,file=fat:rw:esp \
        {{ if net == "tap" { tap_net } else { user_net } }} -device e1000,netdev=net0 \
        -device qemu-xhci \
        {{ if mode == "serial" { "-nographic" } else { "-display gtk" } }}

# copy the staged ESP onto a mounted FAT32 USB stick: `just stage-usb /run/media/$USER/FERRITE`
stage-usb mount app="ferrite-uefi": (stage app)
    mkdir -p "{{ mount }}/efi/boot"
    cp -f esp/efi/boot/bootx64.efi "{{ mount }}/efi/boot/bootx64.efi"
    @echo "staged {{ app }} -> {{ mount }}/efi/boot/bootx64.efi"

# Security audit
audit:
    tempo check security

# Build release binary (output: ./target/release/ferrite)
build-linux:
    cargo build -p ferrite --release

# Build and run with dev profile (use sudo-run for release)
run *args:
    cargo run -p ferrite -- {{args}}

# Build then run as root (avoids sudo needing to find cargo)
sudo-run *args:
    cargo build -p ferrite --release -q
    sudo ./target/release/ferrite {{args}}

# Profile with samply — opens Firefox Profiler for visual/interactive analysis.
# No sudo needed up to ~4G (RLIMIT_MEMLOCK). Open the printed URL in Firefox or Chrome.
profile *args:
    cargo build -p ferrite --profile profiling -q
    samply record --no-open -- ./target/profiling/ferrite {{args}}

# Profile with perf (frame-pointer unwinding) — leaves perf.data for CLI analysis.
# Walking-ones/checkerboard test data produces false [unknown] frames — this is expected.
# Use `just profile-report`, `just profile-svg`, or `just profile-annotate` after this.
profile-perf size="1G" passes="1":
    cargo build -p ferrite --profile profiling -q
    perf record -g -F 997 -o perf.data -- ./target/profiling/ferrite --size {{size}} --passes {{passes}} --tui never

# Profile with DWARF callchain unwinding — cleaner call chains, ~5-10x larger perf.data.
# Use when frame-pointer unwinding loses callers (e.g. verify_avx512's caller shows as [unknown]).
profile-perf-deep size="1G" passes="1":
    cargo build -p ferrite --profile profiling -q
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
    cargo mutants --package ferrite --profile mutant --test-tool nextest

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
