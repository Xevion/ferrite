set shell := ["bash", "-uc"]

ovmf_code := "/usr/share/OVMF/OVMF_CODE_4M.fd"
ovmf_vars := "/usr/share/OVMF/OVMF_VARS_4M.fd"

# e1000 has a UNDI driver in OVMF. `user` gives a built-in DHCP server (guest
# 10.0.2.15) and forwards host :8080 to the guest's :80, but carries no multicast.
# `tap` joins tap-ferrite on libvirt's NAT bridge virbr0, where mDNS works.
user_net := "-netdev user,id=net0,hostfwd=tcp::8080-:80"
tap_net := "-netdev tap,id=net0,ifname=tap-ferrite,script=no,downscript=no"
# The ESP as a FAT image, under target/ so it stays out of git.
qemu_esp := "target/qemu-esp.img"

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
    cargo efi-app-check

# clippy over the host crates and the UEFI apps, plus unused-dependency check
lint: vendor
    cargo clippy --all-targets
    cargo clippy -p ferrite-uefi -p ferrite-recon --lib --tests
    cargo efi-clippy
    cargo efi-app-clippy
    just app-deps
    cargo machete

# run the unit tests on the host
test *args: vendor
    cargo nextest run {{ args }}
    cargo nextest run -p ferrite-uefi -p ferrite-recon --lib {{ args }}

# fail if an ELF-built app pulls in a crate that would call a libc it does not have
app-deps:
    ! cargo tree --config .cargo/app.toml -p ferrite-uefi -p ferrite-wifi-efi -e normal --prefix none -f '{p} [{f}]' | grep -E '^(getrandom|libc|rustix) |[[,]std[],]'

format:
    cargo fmt --all

# desktop dev tool: `just dev log`, `just dev bench`, `just dev run dbcping`
dev *args:
    cargo run -q -p ferrite-dev -- {{ args }}

# regenerate the lkpi stubs and data glue from the built Linux archive; `just stubs --check` only reports drift
stubs *args: vendor
    cargo run -q -p ferrite-dev -- stubs {{ args }}

# run the Wi-Fi C as a Linux process until it hits a kernel symbol nothing implements yet
lab *args: vendor
    cargo run -q -p ferrite-lab -- {{ args }}

# fetch the pinned Linux release and regenerate the untracked snapshot ferrite-wifi-sys compiles; `just vendor --force` redoes it
vendor *args:
    cargo run -q -p ferrite-dev -- vendor {{ args }}

# build every UEFI app (debug, or `just build release`); the ELF-built ones become a .efi through `just stage`
build profile="dev": vendor
    cargo efi-build --profile {{ profile }}
    cargo efi-app-build --profile {{ profile }}

# build one UEFI app and stage it as the ESP bootloader under ./esp; wifitrace and ferrite-uefi also get the firmware of every supported Wi-Fi chip (`ferrite-dev firmware`, chips in ferrite-wifi-sys/firmware-chips.txt), wifi.env from the WPA_ lines of .env and notify.env from its NOTIFY_ lines
stage app="ferrite-uefi": vendor
    mkdir -p esp/efi/boot
    cargo run -q -p ferrite-dev -- build {{ app }} --out esp/efi/boot/bootx64.efi
    {{ if app =~ "^(wifitrace|ferrite-uefi)$" { "cargo run -q -p ferrite-dev -- firmware && just _stage-env" } else { "true" } }}
    test -f OVMF_VARS.fd || cp {{ ovmf_vars }} OVMF_VARS.fd

# write wifi.env (efi/ferrite/firmware) and notify.env (efi/ferrite) from the .env lines, removing stale ones; values are never printed
_stage-env:
    #!/usr/bin/env bash
    set -euo pipefail
    dir=esp/efi/ferrite
    mkdir -p "$dir/firmware"
    rm -f "$dir/firmware/wifi.env" "$dir/notify.env"
    if test -f .env; then
        grep -E '^WPA_[A-Z_0-9]+=' .env > "$dir/firmware/wifi.env" || true
        grep -E '^NOTIFY_[A-Z_0-9]+=' .env > "$dir/notify.env" || true
    fi
    find "$dir" -maxdepth 2 -name '*.env' -empty -delete

# boot an app in QEMU: `just qemu`, `just qemu xhciscan serial`, `just qemu ferrite-uefi serial tap`; a fourth argument picks the CPU model and a fifth the -smp topology (`just qemu membench serial user host 8,cores=4,threads=2`). The ESP is copied into a FAT image (QEMU's `fat:rw:` folder drive aborts under steady log writes); `just qemu-log` prints the app's log from it afterwards
qemu app="ferrite-uefi" mode="gui" net="user" cpu="" smp="": (stage app)
    rm -f {{ qemu_esp }} && mkfs.vfat -C {{ qemu_esp }} 131072 > /dev/null && mcopy -s -i {{ qemu_esp }} esp/efi ::/
    qemu-system-x86_64 \
        -enable-kvm \
        {{ if cpu != "" { "-cpu " + cpu } else { "" } }} \
        {{ if smp != "" { "-smp " + smp } else { "" } }} \
        -machine q35 \
        -m 512M \
        -drive if=pflash,format=raw,readonly=on,file={{ ovmf_code }} \
        -drive if=pflash,format=raw,file=OVMF_VARS.fd \
        -drive format=raw,file={{ qemu_esp }} \
        {{ if net == "tap" { tap_net } else { user_net } }} -device e1000,netdev=net0 \
        -device qemu-xhci \
        -device virtio-rng-pci \
        {{ if mode == "serial" { "-nographic" } else { "-display gtk" } }}

# print an app's log from the last `just qemu` run's FAT image
qemu-log app="ferrite-uefi":
    mtype -i {{ qemu_esp }} ::/efi/ferrite/{{ app }}.txt

# copy the staged ESP onto a mounted FAT32 USB stick, the app and the config under efi/ferrite (firmware/, with wifi.env, and notify.env), not the app logs: `just stage-usb /run/media/$USER/FERRITE`
stage-usb mount app="ferrite-uefi": (stage app)
    mkdir -p "{{ mount }}/efi/boot" "{{ mount }}/efi/ferrite"
    cp -f esp/efi/boot/bootx64.efi "{{ mount }}/efi/boot/bootx64.efi"
    rm -f "{{ mount }}/efi/ferrite/firmware/wifi.env" "{{ mount }}/efi/ferrite/notify.env"
    if test -d esp/efi/ferrite/firmware; then cp -rf esp/efi/ferrite/firmware "{{ mount }}/efi/ferrite/"; fi
    if test -f esp/efi/ferrite/notify.env; then cp -f esp/efi/ferrite/notify.env "{{ mount }}/efi/ferrite/"; fi
    @echo "staged {{ app }} and efi/ferrite -> {{ mount }}"

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
