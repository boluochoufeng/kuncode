//! Turning response bytes into text, including when the page is not UTF-8.
//!
//! A page carries no obligation to be UTF-8, and the legacy web is where the
//! documentation `web_fetch` is pointed at often lives: a GBK reference page, a
//! Shift_JIS changelog, a Latin-1 README. Read as UTF-8 those are not slightly
//! wrong, they are entirely replacement characters — the model gets `"����"` and
//! no way to tell that the page it was handed is not the page that exists.
//!
//! So the encoding is *decided* rather than assumed, following the order the HTML
//! standard prescribes, because that is the order page authors write for: a BOM
//! outranks everything, then the `Content-Type` charset, then a `<meta>`
//! declaration, and only then a guess.

use std::borrow::Cow;
use std::str;

use encoding_rs::{Encoding, UTF_8, WINDOWS_1252};

/// Bytes scanned for a `<meta>` charset declaration. The HTML standard bounds its
/// own prescan at 1024 bytes; a declaration past that is one no browser would
/// honour either, so the page is broken rather than misread here.
const META_PRESCAN_BYTES: usize = 1024;

/// The token a charset declaration is found by, in either `<meta>` spelling.
const CHARSET_TOKEN: &str = "charset";

/// Decodes `body` to text, reporting the encoding it was read as.
///
/// `declared` is the `Content-Type` charset parameter, when the server sent one.
/// Borrowing on the UTF-8 path is the point of the [`Cow`]: that is the common
/// case, and the caller usually only needs to read the text.
pub(super) fn decode<'body>(
    body: &'body [u8],
    declared: Option<&str>,
) -> (Cow<'body, str>, &'static Encoding) {
    let assumed = declared
        .and_then(label_encoding)
        .or_else(|| meta_charset(body))
        // Nothing was declared, so the bytes are the only evidence left. Valid
        // UTF-8 is taken at its word; otherwise windows-1252 is the standard's
        // own default and maps every byte, so a Latin-1 page reads correctly and
        // anything else at least keeps its ASCII instead of losing it to `�`.
        .unwrap_or_else(|| if is_utf8(body) { UTF_8 } else { WINDOWS_1252 });
    // `decode` handles a BOM itself — which is why it reports back the encoding
    // it settled on rather than echoing the argument — and substitutes U+FFFD for
    // malformed sequences instead of failing. Lossy is required, not tolerated:
    // the body cap upstream can cut a page mid-character, and partial text beats
    // no answer at all.
    let (text, used, _had_errors) = assumed.decode(body);
    (text, used)
}

/// Resolves a charset label, refusing the ones the standard maps to its
/// `replacement` encoding.
///
/// That mapping exists to stop a browser from being attacked through ISO-2022-CN
/// and friends, and it works by decoding the whole page to a single `�`. Here
/// there is no markup being rendered to attack, so inheriting that would trade a
/// readable approximation for nothing; falling through to the guess is better.
fn label_encoding(label: &str) -> Option<&'static Encoding> {
    Encoding::for_label_no_replacement(label.as_bytes())
}

/// Reports whether `body` reads as UTF-8, tolerating a sequence the body cap cut
/// in half.
///
/// `error_len()` is `None` exactly when the input ends mid-sequence, so a
/// truncated UTF-8 page is not mistaken for evidence of some other encoding.
fn is_utf8(body: &[u8]) -> bool {
    match str::from_utf8(body) {
        Ok(_) => true,
        Err(error) => error.error_len().is_none(),
    }
}

/// Finds the encoding a `<meta>` tag declares, the way a browser prescans before
/// it can decode anything.
///
/// Searching for the `charset` token instead of parsing the tag around it covers
/// both spellings at once — `<meta charset=gbk>` and the older
/// `<meta http-equiv="Content-Type" content="text/html; charset=gbk">` — and a
/// stray occurrence in prose costs nothing, because it only counts if what
/// follows is `=` and a label that actually names an encoding.
fn meta_charset(body: &[u8]) -> Option<&'static Encoding> {
    // A charset label is ASCII wherever it appears, so scanning a lossy view of
    // still-undecoded bytes cannot lose one.
    let prescan =
        String::from_utf8_lossy(&body[..body.len().min(META_PRESCAN_BYTES)]).to_ascii_lowercase();

    let mut rest = prescan.as_str();
    while let Some(found) = rest.find(CHARSET_TOKEN) {
        rest = &rest[found + CHARSET_TOKEN.len()..];
        let Some(value) = rest.trim_start().strip_prefix('=') else {
            continue;
        };
        let value = value.trim_start();
        let value = value.strip_prefix(['"', '\'']).unwrap_or(value);
        let end = value
            .find(|character: char| {
                matches!(character, '"' | '\'' | ';' | '>' | ',') || character.is_ascii_whitespace()
            })
            .unwrap_or(value.len());
        if let Some(encoding) = label_encoding(&value[..end]) {
            return Some(encoding);
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 中文内容 in GBK, the case that made this module necessary.
    const GBK_TEXT: &[u8] = &[0xd6, 0xd0, 0xce, 0xc4, 0xc4, 0xda, 0xc8, 0xdd];

    fn text(body: &[u8], declared: Option<&str>) -> (String, &'static str) {
        let (text, encoding) = decode(body, declared);
        (text.into_owned(), encoding.name())
    }

    #[test]
    fn a_declared_charset_decodes_the_page_it_describes() {
        assert_eq!(text(GBK_TEXT, Some("gbk")), ("中文内容".to_string(), "GBK"));
    }

    #[test]
    fn a_charset_label_is_matched_the_way_the_web_spells_it() {
        // `gb2312` is an alias for GBK, and labels arrive in any case.
        for label in ["GB2312", "gb_2312-80", "CSGB2312"] {
            assert_eq!(text(GBK_TEXT, Some(label)).0, "中文内容", "label {label}");
        }
        // An encoding nobody implements leaves the guess in charge.
        assert_eq!(text(b"plain", Some("ebcdic-cp-us")).1, "UTF-8");
    }

    #[test]
    fn a_meta_declaration_stands_in_for_a_missing_header() {
        let mut page = br#"<html><head><meta charset="gbk"><title>"#.to_vec();
        page.extend_from_slice(GBK_TEXT);
        page.extend_from_slice(b"</title></head></html>");

        let (decoded, encoding) = text(&page, None);
        assert_eq!(encoding, "GBK");
        assert!(decoded.contains("中文内容"), "decoded: {decoded}");
    }

    #[test]
    fn the_older_http_equiv_spelling_is_found_too() {
        let page = br#"<meta http-equiv="Content-Type" content="text/html; charset=Shift_JIS">"#;
        assert_eq!(text(page, None).1, "Shift_JIS");
    }

    #[test]
    fn a_declaration_past_the_prescan_window_is_not_honoured() {
        let mut page = b"<!-- ".to_vec();
        page.resize(META_PRESCAN_BYTES, b' ');
        page.extend_from_slice(br#" --><meta charset="gbk">"#);
        assert_eq!(text(&page, None).1, "UTF-8");
    }

    #[test]
    fn the_word_charset_in_prose_does_not_decide_anything() {
        let page = "The charset is whatever the server says; charset=??? is not a label.";
        assert_eq!(text(page.as_bytes(), None).1, "UTF-8");
    }

    #[test]
    fn a_header_charset_outranks_a_meta_one() {
        // Both are the page author's claim, but the header is the fresher one and
        // the standard prefers it — a CDN transcoding on the fly only updates it.
        let mut page = br#"<meta charset="utf-8">"#.to_vec();
        page.extend_from_slice(GBK_TEXT);
        assert_eq!(text(&page, Some("gbk")).1, "GBK");
    }

    #[test]
    fn a_bom_outranks_every_declaration() {
        let mut page = vec![0xef, 0xbb, 0xbf];
        page.extend_from_slice("中文内容".as_bytes());
        assert_eq!(
            text(&page, Some("gbk")),
            ("中文内容".to_string(), "UTF-8"),
            "a UTF-8 BOM means the header is stale"
        );
    }

    #[test]
    fn undeclared_utf8_survives_being_cut_mid_character() {
        // What the body cap does to a page: the last character loses its tail.
        // Guessing windows-1252 here would mangle every line above the cut.
        let whole = "中文内容".as_bytes();
        let cut = &whole[..whole.len() - 1];

        let (decoded, encoding) = text(cut, None);
        assert_eq!(encoding, "UTF-8");
        assert!(decoded.starts_with("中文内"), "decoded: {decoded}");
    }

    #[test]
    fn undeclared_latin1_keeps_its_letters_instead_of_losing_them() {
        // `caf\xe9` is not UTF-8 and says so, so the guess falls to windows-1252
        // and the word comes back readable rather than as `caf�`.
        assert_eq!(
            text(b"caf\xe9 na\xefve", None),
            ("café naïve".to_string(), "windows-1252")
        );
    }

    #[test]
    fn plain_ascii_is_borrowed_rather_than_transcoded() {
        let (text, encoding) = decode(b"just ascii", None);
        assert!(matches!(text, Cow::Borrowed(_)), "decoding copied a page");
        assert_eq!(encoding.name(), "UTF-8");
    }

    #[test]
    fn an_empty_body_decodes_to_empty_text() {
        assert_eq!(text(b"", None), (String::new(), "UTF-8"));
    }
}
