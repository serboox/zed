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

/// Puts the engine's own log on the terminal.
///
/// A machine nobody can sit at is the only place some of these run -- the
/// graphics stack of a macOS runner is not one anybody here has -- and which
/// adapter the engine settled for, or why it settled for none, is said in the
/// log and nowhere else.
///
/// Only what the engine says for itself, never its workings: the glyph
/// rasterizer alone writes some thirty-five thousand debug lines while it builds
/// a gamma table, which buries the handful of lines this is here for and did for
/// the run that first switched the log on.
struct Printed;

impl log::Log for Printed {
    fn enabled(&self, _metadata: &log::Metadata<'_>) -> bool {
        true
    }

    fn log(&self, record: &log::Record<'_>) {
        println!(
            "LOG {} {}: {}",
            record.level(),
            record.target(),
            record.args()
        );
    }

    fn flush(&self) {}
}

fn print_the_log() {
    static ONCE: std::sync::Once = std::sync::Once::new();
    ONCE.call_once(|| {
        // Something else may have claimed the log already, and there is only one
        // to claim.
        if log::set_logger(&Printed).is_ok() {
            log::set_max_level(log::LevelFilter::Info);
        }
    });
}

fn page(html: &str, width: f32, height: f32, scale: f32, cx: &mut App) -> Option<HtmlPage> {
    print_the_log();
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

        the_page_can_be_taken_somewhere(cx);
        the_page_can_be_searched(cx);
        the_page_knows_how_wide_it_is(cx);
        the_page_is_laid_out_the_way_it_asks_to_be(cx);

        // Before the media backend is asked for anything. That backend starts a
        // whole library of its own, and a machine whose copy of it will not load
        // takes the process down rather than answering -- which would leave the
        // one thing these last two are about unsaid.
        #[cfg(target_os = "linux")]
        the_page_lends_its_own_memory(cx);
        #[cfg(target_os = "macos")]
        the_page_lends_its_own_surface(cx);
        #[cfg(target_os = "windows")]
        the_page_lends_its_own_texture(cx);

        the_engine_plays_no_media(cx);
        the_page_answers_the_developer_tools(cx);

        if std::env::var("ZED_HTML_BENCH").is_ok() {
            for scale in [1., 1.5, 2.] {
                how_long_a_frame_takes(scale, cx);
            }
            what_a_plain_page_costs(cx);
        }
    });
}

/// What a frame actually costs, at the size a preview really is. Not part of the
/// checks -- it asserts nothing and only runs when asked for -- but the numbers
/// are the only way to tell which of the two paths is worth having.
fn how_long_a_frame_takes(scale: f32, cx: &mut gpui::App) {
    let (width, height) = (1377., 658.);
    let paragraphs = (0..60)
        .map(|number| {
            format!(
                "<p style=\"font:16px sans-serif;margin:8px\">Paragraph {number} of a page that \
                 is long enough to lay out, wrap and paint in earnest, with enough words in it \
                 that the engine cannot pretend there is nothing to do.</p>"
            )
        })
        .collect::<String>();
    let Some(mut heavy) = page(
        &document("background:#fff;color:#111", &paragraphs),
        width,
        height,
        scale,
        cx,
    ) else {
        return;
    };
    wait_for_colour(&mut heavy, [255, 255, 255], "a page to measure");

    // Something has to change, or the engine settles and there is nothing to
    // time. What is measured is the whole way round: ask the page for a change,
    // and wait until a frame holding it is ready.
    let mut frames = Vec::new();
    let mut missed = 0;
    let mut turns = 0_u32;
    let mut turning = Duration::ZERO;
    for step in 0..40 {
        heavy.evaluate(
            &format!("document.body.style.marginTop = '{}px'", step % 7),
            |_| {},
        );
        let started = Instant::now();
        let deadline = started + Duration::from_secs(2);
        loop {
            let turn = Instant::now();
            let new_frame = heavy.pump();
            turning += turn.elapsed();
            turns += 1;
            // Turning the engine over as fast as the processor will go may be
            // holding up the very threads it is waiting for, so the pace can be
            // slowed to see whether that is what the wait is made of.
            if let Ok(pause) = std::env::var("ZED_HTML_PUMP_PAUSE_US")
                && let Ok(pause) = pause.parse::<u64>()
            {
                std::thread::sleep(Duration::from_micros(pause));
            }
            if new_frame {
                frames.push(started.elapsed());
                break;
            }
            if Instant::now() > deadline {
                missed += 1;
                break;
            }
        }
    }
    println!(
        "BENCH: {} turns of the engine for {} frames, {:?} of it inside the engine",
        turns,
        frames.len(),
        turning
    );
    report(
        "a change to the page",
        width,
        height,
        scale,
        &mut frames,
        missed,
    );

    // What the reader actually does: turn the wheel and move the pointer.
    let mut scrolls = Vec::new();
    for _ in 0..40 {
        heavy.scrolled(
            gpui::point(px(200.), px(200.)),
            gpui::point(px(0.), px(-60.)),
        );
        let started = Instant::now();
        let deadline = started + Duration::from_secs(2);
        loop {
            if heavy.pump() {
                scrolls.push(started.elapsed());
                break;
            }
            if Instant::now() > deadline {
                break;
            }
        }
    }
    report("a turn of the wheel", width, height, scale, &mut scrolls, 0);

    let mut moves = Vec::new();
    heavy.mouse_down(gpui::point(px(20.), px(20.)), MouseButton::Left);
    for step in 0..40 {
        heavy.mouse_moved(gpui::point(px(20. + step as f32 * 8.), px(40.)));
        let started = Instant::now();
        pump_for(&mut heavy, Duration::from_millis(0));
        heavy.pump();
        moves.push(started.elapsed());
    }
    heavy.mouse_up(gpui::point(px(340.), px(40.)), MouseButton::Left);
    report(
        "a pointer moved while selecting",
        width,
        height,
        scale,
        &mut moves,
        0,
    );
}

fn report(
    what: &str,
    width: f32,
    height: f32,
    scale: f32,
    taken: &mut Vec<Duration>,
    missed: usize,
) {
    if taken.is_empty() {
        println!("BENCH: {what} never finished");
        return;
    }
    let frames = taken;
    frames.sort();
    let at = |part: f64| frames[((frames.len() as f64 - 1.0) * part) as usize];
    let total: Duration = frames.iter().sum();
    println!(
        "BENCH: {what} at {}x{} device pixels: {} times, {missed} unanswered; {:?} at the \
         middle, {:?} at nine in ten, {:?} at worst, {:.1} a second",
        (width * scale) as u32,
        (height * scale) as u32,
        frames.len(),
        at(0.5),
        at(0.9),
        at(1.0),
        frames.len() as f64 / total.as_secs_f64()
    );
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
        // A width that is not a round number of pixels is the case that broke
        // in the editor: the allocator rounds a row up and the window does not,
        // so the page has to be given a buffer wide enough that neither has
        // anything left to round.
        100.,
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
        (100, 64),
        "the picture should be the size of the page"
    );
    assert!(
        frame.buffer_width >= frame.width,
        "the buffer cannot be narrower than the picture in it"
    );
    assert_eq!(
        frame.stride,
        frame.buffer_width * 4,
        "the window works out a row's length from the buffer's width alone, so \
         a buffer whose rows are any longer than that cannot be drawn"
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

    a_refused_buffer_goes_back_to_copying(&mut lent, &frame);
}

/// The page draws into a surface the window reads where it lies. What matters is
/// that the page is really in there, so it is read back through the very surface
/// the window is handed -- locked and addressed the way anyone reading an
/// `IOSurface` has to.
#[cfg(target_os = "macos")]
fn the_page_lends_its_own_surface(cx: &mut gpui::App) {
    use core_foundation::base::TCFType as _;
    use std::ffi::c_void;

    /// Read only, so the graphics card is not told the pixels are about to
    /// change.
    const READ_ONLY: u32 = 1;

    #[link(name = "CoreVideo", kind = "framework")]
    unsafe extern "C" {
        fn CVPixelBufferGetIOSurface(buffer: *const c_void) -> *mut c_void;
    }

    #[link(name = "IOSurface", kind = "framework")]
    unsafe extern "C" {
        fn IOSurfaceLock(surface: *mut c_void, options: u32, seed: *mut u32) -> i32;
        fn IOSurfaceUnlock(surface: *mut c_void, options: u32, seed: *mut u32) -> i32;
        fn IOSurfaceGetBaseAddress(surface: *mut c_void) -> *mut c_void;
        fn IOSurfaceGetBytesPerRow(surface: *mut c_void) -> usize;
        fn IOSurfaceGetWidth(surface: *mut c_void) -> usize;
        fn IOSurfaceGetHeight(surface: *mut c_void) -> usize;
        fn IOSurfaceGetAllocSize(surface: *mut c_void) -> usize;
    }

    // Two halves, so which way up the surface is laid out can be told apart:
    // OpenGL draws from the bottom, and whoever reads a buffer starts at the top.
    let mut lent = page(
        &document(
            "margin:0",
            "<div style=\"height:32px;background:rgb(255,0,0)\"></div>\
             <div style=\"height:32px;background:rgb(0,0,255)\"></div>",
        ),
        // A width that is not a round number of anything, so that a machine whose
        // allocator lengthens the rows is the case being read here: the row
        // length has to come from the surface and not from the width.
        100.,
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
        (100, 64),
        "the picture should be the size of the page"
    );

    let surface =
        unsafe { CVPixelBufferGetIOSurface(frame.descriptor.as_concrete_TypeRef() as *const _) };
    assert!(
        !surface.is_null(),
        "a frame that is lent has to be a surface, or the window has nothing to \
         read where it lies"
    );
    let (width, height, stride, length) = unsafe {
        (
            IOSurfaceGetWidth(surface),
            IOSurfaceGetHeight(surface),
            IOSurfaceGetBytesPerRow(surface),
            IOSurfaceGetAllocSize(surface),
        )
    };
    assert_eq!(
        (width, height),
        (frame.width as usize, frame.height as usize),
        "the frame should say the size the surface itself is"
    );
    assert_eq!(
        frame.stride as usize, stride,
        "the window works out where a row starts from what the frame says, so a \
         frame that reports a different row length from the surface would be read \
         crooked"
    );
    assert!(
        frame.stride >= frame.width * 4,
        "a row cannot be shorter than the pixels in it"
    );

    // The graphics card may not have finished the frame the moment it is handed
    // over, so this is asked for until the two halves of the page are there.
    let read_the_halves = || {
        let mut seed = 0;
        if unsafe { IOSurfaceLock(surface, READ_ONLY, &mut seed) } != 0 {
            return None;
        }
        let base = unsafe { IOSurfaceGetBaseAddress(surface) };
        if base.is_null() {
            unsafe { IOSurfaceUnlock(surface, READ_ONLY, &mut seed) };
            return None;
        }
        let bytes = unsafe { std::slice::from_raw_parts(base as *const u8, length) };
        // Blue, green, red and alpha is the order a surface of this format holds,
        // so the colour is read back to front.
        let read_row = |row: usize| {
            let at = row * stride + 32 * 4;
            bytes
                .get(at..at + 3)
                .map(|pixel| [pixel[2], pixel[1], pixel[0]])
        };
        let halves = read_row(8).zip(read_row(56));
        unsafe { IOSurfaceUnlock(surface, READ_ONLY, &mut seed) };
        halves
    };
    let mut halves = None;
    let deadline = Instant::now() + DEADLINE;
    while Instant::now() < deadline {
        halves = read_the_halves();
        if let Some((start, end)) = halves
            && !close_to(start, end)
        {
            break;
        }
        lent.pump();
        std::thread::sleep(Duration::from_millis(8));
    }
    let Some((near_the_start, near_the_end)) = halves else {
        panic!("the page's surface is lent but it would not be locked and read");
    };

    println!(
        "SHARED: the surface is {width}x{height} in rows of {stride} bytes; it starts with \
         {near_the_start:?} and ends with {near_the_end:?}; the page is red on top and blue below"
    );
    assert!(
        !close_to(near_the_start, near_the_end),
        "the two halves of the page should have come out different colours; the \
         surface holds {near_the_start:?} throughout"
    );
    let starts_at_the_bottom = close_to(near_the_start, [0, 0, 255]);
    assert!(
        starts_at_the_bottom || close_to(near_the_start, [255, 0, 0]),
        "the lent surface should hold the page the engine drew, blue first in \
         memory as the format it is handed over under says; it starts with \
         {near_the_start:?}"
    );
    // The window draws the surface the way the frame says it is laid out, so what
    // is measured here has to be what is claimed. A page shown upside down is
    // this assertion going unmade.
    assert_eq!(
        frame.bottom_up,
        starts_at_the_bottom,
        "the frame says its first row is {}, and the surface starts with \
         {near_the_start:?} where the page is red on top and blue below",
        if frame.bottom_up {
            "the bottom of the page"
        } else {
            "the top of the page"
        }
    );

    a_refused_buffer_goes_back_to_copying(&mut lent, &frame);
}

/// A window that looks at the page's buffer and cannot draw it says so on the
/// frame, and the page has to go back to copying rather than showing nothing
/// ever again. A page at rest has nothing more to draw of its own accord, which
/// is the case this is about: what has to arrive is a copied frame.
#[cfg(any(target_os = "linux", target_os = "macos"))]
fn a_refused_buffer_goes_back_to_copying(
    page: &mut HtmlPage,
    frame: &std::sync::Arc<gpui::SharedFrame>,
) {
    frame.refuse();
    assert!(
        page.shared_frame().is_none(),
        "a buffer the window has refused should not be offered again"
    );
    let deadline = Instant::now() + DEADLINE;
    while Instant::now() < deadline && colour_at(page, 32, 8) != Some([255, 0, 0]) {
        page.pump();
        std::thread::sleep(Duration::from_millis(8));
    }
    assert_eq!(
        colour_at(page, 32, 8),
        Some([255, 0, 0]),
        "the page should copy frames again once its buffer has been refused"
    );
}

/// The same measurements on a page with no text in it at all. Scrolling costs
/// more for each pixel than redrawing does, and this says whether that is the
/// text being laid down again or simply the pixels being moved.
fn what_a_plain_page_costs(cx: &mut gpui::App) {
    let (width, height) = (1377., 658.);
    let stripes = (0..200)
        .map(|number| {
            let shade = if number % 2 == 0 { "#fff" } else { "#888" };
            format!("<div style=\"height:40px;background:{shade}\"></div>")
        })
        .collect::<String>();
    let Some(mut plain) = page(&document("margin:0", &stripes), width, height, 2., cx) else {
        return;
    };
    let deadline = Instant::now() + DEADLINE;
    while Instant::now() < deadline && plain.frame().is_none() {
        plain.pump();
        std::thread::sleep(Duration::from_millis(8));
    }

    let mut scrolls = Vec::new();
    for _ in 0..40 {
        plain.scrolled(
            gpui::point(px(200.), px(200.)),
            gpui::point(px(0.), px(-60.)),
        );
        let started = Instant::now();
        let deadline = started + Duration::from_secs(2);
        loop {
            if plain.pump() {
                scrolls.push(started.elapsed());
                break;
            }
            if Instant::now() > deadline {
                break;
            }
        }
    }
    report(
        "a turn of the wheel, no text",
        width,
        height,
        2.,
        &mut scrolls,
        0,
    );
}

/// The scrollbar beside a live page is a decoration: the page scrolls inside the
/// engine, tells the editor where it stands, and is told where to go. Both
/// halves are checked against what the page actually shows.
fn the_page_can_be_taken_somewhere(cx: &mut gpui::App) {
    let tall = "<div style=\"height:64px;background:rgb(255,0,0)\"></div>\
         <div style=\"height:2000px;background:rgb(0,0,255)\"></div>";
    let mut page = page(&document("margin:0", tall), 100., 64., 1., cx)
        .expect("the engine started once already");
    wait_for_colour(&mut page, [255, 0, 0], "a page that can be scrolled");

    let standing = std::rc::Rc::new(std::cell::RefCell::new(None));
    let ask = |page: &mut HtmlPage,
               standing: &std::rc::Rc<std::cell::RefCell<Option<[f32; 3]>>>| {
        page.scroll_position({
            let standing = standing.clone();
            move |down, document, view| *standing.borrow_mut() = Some([down, document, view])
        });
        let deadline = Instant::now() + DEADLINE;
        while Instant::now() < deadline && standing.borrow().is_none() {
            page.pump();
            std::thread::sleep(Duration::from_millis(8));
        }
        standing.borrow_mut().take().expect("the page said nothing")
    };

    let [down, document, view] = ask(&mut page, &standing);
    assert_eq!(down, 0., "a page starts at the top");
    assert!(
        document > view && document >= 2000.,
        "the page should be taller than the view it is shown in: {document} against {view}"
    );

    // Where the scrollbar's thumb would take it.
    page.scroll_to(400.);
    wait_for_colour(&mut page, [0, 0, 255], "a page taken past its first screen");
    let [down, _, _] = ask(&mut page, &standing);
    assert!(
        (down - 400.).abs() < 2.,
        "the page should stand where it was taken, not at {down}"
    );

    // And back, so the thumb can be dragged either way.
    page.scroll_to(0.);
    wait_for_colour(&mut page, [255, 0, 0], "a page taken back to the top");
    let [down, _, _] = ask(&mut page, &standing);
    assert!(
        down < 2.,
        "the page should be back at the top, not at {down}"
    );
}

/// Looking for words in the page: the reader is taken to each place they appear
/// in turn, and what is found is what is selected.
fn the_page_can_be_searched(cx: &mut gpui::App) {
    let body = "<p>A giraffe is tall.</p><p>Another giraffe is taller.</p>\
         <p>No giraffe here, only a horse.</p>";
    let mut page = page(
        &document("margin:0;background:#fff", body),
        400.,
        300.,
        1.,
        cx,
    )
    .expect("the engine started once already");
    wait_for_colour(&mut page, [255, 255, 255], "a page to search");
    // The words are wrapped a moment after the page settles, and there is
    // nothing to search until they are. How long that moment takes depends on
    // the machine, so it is waited for rather than guessed at.
    let deadline = Instant::now() + DEADLINE;
    let mut wrapped = 0;
    while Instant::now() < deadline && wrapped == 0 {
        let trace = ask_selection_probe(
            &mut page,
            "window.__zedSelection ? window.__zedSelection.trace() : ''",
        );
        wrapped = trace
            .split("\"words\":")
            .nth(1)
            .and_then(|rest| rest.trim_end_matches('}').trim().parse::<usize>().ok())
            .unwrap_or(0);
        if wrapped == 0 {
            pump_for(&mut page, Duration::from_millis(100));
        }
    }
    assert!(
        wrapped > 0,
        "the page's words should have been wrapped by now"
    );

    let answer = std::rc::Rc::new(std::cell::RefCell::new(None));
    let look = |page: &mut HtmlPage, query: &str, forward: bool| {
        page.find(query, forward, {
            let answer = answer.clone();
            move |at, total| *answer.borrow_mut() = Some((at, total))
        });
        let deadline = Instant::now() + DEADLINE;
        while Instant::now() < deadline && answer.borrow().is_none() {
            page.pump();
            std::thread::sleep(Duration::from_millis(8));
        }
        answer.borrow_mut().take().expect("the page said nothing")
    };

    let (at, total) = look(&mut page, "giraffe", true);
    assert_eq!(
        (at, total),
        (1, 3),
        "there are three giraffes and the reader should be at the first"
    );
    assert_eq!(
        ask_for_selection(&mut page).to_lowercase(),
        "giraffe",
        "what was found should be what is selected"
    );

    let (at, _) = look(&mut page, "giraffe", true);
    assert_eq!(at, 2, "asking again should move to the next one");
    let (at, _) = look(&mut page, "giraffe", false);
    assert_eq!(at, 1, "and asking backwards should move back");

    let (at, total) = look(&mut page, "hippopotamus", true);
    assert_eq!(
        (at, total),
        (0, 0),
        "a word that is not there should be found nowhere"
    );

    // A search widened to whole words: part of one still finds it.
    let (_, total) = look(&mut page, "raff", true);
    assert_eq!(
        total, 3,
        "part of a word should find the words it is part of"
    );
}

/// What the page believes about the room it has. A page laid out for a wider
/// window than it is shown in puts everything to the left of what the reader
/// sees, so this is checked at both resolutions.
fn the_page_knows_how_wide_it_is(cx: &mut gpui::App) {
    for scale in [1., 2.] {
        let mut page = page(
            &document("margin:0;background:#fff", ""),
            500.,
            320.,
            scale,
            cx,
        )
        .expect("the engine started once already");
        wait_for_colour(&mut page, [255, 255, 255], "a page to measure itself");
        let said = ask_selection_probe(
            &mut page,
            "[window.innerWidth, window.innerHeight, window.devicePixelRatio,\
             document.documentElement.clientWidth].join(',')",
        );
        let numbers: Vec<f32> = said
            .split(',')
            .filter_map(|part| part.trim().parse::<f32>().ok())
            .collect();
        assert_eq!(numbers.len(), 4, "the page should have answered: {said:?}");
        println!("VIEWPORT: at {scale}x the page says {said} for a view of 500x320 editor pixels");
        assert!(
            (numbers[0] - 500.).abs() <= 1.,
            "the page should be as wide as the view it is shown in, not {}",
            numbers[0]
        );
        assert!(
            (numbers[1] - 320.).abs() <= 1.,
            "the page should be as tall as the view it is shown in, not {}",
            numbers[1]
        );
        assert!(
            (numbers[2] - scale).abs() < 0.01,
            "the page should know the resolution it is drawn at, not {}",
            numbers[2]
        );
        assert!(
            (numbers[3] - 500.).abs() <= 1.,
            "the document should have the whole width to lay itself out in, not {}",
            numbers[3]
        );
    }
}

/// A page built on a grid, laid out as a grid. The engine keeps several parts of
/// CSS behind switches of its own, all off by default, and a page built on any
/// of them comes out as one block under another against the left edge instead --
/// which is what the whole web looks like without this.
fn the_page_is_laid_out_the_way_it_asks_to_be(cx: &mut gpui::App) {
    let grid = "<div style=\"display:grid;grid-template-columns:1fr 1fr;width:400px\">\
         <div id=\"left\" style=\"height:40px;background:rgb(255,0,0)\"></div>\
         <div id=\"right\" style=\"height:40px;background:rgb(0,0,255)\"></div></div>";
    let mut page = page(
        &document("margin:0;background:#fff", grid),
        500.,
        320.,
        1.,
        cx,
    )
    .expect("the engine started once already");
    wait_for_colour(&mut page, [255, 255, 255], "a page laid out on a grid");

    let placed = ask_selection_probe(
        &mut page,
        "(function(){var r=document.getElementById('right').getBoundingClientRect();\
         return [Math.round(r.left), Math.round(r.top)].join(',');})()",
    );
    let numbers: Vec<f32> = placed
        .split(',')
        .filter_map(|part| part.trim().parse::<f32>().ok())
        .collect();
    assert_eq!(
        numbers.len(),
        2,
        "the page should have answered: {placed:?}"
    );
    // Beside its neighbour, not underneath it: two columns of a four hundred
    // pixel grid put the second one at two hundred.
    assert!(
        (numbers[0] - 200.).abs() <= 2.,
        "the second column should stand beside the first, at 200, not at {}",
        numbers[0]
    );
    assert!(
        numbers[1] <= 2.,
        "the second column should be level with the first, not {} below it",
        numbers[1]
    );

    // And the pixels agree: red on the left half, blue on the right.
    assert_eq!(
        colour_at(&page, 100, 20),
        Some([255, 0, 0]),
        "the first column should be drawn on the left"
    );
    assert_eq!(
        colour_at(&page, 300, 20),
        Some([0, 0, 255]),
        "the second column should be drawn beside it"
    );
}

/// Whether there is anything behind `<video>` and `<audio>`. There should not
/// be: the engine is built without a media backend on purpose, because the one
/// on offer takes the whole editor down on a machine whose plugins fail to
/// load. An engine that answers it can play something has had that backend put
/// back, which is a decision to make deliberately rather than discover.
fn the_engine_plays_no_media(cx: &mut gpui::App) {
    let mut page = page(
        &document("margin:0;background:#fff", ""),
        300.,
        200.,
        1.,
        cx,
    )
    .expect("the engine started once already");
    wait_for_colour(&mut page, [255, 255, 255], "a page to ask about media");

    let answer = ask_selection_probe(
        &mut page,
        "(function(){var v=document.createElement('video');\
         return ['mp4=' + v.canPlayType('video/mp4'),\
         'h264=' + v.canPlayType('video/mp4; codecs=\"avc1.42E01E\"'),\
         'webm=' + v.canPlayType('video/webm'),\
         'vp8=' + v.canPlayType('video/webm; codecs=\"vp8, vorbis\"'),\
         'vp9=' + v.canPlayType('video/webm; codecs=\"vp9\"'),\
         'ogg=' + v.canPlayType('video/ogg'),\
         'mp3=' + v.canPlayType('audio/mpeg'),\
         'wav=' + v.canPlayType('audio/wav')].join(' ');})()",
    );
    println!("MEDIA: {answer}");
    assert!(
        !answer.contains("maybe") && !answer.contains("probably"),
        "a media backend is linked again: the engine answered {answer:?}"
    );
}

/// What the panel in the dock asks the page, and what it gets back. Everything
/// there is read out of the page itself, so if the page stops answering any of
/// these the panel goes blank without a word.
fn the_page_answers_the_developer_tools(cx: &mut gpui::App) {
    let body = "<div id=\"outer\"><p class=\"inner\">Some words</p></div>\
         <script>console.log('hello from the page'); console.error('and a complaint');</script>";
    let mut page = page(
        &document("margin:0;background:#fff", body),
        400.,
        260.,
        1.,
        cx,
    )
    .expect("the engine started once already");
    wait_for_colour(&mut page, [255, 255, 255], "a page to inspect");
    pump_for(&mut page, Duration::from_millis(400));

    let ask = |page: &mut HtmlPage, question: &str| {
        let answer = std::rc::Rc::new(std::cell::RefCell::new(None));
        page.ask_tools(question, {
            let answer = answer.clone();
            move |text| *answer.borrow_mut() = Some(text)
        });
        let deadline = Instant::now() + DEADLINE;
        while Instant::now() < deadline && answer.borrow().is_none() {
            page.pump();
            std::thread::sleep(Duration::from_millis(8));
        }
        answer.borrow_mut().take().expect("the page said nothing")
    };

    // What the page is made of, one row per element.
    let tree = ask(&mut page, "tree(12)");
    assert!(
        tree.contains("div#outer") && tree.contains("p.inner"),
        "the tree should name the page's own elements: {tree}"
    );

    // What its scripts have said, and of what kind.
    let said = ask(&mut page, "said()");
    assert!(
        said.contains("hello from the page") && said.contains("and a complaint"),
        "the console should have kept what the page said: {said}"
    );
    assert!(
        said.contains("\"error\""),
        "a complaint should be marked as one: {said}"
    );
    // Taken once and not again: the panel reads them off as they come.
    let said_again = ask(&mut page, "said()");
    assert!(
        !said_again.contains("hello from the page"),
        "what has been read should not come back: {said_again}"
    );

    // One element in full: where it is, how big, and how it is painted.
    let about = ask(&mut page, "about(1)");
    assert!(
        about.contains("\"box\"") && about.contains("\"styles\"") && about.contains("display"),
        "an element should come with its box and its styles: {about}"
    );

    // And what the page fetched. A page of its own text fetched nothing, so an
    // empty list is the right answer -- but it must be a list.
    let fetched = ask(&mut page, "fetched()");
    assert!(
        fetched.starts_with('['),
        "what was fetched should come back as a list: {fetched}"
    );
}

/// The same page, lent the same way, on a machine whose graphics driver reflects
/// Direct3D textures into OpenGL. The page draws with OpenGL into a texture the
/// window can open on its own device, so what matters is that the page is really
/// in there -- and it is read back through the very handle the window is given.
#[cfg(target_os = "windows")]
fn the_page_lends_its_own_texture(cx: &mut gpui::App) {
    use windows::Win32::Foundation::{HANDLE, HMODULE};
    use windows::Win32::Graphics::Direct3D::{D3D_DRIVER_TYPE_HARDWARE, D3D_DRIVER_TYPE_WARP};
    use windows::Win32::Graphics::Direct3D11::{
        D3D11_CPU_ACCESS_READ, D3D11_CREATE_DEVICE_FLAG, D3D11_MAP_READ, D3D11_MAPPED_SUBRESOURCE,
        D3D11_SDK_VERSION, D3D11_TEXTURE2D_DESC, D3D11_USAGE_STAGING, D3D11CreateDevice,
        ID3D11Device, ID3D11DeviceContext, ID3D11Texture2D,
    };
    use windows::Win32::Graphics::Dxgi::Common::{DXGI_FORMAT_R8G8B8A8_UNORM, DXGI_SAMPLE_DESC};

    // Two halves, so which way up the texture is laid out can be told apart:
    // OpenGL draws from the bottom, and whoever reads a picture starts at the top.
    let mut lent = page(
        &document(
            "margin:0",
            "<div style=\"height:32px;background:rgb(255,0,0)\"></div>\
             <div style=\"height:32px;background:rgb(0,0,255)\"></div>",
        ),
        100.,
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
        (100, 64),
        "the picture should be the size of the page"
    );
    assert!(
        frame.buffer_width >= frame.width,
        "the texture cannot be narrower than the picture in it"
    );
    assert_eq!(
        frame.stride,
        frame.buffer_width * 4,
        "the window works a row's length out from the texture's width alone, so \
         a texture whose rows are any longer than that cannot be drawn"
    );
    assert_eq!(frame.offset, 0, "the picture starts where the texture does");

    // A device of this test's own, on the adapter surfman also asks for, so that
    // what is read here is read the way the window would read it.
    let mut device: Option<ID3D11Device> = None;
    let mut context: Option<ID3D11DeviceContext> = None;
    for kind in [D3D_DRIVER_TYPE_HARDWARE, D3D_DRIVER_TYPE_WARP] {
        #[allow(unsafe_code)]
        let made = unsafe {
            D3D11CreateDevice(
                None,
                kind,
                HMODULE::default(),
                D3D11_CREATE_DEVICE_FLAG(0),
                None,
                D3D11_SDK_VERSION,
                Some(&mut device),
                None,
                Some(&mut context),
            )
        };
        if made.is_ok() && device.is_some() {
            break;
        }
    }
    let (Some(device), Some(context)) = (device, context) else {
        println!("SHARED: the page's texture is lent but nothing here can open it");
        return;
    };

    #[allow(unsafe_code)]
    let opened = unsafe {
        let mut opened: Option<ID3D11Texture2D> = None;
        if device
            .OpenSharedResource(
                HANDLE(frame.descriptor as *mut std::ffi::c_void),
                &mut opened,
            )
            .is_err()
        {
            println!(
                "SHARED: the page's texture is lent but this device would not open it, \
                 so its contents are the window's word"
            );
            return;
        }
        opened.expect("an opened texture")
    };

    // What the frame claims about the texture, against what the texture says of
    // itself. The window places the picture from the frame's numbers alone, so a
    // page shown stretched or upside down is one of these assertions going unmade.
    #[allow(unsafe_code)]
    let description = unsafe {
        let mut description = D3D11_TEXTURE2D_DESC::default();
        opened.GetDesc(&mut description);
        description
    };
    assert_eq!(
        (description.Width, description.Height),
        (frame.buffer_width, frame.height),
        "the texture should be the size the frame says it is"
    );
    assert_eq!(
        description.Format, DXGI_FORMAT_R8G8B8A8_UNORM,
        "the frame is handed over red first, and the texture should hold it that way"
    );

    // Read through a copy, because the page's own texture is one the processor may
    // not touch. The copy's rows are as far apart as the driver chose, which is
    // exactly why the frame publishes the picture's own row and not a layout.
    let staging = D3D11_TEXTURE2D_DESC {
        Width: description.Width,
        Height: description.Height,
        MipLevels: 1,
        ArraySize: 1,
        Format: DXGI_FORMAT_R8G8B8A8_UNORM,
        SampleDesc: DXGI_SAMPLE_DESC {
            Count: 1,
            Quality: 0,
        },
        Usage: D3D11_USAGE_STAGING,
        BindFlags: 0,
        CPUAccessFlags: D3D11_CPU_ACCESS_READ.0 as u32,
        MiscFlags: 0,
    };
    #[allow(unsafe_code)]
    let (near_the_start, near_the_end, row_pitch) = unsafe {
        let mut copy = None;
        device
            .CreateTexture2D(&staging, None, Some(&mut copy))
            .expect("a texture to copy the page into");
        let copy: ID3D11Texture2D = copy.expect("a staging texture");
        context.CopyResource(&copy, &opened);
        let mut mapped = D3D11_MAPPED_SUBRESOURCE::default();
        context
            .Map(&copy, 0, D3D11_MAP_READ, 0, Some(&mut mapped))
            .expect("the copy should be readable");
        let bytes = std::slice::from_raw_parts(
            mapped.pData as *const u8,
            (mapped.RowPitch * description.Height) as usize,
        );
        let read_row = |row: u32| {
            let at = (row * mapped.RowPitch + 32 * 4) as usize;
            [bytes[at], bytes[at + 1], bytes[at + 2]]
        };
        let read = (read_row(8), read_row(56), mapped.RowPitch);
        context.Unmap(&copy, 0);
        read
    };

    println!(
        "SHARED: the texture starts with {near_the_start:?} and ends with {near_the_end:?}, \
         its copy has {row_pitch}-byte rows; the page is red on top and blue below"
    );
    assert!(
        !close_to(near_the_start, near_the_end),
        "the two halves of the page should have come out different colours; the \
         texture holds {near_the_start:?} throughout"
    );
    let starts_at_the_bottom = close_to(near_the_start, [0, 0, 255]);
    assert!(
        starts_at_the_bottom || close_to(near_the_start, [255, 0, 0]),
        "the lent texture should hold the page the engine drew, red first, as the \
         format it is handed over under says; it starts with {near_the_start:?}"
    );
    // The window draws the texture the way the frame says it is laid out, so what
    // is measured here has to be what is claimed.
    assert_eq!(
        frame.bottom_up,
        starts_at_the_bottom,
        "the frame says its first row is {}, and the texture starts with \
         {near_the_start:?} where the page is red on top and blue below",
        if frame.bottom_up {
            "the bottom of the page"
        } else {
            "the top of the page"
        }
    );
}
