//! `web_fetch` behavior against a loopback server.
//!
//! Every test here also stands for the guard's one deliberate exception: the
//! server binds `127.0.0.1`, so a suite that passes proves a local dev server
//! stays fetchable while [`super::address`] refuses the rest of the internal
//! network.

use std::net::SocketAddr;
use std::sync::Arc;

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::task::JoinHandle;

use super::{
    MAX_BODY_BYTES, MAX_CONTENT_BYTES, MAX_SAME_ORIGIN_REDIRECTS, WebFetch, WebFetchArgs, paginate,
};
use crate::permission::{PermissionTarget, PermissionTargetError};
use crate::tool::{PreparationContext, Tool, ToolContext, ToolOutput, execute_for_test};

/// Serves one canned response per connection, in order.
///
/// Hand-rolled rather than pulled in as a dev-dependency: the tool needs a real
/// socket to exercise its resolver, redirect policy, and body cap, but nothing
/// here needs routing or HTTP correctness beyond a well-formed response.
struct TestServer {
    address: SocketAddr,
    served: JoinHandle<()>,
}

impl TestServer {
    async fn new(responses: Vec<Vec<u8>>) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0")
            .await
            .expect("loopback port binds");
        let address = listener.local_addr().expect("bound socket has an address");
        let served = tokio::spawn(async move {
            for response in responses {
                let Ok((mut stream, _)) = listener.accept().await else {
                    return;
                };
                drain_request(&mut stream).await;
                // A client that stopped at the body cap leaves the write short;
                // that is the case under test, not a failure.
                let _ = stream.write_all(&response).await;
                let _ = stream.flush().await;
            }
        });
        Self { address, served }
    }

    fn url(&self, path: &str) -> String {
        format!("http://{}{path}", self.address)
    }
}

impl Drop for TestServer {
    fn drop(&mut self) {
        self.served.abort();
    }
}

/// Reads the request head so the client's write completes before the response.
async fn drain_request(stream: &mut TcpStream) {
    let mut request = Vec::new();
    let mut buffer = [0u8; 512];
    while !request.windows(4).any(|window| window == b"\r\n\r\n") {
        match stream.read(&mut buffer).await {
            Ok(0) | Err(_) => return,
            Ok(read) => request.extend_from_slice(&buffer[..read]),
        }
    }
}

/// Builds a response whose `Connection: close` keeps one request per connection,
/// so [`TestServer`] can hand out its canned responses in order.
///
/// The body is bytes rather than text because a page is: the charset tests serve
/// GBK, which no `String` can hold.
fn response(status: &str, content_type: &str, body: impl AsRef<[u8]>) -> Vec<u8> {
    let body = body.as_ref();
    let mut message = format!(
        "HTTP/1.1 {status}\r\nContent-Type: {content_type}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
        body.len()
    )
    .into_bytes();
    message.extend_from_slice(body);
    message
}

fn redirect(status: &str, location: &str) -> Vec<u8> {
    format!(
        "HTTP/1.1 {status}\r\nLocation: {location}\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
    )
    .into_bytes()
}

/// Serves `payload` under `Content-Encoding: gzip`, which the client only asks
/// for — and only undoes — while the `gzip` feature is on.
fn gzipped_response(content_type: &str, payload: &str) -> Vec<u8> {
    let body = gzip_frame(payload.as_bytes());
    let mut message = format!(
        "HTTP/1.1 200 OK\r\nContent-Type: {content_type}\r\nContent-Encoding: gzip\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
        body.len()
    )
    .into_bytes();
    message.extend_from_slice(&body);
    message
}

/// Wraps `payload` in a gzip frame that stores it rather than compressing it.
///
/// DEFLATE's stored block needs no encoder, so this fixture stays readable — the
/// payload sits verbatim in the bytes — while still being a frame the client has
/// to unwrap. What is under test is the client's side of the exchange, not a
/// compression ratio.
fn gzip_frame(payload: &[u8]) -> Vec<u8> {
    let length = u16::try_from(payload.len()).expect("fixture payload fits one stored block");
    let mut framed = vec![0x1f, 0x8b, 0x08, 0, 0, 0, 0, 0, 0, 0xff];
    framed.push(0x01); // Final block, stored.
    framed.extend_from_slice(&length.to_le_bytes());
    framed.extend_from_slice(&(!length).to_le_bytes());
    framed.extend_from_slice(payload);
    // A decoder rejects the frame outright if either trailer is wrong.
    framed.extend_from_slice(&crc32(payload).to_le_bytes());
    framed.extend_from_slice(&u32::from(length).to_le_bytes());
    framed
}

fn crc32(data: &[u8]) -> u32 {
    let mut crc = !0u32;
    for byte in data {
        crc ^= u32::from(*byte);
        for _ in 0..8 {
            crc = if crc & 1 == 1 {
                (crc >> 1) ^ 0xEDB8_8320
            } else {
                crc >> 1
            };
        }
    }
    !crc
}

fn tool() -> Arc<WebFetch> {
    Arc::new(WebFetch::new().expect("HTTP client builds"))
}

async fn fetch(url: &str) -> ToolOutput {
    fetch_with(serde_json::json!({ "url": url })).await
}

async fn fetch_with(arguments: serde_json::Value) -> ToolOutput {
    execute_for_test(tool(), arguments, &ToolContext::new())
        .await
        .expect("no harness-level error")
}

#[tokio::test]
async fn plain_text_comes_back_verbatim() {
    let server =
        TestServer::new(vec![response("200 OK", "text/plain", "hello from kuncode")]).await;

    let output = fetch(&server.url("/notes.txt")).await;

    assert!(output.ok);
    assert!(!output.truncated);
    let data = output.data.expect("data present");
    assert_eq!(data["content"], "hello from kuncode");
    assert_eq!(data["status"], 200);
    assert_eq!(data["content_type"], "text/plain");
    assert_eq!(data["reduced_from_html"], false);
    assert_eq!(data["body_bytes"], 18);
}

#[tokio::test]
async fn html_is_reduced_to_readable_text() {
    let server = TestServer::new(vec![response(
        "200 OK",
        "text/html; charset=utf-8",
        "<html><head><title>Guide</title></head><body><p>Install <b>kuncode</b>.</p>\
         <script>track()</script></body></html>",
    )])
    .await;

    let output = fetch(&server.url("/guide")).await;

    assert!(output.ok);
    let data = output.data.expect("data present");
    assert_eq!(data["reduced_from_html"], true);
    // Parameters are dropped from the reported type; the charset is not content.
    assert_eq!(data["content_type"], "text/html");
    assert_eq!(data["content"], "# Guide\n\nInstall kuncode.");
}

#[tokio::test]
async fn a_mislabeled_html_page_is_still_reduced() {
    // Docs sites and raw-file hosts routinely serve HTML as `text/plain`, so the
    // decision cannot rest on the header alone.
    let server = TestServer::new(vec![response(
        "200 OK",
        "text/plain",
        "<!DOCTYPE html><html><body><h1>Title</h1></body></html>",
    )])
    .await;

    let output = fetch(&server.url("/page")).await;

    let data = output.data.expect("data present");
    assert_eq!(data["reduced_from_html"], true);
    assert_eq!(data["content"], "# Title");
}

#[tokio::test]
async fn a_page_that_is_not_utf8_is_decoded_rather_than_mangled() {
    // The header names the encoding, so the whole path — decode, then reduce the
    // HTML — has to run on GBK bytes and still produce the text the page says.
    let mut page = br#"<html><head><meta charset="gbk"><title>"#.to_vec();
    page.extend_from_slice(&[0xce, 0xc4, 0xb5, 0xb5]); // 文档
    page.extend_from_slice(b"</title></head><body><p>");
    page.extend_from_slice(&[0xd6, 0xd0, 0xce, 0xc4, 0xc4, 0xda, 0xc8, 0xdd]); // 中文内容
    page.extend_from_slice(b"</p></body></html>");
    let body_bytes = page.len();
    let server = TestServer::new(vec![response("200 OK", "text/html; charset=gbk", page)]).await;

    let output = fetch(&server.url("/doc")).await;

    assert!(output.ok);
    let data = output.data.expect("data present");
    assert_eq!(data["encoding"], "GBK");
    assert_eq!(data["content"], "# 文档\n\n中文内容");
    // The reported size is what came off the socket, not what decoding grew it
    // to: three bytes per character in UTF-8 against two in GBK.
    assert_eq!(data["body_bytes"], body_bytes);
}

#[tokio::test]
async fn a_page_that_declares_nothing_falls_back_to_its_own_meta_tag() {
    // Plenty of legacy pages send a bare `text/html` and put the only truth about
    // their encoding in the markup.
    let mut page = br#"<html><head><meta charset="gbk"></head><body>"#.to_vec();
    page.extend_from_slice(&[0xd6, 0xd0, 0xce, 0xc4]); // 中文
    page.extend_from_slice(b"</body></html>");
    let server = TestServer::new(vec![response("200 OK", "text/html", page)]).await;

    let output = fetch(&server.url("/legacy")).await;

    let data = output.data.expect("data present");
    assert_eq!(data["encoding"], "GBK");
    assert_eq!(data["content"], "中文");
}

#[tokio::test]
async fn a_utf8_page_reports_the_encoding_it_was_read_as() {
    let server = TestServer::new(vec![response("200 OK", "text/plain", "plain ascii")]).await;

    let output = fetch(&server.url("/notes.txt")).await;

    let data = output.data.expect("data present");
    assert_eq!(data["encoding"], "UTF-8");
}

#[tokio::test]
async fn a_redirect_inside_the_authorized_origin_is_followed() {
    let server = TestServer::new(vec![
        redirect("302 Found", "/final"),
        response("200 OK", "text/plain", "arrived"),
    ])
    .await;
    let requested = server.url("/start");

    let output = fetch(&requested).await;

    assert!(output.ok);
    let data = output.data.expect("data present");
    assert_eq!(data["content"], "arrived");
    // The reported URL is the one actually read, so the model is not left
    // believing the redirecting URL served this content.
    assert_eq!(data["url"], server.url("/final"));
    assert_ne!(data["url"], requested.as_str());
}

#[tokio::test]
async fn a_cross_origin_redirect_is_reported_instead_of_followed() {
    // Approval covered one origin. Following this hop would read a second origin
    // under the first one's authorization, so the model is told to ask again.
    let server = TestServer::new(vec![redirect(
        "301 Moved Permanently",
        "https://example.com/x",
    )])
    .await;

    let output = fetch(&server.url("/away")).await;

    assert!(!output.ok);
    let error = output.error.expect("error present");
    assert_eq!(error.kind.as_str(), "cross_origin_redirect");
    assert!(error.message.contains("https://example.com/x"));
    assert!(output.data.is_none());
}

#[tokio::test]
async fn a_redirect_loop_inside_one_origin_stops_at_the_hop_budget() {
    // Staying inside the authorized origin is not on its own a reason to keep
    // going, so the budget is reported as its own failure rather than as a
    // cross-origin one.
    let hops = (0..MAX_SAME_ORIGIN_REDIRECTS + 2)
        .map(|hop| redirect("302 Found", &format!("/hop{hop}")))
        .collect();
    let server = TestServer::new(hops).await;

    let output = fetch(&server.url("/start")).await;

    assert!(!output.ok);
    let error = output.error.expect("error present");
    assert_eq!(error.kind.as_str(), "too_many_redirects");
    assert!(
        error
            .message
            .contains(&MAX_SAME_ORIGIN_REDIRECTS.to_string())
    );
}

#[tokio::test]
async fn a_failing_status_returns_its_body_alongside_the_error() {
    // A 404 or 500 body is usually the explanation, so it is reported, not lost.
    let server = TestServer::new(vec![response(
        "404 Not Found",
        "application/json",
        r#"{"error":"no such page"}"#,
    )])
    .await;

    let output = fetch(&server.url("/missing")).await;

    assert!(!output.ok);
    let error = output.error.expect("error present");
    assert_eq!(error.kind.as_str(), "http_status");
    assert!(error.message.contains("404"));
    let data = output.data.expect("body still returned");
    assert_eq!(data["status"], 404);
    assert_eq!(data["content"], r#"{"error":"no such page"}"#);
}

#[tokio::test]
async fn an_empty_body_is_reported_as_empty_rather_than_as_a_failure() {
    let server = TestServer::new(vec![response("204 No Content", "text/plain", "")]).await;

    let output = fetch(&server.url("/nothing")).await;

    assert!(output.ok);
    let data = output.data.expect("data present");
    assert_eq!(data["content"], "");
    assert_eq!(data["body_bytes"], 0);
}

#[tokio::test]
async fn each_cap_says_whether_a_later_call_can_reach_what_it_withheld() {
    // One body trips both: more bytes than the socket cap reads, and more text
    // than one page returns.
    let body = "x".repeat(MAX_BODY_BYTES + 1_024);
    let server = TestServer::new(vec![response("200 OK", "text/plain", &body)]).await;

    let output = fetch(&server.url("/big.txt")).await;

    assert!(output.ok);
    // Only the body cap counts as truncation. Its bytes are gone for good, while
    // the text past this page is merely unread.
    assert!(output.truncated);
    let data = output.data.expect("data present");
    assert_eq!(data["body_bytes"], MAX_BODY_BYTES);
    assert_eq!(data["start_index"], 0);
    assert_eq!(data["next_index"], MAX_CONTENT_BYTES);
    let content = data["content"].as_str().expect("content is a string");
    assert!(content.starts_with(&"x".repeat(MAX_CONTENT_BYTES)));
    // And the marker keeps the two apart: one is an instruction, one is a loss.
    assert!(content.contains(&format!("start_index={MAX_CONTENT_BYTES}")));
    assert!(content.contains("response body cut off"));
    assert!(content.contains("no later call will reach"));
}

#[tokio::test]
async fn paging_reassembles_the_whole_page_on_line_boundaries() {
    let body = (0..300)
        .map(|index| format!("guide line {index} — what it explains"))
        .collect::<Vec<_>>()
        .join("\n");
    let page_bytes: usize = 900;
    // The loop below asserts the walk terminates on its own; the spare responses
    // only keep the server from being what stops it.
    let server = TestServer::new(vec![response("200 OK", "text/plain", &body); 24]).await;
    let url = server.url("/guide.txt");

    let mut reassembled = String::new();
    let mut next = Some(0u64);
    let mut pages = 0;
    while let Some(start_index) = next {
        let output = fetch_with(serde_json::json!({
            "url": &url,
            "start_index": start_index,
            "max_length": page_bytes,
        }))
        .await;

        assert!(output.ok);
        // Paging is not truncating: every byte stays reachable, so the flag that
        // means "content was dropped" must stay clear.
        assert!(!output.truncated);
        let data = output.data.expect("data present");
        assert_eq!(data["start_index"], start_index);
        assert_eq!(data["content_bytes"], body.len() as u64);
        let content = data["content"].as_str().expect("content is a string");
        next = data["next_index"].as_u64();

        let page = match next {
            // The marker is metadata about the page, not part of it.
            Some(_) => {
                let (page, marker) = content
                    .split_once("\n…⟨kuncode:")
                    .expect("a continued page carries the marker");
                assert!(marker.contains("start_index="));
                // A break lands just after a newline, so no line is ever split.
                assert!(page.ends_with('\n'));
                page
            }
            None => content,
        };
        assert!(page.len() <= page_bytes);
        reassembled.push_str(page);
        pages += 1;
    }

    assert!(
        pages > 3,
        "{} bytes should not fit in {pages} pages",
        body.len()
    );
    assert_eq!(reassembled, body);
}

#[tokio::test]
async fn a_start_index_past_the_end_returns_no_text_rather_than_failing() {
    let body = "short page";
    let server = TestServer::new(vec![response("200 OK", "text/plain", body)]).await;

    let output = fetch_with(serde_json::json!({
        "url": server.url("/short.txt"),
        "start_index": 5_000,
    }))
    .await;

    assert!(output.ok);
    let data = output.data.expect("data present");
    assert_eq!(data["content"], "");
    // Clamped to the end, and reported next to the real size so the model can see
    // that it overshot rather than that the page was empty.
    assert_eq!(data["start_index"], body.len() as u64);
    assert_eq!(data["content_bytes"], body.len() as u64);
    assert!(data["next_index"].is_null());
}

#[tokio::test]
async fn a_zero_max_length_is_refused_before_anything_is_dialed() {
    // No server stands behind this URL: the check belongs to preparation, so the
    // call must fail without a request.
    let output = fetch_with(serde_json::json!({
        "url": "http://127.0.0.1:1/page",
        "max_length": 0,
    }))
    .await;

    let error = output.error.expect("error present");
    assert_eq!(error.kind.as_str(), "invalid_arguments");
    assert!(error.message.contains("max_length"));
}

#[tokio::test]
async fn a_compressed_body_arrives_decompressed() {
    let page = "<html><body><p>Compressed guidance.</p></body></html>";
    let server = TestServer::new(vec![gzipped_response("text/html", page)]).await;

    let output = fetch(&server.url("/guide.html")).await;

    assert!(output.ok);
    let data = output.data.expect("data present");
    assert_eq!(data["content"], "Compressed guidance.");
    // The document's size rather than the transfer's, which is the size the caps
    // are meant to bound.
    assert_eq!(data["body_bytes"], page.len() as u64);
}

#[test]
fn a_page_ends_at_the_last_line_break_it_can_reach() {
    let (page, _, next) = paginate("first line\nsecond line\nthird line", 0, 20);

    // Twenty bytes would land inside "second line"; the page stops after the
    // newline before it instead.
    assert_eq!(page, "first line\n");
    assert_eq!(next, Some(11));
}

#[test]
fn a_page_break_with_no_line_to_land_on_still_splits_between_characters() {
    // Three-byte characters and not one newline, so the cut has to fall back to
    // bytes — and eight bytes of room holds two whole characters, not two thirds
    // of a third.
    let text = "。".repeat(10);

    let (page, start, next) = paginate(&text, 0, 8);

    assert_eq!(page, "。。");
    assert_eq!(start, 0);
    assert_eq!(next, Some(6));
}

#[test]
fn a_start_index_inside_a_character_backs_off_to_its_boundary() {
    let text = "。。。";

    let (page, start, next) = paginate(text, 4, 64);

    // Four is mid-character: reading from there would panic, so the page begins
    // at the boundary below it.
    assert_eq!(start, 3);
    assert_eq!(page, "。。");
    assert_eq!(next, None);
}

#[test]
fn no_page_width_can_split_a_character_or_lose_a_byte() {
    // One-, two-, three-, and four-byte characters, a newline for the
    // line-boundary path to find, and a combining mark. Slicing between the bytes
    // of any character would panic inside `paginate` rather than return, so a walk
    // that completes for every width proves no width finds a bad cut; the join
    // then proves `next_index` neither skipped nor repeated a byte.
    let text = "ascii\n中文 → 内容\ne\u{301}f 🌐🚀\nlast";

    for max_bytes in 1..=text.len() + 4 {
        let mut walked = String::new();
        let mut next = Some(0);
        let mut pages = 0;
        while let Some(start) = next {
            let (page, at, after) = paginate(text, start, max_bytes);
            // A `next_index` that landed inside a character would get floored back
            // here, which is the one way a walk could silently repeat bytes.
            assert_eq!(at, start, "width {max_bytes} moved a page's own start");
            walked.push_str(page);
            next = after;
            pages += 1;
            assert!(pages <= text.len(), "width {max_bytes} does not terminate");
        }
        assert_eq!(
            walked, text,
            "width {max_bytes} did not reassemble the text"
        );
    }
}

#[test]
fn a_max_length_narrower_than_one_character_still_advances() {
    // Rounding down to a boundary would leave no room at all, and a page that
    // returns nothing while repeating its `next_index` never terminates.
    let (page, _, next) = paginate("。。", 0, 1);

    assert_eq!(page, "。");
    assert_eq!(next, Some(3));
}

#[tokio::test]
async fn an_address_no_approval_could_unlock_is_refused_before_authorization() {
    for url in [
        "http://169.254.169.254/latest/meta-data/", // cloud instance metadata
        "http://[::ffff:10.0.0.1]/admin",           // IPv4-mapped private address
    ] {
        let output = fetch(url).await;

        let error = output.error.expect("error present");
        assert_eq!(error.kind.as_str(), "blocked_address", "{url}");
        // Naming the address is the point: the model must not retry it.
        assert!(
            error.message.contains("refusing to fetch internal"),
            "{url}"
        );
    }
}

#[tokio::test]
async fn only_credential_free_http_urls_are_accepted() {
    for url in [
        "file:///etc/passwd",
        "ftp://example.com/x",
        "https://user:secret@example.com/",
        "/relative/path",
        "not a url",
    ] {
        let output = fetch(url).await;

        assert!(!output.ok, "{url}");
        assert_eq!(
            output.error.expect("error present").kind.as_str(),
            "invalid_arguments",
            "{url}"
        );
    }
}

#[tokio::test]
async fn authorization_names_the_origin_and_input_drops_the_fragment() {
    let preparation = tool()
        .prepare(
            serde_json::json!({ "url": "HTTPS://Example.COM:443/docs?page=2#section" }),
            &PreparationContext::new(),
        )
        .await
        .expect("prepares");

    // One check, on the origin: a rule decides a host, not a path, and the
    // fragment never reaches the server so it must not reach authorization.
    assert_eq!(preparation.checks().iter().len(), 1);
    match preparation.checks().first().target() {
        PermissionTarget::WebFetch(origin) => assert_eq!(origin.as_str(), "https://example.com"),
        other => panic!("expected a WebFetch target, got {other:?}"),
    }
    assert_eq!(
        preparation.canonical_input().as_value()["url"],
        "https://example.com/docs?page=2"
    );
    assert_eq!(
        preparation.display().summary(),
        "Fetch URL: https://example.com/docs?page=2"
    );
}

#[test]
fn arguments_advertise_one_required_url() {
    // The schema the model sees is generated from `WebFetchArgs`, so this pins
    // that a URL is the one thing a call must supply, and that the optional rest
    // only pick which stretch of that one page comes back — nothing here widens
    // what the tool can reach.
    let definition = crate::tool::definition_for::<WebFetchArgs>("web_fetch", "");
    assert_eq!(
        definition.parameters["required"],
        serde_json::json!(["url"])
    );
    assert_eq!(
        definition.parameters["properties"]["url"]["type"],
        serde_json::json!("string")
    );
    for paging in ["start_index", "max_length"] {
        assert!(
            !definition.parameters["properties"][paging].is_null(),
            "{paging} is advertised to the model"
        );
    }
}

#[tokio::test]
#[ignore = "hits the real network; requires outbound HTTPS"]
async fn fetches_a_real_page_over_https() {
    // The loopback suite above cannot reach TLS, public DNS through the guarded
    // resolver, or markup written by anyone but this file.
    let output = fetch("https://example.com/").await;

    assert!(output.ok, "{:?}", output.error);
    let data = output.data.expect("data present");
    assert_eq!(data["status"], 200);
    assert_eq!(data["reduced_from_html"], true);
    let content = data["content"].as_str().expect("content is a string");
    assert!(content.contains("Example Domain"), "{content}");
}

#[tokio::test]
#[ignore = "hits the real network; requires outbound HTTPS"]
async fn pages_through_a_real_documentation_page() {
    // A real docs page reduces to several windows' worth of text, arrives
    // compressed, and breaks lines wherever its own markup does — none of which
    // the loopback fixtures can stand in for all at once.
    let url = "https://doc.rust-lang.org/std/vec/struct.Vec.html";

    let mut reassembled = String::new();
    let mut sizes = Vec::new();
    let mut next = Some(0u64);
    while let Some(start_index) = next {
        let output =
            fetch_with(serde_json::json!({ "url": url, "start_index": start_index })).await;

        assert!(output.ok, "page at {start_index}: {:?}", output.error);
        let data = output.data.expect("data present");
        assert_eq!(data["start_index"], start_index);
        let content = data["content"].as_str().expect("content is a string");
        next = data["next_index"].as_u64();
        sizes.push(data["content_bytes"].as_u64().expect("size reported"));

        reassembled.push_str(match next {
            Some(_) => {
                content
                    .split_once("\n…⟨kuncode:")
                    .expect("a continued page carries the marker")
                    .0
            }
            None => content,
        });
    }

    assert!(
        sizes.len() > 1,
        "this page now fits one window; pick another"
    );
    // Every call re-fetches and re-reduces, so a differing size would mean the
    // walk stitched together two different versions of the page.
    assert!(sizes.windows(2).all(|pair| pair[0] == pair[1]), "{sizes:?}");
    assert_eq!(Some(reassembled.len() as u64), sizes.first().copied());
    assert!(reassembled.contains("A contiguous growable array type"));
}

#[test]
fn an_origin_is_what_a_permission_rule_can_be_written_about() {
    // `WebFetch(domain:…)` rules match on the host of a canonical origin, so a
    // selector this tool cannot produce would be a rule nobody could use.
    let error = crate::permission::CanonicalOrigin::new("https://")
        .expect_err("a hostless URL has no origin");
    assert!(matches!(error, PermissionTargetError::InvalidOrigin));
}
