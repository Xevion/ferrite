# Architecture

## Output Model

ferrite separates output into three orthogonal concerns:

### TUI (`--tui auto|always|never`)

Controls the live interactive display. In `auto` mode (default), the TUI activates when stdout is a terminal and falls back to headless mode otherwise.

- **TUI on:** ratatui renders an inline viewport to stdout with heatmaps, progress, and live log lines. Tracing events are captured and displayed inline via a dedicated tracing layer.
- **TUI off:** No interactive display. Tracing goes to stderr only. Progress bars via indicatif.

Code: `src/tui/` (event loop, rendering, `TuiMakeWriter` for tracing integration)

### Tracing (stderr)

Diagnostic logs (`info!`, `warn!`, etc.) always go to stderr via `tracing-subscriber`. This includes physical address map stats, DIMM topology, region lifecycle events, ECC deltas, and warnings.

Tracing uses a layered subscriber (`tracing_subscriber::registry`) with conditional layers:

| Mode              | TUI layer                | stderr layer                |
|-------------------|--------------------------|-----------------------------|
| TUI + `--json`    | human ANSI -> TUI channel | JSON -> stderr               |
| TUI + no `--json` | human ANSI -> TUI channel | human (no ANSI) -> stderr    |
| no TUI + `--json` | --                        | JSON -> stderr               |
| no TUI + no JSON  | --                        | human -> stderr              |

Each layer is an `Option<Layer>` (`None` = no-op). The TUI channel layer and the stderr layer run independently -- every tracing event is formatted and dispatched to both active layers.

Tracing is for moment-by-moment operational detail, not final results.

### Results (`OutputSink`)

Final test results and structured events go through `OutputSink`. The format is controlled by `--json`:

- **Default:** Human-readable summary via `OutputSink::Human`.
- **`--json` / `--json -`:** NDJSON events to stdout.
- **`--json path`:** NDJSON events to a file, human output to stdout.

**Constraint:** `--json -` (stdout) is incompatible with `--tui` because both claim stdout. ferrite errors with guidance: use `--json <file>` or `--tui never`. `--json <file>` works with any TUI mode.

In TUI mode, `OutputSink` is wrapped in `Arc<Mutex<>>` so region workers can emit JSON events concurrently. The `print_*()` methods (human output) are suppressed during TUI mode since the TUI itself provides the visual display.

Code: `src/output.rs` (`OutputSink` enum, event emission, human-readable printing)

## Module Map

| Module | Purpose |
|---|---|
| `main.rs` | CLI parsing, mode dispatch, worker orchestration, tracing setup |
| `alloc.rs` | `LockedRegion` -- mmap + mlock anonymous memory |
| `pattern.rs` | Test patterns (solid bits, walking ones/zeros, checkerboard, stuck address) |
| `runner.rs` | Multi-pass test runner, coordinates patterns across regions |
| `simd.rs` | SIMD/non-temporal fill and verify routines |
| `phys.rs` | Physical address resolution via `/proc/self/pagemap` |
| `stability.rs` | `CompactionGuard` -- disables kernel page migration during tests |
| `edac.rs` | ECC error counters from `/sys/devices/system/edac/` |
| `smbios.rs` | DIMM info from `/sys/firmware/dmi/tables/` |
| `dimm.rs` | `DimmTopology` -- merges SMBIOS + EDAC into per-DIMM view |
| `error_analysis.rs` | Post-test bit error classification (stuck bits, coupling, mixed) |
| `output.rs` | `OutputSink` -- human-readable and NDJSON output |
| `units.rs` | Binary/decimal size and rate formatting |
| `tui/` | ratatui TUI: event loop, heatmap rendering, activity tracking, palette |

## Data Flow

```
main()
 ├─ parse CLI, check privileges
 ├─ create OutputSink (human or JSON)
 ├─ conflict guard: --json stdout + TUI -> error
 ├─ setup_tracing() -> layered registry (TUI layer + stderr layer)
 ├─ setup_test() -> TestSetup:
 │   ├─ allocate LockedRegion (mmap + mlock + parallel page fault)
 │   ├─ CompactionGuard (optional)
 │   ├─ setup_phys() -> (PagemapResolver, MapStats) (optional)
 │   └─ DimmTopology::build() (optional)
 │
 ├─ TUI mode:
 │   ├─ wrap OutputSink in Arc<Mutex<>>
 │   ├─ emit map_info to sink
 │   ├─ spawn region workers (thread::scope)
 │   │   └─ run_region_worker() per chunk
 │   │       ├─ run_pattern() for each pattern x pass
 │   │       ├─ emit_test_start / emit_test_complete to sink
 │   │       ├─ send TuiEvent::Error / RegionDone
 │   │       └─ ECC snapshot delta -> emit_ecc_deltas
 │   ├─ run_tui() event loop (render, keyboard, tick)
 │   └─ emit_summary + print_final_result
 │
 └─ Headless mode:
     ├─ emit_map_info / print_map_info to sink
     ├─ runner::run() (patterns x passes, progress via OutputSink)
     ├─ error analysis (if failures)
     └─ emit_summary + print_final_result + exit code
```
