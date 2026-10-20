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
const ROWS: [&str; 5] = [
    "no font named",
    "monospace",
    "sans-serif",
    "a missing font, then monospace",
    "the rectangle",
];

/// A page of drawings, each a row tall. The first four hold only large black
/// text: no font named, `monospace`, `sans-serif`, and a stack that names an
/// uninstalled font ahead of `monospace`. The last holds a black rectangle, to
/// tell a page that was drawn without its text from one that was not drawn.
const PAGE: &str = "<html><body style=\"margin:0;background:#fff\">\
    <svg width=\"400\" height=\"80\" style=\"display:block\">\
        <text x=\"10\" y=\"60\" font-size=\"56\" fill=\"#000\">MMMM</text></svg>\
    <svg width=\"400\" height=\"80\" style=\"display:block\">\
        <text x=\"10\" y=\"60\" font-size=\"56\" fill=\"#000\" font-family=\"monospace\">MMMM</text></svg>\
    <svg width=\"400\" height=\"80\" style=\"display:block\">\
        <text x=\"10\" y=\"60\" font-size=\"56\" fill=\"#000\" font-family=\"sans-serif\">MMMM</text></svg>\
    <svg width=\"400\" height=\"80\" style=\"display:block\">\
        <text x=\"10\" y=\"60\" font-size=\"56\" fill=\"#000\" font-family=\"No Such Font, monospace\">MMMM</text></svg>\
    <svg width=\"400\" height=\"80\" style=\"display:block\">\
        <rect x=\"10\" y=\"10\" width=\"100\" height=\"40\" fill=\"#000\"/></svg>\
    </body></html>";

/// Fewer dark pixels than this in a row is a row with nothing in it: four
/// letters at 56 px, or the rectangle, put thousands there.
const MIN_INK: usize = 500;

/// How many pixels in a row of the page are dark.
fn dark_pixels_in_row(page: &HtmlPage, row: usize) -> usize {
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
        .filter(|pixel| pixel[..3].iter().all(|channel| *channel < 128))
        .count()
}

#[gpui::test]
fn an_inline_svg_draws_its_text_whatever_fontconfig_names_the_generic_families(
    cx: &mut TestAppContext,
) {
    let workspace = tempfile::tempdir().expect("a scratch directory");
    hermetic_fonts::use_only_the_bundled_fonts(workspace.path());

    cx.update(|cx| {
        let mut page = match HtmlPage::open(
            PAGE.to_string().into(),
            None,
            size(px(PAGE_WIDTH as f32), px((ROW_HEIGHT * ROWS.len()) as f32)),
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

        let deadline = Instant::now() + DEADLINE;
        let mut ink = [0; ROWS.len()];
        while Instant::now() < deadline {
            page.pump();
            for (row, count) in ink.iter_mut().enumerate() {
                *count = dark_pixels_in_row(&page, row);
            }
            if ink.iter().all(|count| *count >= MIN_INK) {
                return;
            }
            std::thread::sleep(Duration::from_millis(8));
        }
        let missing: Vec<_> = ROWS
            .iter()
            .zip(ink)
            .filter(|(_, count)| *count < MIN_INK)
            .map(|(row, count)| format!("{row}: {count} dark pixels"))
            .collect();
        panic!("an inline SVG's text was left out of the page: {missing:?}");
    });
}
