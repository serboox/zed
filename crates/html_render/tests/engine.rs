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
    match HtmlPage::open(
        html.to_string().into(),
        None,
        size(px(width), px(height)),
        scale,
        cx,
    ) {
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

/// The colour at a point on the page, as red, green and blue.
fn colour_at(page: &HtmlPage, x: usize, y: usize) -> Option<[u8; 3]> {
    let frame = page.frame()?;
    let size = frame.size(0);
    let (width, height) = (
        u32::from(size.width) as usize,
        u32::from(size.height) as usize,
    );
    if x >= width || y >= height {
        return None;
    }
    let bytes = frame.as_bytes(0)?;
    let offset = (y * width + x) * 4;
    let pixel = bytes.get(offset..offset + 4)?;
    // The frames are premultiplied BGRA, which is what gpui draws.
    Some([pixel[2], pixel[1], pixel[0]])
}

/// The colour at the middle of the page, as red, green and blue.
fn middle_colour(page: &HtmlPage) -> Option<[u8; 3]> {
    let frame = page.frame()?;
    let size = frame.size(0);
    let (width, height) = (
        u32::from(size.width) as usize,
        u32::from(size.height) as usize,
    );
    colour_at(page, width / 2, height / 2)
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

/// Turns the engine over for a while, so that whatever was just handed to it --
/// an event, a script, a question -- is actually dealt with.
fn pump_for(page: &mut HtmlPage, how_long: Duration) {
    let deadline = Instant::now() + how_long;
    while Instant::now() < deadline {
        page.pump();
        std::thread::sleep(Duration::from_millis(8));
    }
}

/// Runs a script in the page and waits for the answer.
fn ask_selection_probe(page: &mut HtmlPage, script: &str) -> String {
    let answer = std::rc::Rc::new(std::cell::RefCell::new(None));
    page.evaluate(script, {
        let answer = answer.clone();
        move |text| *answer.borrow_mut() = Some(text)
    });
    let deadline = Instant::now() + DEADLINE;
    while Instant::now() < deadline && answer.borrow().is_none() {
        page.pump();
        std::thread::sleep(Duration::from_millis(8));
    }
    answer.borrow_mut().take().unwrap_or_default()
}

/// What the page says is selected, waited for: the answer comes back through the
/// engine's own loop.
fn ask_for_selection(page: &mut HtmlPage) -> String {
    let answer = std::rc::Rc::new(std::cell::RefCell::new(None));
    page.selected_text({
        let answer = answer.clone();
        move |text| *answer.borrow_mut() = Some(text)
    });
    let deadline = Instant::now() + DEADLINE;
    while Instant::now() < deadline && answer.borrow().is_none() {
        page.pump();
        std::thread::sleep(Duration::from_millis(8));
    }
    answer.borrow_mut().take().unwrap_or_default()
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
        let Some(mut stylesheet) =
            page(&document("background:rgb(255,0,0)", ""), 400., 300., 1., cx)
        else {
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
        let mut scripted = page(
            &document("background:rgb(255,0,0)", script),
            400.,
            300.,
            1.,
            cx,
        )
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
        let mut clicked = page(
            &document("background:rgb(255,0,0)", button),
            400.,
            300.,
            1.,
            cx,
        )
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

        // Dragging over text selects it. The engine has no selection of its own
        // outside form fields, so this exercises the page-side implementation
        // all the way through: the drag, the highlight it paints, and the text
        // it hands back for the clipboard.
        let text = "<p style=\"position:absolute;left:0;top:0;margin:0;\
         font:16px monospace;color:black\">SELECT ME PLEASE</p>";
        let mut selectable = page(&document("background:white", text), 600., 200., 1., cx)
            .expect("the engine started once already");
        wait_for_colour(&mut selectable, [255, 255, 255], "the page behind the text");

        let start = gpui::point(px(2.), px(8.));
        selectable.mouse_moved(start);
        selectable.mouse_down(start, MouseButton::Left);
        for x in [30., 60., 90., 120.] {
            selectable.mouse_moved(gpui::point(px(x), px(8.)));
            pump_for(&mut selectable, Duration::from_millis(120));
        }
        selectable.mouse_up(gpui::point(px(120.), px(8.)), MouseButton::Left);
        pump_for(&mut selectable, Duration::from_millis(400));

        let selected = ask_for_selection(&mut selectable);
        assert!(
            selected.starts_with("SELECT"),
            "the page should have handed back the text under the drag, not {selected:?}"
        );

        // A drag along one line takes that line, and nothing above or below it.
        let lines = "<p style=\"position:absolute;left:0;top:0;margin:0;font:16px monospace\">\
         AAAA BBBB CCCC</p>\
         <p style=\"position:absolute;left:0;top:40px;margin:0;font:16px monospace\">\
         DDDD EEEE FFFF</p>\
         <p style=\"position:absolute;left:0;top:80px;margin:0;font:16px monospace\">\
         GGGG HHHH IIII</p>";
        let mut three_lines = page(&document("background:white", lines), 600., 200., 1., cx)
            .expect("the engine started once already");
        wait_for_colour(
            &mut three_lines,
            [255, 255, 255],
            "the page behind three lines",
        );

        let middle_line = px(48.);
        three_lines.mouse_moved(gpui::point(px(2.), middle_line));
        three_lines.mouse_down(gpui::point(px(2.), middle_line), MouseButton::Left);
        for x in [40., 80., 110.] {
            three_lines.mouse_moved(gpui::point(px(x), middle_line));
            pump_for(&mut three_lines, Duration::from_millis(120));
        }
        three_lines.mouse_up(gpui::point(px(110.), middle_line), MouseButton::Left);
        pump_for(&mut three_lines, Duration::from_millis(400));

        let one_line = ask_for_selection(&mut three_lines);
        assert!(
            one_line.contains("DDDD") && !one_line.contains("AAAA") && !one_line.contains("IIII"),
            "a drag along the middle line should take the middle line, not {one_line:?}"
        );

        // The same drag, but ending past the right edge of the text: a reader
        // sweeping to the end of a line must not be given the rest of the page.
        three_lines.mouse_moved(gpui::point(px(2.), middle_line));
        three_lines.mouse_down(gpui::point(px(2.), middle_line), MouseButton::Left);
        for x in [200., 400., 580.] {
            three_lines.mouse_moved(gpui::point(px(x), middle_line));
            pump_for(&mut three_lines, Duration::from_millis(120));
        }
        three_lines.mouse_up(gpui::point(px(580.), middle_line), MouseButton::Left);
        pump_for(&mut three_lines, Duration::from_millis(400));

        // A page taller than its window, to find out what the engine's own
        // geometry means once the compositor has scrolled it.
        let tall = (1..=40)
            .map(|i| {
                format!("<p style=\"margin:0;font:16px monospace\">LINE{i:02} of the tall page</p>")
            })
            .collect::<String>();
        let mut scrollable = page(&document("background:white", &tall), 600., 200., 1., cx)
            .expect("the engine started once already");
        wait_for_colour(&mut scrollable, [255, 255, 255], "the tall page");
        // The shape of a real page: a column of wrapped text in a window,
        // scrolled with the wheel before anything is selected. This is where a
        // drag along one line was taking whole paragraphs.
        let column = (1..=20)
            .map(|i| {
                format!(
                    "<p style=\"max-width:40em\">Paragraph {i}. This page has no script, \
             no animation and no transition: once it is laid out and painted there is nothing left \
             for the engine to do, so any processor time spent while it is on screen is time the \
             preview is wasting.</p>"
                )
            })
            .collect::<String>();
        let mut wrapped = page(
            &document("background:white;font:16px/1.6 sans-serif", &column),
            1200.,
            900.,
            1.,
            cx,
        )
        .expect("the engine started once already");
        wait_for_colour(&mut wrapped, [255, 255, 255], "the column of text");
        wrapped.scrolled(
            gpui::point(px(600.), px(400.)),
            gpui::point(px(0.), px(-378.)),
        );
        pump_for(&mut wrapped, Duration::from_millis(600));
        let line = px(100.);
        wrapped.mouse_moved(gpui::point(px(465.), line));
        wrapped.mouse_down(gpui::point(px(465.), line), MouseButton::Left);
        for x in [520., 620., 720., 815.] {
            wrapped.mouse_moved(gpui::point(px(x), line));
            pump_for(&mut wrapped, Duration::from_millis(120));
        }
        wrapped.mouse_up(gpui::point(px(815.), line), MouseButton::Left);
        pump_for(&mut wrapped, Duration::from_millis(400));
        for probe in [
            "(function(){var w=document.querySelectorAll('[data-zed-selection=word]');\
             var out=[];[0,20,50,100,200,400,879].forEach(function(i){\
             var b=w[i].getBoundingClientRect();\
             out.push(i+':'+w[i].textContent+'@'+Math.round(b.top)+'h'+Math.round(b.height));});\
             return out.join(' | ');})()",
            "(function(){var w=document.querySelectorAll('[data-zed-selection=word]');\
             var zero=0,tall=0;for(var i=0;i<w.length;i++){\
             if(w[i].getBoundingClientRect().height>0)tall++;else zero++;}\
             return tall+' measured, '+zero+' with no box';})()",
            "(function(){var p=document.querySelectorAll('p');\
             var b=p[4].getBoundingClientRect();\
             return 'p5 top='+Math.round(b.top)+' h='+Math.round(b.height);})()",
        ] {
            println!("WRAP {}", ask_selection_probe(&mut wrapped, probe));
        }
        let swept = ask_for_selection(&mut wrapped);
        println!("WRAPPED selected {:?}", swept);
        drop(wrapped);

        // A wheel turn moves the page by what it was told, once: the delta the
        // editor hands over in editor pixels is what the page travels.
        let line_at_top = |page: &mut HtmlPage| {
            ask_selection_probe(
                page,
                "(function(){var e=document.elementFromPoint(10,100);\
                 return (e.textContent||'').slice(0,6)+'@'+window.scrollY;})()",
            )
        };
        let before = line_at_top(&mut scrollable);
        scrollable.scrolled(
            gpui::point(px(300.), px(100.)),
            gpui::point(px(0.), px(-60.)),
        );
        pump_for(&mut scrollable, Duration::from_millis(600));
        let after_one = line_at_top(&mut scrollable);
        assert_ne!(
            before, after_one,
            "one turn of the wheel should move the page"
        );
        assert!(
            after_one.ends_with("@60"),
            "the page should travel exactly as far as the wheel said, not {after_one:?}"
        );
        drop(scrollable);

        let past_the_edge = ask_for_selection(&mut three_lines);
        assert!(
            past_the_edge.contains("FFFF") && !past_the_edge.contains("GGGG"),
            "a drag past the end of a line should stop at that line, not {past_the_edge:?}"
        );
        drop(three_lines);

        // And the reader has to be able to see it: the highlight is painted over
        // the words, so the page is no longer plain white where they are.
        let highlight = (0..40)
            .flat_map(|x| (2..18).map(move |y| (x * 3, y)))
            .filter_map(|(x, y)| colour_at(&selectable, x, y))
            .find(|colour| colour[2] > colour[0].saturating_add(20));
        assert!(
            highlight.is_some(),
            "the selected words should be painted over with the highlight"
        );
        drop(selectable);

        // On a high-resolution display the page is painted at the display's own
        // resolution rather than stretched from a smaller picture.
        let mut sharp = page(&document("background:rgb(255,0,0)", ""), 400., 300., 2., cx)
            .expect("the engine started once already");
        wait_for_colour(
            &mut sharp,
            [255, 0, 0],
            "a page on a high-resolution display",
        );
        assert_eq!(
            frame_size(&sharp),
            Some((800, 600)),
            "the surface should carry two device pixels per editor pixel"
        );
        drop(sharp);

        #[cfg(target_os = "linux")]
        the_page_lends_its_own_memory(cx);
    });
}

/// A machine whose driver will allocate a buffer both sides can use hands the
/// window the very memory the page drew into. What matters is that the page is
/// really in there, so it is read back through the same descriptor the window
/// is given.
#[cfg(target_os = "linux")]
fn the_page_lends_its_own_memory(cx: &mut gpui::App) {
    // Two halves, so which way up the buffer is laid out can be told apart:
    // OpenGL draws from the bottom, and whoever reads the buffer starts at the
    // top.
    let mut lent = page(
        &document(
            "margin:0",
            "<div style=\"height:32px;background:rgb(255,0,0)\"></div>\
             <div style=\"height:32px;background:rgb(0,0,255)\"></div>",
        ),
        64.,
        64.,
        1.,
        cx,
    )
    .expect("the engine started once already");
    let deadline = Instant::now() + DEADLINE;
    while Instant::now() < deadline && colour_at(&lent, 32, 8) != Some([255, 0, 0]) {
        lent.pump();
        std::thread::sleep(Duration::from_millis(8));
    }
    assert_eq!(
        colour_at(&lent, 32, 8),
        Some([255, 0, 0]),
        "the copied frame should have the red half at the top"
    );

    let Some(frame) = lent.shared_frame() else {
        println!("SHARED: this machine copies frames");
        return;
    };
    assert_eq!(
        (frame.width, frame.height),
        (64, 64),
        "the lent buffer should be the size of the page"
    );
    assert_eq!(
        frame.modifier, 0,
        "only a buffer laid out row after row may be lent"
    );
    assert!(
        frame.stride >= frame.width * 4,
        "a row cannot be shorter than the pixels in it"
    );

    let length = (frame.offset + frame.stride * frame.height) as usize;
    #[allow(unsafe_code)]
    let mapped = unsafe {
        libc::mmap(
            std::ptr::null_mut(),
            length,
            libc::PROT_READ,
            libc::MAP_SHARED,
            std::os::fd::AsRawFd::as_raw_fd(&frame.descriptor),
            0,
        )
    };
    if mapped == libc::MAP_FAILED {
        println!(
            "SHARED: the page's buffer is lent but the processor may not read it, \
             so its contents are the window's word"
        );
        return;
    }

    // Whoever reads a shared buffer has to say so, or the graphics card is under
    // no obligation to have finished writing it.
    const SYNC_READ_START: u64 = 1 | (1 << 2);
    const SYNC_READ_END: u64 = (1 << 1) | (1 << 2);
    const SYNC: libc::c_ulong = 0x4008_6200;
    let descriptor = std::os::fd::AsRawFd::as_raw_fd(&frame.descriptor);
    #[allow(unsafe_code)]
    unsafe {
        libc::ioctl(descriptor, SYNC, &SYNC_READ_START);
    }
    let read_row = |row: u32| {
        let at = (frame.offset + frame.stride * row + 32 * 4) as usize;
        #[allow(unsafe_code)]
        let bytes = unsafe { std::slice::from_raw_parts(mapped as *const u8, length) };
        [bytes[at], bytes[at + 1], bytes[at + 2]]
    };
    let near_the_start = read_row(8);
    let near_the_end = read_row(56);
    #[allow(unsafe_code)]
    unsafe {
        libc::ioctl(descriptor, SYNC, &SYNC_READ_END);
        libc::munmap(mapped, length);
    }

    println!(
        "SHARED: the buffer starts with {near_the_start:?} and ends with {near_the_end:?}; \
         the page is red on top and blue below"
    );
    assert!(
        !close_to(near_the_start, near_the_end),
        "the two halves of the page should have come out different colours; the \
         buffer holds {near_the_start:?} throughout"
    );
    let starts_at_the_bottom = close_to(near_the_start, [0, 0, 255]);
    assert!(
        starts_at_the_bottom || close_to(near_the_start, [255, 0, 0]),
        "the lent buffer should hold the page the engine drew, red first, as the \
         format it is handed over under says; it starts with {near_the_start:?}"
    );
    // The window draws the buffer the way the frame says it is laid out, so what
    // is measured here has to be what is claimed. A page shown upside down is
    // this assertion going unmade.
    assert_eq!(
        frame.bottom_up,
        starts_at_the_bottom,
        "the frame says its first row is {}, and the buffer starts with \
         {near_the_start:?} where the page is red on top and blue below",
        if frame.bottom_up {
            "the bottom of the page"
        } else {
            "the top of the page"
        }
    );
}
