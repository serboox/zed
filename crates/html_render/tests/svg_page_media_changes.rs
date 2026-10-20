#![cfg(feature = "servo")]

use std::time::{Duration, Instant};

use gpui::{TestAppContext, px, size};
use html_render::HtmlPage;

#[path = "support/hermetic_fonts.rs"]
mod hermetic_fonts;
#[path = "support/pixels.rs"]
mod pixels;
#[path = "support/svg_rows.rs"]
mod svg_rows;

use pixels::{is_black, is_cyan, is_green};
use svg_rows::{drawing, rectangle};

const DEADLINE: Duration = Duration::from_secs(45);

/// Set on a machine that is expected to have a working engine, so an engine
/// that fails to start is a failure rather than a note.
const REQUIRE: &str = "ZED_REQUIRE_HTML_ENGINE";

const ROW_HEIGHT: usize = 80;
const PAGE_WIDTH: usize = 400;

/// More pixels than this of the colour asked for in a row is a row painted
/// that colour: a rectangle is 32 000 pixels.
const MIN_PIXELS: usize = 500;

/// How long every row has to show what it should before the page is taken to be
/// done: a rectangle that is black while it waits for its rule looks like one that
/// no rule paints.
const STEADY_FOR: Duration = Duration::from_millis(1500);

#[gpui::test]
fn a_drawing_is_made_again_when_a_media_feature_of_the_page_changes(cx: &mut TestAppContext) {
    let workspace = tempfile::tempdir().expect("a scratch directory");
    hermetic_fonts::use_only_the_bundled_fonts(workspace.path());
    let document = format!(
        "<html><head><style>\
            @media (min-width: 500px) {{ svg .wide {{ fill: #0f0 }} }}\
            @media (prefers-color-scheme: dark) {{ svg .dark {{ fill: #0ff }} }}\
        </style></head><body style=\"margin:0;background:#fff\">{}{}</body></html>",
        drawing(&rectangle("wide", 0, 400)),
        drawing(&rectangle("dark", 0, 400)),
    );

    cx.update(|cx| {
        let mut page = match HtmlPage::open(
            document.into(),
            Some(workspace.path()),
            size(px(PAGE_WIDTH as f32), px((ROW_HEIGHT * 2) as f32)),
            1.,
            cx,
        ) {
            Ok(page) => page,
            Err(error) => {
                assert!(
                    std::env::var(REQUIRE).is_err(),
                    "{REQUIRE} is set, so the engine was expected to start: {error:#}"
                );
                return;
            }
        };

        let count = |page: &HtmlPage, row: usize, wanted: fn([u8; 3]) -> bool| {
            pixels::pixels_in_row(page, row, ROW_HEIGHT, wanted)
        };
        let wait_until = |page: &mut HtmlPage, what: &str, done: &dyn Fn(&HtmlPage) -> bool| {
            let deadline = Instant::now() + DEADLINE;
            while Instant::now() < deadline {
                page.pump();
                if done(page) {
                    return;
                }
                std::thread::sleep(Duration::from_millis(8));
            }
            panic!("{what}");
        };

        wait_until(
            &mut page,
            "both drawings are black before anything changes",
            &|page| {
                count(page, 0, is_black) >= MIN_PIXELS && count(page, 1, is_black) >= MIN_PIXELS
            },
        );
        let settled = Instant::now() + STEADY_FOR;
        while Instant::now() < settled {
            page.pump();
            std::thread::sleep(Duration::from_millis(8));
        }
        assert!(
            count(&page, 0, is_green) < MIN_PIXELS && count(&page, 1, is_cyan) < MIN_PIXELS,
            "a media query that does not hold painted a drawing"
        );

        page.resize(size(px(600.), px((ROW_HEIGHT * 2) as f32)), 1.);
        wait_until(
            &mut page,
            "a drawing was not made again when the page became wide",
            &|page| count(page, 0, is_green) >= MIN_PIXELS,
        );

        page.set_dark(true);
        wait_until(
            &mut page,
            "a drawing was not made again when the page became dark",
            &|page| count(page, 1, is_cyan) >= MIN_PIXELS,
        );
    });
}
