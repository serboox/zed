#![cfg(feature = "servo")]

use std::time::{Duration, Instant};

use gpui::{TestAppContext, px, size};
use html_render::HtmlPage;

const DEADLINE: Duration = Duration::from_secs(45);

/// Set on a machine that is expected to have a working engine, so an engine
/// that fails to start is a failure rather than a note.
const REQUIRE: &str = "ZED_REQUIRE_HTML_ENGINE";

const WIDTH: f32 = 420.;
const HEIGHT: f32 = 300.;

/// Every case is a pair of pages: the one that numbers with counters or an
/// ordered list, and the one that has the numbers typed out. Laid out the same
/// way, they have to paint the same pixels, which is how a number is checked
/// without reading it back out of the picture.
struct Case {
    name: &'static str,
    /// What lays the pages out, and for the counted page, what numbers them.
    styles: &'static str,
    /// What lays out the typed page, which has no counters to number it.
    typed_styles: &'static str,
    counted: &'static str,
    typed: &'static str,
}

const CASES: &[Case] = &[
    Case {
        name: "an ordered list counts from one",
        styles: "ol { margin: 0; padding: 0; list-style-position: inside }",
        typed_styles: "ol { margin: 0; padding: 0 }",
        counted: "<ol><li>one</li><li>two</li><li>three</li></ol>",
        typed: "<div>1. one</div><div>2. two</div><div>3. three</div>",
    },
    Case {
        name: "start and value move the count, reversed counts down",
        styles: "ol { margin: 0; padding: 0; list-style-position: inside }",
        typed_styles: "ol { margin: 0; padding: 0 }",
        counted: "<ol start=\"5\"><li>a</li><li>b</li></ol>\
                  <ol reversed><li>c</li><li>d</li><li>e</li></ol>\
                  <ol><li value=\"9\">f</li><li>g</li></ol>",
        typed: "<div>5. a</div><div>6. b</div>\
                <div>3. c</div><div>2. d</div><div>1. e</div>\
                <div>9. f</div><div>10. g</div>",
    },
    Case {
        name: "a nested list starts again and its parent carries on",
        styles: "ol { margin: 0; padding: 0; list-style-position: inside } \
                 ol ol { margin-left: 20px }",
        typed_styles: "ol ol { margin-left: 20px }",
        counted: "<ol><li>a<ol><li>x</li><li>y</li></ol></li><li>b</li></ol>",
        typed: "<div>1. a</div><div style=\"margin-left:20px\">1. x</div>\
                <div style=\"margin-left:20px\">2. y</div><div>2. b</div>",
    },
    Case {
        name: "list styles that write letters and numerals",
        styles: "ol { margin: 0; padding: 0; list-style-position: inside }",
        typed_styles: "ol { margin: 0; padding: 0 }",
        counted: "<ol style=\"list-style-type: lower-roman\"><li>a</li><li>b</li><li>c</li><li>d</li></ol>\
                  <ol style=\"list-style-type: upper-alpha\"><li>e</li><li>f</li></ol>\
                  <ol style=\"list-style-type: decimal-leading-zero\"><li>g</li></ol>",
        typed: "<div>i. a</div><div>ii. b</div><div>iii. c</div><div>iv. d</div>\
                <div>A. e</div><div>B. f</div><div>01. g</div>",
    },
    Case {
        name: "counter() in generated content numbers whatever it is asked to",
        styles: "body { counter-reset: step } p::before { counter-increment: step; \
                 content: counter(step) \") \" }",
        typed_styles: "",
        counted: "<p>one</p><p>two</p><p>three</p>",
        typed: "<p>1) one</p><p>2) two</p><p>3) three</p>",
    },
    Case {
        name: "a counter the list item increments and its ::before writes",
        styles: ".s { counter-reset: step; list-style: none; padding: 0; margin: 0 } \
                 .s > li { counter-increment: step } \
                 .s > li::before { content: counter(step) \". \" }",
        typed_styles: "",
        counted: "<ol class=\"s\"><li>a</li><li>b</li><li>c</li></ol>",
        typed: "<div>1. a</div><div>2. b</div><div>3. c</div>",
    },
    Case {
        name: "counters() joins the numbers of nested counters",
        styles: "div { counter-reset: part } h4 { margin: 0 } \
                 h4::before { counter-increment: part; content: counters(part, \".\") \" \" } \
                 div div { margin-left: 10px }",
        typed_styles: "h4 { margin: 0 } div div { margin-left: 10px }",
        counted: "<div><h4>a</h4><h4>b</h4><div><h4>c</h4><h4>d</h4></div><h4>e</h4></div>",
        typed: "<div><h4>1 a</h4><h4>2 b</h4><div style=\"margin-left:10px\"><h4>2.1 c</h4>\
                <h4>2.2 d</h4></div><h4>2.3 e</h4></div>",
    },
];

fn open(html: &str, cx: &mut gpui::App) -> Option<HtmlPage> {
    match HtmlPage::open(
        html.to_string().into(),
        None,
        size(px(WIDTH), px(HEIGHT)),
        1.,
        cx,
    ) {
        Ok(page) => Some(page),
        Err(error) => {
            println!("HTML_ENGINE: unavailable: {error:#}");
            assert!(
                std::env::var(REQUIRE).is_err(),
                "{REQUIRE} is set, so the engine was expected to start: {error:#}"
            );
            None
        }
    }
}

fn document(styles: &str, body: &str) -> String {
    format!(
        "<html><head><style>body {{ margin: 0; font: 18px/24px sans-serif; background: #fff; \
         color: #000 }} p {{ margin: 0 }} {styles}</style></head><body>{body}</body></html>"
    )
}

/// The page, once it has painted the same picture twice running.
fn settled_frame(page: &mut HtmlPage) -> Vec<u8> {
    let deadline = Instant::now() + DEADLINE;
    let mut previous: Option<Vec<u8>> = None;
    let mut steady_since = Instant::now();
    while Instant::now() < deadline {
        page.pump();
        if let Some(bytes) = page
            .frame()
            .and_then(|frame| frame.as_bytes(0).map(<[u8]>::to_vec))
        {
            if previous.as_deref() == Some(bytes.as_slice()) {
                if steady_since.elapsed() > Duration::from_millis(600) {
                    return bytes;
                }
            } else {
                steady_since = Instant::now();
                previous = Some(bytes);
            }
        }
        std::thread::sleep(Duration::from_millis(8));
    }
    previous.unwrap_or_default()
}

fn ink(bytes: &[u8]) -> usize {
    bytes
        .chunks_exact(4)
        .filter(|pixel| pixel[..3].iter().any(|channel| *channel < 128))
        .count()
}

#[gpui::test]
fn counters_and_ordered_lists_number_like_typed_out_text(cx: &mut TestAppContext) {
    cx.update(|cx| {
        let mut failures = Vec::new();
        for case in CASES {
            let Some(mut counted) = open(&document(case.styles, case.counted), cx) else {
                return;
            };
            let counted_frame = settled_frame(&mut counted);
            drop(counted);
            let mut typed = open(&document(case.typed_styles, case.typed), cx)
                .expect("the engine started once already");
            let typed_frame = settled_frame(&mut typed);
            drop(typed);

            let typed_ink = ink(&typed_frame);
            if typed_ink < 100 {
                failures.push(format!("{}: the typed page drew almost nothing", case.name));
            } else if counted_frame != typed_frame {
                failures.push(format!(
                    "{}: the numbered page is not the typed one ({} dark pixels against {})",
                    case.name,
                    ink(&counted_frame),
                    typed_ink
                ));
            }
        }
        assert!(failures.is_empty(), "{}", failures.join("\n"));
    });
}
