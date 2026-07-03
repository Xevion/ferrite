# Architecture

## Output Model

ferrite separates output into three orthogonal concerns.

### TUI (`--tui auto|always|never`)

Controls the live interactive display. In `auto` mode (default), the TUI activates when stdout is a terminal and falls back to headless mode otherwise.

- **TUI on:** ratatui renders an inline viewport to stdout with heatmaps, progress, and live log lines. Tracing events are captured and displayed inline via a dedicated tracing layer.
- **TUI off:** No interactive display. Tracing goes to stderr only.

Code: `src/tui/` (event loop, rendering, `TuiMakeWriter` for tracing integration)

### Tracing (stderr)

Diagnostic logs (`info!`, `warn!`, etc.) carry moment-by-moment operational detail: physical address map stats, DIMM topology, segment lifecycle events, ECC deltas, and warnings. They are human-readable, never JSON, and independent of `--format` (which governs the results surface on stdout, not tracing).

Tracing uses a single reloadable layer (`tracing_subscriber::reload`) over a `registry`, initialized in `main.rs::init_tracing()`:

| Mode    | Tracing destination                                                                                  |
|---------|-----------------------------------------------------------------------------------------------------|
| non-TUI | human text → stderr                                                                                  |
| TUI     | human text → TUI channel (hot-swapped via the reload handle; restored to stderr after the TUI exits) |

The reload handle lets `run_tui_mode()` swap the stderr writer for a `TuiMakeWriter` that routes log lines into the TUI, then reroute back to stderr on exit via `TuiTraceState`.

> The NDJSON schema reserves a `RunEvent::Log` variant for carrying diagnostics as structured events; it may be emitted into the NDJSON stream when structured output is active, but tracing's primary destinations remain stderr and the TUI channel.

### Results (Event Consumers + Renderers)

Output is split into three independent concerns:

- **Live human text:** `HeadlessPrinter` consumes `RunEvent`s and writes PASS/FAIL lines, banners, ECC info to stdout. Active in headless (non-TUI) mode.
- **NDJSON events:** `NdjsonEventWriter` serializes `RunEvent`s as newline-delimited JSON. Streams to stdout when `--format json`; additionally written to a file when `--events <path>` is given (independent of `--format`).
- **Post-run results:** `ResultsDoc` + `ResultsRenderer` trait (`TableRenderer`, `JsonRenderer`) render the final summary after the run completes.

**Constraint:** `--format json` is incompatible with `--tui` (the TUI owns stdout for its live display). ferrite errors with guidance: use `--tui never` for JSON output. `--events <file>` *is* supported alongside the TUI — the event stream is written to the file while the TUI renders.

In TUI mode, the `EventBridge` translates `RunEvent`s to `TuiEvent`s for the TUI event loop, and optionally writes the NDJSON event stream to the `--events` file. No headless human printing occurs.

Code: `src/headless.rs`, `src/ndjson.rs`, `src/results/`, `src/tui/bridge.rs`

Error handling is [snafu](https://docs.rs/snafu) exclusively (`Snafu` enums for typed library errors, `Whatever` for loose message-based errors in binary-side glue) — no `thiserror`/`anyhow`.

## Module Map

| Module | Purpose |
|---|---|
| `main.rs` | Entry point, mode dispatch (`--devmem` / TUI / headless), signal handler installation |
| `cli.rs` | `Cli` (clap derive), `setup_test()`, `check_privileges()`, size parsing |
| `output.rs` | Shared headless output wiring: event consumption, post-run results rendering, NDJSON events-file opening — used by both the anonymous-memory path (`main.rs`) and `devmem_run.rs` so `--format`/`--events` behave identically |
| `devmem_run.rs` | `/dev/mem` targeted-testing execution path (`--devmem`): resolves the requested physical range into concrete mappings, then tests or read-only probes each; always headless |
| `alloc.rs` | `TestBuffer` — chunked, OOM-safe mmap + mlock of anonymous memory; `CompactionGuard` — disables kernel page compaction during tests |
| `failure.rs` | `Failure` — one failing 64-bit word (virtual address, expected, actual, physical address) |
| `pattern/` | `Pattern` enum, `run_pattern()`, per-pattern modules: `solid`, `walking`, `checkerboard`, `stuck_address`, `march` (March C-, sequential per-cell march executor) |
| `runner.rs` | Headless multi-pass runner: `run()`, `PassResult`, `PatternResult` |
| `ops/` | Three-layer fill/verify operations: `scalar.rs` (coverage-measured), `avx512.rs` (excluded from coverage), `mod.rs` (dispatch) |
| `physmem/mod.rs` | Physical-memory subsystem root: `Pfn`/`PfnRange` re-exports, `PAGE_BYTES` constant, hex-range parsing shared by `--devmem` |
| `physmem/phys.rs` | Physical address resolution via `/proc/self/pagemap`; `PhysAddr`, `PagemapResolver`, `PhysResolver` trait, `MapStats` |
| `physmem/pfn.rs` | `Pfn` newtype (page frame number, `physical address >> 12`) and `PfnRange` set algebra (compaction, union, subtraction, membership) — the shared substrate for coverage, gap classification, and culling |
| `physmem/kpageflags.rs` | Unified `/proc/kpageflags` bit table (`KPageFlags`, via the `bitflags` crate); single-frame and batch readers used by `phys.rs` and `gap.rs` |
| `physmem/sysmem.rs` | Installed-RAM denominator (`/proc/iomem` "System RAM", `MemTotal` fallback) and single-run physical `Coverage`; `RamSource`, `InstalledRam`, `Cumulative` |
| `physmem/coverage.rs` | Cross-run coverage persistence (`--coverage-file`): `CoverageStore`, PFN range compaction/merge/subtraction, machine fingerprint guard |
| `physmem/gap.rs` | Untested-remainder classification via `/proc/kpageflags`: `FrameClass`, `GapReport`, system gap scan |
| `physmem/sieve.rs` | Frame-hostage culling (`--cull`): `FrameSieve` sweeps available RAM, holds covered 2 MiB blocks hostage, releases fresh frames for the test buffer |
| `physmem/lifecycle.rs` | Coverage-store lifecycle glue: opens/inits the `--coverage-file` store, derives the `--cull` hostage set, merges a completed run in, attaches cumulative + gap stats to `RunResults` |
| `physmem/devmem.rs` | `/dev/mem` targeted testing (`--devmem`): target parsing, `memmap=`/iomem write-safety classification (`Safety::Reserved`/`SystemRam`/`FirmwareOrMmio`), `pread` read-only probe, trivial `DevMemResolver` (phys known exactly) |
| `edac.rs` | ECC error counters from `/sys/devices/system/edac/`; `EdacSnapshot`, `DimmEdac`, `EccDelta` |
| `smbios.rs` | DIMM info from `/sys/firmware/dmi/tables/` |
| `dimm.rs` | `DimmTopology` — merges SMBIOS + EDAC into a per-DIMM view |
| `error_analysis.rs` | Post-test bit error classification: `BitErrorStats`, `ErrorClassification` (StuckBit, Coupling, Mixed) |
| `headless.rs` | `HeadlessPrinter` — human-readable live output from event bus |
| `ndjson.rs` | `NdjsonEventWriter` — NDJSON event serialization for `--format json` / `--events` |
| `results/mod.rs` | `ResultsDoc`, `ResultsRenderer` trait, `TableRenderer`, `JsonRenderer` |
| `results/render.rs` | Shared line rendering (`write_pattern_result_line`, etc.) used by both `HeadlessPrinter` and `TableRenderer` |
| `results/fixtures.rs` | Test-only `RunResults`/`PassResult` builders shared by doc and render tests |
| `units.rs` | Binary/decimal size and rate formatting |
| `shutdown.rs` | Signal handling, panic hook, coordinated shutdown, exit codes |
| `tui/mod.rs` | `TuiConfig`, `run_tui()`, `run_event_loop<B>()`, `finish_viewport<B>()` — the event-loop hub (exit teardown: drain buffered logs to scrollback, collapse the viewport cleanly) |
| `tui/run.rs` | `run_tui_mode()`, `TuiTestSetup` |
| `tui/bridge.rs` | `EventBridge` — translates `RunEvent` → `TuiEvent`, updates `Segment` atomics |
| `tui/segment.rs` | `Segment` — the TUI's per-run display unit (pattern index, progress, failure count) |
| `tui/event.rs` | `TuiEvent`, `TuiFailure`, `TuiOutcome`, `TuiLoopResult`, `FlippedBits` |
| `tui/trace.rs` | `TuiMakeWriter`, `TuiTraceState`, `TuiTraceGuard` — routes tracing output into the TUI channel and back to stderr on exit |
| `tui/render.rs` | Frame rendering: heatmaps, progress bars, status header |
| `tui/activity.rs` | `ActivityBuffer` — activity tracking for heatmap data |
| `tui/palette.rs` | Color palette functions (error severity, background fade) |

## Data Flow

```
main()
 ├─ install_signal_handlers(), install_panic_hook()
 ├─ init_tracing() → reloadable stderr layer  [main.rs]
 ├─ check_privileges(size, need_phys)         [cli.rs]
 ├─ select patterns (Pattern::ALL or --test)
 ├─ CoverageCtx::open(--coverage-file)        [physmem/lifecycle.rs]
 │
 ├─ --devmem given → devmem_run::run(...)     [devmem_run.rs]  (always headless, distinct backend)
 │   ├─ resolve_mappings() → classify each range's write Safety   [physmem/devmem.rs]
 │   ├─ test or read-only probe each mapping via runner::run()    [runner.rs]
 │   └─ output::render_results(...)           [output.rs]
 │
 ├─ conflict check: --format json + --tui → error
 │
 ├─ TUI mode:
 │   ├─ lifecycle::cull_ranges() → hostage set (if --cull)        [physmem/lifecycle.rs]
 │   ├─ setup_test() → TestSetup              [cli.rs]
 │   └─ run_tui_mode(...)                     [tui/run.rs]
 │       ├─ hot-swap tracing layer → TUI channel (reload handle)  [tui/trace.rs]
 │       ├─ spawn "test-driver" thread → runner::run() over the whole allocation, emits RunEvents
 │       │   (rayon parallelism inside the pattern loop when --parallel > 1)
 │       ├─ spawn "event-bridge" thread → EventBridge translates RunEvent → TuiEvent  [tui/bridge.rs]
 │       ├─ run_event_loop(terminal, config, segment, rx)  ← event loop [tui/mod.rs]
 │       └─ wait for test-driver (5 s timeout) + process::exit
 │   └─ lifecycle merges run into CoverageStore; output::render_results(...) [output.rs]
 │
 └─ Headless mode:                            (tracing stays on stderr)
     ├─ lifecycle::cull_ranges() → hostage set (if --cull)        [physmem/lifecycle.rs]
     ├─ setup_test() → TestSetup              [cli.rs]
     ├─ create NdjsonEventWriter (if --format json or --events)  [ndjson.rs]
     ├─ spawn consumer thread:
     │   ├─ HeadlessPrinter.handle_event()    [headless.rs]
     │   └─ NdjsonEventWriter.handle_event()  [ndjson.rs, optional]
     ├─ runner::run(buf, patterns, passes, parallel, &tx, resolver)   [runner.rs]
     ├─ error_analysis (if failures)          [error_analysis.rs]
     ├─ NdjsonEventWriter.write_run_complete()  [optional]
     ├─ lifecycle merges run into CoverageStore                  [physmem/lifecycle.rs]
     ├─ TableRenderer.render(ResultsDoc)      [results/mod.rs]
     └─ exit code
```
