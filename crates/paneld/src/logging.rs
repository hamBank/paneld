//! Log output setup (`--log-format`, `PANELD_LOG_FORMAT`).
//!
//! Two formats, both on stdout and both filtered by `RUST_LOG` (default
//! `info`):
//!
//! * `text`: the human format, one line per event. The right choice for
//!   journald and for a terminal.
//! * `json`: one JSON object per line (`timestamp`, `level`, `fields`,
//!   `target`), for log shippers such as Vector, Promtail or Fluent Bit.
//!
//! Never log secrets: tokens come from `*_env` variables and are not stored
//! in any value that gets formatted. Errors from `reqwest` carry the request
//! URL, so sources log `e.without_url()`.

use std::io::IsTerminal;

use clap::ValueEnum;
use tracing::Subscriber;
use tracing_subscriber::fmt::MakeWriter;
use tracing_subscriber::EnvFilter;

/// Log line format.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, ValueEnum)]
pub enum LogFormat {
    /// Human-readable lines (journald, terminals).
    #[default]
    Text,
    /// One JSON object per line (log shippers).
    Json,
}

/// The filter from `RUST_LOG`, or `info` when it is unset or invalid.
pub fn env_filter() -> EnvFilter {
    EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info"))
}

/// Build a subscriber writing `format` lines to `writer`. `ansi` colours the
/// text format; JSON is never coloured.
pub fn subscriber<W>(
    format: LogFormat,
    filter: EnvFilter,
    writer: W,
    ansi: bool,
) -> Box<dyn Subscriber + Send + Sync>
where
    W: for<'w> MakeWriter<'w> + Send + Sync + 'static,
{
    let builder = tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_writer(writer);
    match format {
        LogFormat::Text => Box::new(builder.with_ansi(ansi).finish()),
        LogFormat::Json => Box::new(builder.json().finish()),
    }
}

/// Install the global subscriber on stdout. Colours only on a terminal, so
/// journald and files never get escape codes.
pub fn init(format: LogFormat) {
    let ansi = std::io::stdout().is_terminal();
    let sub = subscriber(format, env_filter(), std::io::stdout, ansi);
    // Fails only if a subscriber is already installed; keep that one.
    let _ = tracing::subscriber::set_global_default(sub);
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{Arc, Mutex};

    #[derive(Clone, Default)]
    struct Capture(Arc<Mutex<Vec<u8>>>);

    impl std::io::Write for Capture {
        fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
            self.0.lock().unwrap().extend_from_slice(buf);
            Ok(buf.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    impl<'a> MakeWriter<'a> for Capture {
        type Writer = Capture;
        fn make_writer(&'a self) -> Capture {
            self.clone()
        }
    }

    fn capture(format: LogFormat, filter: &str, emit: impl FnOnce()) -> String {
        let cap = Capture::default();
        let sub = subscriber(format, EnvFilter::new(filter), cap.clone(), false);
        tracing::subscriber::with_default(sub, emit);
        let bytes = cap.0.lock().unwrap().clone();
        String::from_utf8(bytes).unwrap()
    }

    #[test]
    fn json_lines_parse_with_level_target_and_fields() {
        let out = capture(LogFormat::Json, "info", || {
            tracing::info!(source = "ha", count = 3, "sources started");
            tracing::warn!("second line");
        });
        let lines: Vec<serde_json::Value> = out
            .lines()
            .map(|l| serde_json::from_str(l).expect("each line is a JSON object"))
            .collect();
        assert_eq!(lines.len(), 2);
        let first = &lines[0];
        assert_eq!(first["level"], "INFO");
        assert_eq!(first["target"], "paneld::logging::tests");
        assert_eq!(first["fields"]["message"], "sources started");
        assert_eq!(first["fields"]["source"], "ha");
        assert_eq!(first["fields"]["count"], 3);
        assert!(first["timestamp"].is_string());
        assert_eq!(lines[1]["level"], "WARN");
    }

    #[test]
    fn json_escapes_newlines_so_one_event_stays_one_line() {
        let out = capture(LogFormat::Json, "info", || {
            tracing::error!("a\nb \"quoted\"");
        });
        assert_eq!(out.lines().count(), 1);
        let v: serde_json::Value = serde_json::from_str(out.trim_end()).unwrap();
        assert_eq!(v["fields"]["message"], "a\nb \"quoted\"");
    }

    #[test]
    fn text_format_is_not_json_and_has_no_escape_codes() {
        let out = capture(LogFormat::Text, "info", || {
            tracing::info!(source = "ha", "connected");
        });
        assert!(serde_json::from_str::<serde_json::Value>(out.trim_end()).is_err());
        assert!(out.contains("INFO"));
        assert!(out.contains("connected"));
        assert!(out.contains("source=\"ha\""));
        assert!(!out.contains('\x1b'));
    }

    #[test]
    fn filter_applies_to_both_formats() {
        for format in [LogFormat::Text, LogFormat::Json] {
            let out = capture(format, "warn", || {
                tracing::info!("hidden");
                tracing::warn!("shown");
            });
            assert!(!out.contains("hidden"), "{format:?}");
            assert!(out.contains("shown"), "{format:?}");
        }
    }

    #[test]
    fn filter_can_target_a_module() {
        let out = capture(LogFormat::Json, "warn,paneld::logging=debug", || {
            tracing::debug!("module debug");
        });
        assert!(out.contains("module debug"));
    }
}
