//! `ToolError`: the in-memory error returned by `Tool::execute` and
//! `ToolRuntime::execute`. Its discriminant maps 1:1 to `ToolErrorKind`, which
//! is what lands in `tool.failed` event payloads. See Phase 2 plan §9.5.
//!
//! Each variant must carry enough diagnostic information that a human can
//! read it and a machine can match the kind.

use crate::result::SUMMARY_MAX_CHARS;
use kuncode_core::ToolErrorKind;
use std::path::{Path, PathBuf};
use thiserror::Error;

const ELLIPSIS: &str = "…"; // 3 bytes UTF-8
const ELLIPSIS_LEN: usize = ELLIPSIS.len();

/// Tail-truncate `s` to at most `max` bytes, appending `…` when it had to cut.
/// Always splits on a UTF-8 char boundary. `max` must be ≥ `ELLIPSIS_LEN`.
fn cap(s: &str, max: usize) -> String {
    debug_assert!(max >= ELLIPSIS_LEN, "max budget too small for ellipsis");
    if s.len() <= max {
        return s.to_owned();
    }
    let mut end = max.saturating_sub(ELLIPSIS_LEN);
    while end > 0 && !s.is_char_boundary(end) {
        end -= 1;
    }
    let mut out = String::with_capacity(max);
    out.push_str(&s[..end]);
    out.push_str(ELLIPSIS);
    out
}

/// Middle-truncate a path so the filename always survives:
/// `"a/b/c/very/long/path/file.rs"` with `max=20` → `"a/b/c…/file.rs"`.
/// Falls back to plain tail truncation when the filename alone overruns budget.
fn cap_path(p: &Path, max: usize) -> String {
    let s = p.display().to_string();
    if s.len() <= max {
        return s;
    }
    let name = p.file_name().and_then(|n| n.to_str()).unwrap_or("");
    let suffix = format!("{ELLIPSIS}/{name}");
    if name.is_empty() || suffix.len() >= max {
        return cap(&s, max);
    }
    let head_max = max - suffix.len();
    let mut end = head_max;
    while end > 0 && !s.is_char_boundary(end) {
        end -= 1;
    }
    let mut out = String::with_capacity(max);
    out.push_str(&s[..end]);
    out.push_str(&suffix);
    out
}

#[derive(Debug, Error)]
pub enum ToolError {
    #[error("unknown tool `{name}`")]
    UnknownTool { name: String },

    #[error("invalid input for tool `{tool}`: {message}")]
    InvalidInput { tool: String, message: String },

    #[error("capability denied for tool `{tool}`: requires one of {required:?}, granted {granted:?}")]
    CapabilityDenied { tool: String, required: Vec<String>, granted: Vec<String> },

    #[error("workspace error on `{path}`: {message}")]
    Workspace { path: PathBuf, message: String },

    #[error("io error on `{path}`: {source}")]
    Io {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },

    #[error("process error: {message}")]
    Process { message: String },

    #[error("tool `{tool}` timed out after {elapsed_ms} ms")]
    Timeout { tool: String, elapsed_ms: u64 },

    #[error("tool `{tool}` cancelled")]
    Cancelled { tool: String },

    #[error("artifact error: {message}")]
    Artifact { message: String },

    #[error("tool `{tool}` result too large: {message}")]
    ResultTooLarge { tool: String, message: String },

    #[error("internal error in tool `{tool}`: {message}")]
    Internal { tool: String, message: String },
}

impl ToolError {
    /// Wire-stable classification suitable for `tool.failed.error_kind`.
    pub fn kind(&self) -> ToolErrorKind {
        match self {
            Self::UnknownTool { name: _ } => ToolErrorKind::UnknownTool,
            Self::InvalidInput { tool: _, message: _ } => ToolErrorKind::InvalidInput,
            Self::CapabilityDenied { tool: _, required: _, granted: _ } => ToolErrorKind::CapabilityDenied,
            Self::Workspace { path: _, message: _ } => ToolErrorKind::Workspace,
            Self::Io { path: _, source: _ } => ToolErrorKind::Io,
            Self::Process { message: _ } => ToolErrorKind::Process,
            Self::Timeout { tool: _, elapsed_ms: _ } => ToolErrorKind::Timeout,
            Self::Cancelled { tool: _ } => ToolErrorKind::Cancelled,
            Self::Artifact { message: _ } => ToolErrorKind::Artifact,
            Self::ResultTooLarge { tool: _, message: _ } => ToolErrorKind::ResultTooLarge,
            Self::Internal { tool: _, message: _ } => ToolErrorKind::Internal,
        }
    }

    /// Short, human-readable summary suitable for `tool.failed.summary`.
    ///
    /// Strategy per plan §6.1:
    ///
    /// 1. Each variant has its own renderer with **per-field byte budgets**
    ///    chosen so the natural rendering stays within `SUMMARY_MAX_CHARS`.
    /// 2. Field budgets prioritize the diagnostic carrier (`message` /
    ///    `source`) over decorative fields (`tool` name, `path` prefix).
    /// 3. Paths use `cap_path` to keep the file basename — losing it would
    ///    drop the most actionable bit of information.
    /// 4. The final `cap` is a backstop for any miscalculation; in practice
    ///    the per-variant budgets keep us under the cap on their own.
    pub fn summary(&self) -> String {
        let raw = match self {
            Self::UnknownTool { name } => {
                format!("unknown tool `{}`", cap(name, 150))
            }
            Self::InvalidInput { tool, message } => {
                format!("invalid input `{}`: {}", cap(tool, 40), cap(message, 130))
            }
            Self::CapabilityDenied { tool, required, granted } => {
                let req = required.join(", ");
                let grant = granted.join(", ");
                format!(
                    "capability denied `{}` (required [{}], granted [{}])",
                    cap(tool, 30),
                    cap(&req, 55),
                    cap(&grant, 55),
                )
            }
            Self::Workspace { path, message } => {
                format!("workspace `{}`: {}", cap_path(path, 70), cap(message, 100))
            }
            Self::Io { path, source } => {
                format!("io `{}`: {}", cap_path(path, 70), cap(&source.to_string(), 100))
            }
            Self::Process { message } => {
                format!("process: {}", cap(message, 170))
            }
            Self::Timeout { tool, elapsed_ms } => {
                format!("`{}` timed out after {elapsed_ms} ms", cap(tool, 80))
            }
            Self::Cancelled { tool } => {
                format!("`{}` cancelled", cap(tool, 150))
            }
            Self::Artifact { message } => {
                format!("artifact: {}", cap(message, 170))
            }
            Self::ResultTooLarge { tool, message } => {
                format!("`{}` result too large: {}", cap(tool, 40), cap(message, 130))
            }
            Self::Internal { tool, message } => {
                format!("internal `{}`: {}", cap(tool, 40), cap(message, 140))
            }
        };
        if raw.len() <= SUMMARY_MAX_CHARS { raw } else { cap(&raw, SUMMARY_MAX_CHARS) }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn dummy_io() -> std::io::Error {
        std::io::Error::new(std::io::ErrorKind::NotFound, "missing")
    }

    #[test]
    fn kind_maps_every_variant_to_tool_error_kind() {
        let cases: [(ToolError, ToolErrorKind); 11] = [
            (ToolError::UnknownTool { name: "x".into() }, ToolErrorKind::UnknownTool),
            (ToolError::InvalidInput { tool: "x".into(), message: "m".into() }, ToolErrorKind::InvalidInput),
            (
                ToolError::CapabilityDenied { tool: "x".into(), required: vec![], granted: vec![] },
                ToolErrorKind::CapabilityDenied,
            ),
            (ToolError::Workspace { path: PathBuf::from("p"), message: "m".into() }, ToolErrorKind::Workspace),
            (ToolError::Io { path: PathBuf::from("p"), source: dummy_io() }, ToolErrorKind::Io),
            (ToolError::Process { message: "m".into() }, ToolErrorKind::Process),
            (ToolError::Timeout { tool: "x".into(), elapsed_ms: 1 }, ToolErrorKind::Timeout),
            (ToolError::Cancelled { tool: "x".into() }, ToolErrorKind::Cancelled),
            (ToolError::Artifact { message: "m".into() }, ToolErrorKind::Artifact),
            (ToolError::ResultTooLarge { tool: "x".into(), message: "m".into() }, ToolErrorKind::ResultTooLarge),
            (ToolError::Internal { tool: "x".into(), message: "m".into() }, ToolErrorKind::Internal),
        ];
        for (err, expected) in cases {
            assert_eq!(err.kind(), expected, "{err:?} should map to {expected:?}");
        }
    }

    #[test]
    fn summary_is_non_empty_and_under_200_chars() {
        let err = ToolError::Workspace { path: PathBuf::from("src/lib.rs"), message: "path escapes".into() };
        let summary = err.summary();
        assert!(!summary.is_empty());
        assert!(summary.len() <= 200, "summary must stay within 200 chars: {summary}");
    }

    #[test]
    fn summary_fits_under_budget_for_every_variant_with_pathological_inputs() {
        let huge = "x".repeat(10_000);
        let long_path = PathBuf::from(format!("{}/important.rs", "deep/".repeat(200)));
        let cases: Vec<ToolError> = vec![
            ToolError::UnknownTool { name: huge.clone() },
            ToolError::InvalidInput { tool: huge.clone(), message: huge.clone() },
            ToolError::CapabilityDenied {
                tool: huge.clone(),
                required: vec![huge.clone(), huge.clone()],
                granted: vec![huge.clone()],
            },
            ToolError::Workspace { path: long_path.clone(), message: huge.clone() },
            ToolError::Io { path: long_path.clone(), source: std::io::Error::other(huge.clone()) },
            ToolError::Process { message: huge.clone() },
            ToolError::Timeout { tool: huge.clone(), elapsed_ms: 1 },
            ToolError::Cancelled { tool: huge.clone() },
            ToolError::Artifact { message: huge.clone() },
            ToolError::ResultTooLarge { tool: huge.clone(), message: huge.clone() },
            ToolError::Internal { tool: huge.clone(), message: huge.clone() },
        ];
        for err in cases {
            let kind = err.kind();
            let s = err.summary();
            assert!(!s.is_empty(), "{kind:?} produced empty summary");
            assert!(s.len() <= 200, "{kind:?} produced {} bytes: {s}", s.len());
        }
    }

    #[test]
    fn summary_preserves_utf8_when_truncating_multibyte_content() {
        let cn = "工作区路径越界".repeat(100); // 21 bytes × 100 = 2100 bytes
        let err = ToolError::Workspace { path: PathBuf::from("a"), message: cn };
        let s = err.summary();
        assert!(s.len() <= 200);
        // chars() walks the string; would panic on a mid-codepoint cut.
        let _: Vec<char> = s.chars().collect();
    }

    #[test]
    fn summary_keeps_filename_when_path_overruns_budget() {
        let long = format!("{}important.rs", "x/".repeat(200));
        let err = ToolError::Workspace { path: PathBuf::from(long), message: "not found".into() };
        let s = err.summary();
        assert!(s.contains("important.rs"), "filename should survive: {s}");
        assert!(s.contains("not found"), "message should survive: {s}");
    }

    #[test]
    fn cap_path_falls_back_when_filename_alone_overruns_budget() {
        let huge_name = "x".repeat(500);
        let err = ToolError::Workspace { path: PathBuf::from(&huge_name), message: "m".into() };
        let s = err.summary();
        assert!(s.len() <= 200);
        assert!(s.contains('…'));
    }
}
