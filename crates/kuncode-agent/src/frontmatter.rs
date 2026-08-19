//! Minimal `---`-delimited frontmatter shared by skill and agent-type files.
//!
//! Only single-line `key: value` scalars are recognized — the subset both
//! catalogs need — so no YAML dependency is warranted. Unknown keys are kept
//! (callers pick what they use), and a document written for a richer harness
//! still parses here.

use std::collections::BTreeMap;

/// Parsed fields plus the body that follows the closing delimiter.
pub(crate) struct Frontmatter<'a> {
    fields: BTreeMap<&'a str, &'a str>,
    body: &'a str,
}

impl<'a> Frontmatter<'a> {
    /// Returns the trimmed value of `key`, when the frontmatter carried it.
    pub(crate) fn field(&self, key: &str) -> Option<&'a str> {
        self.fields.get(key).copied()
    }

    /// The document body after the closing delimiter — or the whole document
    /// when no frontmatter block was found.
    pub(crate) fn body(&self) -> &'a str {
        self.body
    }
}

/// Collapses whitespace runs to single spaces and caps the char count, so one
/// catalog entry stays one line of bounded prompt cost. Shared by both
/// frontmatter consumers (skills and agent types) because their names and
/// descriptions end up in the same kind of prompt-prefix line.
pub(crate) fn flatten(text: &str, max_chars: usize) -> String {
    let mut flat = text.split_whitespace().collect::<Vec<_>>().join(" ");
    if flat.chars().count() > max_chars {
        flat = flat.chars().take(max_chars).collect();
    }
    flat
}

/// Splits optional frontmatter from the body.
///
/// A block opens with `---` as the exact first line and closes at the next
/// `---` line. An unterminated block is treated as plain body: guessing where
/// it "would" end risks hiding most of the document.
pub(crate) fn parse(document: &str) -> Frontmatter<'_> {
    let plain = Frontmatter {
        fields: BTreeMap::new(),
        body: document,
    };
    let Some(after_open) = document
        .strip_prefix("---\n")
        .or_else(|| document.strip_prefix("---\r\n"))
    else {
        return plain;
    };
    let mut fields = BTreeMap::new();
    let mut cursor = 0usize;
    while cursor < after_open.len() {
        let line_end = after_open[cursor..].find('\n').map(|found| cursor + found);
        let line = match line_end {
            Some(end) => &after_open[cursor..end],
            None => &after_open[cursor..],
        };
        let line = line.trim_end_matches('\r');
        if line.trim() == "---" {
            let body = match line_end {
                Some(end) => &after_open[end + 1..],
                None => "",
            };
            return Frontmatter { fields, body };
        }
        if let Some((key, value)) = line.split_once(':') {
            fields.insert(key.trim(), value.trim());
        }
        match line_end {
            Some(end) => cursor = end + 1,
            None => break,
        }
    }
    plain
}

#[cfg(test)]
mod tests {
    use super::parse;

    #[test]
    fn fields_and_body_split_at_the_closing_delimiter() {
        let doc = "---\nname: pdf\ndescription: Process PDFs.\nextra: kept\n---\nBody here.";
        let parsed = parse(doc);

        assert_eq!(parsed.field("name"), Some("pdf"));
        assert_eq!(parsed.field("description"), Some("Process PDFs."));
        assert_eq!(parsed.field("extra"), Some("kept"));
        assert_eq!(parsed.field("absent"), None);
        assert_eq!(parsed.body(), "Body here.");
    }

    #[test]
    fn documents_without_frontmatter_are_all_body() {
        let parsed = parse("Just text.\nname: not a field");

        assert_eq!(parsed.field("name"), None);
        assert_eq!(parsed.body(), "Just text.\nname: not a field");
    }

    #[test]
    fn an_unterminated_block_is_treated_as_body() {
        let doc = "---\nname: hidden\nno closing delimiter";
        let parsed = parse(doc);

        assert_eq!(parsed.field("name"), None);
        assert_eq!(parsed.body(), doc);
    }

    #[test]
    fn crlf_documents_parse_the_same() {
        let parsed = parse("---\r\nname: a\r\n---\r\nbody");

        assert_eq!(parsed.field("name"), Some("a"));
        assert_eq!(parsed.body(), "body");
    }
}
