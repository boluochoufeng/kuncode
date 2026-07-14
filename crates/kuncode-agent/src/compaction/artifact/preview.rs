//! Produces deterministic, UTF-8-safe previews for stored artifact payloads.
//!
//! Previews prefer complete leading and trailing lines, but the byte ceiling is
//! authoritative and includes the omission marker itself.

const CANONICAL_ARTIFACT_PREVIEW_BYTES: usize = 4_096;
const MIN_OMISSION_PREVIEW_BYTES: usize = 32;

pub(super) fn canonical_artifact_preview(payload: &str) -> String {
    adaptive_preview(payload, CANONICAL_ARTIFACT_PREVIEW_BYTES)
}

/// Returns a head-tail preview that never exceeds `max_bytes` or splits UTF-8.
///
/// Very small budgets yield an empty string because a useful omission marker
/// cannot fit. Line boundaries are preferred only when they preserve the hard
/// byte bound.
pub(crate) fn adaptive_preview(value: &str, max_bytes: usize) -> String {
    if max_bytes < MIN_OMISSION_PREVIEW_BYTES {
        return String::new();
    }
    if value.len() <= max_bytes {
        return value.to_string();
    }
    let mut head_end = floor_char_boundary(value, max_bytes / 2);
    let mut tail_start = ceil_char_boundary(value, value.len().saturating_sub(max_bytes / 2));
    head_end = preferred_head_line(value, head_end);
    tail_start = preferred_tail_line(value, tail_start);
    while head_end < tail_start {
        let omitted = tail_start - head_end;
        let separator = format!("\n...[{omitted} bytes omitted]...\n");
        let total = head_end + separator.len() + value.len() - tail_start;
        if total <= max_bytes {
            return format!(
                "{}{}{}",
                &value[..head_end],
                separator,
                &value[tail_start..]
            );
        }
        if head_end >= value.len() - tail_start {
            head_end = previous_char_boundary(value, head_end);
        } else {
            tail_start = next_char_boundary(value, tail_start);
        }
    }
    value[..floor_char_boundary(value, max_bytes)].to_string()
}

fn preferred_head_line(value: &str, end: usize) -> usize {
    value[..end]
        .rfind('\n')
        .map_or(end, |newline| newline.saturating_add(1))
}

fn preferred_tail_line(value: &str, start: usize) -> usize {
    value[start..]
        .find('\n')
        .map_or(start, |newline| start + newline + 1)
}

fn floor_char_boundary(value: &str, mut index: usize) -> usize {
    index = index.min(value.len());
    while index > 0 && !value.is_char_boundary(index) {
        index -= 1;
    }
    index
}

fn ceil_char_boundary(value: &str, mut index: usize) -> usize {
    index = index.min(value.len());
    while index < value.len() && !value.is_char_boundary(index) {
        index += 1;
    }
    index
}

fn previous_char_boundary(value: &str, index: usize) -> usize {
    floor_char_boundary(value, index.saturating_sub(1))
}

fn next_char_boundary(value: &str, index: usize) -> usize {
    ceil_char_boundary(value, index.saturating_add(1))
}

#[cfg(test)]
mod tests {
    #[test]
    fn preview_keeps_utf8_lines_and_omission_scale() {
        let preview = super::adaptive_preview(
            "第一行\n第二行很长很长很长很长很长很长很长很长\n最后一行",
            48,
        );

        assert!(preview.is_char_boundary(preview.len()));
        assert!(preview.len() <= 48);
        assert!(preview.contains("bytes omitted"));
        assert!(preview.starts_with("第一行\n"));
        assert!(preview.ends_with("最后一行"));
    }

    #[test]
    fn tiny_preview_budget_terminates_with_empty_preview() {
        assert_eq!(super::adaptive_preview("payload", 0), "");
        assert_eq!(super::adaptive_preview("payload", 16), "");
    }
}
