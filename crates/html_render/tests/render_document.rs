#![cfg(feature = "servo")]

use std::path::PathBuf;
use std::time::{Duration, Instant};

use gpui::{TestAppContext, px, size};
use html_render::HtmlPage;

#[path = "support/hermetic_fonts.rs"]
mod hermetic_fonts;

const DOCUMENT: &str = "ZED_HTML_RENDER_DOCUMENT";
const OUTPUT: &str = "ZED_HTML_RENDER_OUTPUT";
const WIDTH: &str = "ZED_HTML_RENDER_WIDTH";
const HEIGHT: &str = "ZED_HTML_RENDER_HEIGHT";
const SETTLE_SECONDS: &str = "ZED_HTML_RENDER_SETTLE_SECONDS";

fn number_from_environment(name: &str, default: u32) -> u32 {
    std::env::var(name)
        .ok()
        .and_then(|value| value.parse().ok())
        .unwrap_or(default)
}

/// Draws the document named by `ZED_HTML_RENDER_DOCUMENT` once, as the editor would,
/// and saves the picture to `ZED_HTML_RENDER_OUTPUT`. It does nothing without them:
/// `script/html-preview-parity` is what sets them, to put the picture beside a
/// browser's.
#[gpui::test]
fn render_the_document_named_in_the_environment(cx: &mut TestAppContext) {
    let path_from_environment = |name: &str| {
        std::env::var_os(name)
            .filter(|value| !value.is_empty())
            .map(PathBuf::from)
    };
    let (Some(document), Some(output)) = (
        path_from_environment(DOCUMENT),
        path_from_environment(OUTPUT),
    ) else {
        return;
    };
    let width = number_from_environment(WIDTH, 1400);
    let height = number_from_environment(HEIGHT, 4000);
    let settle = Duration::from_secs(u64::from(number_from_environment(SETTLE_SECONDS, 12)));
    let html = std::fs::read_to_string(&document)
        .unwrap_or_else(|error| panic!("reading {}: {error}", document.display()));
    let workspace = tempfile::tempdir().expect("a scratch directory");
    hermetic_fonts::use_only_the_bundled_fonts(workspace.path());

    cx.update(|cx| {
        let mut page = HtmlPage::open(
            html.into(),
            document.parent(),
            size(px(width as f32), px(height as f32)),
            1.,
            cx,
        )
        .expect("the engine starts");
        let until = Instant::now() + settle;
        while Instant::now() < until {
            page.pump();
            std::thread::sleep(Duration::from_millis(8));
        }
        let frame = page.frame().expect("a frame was drawn");
        let dimensions = frame.size(0);
        let mut bytes = frame.as_bytes(0).expect("the frame has pixels").to_vec();
        for pixel in bytes.chunks_exact_mut(4) {
            pixel.swap(0, 2);
        }
        image::RgbaImage::from_raw(
            u32::from(dimensions.width),
            u32::from(dimensions.height),
            bytes,
        )
        .expect("the pixels fit the size")
        .save(&output)
        .unwrap_or_else(|error| panic!("writing {}: {error}", output.display()));
    });
}
