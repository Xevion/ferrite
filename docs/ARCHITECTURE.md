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

> The NDJSON schema reserves a `RunEvent::Log` variant for carrying diagnostics as structured events, but no production code emits it yet — tracing currently reaches only stderr or the TUI.

### Results (Event Consumers + Renderers)

Output is split into three independent concerns:

- **Live human text:** `HeadlessPrinter` consumes `RunEvent`s and writes PASS/FAIL lines, banners, ECC info to stdout. Active in headless (non-TUI) mode.
- **NDJSON events:** `NdjsonEventWriter` serializes `RunEvent`s as newline-delimited JSON. Streams to stdout when `--format json`; additionally written to a file when `--events <path>` is given (independent of `--format`).
- **Post-run results:** `ResultsDoc` + `ResultsRenderer` trait (`TableRenderer`, `JsonRenderer`) render the final summary after the run completes.

**Constraint:** `--format json` is incompatible with `--tui` (the TUI owns stdout for its live display). ferrite errors with guidance: use `--tui never` for JSON output. `--events <file>` *is* supported alongside the TUI — the event stream is written to the file while the TUI renders.

In TUI mode, the `EventBridge` translates `RunEvent`s to `TuiEvent`s for the TUI event loop, and optionally writes the NDJSON event stream to the `--events` file. No headless human printing occurs.

Code: `src/headless.rs`, `src/ndjson.rs`, `src/results.rs`, `src/tui/bridge.rs`

## Module Map

| Module | Purpose |
|---|---|
| `main.rs` | Entry point, mode dispatch, signal handler installation |
| `cli.rs` | `Cli` (clap derive), `setup_test()`, `check_privileges()`, size parsing |
| `alloc.rs` | `LockedRegion` — mmap + mlock anonymous memory; `CompactionGuard` — disables kernel page compaction during tests |
| `failure.rs` | `Failure` — one failing 64-bit word (virtual address, expected, actual, physical address) |
| `pattern/` | `Pattern` enum, `run_pattern()`, per-pattern modules: `solid`, `walking`, `checkerboard`, `stuck_address`, `march` (March C-, sequential per-cell march executor) |
| `runner.rs` | Headless multi-pass runner: `run()`, `PassResult`, `PatternResult` |
| `ops/` | Three-layer fill/verify operations: `scalar.rs` (coverage-measured), `avx512.rs` (excluded from coverage), `mod.rs` (dispatch) |
| `phys.rs` | Physical address resolution via `/proc/self/pagemap`; `PhysAddr`, `PagemapResolver`, `PhysResolver` trait, `MapStats` |
| `sysmem.rs` | Installed-RAM denominator (`/proc/iomem` "System RAM", `MemTotal` fallback) and single-run physical `Coverage`; `RamSource`, `InstalledRam`, `Cumulative` |
| `coverage.rs` | Cross-run coverage persistence (`--coverage-file`): `CoverageStore`, PFN range compaction/merge/subtraction, machine fingerprint guard |
| `gap.rs` | Untested-remainder classification via `/proc/kpageflags`: `FrameClass`, `GapReport`, system gap scan |
| `sieve.rs` | Frame-hostage culling (`--cull`): `FrameSieve` sweeps available RAM, holds covered 2 MiB blocks hostage, releases fresh frames for the test buffer |
| `devmem.rs` | `/dev/mem` targeted testing (`--devmem`): target parsing, `memmap=`/iomem write-safety classification, `pread` read-only probe, trivial `DevMemResolver` (phys known exactly) |
| `edac.rs` | ECC error counters from `/sys/devices/system/edac/`; `EdacSnapshot`, `DimmEdac`, `EccDelta` |
| `smbios.rs` | DIMM info from `/sys/firmware/dmi/tables/` |
| `dimm.rs` | `DimmTopology` — merges SMBIOS + EDAC into a per-DIMM view |
| `error_analysis.rs` | Post-test bit error classification: `BitErrorStats`, `ErrorClassification` (StuckBit, Coupling, Mixed) |
| `headless.rs` | `HeadlessPrinter` — human-readable live output from event bus |
| `ndjson.rs` | `NdjsonEventWriter` — NDJSON event serialization for `--format json` / `--events` |
| `results.rs` | `ResultsDoc`, `ResultsRenderer` trait, `TableRenderer`, `JsonRenderer` |
| `units.rs` | Binary/decimal size and rate formatting |
| `shutdown.rs` | Signal handling, panic hook, coordinated shutdown, exit codes |
| `tui/mod.rs` | `Segment`, `TuiEvent`, `TuiConfig`, `run_tui()`, `run_event_loop<B>()`, `finish_viewport<B>()` (exit teardown: drain buffered logs to scrollback, collapse the viewport cleanly) |
| `tui/run.rs` | `run_tui_mode()`, `TuiTestSetup` |
| `tui/render.rs` | Frame rendering: heatmaps, progress bars, status header |
| `tui/activity.rs` | Activity tracking for heatmap data |
| `tui/palette.rs` | Color palette functions (error severity, background fade) |

## Data Flow

```
main()
 ├─ install_signal_handlers(), install_panic_hook()
 ├─ init_tracing() → reloadable stderr layer  [main.rs]
 ├─ check_privileges(size, need_phys)         [cli.rs]
 ├─ select patterns (Pattern::ALL or --test)
 ├─ conflict check: --format json + --tui → error
 │
 ├─ TUI mode:
 │   ├─ setup_test() → TestSetup              [cli.rs]
 │   └─ run_tui_mode(...)                     [tui/run.rs]
 │       ├─ hot-swap tracing layer → TUI channel (reload handle)
 │       ├─ spawn "test-driver" thread → runner::run() over the whole allocation, emits RunEvents
 │       │   (rayon parallelism inside the pattern loop when --parallel > 1)
 │       ├─ spawn "event-bridge" thread → EventBridge translates RunEvent → TuiEvent
 │       ├─ run_tui(config, segment, tx, rx)  ← event loop [tui/mod.rs]
 │       └─ wait for test-driver (5 s timeout) + process::exit
 │
 └─ Headless mode:                            (tracing stays on stderr)
     ├─ setup_test() → TestSetup              [cli.rs]
     ├─ create NdjsonEventWriter (if --format json or --events)  [ndjson.rs]
     ├─ spawn consumer thread:
     │   ├─ HeadlessPrinter.handle_event()    [headless.rs]
     │   └─ NdjsonEventWriter.handle_event()  [ndjson.rs, optional]
     ├─ runner::run(buf, patterns, passes, parallel, &tx, resolver)   [runner.rs]
     ├─ error_analysis (if failures)          [error_analysis.rs]
     ├─ NdjsonEventWriter.write_run_complete()  [optional]
     ├─ TableRenderer.render(ResultsDoc)      [results.rs]
     └─ exit code
```
