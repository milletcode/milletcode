use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use tracing_appender::non_blocking;
use tracing_subscriber::{
    fmt,
    layer::SubscriberExt,
    util::SubscriberInitExt,
    EnvFilter,
};

use crate::config::{LogConfig, LogLevel, LogOutput};

/// Global flag controlling session conversation logging.
/// Set once at startup from config, read cheaply via atomic at each log call.
static SESSION_LOG_ENABLED: AtomicBool = AtomicBool::new(true);

/// Returns whether detailed session conversation logging is enabled.
pub fn is_session_log_enabled() -> bool {
    SESSION_LOG_ENABLED.load(Ordering::Relaxed)
}

/// Holds WorkerGuard references — must live for the process lifetime
/// to ensure non-blocking writers flush buffered logs on drop.
pub struct LoggingGuard {
    _stdout_guard: Option<non_blocking::WorkerGuard>,
    _file_guard: Option<non_blocking::WorkerGuard>,
}

/// Initialize tracing subscriber based on log config settings.
/// Returns a guard that must be stored in main() and held until exit.
pub fn init(logs: &LogConfig) -> LoggingGuard {
    SESSION_LOG_ENABLED.store(logs.session_log_enabled, Ordering::SeqCst);

    let log_dir = logs
        .session_log_dir
        .as_ref()
        .filter(|s| !s.is_empty())
        .map(PathBuf::from)
        .unwrap_or_else(|| {
            dirs::home_dir()
                .unwrap_or_else(|| PathBuf::from("."))
                .join(".milletcode")
                .join("logs")
        });

    let env_filter = build_env_filter(&logs.level);

    let mut stdout_guard = None;
    let mut file_guard = None;

    let stdout_layer: Option<_> = match logs.output {
        LogOutput::File => None,
        LogOutput::Stdout | LogOutput::Both => {
            let (writer, guard) = non_blocking(std::io::stdout());
            stdout_guard = Some(guard);
            Some(
                fmt::layer()
                    .with_writer(writer)
                    .with_target(false)
                    .with_timer(fmt::time::time())
                    .compact(),
            )
        }
    };

    let file_layer: Option<_> = match logs.output {
        LogOutput::Stdout => None,
        LogOutput::File | LogOutput::Both => {
            std::fs::create_dir_all(&log_dir).ok();

            if logs.max_files > 0 {
                prune_old_logs(&log_dir, logs.max_files);
            }

            let file_appender = tracing_appender::rolling::daily(&log_dir, "agent-console");
            let (writer, guard) = non_blocking(file_appender);
            file_guard = Some(guard);

            Some(
                fmt::layer()
                    .with_writer(writer)
                    .with_target(true)
                    .with_timer(fmt::time::time())
                    .with_ansi(false),
            )
        }
    };

    // Install panic hook so panics are captured in logs before printing to stderr
    std::panic::set_hook(Box::new(|info| {
        let backtrace = std::backtrace::Backtrace::force_capture();
        if let Some(loc) = info.location() {
            tracing::error!(
                panic.file = %loc.file(),
                panic.line = loc.line(),
                panic.column = loc.column(),
                panic.message = %info.to_string(),
                "PANIC\n{}",
                backtrace
            );
        } else {
            tracing::error!(
                panic.message = %info.to_string(),
                "PANIC\n{}",
                backtrace
            );
        }
    }));

    tracing_subscriber::registry()
        .with(env_filter)
        .with(stdout_layer)
        .with(file_layer)
        .init();

    LoggingGuard {
        _stdout_guard: stdout_guard,
        _file_guard: file_guard,
    }
}

fn build_env_filter(level: &LogLevel) -> EnvFilter {
    EnvFilter::try_from_default_env().unwrap_or_else(|_| {
        EnvFilter::new(format!("agent_console={}", level))
    })
}

/// Delete log files beyond the retention count, oldest first.
/// rolling::daily names files as "agent-console.YYYY-MM-DD".
fn prune_old_logs(log_dir: &PathBuf, max_files: usize) {
    let mut entries: Vec<_> = std::fs::read_dir(log_dir)
        .ok()
        .into_iter()
        .flatten()
        .filter_map(|e| e.ok())
        .filter(|e| {
            e.file_name()
                .to_string_lossy()
                .starts_with("agent-console.")
        })
        .filter_map(|e| {
            let metadata = e.metadata().ok()?;
            let modified = metadata.modified().ok()?;
            Some((e.path(), modified))
        })
        .collect();

    entries.sort_by_key(|(_, m)| *m);

    let to_delete = entries.len().saturating_sub(max_files);
    for (path, _) in entries.iter().take(to_delete) {
        let _ = std::fs::remove_file(path);
    }
}
