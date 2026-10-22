#![cfg(feature = "servo")]

use std::cell::RefCell;
use std::rc::Rc;
use std::time::{Duration, Instant};

use gpui::{TestAppContext, point, px, size};
use html_render::HtmlPage;

const DEADLINE: Duration = Duration::from_secs(60);

/// Set on a machine that is expected to have a working engine, so an engine
/// that fails to start is a failure rather than a note.
const REQUIRE: &str = "ZED_REQUIRE_HTML_ENGINE";

const MOVES: u32 = 150;
const SECONDS: f64 = 1.0;

/// Every move the page hears of costs the engine a search of the whole document,
/// so a pointer that reports itself every few milliseconds is told to the page
/// at a pace the page can keep: about twenty-five a second at the 40
/// milliseconds it is set to, with some room for the pump's own timing.
const MOST_MOVES_HEARD: u32 = 40;

const DOCUMENT: &str = "<html><body style=\"margin:0;height:4000px\">\
    <script>var moves = 0, last = ''; \
    document.addEventListener('mousemove', function (event) { \
      moves += 1; last = event.clientX + ',' + event.clientY; });</script></body></html>";

fn pump_for(page: &mut HtmlPage, how_long: Duration) {
    let until = Instant::now() + how_long;
    while Instant::now() < until {
        page.pump();
        std::thread::sleep(Duration::from_millis(1));
    }
}

fn ask(page: &mut HtmlPage, script: &str) -> String {
    let answer = Rc::new(RefCell::new(None));
    let delivered = answer.clone();
    page.evaluate(script, move |text| *delivered.borrow_mut() = Some(text));
    let deadline = Instant::now() + DEADLINE;
    while answer.borrow().is_none() {
        page.pump();
        assert!(
            Instant::now() < deadline,
            "the page never answered the script"
        );
        std::thread::sleep(Duration::from_millis(1));
    }
    answer.borrow().clone().expect("the page answered")
}

#[gpui::test]
fn a_pointer_that_reports_itself_constantly_is_told_to_the_page_at_a_pace_it_can_keep(
    cx: &mut TestAppContext,
) {
    cx.update(|cx| {
        let mut page = match HtmlPage::open(
            DOCUMENT.to_string().into(),
            None,
            size(px(400.), px(300.)),
            1.,
            cx,
        ) {
            Ok(page) => page,
            Err(error) => {
                println!("HTML_ENGINE: unavailable: {error:#}");
                assert!(
                    std::env::var(REQUIRE).is_err(),
                    "{REQUIRE} is set, so the engine was expected to start: {error:#}"
                );
                return;
            }
        };
        println!("HTML_ENGINE: ok");

        let deadline = Instant::now() + DEADLINE;
        while page.is_loading() {
            page.pump();
            assert!(Instant::now() < deadline, "the page never finished loading");
            std::thread::sleep(Duration::from_millis(1));
        }
        pump_for(&mut page, Duration::from_millis(500));

        let started = Instant::now();
        for step in 0..MOVES {
            page.mouse_moved(point(px(100. + step as f32), px(200.)));
            let due = started + Duration::from_secs_f64(SECONDS * (step + 1) as f64 / MOVES as f64);
            while Instant::now() < due {
                page.pump();
                std::thread::sleep(Duration::from_millis(1));
            }
        }
        pump_for(&mut page, Duration::from_millis(400));

        let heard = ask(&mut page, "moves + '|' + last");
        println!("BENCH: the pointer made {MOVES} moves and the page heard {heard}");
        let (count, last) = heard.split_once('|').expect("a count and a position");
        let count: u32 = count.parse().expect("a count");
        assert!(
            count <= MOST_MOVES_HEARD,
            "the page heard of {count} of {MOVES} moves of the pointer: each one costs a search of \
             the whole document"
        );
        assert!(
            count >= 5,
            "the page heard of only {count} moves, so it cannot follow the pointer"
        );
        assert_eq!(
            last,
            format!("{},200", 100 + MOVES - 1),
            "the page has to end up told where the pointer stopped"
        );
    });
}
