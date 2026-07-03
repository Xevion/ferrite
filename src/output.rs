//! Shared headless output wiring.
//!
//! Event consumption ([`consume_headless_events`]), post-run results rendering
//! ([`render_results`]), and NDJSON events-file opening ([`open_events_writer`])
//! used by both the anonymous-memory headless path (`main.rs`) and the
//! `/dev/mem` backend (`devmem_run.rs`), so `--format` and `--events` behave
//! identically across execution modes.

use std::io::Write;

use snafu::{ResultExt, Whatever};

use ferrite::events::{EventRx, RunEvent};
use ferrite::headless::HeadlessPrinter;
use ferrite::ndjson::NdjsonEventWriter;
use ferrite::results::{ResultsDoc, ResultsRenderer, TableRenderer};

use crate::cli::{OutputConfig, OutputFormat};

/// Render final results based on output configuration.
///
/// When `full_table` is true, the table renderer includes per-pattern detail
/// (used after TUI exit, where no live output was shown). When false, only
/// the summary and error analysis are rendered (after `HeadlessPrinter`
/// already streamed live results). `full_table` is ignored for JSON output.
pub fn render_results(
    output: &OutputConfig,
    results: &ferrite::runner::RunResults,
    unit_system: ferrite::units::UnitSystem,
    full_table: bool,
    out: &mut dyn Write,
) {
    let doc = ResultsDoc::from_results(results);
    match output.format {
        OutputFormat::Json => {
            ferrite::results::JsonRenderer
                .render(&doc, out)
                .unwrap_or_else(|e| eprintln!("warning: failed to render results: {e}"));
        }
        OutputFormat::Table => {
            let renderer = if full_table {
                TableRenderer::full(unit_system)
            } else {
                TableRenderer::new(unit_system)
            };
            renderer
                .render(&doc, out)
                .unwrap_or_else(|e| eprintln!("warning: failed to render results: {e}"));
        }
    }
}

/// Open the NDJSON event writer for `--events <file>`, if configured.
///
/// # Errors
///
/// Fails when the events file cannot be created or opened for writing.
pub fn open_events_writer(output: &OutputConfig) -> Result<Option<NdjsonEventWriter>, Whatever> {
    output
        .events_file
        .as_deref()
        .map(|p| {
            let path_str = p
                .to_str()
                .expect("events_file path validated as UTF-8 in resolve_output");
            NdjsonEventWriter::from_path(path_str)
                .with_whatever_context(|_| format!("failed to open events file: {}", p.display()))
        })
        .transpose()
}

/// Consume events from the runner and drive human-readable output + JSON emission.
///
/// Runs on a dedicated thread. The [`HeadlessPrinter`] handles human-readable
/// text while [`NdjsonEventWriter`] handles JSON emission (when present).
pub fn consume_headless_events(
    rx: &EventRx,
    printer: &mut HeadlessPrinter<std::io::Stdout>,
    stdout_ndjson: &mut Option<NdjsonEventWriter>,
    events_ndjson: &mut Option<NdjsonEventWriter>,
    suppress_human: bool,
) {
    while let Ok(event) = rx.recv() {
        if !suppress_human {
            printer.handle_event(&event);
        }
        if let Some(w) = stdout_ndjson.as_mut() {
            w.handle_event(&event);
        }
        if let Some(w) = events_ndjson.as_mut() {
            w.handle_event(&event);
        }
        if matches!(event, RunEvent::RunComplete) {
            break;
        }
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use assert2::check;

    use ferrite::pattern::Pattern;
    use ferrite::runner::{RunConfig, RunResults};

    use crate::cli::{OutputConfig, OutputFormat};

    use super::render_results;

    fn clean_results() -> RunResults {
        let config = RunConfig {
            size: 4096,
            passes: 1,
            patterns: vec![Pattern::SolidBits],
            workers: 1,
        };
        RunResults::from_passes(vec![], config, Duration::ZERO)
    }

    fn output(format: OutputFormat) -> OutputConfig {
        OutputConfig {
            format,
            events_file: None,
            color_enabled: false,
        }
    }

    #[test]
    fn json_format_selects_json_renderer() {
        let results = clean_results();
        let mut buf = Vec::new();
        render_results(
            &output(OutputFormat::Json),
            &results,
            ferrite::units::UnitSystem::Binary,
            false,
            &mut buf,
        );
        let text = String::from_utf8(buf).unwrap();
        // JSON output parses as a JSON object; table output would not.
        let value: serde_json::Value = serde_json::from_str(&text).unwrap();
        check!(value.is_object());
        check!(value["total_failures"] == 0);
    }

    #[test]
    fn table_format_selects_table_renderer() {
        let results = clean_results();
        let mut buf = Vec::new();
        render_results(
            &output(OutputFormat::Table),
            &results,
            ferrite::units::UnitSystem::Binary,
            false,
            &mut buf,
        );
        let text = String::from_utf8(buf).unwrap();
        // Human table output is not valid JSON.
        check!(serde_json::from_str::<serde_json::Value>(&text).is_err());
    }
}
