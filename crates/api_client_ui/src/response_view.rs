use api_client::ParsedCookie;
use gpui::{AnyElement, App, ClipboardItem, SharedString};
use ui::{ButtonStyle, IconButton, IconName, IconSize, Tooltip, prelude::*};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ResponseTab {
    Pretty,
    Preview,
    Raw,
    Headers,
    Cookies,
    Diff,
    Timing,
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
    /// Where the time went, for the timing diagram.
    pub timings: api_client::Timings,
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
            timings: summary.timings,
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
/// One bar of the timing diagram: what it was, what it means, where it starts and
/// how long it lasted, in milliseconds.
pub struct Phase {
    pub name: &'static str,
    pub what_it_is: &'static str,
    pub starts_ms: f32,
    pub lasts_ms: f32,
    pub colour: Color,
}

/// The phases of an exchange, in the order they happen, each starting where the
/// one before it ended. What is left of the total after the measured phases is a
/// phase of its own rather than being hidden, so the bars add up to the number
/// shown beside the status.
pub fn phases_of(timings: api_client::Timings) -> Vec<Phase> {
    let mut phases = Vec::new();
    let mut at = 0.;
    if let Some(resolve) = timings.resolve_ms {
        phases.push(Phase {
            name: "Looking up the host",
            what_it_is: "Turning the name into an address.",
            starts_ms: at,
            lasts_ms: resolve as f32,
            colour: Color::Info,
        });
        at += resolve as f32;
    }
    phases.push(Phase {
        name: "Waiting for the response",
        what_it_is: "Connecting, the handshake if there is one, sending, and the \
                     server's own thinking: the client underneath reports these as one.",
        starts_ms: at,
        lasts_ms: timings.wait_ms as f32,
        colour: Color::Warning,
    });
    at += timings.wait_ms as f32;
    phases.push(Phase {
        name: "Reading the body",
        what_it_is: "From the headers arriving to the last byte of the body.",
        starts_ms: at,
        lasts_ms: timings.download_ms as f32,
        colour: Color::Success,
    });
    at += timings.download_ms as f32;
    let unaccounted = timings.total_ms as f32 - at;
    if unaccounted > 0.5 {
        phases.push(Phase {
            name: "Everything else",
            what_it_is: "Building the request, and the moments between the phases above.",
            starts_ms: at,
            lasts_ms: unaccounted,
            colour: Color::Muted,
        });
    }
    phases
}

/// The diagram of where a request's time went: one bar a phase, on one axis, so
/// the reader can see at a glance whether the wait was the network, the server or
/// the size of what came back.
pub fn render_timing(timings: api_client::Timings, cx: &mut App) -> AnyElement {
    let total = timings.total_ms.max(1) as f32;
    let border = cx.theme().colors().border_variant;
    let phases = phases_of(timings);

    let axis = h_flex()
        .w_full()
        .justify_between()
        .children([0., 0.25, 0.5, 0.75, 1.].map(|part| {
            Label::new(format!("{:.0} ms", total * part))
                .size(LabelSize::XSmall)
                .color(Color::Muted)
        }));

    let rows = phases.into_iter().enumerate().map(|(at_row, phase)| {
        let (name, what_it_is, starts, lasts, colour) = (
            phase.name,
            phase.what_it_is,
            phase.starts_ms,
            phase.lasts_ms,
            phase.colour,
        );
        {
            let from = (starts / total).clamp(0., 1.);
            // A phase of a millisecond still has to be visible, or a fast request
            // shows an empty diagram.
            let wide = (lasts / total).clamp(0.004, 1. - from);
            h_flex()
                .id(("timing-row", at_row))
                .w_full()
                .gap_2()
                .py_0p5()
                .items_center()
                .hover(|row| row.bg(ui::cyberpunk::row_hovered()))
                .child(
                    div()
                        .flex_none()
                        .w(px(180.))
                        .child(Label::new(name).size(LabelSize::Small).color(Color::Muted)),
                )
                .child(
                    div()
                        .id(("timing-track", at_row))
                        .relative()
                        .flex_1()
                        .h(px(14.))
                        .border_b_1()
                        .border_color(border.opacity(0.4))
                        // The quarter marks, so a bar can be read against the axis
                        // above rather than guessed at.
                        .children([0.25, 0.5, 0.75].map(|part| {
                            div()
                                .absolute()
                                .top_0()
                                .left(gpui::relative(part))
                                .w(px(1.))
                                .h_full()
                                .bg(border.opacity(0.5))
                        }))
                        .child(
                            div()
                                .id(("timing-bar", at_row))
                                .absolute()
                                .top(px(2.))
                                .left(gpui::relative(from))
                                .w(gpui::relative(wide))
                                .h(px(10.))
                                .bg(colour.color(cx))
                                .tooltip(Tooltip::text(format!("{name}: {lasts:.0} ms"))),
                        ),
                )
                .child(
                    div().flex_none().w(px(72.)).child(
                        Label::new(format!("{lasts:.0} ms"))
                            .size(LabelSize::Small)
                            .buffer_font(cx),
                    ),
                )
                .child(
                    div().flex_none().w(px(220.)).child(
                        Label::new(what_it_is)
                            .size(LabelSize::XSmall)
                            .color(Color::Muted),
                    ),
                )
                .into_any_element()
        }
    });

    v_flex()
        .w_full()
        .gap_1()
        .child(
            h_flex()
                .w_full()
                .gap_2()
                .child(div().flex_none().w(px(180.)))
                .child(div().flex_1().child(axis))
                .child(div().flex_none().w(px(72. + 220. + 8.))),
        )
        .children(rows)
        .child(
            Label::new(match timings.resolve_ms {
                Some(_) => "The host was looked up here, before the request went out.",
                None => {
                    "The host was not looked up here: the client did it itself, and \
                         its share is inside the wait."
                }
            })
            .size(LabelSize::XSmall)
            .color(Color::Muted),
        )
        .into_any_element()
}

/// One row of a table of names and values, as the response's headers and cookies
/// are shown.
pub struct Pair {
    pub name: SharedString,
    pub value: SharedString,
    /// Anything else worth showing after the value, in a quieter colour -- a
    /// cookie's attributes, say.
    pub also: Option<SharedString>,
}

/// A table of names and values: aligned columns, a row under the pointer marked,
/// and every value copyable -- by the row's own button, or the whole table at
/// once. A list of label pairs reads as a paragraph; what a reader does with
/// headers is look one up and copy it.
pub fn render_pairs(
    id: &'static str,
    empty: &'static str,
    pairs: Vec<Pair>,
    cx: &mut App,
) -> AnyElement {
    if pairs.is_empty() {
        return Label::new(empty)
            .size(LabelSize::Small)
            .color(Color::Muted)
            .into_any_element();
    }

    let all_of_it = pairs
        .iter()
        .map(|pair| match &pair.also {
            Some(also) => format!("{}: {} {}", pair.name, pair.value, also),
            None => format!("{}: {}", pair.name, pair.value),
        })
        .collect::<Vec<_>>()
        .join("\n");
    let border = cx.theme().colors().border_variant;
    let count = pairs.len();

    let rows =
        pairs.into_iter().enumerate().map(|(at, pair)| {
            let value = pair.value.clone();
            h_flex()
                .id((id, at))
                .w_full()
                .py_0p5()
                .px_1()
                .gap_2()
                .items_start()
                .when(at + 1 < count, |row| {
                    row.border_b_1().border_color(border.opacity(0.4))
                })
                .hover(|row| row.bg(ui::cyberpunk::row_hovered()))
                .child(
                    div()
                        .flex_none()
                        // A column wide enough for the names a server actually sends,
                        // so the values line up and can be read down the page.
                        .w(px(220.))
                        .child(
                            Label::new(pair.name.clone())
                                .size(LabelSize::Small)
                                .color(Color::Muted)
                                .buffer_font(cx),
                        ),
                )
                .child(
                    div()
                        .flex_1()
                        .min_w_0()
                        .child(
                            Label::new(pair.value.clone())
                                .size(LabelSize::Small)
                                .buffer_font(cx),
                        )
                        .children(pair.also.map(|also| {
                            Label::new(also).size(LabelSize::Small).color(Color::Muted)
                        })),
                )
                .child(
                    IconButton::new((id, at), IconName::Copy)
                        .icon_size(IconSize::XSmall)
                        .tooltip(Tooltip::text("Copy this value"))
                        .on_click(move |_, _, cx| {
                            cx.write_to_clipboard(ClipboardItem::new_string(value.to_string()));
                        }),
                )
                .into_any_element()
        });

    v_flex()
        .w_full()
        .gap_0p5()
        .child(
            h_flex()
                .w_full()
                .justify_between()
                .items_center()
                .child(
                    Label::new(format!("{count}"))
                        .size(LabelSize::Small)
                        .color(Color::Muted),
                )
                .child(
                    Button::new(SharedString::from(format!("{id}-copy-all")), "Copy all")
                        .label_size(LabelSize::Small)
                        .style(ButtonStyle::Subtle)
                        .on_click(move |_, _, cx| {
                            cx.write_to_clipboard(ClipboardItem::new_string(all_of_it.clone()));
                        }),
                ),
        )
        .children(rows)
        .into_any_element()
}

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
    /// The bars have to line up end to end and add up to the total, or the diagram
    /// says the request took a different time than the number beside the status.
    #[test]
    fn the_phases_run_end_to_end_and_add_up_to_the_whole() {
        let timings = api_client::Timings {
            resolve_ms: Some(12),
            wait_ms: 300,
            download_ms: 40,
            total_ms: 360,
        };

        let phases = super::phases_of(timings);
        let names: Vec<&str> = phases.iter().map(|phase| phase.name).collect();
        assert_eq!(
            names,
            vec![
                "Looking up the host",
                "Waiting for the response",
                "Reading the body",
                "Everything else",
            ],
            "what was not measured is a phase of its own rather than being hidden"
        );

        let mut expected_start = 0.;
        for phase in &phases {
            assert!(
                (phase.starts_ms - expected_start).abs() < 0.01,
                "{} starts at {} rather than {expected_start}",
                phase.name,
                phase.starts_ms
            );
            expected_start += phase.lasts_ms;
        }
        assert!(
            (expected_start - timings.total_ms as f32).abs() < 0.01,
            "the phases add up to {expected_start}ms, not the {}ms the exchange took",
            timings.total_ms
        );
    }

    #[test]
    fn a_lookup_that_did_not_happen_here_gets_no_bar() {
        let phases = super::phases_of(api_client::Timings {
            resolve_ms: None,
            wait_ms: 120,
            download_ms: 5,
            total_ms: 125,
        });

        assert!(
            !phases.iter().any(|phase| phase.name.contains("Looking up")),
            "a lookup nobody timed must not be drawn as though it were measured"
        );
        assert_eq!(phases.len(), 2, "and nothing is left over to explain");
    }

    #[test]
    fn a_request_that_took_no_time_still_draws() {
        let phases = super::phases_of(api_client::Timings::default());

        assert!(!phases.is_empty());
        assert!(
            phases.iter().all(|phase| phase.lasts_ms >= 0.),
            "no phase may last a negative time, whatever the numbers say"
        );
    }

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
            timings: api_client::Timings::default(),
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
            timings: api_client::Timings::default(),
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
