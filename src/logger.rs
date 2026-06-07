use std::fs::OpenOptions;
use std::io;
use std::path::Path;
use std::sync::Mutex;

use chrono::Local;
use tracing::field::{Field, Visit};
use tracing::{Event, Level, Subscriber};
use tracing_subscriber::EnvFilter;
use tracing_subscriber::filter::{LevelFilter, Targets};
use tracing_subscriber::fmt::format::Writer;
use tracing_subscriber::fmt::{self, FmtContext, FormatEvent, FormatFields};
use tracing_subscriber::layer::{Layer, SubscriberExt};
use tracing_subscriber::registry::LookupSpan;
use tracing_subscriber::util::SubscriberInitExt;

const COLOR_GREEN: &str = "\x1b[32m";
const COLOR_BLUE: &str = "\x1b[34m";
const COLOR_MAGENTA: &str = "\x1b[35m";
const COLOR_YELLOW: &str = "\x1b[33m";
const COLOR_RED: &str = "\x1b[31m";
const COLOR_CYAN: &str = "\x1b[36m";
const COLOR_WHITE: &str = "\x1b[37m";
const COLOR_DARK_ORANGE: &str = "\x1b[38;5;208m";
const COLOR_CYAN1: &str = "\x1b[38;5;51m";
const COLOR_DARK_SLATE_GRAY1: &str = "\x1b[38;5;123m";
const COLOR_BRIGHT_BLUE: &str = "\x1b[94m";
const COLOR_BRIGHT_MAGENTA: &str = "\x1b[95m";
const COLOR_BRIGHT_CYAN: &str = "\x1b[96m";
const COLOR_RESET: &str = "\x1b[0m";

const ACCESS_TARGET: &str = "access";

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

#[derive(Debug, Clone, Copy)]
struct HarukiFormat {
    ansi: bool,
}

impl<S, N> FormatEvent<S, N> for HarukiFormat
where
    S: Subscriber + for<'a> LookupSpan<'a>,
    N: for<'a> FormatFields<'a> + 'static,
{
    fn format_event(
        &self,
        _ctx: &FmtContext<'_, S, N>,
        mut writer: Writer<'_>,
        event: &Event<'_>,
    ) -> std::fmt::Result {
        let now = Local::now().format("%Y-%m-%d %H:%M:%S%.3f");
        let metadata = event.metadata();
        let level = level_name(metadata.level());
        let component = component_name(metadata.target());
        let mut visitor = EventVisitor::default();
        event.record(&mut visitor);

        let identity_tags = visitor.identity_tags(self.ansi);
        let after_component = if identity_tags.is_empty() { " " } else { "" };
        let after_identity = if identity_tags.is_empty() { "" } else { " " };
        let fields = if visitor.fields.is_empty() {
            String::new()
        } else {
            format!(" {}", visitor.fields.join(" "))
        };
        let plain_message = format!("{}{}", visitor.message.unwrap_or_default(), fields);

        if self.ansi {
            let level_color = level_color(metadata.level());
            let component_color = component_color(component);
            write!(
                writer,
                "{}[{}]{}[{}{}{}][{}{}{}]{}{}{}{}{}{}",
                COLOR_DARK_SLATE_GRAY1,
                now,
                COLOR_RESET,
                level_color,
                level,
                COLOR_RESET,
                component_color,
                component,
                COLOR_RESET,
                after_component,
                identity_tags,
                after_identity,
                COLOR_WHITE,
                plain_message,
                COLOR_RESET
            )?;
        } else {
            write!(
                writer,
                "[{ts}][{lvl}][{component}]{after_component}{identity_tags}{after_identity}{message}",
                ts = now,
                lvl = level,
                component = component,
                after_component = after_component,
                identity_tags = identity_tags,
                after_identity = after_identity,
                message = plain_message
            )?;
        }

        writeln!(writer)
    }
}

#[derive(Default)]
struct EventVisitor {
    message: Option<String>,
    user_id: Option<String>,
    fields: Vec<String>,
}

impl EventVisitor {
    fn record_value(&mut self, field: &Field, value: String) {
        match field.name() {
            "message" => self.message = Some(value),
            "user_id" | "uid" if !value.trim().is_empty() => self.user_id = Some(value),
            "log_message" => self.fields.push(format!("message={value}")),
            _ => self.fields.push(format!("{}={}", field.name(), value)),
        }
    }

    fn identity_tags(&self, ansi: bool) -> String {
        let mut tags = String::new();
        if let Some(user_id) = &self.user_id {
            if ansi {
                tags.push_str(&format!("[{}User-{}{}]", COLOR_CYAN1, user_id, COLOR_RESET));
            } else {
                tags.push_str(&format!("[User-{user_id}]"));
            }
        }
        tags
    }
}

impl Visit for EventVisitor {
    fn record_str(&mut self, field: &Field, value: &str) {
        self.record_value(field, value.to_string());
    }

    fn record_bool(&mut self, field: &Field, value: bool) {
        self.record_value(field, value.to_string());
    }

    fn record_i64(&mut self, field: &Field, value: i64) {
        self.record_value(field, value.to_string());
    }

    fn record_u64(&mut self, field: &Field, value: u64) {
        self.record_value(field, value.to_string());
    }

    fn record_debug(&mut self, field: &Field, value: &dyn std::fmt::Debug) {
        self.record_value(field, format!("{value:?}"));
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

fn level_name(level: &Level) -> &'static str {
    match *level {
        Level::TRACE => "TRACE",
        Level::DEBUG => "DEBUG",
        Level::INFO => "INFO",
        Level::WARN => "WARNING",
        Level::ERROR => "ERROR",
    }
}

fn level_color(level: &Level) -> &'static str {
    match *level {
        Level::TRACE => COLOR_MAGENTA,
        Level::DEBUG => COLOR_BLUE,
        Level::INFO => COLOR_GREEN,
        Level::WARN => COLOR_DARK_ORANGE,
        Level::ERROR => COLOR_RED,
    }
}

fn component_name(target: &str) -> &str {
    let mut parts = target.split("::");
    match parts.next() {
        Some("haruki_event_tracker") => parts.next().unwrap_or("main"),
        Some("tower_http") => "http",
        Some(component) => component,
        None => "main",
    }
}

fn component_color(component: &str) -> &'static str {
    match component {
        "main" => COLOR_BRIGHT_CYAN,
        "api" | "http" | "router" | "handler" => COLOR_GREEN,
        "tracker" | "daemon" => COLOR_MAGENTA,
        "sekai_api" | "client" => COLOR_BRIGHT_BLUE,
        "db" | "storage" => COLOR_BLUE,
        "cache" | "state" => COLOR_CYAN,
        "config" | "model" => COLOR_BRIGHT_MAGENTA,
        "access" => COLOR_YELLOW,
        _ => COLOR_BRIGHT_CYAN,
    }
}

fn open_log_file(path: &Path) -> Result<std::fs::File, LoggerError> {
    if let Some(parent) = path.parent()
        && !parent.as_os_str().is_empty()
    {
        std::fs::create_dir_all(parent).map_err(|source| LoggerError::CreateDir {
            path: parent.display().to_string(),
            source,
        })?;
    }
    OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
        .map_err(|source| LoggerError::OpenFile {
            path: path.display().to_string(),
            source,
        })
}

/// Initialise the global tracing subscriber.
///
/// - stdout: every event with ANSI colours.
/// - `main_log_file` (optional): every event *except* `target = "access"`,
///   no ANSI. Empty path disables.
/// - `access_log_file` (optional): only `target = "access"` events, no ANSI.
///   Empty path disables; access events still go to stdout (and the main
///   file when no access path is configured) so dev runs don't lose them.
pub fn init<P: AsRef<Path>>(
    level: &str,
    main_log_file: Option<P>,
    access_log_file: Option<P>,
) -> Result<(), LoggerError> {
    let level_filter = parse_level(level);
    let env_filter = EnvFilter::builder()
        .with_default_directive(level_filter.into())
        .from_env_lossy();

    let stdout_layer = fmt::layer()
        .event_format(HarukiFormat { ansi: true })
        .with_writer(io::stdout);

    let access_path = access_log_file
        .as_ref()
        .map(|p| p.as_ref())
        .filter(|p| !p.as_os_str().is_empty());

    let access_layer = match access_path {
        Some(path) => {
            let f = open_log_file(path)?;
            Some(
                fmt::layer()
                    .event_format(HarukiFormat { ansi: false })
                    .with_writer(Mutex::new(f))
                    .with_filter(Targets::new().with_target(ACCESS_TARGET, LevelFilter::TRACE)),
            )
        }
        None => None,
    };

    let main_path = main_log_file
        .as_ref()
        .map(|p| p.as_ref())
        .filter(|p| !p.as_os_str().is_empty());

    let main_file_layer = match main_path {
        Some(path) => {
            let f = open_log_file(path)?;
            // When access goes to its own file we exclude it from the main
            // file; otherwise keep it in the main file so a single
            // `main_log_file` config still captures everything.
            let layer = fmt::layer()
                .event_format(HarukiFormat { ansi: false })
                .with_writer(Mutex::new(f));
            let layer = if access_path.is_some() {
                layer
                    .with_filter(
                        Targets::new()
                            .with_target(ACCESS_TARGET, LevelFilter::OFF)
                            .with_default(LevelFilter::TRACE),
                    )
                    .boxed()
            } else {
                layer.boxed()
            };
            Some(layer)
        }
        None => None,
    };

    tracing_subscriber::registry()
        .with(env_filter)
        .with(stdout_layer)
        .with(main_file_layer)
        .with(access_layer)
        .try_init()?;
    Ok(())
}
