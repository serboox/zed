#![cfg(feature = "servo")]

use std::cell::RefCell;
use std::rc::Rc;
use std::time::{Duration, Instant};

use gpui::{TestAppContext, px, size};
use html_render::HtmlPage;

const DEADLINE: Duration = Duration::from_secs(45);

/// Set on a machine that is expected to have a working engine, so an engine
/// that fails to start is a failure rather than a note.
const REQUIRE: &str = "ZED_REQUIRE_HTML_ENGINE";

/// Text that a rule about its own tag styles blue, inside a box whose rule about
/// spans would style any span in it red and enormous.
const PAGE: &str = "<html><head><style>\
    .card span { font-size: 60px; color: #ff0000 }\
    .card b { font-size: 40px; color: #0000ff }\
    </style></head><body style=\"margin:0;background:#fff\">\
    <div class=\"card\"><b>MMMM</b></div></body></html>";

const MIN_PIXELS: usize = 500;

fn count_pixels(page: &HtmlPage, wanted: impl Fn([u8; 3]) -> bool) -> usize {
    let Some(frame) = page.frame() else {
        return 0;
    };
    let Some(bytes) = frame.as_bytes(0) else {
        return 0;
    };
    bytes
        .chunks_exact(4)
        // The frames are premultiplied BGRA, which is what gpui draws.
        .filter(|pixel| wanted([pixel[2], pixel[1], pixel[0]]))
        .count()
}

fn ask(page: &mut HtmlPage, script: &str) -> Option<String> {
    let answer = Rc::new(RefCell::new(None));
    let slot = answer.clone();
    page.evaluate(script, move |text| *slot.borrow_mut() = Some(text));
    let deadline = Instant::now() + Duration::from_secs(10);
    while Instant::now() < deadline {
        page.pump();
        if let Some(text) = answer.borrow_mut().take() {
            return Some(text);
        }
        std::thread::sleep(Duration::from_millis(8));
    }
    None
}

#[gpui::test]
fn a_page_rule_about_spans_does_not_reach_the_words_the_selection_wraps(cx: &mut TestAppContext) {
    cx.update(|cx| {
        let mut page = match HtmlPage::open(
            PAGE.to_string().into(),
            None,
            size(px(400.), px(200.)),
            1.,
            cx,
        ) {
            Ok(page) => {
                println!("HTML_ENGINE: ok");
                page
            }
            Err(error) => {
                println!("HTML_ENGINE: unavailable: {error:#}");
                assert!(
                    std::env::var(REQUIRE).is_err(),
                    "{REQUIRE} is set, so the engine was expected to start: {error:#}"
                );
                return;
            }
        };

        // The words are wrapped once the page has settled; what is asked is
        // whether that has happened, so that the colours are read after it.
        let deadline = Instant::now() + DEADLINE;
        let mut wrapped = false;
        while Instant::now() < deadline {
            let count = ask(
                &mut page,
                "String(document.querySelectorAll('[data-zed-selection=word]').length)",
            );
            if count
                .as_deref()
                .is_some_and(|count| count != "0" && count != "\"0\"")
            {
                wrapped = true;
                break;
            }
            std::thread::sleep(Duration::from_millis(100));
        }
        assert!(wrapped, "the selection never wrapped the page's words");

        let settle = Instant::now() + Duration::from_secs(2);
        while Instant::now() < settle {
            page.pump();
            std::thread::sleep(Duration::from_millis(8));
        }

        let blue = count_pixels(&page, |[red, green, blue]| {
            red < 40 && green < 40 && blue > 215
        });
        let red = count_pixels(&page, |[red, green, blue]| {
            red > 215 && green < 40 && blue < 40
        });
        assert!(
            blue >= MIN_PIXELS && red == 0,
            "the words took the page's rule about spans: {blue} blue pixels, {red} red"
        );
    });
}
