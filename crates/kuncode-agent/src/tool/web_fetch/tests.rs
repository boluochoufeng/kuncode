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

use super::{MAX_BODY_BYTES, MAX_CONTENT_BYTES, MAX_SAME_ORIGIN_REDIRECTS, WebFetch, WebFetchArgs};
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

fn tool() -> Arc<WebFetch> {
    Arc::new(WebFetch::new().expect("HTTP client builds"))
}

async fn fetch(url: &str) -> ToolOutput {
    execute_for_test(
        tool(),
        serde_json::json!({ "url": url }),
        &ToolContext::new(),
    )
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
async fn both_caps_bound_the_returned_content() {
    // One body trips both: more bytes than the socket cap reads, and more text
    // than the model cap returns.
    let body = "x".repeat(MAX_BODY_BYTES + 1_024);
    let server = TestServer::new(vec![response("200 OK", "text/plain", &body)]).await;

    let output = fetch(&server.url("/big.txt")).await;

    assert!(output.ok);
    assert!(output.truncated);
    let data = output.data.expect("data present");
    assert_eq!(data["body_bytes"], MAX_BODY_BYTES);
    let content = data["content"].as_str().expect("content is a string");
    assert!(content.starts_with(&"x".repeat(MAX_CONTENT_BYTES)));
    // The marker names both caps and says a second call will not continue.
    assert!(content.contains("response body capped"));
    assert!(content.contains("text capped"));
    assert!(content.contains("fetching again will not add it"));
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
    // that the tool takes a URL and nothing that could widen its reach.
    let definition = crate::tool::definition_for::<WebFetchArgs>("web_fetch", "");
    assert_eq!(
        definition.parameters["required"],
        serde_json::json!(["url"])
    );
    assert_eq!(
        definition.parameters["properties"]["url"]["type"],
        serde_json::json!("string")
    );
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

#[test]
fn an_origin_is_what_a_permission_rule_can_be_written_about() {
    // `WebFetch(domain:…)` rules match on the host of a canonical origin, so a
    // selector this tool cannot produce would be a rule nobody could use.
    let error = crate::permission::CanonicalOrigin::new("https://")
        .expect_err("a hostless URL has no origin");
    assert!(matches!(error, PermissionTargetError::InvalidOrigin));
}
