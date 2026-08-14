//! Logging: a rolling file on disk, plus a live feed into the in-app viewer.
//!
//! **Secrets stay out of both by construction, not by filtering.** Keys are [`secrecy::SecretString`]
//! (its `Debug` prints `[REDACTED]`), `reqwest` errors are converted with `.without_url()` so a URL
//! carrying credentials in its query string cannot reach a log line, and request bodies are never
//! logged. A regex scrubber over the formatted output would be guesswork layered on top of that —
//! it would catch `sk-…` and miss everything else, while suggesting the real guarantee lives here.
//! It doesn't; it lives in the types.

use std::{fmt::Write as _, path::PathBuf};

use tokio::sync::broadcast;
use tracing::{
    Level,
    field::{Field, Visit},
};
use tracing_appender::non_blocking::WorkerGuard;
use tracing_subscriber::{EnvFilter, Layer, layer::Context, prelude::*};

use crate::proto::{EngineEvent, LogLevel, LogRecord};

/// `~/.local/share/meshflow/logs` on Linux, the platform equivalent elsewhere.
pub fn log_dir() -> Option<PathBuf> {
    directories::ProjectDirs::from("", "", "meshflow").map(|d| d.data_dir().join("logs"))
}

/// Install the subscriber. The returned guard flushes the log file on drop, so `main` must hold
/// it for the life of the process — dropping it early silently truncates the last writes.
pub fn init(events: broadcast::Sender<EngineEvent>) -> WorkerGuard {
    let sink: Box<dyn std::io::Write + Send> = match log_dir() {
        Some(dir) => Box::new(tracing_appender::rolling::daily(dir, "meshflow.log")),
        // No data directory is survivable: the console and the in-app viewer still work.
        None => Box::new(std::io::sink()),
    };
    let (file_writer, guard) = tracing_appender::non_blocking(sink);

    let filter = EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| EnvFilter::new("meshflow=info,mf_engine=info,mf_ui=info"));

    tracing_subscriber::registry()
        .with(filter)
        .with(tracing_subscriber::fmt::layer().with_writer(std::io::stderr))
        // No ANSI in the file: the point of it is to be pasted into a bug report.
        .with(tracing_subscriber::fmt::layer().with_ansi(false).with_writer(file_writer))
        .with(BroadcastLayer { events })
        .init();

    guard
}

/// Mirrors events onto the engine's broadcast channel so the log viewer can show them live.
struct BroadcastLayer {
    events: broadcast::Sender<EngineEvent>,
}

impl<S: tracing::Subscriber> Layer<S> for BroadcastLayer {
    fn on_event(&self, event: &tracing::Event<'_>, _ctx: Context<'_, S>) {
        let meta = event.metadata();
        let level = match *meta.level() {
            Level::ERROR => LogLevel::Error,
            Level::WARN => LogLevel::Warn,
            Level::INFO => LogLevel::Info,
            // DEBUG and TRACE go to the file only. Forwarding them would also put the UI's own
            // "receiver lagged" warning back into the channel it is complaining about.
            _ => return,
        };

        let mut fields = Collector::default();
        event.record(&mut fields);

        let _ = self.events.send(EngineEvent::Log(LogRecord {
            ts: chrono::Local::now().format("%H:%M:%S").to_string(),
            level,
            target: meta.target().to_owned(),
            message: fields.finish(),
        }));
    }
}

/// Flattens an event's fields into one line: the `message` first, then `key=value` pairs.
#[derive(Default)]
struct Collector {
    message: String,
    fields: String,
}

impl Collector {
    /// Takes the field *name* rather than the `Field` so this stays testable — `Field` has no
    /// public constructor and can only be obtained from a live callsite.
    fn push(&mut self, name: &str, value: std::fmt::Arguments<'_>) {
        if name == "message" {
            let _ = self.message.write_fmt(value);
        } else {
            let _ = write!(self.fields, " {name}=");
            let _ = self.fields.write_fmt(value);
        }
    }

    fn finish(self) -> String {
        format!("{}{}", self.message, self.fields)
    }
}

impl Visit for Collector {
    fn record_str(&mut self, field: &Field, value: &str) {
        self.push(field.name(), format_args!("{value}"));
    }

    fn record_debug(&mut self, field: &Field, value: &dyn std::fmt::Debug) {
        self.push(field.name(), format_args!("{value:?}"));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_message_leads_and_the_rest_follows_as_pairs() {
        // The viewer shows one line per record, and a message buried behind six key=value pairs
        // is a log nobody reads.
        let mut c = Collector::default();
        c.push("tool", format_args!("run_command"));
        c.push("message", format_args!("tool finished"));
        c.push("ok", format_args!("{}", true));
        assert_eq!(c.finish(), "tool finished tool=run_command ok=true");
    }
}
