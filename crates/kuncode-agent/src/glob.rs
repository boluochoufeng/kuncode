//! Pure, filesystem-free glob matching.
//!
//! Shared by the `glob` tool (matching walked workspace paths) and the
//! permission layer (matching rule resources against a request's path or
//! command). It is deliberately a *string* matcher: it never touches the
//! filesystem, so the permission layer can reuse it without any IO or TOCTOU
//! concern.
//!
//! Two matching modes, because paths and shell commands treat `/` differently:
//!
//! - [`glob_match`] is **path-segmented**: `/` separates segments, `**` crosses
//!   them, and within a segment `*` matches any run of characters (never `/`)
//!   and `?` matches exactly one. Use it for file paths and glob patterns.
//! - [`command_match`] is **flat**: the whole command is one string and `*`
//!   spans everything, including `/`. A `/` inside a command is a literal slash
//!   (part of a path argument), not a glob separator. Use it for `Bash(...)`
//!   rules so `sudo*` still matches `sudo rm -rf /home`.

/// Normalizes a pattern or path for matching by turning `\` separators into
/// `/`, so Windows-style and Unix-style separators compare equal.
pub fn normalize_pattern(pattern: &str) -> String {
    pattern.replace('\\', "/")
}

/// Returns `true` when `path` matches `pattern`, treating `/` as a segment
/// separator. Both should already be [`normalize_pattern`]-ed by the caller
/// when separator style might differ.
pub fn glob_match(pattern: &str, path: &str) -> bool {
    let pattern_parts = pattern.split('/').collect::<Vec<_>>();
    let path_parts = path.split('/').collect::<Vec<_>>();
    glob_parts_match(&pattern_parts, &path_parts)
}

/// Returns `true` when `command` matches `pattern` as one flat string: `*`
/// spans any characters including `/`, `?` matches one. There is no segment or
/// `**` semantics — `/` is just a literal. This is the matcher for shell
/// command rules, where a slash is part of a path argument, not a separator.
pub fn command_match(pattern: &str, command: &str) -> bool {
    segment_match(pattern, command)
}

fn glob_parts_match(pattern: &[&str], path: &[&str]) -> bool {
    let mut previous = vec![false; path.len() + 1];
    previous[0] = true;
    for pattern_segment in pattern {
        let mut current = vec![false; path.len() + 1];
        if *pattern_segment == "**" {
            current[0] = previous[0];
            for index in 1..=path.len() {
                current[index] = previous[index] || current[index - 1];
            }
        } else {
            for index in 1..=path.len() {
                current[index] =
                    previous[index - 1] && segment_match(pattern_segment, path[index - 1]);
            }
        }
        previous = current;
    }
    previous[path.len()]
}

fn segment_match(pattern: &str, text: &str) -> bool {
    let pattern = pattern.chars().collect::<Vec<_>>();
    let text = text.chars().collect::<Vec<_>>();
    let mut pattern_index = 0usize;
    let mut text_index = 0usize;
    let mut last_star = None;
    let mut star_text_index = 0usize;

    while text_index < text.len() {
        if pattern_index < pattern.len()
            && (pattern[pattern_index] == '?' || pattern[pattern_index] == text[text_index])
        {
            pattern_index += 1;
            text_index += 1;
        } else if pattern_index < pattern.len() && pattern[pattern_index] == '*' {
            last_star = Some(pattern_index);
            pattern_index += 1;
            star_text_index = text_index;
        } else if let Some(star) = last_star {
            star_text_index += 1;
            text_index = star_text_index;
            pattern_index = star + 1;
        } else {
            return false;
        }
    }
    while pattern_index < pattern.len() && pattern[pattern_index] == '*' {
        pattern_index += 1;
    }
    pattern_index == pattern.len()
}

#[cfg(test)]
mod tests {
    use super::{command_match, glob_match};

    #[test]
    fn glob_match_supports_segment_and_recursive_wildcards() {
        assert!(glob_match("*.rs", "main.rs"));
        assert!(!glob_match("*.rs", "src/main.rs"));
        assert!(glob_match("**/*.rs", "src/main.rs"));
        assert!(glob_match("src/**/main.??", "src/bin/main.rs"));
        assert!(!glob_match("src/**/main.??", "src/bin/main.txt"));
        assert!(glob_match("**/**/**/main.rs", "src/bin/main.rs"));
        assert!(glob_match("a/**", "a"));
    }

    #[test]
    fn command_match_spans_slashes_and_spaces() {
        // A command is one flat string: `*` spans spaces *and* the `/home` path
        // — this is how `Bash(sudo*)` / `Bash(cargo *)` rules work.
        assert!(command_match("cargo *", "cargo build --release"));
        assert!(command_match("sudo*", "sudo rm -rf /home"));
        assert!(command_match("rm -rf /*", "rm -rf /"));
        assert!(command_match("rm -rf /*", "rm -rf /home"));
        assert!(!command_match("cargo *", "rustc main.rs"));
    }
}
