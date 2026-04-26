use std::fs::OpenOptions;
use std::io;
use std::path::Path;
use std::sync::Mutex;

use chrono::Local;
use tracing::{Event, Level, Subscriber};
use tracing_subscriber::filter::LevelFilter;
use tracing_subscriber::fmt::format::Writer;
use tracing_subscriber::fmt::{self, FmtContext, FormatEvent, FormatFields};
use tracing_subscriber::layer::SubscriberExt;
use tracing_subscriber::registry::LookupSpan;
use tracing_subscriber::util::SubscriberInitExt;
use tracing_subscriber::EnvFilter;

const COLOR_GREEN: &str = "\x1b[32m";
const COLOR_BLUE: &str = "\x1b[34m";
const COLOR_MAGENTA: &str = "\x1b[35m";
const COLOR_YELLOW: &str = "\x1b[33m";
const COLOR_RED: &str = "\x1b[31m";
const COLOR_RESET: &str = "\x1b[0m";

#[derive(Debug, thiserror::Error)]
pub enum LoggerError {
    #[error("create log directory `{path}`: {source}")]
    CreateDir {
        path: String,
        #[source]
        source: io::Error,
    },
    #[error("open log file `{path}`: {source}")]
    OpenFile {
        path: String,
        #[source]
        source: io::Error,
    },
    #[error("install global tracing subscriber: {0}")]
    Install(#[from] tracing_subscriber::util::TryInitError),
}

/// Match the Go logger output: `[YYYY-MM-DD HH:MM:SS.mmm][LEVEL][target] message`.
/// `WARN` is rendered as `WARNING` and TRACE/DEBUG share the blue stdout colour
/// so log files written by either build look the same.
#[derive(Debug, Clone, Copy)]
struct GoStyleFormat {
    ansi: bool,
}

impl<S, N> FormatEvent<S, N> for GoStyleFormat
where
    S: Subscriber + for<'a> LookupSpan<'a>,
    N: for<'a> FormatFields<'a> + 'static,
{
    fn format_event(
        &self,
        ctx: &FmtContext<'_, S, N>,
        mut writer: Writer<'_>,
        event: &Event<'_>,
    ) -> std::fmt::Result {
        let now = Local::now().format("%Y-%m-%d %H:%M:%S%.3f");
        let metadata = event.metadata();
        let level = *metadata.level();
        let level_label = match level {
            Level::TRACE => "TRACE",
            Level::DEBUG => "DEBUG",
            Level::INFO => "INFO",
            Level::WARN => "WARNING",
            Level::ERROR => "ERROR",
        };
        let target = metadata.target();

        if self.ansi {
            let level_color = match level {
                Level::TRACE | Level::DEBUG => COLOR_BLUE,
                Level::INFO => COLOR_GREEN,
                Level::WARN => COLOR_YELLOW,
                Level::ERROR => COLOR_RED,
            };
            write!(
                writer,
                "{g}[{ts}]{r}[{lc}{lvl}{r}][{m}{tgt}{r}] ",
                g = COLOR_GREEN,
                r = COLOR_RESET,
                ts = now,
                lc = level_color,
                lvl = level_label,
                m = COLOR_MAGENTA,
                tgt = target,
            )?;
        } else {
            write!(writer, "[{ts}][{lvl}][{tgt}] ", ts = now, lvl = level_label, tgt = target)?;
        }

        ctx.field_format().format_fields(writer.by_ref(), event)?;
        writeln!(writer)
    }
}

/// Map Go's level keywords (`DEBUG`/`INFO`/`WARN`/`WARNING`/`ERROR`/`CRITICAL`)
/// onto a `tracing` filter. Anything unrecognised falls back to `INFO`, matching
/// the Go logger's default.
fn parse_level(level: &str) -> LevelFilter {
    match level.trim().to_uppercase().as_str() {
        "TRACE" => LevelFilter::TRACE,
        "DEBUG" => LevelFilter::DEBUG,
        "INFO" => LevelFilter::INFO,
        "WARN" | "WARNING" => LevelFilter::WARN,
        "ERROR" | "CRITICAL" => LevelFilter::ERROR,
        _ => LevelFilter::INFO,
    }
}

/// Initialise the global tracing subscriber. Stdout always receives ANSI output;
/// when `file` is `Some` and non-empty, a parallel layer mirrors the same events
/// (sans ANSI) into the file. Caller may pass an empty string to disable file
/// output, matching the Go config where `main_log_file: ""` is valid.
pub fn init<P: AsRef<Path>>(level: &str, file: Option<P>) -> Result<(), LoggerError> {
    let level_filter = parse_level(level);
    let env_filter = EnvFilter::builder()
        .with_default_directive(level_filter.into())
        .from_env_lossy();

    let stdout_layer = fmt::layer()
        .event_format(GoStyleFormat { ansi: true })
        .with_writer(io::stdout);

    let file_layer = match file {
        Some(p) if !p.as_ref().as_os_str().is_empty() => {
            let path = p.as_ref();
            if let Some(parent) = path.parent() {
                if !parent.as_os_str().is_empty() {
                    std::fs::create_dir_all(parent).map_err(|source| LoggerError::CreateDir {
                        path: parent.display().to_string(),
                        source,
                    })?;
                }
            }
            let f = OpenOptions::new()
                .create(true)
                .append(true)
                .open(path)
                .map_err(|source| LoggerError::OpenFile {
                    path: path.display().to_string(),
                    source,
                })?;
            Some(
                fmt::layer()
                    .event_format(GoStyleFormat { ansi: false })
                    .with_writer(Mutex::new(f)),
            )
        }
        _ => None,
    };

    tracing_subscriber::registry()
        .with(env_filter)
        .with(stdout_layer)
        .with(file_layer)
        .try_init()?;
    Ok(())
}
