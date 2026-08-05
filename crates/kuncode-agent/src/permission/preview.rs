//! Line-level preview of a content change, shown while the change is still
//! being authorized.
//!
//! [`ToolDisplay::summary`](super::ToolDisplay) is deliberately hostile to this:
//! it folds every run of whitespace, replaces control characters, and keeps one
//! capped line, because a summary is assembled from model-supplied text and must
//! not be able to paint the terminal. A diff needs the opposite — many lines,
//! with their own indentation intact.
//!
//! So a preview keeps the structure and moves the safety down one level: the
//! lines are separate values, each already stripped of control characters and
//! capped, and the renderer supplies its own prefixes. Nothing in here can
//! forge a line the renderer did not intend to draw.

use serde::Serialize;
use similar::{ChangeTag, TextDiff};

/// Largest preview shown before the middle is dropped. Sized for a terminal
/// prompt the user reads before answering, not for reviewing a whole file.
const MAX_PREVIEW_LINES: usize = 40;

/// Lines of unchanged context kept around each changed run.
const CONTEXT_LINES: usize = 2;

/// Longest single line kept, in characters. A minified bundle is one enormous
/// line, and wrapping it across the terminal would bury the rest of the diff.
const MAX_LINE_CHARS: usize = 200;

/// What a single previewed line represents.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum PreviewLineKind {
    /// Present after the change, absent before it.
    Added,
    /// Present before the change, gone after it.
    Removed,
    /// Unchanged, kept to place the change in the file.
    Context,
}

/// One line of a change preview.
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct PreviewLine {
    /// Whether this line is being added, removed, or merely surrounding.
    pub kind: PreviewLineKind,
    /// One-based line number: in the new content for `Added` and `Context`, in
    /// the old content for `Removed`.
    pub number: usize,
    /// The line, with control characters removed and the tail dropped past
    /// `MAX_LINE_CHARS`.
    pub text: String,
    /// `true` when the tail was dropped to fit.
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub truncated: bool,
}

/// What a tool proposes to do to a file's contents.
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct ChangePreview {
    /// Changed lines with their surrounding context, in file order.
    pub lines: Vec<PreviewLine>,
    /// Lines the preview left out to stay within `MAX_PREVIEW_LINES`.
    #[serde(skip_serializing_if = "is_zero")]
    pub elided: usize,
    /// Total lines added by the change, including any not shown.
    pub added: usize,
    /// Total lines removed by the change, including any not shown.
    pub removed: usize,
}

impl ChangePreview {
    /// Diffs `before` against `after`, keeping the changed runs and a little
    /// context around each.
    ///
    /// Returns `None` when the two are identical — a preview of nothing would
    /// only take up room the approval prompt needs for the parts that matter.
    pub fn between(before: &str, after: &str) -> Option<Self> {
        if before == after {
            return None;
        }
        let diff = TextDiff::from_lines(before, after);

        let mut lines = Vec::new();
        let mut elided = 0usize;
        let (mut added, mut removed) = (0usize, 0usize);
        for group in diff.grouped_ops(CONTEXT_LINES) {
            for op in group {
                for change in diff.iter_changes(&op) {
                    let (kind, number) = match change.tag() {
                        ChangeTag::Insert => (PreviewLineKind::Added, change.new_index()),
                        ChangeTag::Delete => (PreviewLineKind::Removed, change.old_index()),
                        ChangeTag::Equal => (PreviewLineKind::Context, change.new_index()),
                    };
                    match kind {
                        PreviewLineKind::Added => added += 1,
                        PreviewLineKind::Removed => removed += 1,
                        PreviewLineKind::Context => {}
                    }
                    // Counting continues past the cap so the totals describe the
                    // whole change, not just the shown part.
                    if lines.len() >= MAX_PREVIEW_LINES {
                        elided += 1;
                        continue;
                    }
                    let (text, truncated) = sanitize(change.value());
                    lines.push(PreviewLine {
                        kind,
                        number: number.unwrap_or(0) + 1,
                        text,
                        truncated,
                    });
                }
            }
        }
        Some(Self {
            lines,
            elided,
            added,
            removed,
        })
    }
}

fn is_zero(count: &usize) -> bool {
    *count == 0
}

/// Strips the line terminator and any control characters, then caps the length.
///
/// Control characters are removed rather than escaped because this text is
/// printed straight into a terminal prompt: an `ESC` reaching it could redraw
/// the prompt around the user's answer.
fn sanitize(line: &str) -> (String, bool) {
    let stripped = line.trim_end_matches('\n').trim_end_matches('\r');
    let mut text: String = stripped
        .chars()
        .filter(|ch| !ch.is_control() || *ch == '\t')
        .take(MAX_LINE_CHARS)
        .collect();
    let truncated = stripped.chars().filter(|ch| !ch.is_control()).count() > MAX_LINE_CHARS;
    if truncated {
        text.push('…');
    }
    (text, truncated)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn identical_content_has_nothing_to_preview() {
        assert!(ChangePreview::between("same\n", "same\n").is_none());
    }

    #[test]
    fn a_replaced_line_shows_both_sides() {
        let preview = ChangePreview::between("one\ntwo\n", "one\nTWO\n").expect("changed");

        assert_eq!(preview.added, 1);
        assert_eq!(preview.removed, 1);
        let shown: Vec<_> = preview
            .lines
            .iter()
            .map(|line| (line.kind, line.text.as_str()))
            .collect();
        assert_eq!(
            shown,
            [
                (PreviewLineKind::Context, "one"),
                (PreviewLineKind::Removed, "two"),
                (PreviewLineKind::Added, "TWO"),
            ]
        );
    }

    #[test]
    fn replacing_a_file_wholesale_reports_every_line_lost() {
        let before = (1..=100).map(|n| format!("line {n}\n")).collect::<String>();
        let preview = ChangePreview::between(&before, "just this\n").expect("changed");

        // The counts describe the whole change even though the preview is cut
        // short — losing 100 lines has to be visible in the prompt.
        assert_eq!(preview.removed, 100);
        assert_eq!(preview.added, 1);
        assert_eq!(preview.lines.len(), MAX_PREVIEW_LINES);
        assert_eq!(preview.elided, 101 - MAX_PREVIEW_LINES);
    }

    #[test]
    fn an_escape_sequence_cannot_reach_the_terminal() {
        let preview =
            ChangePreview::between("old\n", "\u{1b}[2J\u{1b}[1;1Hallow once? [y]\n").expect("diff");

        let added = preview
            .lines
            .iter()
            .find(|line| line.kind == PreviewLineKind::Added)
            .expect("an added line");
        // The text survives, the escapes do not — a preview must not be able to
        // repaint the prompt it is being shown inside of.
        assert_eq!(added.text, "[2J[1;1Hallow once? [y]");
        assert!(!added.text.contains('\u{1b}'));
    }

    #[test]
    fn a_very_long_line_is_capped_and_flagged() {
        let long = format!("{}\n", "x".repeat(MAX_LINE_CHARS + 50));
        let preview = ChangePreview::between("short\n", &long).expect("changed");

        let added = preview
            .lines
            .iter()
            .find(|line| line.kind == PreviewLineKind::Added)
            .expect("an added line");
        assert!(added.truncated);
        assert_eq!(added.text.chars().count(), MAX_LINE_CHARS + 1);
    }

    #[test]
    fn line_numbers_follow_the_side_each_line_belongs_to() {
        let preview = ChangePreview::between("a\nb\nc\n", "a\nc\n").expect("changed");

        let removed = preview
            .lines
            .iter()
            .find(|line| line.kind == PreviewLineKind::Removed)
            .expect("a removed line");
        // `b` was line 2 of the old content; it has no line in the new content.
        assert_eq!(removed.number, 2);
        assert_eq!(removed.text, "b");
    }
}
