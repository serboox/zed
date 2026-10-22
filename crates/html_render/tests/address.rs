#![cfg(feature = "servo")]

use std::time::{Duration, Instant};

use gpui::{TestAppContext, px, size};
use html_render::HtmlPage;

const DEADLINE: Duration = Duration::from_secs(60);

/// Set on a machine that is expected to have a working engine, so an engine
/// that fails to start is a failure rather than a note.
const REQUIRE: &str = "ZED_REQUIRE_HTML_ENGINE";

const SHOWN: &str = "file:///home/reader/docs/backlog.html";

fn pump_for(page: &mut HtmlPage, how_long: Duration) {
    let until = Instant::now() + how_long;
    while Instant::now() < until {
        page.pump();
        std::thread::sleep(Duration::from_millis(1));
    }
}

#[gpui::test]
fn a_document_of_the_editors_own_reads_as_the_path_it_came_from(cx: &mut TestAppContext) {
    cx.update(|cx| {
        let mut page = match HtmlPage::open(
            "<html><body style=\"height:3000px\"><h1 id=\"backlog\">Backlog</h1></body></html>"
                .to_string()
                .into(),
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
        pump_for(&mut page, Duration::from_millis(300));

        let engines = page
            .address()
            .expect("the engine has an address for the page");
        assert!(
            engines.path().ends_with("/page.html"),
            "the engine loads the document from a file of its own: {engines}"
        );

        let shown = url::Url::parse(SHOWN).expect("a url");
        assert_eq!(page.address_as(Some(&shown)), Some(shown.clone()));
        assert_eq!(
            page.address_as(None),
            None,
            "a document with no path of its own reads as nothing, not as a temporary file"
        );

        page.evaluate("location.hash = '#backlog'", |_| {});
        pump_for(&mut page, Duration::from_millis(500));
        let following_a_link = url::Url::parse(&format!("{SHOWN}#backlog")).expect("a url");
        assert_eq!(
            page.address_as(Some(&shown)),
            Some(following_a_link),
            "a link inside the page adds its fragment to the path that is shown"
        );
    });
}
