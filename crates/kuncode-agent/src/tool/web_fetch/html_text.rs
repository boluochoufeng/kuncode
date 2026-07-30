//! Reduces an HTML document to the readable text a model should see.
//!
//! Deliberately lossy and one-way: the goal is prose a model can reason about,
//! not markup that round-trips. Structure survives only where it changes
//! meaning — headings, list items, code blocks, table cell boundaries, and link
//! targets — and everything else (attributes, styling, script and style bodies)
//! is dropped. Nothing here validates HTML: real pages are malformed, so an
//! unrecognized or unclosed construct degrades to text rather than failing.

use std::borrow::Cow;

use html_escape::decode_html_entities;
use url::Url;

/// Longest element name [`Element::classify`] recognizes (`blockquote`,
/// `figcaption`). A longer name is `Inline` whatever its case, so it needs no
/// normalization.
const MAX_ELEMENT_NAME: usize = 10;

/// What an element contributes to the reduced text.
///
/// Classifying a tag name once, at parse time, is what keeps [`TextWriter::open`]
/// and [`TextWriter::close`] from disagreeing about an element — both match this
/// type exhaustively, so a new variant cannot be handled on one side only. It is
/// also why a tag name is never carried past the parser: nothing downstream
/// prints one, so nothing downstream needs to own one.
#[derive(Clone, Copy, Debug)]
enum Element {
    /// `<a href>`: the resolved target follows the link text.
    Anchor,
    /// A heading or `<title>`, carrying its ATX marker.
    Heading(&'static str),
    /// `<p>`: a blank line on both sides.
    Paragraph,
    /// `<li>`: its own bulleted line.
    ListItem,
    /// `<pre>`: fenced, and whitespace inside it is significant.
    Preformatted,
    /// `<blockquote>`.
    Quote,
    /// `<br>` / `<hr>`: a line break and nothing else.
    Break,
    /// `<td>` / `<th>`: ` | ` separates it from the previous cell.
    Cell,
    /// Content is raw text to drop wholesale, carrying the canonical name its
    /// end tag must match so no `String` is needed to find it.
    RawText(&'static str),
    /// Starts a line, but no blank line of its own.
    Block,
    /// Contributes its text and no structure.
    Inline,
}

impl Element {
    /// Classifies a tag name case-insensitively.
    fn classify(name: &str) -> Self {
        // Tag names are ASCII and short, so lowercasing through a fixed buffer
        // keeps the parser free of per-tag allocation.
        let mut lowered = [0u8; MAX_ELEMENT_NAME];
        let Some(slot) = lowered.get_mut(..name.len()) else {
            return Self::Inline;
        };
        slot.copy_from_slice(name.as_bytes());
        slot.make_ascii_lowercase();
        // The parser accepts only ASCII alphanumerics as a name, so this cannot
        // fail; an empty name would classify as `Inline` regardless.
        match std::str::from_utf8(slot).unwrap_or_default() {
            "a" => Self::Anchor,
            // `<title>` becomes the top heading: it is the page's name, and no
            // page needs two of them.
            "title" | "h1" => Self::Heading("# "),
            "h2" => Self::Heading("## "),
            "h3" => Self::Heading("### "),
            "h4" | "h5" | "h6" => Self::Heading("#### "),
            "p" => Self::Paragraph,
            "li" => Self::ListItem,
            "pre" => Self::Preformatted,
            "blockquote" => Self::Quote,
            "br" | "hr" => Self::Break,
            "td" | "th" => Self::Cell,
            // Raw text carries nothing a reader wants, and skipping it wholesale
            // is what stops an inline script from leaking code into the prose.
            "script" => Self::RawText("script"),
            "style" => Self::RawText("style"),
            "address" | "article" | "aside" | "body" | "caption" | "dd" | "details" | "dialog"
            | "div" | "dl" | "dt" | "fieldset" | "figcaption" | "figure" | "footer" | "form"
            | "head" | "header" | "html" | "main" | "nav" | "ol" | "section" | "summary"
            | "table" | "tbody" | "tfoot" | "thead" | "tr" | "ul" => Self::Block,
            _ => Self::Inline,
        }
    }
}

/// Reduces `input` to readable text, resolving link targets against `base` —
/// the URL the document was read from.
pub(super) fn html_to_text(input: &str, base: &Url) -> String {
    let mut writer = TextWriter::new(base);
    let mut rest = input;
    // Depth rather than a flag: nested `<pre>` is malformed but occurs, and the
    // inner close must not re-enable whitespace collapsing for the outer block.
    let mut preformatted = 0usize;

    while let Some(open) = rest.find('<') {
        writer.push_text(&rest[..open], preformatted > 0);
        // `<` is one ASCII byte, so the parser receives the tag body and the
        // precondition "input starts with `<`" cannot be violated at all.
        let (construct, remainder) = Construct::parse(&rest[open + 1..]);
        rest = remainder;

        match construct {
            Construct::Literal => writer.push_text("<", preformatted > 0),
            Construct::Ignored => {}
            Construct::Start {
                element,
                attributes,
                self_closing,
            } => match element {
                Element::RawText(name) if !self_closing => rest = skip_raw_text(rest, name),
                Element::Anchor => writer.open_link(attribute(attributes, "href")),
                Element::Preformatted if !self_closing => {
                    preformatted += 1;
                    writer.open(element);
                }
                element => writer.open(element),
            },
            Construct::End { element } => match element {
                Element::Anchor => writer.close_link(),
                Element::Preformatted => {
                    preformatted = preformatted.saturating_sub(1);
                    writer.close(element);
                }
                element => writer.close(element),
            },
        }
    }
    writer.push_text(rest, preformatted > 0);
    normalize(&writer.finish())
}

/// One construct at the head of the document.
enum Construct<'a> {
    /// A start tag, possibly self-closing.
    Start {
        element: Element,
        attributes: &'a str,
        self_closing: bool,
    },
    /// An end tag.
    End { element: Element },
    /// A `<` that starts no tag at all, so it is the literal character it looks
    /// like (`a < b`).
    Literal,
    /// Consumed but contributing nothing: a comment, doctype, processing
    /// instruction, or a tag left unterminated at end of input.
    Ignored,
}

impl<'a> Construct<'a> {
    /// Splits the construct following a `<` from the rest of the document.
    ///
    /// `body` is the text *after* the `<`. A [`Self::Literal`] result therefore
    /// leaves `body` itself as the remainder, which is both what the caller needs
    /// in order to emit the `<` as text and what guarantees the scan advances.
    fn parse(body: &'a str) -> (Self, &'a str) {
        if let Some(comment) = body.strip_prefix("!--") {
            // An unterminated comment swallows the rest of the document, which
            // is what a browser does with it too.
            let end = comment.find("-->").map_or(comment.len(), |at| at + 3);
            return (Self::Ignored, &comment[end..]);
        }
        if body.starts_with(['!', '?']) {
            let end = body.find('>').map_or(body.len(), |at| at + 1);
            return (Self::Ignored, &body[end..]);
        }

        let (rest, closing) = match body.strip_prefix('/') {
            Some(rest) => (rest, true),
            None => (body, false),
        };
        if !rest.starts_with(|ch: char| ch.is_ascii_alphabetic()) {
            return (Self::Literal, body);
        }
        let name_end = rest
            .find(|ch: char| !ch.is_ascii_alphanumeric())
            .unwrap_or(rest.len());
        let (name, rest) = rest.split_at(name_end);
        let element = Element::classify(name);

        let Some(tag_end) = find_tag_end(rest) else {
            // A tag left open at end of input names no content; a browser
            // discards it, and printing its raw markup would read worse.
            return (Self::Ignored, "");
        };
        let (attributes, remainder) = rest.split_at(tag_end);
        let remainder = &remainder[1..];
        if closing {
            return (Self::End { element }, remainder);
        }
        (
            Self::Start {
                element,
                attributes,
                self_closing: attributes.trim_end().ends_with('/'),
            },
            remainder,
        )
    }
}

/// Locates the `>` closing a tag, skipping quoted attribute values so a `>`
/// inside one does not end the tag early.
fn find_tag_end(body: &str) -> Option<usize> {
    let mut quote = None::<char>;
    for (index, ch) in body.char_indices() {
        match (quote, ch) {
            (Some(open), ch) if ch == open => quote = None,
            (Some(_), _) => {}
            (None, '"' | '\'') => quote = Some(ch),
            (None, '>') => return Some(index),
            (None, _) => {}
        }
    }
    None
}

/// Returns the value of `name` in a start tag's attribute text.
fn attribute<'a>(attributes: &'a str, name: &str) -> Option<Cow<'a, str>> {
    let mut rest = attributes;
    while let Some(at) = rest.find(|ch: char| ch.is_ascii_alphabetic()) {
        rest = &rest[at..];
        let key_end = rest
            .find(|ch: char| !ch.is_ascii_alphanumeric() && ch != '-' && ch != ':')
            .unwrap_or(rest.len());
        let (key, remainder) = rest.split_at(key_end);
        let matched = key.eq_ignore_ascii_case(name);
        let Some(remainder) = remainder.trim_start().strip_prefix('=') else {
            // A valueless attribute (`disabled`); resume after its name.
            rest = remainder;
            continue;
        };
        let remainder = remainder.trim_start();
        let (value, remainder) = match remainder.strip_prefix(['"', '\'']) {
            // The prefix matched, so byte 0 is that ASCII quote character.
            Some(quoted) => match quoted.split_once(remainder.as_bytes()[0] as char) {
                Some(pair) => pair,
                None => (quoted, ""),
            },
            None => remainder.split_at(
                remainder
                    .find(char::is_whitespace)
                    .unwrap_or(remainder.len()),
            ),
        };
        if matched {
            return Some(decode_html_entities(value));
        }
        rest = remainder;
    }
    None
}

/// Skips past the matching end tag of a raw-text element, whose body may contain
/// anything — including markup that must not be parsed as such.
fn skip_raw_text<'a>(input: &'a str, name: &str) -> &'a str {
    let mut rest = input;
    loop {
        let Some(at) = rest.find("</") else {
            return "";
        };
        rest = &rest[at..];
        if closes_raw_text(&rest.as_bytes()[2..], name) {
            let end = rest.find('>').map_or(rest.len(), |at| at + 1);
            return &rest[end..];
        }
        rest = &rest[2..];
    }
}

/// Reports whether `after_slash` — the bytes following `</` — closes the
/// raw-text element `name`.
///
/// The tag name must be followed by a delimiter. Matching the name as a bare
/// prefix would let `</scriptfoo>` end a `<script>`, resuming markup parsing
/// inside the element and spilling its code into the extracted prose.
fn closes_raw_text(after_slash: &[u8], name: &str) -> bool {
    let Some(delimiter) = after_slash.split_at_checked(name.len()) else {
        return false;
    };
    let (candidate, delimiter) = delimiter;
    candidate.eq_ignore_ascii_case(name.as_bytes())
        // Nothing after the name means the document ended mid-tag, which closes
        // the element as surely as `>` would: there is no more content either way.
        && delimiter
            .first()
            .is_none_or(|byte| byte.is_ascii_whitespace() || *byte == b'/' || *byte == b'>')
}

/// Accumulates reduced text, inserting the boundaries that carry meaning.
struct TextWriter<'a> {
    buffer: String,
    /// Open `<a href>` targets, already resolved. A stack because nested anchors
    /// are malformed but appear, and a close must pair with what opened it.
    links: Vec<Option<String>>,
    /// URL the document was read from, which relative targets resolve against.
    base: &'a Url,
}

impl<'a> TextWriter<'a> {
    fn new(base: &'a Url) -> Self {
        Self {
            buffer: String::new(),
            links: Vec::new(),
            base,
        }
    }

    fn finish(self) -> String {
        self.buffer
    }

    fn open(&mut self, element: Element) {
        match element {
            Element::Heading(marker) => {
                self.blank_line();
                self.buffer.push_str(marker);
            }
            Element::Paragraph => self.blank_line(),
            Element::ListItem => {
                self.newline();
                self.buffer.push_str("- ");
            }
            Element::Preformatted => {
                self.blank_line();
                self.buffer.push_str("```\n");
            }
            Element::Quote => {
                self.blank_line();
                self.buffer.push_str("> ");
            }
            Element::Break | Element::Block => self.newline(),
            Element::Cell => self.cell_boundary(),
            // An anchor's target goes through `open_link`; raw text never reaches
            // the writer at all; inline elements contribute only their text.
            Element::Anchor | Element::RawText(_) | Element::Inline => {}
        }
    }

    fn close(&mut self, element: Element) {
        match element {
            Element::Heading(_) | Element::Paragraph | Element::Quote => self.blank_line(),
            Element::Preformatted => {
                self.newline();
                self.buffer.push_str("```\n");
                self.blank_line();
            }
            Element::ListItem | Element::Block => self.newline(),
            // A cell boundary is written when the *next* cell opens, so closing
            // one must not add a line the row does not have. A void element's end
            // tag is meaningless markup — its break was emitted on open.
            Element::Cell
            | Element::Break
            | Element::Anchor
            | Element::RawText(_)
            | Element::Inline => {}
        }
    }

    fn open_link(&mut self, href: Option<Cow<'_, str>>) {
        let resolved = href.and_then(|href| self.resolve(&href));
        self.links.push(resolved);
    }

    /// Emits the link target after its text, so `docs (https://…)` reads as
    /// prose while the model still learns the URL it can fetch next.
    fn close_link(&mut self) {
        let Some(href) = self.links.pop().flatten() else {
            return;
        };
        self.buffer.push_str(" (");
        self.buffer.push_str(&href);
        self.buffer.push(')');
    }

    /// Resolves a link target against the document's own URL, because an
    /// emitted target is only worth its bytes if it is one `web_fetch` can be
    /// handed back: a bare `index.html` tells the model nothing.
    ///
    /// Returns `None` for a target that names no document to fetch — a bare
    /// fragment, or a `javascript:` / `mailto:` / `data:` scheme.
    fn resolve(&self, href: &str) -> Option<String> {
        let href = href.trim();
        if href.is_empty() || href.starts_with('#') {
            return None;
        }
        let resolved = self.base.join(href).ok()?;
        matches!(resolved.scheme(), "http" | "https").then(|| resolved.to_string())
    }

    fn push_text(&mut self, text: &str, preformatted: bool) {
        if text.is_empty() {
            return;
        }
        let text = decode_html_entities(text);
        if preformatted {
            self.buffer.push_str(&plain_spaces(&text));
            return;
        }
        let collapsed = collapse_inline(&text);
        if collapsed.trim().is_empty() {
            // Whitespace between tags still separates words, so it survives as
            // at most one space — never as a blank line.
            self.space();
            return;
        }
        // Markup alone never separates words: `<b>ku</b><i>ncode</i>` is one
        // word, and the space that belongs between two runs is present in the
        // source, where the collapse above preserved it. It is dropped only
        // where the buffer already ends in whitespace, so it cannot double a
        // separator or indent a line a block boundary just opened.
        let collapsed = if self.buffer.is_empty() || self.ends_with_space() {
            collapsed.trim_start()
        } else {
            collapsed.as_ref()
        };
        self.buffer.push_str(collapsed);
    }

    fn newline(&mut self) {
        if !self.buffer.is_empty() && !self.buffer.ends_with('\n') {
            self.buffer.push('\n');
        }
    }

    fn blank_line(&mut self) {
        if self.buffer.is_empty() || self.buffer.ends_with("\n\n") {
            return;
        }
        self.newline();
        self.buffer.push('\n');
    }

    fn cell_boundary(&mut self) {
        if self.buffer.is_empty() || self.buffer.ends_with('\n') || self.buffer.ends_with(" | ") {
            return;
        }
        self.buffer.push_str(" | ");
    }

    fn space(&mut self) {
        if !self.buffer.is_empty() && !self.ends_with_space() {
            self.buffer.push(' ');
        }
    }

    fn ends_with_space(&self) -> bool {
        self.buffer.ends_with(char::is_whitespace)
    }
}

/// Collapses runs of whitespace to one space, preserving whether the run started
/// or ended the text so word boundaries across tags survive.
fn collapse_inline(text: &str) -> Cow<'_, str> {
    if !text.contains(char::is_whitespace) {
        return Cow::Borrowed(text);
    }
    let mut words = text.split_whitespace();
    let Some(first) = words.next() else {
        // Whitespace only — the gap between two tags, by far the most common
        // text run on a page. One space is everything it meant.
        return Cow::Borrowed(" ");
    };
    let mut collapsed = String::with_capacity(text.len());
    if text.starts_with(char::is_whitespace) {
        collapsed.push(' ');
    }
    collapsed.push_str(first);
    for word in words {
        collapsed.push(' ');
        collapsed.push_str(word);
    }
    if text.ends_with(char::is_whitespace) {
        collapsed.push(' ');
    }
    Cow::Owned(collapsed)
}

/// Rewrites the space-like characters a reference can produce as plain spaces.
///
/// Only needed where [`collapse_inline`] does not run, which is preformatted
/// text: a code block that indents with `&nbsp;` is old but real, and a U+00A0
/// the model copies into a shell or a source file is a syntax error it cannot
/// see. ASCII whitespace is left alone — a tab is indentation and a newline is a
/// line.
fn plain_spaces(text: &str) -> Cow<'_, str> {
    let space_like = |character: char| character.is_whitespace() && !character.is_ascii();
    if !text.contains(space_like) {
        return Cow::Borrowed(text);
    }
    Cow::Owned(
        text.chars()
            .map(|character| {
                if space_like(character) {
                    ' '
                } else {
                    character
                }
            })
            .collect(),
    )
}

/// Trims trailing whitespace per line and collapses runs of blank lines, which
/// the boundary rules above emit freely rather than tracking across elements.
fn normalize(text: &str) -> String {
    let mut normalized = String::with_capacity(text.len());
    let mut pending_blank = false;
    // Splitting on `\n` and trimming each line's tail handles CRLF for free: a
    // stray `\r` is trailing whitespace on its own line.
    for line in text.split('\n') {
        let line = line.trim_end();
        if line.is_empty() {
            pending_blank = true;
            continue;
        }
        if !normalized.is_empty() {
            normalized.push('\n');
            if pending_blank {
                normalized.push('\n');
            }
        }
        pending_blank = false;
        normalized.push_str(line);
    }
    normalized
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Reduces `input` as if it had been read from a real page, so relative link
    /// targets have something to resolve against.
    fn reduce(input: &str) -> String {
        html_to_text(
            input,
            &Url::parse("https://docs.example.com/guide/intro").expect("base URL parses"),
        )
    }

    #[test]
    fn headings_lists_and_links_survive_as_readable_text() {
        let reduced = reduce(
            r#"<html><head><title>Docs</title><style>body{color:red}</style></head>
               <body><h2>Install</h2><p>Run <code>cargo add kuncode</code> first.</p>
               <ul><li>See the <a href="https://example.com/guide">guide</a></li></ul>
               <script>alert('x')</script></body></html>"#,
        );

        assert_eq!(
            reduced,
            "# Docs\n\n## Install\n\nRun cargo add kuncode first.\n\n- See the guide (https://example.com/guide)"
        );
    }

    #[test]
    fn preformatted_text_keeps_its_line_structure() {
        let reduced = reduce("<p>Example:</p><pre>fn main() {\n    ok();\n}</pre>");

        assert_eq!(reduced, "Example:\n\n```\nfn main() {\n    ok();\n}\n```");
    }

    #[test]
    fn table_rows_become_delimited_lines() {
        let reduced = reduce(
            "<table><tr><th>Name</th><th>Port</th></tr><tr><td>api</td><td>8080</td></tr></table>",
        );

        assert_eq!(reduced, "Name | Port\napi | 8080");
    }

    #[test]
    fn character_references_resolve_and_unknown_ones_stay_verbatim() {
        let reduced = reduce("<p>a&nbsp;&amp;&nbsp;b &#8212; c &notareference; &#x41;</p>");

        assert_eq!(reduced, "a & b — c &notareference; A");
    }

    #[test]
    fn the_whole_html5_reference_table_resolves() {
        // The point of the dependency: the table is 2231 entries, and a page is
        // free to use any of them. These are the tiers a hand-written table kept
        // getting wrong — Greek letters and mathematical operators in technical
        // prose, box-drawing and rarely-seen names, and the case-distinct pairs.
        assert_eq!(
            reduce("<p>&alpha; &Omega; &sum; &infin; &ne; &asymp; &times; &frac12;</p>"),
            "α Ω ∑ ∞ ≠ ≈ × ½"
        );
        assert_eq!(
            reduce("<p>&boxdl; &nldr; &plankv; &Zopf; &vzigzag; &dagger; &Dagger;</p>"),
            "┐ ‥ ℏ ℤ ⦚ † ‡"
        );
        // Resolving is what keeps them from spending the content budget: seven
        // bytes of `&alpha;` against two of `α`, against a 50 kB cap.
        assert!("&alpha;".len() > "α".len());
    }

    #[test]
    fn multi_codepoint_references_lose_their_combining_mark() {
        // A known defect in `html-escape`: the 93 table entries that map to two
        // code points come back as the first one only, so `&NotGreaterFullEqual;`
        // reads as its own opposite. Pinned rather than worked around — patching
        // it here would mean maintaining a slice of the very table the dependency
        // exists to own. If this test ever fails, upstream fixed it: delete the
        // test and this comment.
        assert_eq!(reduce("<p>&NotGreaterFullEqual;</p>"), "≧");
        assert_eq!(reduce("<p>&NotLessLess;</p>"), "≪");

        // Same trade on the 106 entries HTML5 allows without a semicolon. Leaving
        // them verbatim is the safe direction: `&AMP` still reads as an ampersand
        // to a model, where a silently wrong operator does not.
        assert_eq!(reduce("<p>&AMP &copy</p>"), "&AMP &copy");
    }

    #[test]
    fn space_like_references_do_not_leak_into_code_blocks() {
        // `&nbsp;` indentation in a `<pre>` is old but real, and U+00A0 copied
        // into a shell or a source file is an error the model cannot see. Outside
        // preformatted text the collapse step already handles them.
        let reduced = reduce("<pre>fn main() {\n&nbsp;&nbsp;&nbsp;&nbsp;ok();\n}</pre>");

        assert_eq!(reduced, "```\nfn main() {\n    ok();\n}\n```");
        assert!(!reduced.contains('\u{a0}'), "U+00A0 reached the model");
        // A tab is indentation, not a reference artifact, so it is left alone.
        assert_eq!(reduce("<pre>a\n\tb</pre>"), "```\na\n\tb\n```");
    }

    #[test]
    fn attribute_values_containing_angle_brackets_do_not_end_the_tag() {
        let reduced = reduce(r#"<a href="https://example.com/?q=a>b" title='x>y'>link</a>"#);

        // The whole query survived, so neither `>` ended the tag early. It comes
        // back percent-encoded because resolution normalizes the target into one
        // that can be requested as-is.
        assert_eq!(reduced, "link (https://example.com/?q=a%3Eb)");
    }

    #[test]
    fn stray_and_unterminated_markup_keeps_the_text_around_it() {
        // A bare `<` in prose, an unclosed comment, and a tag with no `>` are
        // all ordinary on real pages; none may drop the surrounding text.
        assert_eq!(reduce("<p>a &lt; b and c < d</p>"), "a < b and c < d");
        assert_eq!(reduce("<p>kept</p><!-- dangling"), "kept");
        assert_eq!(reduce("<p>kept</p><span class="), "kept");
    }

    #[test]
    fn classification_is_case_insensitive_up_to_the_longest_name() {
        // `blockquote` and `figcaption` are exactly `MAX_ELEMENT_NAME` long, so an
        // upper-case spelling is what actually exercises the buffer's edge. A name
        // that outgrew the bound would silently fall through to `Inline` and lose
        // its boundary, which no other test would notice — hence this one.
        assert_eq!(
            reduce("<P>a</P><BLOCKQUOTE>quoted</BLOCKQUOTE><P>b</P>"),
            "a\n\n> quoted\n\nb"
        );
        assert_eq!(reduce("<FIGCAPTION>cap</FIGCAPTION>x"), "cap\nx");
        assert_eq!(reduce("<p>kept</p><SCRIPT>gone()</SCRIPT>"), "kept");
    }

    #[test]
    fn an_end_tag_lookalike_does_not_close_a_raw_text_element() {
        // `</scriptfoo>` is not an end tag. Ending the element on a bare prefix
        // match resumes markup parsing inside the script and spills its code
        // into the prose — which is exactly what the model must not read as text.
        assert_eq!(
            reduce("<p>before</p><script>var a = 1; </scriptfoo> leaked();</script><p>after</p>"),
            "before\n\nafter"
        );
        // Any delimiter closes it, not just `>`.
        assert_eq!(reduce("<p>a</p><script>x()</script  >\n<p>b</p>"), "a\n\nb");
        assert_eq!(reduce("<p>a</p><style>.x{}</style/><p>b</p>"), "a\n\nb");
        // A document that ends mid-end-tag has no further content to recover.
        assert_eq!(reduce("<p>a</p><script>x()</script"), "a");
    }

    #[test]
    fn script_bodies_never_reach_the_output() {
        let reduced = reduce(
            "<p>before</p><script type=\"text/javascript\">var a = '</p>'; leak();</script><p>after</p>",
        );

        assert_eq!(reduced, "before\n\nafter");
    }

    #[test]
    fn markup_alone_neither_splits_nor_glues_words() {
        assert_eq!(reduce("<p><b>ku</b><i>ncode</i></p>"), "kuncode");
        assert_eq!(reduce("<p>one <b>two</b> three</p>"), "one two three");
        assert_eq!(
            reduce("<p>see <a href=\"/x\">it</a>.</p>"),
            "see it (https://docs.example.com/x)."
        );
    }

    #[test]
    fn source_line_breaks_collapse_and_only_boundaries_start_lines() {
        // HTML whitespace semantics: a newline in the source is a space, and a
        // line starts only where `<br>` or a block boundary says so.
        assert_eq!(reduce("<p>one\n   two</p><p>three</p>"), "one two\n\nthree");
        assert_eq!(reduce("<p>a<br>b</p>"), "a\nb");
    }

    #[test]
    fn relative_link_targets_resolve_against_the_page_they_came_from() {
        // A bare `index.html` is not something the model can be handed back, so
        // a target is only worth emitting once it is absolute.
        assert_eq!(
            reduce(r#"<a href="index.html">up</a>"#),
            "up (https://docs.example.com/guide/index.html)"
        );
        assert_eq!(
            reduce(r#"<a href="../install">install</a>"#),
            "install (https://docs.example.com/install)"
        );
        assert_eq!(
            reduce(r#"<a href="//cdn.example.net/a">cdn</a>"#),
            "cdn (https://cdn.example.net/a)"
        );
    }

    #[test]
    fn fragment_and_script_link_targets_are_dropped() {
        // Neither names a document `web_fetch` could be pointed at next.
        assert_eq!(reduce(r##"<a href="#top">top</a>"##), "top");
        assert_eq!(reduce(r#"<a href="javascript:x()">go</a>"#), "go");
    }
}
