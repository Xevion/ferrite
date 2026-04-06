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

### Results (`OutputSink`)

Final test results and structured events go through `OutputSink`. The format is controlled by `--json`:

- **Default:** Human-readable summary via `OutputSink::Human`.
- **`--json` / `--json -`:** NDJSON events to stdout.
- **`--json path`:** NDJSON events to a file, human output to stdout.

**Constraint:** `--json -` (stdout) is incompatible with `--tui` because both claim stdout. ferrite errors with guidance: use `--json <file>` or `--tui never`.

In TUI mode, `OutputSink` is wrapped in `Arc<Mutex<>>` so segment workers can emit JSON events concurrently. The `print_*()` methods (human output) are suppressed during TUI mode since the TUI itself provides the visual display.

Code: `src/output.rs` (`OutputSink` enum, event emission, human-readable printing)

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
| `output.rs` | `OutputSink` — human-readable and NDJSON output |
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
 ├─ create OutputSink (Human or Json)
 ├─ conflict check: --json stdout + --tui → error
 │
 ├─ TUI mode:
 │   ├─ setup_test() → TestSetup              [cli.rs]
 │   │   ├─ LockedRegion::new(size)           [alloc.rs]
 │   │   ├─ CompactionGuard::new()            [alloc.rs, optional]
 │   │   ├─ PagemapResolver + build_map()     [phys.rs, optional]
 │   │   └─ DimmTopology::build()             [dimm.rs, optional]
 │   └─ run_tui_mode(...)                     [tui/run.rs]
 │       ├─ setup_tracing(json, Some(TuiMakeWriter))
 │       ├─ emit_map_info to sink
 │       ├─ split allocation into N segments  (--regions or CPU count)
 │       ├─ spawn "test-driver" thread
 │       │   └─ thread::scope → N scoped segment threads
 │       │       └─ run_region_worker(chunk, patterns, passes, ...)
 │       │           ├─ run_pattern() per pattern × pass   [pattern/]
 │       │           ├─ emit_test_start / emit_test_complete to sink (Arc<Mutex>)
 │       │           ├─ send TuiEvent to TUI channel
 │       │           └─ ECC snapshot delta → emit_ecc_deltas
 │       ├─ run_tui(config, segments, tx, rx)  ← event loop [tui/mod.rs]
 │       ├─ wait for test-driver (5 s timeout)
 │       └─ emit_summary + print_final_result + process::exit
 │
 └─ Headless mode:
     ├─ setup_tracing(json, None)
     ├─ setup_test() → TestSetup              [cli.rs]
     ├─ emit_map_info / print_map_info
     ├─ runner::run(buf, patterns, passes, parallel, sink, resolver)   [runner.rs]
     ├─ error_analysis (if failures)          [error_analysis.rs]
     └─ emit_summary + print_final_result + exit code
```
