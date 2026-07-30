//! Reduces an HTML document to the readable text a model should see.
//!
//! Deliberately lossy and one-way: the goal is prose a model can reason about,
//! not markup that round-trips. Structure survives only where it changes
//! meaning — headings, list items, code blocks, table cell boundaries, and link
//! targets — and everything else (attributes, styling, script and style bodies)
//! is dropped.
//!
//! Tokenizing is `html5ever`'s, reducing is this module's, and the split is
//! deliberate. Where a `<script>` body ends, what `&NotLessLess;` stands for,
//! whether `<a title="a<b">` closed its tag — those are settled by the HTML
//! standard, and getting them wrong means reading a page differently than every
//! other reader does. Which elements carry meaning worth keeping, and what shape
//! that meaning takes as text, the standard says nothing about.

use std::borrow::Cow;
use std::cell::{Cell, RefCell};

use html5ever::buffer_queue::BufferQueue;
use html5ever::interface::Attribute;
use html5ever::tendril::StrTendril;
use html5ever::tokenizer::states::RawKind;
use html5ever::tokenizer::{Tag, TagKind, Token, TokenSink, TokenSinkResult, Tokenizer};
use url::Url;

/// What an element contributes to the reduced text.
///
/// Classifying a tag name once, on its start tag, is what keeps
/// [`TextWriter::open`] and [`TextWriter::close`] from disagreeing about an
/// element — both match this type exhaustively, so a new variant cannot be
/// handled on one side only.
#[derive(Clone, Copy, Debug)]
enum Element {
    /// `<a href>`: the resolved target follows the link text.
    Anchor,
    /// `<base href>`: retargets every relative link that follows.
    Base,
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
    /// The element and everything inside it stays out of the prose. The
    /// [`RawKind`] is the tokenizer state its content must be read in, for the
    /// elements whose bodies are not markup at all.
    Discarded(Option<RawKind>),
    /// Starts a line, but no blank line of its own.
    Block,
    /// Contributes its text and no structure.
    Inline,
}

impl Element {
    /// Classifies a tag name. The tokenizer lowercases names, so nothing here
    /// normalizes one.
    fn classify(name: &str) -> Self {
        match name {
            "a" => Self::Anchor,
            "base" => Self::Base,
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
            // A script or style body carries nothing a reader wants, and asking
            // for its raw state is what stops `if (a < b)` from being read as a
            // tag. `<textarea>` is a form's default value rather than prose, and
            // its body is text even where it looks like markup.
            "script" => Self::Discarded(Some(RawKind::ScriptData)),
            "style" => Self::Discarded(Some(RawKind::Rawtext)),
            "textarea" => Self::Discarded(Some(RawKind::Rcdata)),
            // A `<template>` body is markup the page never renders — it is
            // material for scripts this tool does not run — so it is dropped
            // without changing how it is tokenized.
            "template" => Self::Discarded(None),
            "address" | "article" | "aside" | "body" | "caption" | "dd" | "details" | "dialog"
            | "div" | "dl" | "dt" | "fieldset" | "figcaption" | "figure" | "footer" | "form"
            | "head" | "header" | "html" | "main" | "nav" | "ol" | "section" | "summary"
            | "table" | "tbody" | "tfoot" | "thead" | "tr" | "ul" => Self::Block,
            _ => Self::Inline,
        }
    }
}

/// Reduces `input` to readable text, resolving link targets against `base` —
/// the URL the document was read from, unless the document declares another.
pub(super) fn html_to_text(input: &str, base: &Url) -> String {
    let tokenizer = Tokenizer::new(Reducer::new(base.clone()), Default::default());
    let queue = BufferQueue::default();
    queue.push_back(StrTendril::from(input));
    // The sink never asks the tokenizer to stop, so feeding once drains the
    // whole document and the result carries nothing to act on.
    let _ = tokenizer.feed(&queue);
    tokenizer.end();
    normalize(&tokenizer.sink.finish())
}

/// Turns the token stream into text.
///
/// The interior mutability is `html5ever`'s requirement, not a design choice:
/// [`TokenSink::process_token`] takes `&self` so a sink can be shared, and this
/// one has to accumulate.
struct Reducer {
    writer: RefCell<TextWriter>,
    /// Depth rather than a flag: nested `<pre>` is malformed but occurs, and the
    /// inner close must not re-enable whitespace collapsing for the outer block.
    preformatted: Cell<usize>,
    /// Depth of discarded elements. `<template>` nests, and the tokenizer's raw
    /// states keep the others from ever nesting, so one counter serves both.
    discarded: Cell<usize>,
}

impl Reducer {
    fn new(base: Url) -> Self {
        Self {
            writer: RefCell::new(TextWriter::new(base)),
            preformatted: Cell::new(0),
            discarded: Cell::new(0),
        }
    }

    fn finish(&self) -> String {
        self.writer.borrow().text().to_string()
    }

    fn tag(&self, tag: Tag) -> TokenSinkResult<()> {
        let element = Element::classify(&tag.name);
        let ending = tag.kind == TagKind::EndTag;

        // Inside a discarded element the only tag that matters is one that
        // changes the depth; everything else contributes neither text nor
        // structure, so it is not worth telling the writer about.
        if self.discarded.get() > 0 {
            if let Element::Discarded(_) = element {
                self.depth(&self.discarded, ending, tag.self_closing);
            }
            return TokenSinkResult::Continue;
        }

        if let Element::Preformatted = element {
            self.depth(&self.preformatted, ending, tag.self_closing);
        }
        if ending {
            match element {
                Element::Anchor => self.writer.borrow_mut().close_link(),
                element => self.writer.borrow_mut().close(element),
            }
            return TokenSinkResult::Continue;
        }
        match element {
            Element::Anchor => self
                .writer
                .borrow_mut()
                .open_link(attribute(&tag.attrs, "href")),
            Element::Base => {
                if let Some(href) = attribute(&tag.attrs, "href") {
                    self.writer.borrow_mut().declare_base(href);
                }
            }
            Element::Discarded(raw) => {
                self.depth(&self.discarded, false, tag.self_closing);
                // Asking for the raw state is what makes the tokenizer read the
                // body as text and find its end tag by the standard's rules
                // rather than by looking for the next `<`.
                if let Some(kind) = raw.filter(|_| !tag.self_closing) {
                    return TokenSinkResult::RawData(kind);
                }
            }
            element => self.writer.borrow_mut().open(element),
        }
        TokenSinkResult::Continue
    }

    /// Tracks entering and leaving a nesting element.
    ///
    /// A self-closing *start* tag opens nothing, so it changes no depth. An end
    /// tag closes regardless: `</style/>` carries the self-closing flag because
    /// the standard routes that stray slash through the self-closing state, and
    /// treating it as "not really an end tag" would leave the element open and
    /// swallow the rest of the page.
    fn depth(&self, counter: &Cell<usize>, ending: bool, self_closing: bool) {
        if ending {
            counter.set(counter.get().saturating_sub(1));
        } else if !self_closing {
            counter.set(counter.get() + 1);
        }
    }
}

impl TokenSink for Reducer {
    type Handle = ();

    fn process_token(&self, token: Token, _line_number: u64) -> TokenSinkResult<()> {
        match token {
            Token::TagToken(tag) => return self.tag(tag),
            Token::CharacterTokens(text) if self.discarded.get() == 0 => {
                self.writer
                    .borrow_mut()
                    .push_text(&text, self.preformatted.get() > 0);
            }
            // Text inside a discarded element, plus the tokens that carry nothing
            // a reader wants. A NUL is dropped rather than turned into the U+FFFD
            // the standard asks for: this output is prose, and a replacement
            // character is noise either way.
            Token::CharacterTokens(_)
            | Token::NullCharacterToken
            | Token::CommentToken(_)
            | Token::DoctypeToken(_)
            | Token::EOFToken
            | Token::ParseError(_) => {}
        }
        TokenSinkResult::Continue
    }
}

/// Reads an attribute value from a start tag.
fn attribute<'tag>(attrs: &'tag [Attribute], name: &str) -> Option<&'tag str> {
    attrs
        .iter()
        .find(|attribute| &*attribute.name.local == name)
        .map(|attribute| &*attribute.value)
}

/// Accumulates reduced text, inserting the boundaries that carry meaning.
struct TextWriter {
    buffer: String,
    /// Open `<a href>` targets, already resolved. A stack because a close must
    /// pair with what opened it.
    links: Vec<Option<String>>,
    /// URL relative targets resolve against.
    base: Url,
    /// Whether a `<base href>` has replaced it. Only the first one counts.
    base_declared: bool,
}

impl TextWriter {
    fn new(base: Url) -> Self {
        Self {
            buffer: String::new(),
            links: Vec::new(),
            base,
            base_declared: false,
        }
    }

    fn text(&self) -> &str {
        &self.buffer
    }

    /// Retargets relative links to the URL the document names as its base.
    ///
    /// Without this, a page served from one host but written for another — an
    /// ordinary arrangement behind a CDN or a docs mirror — yields links that
    /// resolve against the wrong origin, and the model is handed URLs that 404.
    /// A later `<base>` is ignored, as in a browser.
    fn declare_base(&mut self, href: &str) {
        if self.base_declared {
            return;
        }
        if let Ok(declared) = self.base.join(href.trim()) {
            self.base = declared;
            self.base_declared = true;
        }
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
            // An anchor's target goes through `open_link` and a base through
            // `declare_base`; a discarded element never reaches the writer;
            // inline elements contribute only their text.
            Element::Anchor | Element::Base | Element::Discarded(_) | Element::Inline => {}
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
            // contributes nothing — its break was emitted on open.
            Element::Cell
            | Element::Break
            | Element::Anchor
            | Element::Base
            | Element::Discarded(_)
            | Element::Inline => {}
        }
    }

    fn open_link(&mut self, href: Option<&str>) {
        let resolved = href.and_then(|href| self.resolve(href));
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

    /// Resolves a link target against the document's base, because an emitted
    /// target is only worth its bytes if it is one `web_fetch` can be handed
    /// back: a bare `index.html` tells the model nothing.
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
        if preformatted {
            self.buffer.push_str(&plain_spaces(text));
            return;
        }
        let collapsed = collapse_inline(text);
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

/// Rewrites the space-like characters a character reference can produce as plain
/// spaces.
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
    fn a_declared_base_retargets_relative_links() {
        // A page served from one host but written for another is ordinary behind
        // a CDN or a docs mirror. Resolving against the fetched URL instead
        // hands the model links that 404.
        assert_eq!(
            reduce(r#"<base href="https://cdn.example.com/v2/"><a href="x.html">link</a>"#),
            "link (https://cdn.example.com/v2/x.html)"
        );
        // Only the first `<base>` counts, as in a browser.
        assert_eq!(
            reduce(
                r#"<base href="https://a.example.com/"><base href="https://b.example.com/"><a href="x">l</a>"#
            ),
            "l (https://a.example.com/x)"
        );
        // Without one, the URL the document was read from still governs.
        assert_eq!(
            reduce(r#"<a href="x.html">link</a>"#),
            "link (https://docs.example.com/guide/x.html)"
        );
    }

    #[test]
    fn elements_that_are_not_prose_are_dropped_whole() {
        // A `<template>` body is material for scripts this tool never runs, and
        // a `<textarea>` holds a form's default value — neither is what the page
        // says. Both nest and both stay out.
        assert_eq!(reduce("<template><p>hidden</p></template><p>b</p>"), "b");
        assert_eq!(
            reduce("<template><p>a</p><template><p>deep</p></template><p>c</p></template><p>b</p>"),
            "b"
        );
        assert_eq!(reduce("<textarea><b>default</b></textarea><p>b</p>"), "b");
        // `<noscript>`, by contrast, is written for exactly this reader: one that
        // will not run the page's JavaScript.
        assert_eq!(reduce("<noscript><p>fallback</p></noscript>"), "fallback");
    }

    #[test]
    fn a_null_byte_does_not_reach_the_model() {
        // The standard turns a NUL in text into U+FFFD; prose wants neither, and
        // a raw NUL in a tool result is a hazard for whatever serializes it next.
        assert_eq!(reduce("<p>a\u{0}b</p>"), "ab");
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
        let reduced = reduce("<p>a&nbsp;&amp;&nbsp;b &#8212; c &bogus; &#x41;</p>");

        assert_eq!(reduced, "a & b — c &bogus; A");
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
    fn every_reference_the_standard_defines_resolves() {
        // Two code points, which is what 93 of the table's entries map to.
        // Keeping only the first would print `≧` for a name that means the
        // opposite of `≧` — the defect a hand-written table shipped with.
        assert_eq!(reduce("<p>&NotGreaterFullEqual;</p>"), "≧\u{338}");
        assert_eq!(reduce("<p>&NotLessLess;</p>"), "≪\u{338}");
        assert_eq!(reduce("<p>&lvertneqq;</p>"), "≨\u{fe00}");
        // The 106 names HTML5 still resolves without a semicolon, for historical
        // reasons.
        assert_eq!(reduce("<p>&AMP &copy</p>"), "& ©");
        // And the longest-match rule that follows from them: `&not` is one such
        // name, so this is not an unknown reference left verbatim — it is `¬`
        // followed by the rest of the text, exactly as a browser reads it.
        assert_eq!(reduce("<p>&notareference;</p>"), "¬areference;");
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
    fn tag_names_are_classified_whatever_their_case() {
        // The tokenizer lowercases names, which is the whole reason
        // `Element::classify` matches only lower-case spellings. If that stopped
        // holding, every upper-case tag would fall through to `Inline` and lose
        // its boundary — silently, because the text still comes out.
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
        // Any delimiter closes it, not just `>`. The stray slash in `</style/>`
        // is the one that bites: the standard routes it through the self-closing
        // state, so the end tag arrives carrying that flag, and a depth counter
        // that skips self-closing tags would leave `<style>` open forever and
        // swallow the rest of the page.
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
