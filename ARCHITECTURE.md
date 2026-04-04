# Architecture

## Output Model

ferrite separates output into three orthogonal concerns:

### TUI (`--tui auto|always|never`)

Controls the live interactive display. In `auto` mode (default), the TUI activates when stdout is a terminal and falls back to headless mode otherwise.

- **TUI on:** ratatui renders an inline viewport to stdout with heatmaps, progress, and live log lines. Tracing events are captured and displayed inline *and* teed to stderr.
- **TUI off:** No interactive display. Tracing goes to stderr only. Progress bars via indicatif.

Code: `src/tui/` (event loop, rendering, `TuiMakeWriter` for tracing integration)

### Tracing (stderr)

Diagnostic logs (`info!`, `warn!`, etc.) always go to stderr via `tracing-subscriber`. This includes physical address map stats, DIMM topology, region lifecycle events, ECC deltas, and warnings.

- **Headless mode:** `tracing_subscriber::fmt` writes directly to stderr.
- **TUI mode:** `TuiMakeWriter` tees each log line to both the TUI channel (for inline display) and stderr (for capture via `2>log.txt`).

Tracing is for moment-by-moment operational detail, not final results.

### Results (stdout)

Final test results and structured events go to stdout. The format is controlled by `--json`:

- **Default:** Human-readable summary via `OutputSink::Human`.
- **`--json` / `--json -`:** NDJSON events to stdout.
- **`--json path`:** NDJSON events to a file, human output to stdout.

`--json` is orthogonal to `--tui` -- you can run `--tui always --json events.jsonl` to get the interactive TUI while capturing structured output to a file.

Code: `src/output.rs` (`OutputSink` enum, event emission, human-readable printing)

## Module Map

| Module | Purpose |
|---|---|
| `main.rs` | CLI parsing, mode dispatch, worker orchestration |
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
 ├─ allocate LockedRegion (mmap + mlock + parallel page fault)
 ├─ setup_phys() → PagemapResolver (optional)
 ├─ DimmTopology::build() (optional)
 │
 ├─ TUI mode:
 │   ├─ set up tracing → TuiMakeWriter (tees to TUI + stderr)
 │   ├─ spawn region workers (thread::scope)
 │   │   └─ run_region_worker() per chunk
 │   │       ├─ run_pattern() for each pattern × pass
 │   │       ├─ send TuiEvent::Error / RegionDone
 │   │       └─ ECC snapshot delta
 │   └─ run_tui() event loop (render, keyboard, tick)
 │
 └─ Headless mode:
     ├─ set up tracing → stderr
     ├─ runner::run() (patterns × passes, progress via OutputSink)
     └─ emit summary + exit code
```
