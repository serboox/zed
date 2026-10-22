#![cfg(all(feature = "servo", target_os = "linux"))]

use std::cell::RefCell;
use std::rc::Rc;
use std::time::{Duration, Instant};

use gpui::{TestAppContext, point, px, size};
use html_render::HtmlPage;

const DEADLINE: Duration = Duration::from_secs(60);

/// Set on a machine that is expected to have a working engine, so an engine
/// that fails to start is a failure rather than a note.
const REQUIRE: &str = "ZED_REQUIRE_HTML_ENGINE";

const WIDTH: f32 = 900.;
const HEIGHT: f32 = 600.;
const STEPS: u32 = 60;

/// How much of a document's hit test a scroll may cost the script thread. A scroll
/// moves the compositor's own node; whatever the script thread does beyond handing
/// the new offset to layout is waste, and finding what is under the pointer after
/// every frame cost about as much as one whole hit test.
const MOST_OF_A_HIT_TEST: f64 = 0.3;

/// A document with as many boxes as a long reference page: many tables, whose
/// cells hold several inline elements each.
fn long_document() -> String {
    let mut body = String::new();
    for table in 0..40 {
        body.push_str(&format!(
            "<h2>Section {table}</h2><table style=\"border-collapse:collapse;width:100%\">"
        ));
        for row in 0..14 {
            body.push_str("<tr>");
            for column in 0..4 {
                body.push_str(&format!(
                    "<td style=\"border:1px solid #ccc;padding:4px\"><code>cell {table}-{row}-{column}</code> \
                     and <code>more</code> text <b>bold</b> words here</td>"
                ));
            }
            body.push_str("</tr>");
        }
        body.push_str("</table><p>A paragraph of prose follows each table.</p>");
    }
    format!("<html><body style=\"margin:8px;font:14px sans-serif\">{body}</body></html>")
}

/// What the page's script thread has used of the processor so far.
fn script_thread_cpu() -> Duration {
    let ticks_a_second = unsafe { libc::sysconf(libc::_SC_CLK_TCK) } as f64;
    let mut total = 0_u64;
    let Ok(tasks) = std::fs::read_dir("/proc/self/task") else {
        return Duration::ZERO;
    };
    for task in tasks.flatten() {
        let Ok(stat) = std::fs::read_to_string(task.path().join("stat")) else {
            continue;
        };
        let (Some(open), Some(close)) = (stat.find('('), stat.rfind(')')) else {
            continue;
        };
        if !stat[open + 1..close].starts_with("Script") {
            continue;
        }
        let fields: Vec<&str> = stat[close + 2..].split_whitespace().collect();
        let user = fields.get(11).and_then(|field| field.parse::<u64>().ok());
        let system = fields.get(12).and_then(|field| field.parse::<u64>().ok());
        if let (Some(user), Some(system)) = (user, system) {
            total += user + system;
        }
    }
    Duration::from_secs_f64(total as f64 / ticks_a_second)
}

fn pump_for(page: &mut HtmlPage, how_long: Duration) {
    let until = Instant::now() + how_long;
    while Instant::now() < until {
        page.pump();
        std::thread::sleep(Duration::from_millis(1));
    }
}

/// One turn of the wheel, and the wait for the frame it makes.
fn scroll_once(page: &mut HtmlPage) {
    page.scrolled(point(px(300.), px(300.)), point(px(0.), px(-60.)));
    let deadline = Instant::now() + Duration::from_secs(2);
    while !page.pump() && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(1));
    }
}

fn script_cpu_per_scroll(page: &mut HtmlPage) -> Duration {
    pump_for(page, Duration::from_millis(500));
    let before = script_thread_cpu();
    for _ in 0..STEPS {
        scroll_once(page);
    }
    (script_thread_cpu() - before) / STEPS
}

/// How long the page's script thread takes to find what is at a point, as a
/// script in the page sees it.
fn hit_test_cost(page: &mut HtmlPage) -> Duration {
    let answer = Rc::new(RefCell::new(None));
    let delivered = answer.clone();
    page.evaluate(
        "(function () { var began = performance.now(); \
         for (var i = 0; i < 10; i++) { document.elementFromPoint(300, 300); } \
         return String((performance.now() - began) / 10); })()",
        move |text| *delivered.borrow_mut() = Some(text),
    );
    let deadline = Instant::now() + DEADLINE;
    while answer.borrow().is_none() {
        page.pump();
        assert!(
            Instant::now() < deadline,
            "the page never answered the script"
        );
        std::thread::sleep(Duration::from_millis(1));
    }
    let milliseconds = answer
        .borrow()
        .as_deref()
        .and_then(|text| text.parse::<f64>().ok())
        .expect("the script answered with a number");
    Duration::from_secs_f64(milliseconds / 1000.)
}

#[gpui::test]
fn a_pointer_over_a_long_page_does_not_make_every_scroll_pay_for_a_hit_test(
    cx: &mut TestAppContext,
) {
    cx.update(|cx| {
        let mut page = match HtmlPage::open(
            long_document().into(),
            None,
            size(px(WIDTH), px(HEIGHT)),
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
        pump_for(&mut page, Duration::from_secs(2));

        let hit_test = hit_test_cost(&mut page);
        println!("BENCH: a hit test of this page takes {hit_test:?}");
        assert!(
            hit_test > Duration::from_millis(2),
            "a hit test of {hit_test:?} is too quick for this page to tell the two apart"
        );

        let without_the_pointer = script_cpu_per_scroll(&mut page);
        println!("BENCH: a scroll costs the script thread {without_the_pointer:?} with no pointer");

        page.mouse_moved(point(px(300.), px(300.)));
        pump_for(&mut page, Duration::from_millis(500));

        let with_the_pointer = script_cpu_per_scroll(&mut page);
        println!("BENCH: a scroll costs the script thread {with_the_pointer:?} with the pointer over the page");

        assert!(
            with_the_pointer.as_secs_f64() < hit_test.as_secs_f64() * MOST_OF_A_HIT_TEST,
            "with the pointer over the page, a scroll cost the script thread {with_the_pointer:?}, \
             against {hit_test:?} for a whole hit test of the page and {without_the_pointer:?} with no \
             pointer there: something finds what is under the pointer after every frame"
        );
    });
}
