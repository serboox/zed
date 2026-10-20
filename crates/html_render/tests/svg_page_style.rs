#![cfg(feature = "servo")]

use std::time::{Duration, Instant};

use gpui::{TestAppContext, px, size};
use html_render::HtmlPage;

#[path = "support/hermetic_fonts.rs"]
mod hermetic_fonts;

const DEADLINE: Duration = Duration::from_secs(45);

/// Set on a machine that is expected to have a working engine, so an engine
/// that fails to start is a failure rather than a note.
const REQUIRE: &str = "ZED_REQUIRE_HTML_ENGINE";

const ROW_HEIGHT: usize = 80;
const PAGE_WIDTH: usize = 400;
const ROWS: usize = 9;

/// Drawings styled only by the page's own stylesheet, one per row: a rectangle
/// painted green by a rule, red text painted by a rule, a rectangle that a rule
/// in a `print` stylesheet would paint red, and a rectangle that a rule can only
/// reach through the page's own markup above the drawing. The drawing is
/// rasterised without its surroundings, so that rule does not match. The last
/// rectangle is painted by a stylesheet the page adds after the drawing was
/// already made, which has to make the drawing again. Then a rectangle painted
/// through a custom property the root sets, one painted by a stylesheet the page
/// links to, one painted with `currentColor` where the root sets the colour, and one
/// painted with `currentColor` where a wrapper of the drawing sets a colour of its own.
const PAGE: &str = "<html><head>\
    <link rel=\"stylesheet\" href=\"svg.css\">\
    <style>\
        :root { --cyan: #00ffff }\
        body { color: #ffff00 }\
        svg .var { fill: var(--cyan) }\
        svg .box { fill: #00ff00 }\
        svg text.label { font: 56px sans-serif; fill: #ff0000 }\
        .card svg .inside { fill: #0000ff }\
    </style>\
    <style media=\"print\">svg .only-in-print { fill: #ff0000 }</style>\
    </head><body style=\"margin:0;background:#fff\">\
    <svg width=\"400\" height=\"80\" style=\"display:block\">\
        <rect class=\"box\" x=\"0\" y=\"0\" width=\"400\" height=\"80\"/></svg>\
    <svg width=\"400\" height=\"80\" style=\"display:block\">\
        <text class=\"label\" x=\"10\" y=\"60\">MMMM</text></svg>\
    <svg width=\"400\" height=\"80\" style=\"display:block\">\
        <rect class=\"only-in-print\" x=\"0\" y=\"0\" width=\"400\" height=\"80\"/></svg>\
    <div class=\"card\"><svg width=\"400\" height=\"80\" style=\"display:block\">\
        <rect class=\"inside\" x=\"0\" y=\"0\" width=\"400\" height=\"80\"/></svg></div>\
    <svg width=\"400\" height=\"80\" style=\"display:block\">\
        <rect class=\"late\" x=\"0\" y=\"0\" width=\"400\" height=\"80\"/></svg>\
    <svg width=\"400\" height=\"80\" style=\"display:block\">\
        <rect class=\"var\" x=\"0\" y=\"0\" width=\"400\" height=\"80\"/></svg>\
    <svg width=\"400\" height=\"80\" style=\"display:block\">\
        <rect class=\"linked\" x=\"0\" y=\"0\" width=\"400\" height=\"80\"/></svg>\
    <svg width=\"400\" height=\"80\" style=\"display:block\">\
        <rect fill=\"currentColor\" x=\"0\" y=\"0\" width=\"400\" height=\"80\"/></svg>\
    <div style=\"color:#ff00ff\"><svg width=\"400\" height=\"80\" style=\"display:block\">\
        <rect fill=\"currentColor\" x=\"0\" y=\"0\" width=\"400\" height=\"80\"/></svg></div>\
    <script>setTimeout(function () {\
        var style = document.createElement('style');\
        style.textContent = 'svg .late { fill: #0000ff }';\
        document.head.appendChild(style);\
    }, 1500);</script>\
    </body></html>";

/// More pixels than this of the colour asked for in a row is a row painted
/// that colour: the rectangles are 32 000 pixels, four letters at 56 px thousands.
const MIN_PIXELS: usize = 500;

fn is_green([red, green, blue]: [u8; 3]) -> bool {
    red < 40 && green > 215 && blue < 40
}

fn is_red([red, green, blue]: [u8; 3]) -> bool {
    red > 215 && green < 40 && blue < 40
}

fn is_blue([red, green, blue]: [u8; 3]) -> bool {
    red < 40 && green < 40 && blue > 215
}

fn is_cyan([red, green, blue]: [u8; 3]) -> bool {
    red < 40 && green > 215 && blue > 215
}

fn is_magenta([red, green, blue]: [u8; 3]) -> bool {
    red > 215 && green < 40 && blue > 215
}

fn is_yellow([red, green, blue]: [u8; 3]) -> bool {
    red > 215 && green > 215 && blue < 40
}

fn is_black([red, green, blue]: [u8; 3]) -> bool {
    red < 40 && green < 40 && blue < 40
}

/// How many pixels of a row of the page satisfy `wanted`.
fn pixels_in_row(page: &HtmlPage, row: usize, wanted: impl Fn([u8; 3]) -> bool) -> usize {
    let Some(frame) = page.frame() else {
        return 0;
    };
    let width = u32::from(frame.size(0).width) as usize;
    let Some(bytes) = frame.as_bytes(0) else {
        return 0;
    };
    let first = row * ROW_HEIGHT * width * 4;
    let last = (row + 1) * ROW_HEIGHT * width * 4;
    let Some(pixels) = bytes.get(first..last) else {
        return 0;
    };
    pixels
        .chunks_exact(4)
        // The frames are premultiplied BGRA, which is what gpui draws.
        .filter(|pixel| wanted([pixel[2], pixel[1], pixel[0]]))
        .count()
}

#[gpui::test]
fn an_inline_svg_is_styled_by_the_page_stylesheet(cx: &mut TestAppContext) {
    let workspace = tempfile::tempdir().expect("a scratch directory");
    hermetic_fonts::use_only_the_bundled_fonts(workspace.path());
    std::fs::write(
        workspace.path().join("svg.css"),
        "svg .linked { fill: #ff00ff }",
    )
    .expect("a stylesheet to link to");

    cx.update(|cx| {
        let mut page = match HtmlPage::open(
            PAGE.to_string().into(),
            Some(workspace.path()),
            size(px(PAGE_WIDTH as f32), px((ROW_HEIGHT * ROWS) as f32)),
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

        let expectations: [(&str, usize, fn([u8; 3]) -> bool); ROWS] = [
            ("a rule paints a rectangle green", 0, is_green),
            ("a rule paints text red", 1, is_red),
            ("a print-only rule leaves a rectangle black", 2, is_black),
            (
                "a rule through markup outside the drawing leaves a rectangle black",
                3,
                is_black,
            ),
            (
                "a stylesheet added after the drawing was made paints it",
                4,
                is_blue,
            ),
            (
                "a custom property the root sets paints a rectangle",
                5,
                is_cyan,
            ),
            ("a linked stylesheet paints a rectangle", 6, is_magenta),
            ("currentColor is the colour the root sets", 7, is_yellow),
            (
                "currentColor is the colour a wrapper of the drawing sets",
                8,
                is_magenta,
            ),
        ];
        let deadline = Instant::now() + DEADLINE;
        let mut counts = [0; ROWS];
        while Instant::now() < deadline {
            page.pump();
            for (index, (_, row, wanted)) in expectations.iter().enumerate() {
                counts[index] = pixels_in_row(&page, *row, wanted);
            }
            if counts.iter().all(|count| *count >= MIN_PIXELS) {
                return;
            }
            std::thread::sleep(Duration::from_millis(8));
        }
        let failed: Vec<_> = expectations
            .iter()
            .zip(counts)
            .filter(|(_, count)| *count < MIN_PIXELS)
            .map(|((what, ..), count)| format!("{what}: {count} pixels"))
            .collect();
        panic!(
            "the page's stylesheet did not reach its drawings: {failed:?}, every row: {counts:?}"
        );
    });
}
