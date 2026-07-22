//! Initializes process-wide file logging and records safe agent-event metadata.

use std::{io, path::Path};

#[cfg(test)]
use std::path::PathBuf;

use kuncode_agent::observer::{AgentEvent, AgentObserver, EventKind};
use tracing_appender::{
    non_blocking::{ErrorCounter, NonBlocking, NonBlockingBuilder, WorkerGuard},
    rolling::{RollingFileAppender, Rotation},
};
use tracing_subscriber::{EnvFilter, Layer, layer::SubscriberExt, util::SubscriberInitExt};

use crate::settings::{LoggingSettings, SettingsError, load_logging_settings};

const DEFAULT_LEVEL: &str = "info";
const PROJECT_TARGETS: [&str; 9] = [
    "kuncode::agent",
    "kuncode::compaction",
    "kuncode::hook",
    "kuncode::logging",
    "kuncode::permission",
    "kuncode::persistence",
    "kuncode::provider",
    "kuncode::runtime",
    "kuncode::tool",
];
const LOG_DIRECTORY: &str = ".kuncode/logs";
const LOG_FILE_PREFIX: &str = "kuncode";
const RETAINED_LOG_FILES: usize = 7;
const CONTENT_PREVIEW_CHARS: usize = 256;
const SENSITIVE_MARKERS: [&str; 10] = [
    "api_key",
    "apikey",
    "authorization",
    "bearer",
    "credential",
    "passwd",
    "password",
    "private_key",
    "secret",
    "token",
];
const SENSITIVE_TOKEN_PREFIXES: [&str; 8] = [
    "akia",
    "asia",
    "github_pat_",
    "ghp_",
    "glpat-",
    "sk-",
    "xoxb-",
    "xoxp-",
];

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct BootstrapDiagnostic {
    kind: &'static str,
    diagnostic_chars: usize,
}

/// Keeps the background writer alive and flushes queued records on drop.
pub(crate) struct LoggingGuard {
    _writer: Option<WorkerGuard>,
    dropped_lines: Option<ErrorCounter>,
}

impl Drop for LoggingGuard {
    fn drop(&mut self) {
        let dropped_lines = self
            .dropped_lines
            .as_ref()
            .map_or(0, ErrorCounter::dropped_lines);
        if dropped_lines == 0 {
            return;
        }

        tracing::warn!(
            target: "kuncode::logging",
            dropped_lines,
            "log records were dropped because the writer queue was full",
        );
        eprintln!("kuncode: logging dropped {dropped_lines} records because its queue was full");
    }
}

/// Initializes a daily rolling log under `~/.kuncode/logs`.
///
/// Invalid bootstrap configuration and file-open failures fall back to an
/// INFO-filtered stderr subscriber. Diagnostics are emitted without exposing
/// settings content, prompts, model output, or tool payloads.
pub(crate) fn init(project_root: Option<&Path>) -> LoggingGuard {
    let (settings, settings_diagnostic) = bootstrap_settings(project_root);
    let env_filter = std::env::var("RUST_LOG")
        .ok()
        .filter(|value| !value.trim().is_empty());
    let (filter, filter_diagnostic) = effective_filter(&settings, env_filter.as_deref());

    let Some(home) = std::env::home_dir() else {
        install_stderr(filter);
        eprintln!("kuncode: home directory unavailable; logging to stderr");
        report_bootstrap_diagnostics(settings_diagnostic, filter_diagnostic);
        return LoggingGuard {
            _writer: None,
            dropped_lines: None,
        };
    };

    let log_directory = home.join(LOG_DIRECTORY);
    match file_writer(&log_directory) {
        Ok((writer, guard, dropped_lines)) => {
            let layer = tracing_subscriber::fmt::layer()
                .with_ansi(false)
                .with_target(true)
                .with_thread_ids(true)
                .with_writer(writer)
                .with_filter(filter);
            if tracing_subscriber::registry()
                .with(layer)
                .try_init()
                .is_err()
            {
                eprintln!("kuncode: logging subscriber was already initialized");
            } else {
                tracing::info!(
                    target: "kuncode::logging",
                    directory = %log_directory.display(),
                    retained_files = RETAINED_LOG_FILES,
                    "file logging initialized",
                );
                log_diagnostic(settings_diagnostic);
                log_diagnostic(filter_diagnostic);
            }
            LoggingGuard {
                _writer: Some(guard),
                dropped_lines: Some(dropped_lines),
            }
        }
        Err(error) => {
            install_stderr(filter);
            eprintln!(
                "kuncode: failed to initialize file logging ({}); logging to stderr",
                error.kind()
            );
            report_bootstrap_diagnostics(settings_diagnostic, filter_diagnostic);
            LoggingGuard {
                _writer: None,
                dropped_lines: None,
            }
        }
    }
}

/// Adds a payload-safe panic record while preserving the existing panic hook.
///
/// Ratatui later wraps this hook with terminal restoration, so both the log and
/// terminal cleanup run for interactive panics.
pub(crate) fn install_panic_hook() {
    let previous = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        let location = info.location();
        let message = panic_payload(info.payload());
        tracing::error!(
            target: "kuncode::runtime",
            source_file = location.map_or("-", std::panic::Location::file),
            source_line = location.map_or(0, std::panic::Location::line),
            source_column = location.map_or(0, std::panic::Location::column),
            diagnostic_chars = message.map_or(0, |value| value.chars().count()),
            "process panicked",
        );
        if let Some(message) = message {
            tracing::debug!(
                target: "kuncode::runtime",
                preview = %redacted_preview(message),
                "panic diagnostic preview",
            );
        }
        previous(info);
    }));
}

fn panic_payload(payload: &(dyn std::any::Any + Send)) -> Option<&str> {
    payload
        .downcast_ref::<&str>()
        .copied()
        .or_else(|| payload.downcast_ref::<String>().map(String::as_str))
}

fn bootstrap_settings(
    project_root: Option<&Path>,
) -> (LoggingSettings, Option<BootstrapDiagnostic>) {
    let Some(root) = project_root else {
        return (
            default_settings(),
            Some(BootstrapDiagnostic {
                kind: "current_directory_unavailable",
                diagnostic_chars: 0,
            }),
        );
    };
    match load_logging_settings(root) {
        Ok(settings) => (settings, None),
        Err(error) => (
            default_settings(),
            Some(BootstrapDiagnostic {
                kind: settings_error_kind(&error),
                diagnostic_chars: error.to_string().chars().count(),
            }),
        ),
    }
}

fn settings_error_kind(error: &SettingsError) -> &'static str {
    match error {
        SettingsError::Read(_) => "settings_read",
        SettingsError::Parse(_) => "settings_parse",
        SettingsError::UserRead(_) => "provider_profiles_read",
        SettingsError::UserParse(_) => "provider_profiles_parse",
        SettingsError::Workspace(_) => "settings_workspace",
        SettingsError::Rule(_, _) => "settings_rule",
        SettingsError::Mode(_) => "settings_mode",
        SettingsError::Model(_) => "settings_model",
        SettingsError::Agent(_) => "settings_agent",
        SettingsError::Logging(_) => "settings_logging",
        SettingsError::CompactionMode(_) => "settings_compaction_mode",
        SettingsError::CompactionContextLimit => "settings_compaction_context_limit",
        SettingsError::Compaction(_) => "settings_compaction",
    }
}

fn default_settings() -> LoggingSettings {
    LoggingSettings {
        level: DEFAULT_LEVEL.to_string(),
    }
}

fn effective_filter(
    settings: &LoggingSettings,
    env_override: Option<&str>,
) -> (EnvFilter, Option<BootstrapDiagnostic>) {
    let source = env_override
        .filter(|value| !value.trim().is_empty())
        .map(str::trim)
        .map(str::to_string)
        .unwrap_or_else(|| project_filter(&settings.level));
    match EnvFilter::try_new(&source) {
        Ok(filter) => (filter, None),
        Err(_) => (
            EnvFilter::new(project_filter(DEFAULT_LEVEL)),
            Some(BootstrapDiagnostic {
                kind: if env_override.is_some() {
                    "invalid_rust_log"
                } else {
                    "invalid_logging_filter"
                },
                diagnostic_chars: source.chars().count(),
            }),
        ),
    }
}

fn project_filter(level: &str) -> String {
    let level = level.trim().to_ascii_lowercase();
    let project_targets = PROJECT_TARGETS
        .map(|target| format!("{target}={level}"))
        .join(",");
    format!("off,{project_targets}")
}

fn file_writer(directory: &Path) -> io::Result<(NonBlocking, WorkerGuard, ErrorCounter)> {
    std::fs::create_dir_all(directory)?;
    secure_log_directory(directory)?;
    let appender = RollingFileAppender::builder()
        .rotation(Rotation::DAILY)
        .filename_prefix(LOG_FILE_PREFIX)
        .max_log_files(RETAINED_LOG_FILES)
        .build(directory)
        .map_err(io::Error::other)?;
    secure_existing_log_files(directory)?;
    let (writer, guard) = NonBlockingBuilder::default().finish(appender);
    let dropped_lines = writer.error_counter();
    Ok((writer, guard, dropped_lines))
}

#[cfg(unix)]
fn secure_log_directory(directory: &Path) -> io::Result<()> {
    use std::{fs::Permissions, os::unix::fs::PermissionsExt};

    std::fs::set_permissions(directory, Permissions::from_mode(0o700))
}

#[cfg(unix)]
fn secure_existing_log_files(directory: &Path) -> io::Result<()> {
    use std::{fs::Permissions, os::unix::fs::PermissionsExt};

    for entry in std::fs::read_dir(directory)? {
        let entry = entry?;
        if entry.file_type()?.is_file()
            && entry
                .file_name()
                .to_string_lossy()
                .starts_with(&format!("{LOG_FILE_PREFIX}."))
        {
            std::fs::set_permissions(entry.path(), Permissions::from_mode(0o600))?;
        }
    }
    Ok(())
}

#[cfg(not(unix))]
fn secure_log_directory(_directory: &Path) -> io::Result<()> {
    Ok(())
}

#[cfg(not(unix))]
fn secure_existing_log_files(_directory: &Path) -> io::Result<()> {
    Ok(())
}

fn install_stderr(filter: EnvFilter) {
    let layer = tracing_subscriber::fmt::layer()
        .with_ansi(false)
        .with_target(true)
        .with_writer(io::stderr)
        .with_filter(filter);
    let _ = tracing_subscriber::registry().with(layer).try_init();
}

fn report_bootstrap_diagnostics(
    first: Option<BootstrapDiagnostic>,
    second: Option<BootstrapDiagnostic>,
) {
    for diagnostic in [first, second].into_iter().flatten() {
        eprintln!(
            "kuncode: logging bootstrap warning: kind={}, diagnostic_chars={}",
            diagnostic.kind, diagnostic.diagnostic_chars
        );
    }
}

fn log_diagnostic(diagnostic: Option<BootstrapDiagnostic>) {
    if let Some(diagnostic) = diagnostic {
        tracing::warn!(
            target: "kuncode::logging",
            diagnostic_kind = diagnostic.kind,
            diagnostic_chars = diagnostic.diagnostic_chars,
            "logging bootstrap used defaults",
        );
    }
}

/// Logs an opt-in, bounded preview of human input at DEBUG level.
pub(crate) fn log_prompt_preview(prompt: &str) {
    tracing::debug!(
        target: "kuncode::agent",
        prompt_chars = prompt.chars().count(),
        preview = %redacted_preview(prompt),
        "user prompt preview",
    );
}

fn redacted_preview(content: &str) -> String {
    let mut preview = String::new();
    let mut truncated = false;
    for (index, line) in content.lines().enumerate() {
        if index > 0 && !push_preview_char(&mut preview, ' ') {
            truncated = true;
            break;
        }
        let marker_probe = line
            .chars()
            .take(CONTENT_PREVIEW_CHARS)
            .collect::<String>()
            .to_ascii_lowercase();
        let safe_line = if line_is_sensitive(&marker_probe) {
            "[REDACTED]"
        } else {
            line
        };
        for character in safe_line.chars() {
            let character = if character.is_control() {
                ' '
            } else {
                character
            };
            if !push_preview_char(&mut preview, character) {
                truncated = true;
                break;
            }
        }
        if truncated {
            break;
        }
    }
    if truncated {
        preview.push('…');
    }
    preview
}

fn line_is_sensitive(probe: &str) -> bool {
    if SENSITIVE_MARKERS
        .iter()
        .any(|marker| probe.contains(marker))
        || (probe.contains("-----begin") && probe.contains("private key"))
    {
        return true;
    }

    probe
        .split(|character: char| {
            character.is_whitespace()
                || matches!(
                    character,
                    '"' | '\'' | '`' | ',' | ';' | ':' | '=' | '(' | ')' | '[' | ']' | '{' | '}'
                )
        })
        .map(|token| token.trim_matches(|character: char| matches!(character, '=' | '<' | '>')))
        .filter(|token| !token.is_empty())
        .any(looks_like_secret_token)
}

fn looks_like_secret_token(token: &str) -> bool {
    let token = token.to_ascii_lowercase();
    if SENSITIVE_TOKEN_PREFIXES
        .iter()
        .any(|prefix| token.starts_with(prefix))
    {
        return true;
    }

    token.len() >= 32
        && token.chars().all(|character| {
            character.is_ascii_alphanumeric() || matches!(character, '-' | '_' | '.' | '+' | '/')
        })
        && token
            .chars()
            .any(|character| character.is_ascii_alphabetic())
        && token.chars().any(|character| character.is_ascii_digit())
}

fn push_preview_char(preview: &mut String, character: char) -> bool {
    if preview.chars().count() >= CONTENT_PREVIEW_CHARS.saturating_sub(1) {
        return false;
    }
    preview.push(character);
    true
}

/// Converts the runner's event stream into bounded, payload-free log records.
pub(crate) struct LoggingObserver;

impl AgentObserver for LoggingObserver {
    fn on_event(&self, event: &AgentEvent) {
        let seq = event.seq;
        let iteration = event.iteration;
        match &event.kind {
            EventKind::ModelStart => tracing::debug!(
                target: "kuncode::agent",
                seq,
                iteration = ?iteration,
                "model request started",
            ),
            EventKind::TextDelta { .. } | EventKind::ReasoningDelta { .. } => {}
            EventKind::Assistant { text, tool_calls } => tracing::info!(
                target: "kuncode::agent",
                seq,
                iteration = ?iteration,
                text_chars = text.chars().count(),
                tool_calls = tool_calls.len(),
                final_answer = tool_calls.is_empty(),
                "model response completed",
            ),
            EventKind::ToolStart {
                tool_call_id,
                tool,
                summary: _,
            } => tracing::info!(
                target: "kuncode::tool",
                seq,
                iteration = ?iteration,
                tool_call_id = %tool_call_id,
                tool = %tool,
                "tool call started",
            ),
            EventKind::ToolEnd {
                tool_call_id,
                tool,
                ok,
                truncated,
                error,
            } => tracing::info!(
                target: "kuncode::tool",
                seq,
                iteration = ?iteration,
                tool_call_id = %tool_call_id,
                tool = %tool,
                ok,
                truncated,
                error_kind = error.as_ref().map_or("-", |failure| failure.kind.as_str()),
                "tool call completed",
            ),
            EventKind::Error { kind, message } => tracing::error!(
                target: "kuncode::agent",
                seq,
                iteration = ?iteration,
                error_kind = %kind,
                diagnostic_chars = message.chars().count(),
                "agent turn failed",
            ),
            EventKind::TodoUpdate { todos } => tracing::debug!(
                target: "kuncode::agent",
                seq,
                iteration = ?iteration,
                todo_count = todos.len(),
                "task plan updated",
            ),
            EventKind::Warning { message } => tracing::warn!(
                target: "kuncode::agent",
                seq,
                iteration = ?iteration,
                diagnostic_chars = message.chars().count(),
                "agent runtime degraded",
            ),
            EventKind::CompactionStarted {
                reason,
                before_tokens,
                precision,
            } => tracing::info!(
                target: "kuncode::compaction",
                seq,
                iteration = ?iteration,
                reason = %reason,
                before_tokens,
                precision = ?precision,
                "context compaction started",
            ),
            EventKind::CompactionCompleted {
                before_tokens,
                after_tokens,
                target_reached,
                passes,
                source_seq_start,
                source_seq_end,
                checkpoint_seq,
                artifact_count,
                summary_usage,
                summary_latency_ms,
                latency_ms,
            } => tracing::info!(
                target: "kuncode::compaction",
                seq,
                iteration = ?iteration,
                before_tokens,
                after_tokens,
                target_reached,
                pass_count = passes.len(),
                source_seq_start,
                source_seq_end,
                checkpoint_seq,
                artifact_count,
                summary_tokens = summary_usage.map_or(0, |usage| usage.total_tokens),
                summary_latency_ms = ?summary_latency_ms,
                latency_ms,
                "context compaction completed",
            ),
            EventKind::CompactionSkipped {
                reason,
                before_tokens,
                precision,
            } => tracing::debug!(
                target: "kuncode::compaction",
                seq,
                iteration = ?iteration,
                reason = %reason,
                before_tokens,
                precision = ?precision,
                "context compaction skipped",
            ),
            EventKind::CompactionObserved {
                before_tokens,
                projected_after_tokens,
                safe_prefix_groups,
                artifact_shape_candidates,
                requires_summary,
                precision,
            } => tracing::debug!(
                target: "kuncode::compaction",
                seq,
                iteration = ?iteration,
                before_tokens,
                projected_after_tokens,
                safe_prefix_groups,
                artifact_shape_candidates,
                requires_summary,
                precision = ?precision,
                "context compaction shadow observation completed",
            ),
            EventKind::CompactionFailed {
                stage,
                error,
                recoverable,
                before_tokens,
                summary_usage,
                latency_ms,
            } => tracing::warn!(
                target: "kuncode::compaction",
                seq,
                iteration = ?iteration,
                stage = %stage,
                diagnostic_chars = error.chars().count(),
                recoverable,
                before_tokens,
                summary_tokens = summary_usage.map_or(0, |usage| usage.total_tokens),
                latency_ms,
                "context compaction failed",
            ),
        }

        match &event.kind {
            EventKind::Assistant { text, .. } => tracing::debug!(
                target: "kuncode::agent",
                seq,
                iteration = ?iteration,
                preview = %redacted_preview(text),
                "model response preview",
            ),
            EventKind::ToolStart { summary, .. } => tracing::debug!(
                target: "kuncode::tool",
                seq,
                iteration = ?iteration,
                preview = %redacted_preview(summary),
                "tool call summary preview",
            ),
            EventKind::ToolEnd {
                error: Some(error), ..
            } => tracing::debug!(
                target: "kuncode::tool",
                seq,
                iteration = ?iteration,
                preview = %redacted_preview(&error.message),
                "tool failure preview",
            ),
            EventKind::Error { message, .. } | EventKind::Warning { message } => tracing::debug!(
                target: "kuncode::agent",
                seq,
                iteration = ?iteration,
                preview = %redacted_preview(message),
                "runtime diagnostic preview",
            ),
            EventKind::CompactionFailed { error, .. } => tracing::debug!(
                target: "kuncode::compaction",
                seq,
                iteration = ?iteration,
                preview = %redacted_preview(error),
                "compaction failure preview",
            ),
            _ => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use std::{io::Write, time::Duration};

    use super::*;

    #[test]
    fn settings_level_builds_filter() {
        let settings = LoggingSettings {
            level: "debug".to_string(),
        };

        let (filter, diagnostic) = effective_filter(&settings, None);

        assert!(diagnostic.is_none());
        let rendered = filter.to_string();
        assert!(rendered.contains("kuncode::agent=debug"));
        assert!(rendered.contains("kuncode::runtime=debug"));

        let layer = tracing_subscriber::fmt::layer()
            .with_writer(io::sink)
            .with_filter(filter);
        let subscriber = tracing_subscriber::registry().with(layer);
        tracing::subscriber::with_default(subscriber, || {
            assert!(tracing::enabled!(target: "kuncode::runtime", tracing::Level::DEBUG));
            assert!(!tracing::enabled!(target: "reqwest", tracing::Level::DEBUG));
            assert!(!tracing::enabled!(target: "reqwest", tracing::Level::ERROR));
        });
    }

    #[test]
    fn padded_off_level_remains_disabled() {
        let settings = LoggingSettings {
            level: " off ".to_string(),
        };

        let (filter, diagnostic) = effective_filter(&settings, None);

        assert!(diagnostic.is_none());
        let layer = tracing_subscriber::fmt::layer()
            .with_writer(io::sink)
            .with_filter(filter);
        let subscriber = tracing_subscriber::registry().with(layer);
        tracing::subscriber::with_default(subscriber, || {
            assert!(!tracing::enabled!(target: "kuncode::runtime", tracing::Level::ERROR));
        });
    }

    #[test]
    fn bootstrap_configuration_error_is_reduced_to_safe_metadata() {
        let directory = std::env::temp_dir().join(format!(
            "kuncode-logging-sensitive-bootstrap-{}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&directory);
        std::fs::create_dir_all(directory.join(".kuncode")).expect("fixture directory");
        std::fs::write(
            directory.join(".kuncode/settings.json"),
            r#"{ "logging": { "level": "GITHUB_PAT=ghp_Example123456789012345678901234" } }"#,
        )
        .expect("fixture settings");

        let (settings, diagnostic) = bootstrap_settings(Some(&directory));

        let _ = std::fs::remove_dir_all(&directory);
        assert_eq!(settings.level, DEFAULT_LEVEL);
        assert_eq!(diagnostic.map(|value| value.kind), Some("settings_logging"));
        assert!(diagnostic.is_some_and(|value| value.diagnostic_chars > 0));
    }

    #[test]
    fn log_directory_is_under_home() {
        let home = PathBuf::from("/tmp/example-home");

        assert_eq!(home.join(LOG_DIRECTORY), home.join(".kuncode/logs"));
    }

    #[test]
    fn rolling_writer_creates_a_log_file() {
        let directory =
            std::env::temp_dir().join(format!("kuncode-logging-writer-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&directory);
        let (mut writer, guard, dropped_lines) =
            file_writer(&directory).expect("writer initializes");

        writer.write_all(b"log probe\n").expect("probe enqueues");
        drop(writer);
        drop(guard);
        assert_eq!(dropped_lines.dropped_lines(), 0);

        let files = std::fs::read_dir(&directory)
            .expect("log directory exists")
            .filter_map(Result::ok)
            .collect::<Vec<_>>();
        let _ = std::fs::remove_dir_all(&directory);
        assert!(
            files
                .iter()
                .any(|entry| entry.file_name().to_string_lossy().starts_with("kuncode."))
        );
    }

    #[test]
    fn writer_initialization_fails_when_log_directory_is_a_file() {
        let path =
            std::env::temp_dir().join(format!("kuncode-logging-file-{}", std::process::id()));
        let _ = std::fs::remove_file(&path);
        std::fs::write(&path, b"not a directory").expect("fixture file");

        let error = file_writer(&path).expect_err("a file cannot serve as the log directory");

        let _ = std::fs::remove_file(&path);
        assert!(matches!(
            error.kind(),
            io::ErrorKind::AlreadyExists | io::ErrorKind::NotADirectory
        ));
    }

    #[test]
    fn rolling_writer_prunes_files_to_the_retention_limit() {
        let directory =
            std::env::temp_dir().join(format!("kuncode-logging-retention-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&directory);
        std::fs::create_dir_all(&directory).expect("fixture directory");
        for day in 1..=RETAINED_LOG_FILES + 3 {
            std::fs::write(
                directory.join(format!("{LOG_FILE_PREFIX}.2000-01-{day:02}")),
                b"old log",
            )
            .expect("fixture log");
        }

        let (writer, guard, _) = file_writer(&directory).expect("writer initializes");
        drop(writer);
        drop(guard);
        let retained = std::fs::read_dir(&directory)
            .expect("log directory")
            .filter_map(Result::ok)
            .filter(|entry| {
                entry
                    .file_name()
                    .to_string_lossy()
                    .starts_with(&format!("{LOG_FILE_PREFIX}."))
            })
            .count();

        let _ = std::fs::remove_dir_all(&directory);
        assert_eq!(retained, RETAINED_LOG_FILES);
    }

    #[test]
    fn lossy_writer_counts_records_dropped_under_backpressure() {
        struct SlowWriter;

        impl Write for SlowWriter {
            fn write(&mut self, buffer: &[u8]) -> io::Result<usize> {
                std::thread::sleep(Duration::from_millis(25));
                Ok(buffer.len())
            }

            fn flush(&mut self) -> io::Result<()> {
                Ok(())
            }
        }

        let (mut writer, guard) = NonBlockingBuilder::default()
            .buffered_lines_limit(1)
            .finish(SlowWriter);
        let dropped_lines = writer.error_counter();
        for _ in 0..100 {
            writer.write_all(b"probe\n").expect("enqueue attempt");
        }
        drop(writer);
        drop(guard);

        assert!(dropped_lines.dropped_lines() > 0);
    }

    #[test]
    fn content_preview_redacts_sensitive_lines_and_flattens_newlines() {
        let preview = redacted_preview("safe line\nAPI_KEY=do-not-log\nlast line");

        assert_eq!(preview, "safe line [REDACTED] last line");
        assert!(!preview.contains("do-not-log"));
        assert!(!preview.contains('\n'));
    }

    #[test]
    fn content_preview_is_bounded_on_utf8_boundaries() {
        let preview = redacted_preview(&"你".repeat(CONTENT_PREVIEW_CHARS + 20));

        assert_eq!(preview.chars().count(), CONTENT_PREVIEW_CHARS);
        assert!(preview.ends_with('…'));
    }

    #[test]
    fn content_preview_redacts_common_secret_shapes() {
        let preview = redacted_preview(
            "safe\nsk-example1234567890\nghp_Example123456789012345678901234\n-----BEGIN PRIVATE KEY-----\nabc1234567890def4567890ghi4567890",
        );

        assert_eq!(preview, "safe [REDACTED] [REDACTED] [REDACTED] [REDACTED]");
        assert!(!preview.contains("Example"));
    }

    #[test]
    fn content_preview_redacts_assignment_credentials() {
        let preview = redacted_preview(
            "safe\nGITHUB_PAT=ghp_Example123456789012345678901234\nAWS_ACCESS_KEY_ID=AKIAEXAMPLE1234567890",
        );

        assert_eq!(preview, "safe [REDACTED] [REDACTED]");
        assert!(!preview.contains("ghp_"));
        assert!(!preview.contains("AKIA"));
    }

    #[test]
    fn panic_payload_supports_standard_message_types() {
        let borrowed: &(dyn std::any::Any + Send) = &"borrowed panic";
        let owned: &(dyn std::any::Any + Send) = &"owned panic".to_string();
        let opaque: &(dyn std::any::Any + Send) = &42_u32;

        assert_eq!(panic_payload(borrowed), Some("borrowed panic"));
        assert_eq!(panic_payload(owned), Some("owned panic"));
        assert_eq!(panic_payload(opaque), None);
    }
}
