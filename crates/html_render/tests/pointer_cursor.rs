#![cfg(feature = "servo")]

use std::time::{Duration, Instant};

use gpui::{CursorStyle, TestAppContext, point, px, size};
use html_render::HtmlPage;

const DEADLINE: Duration = Duration::from_secs(45);

/// Set on a machine that is expected to have a working engine, so an engine
/// that fails to start is a failure rather than a note.
const REQUIRE: &str = "ZED_REQUIRE_HTML_ENGINE";

const WIDTH: f32 = 400.;
const HEIGHT: f32 = 200.;

/// Every control sits in a row of its own, so a point picks exactly one.
const CONTROL_HEIGHT: f32 = 30.;

const PAGE: &str = "<html><head><style>\
    body { margin: 0; background: #fff }\
    .row { display: block; width: 300px; height: 30px; margin: 0; padding: 0; border: 0 }\
    </style></head><body>\
    <button class=\"row\">button</button>\
    <button class=\"row\" disabled>disabled</button>\
    <button class=\"row\" style=\"cursor: default\">author says default</button>\
    <div class=\"row\" role=\"button\">role button</div>\
    <div class=\"row\">plain</div>\
    </body></html>";

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

/// How long the engine is given to answer a pointer that has moved. The test
/// always waits it out in full: an answer that has not come yet looks the same
/// as the arrow, so stopping at the first arrow would prove nothing.
const SETTLE: Duration = Duration::from_millis(900);

/// Moves the pointer onto the middle of the row, pumps the engine for the
/// settling time and reads the pointer the page asks for.
fn pointer_over_row(page: &mut HtmlPage, row: usize) -> CursorStyle {
    let at = point(
        px(100.),
        px(CONTROL_HEIGHT * row as f32 + CONTROL_HEIGHT / 2.),
    );
    let until = Instant::now() + SETTLE;
    while Instant::now() < until {
        // A pointer move is heard at a pace the page can keep, so it is said
        // again on every turn.
        page.mouse_moved(at);
        page.pump();
        std::thread::sleep(Duration::from_millis(16));
    }
    page.cursor()
}

#[gpui::test]
fn the_controls_of_a_page_answer_with_a_hand(cx: &mut TestAppContext) {
    cx.update(|cx| {
        let Some(mut page) = open(PAGE, cx) else {
            return;
        };
        // Wait for the first paint, so there is a layout to point at.
        let deadline = Instant::now() + DEADLINE;
        while Instant::now() < deadline && page.frame().is_none() {
            page.pump();
            std::thread::sleep(Duration::from_millis(16));
        }

        assert_eq!(
            pointer_over_row(&mut page, 0),
            CursorStyle::PointingHand,
            "a button shows a hand"
        );
        assert_eq!(
            pointer_over_row(&mut page, 3),
            CursorStyle::PointingHand,
            "an element with the role of a button shows a hand"
        );
        assert_eq!(
            pointer_over_row(&mut page, 4),
            CursorStyle::Arrow,
            "plain text on a plain box is not a control"
        );
        // Back onto a control first, so the arrow that follows is an answer
        // the page gave and not the pointer it started with.
        assert_eq!(
            pointer_over_row(&mut page, 0),
            CursorStyle::PointingHand,
            "a button shows a hand again"
        );
        assert_eq!(
            pointer_over_row(&mut page, 1),
            CursorStyle::Arrow,
            "a disabled button cannot be pressed, so it shows no hand"
        );
        assert_eq!(
            pointer_over_row(&mut page, 0),
            CursorStyle::PointingHand,
            "a button shows a hand once more"
        );
        assert_eq!(
            pointer_over_row(&mut page, 2),
            CursorStyle::Arrow,
            "a page that says what its control shows has the last word"
        );
    });
}
