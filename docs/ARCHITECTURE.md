# Architecture

## Output Model

ferrite separates output into three orthogonal concerns.

### TUI (`--tui auto|always|never`)

Controls the live interactive display. In `auto` mode (default), the TUI activates when stdout is a terminal and falls back to headless mode otherwise.

- **TUI on:** ratatui renders an inline viewport to stdout with heatmaps, progress, and live log lines. Tracing events are captured and displayed inline via a dedicated tracing layer.
- **TUI off:** No interactive display. Tracing goes to stderr only.

Code: `src/tui/` (event loop, rendering, `TuiMakeWriter` for tracing integration)

### Tracing (stderr)

Diagnostic logs (`info!`, `warn!`, etc.) go to stderr via `tracing-subscriber`. This includes physical address map stats, DIMM topology, segment lifecycle events, ECC deltas, and warnings.

Tracing uses a layered subscriber (`tracing_subscriber::registry`) with conditional layers:

| Mode              | TUI layer                  | stderr layer                |
|-------------------|----------------------------|-----------------------------|
| TUI + `--json`    | human ANSI → TUI channel   | JSON → stderr               |
| TUI + no `--json` | human ANSI → TUI channel   | (none — events to TUI only) |
| no TUI + `--json` | —                          | JSON → stderr               |
| no TUI + no JSON  | —                          | human → stderr              |

Each layer is an `Option<Layer>` (`None` = no-op). Setup lives in `tui/run.rs::setup_tracing()`.

Tracing is for moment-by-moment operational detail, not final results.

### Results (Event Consumers + Renderers)

Output is split into three independent concerns:

- **Live human text:** `HeadlessPrinter` consumes `RunEvent`s and writes PASS/FAIL lines, banners, ECC info to stdout. Active in headless (non-TUI) mode.
- **NDJSON events:** `NdjsonEventWriter` serializes `RunEvent`s as newline-delimited JSON. Active when `--json` is specified. Writes to stdout (`--json -`) or a file (`--json path`).
- **Post-run results:** `ResultsDoc` + `ResultsRenderer` trait (`TableRenderer`, `JsonRenderer`) render the final summary after the run completes.

**Constraint:** `--json` is incompatible with `--tui` (TUI handles its own live display). ferrite errors with guidance: use `--tui never` for JSON output.

In TUI mode, the `EventBridge` translates `RunEvent`s to `TuiEvent`s for the TUI event loop. No NDJSON or headless printing occurs.

Code: `src/headless.rs`, `src/ndjson.rs`, `src/results.rs`, `src/tui/bridge.rs`

## Module Map

| Module | Purpose |
|---|---|
| `main.rs` | Entry point, mode dispatch, signal handler installation |
| `cli.rs` | `Cli` (clap derive), `setup_test()`, `check_privileges()`, size parsing |
| `alloc.rs` | `LockedRegion` — mmap + mlock anonymous memory; `CompactionGuard` — disables kernel page compaction during tests |
| `failure.rs` | `Failure` — one failing 64-bit word (virtual address, expected, actual, physical address) |
| `pattern/` | `Pattern` enum, `run_pattern()`, per-pattern modules: `solid`, `walking`, `checkerboard`, `stuck_address` |
| `runner.rs` | Headless multi-pass runner: `run()`, `PassResult`, `PatternResult` |
| `ops/` | Three-layer fill/verify operations: `scalar.rs` (coverage-measured), `avx512.rs` (excluded from coverage), `mod.rs` (dispatch) |
| `phys.rs` | Physical address resolution via `/proc/self/pagemap`; `PhysAddr`, `PagemapResolver`, `PhysResolver` trait, `MapStats` |
| `edac.rs` | ECC error counters from `/sys/devices/system/edac/`; `EdacSnapshot`, `DimmEdac`, `EccDelta` |
| `smbios.rs` | DIMM info from `/sys/firmware/dmi/tables/` |
| `dimm.rs` | `DimmTopology` — merges SMBIOS + EDAC into a per-DIMM view |
| `error_analysis.rs` | Post-test bit error classification: `BitErrorStats`, `ErrorClassification` (StuckBit, Coupling, Mixed) |
| `headless.rs` | `HeadlessPrinter` — human-readable live output from event bus |
| `ndjson.rs` | `NdjsonEventWriter` — NDJSON event serialization for `--json` |
| `results.rs` | `ResultsDoc`, `ResultsRenderer` trait, `TableRenderer`, `JsonRenderer` |
| `units.rs` | Binary/decimal size and rate formatting |
| `shutdown.rs` | Signal handling, panic hook, coordinated shutdown, exit codes |
| `tui/mod.rs` | `RegionState`, `TuiEvent`, `TuiConfig`, `run_tui()`, `run_event_loop<B>()` |
| `tui/run.rs` | `run_tui_mode()`, `run_region_worker()`, `setup_tracing()`, `TuiTestSetup` |
| `tui/render.rs` | Frame rendering: heatmaps, progress bars, status header |
| `tui/activity.rs` | Activity tracking for heatmap data |
| `tui/palette.rs` | Color palette functions (error severity, background fade) |

## Data Flow

```
main()
 ├─ install_signal_handlers(), install_panic_hook()
 ├─ check_privileges(size, need_phys)         [cli.rs]
 ├─ select patterns (Pattern::ALL or --test)
 ├─ conflict check: --json + --tui → error
 │
 ├─ TUI mode:
 │   ├─ setup_test() → TestSetup              [cli.rs]
 │   └─ run_tui_mode(...)                     [tui/run.rs]
 │       ├─ setup_tracing(json_mode, Some(TuiMakeWriter))
 │       ├─ split allocation into N segments  (--regions or CPU count)
 │       ├─ spawn "test-driver" thread → runner::run() emits RunEvents
 │       ├─ spawn "event-bridge" thread → EventBridge translates RunEvent → TuiEvent
 │       ├─ run_tui(config, segments, tx, rx)  ← event loop [tui/mod.rs]
 │       └─ wait for test-driver (5 s timeout) + process::exit
 │
 └─ Headless mode:
     ├─ setup_tracing(json, None)
     ├─ setup_test() → TestSetup              [cli.rs]
     ├─ create NdjsonEventWriter (if --json)  [ndjson.rs]
     ├─ spawn consumer thread:
     │   ├─ HeadlessPrinter.handle_event()    [headless.rs]
     │   └─ NdjsonEventWriter.handle_event()  [ndjson.rs, optional]
     ├─ runner::run(buf, patterns, passes, parallel, &tx, resolver)   [runner.rs]
     ├─ error_analysis (if failures)          [error_analysis.rs]
     ├─ NdjsonEventWriter.write_summary()     [optional]
     ├─ TableRenderer.render(ResultsDoc)      [results.rs]
     └─ exit code
```
