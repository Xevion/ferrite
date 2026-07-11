//! Shared `RunResults` builders used by both the doc/query tests (`super`)
//! and the rendering tests (`super::render`).

use std::time::Duration;

use crate::error_analysis;
use crate::failure::FailureBuilder;
use crate::pattern::Pattern;
use crate::runner::{PassResult, PatternResult, RunConfig, RunResults};

pub fn make_config() -> RunConfig {
    RunConfig {
        size: 8192,
        passes: 1,
        patterns: vec![Pattern::SolidBits],
        workers: 1,
    }
}

pub fn clean_results() -> RunResults {
    RunResults::from_passes(
        vec![PassResult {
            pass_number: 1,
            pattern_results: vec![PatternResult {
                pattern: Pattern::SolidBits,
                failures: vec![],
                elapsed: Duration::from_millis(100),
                bytes_processed: 8192,
                interrupted: false,
                capped: false,
            }],
            ecc_deltas: vec![],
        }],
        make_config(),
        Duration::from_millis(100),
    )
}

pub fn failing_results() -> RunResults {
    let failures = vec![
        FailureBuilder::default()
            .addr(0x1000)
            .expected(0x0)
            .actual(1 << 20)
            .phys(0x5000)
            .build(),
        FailureBuilder::default()
            .addr(0x2000)
            .expected(0x0)
            .actual(1 << 20)
            .phys(0x9000)
            .build(),
    ];
    let mut results = RunResults::from_passes(
        vec![PassResult {
            pass_number: 1,
            pattern_results: vec![PatternResult {
                pattern: Pattern::SolidBits,
                failures,
                elapsed: Duration::from_millis(50),
                bytes_processed: 8192,
                interrupted: false,
                capped: false,
            }],
            ecc_deltas: vec![],
        }],
        make_config(),
        Duration::from_millis(50),
    );
    error_analysis::analyze(&mut results);
    results
}

/// One pass, two patterns -- distinct timings and sizes so per-pattern
/// rendering is observable.
pub fn multi_pattern_results() -> RunResults {
    RunResults::from_passes(
        vec![PassResult {
            pass_number: 1,
            pattern_results: vec![
                PatternResult {
                    pattern: Pattern::SolidBits,
                    failures: vec![],
                    elapsed: Duration::from_millis(100),
                    bytes_processed: 8192,
                    interrupted: false,
                    capped: false,
                },
                PatternResult {
                    pattern: Pattern::Checkerboard,
                    failures: vec![],
                    elapsed: Duration::from_millis(50),
                    bytes_processed: 4096,
                    interrupted: false,
                    capped: false,
                },
            ],
            ecc_deltas: vec![],
        }],
        RunConfig {
            size: 16384,
            passes: 1,
            patterns: vec![Pattern::SolidBits, Pattern::Checkerboard],
            workers: 4,
        },
        Duration::from_millis(150),
    )
}

/// Clean results with measured coverage: 64 MiB tested of 32 GiB installed.
pub fn covered_results() -> RunResults {
    let mut r = clean_results();
    r.coverage = crate::physmem::sysmem::Coverage::Measured {
        tested_bytes: 64 * 1024 * 1024,
        total_bytes: 32 * 1024 * 1024 * 1024,
        source: crate::physmem::sysmem::RamSource::ProcIomem,
        cumulative: None,
        gap: None,
    };
    r
}
