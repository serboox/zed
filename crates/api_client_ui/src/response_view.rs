use api_client::ParsedCookie;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ResponseTab {
    Pretty,
    Preview,
    Raw,
    Headers,
    Cookies,
    Diff,
    TestResults,
    Visualize,
}

/// A completed HTTP exchange's status/timing/headers/body/cookies, in the
/// shape the response area renders directly -- kept separate from
/// `api_client::HttpResponseSummary` so the UI can add derived fields
/// (`size_bytes`, parsed `cookies`) without api_client needing to know about
/// them.
#[derive(Debug, Clone)]
pub struct ResponseData {
    pub status: u16,
    pub status_text: String,
    pub elapsed_ms: u64,
    pub size_bytes: usize,
    pub headers: Vec<(String, String)>,
    pub body: Vec<u8>,
    pub cookies: Vec<ParsedCookie>,
}

impl ResponseData {
    pub fn from_summary(summary: api_client::HttpResponseSummary) -> Self {
        let cookies = api_client::parse_set_cookie_headers(&summary.headers);
        Self {
            status: summary.status,
            status_text: summary.status_text,
            elapsed_ms: summary.elapsed_ms,
            size_bytes: summary.body.len(),
            headers: summary.headers,
            body: summary.body,
            cookies,
        }
    }

    pub fn content_type(&self) -> &str {
        self.headers
            .iter()
            .find(|(name, _)| name.eq_ignore_ascii_case("content-type"))
            .map(|(_, value)| value.as_str())
            .unwrap_or("")
    }
}

pub enum SendState {
    Idle,
    Sending,
    Success(ResponseData),
    Error(String),
}

/// Formats a byte count the way a response inspector should: plain bytes
/// below 1 KB, otherwise one decimal of KB -- large enough to matter, small
/// enough that a body under a kilobyte doesn't print a misleading "0.4 KB".
pub fn format_size(bytes: usize) -> String {
    if bytes < 1024 {
        format!("{bytes} B")
    } else {
        format!("{:.1} KB", bytes as f64 / 1024.0)
    }
}

/// Pretty-prints `body` when its content type (or, failing that, its own
/// shape) indicates JSON or XML. Returns `None` when the body isn't
/// well-formed JSON/XML or isn't valid UTF-8 -- callers fall back to the raw
/// bytes rather than showing an error for a body that was never meant to be
/// pretty-printed.
pub fn pretty_print_body(body: &[u8], content_type: &str) -> Option<(String, &'static str)> {
    let text = std::str::from_utf8(body).ok()?;
    let looks_like_json =
        content_type.contains("json") || text.trim_start().starts_with(['{', '[']);
    if looks_like_json {
        if let Ok(value) = serde_json::from_str::<serde_json::Value>(text) {
            return serde_json::to_string_pretty(&value)
                .ok()
                .map(|pretty| (pretty, "JSON"));
        }
    }
    let looks_like_xml = content_type.contains("xml") || text.trim_start().starts_with('<');
    if looks_like_xml {
        return Some((text.to_string(), "XML"));
    }
    None
}

/// The text a response's body should be diffed as: pretty-printed when the
/// body is JSON/XML (so a diff compares meaningful structure, not
/// whatever whitespace the server happened to send), otherwise the raw
/// UTF-8 text, otherwise a byte-length placeholder for genuinely binary
/// bodies (diffing binary bytes as text would produce useless noise).
fn diffable_body_text(response: &ResponseData) -> String {
    if let Some((pretty, _)) = pretty_print_body(&response.body, response.content_type()) {
        return pretty;
    }
    match std::str::from_utf8(&response.body) {
        Ok(text) => text.to_string(),
        Err(_) => format!("<binary body, {} bytes>", response.body.len()),
    }
}

/// A unified diff between two responses for the same request -- Postman has
/// no equivalent, this is a genuine addition once response history exists.
pub fn response_diff_text(previous: &ResponseData, current: &ResponseData) -> String {
    let old_text = diffable_body_text(previous);
    let new_text = diffable_body_text(current);
    if old_text == new_text {
        return "No difference from the previous response body.".to_string();
    }
    language::unified_diff(&old_text, &new_text)
}

/// Whether `Preview` should be offered for this response. GPUI has no
/// sandboxed HTML/iframe rendering primitive (checked before implementing
/// this), so `Preview` never renders real HTML -- it only makes sense to
/// show for bodies that look like HTML in the first place.
pub fn is_html_content(content_type: &str, body: &[u8]) -> bool {
    if content_type.contains("html") {
        return true;
    }
    let Ok(text) = std::str::from_utf8(body) else {
        return false;
    };
    text.trim_start()
        .to_ascii_lowercase()
        .starts_with("<!doctype html")
        || text.trim_start().to_ascii_lowercase().starts_with("<html")
}

/// Renders an HTML body as readable plain text: script/style element content
/// is dropped entirely (never surfaced, not even as visible text) and every
/// remaining tag is stripped, leaving just what a reader would see. This is
/// a deliberately scoped-down stand-in for real HTML rendering -- GPUI has
/// no sandboxed webview/iframe primitive to safely render untrusted HTML,
/// so this never executes scripts or applies styles/layout.
pub fn strip_html_to_readable_text(html: &str) -> String {
    let mut output = String::with_capacity(html.len());
    let mut chars = html.chars().peekable();
    let mut skip_until_tag_named: Option<&'static str> = None;

    while let Some(ch) = chars.next() {
        if ch != '<' {
            if skip_until_tag_named.is_none() {
                output.push(ch);
            }
            continue;
        }

        let mut tag = String::new();
        for inner in chars.by_ref() {
            if inner == '>' {
                break;
            }
            tag.push(inner);
        }
        let tag_lower = tag.to_ascii_lowercase();
        let tag_name = tag_lower
            .trim_start_matches('/')
            .split_whitespace()
            .next()
            .unwrap_or("");

        if let Some(skip_tag) = skip_until_tag_named {
            if tag_lower.starts_with('/') && tag_name == skip_tag {
                skip_until_tag_named = None;
            }
            continue;
        }

        match tag_name {
            "script" | "style" => skip_until_tag_named = Some(tag_name_owned(tag_name)),
            "br" | "p" | "div" | "li" | "tr" => output.push('\n'),
            _ => {}
        }
    }

    unescape_html_entities(output.trim())
}

/// `strip_html_to_readable_text`'s skip-tag match arms need a `&'static
/// str` to stash in `skip_until_tag_named`, but `tag_name` above borrows
/// from a loop-local `String` -- this maps back to the matching static so
/// the borrow doesn't have to outlive the loop iteration.
fn tag_name_owned(tag_name: &str) -> &'static str {
    match tag_name {
        "style" => "style",
        _ => "script",
    }
}

fn unescape_html_entities(text: &str) -> String {
    text.replace("&nbsp;", " ")
        .replace("&amp;", "&")
        .replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&quot;", "\"")
        .replace("&#39;", "'")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sizes_under_a_kilobyte_are_shown_as_plain_bytes() {
        assert_eq!(format_size(512), "512 B");
    }

    #[test]
    fn sizes_at_or_above_a_kilobyte_are_shown_in_kb_with_one_decimal() {
        assert_eq!(format_size(2048), "2.0 KB");
        assert_eq!(format_size(1536), "1.5 KB");
    }

    #[test]
    fn a_compact_json_body_is_reformatted_and_tagged_as_json() {
        let body = br#"{"a":1,"b":[2,3]}"#;
        let (pretty, language) = pretty_print_body(body, "application/json").unwrap();
        assert_eq!(language, "JSON");
        assert!(pretty.contains("\n"));
        assert!(pretty.contains("\"a\": 1"));
    }

    #[test]
    fn json_is_detected_from_body_shape_even_without_a_content_type_header() {
        let body = br#"{"ok":true}"#;
        let (_, language) = pretty_print_body(body, "").unwrap();
        assert_eq!(language, "JSON");
    }

    #[test]
    fn malformed_json_falls_back_to_none_rather_than_panicking() {
        let body = br#"{"a": "#;
        assert!(pretty_print_body(body, "application/json").is_none());
    }

    #[test]
    fn an_xml_body_is_tagged_as_xml_without_reformatting() {
        let body = b"<root><child/></root>";
        let (text, language) = pretty_print_body(body, "application/xml").unwrap();
        assert_eq!(language, "XML");
        assert_eq!(text, "<root><child/></root>");
    }

    #[test]
    fn plain_text_that_is_neither_json_nor_xml_is_not_pretty_printed() {
        assert!(pretty_print_body(b"just plain text", "text/plain").is_none());
    }

    #[test]
    fn content_type_lookup_is_case_insensitive_on_the_header_name() {
        let response = ResponseData {
            status: 200,
            status_text: "OK".into(),
            elapsed_ms: 10,
            size_bytes: 2,
            headers: vec![("Content-Type".to_string(), "application/json".to_string())],
            body: b"{}".to_vec(),
            cookies: Vec::new(),
        };
        assert_eq!(response.content_type(), "application/json");
    }

    #[test]
    fn html_is_detected_from_content_type_or_a_doctype_shape() {
        assert!(is_html_content("text/html; charset=utf-8", b""));
        assert!(is_html_content("", b"<!DOCTYPE html><html></html>"));
        assert!(!is_html_content("application/json", b"{}"));
    }

    #[test]
    fn stripping_html_drops_tags_but_keeps_the_visible_text() {
        let html = "<html><body><h1>Title</h1><p>Hello <b>world</b></p></body></html>";
        let text = strip_html_to_readable_text(html);
        assert!(text.contains("Title"));
        assert!(text.contains("Hello"));
        assert!(text.contains("world"));
        assert!(!text.contains('<'));
    }

    #[test]
    fn stripping_html_drops_script_and_style_content_entirely() {
        let html = "<html><head><style>body{color:red}</style></head><body><script>alert('hi')</script>Visible</body></html>";
        let text = strip_html_to_readable_text(html);
        assert_eq!(text, "Visible");
    }

    fn response_with_body(body: &str) -> ResponseData {
        ResponseData {
            status: 200,
            status_text: "OK".into(),
            elapsed_ms: 1,
            size_bytes: body.len(),
            headers: vec![("Content-Type".to_string(), "application/json".to_string())],
            body: body.as_bytes().to_vec(),
            cookies: Vec::new(),
        }
    }

    #[test]
    fn identical_response_bodies_report_no_difference() {
        let previous = response_with_body(r#"{"a":1}"#);
        let current = response_with_body(r#"{"a": 1}"#); // same after pretty-printing
        assert_eq!(
            response_diff_text(&previous, &current),
            "No difference from the previous response body."
        );
    }

    #[test]
    fn changed_response_bodies_produce_a_unified_diff() {
        let previous = response_with_body(r#"{"a":1}"#);
        let current = response_with_body(r#"{"a":2}"#);
        let diff = response_diff_text(&previous, &current);
        assert!(diff.contains('-'));
        assert!(diff.contains('+'));
        assert!(diff.contains('1'));
        assert!(diff.contains('2'));
    }

    #[test]
    fn stripping_html_unescapes_common_entities() {
        let html = "<p>Tom &amp; Jerry &lt;3</p>";
        let text = strip_html_to_readable_text(html);
        assert_eq!(text, "Tom & Jerry <3");
    }
}
