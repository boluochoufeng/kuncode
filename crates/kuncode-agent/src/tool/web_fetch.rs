//! The `web_fetch` tool: read one HTTP(S) URL as text.
//!
//! Two boundaries shape everything here, and both exist because a fetched page
//! is untrusted input that goes on to steer the agent:
//!
//! - **Which address may be dialed.** The private `address` module vets the
//!   resolved address, not the hostname, keeping the tool off cloud metadata and
//!   private networks. This client's system proxy discovery is disabled so every
//!   target still passes through that resolver.
//! - **Which origin was authorized.** A call is approved for one
//!   [`CanonicalOrigin`], so a redirect that leaves it is reported back instead
//!   of followed — the model can ask again for the new origin, and the user
//!   decides that separately.
//!
//! What comes back is bounded and lossy on purpose: the body is capped as it is
//! read, decoded to text by the private `charset` module, HTML is reduced to
//! prose by the private `html_text` module, and only a window of that text goes
//! to the model. The two bounds differ in kind. The window is a view — the rest
//! of the text is a `start_index` away — while the body cap discards bytes for
//! good, so the marker appended to the content distinguishes them.

// Imported anonymously for `source()`: the cause chain is where a transport
// failure actually explains itself.
use std::error::Error as _;
use std::net::IpAddr;
use std::time::Duration;

use async_trait::async_trait;
use kuncode_core::completion::ToolDefinition;
use kuncode_core::non_empty_vec::NonEmptyVec;
use reqwest::header::{ACCEPT, CONTENT_TYPE, HeaderMap, HeaderValue, LOCATION, USER_AGENT};
use reqwest::{Client, Response, StatusCode, redirect};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use thiserror::Error;
use url::{Host, Url};

use crate::{
    permission::{
        CanonicalOrigin, CanonicalToolInput, PermissionCheckSpec, PermissionTarget, ToolDisplay,
    },
    tool::{
        PreparationContext, ToolContext, ToolErrorKind, ToolErrorPayload, ToolOutput,
        TypedPreparation, TypedTool, definition_for, output::floor_char_boundary,
    },
};

mod address;
mod charset;
mod html_text;

/// Bound on dialing the host.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);
/// Total deadline, connect through full body. Unlike a provider stream, a page
/// has a known end, so a total deadline is the right shape: a server that
/// dribbles bytes forever must not hold the agent loop open.
const REQUEST_TIMEOUT: Duration = Duration::from_secs(30);
/// Cap on what is read off the socket, applied while reading so an unbounded or
/// hostile body is never buffered in full. Compression does not slip past it:
/// `chunk()` yields decompressed bytes, so an archive that inflates without bound
/// still stops here rather than after expanding.
const MAX_BODY_BYTES: usize = 2 * 1024 * 1024;
/// Cap on the text one call hands to the model, after HTML reduction, and the
/// default window size. Text beyond it is withheld rather than lost: `next_index`
/// resumes there. The price is a fresh fetch and reduction per call, this tool
/// holding no state between them.
const MAX_CONTENT_BYTES: usize = 50_000;
/// Redirect hops followed within the authorized origin.
const MAX_SAME_ORIGIN_REDIRECTS: usize = 5;
/// Leading characters sniffed for markup when the server's `Content-Type` is
/// missing or wrong, which is common enough to be worth handling.
const HTML_SNIFF_CHARS: usize = 512;

const FETCH_USER_AGENT: &str = "kuncode-web-fetch/1.0";
/// Nudges content negotiation toward documents rather than asset bundles.
const FETCH_ACCEPT: &str = "text/html,text/plain,text/markdown,application/json,*/*;q=0.5";

const DESCRIPTION: &str = "Fetch one http(s) URL and return its content as text. \
HTML is reduced to readable text (scripts, styles, and tags stripped, whitespace collapsed); \
JSON, plain text, and markdown come back verbatim. Use it to read documentation, API \
responses, or files hosted outside the workspace. A page too long for one reply comes back in \
stretches: pass the `next_index` it returns back as `start_index` to read on. Fetches only the \
URL given: a redirect to another origin is reported instead of followed, so call again with \
that URL to follow it.";

/// Arguments accepted by the [`WebFetch`] tool.
#[derive(Debug, Deserialize, JsonSchema)]
pub struct WebFetchArgs {
    /// Absolute URL to fetch, beginning with `http://` or `https://`.
    url: String,
    /// Byte offset into the page text to start returning from. Defaults to `0`
    /// (the beginning). Feed back the `next_index` from a previous result to read
    /// the next stretch of a page too long to return at once.
    #[serde(default)]
    start_index: Option<usize>,
    /// Maximum number of bytes of page text to return.
    #[serde(default)]
    max_length: Option<usize>,
}

/// Text content read from one URL.
#[derive(Debug, Serialize)]
pub struct WebFetchOutput {
    /// URL actually read, which differs from the requested one only after a
    /// redirect within the same origin.
    pub url: String,
    /// Status code the server returned.
    pub status: u16,
    /// `Content-Type` with its parameters dropped, when the server sent one.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub content_type: Option<String>,
    /// Bytes of body read, counted after transfer decompression and before any
    /// extraction, so this is the size of the document rather than of the transfer.
    pub body_bytes: usize,
    /// Encoding the body was read as. Anything other than `UTF-8` means the text
    /// was transcoded, so a page that misdeclares its charset can still read as
    /// mojibake even though the fetch itself succeeded.
    pub encoding: String,
    /// `true` when the body was HTML reduced to text. That reduction is lossy:
    /// markup, attributes, and script bodies are gone, so this content answers
    /// questions about what the page *says*, never about how it is built.
    pub reduced_from_html: bool,
    /// The stretch of page text this call returns.
    pub content: String,
    /// Byte offset of [`Self::content`] within the whole page text.
    pub start_index: usize,
    /// Size of the whole page text, so the share still unread is visible without
    /// fetching it. Unlike `read_file`, which would have to read a whole file to
    /// count its lines, this tool already holds the entire text before slicing.
    pub content_bytes: usize,
    /// Byte offset to pass back as `start_index` to continue where this call left
    /// off. Present only while text remains.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub next_index: Option<usize>,
}

/// Authorized URL paired with the validated window of text to return.
#[derive(Debug)]
pub struct PreparedWebFetch {
    url: Url,
    start_index: usize,
    max_length: usize,
}

/// Reads one URL over HTTP(S).
#[derive(Debug)]
pub struct WebFetch {
    definition: ToolDefinition,
    client: Client,
}

/// The `web_fetch` HTTP client could not be configured.
#[derive(Debug, Error)]
pub enum WebFetchError {
    /// TLS or resolver setup failed while building the client.
    #[error("failed to configure the web_fetch HTTP client: {0}")]
    Client(#[from] reqwest::Error),
}

impl WebFetch {
    /// Builds the tool and its HTTP client.
    ///
    /// # Errors
    /// Returns [`WebFetchError::Client`] when the HTTP client cannot be built,
    /// which surfaces a broken TLS backend at startup rather than per call.
    pub fn new() -> Result<Self, WebFetchError> {
        let mut headers = HeaderMap::new();
        headers.insert(USER_AGENT, HeaderValue::from_static(FETCH_USER_AGENT));
        headers.insert(ACCEPT, HeaderValue::from_static(FETCH_ACCEPT));
        let client = Client::builder()
            .connect_timeout(CONNECT_TIMEOUT)
            .timeout(REQUEST_TIMEOUT)
            .default_headers(headers)
            .no_proxy()
            .dns_resolver(address::GuardedResolver::new())
            .redirect(same_origin_redirects())
            .build()?;
        Ok(Self {
            definition: definition_for::<WebFetchArgs>("web_fetch", DESCRIPTION),
            client,
        })
    }
}

#[async_trait]
impl TypedTool for WebFetch {
    type Args = WebFetchArgs;
    type Prepared = PreparedWebFetch;
    type Output = WebFetchOutput;

    fn definition(&self) -> &ToolDefinition {
        &self.definition
    }

    async fn prepare_typed(
        &self,
        args: WebFetchArgs,
        _canonical_input: CanonicalToolInput,
        _ctx: &PreparationContext,
    ) -> Result<TypedPreparation<Self::Prepared>, ToolOutput> {
        let mut url = Url::parse(args.url.trim()).map_err(|error| {
            ToolOutput::failure(
                ToolErrorKind::InvalidArguments,
                format!("`url` must be an absolute http(s) URL: {error}"),
            )
        })?;
        // A fragment never reaches the server, so keeping it would let two
        // fetches of one document present authorization different input.
        url.set_fragment(None);
        // Also what rejects a non-http scheme, a missing host, and embedded
        // credentials: an origin exists for exactly the URLs this tool may read.
        let origin = CanonicalOrigin::new(url.as_str()).map_err(|error| {
            ToolOutput::failure(
                ToolErrorKind::InvalidArguments,
                format!("`url` must be an absolute credential-free http(s) URL: {error}"),
            )
        })?;
        // A URL naming an address literally is settled before approval: no
        // decision the user could make would make it fetchable, and unlike a
        // hostname it needs no lookup, so preparation stays free of I/O.
        if let Some(address) = literal_address(&url)
            && address::is_blocked(address)
        {
            return Err(ToolOutput::failure(
                "blocked_address",
                format!("refusing to fetch internal address {address}"),
            ));
        }

        if matches!(args.max_length, Some(0)) {
            return Err(ToolOutput::failure(
                ToolErrorKind::InvalidArguments,
                "`max_length` must be greater than zero",
            ));
        }
        let start_index = args.start_index.unwrap_or(0);
        // Asking for more than one page yields a page anyway, matching how
        // `read_file` lets `limit` exceed its own byte budget: the window is
        // whatever fits, and `next_index` carries the rest.
        let max_length = args
            .max_length
            .unwrap_or(MAX_CONTENT_BYTES)
            .min(MAX_CONTENT_BYTES);

        let canonical_input = CanonicalToolInput::new(serde_json::json!({
            "url": url.as_str(),
            "start_index": start_index,
            "max_length": max_length,
        }));
        let display = ToolDisplay::new(if start_index == 0 {
            format!("Fetch URL: {url}")
        } else {
            format!("Fetch URL: {url} (from byte {start_index})")
        });
        Ok(TypedPreparation::new(
            PreparedWebFetch {
                url,
                start_index,
                max_length,
            },
            canonical_input,
            NonEmptyVec::new(PermissionCheckSpec::new(PermissionTarget::WebFetch(origin))),
            display,
        ))
    }

    async fn run_prepared(
        &self,
        prepared: PreparedWebFetch,
        _ctx: &ToolContext,
    ) -> ToolOutput<WebFetchOutput> {
        let PreparedWebFetch {
            url,
            start_index,
            max_length,
        } = prepared;
        let mut response = match self.client.get(url.clone()).send().await {
            Ok(response) => response,
            Err(error) => return transport_failure(&url, &error),
        };

        let status = response.status();
        let fetched = response.url().clone();
        let content_type = response
            .headers()
            .get(CONTENT_TYPE)
            .and_then(|value| value.to_str().ok())
            .map(ContentType::parse)
            .unwrap_or_default();

        // The redirect policy stopped rather than followed, so the hop it
        // reports is one this call was never authorized to make.
        if status.is_redirection()
            && let Some(target) = redirect_target(&response, &fetched)
        {
            return redirect_refusal(&url, &target);
        }

        let (body, body_truncated) = match read_body(&mut response).await {
            Ok(body) => body,
            Err(error) => return transport_failure(&fetched, &error),
        };

        let (decoded, encoding) = charset::decode(&body, content_type.charset.as_deref());
        let reduced_from_html = content_type
            .media_type
            .as_deref()
            .is_some_and(|value| value.contains("html"))
            || looks_like_html(&decoded);
        // Link targets resolve against the URL actually read, not the requested
        // one, so a relative target on a redirected page still comes out as a
        // URL the model can hand straight back.
        let text = if reduced_from_html {
            html_text::html_to_text(&decoded, &fetched)
        } else {
            decoded.into_owned()
        };

        let (page, start_index, next_index) = paginate(&text, start_index, max_length);
        let mut content = page.to_string();
        if let Some(note) = truncation_note(body_truncated, next_index) {
            content.push_str(&note);
        }

        ToolOutput {
            ok: status.is_success(),
            data: Some(WebFetchOutput {
                url: fetched.to_string(),
                status: status.as_u16(),
                content_type: content_type.media_type,
                body_bytes: body.len(),
                encoding: encoding.name().to_string(),
                reduced_from_html,
                content,
                start_index,
                content_bytes: text.len(),
                next_index,
            }),
            // The body of a failing response usually explains it, so it is
            // returned alongside the error rather than dropped.
            error: (!status.is_success()).then(|| http_status_error(&fetched, status)),
            // Only the body cap loses content for good. Text past the end of a
            // page is not truncated, just unread: `next_index` reaches it.
            truncated: body_truncated,
        }
    }
}

/// Follows redirects only within the origin the call was authorized for, and
/// only for a bounded number of hops.
///
/// Stopping surfaces the 3xx response itself, which is how [`redirect_refusal`]
/// can name the URL the model should ask for next.
fn same_origin_redirects() -> redirect::Policy {
    redirect::Policy::custom(|attempt| {
        // `previous` starts with the URL this call authorized, so comparing
        // against its first entry needs no per-request policy.
        let authorized = attempt
            .previous()
            .first()
            .is_some_and(|first| first.origin() == attempt.url().origin());
        let within_budget = attempt.previous().len() <= MAX_SAME_ORIGIN_REDIRECTS;
        if authorized && within_budget {
            attempt.follow()
        } else {
            attempt.stop()
        }
    })
}

/// Reads the response body, stopping at [`MAX_BODY_BYTES`], and reports whether
/// content was left unread.
async fn read_body(response: &mut Response) -> Result<(Vec<u8>, bool), reqwest::Error> {
    let mut body = Vec::new();
    while let Some(chunk) = response.chunk().await? {
        if body.len() + chunk.len() > MAX_BODY_BYTES {
            let room = MAX_BODY_BYTES.saturating_sub(body.len());
            body.extend_from_slice(&chunk[..room]);
            return Ok((body, true));
        }
        body.extend_from_slice(&chunk);
    }
    Ok((body, false))
}

/// Resolves the `Location` of a stopped redirect against the URL that produced
/// it, since the header is allowed to be relative.
fn redirect_target(response: &Response, fetched: &Url) -> Option<Url> {
    let location = response.headers().get(LOCATION)?.to_str().ok()?;
    fetched.join(location).ok()
}

fn redirect_refusal(url: &Url, target: &Url) -> ToolOutput<WebFetchOutput> {
    if url.origin() == target.origin() {
        ToolOutput::failure(
            "too_many_redirects",
            format!(
                "`{url}` redirected more than {MAX_SAME_ORIGIN_REDIRECTS} times within its origin, last to `{target}`"
            ),
        )
    } else {
        ToolOutput::failure(
            "cross_origin_redirect",
            format!(
                "`{url}` redirects to `{target}`, which is a different origin than this call was \
                 authorized for. Call web_fetch again with that URL to read it."
            ),
        )
    }
}

fn http_status_error(fetched: &Url, status: StatusCode) -> ToolErrorPayload {
    ToolErrorPayload {
        kind: "http_status".into(),
        message: format!("`{fetched}` returned HTTP {status}"),
    }
}

fn transport_failure(url: &Url, error: &reqwest::Error) -> ToolOutput<WebFetchOutput> {
    if error.is_timeout() {
        return ToolOutput::failure(
            "timeout",
            format!(
                "`{url}` did not complete within {} seconds",
                REQUEST_TIMEOUT.as_secs()
            ),
        );
    }
    ToolOutput::failure(
        "fetch",
        format!("failed to fetch `{url}`: {}", describe(error)),
    )
}

/// Renders a transport failure with its cause chain. `reqwest`'s own message
/// names little more than the URL, while the reason — a refused address, an
/// untrusted certificate, a reset connection — sits one or more sources deeper.
fn describe(error: &reqwest::Error) -> String {
    let mut described = error.to_string();
    let mut next = error.source();
    while let Some(cause) = next {
        described.push_str(": ");
        described.push_str(&cause.to_string());
        next = cause.source();
    }
    described
}

fn literal_address(url: &Url) -> Option<IpAddr> {
    match url.host()? {
        Host::Ipv4(address) => Some(IpAddr::V4(address)),
        Host::Ipv6(address) => Some(IpAddr::V6(address)),
        Host::Domain(_) => None,
    }
}

/// The two things `web_fetch` acts on in a `Content-Type`: the media type decides
/// whether the body is reduced as HTML, and the charset decides how it is decoded.
///
/// Both are optional independently — a server may send a bare media type, a
/// charset on a type this tool makes nothing of, or a header that parses to
/// neither — so the default is "the server told us nothing" rather than a guess.
#[derive(Debug, Default)]
struct ContentType {
    media_type: Option<String>,
    charset: Option<String>,
}

impl ContentType {
    fn parse(value: &str) -> Self {
        let (media_type, parameters) = value.split_once(';').unwrap_or((value, ""));
        let media_type = media_type.trim().to_ascii_lowercase();
        Self {
            media_type: (!media_type.is_empty()).then_some(media_type),
            charset: parameters
                .split(';')
                .filter_map(|parameter| parameter.split_once('='))
                .find(|(name, _)| name.trim().eq_ignore_ascii_case("charset"))
                // A parameter value is allowed to be quoted (RFC 9110), and the
                // label itself never contains a quote.
                .map(|(_, label)| label.trim().trim_matches(['"', '\'']).to_string())
                .filter(|label| !label.is_empty()),
        }
    }
}

/// Detects markup in the leading characters, so a server that mislabels an HTML
/// page as `text/plain` still gets it reduced.
fn looks_like_html(text: &str) -> bool {
    let head = text
        .chars()
        .take(HTML_SNIFF_CHARS)
        .collect::<String>()
        .to_ascii_lowercase();
    head.contains("<!doctype html") || head.contains("<html")
}

/// Slices one page out of `text`: at most `max_bytes` from `start`, cut at a line
/// boundary whenever the window contains one.
///
/// Returns the page, the offset it really starts at, and the offset to resume
/// from — `None` once nothing follows.
fn paginate(text: &str, start: usize, max_bytes: usize) -> (&str, usize, Option<usize>) {
    let start = floor_char_boundary(text, start);
    let rest = &text[start..];
    if rest.len() <= max_bytes {
        return (rest, start, None);
    }

    let room = match floor_char_boundary(rest, max_bytes) {
        // A `max_length` narrower than the first character floors to zero, which
        // would return an empty page and repeat the same `next_index` forever.
        // Overshooting by that one character keeps every page advancing.
        0 => rest.chars().next().map_or(0, char::len_utf8),
        room => room,
    };
    // Cutting after the last newline carries whole lines across a page break, so
    // a table row or a code line is never split down the middle. A window holding
    // no newline at all — a single-line JSON body, a minified page — takes the
    // plain byte cut, which is what keeps such content paginable instead of
    // stalling on one unbreakable line.
    let end = rest[..room].rfind('\n').map_or(room, |newline| newline + 1);
    (&rest[..end], start, Some(start + end))
}

/// Builds the marker that tells the model what it is not being shown, and which
/// of the two caps withheld it.
///
/// The distinction is the point: text past the end of a page is one more call
/// away, while the body cap stopped bytes that were never downloaded and that a
/// re-fetch would stop again in the same place.
fn truncation_note(body_truncated: bool, next_index: Option<usize>) -> Option<String> {
    let mut notes = Vec::new();
    if let Some(next_index) = next_index {
        notes.push(format!(
            "text continues — call web_fetch again with start_index={next_index}"
        ));
    }
    if body_truncated {
        notes.push(format!(
            "response body cut off at {MAX_BODY_BYTES} bytes, which no later call will reach"
        ));
    }
    if notes.is_empty() {
        return None;
    }
    Some(format!("\n…⟨kuncode: {}⟩", notes.join("; ")))
}

#[cfg(test)]
mod tests;
