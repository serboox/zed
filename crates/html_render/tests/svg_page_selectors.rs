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

use svg_rows::{drawing, rectangle};

use pixels::{is_black, is_cyan, is_green, is_magenta, is_red, is_yellow};

const DEADLINE: Duration = Duration::from_secs(45);

/// Set on a machine that is expected to have a working engine, so an engine
/// that fails to start is a failure rather than a note.
const REQUIRE: &str = "ZED_REQUIRE_HTML_ENGINE";

const ROW_HEIGHT: usize = 80;
const PAGE_WIDTH: usize = 400;

/// More pixels than this of the colour asked for in a row is a row painted
/// that colour: a rectangle is 32 000 pixels, half of one 16 000.
const MIN_PIXELS: usize = 500;

/// How long every row has to show what it should before the page is taken to be
/// done: a rectangle that is black while it waits for its rule looks like one that
/// no rule paints.
const STEADY_FOR: Duration = Duration::from_millis(1500);

fn page() -> String {
    let full = |class: &str| drawing(&rectangle(class, 0, 400));
    let mut rows = Vec::new();
    // 0: a rule through the markup above the drawing, beaten by a later but weaker one.
    rows.push(format!("<div class=\"card\">{}</div>", full("a")));
    // 1 and 2: `:not`.
    rows.push(full("n"));
    rows.push(full("n skip"));
    // 3: `:nth-child`, painting the second of two rectangles.
    rows.push(drawing(
        &(rectangle("p", 0, 200) + &rectangle("q", 200, 200)),
    ));
    // 4: a sibling combinator that is more specific than `:nth-child` and comes first.
    rows.push(drawing(
        &(rectangle("first", 0, 200) + &rectangle("second", 200, 200)),
    ));
    // 5: `:root` above the drawing.
    rows.push(full("r"));
    // 6 to 11: media queries that hold, that do not, and that nest.
    for class in ["m1", "m2", "m3", "m4", "m5", "m6"] {
        rows.push(full(class));
    }
    // 12: a cascade layer.
    rows.push(full("l"));
    // 13: a rule about an element that has no such ancestor.
    rows.push(full("o"));
    // 14: a rule from an imported stylesheet.
    rows.push(full("imp"));
    // 15: a rule in no layer, less specific than one in a layer, which still wins.
    rows.push(full("ly ly2"));
    // 16 and 17: `@supports` that holds and one that does not.
    rows.push(full("sup"));
    rows.push(full("nosup"));
    // 18: a drawing that carries the attribute the marks are made with.
    rows.push(drawing(
        "<rect class=\"stale\" data-zed-matched-rules=\"0\" x=\"0\" y=\"0\" width=\"400\" height=\"80\"/>",
    ));
    // 19: a rule with two selectors of different specificity, which is only as specific
    // for each element as the selector that matched it.
    rows.push(drawing(
        "<path id=\"b\" class=\"sp\" d=\"M0 0H200V80H0z\"/>\
         <path class=\"sp\" d=\"M200 0H400V80H200z\"/>",
    ));
    format!(
        "<html><head><style>\
            @import url(\"imported.css\");\
            .card svg .a {{ fill: #f00 }}\
            svg .a {{ fill: #00f }}\
            svg .n:not(.skip) {{ fill: #0f0 }}\
            svg rect:nth-child(2) {{ fill: #f0f }}\
            svg .first + .second {{ fill: #0ff }}\
            :root svg .r {{ fill: #ff0 }}\
            @media (min-width: 100px) {{ svg .m1 {{ fill: #0f0 }} }}\
            @media (max-width: 50px) {{ svg .m2 {{ fill: #f00 }} }}\
            @media print {{ svg .m3 {{ fill: #f00 }} }}\
            @media (prefers-color-scheme: light) {{ svg .m4 {{ fill: #0ff }} }}\
            @media (prefers-color-scheme: dark) {{ svg .m5 {{ fill: #f00 }} }}\
            @media (min-width: 100px) {{ @media (max-width: 3000px) {{ svg .m6 {{ fill: #f0f }} }} }}\
            @layer base {{ svg .l {{ fill: #ff0 }} }}\
            @layer base {{ svg rect.ly.ly2 {{ fill: #f00 }} }}\
            svg .ly {{ fill: #0f0 }}\
            #b, .sp {{ fill: #f00 }}\
            .sp {{ fill: #0f0 }}\
            @supports (display: grid) {{ svg .sup {{ fill: #0ff }} }}\
            @supports (display: definitely-not-a-thing) {{ svg .nosup {{ fill: #f00 }} }}\
            .outside .o {{ fill: #f00 }}\
        </style></head><body style=\"margin:0;background:#fff\">{}</body></html>",
        rows.concat()
    )
}

#[gpui::test]
fn an_inline_svg_follows_the_selectors_and_conditions_of_the_page(cx: &mut TestAppContext) {
    let workspace = tempfile::tempdir().expect("a scratch directory");
    hermetic_fonts::use_only_the_bundled_fonts(workspace.path());
    std::fs::write(
        workspace.path().join("imported.css"),
        "svg .imp { fill: #0ff }",
    )
    .expect("a stylesheet to import");
    let document = page();
    let rows = document.matches("<svg").count();

    // What each row has to show: (what the page says, row, colour, whether it must be there).
    let expectations: Vec<(&str, usize, fn([u8; 3]) -> bool, bool)> = vec![
        (
            "a rule through the markup above the drawing, more specific than a later one",
            0,
            is_red,
            true,
        ),
        (
            "`:not` keeps a rectangle it is not told to skip",
            1,
            is_green,
            true,
        ),
        (
            "`:not` leaves a rectangle it is told to skip",
            2,
            is_black,
            true,
        ),
        (
            "`:nth-child` leaves the first rectangle alone",
            3,
            is_black,
            true,
        ),
        (
            "`:nth-child` paints the second rectangle",
            3,
            is_magenta,
            true,
        ),
        (
            "a more specific rule beats `:nth-child`, which comes after it",
            4,
            is_cyan,
            true,
        ),
        (
            "it also takes the colour from `:nth-child` away",
            4,
            is_magenta,
            false,
        ),
        ("`:root` above the drawing", 5, is_yellow, true),
        ("a media query that holds", 6, is_green, true),
        (
            "a media query on the width that does not hold",
            7,
            is_black,
            true,
        ),
        ("a print media query", 8, is_black, true),
        (
            "a light colour scheme, which is the page's",
            9,
            is_cyan,
            true,
        ),
        ("a dark colour scheme, which is not", 10, is_black, true),
        ("nested media queries that hold", 11, is_magenta, true),
        ("a cascade layer", 12, is_yellow, true),
        (
            "a rule about an ancestor the drawing does not have",
            13,
            is_black,
            true,
        ),
        ("a rule of an imported stylesheet", 14, is_cyan, true),
        (
            "a rule in no layer beats a more specific one in a layer",
            15,
            is_green,
            true,
        ),
        ("`@supports` that holds", 16, is_cyan, true),
        ("`@supports` that does not hold", 17, is_black, true),
        (
            "an attribute of the page's own that looks like a mark",
            18,
            is_black,
            true,
        ),
        (
            "a rule is as specific for an element as the selector that matched it",
            19,
            is_red,
            true,
        ),
        (
            "so a less specific selector of the same rule loses to a later rule",
            19,
            is_green,
            true,
        ),
    ];
    assert_eq!(rows, 20, "the page has the rows the expectations are about");

    cx.update(|cx| {
        let mut page = match HtmlPage::open(
            document.into(),
            Some(workspace.path()),
            size(px(PAGE_WIDTH as f32), px((ROW_HEIGHT * rows) as f32)),
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
        let mut counts = vec![0; expectations.len()];
        let mut steady_since: Option<Instant> = None;
        while Instant::now() < deadline {
            page.pump();
            for (index, (_, row, wanted, _)) in expectations.iter().enumerate() {
                counts[index] = pixels::pixels_in_row(&page, *row, ROW_HEIGHT, wanted);
            }
            let settled = expectations
                .iter()
                .zip(&counts)
                .all(|((.., present), count)| (*count >= MIN_PIXELS) == *present);
            if !settled {
                steady_since = None;
            } else if steady_since.get_or_insert_with(Instant::now).elapsed() >= STEADY_FOR {
                return;
            }
            std::thread::sleep(Duration::from_millis(8));
        }
        let failed: Vec<_> = expectations
            .iter()
            .zip(&counts)
            .filter(|((.., present), count)| (**count >= MIN_PIXELS) != *present)
            .map(|((what, ..), count)| format!("{what}: {count} pixels"))
            .collect();
        panic!("the page's rules did not reach its drawings as its engine meant them to: {failed:#?}, every row: {counts:?}");
    });
}
