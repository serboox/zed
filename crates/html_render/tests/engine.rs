#![cfg(feature = "servo")]

use std::time::{Duration, Instant};

use gpui::{App, TestAppContext, px, size};
use html_render::{HtmlPage, MouseButton};

/// Long enough for a cold engine to start, short enough that a wedged one is
/// reported rather than waited on until the runner gives up.
const DEADLINE: Duration = Duration::from_secs(60);

/// Set on a machine that is expected to have a working engine, so an engine
/// that fails to start is a failure rather than a note.
const REQUIRE: &str = "ZED_REQUIRE_HTML_ENGINE";

fn page(html: &str, width: f32, height: f32, scale: f32, cx: &mut App) -> Option<HtmlPage> {
    match HtmlPage::open(html.to_string().into(), None, size(px(width), px(height)), scale, cx) {
        Ok(page) => {
            println!("HTML_ENGINE: ok");
            Some(page)
        }
        Err(error) => {
            // A machine without a GPU driver the engine can talk to has no
            // engine, and the preview falls back to the Markdown rendering.
            // That is a fact about the machine, not a broken build.
            println!("HTML_ENGINE: unavailable: {error:#}");
            assert!(
                std::env::var(REQUIRE).is_err(),
                "{REQUIRE} is set, so the engine was expected to start: {error:#}"
            );
            None
        }
    }
}

fn document(body_style: &str, extra: &str) -> String {
    format!("<html><body style=\"margin:0;{body_style}\">{extra}</body></html>")
}

/// The colour at the middle of the page, as red, green and blue.
fn middle_colour(page: &HtmlPage) -> Option<[u8; 3]> {
    let frame = page.frame()?;
    let size = frame.size(0);
    let (width, height) = (u32::from(size.width) as usize, u32::from(size.height) as usize);
    let bytes = frame.as_bytes(0)?;
    let offset = ((height / 2) * width + width / 2) * 4;
    let pixel = bytes.get(offset..offset + 4)?;
    // The frames are premultiplied BGRA, which is what gpui draws.
    Some([pixel[2], pixel[1], pixel[0]])
}

fn close_to(colour: [u8; 3], want: [u8; 3]) -> bool {
    colour
        .iter()
        .zip(want.iter())
        .all(|(got, want)| got.abs_diff(*want) <= 2)
}

/// Turns the engine over until the middle of the page is the colour asked for.
fn wait_for_colour(page: &mut HtmlPage, want: [u8; 3], what: &str) {
    let deadline = Instant::now() + DEADLINE;
    let mut last = None;
    while Instant::now() < deadline {
        page.pump();
        if let Some(colour) = middle_colour(page) {
            if close_to(colour, want) {
                return;
            }
            last = Some(colour);
        }
        std::thread::sleep(Duration::from_millis(8));
    }
    panic!("{what}: the page never turned {want:?}, it stayed {last:?}");
}

fn frame_size(page: &HtmlPage) -> Option<(u32, u32)> {
    let frame = page.frame()?;
    let size = frame.size(0);
    Some((u32::from(size.width), u32::from(size.height)))
}

/// Everything the engine has to do, checked against the pixels it produced. It
/// is one test on purpose: the engine keeps process-wide state and its handles
/// are not `Send`, so a second instance on a second test thread is not
/// something to invite.
#[gpui::test]
fn the_engine_renders_scripts_and_answers_input(cx: &mut TestAppContext) {
    cx.update(|cx| {
        let Some(mut stylesheet) = page(
            &document("background:rgb(255,0,0)", ""),
            400.,
            300.,
            1.,
            cx,
        ) else {
            return;
        };

        wait_for_colour(&mut stylesheet, [255, 0, 0], "a page's own stylesheet");
        assert_eq!(
            frame_size(&stylesheet),
            Some((400, 300)),
            "the surface should be the size it was asked for"
        );

        // A script that runs after the load is what separates a live page from
        // a picture of one.
        let script = "<script>setTimeout(function () {\
             document.body.style.background = 'rgb(0,128,0)';\
         }, 0);</script>";
        let mut scripted = page(&document("background:rgb(255,0,0)", script), 400., 300., 1., cx)
            .expect("the engine started once already");
        wait_for_colour(&mut scripted, [0, 128, 0], "a script the page runs itself");
        drop(scripted);

        // The button covers the page so that any point lands on it, and is
        // transparent so that what shows through is the colour under test.
        let button = "<button id=\"b\" style=\"position:fixed;inset:0;border:0;padding:0;\
         background:transparent\"></button>\
         <script>document.getElementById('b').addEventListener('click', function () {\
             document.body.style.background = 'rgb(0,0,255)';\
         });\
         document.getElementById('b').addEventListener('keydown', function () {\
             document.body.style.background = 'rgb(128,0,128)';\
         });</script>";
        let mut clicked = page(&document("background:rgb(255,0,0)", button), 400., 300., 1., cx)
            .expect("the engine started once already");
        wait_for_colour(&mut clicked, [255, 0, 0], "the page before it is clicked");

        let middle = gpui::point(px(200.), px(150.));
        clicked.mouse_moved(middle);
        clicked.mouse_down(middle, MouseButton::Left);
        clicked.mouse_up(middle, MouseButton::Left);
        wait_for_colour(&mut clicked, [0, 0, 255], "a click the page handled");

        // The click left the button focused, so the keys have somewhere to go.
        clicked.key(keyboard_types::KeyboardEvent {
            state: keyboard_types::KeyState::Down,
            key: keyboard_types::Key::Character("a".into()),
            code: keyboard_types::Code::KeyA,
            location: keyboard_types::Location::Standard,
            modifiers: keyboard_types::Modifiers::empty(),
            repeat: false,
            is_composing: false,
        });
        wait_for_colour(&mut clicked, [128, 0, 128], "a key the page handled");
        drop(clicked);

        // A resized view gets a resized surface, and the page lays itself out
        // again to fill it.
        stylesheet.resize(size(px(640.), px(480.)), 1.);
        let deadline = Instant::now() + DEADLINE;
        while Instant::now() < deadline && frame_size(&stylesheet) != Some((640, 480)) {
            stylesheet.pump();
            std::thread::sleep(Duration::from_millis(8));
        }
        assert_eq!(
            frame_size(&stylesheet),
            Some((640, 480)),
            "the surface should follow the view"
        );
        drop(stylesheet);

        // On a high-resolution display the page is painted at the display's own
        // resolution rather than stretched from a smaller picture.
        let mut sharp = page(&document("background:rgb(255,0,0)", ""), 400., 300., 2., cx)
            .expect("the engine started once already");
        wait_for_colour(&mut sharp, [255, 0, 0], "a page on a high-resolution display");
        assert_eq!(
            frame_size(&sharp),
            Some((800, 600)),
            "the surface should carry two device pixels per editor pixel"
        );
    });
}
