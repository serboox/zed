use crate::log_digest::{Advance, LogDigest, TerminalTail};
use crate::log_lens_view::LogLensView;
use crate::log_reader::{Level, LogLine};
use gpui::{
    AppContext as _, Bounds, Entity, Modifiers, Point as GpuiPoint, Size, TestAppContext,
    VisualTestContext, px,
};
use project::Project;
use serde_json::json;
use std::path::Path;
use terminal::{
    Terminal, TerminalBounds, TerminalBuilder,
    terminal_settings::{AlternateScroll, CursorShape},
};
use util::path;
use util::paths::PathStyle;
use workspace::{AppState, MultiWorkspace, Workspace};

const ZAP_CONSOLE: &str = include_str!("../test_data/zap_console.log");
const ZAP_JSON: &str = include_str!("../test_data/zap_json.log");
const ZAP_CONSOLE_REPEATED: &str = include_str!("../test_data/zap_console_repeated.log");
const LOGFMT: &str = include_str!("../test_data/logfmt.log");
const RUST_TRACING: &str = include_str!("../test_data/rust_tracing.log");
const PYTHON_LOGGING: &str = include_str!("../test_data/python_logging.log");
const JVM_LOGBACK: &str = include_str!("../test_data/jvm_logback.log");
const UNRECOGNISED: &str = include_str!("../test_data/unrecognised.log");
const SENTINEL: &str = "2026-09-07T11:23:53.999+0300\tINFO\tmain/main.go:1\tListening\t{\"version\": \"dev\", \"name\": \"finbox_wrapper_api\", \"revision\": \"HEAD\"}\n";

fn digest_of(fixture: &str) -> LogDigest {
    let mut digest = LogDigest::default();
    digest.read(fixture.trim_end_matches('\n'));
    digest
}

#[test]
fn a_line_no_reader_claims_comes_out_byte_identical() {
    let source = UNRECOGNISED.trim_end_matches('\n');
    let digest = digest_of(UNRECOGNISED);
    let lines: Vec<&LogLine> = digest.lines().iter().collect();
    assert!(
        lines.iter().all(|line| matches!(line, LogLine::Raw(_))),
        "no reader may claim any of {lines:?}"
    );
    let rebuilt: Vec<String> = lines.iter().map(|line| line.source_text()).collect();
    assert_eq!(
        rebuilt.join("\n"),
        source,
        "the rows must rebuild the input byte for byte"
    );

    // Each of the three shapes the fixture exists to protect, checked on its
    // own so a failure names the shape that broke.
    let expected: Vec<&str> = source.split('\n').collect();
    assert_eq!(rebuilt.len(), expected.len());
    assert_eq!(rebuilt[0], "Building the workspace");
    assert!(
        rebuilt[1].contains('\u{1b}') && rebuilt[1].contains('\r'),
        "the ANSI escapes and the carriage returns must survive: {:?}",
        rebuilt[1]
    );
    assert_eq!(rebuilt[2], "  make: leaving directory");
    for (rebuilt, expected) in rebuilt.iter().zip(expected) {
        assert_eq!(rebuilt.as_bytes(), expected.as_bytes());
    }
}

#[test]
fn a_field_that_varies_is_not_folded() {
    let digest = digest_of(ZAP_CONSOLE_REPEATED);
    let folded: Vec<String> = digest
        .folded_fields()
        .into_iter()
        .map(|field| field.name)
        .collect();
    assert_eq!(folded, vec!["version", "name", "revision"]);
    assert!(
        !digest.is_folded("domain_id"),
        "domain_id takes 34 different values and cannot be folded"
    );
}

#[test]
fn a_field_folded_earlier_reappears_in_the_rows_once_a_later_line_disagrees() {
    let mut digest = LogDigest::default();
    digest.read(ZAP_CONSOLE.trim_end_matches('\n'));
    assert!(digest.is_folded("name"), "name agrees on both lines so far");

    digest.read(
        "2026-09-07T11:23:53.245+0300\tINFO\ttranslations/translations.go:252\tFetch all translation files\t{\"version\": \"dev\", \"name\": \"other_service\", \"revision\": \"HEAD\"}",
    );
    assert!(
        !digest.is_folded("name"),
        "a later line disagreed, so name belongs back in the rows"
    );
    let event = digest.event(2).expect("the third line was read");
    let unfolded: Vec<&str> = digest
        .unfolded_fields(event)
        .iter()
        .map(|field| field.name.as_str())
        .collect();
    assert!(unfolded.contains(&"name"), "got {unfolded:?}");
    assert!(
        digest.is_folded("version") && digest.is_folded("revision"),
        "the fields that still agree stay folded"
    );
}

#[test]
fn thirty_four_identical_messages_become_one_row_saying_34() {
    let digest = digest_of(ZAP_CONSOLE_REPEATED);
    assert_eq!(digest.lines().len(), 34);
    assert_eq!(digest.rows().len(), 1, "34 identical messages are one row");
    let row = &digest.rows()[0];
    assert_eq!(row.count(), 34);
    assert!(row.is_collapsed());
    assert_eq!(
        row.lines,
        (0..34).collect::<Vec<usize>>(),
        "the originals are all still reachable from the row"
    );
}

#[test]
fn the_differing_numeric_field_is_summarised_as_a_contiguous_range() {
    let digest = digest_of(ZAP_CONSOLE_REPEATED);
    let row = &digest.rows()[0];
    assert_eq!(digest.differences(row), vec!["domain_id 1–34".to_string()]);
}

#[test]
fn a_run_whose_values_do_not_form_a_range_is_summarised_as_a_value_count() {
    let mut digest = LogDigest::default();
    for domain in [3, 9, 40] {
        digest.read(&format!(
            "2026-09-07T11:23:53.244+0300\tINFO\ttranslations/translations.go:252\tFetch all translation files\t{{\"domain_id\": {domain}}}"
        ));
    }
    let row = &digest.rows()[0];
    assert_eq!(row.count(), 3);
    assert_eq!(
        digest.differences(row),
        vec!["domain_id 3 values".to_string()]
    );
}

#[test]
fn both_zap_formats_produce_the_same_level_caller_message_and_fields_from_the_same_event() {
    let console = digest_of(ZAP_CONSOLE);
    let json = digest_of(ZAP_JSON);
    let from_console = console.event(1).expect("the second console line was read");
    let from_json = json.event(0).expect("the json line was read");

    assert_eq!(from_console.level, Some(Level::Info));
    assert_eq!(from_json.level, from_console.level);
    assert_eq!(
        from_console.caller.as_deref(),
        Some("translations/translations.go:252")
    );
    assert_eq!(from_json.caller, from_console.caller);
    assert_eq!(from_console.message, "Fetch all translation files");
    assert_eq!(from_json.message, from_console.message);

    let names_and_values = |event: &crate::log_reader::LogEvent| {
        event
            .fields
            .iter()
            .map(|field| (field.name.clone(), field.value.clone()))
            .collect::<Vec<_>>()
    };
    assert_eq!(
        names_and_values(from_console),
        vec![
            ("version".to_string(), "dev".to_string()),
            ("name".to_string(), "finbox_wrapper_api".to_string()),
            ("revision".to_string(), "HEAD".to_string()),
            ("domain_id".to_string(), "73".to_string()),
        ]
    );
    assert_eq!(
        names_and_values(from_json),
        vec![
            ("version".to_string(), "dev".to_string()),
            ("name".to_string(), "finbox_wrapper_api".to_string()),
            ("revision".to_string(), "HEAD".to_string()),
            ("domain_id".to_string(), "10".to_string()),
        ]
    );
    assert!(
        from_json.time.is_some() && from_console.time.is_some(),
        "both encoders carry a timestamp"
    );
}

#[test]
fn a_python_traceback_stays_one_block() {
    let digest = digest_of(PYTHON_LOGGING);
    assert_eq!(
        digest.rows().len(),
        3,
        "three log lines, and the traceback is not one of them: {:?}",
        digest.lines()
    );
    let failing = digest.event(1).expect("the failing line was read");
    assert_eq!(failing.message, "Cache load failed");
    assert_eq!(
        failing.block,
        vec![
            "Traceback (most recent call last):",
            "  File \"/srv/app/translations.py\", line 252, in fetch_all",
            "    cache = load(path)",
            "  File \"/srv/app/cache.py\", line 55, in load",
            "    raise ValueError(\"truncated cache\")",
            "ValueError: truncated cache",
        ]
    );
    let after = digest
        .event(2)
        .expect("the line after the traceback was read");
    assert_eq!(after.message, "Fetch all translation files");
}

#[test]
fn a_jvm_stack_trace_stays_one_block() {
    let digest = digest_of(JVM_LOGBACK);
    assert_eq!(digest.rows().len(), 3);
    let failing = digest.event(1).expect("the failing line was read");
    assert_eq!(failing.message, "Cache load failed");
    assert_eq!(failing.block.len(), 5, "got {:?}", failing.block);
    assert_eq!(
        failing.block[0],
        "java.lang.IllegalStateException: truncated cache"
    );
    assert!(failing.block[3].starts_with("Caused by: java.io.EOFException"));
    assert_eq!(failing.caller.as_deref(), Some("com.example.Cache"));
    assert_eq!(failing.field("thread"), Some("pool-1-thread-3"));
}

#[test]
fn logfmt_rust_and_jvm_lines_each_yield_a_level_and_a_message() {
    let logfmt = digest_of(LOGFMT);
    assert_eq!(logfmt.rows().len(), 2);
    let first = logfmt.event(0).expect("the first logfmt line was read");
    assert_eq!(first.level, Some(Level::Info));
    assert_eq!(first.message, "Fetch all translation files");
    assert_eq!(
        first.caller.as_deref(),
        Some("translations/translations.go:252")
    );
    assert_eq!(first.field("domain_id"), Some("73"));

    let tracing = digest_of(RUST_TRACING);
    assert_eq!(tracing.rows().len(), 3);
    let plain = tracing.event(0).expect("the first tracing line was read");
    assert_eq!(plain.level, Some(Level::Info));
    assert_eq!(plain.message, "Fetch all translation files");
    assert_eq!(
        plain.caller.as_deref(),
        Some("finbox_wrapper_api::translations")
    );
    let bracketed = tracing.event(2).expect("the bracketed line was read");
    assert_eq!(bracketed.level, Some(Level::Warn));
    assert_eq!(
        bracketed.message,
        "Falling back to the bundled translations"
    );

    let jvm = digest_of(JVM_LOGBACK);
    let last = jvm.event(2).expect("the last jvm line was read");
    assert_eq!(last.level, Some(Level::Debug));
    assert_eq!(last.message, "Done");
    assert_eq!(last.time.as_deref(), Some("11:23:53.245"));
}

#[test]
fn only_the_new_tail_reaches_the_readers_when_output_grows() {
    let mut tail = TerminalTail::default();
    let first = "one\ntwo\nthree\n";
    let (advance, provisional) = tail.advance(first);
    assert_eq!(advance, Advance::Restarted("one\ntwo".to_string()));
    assert_eq!(provisional, "three");

    let (advance, provisional) = tail.advance(first);
    assert_eq!(advance, Advance::Unchanged);
    assert_eq!(provisional, "three");

    let grown = "one\ntwo\nthree\nfour\nfive\n\n\n";
    let (advance, provisional) = tail.advance(grown);
    assert_eq!(advance, Advance::Appended("three\nfour".to_string()));
    assert_eq!(
        provisional, "five",
        "the blank rows the grid pads with are not lines"
    );

    // A run of identical lines is the case a one-line anchor cannot tell apart,
    // so the tail must still find its place in it.
    let mut tail = TerminalTail::default();
    let same = "same\n".repeat(40);
    tail.advance(&same);
    let grown_run = format!("{same}last\n");
    let (advance, provisional) = tail.advance(&grown_run);
    assert_eq!(advance, Advance::Appended("same".to_string()));
    assert_eq!(provisional, "last");
}

#[test]
fn a_terminal_that_scrolled_its_anchor_away_is_read_again_from_scratch() {
    let mut tail = TerminalTail::default();
    tail.advance("one\ntwo\nthree\n");
    let (advance, _) = tail.advance("wholly\ndifferent\noutput\n");
    assert_eq!(
        advance,
        Advance::Restarted("wholly\ndifferent".to_string()),
        "text that does not continue what was read must be read again"
    );
}

/// Builds a workspace over a fake project, a display-only terminal holding
/// `output`, and a lens reading that terminal.
///
/// The lens is the root view of its own window: painted bounds only exist for
/// the window that was drawn, and a lens nested in a workspace pane is not
/// reached by a drawn test frame. Every click below therefore lands in the real
/// element tree the running app renders, in a window of its own.
async fn init_lens<'a>(
    cx: &'a mut TestAppContext,
    bounds: TerminalBounds,
    output: &str,
) -> (
    Entity<Terminal>,
    Entity<LogLensView>,
    Entity<Workspace>,
    &'a mut VisualTestContext,
) {
    let app_state = cx.update(AppState::test);
    let fs = app_state.fs.as_fake().clone();
    cx.update(|cx| {
        theme_settings::init(theme::LoadThemes::JustBase, cx);
        editor::init(cx);
    });
    fs.insert_tree(
        path!("/project"),
        json!({
            "translations": {
                "translations.go": "package translations\n".repeat(300),
            },
        }),
    )
    .await;
    let project = Project::test(fs, [Path::new(path!("/project"))], cx).await;

    let terminal = cx.new(|cx| {
        TerminalBuilder::new_display_only_with_bounds(
            CursorShape::default(),
            AlternateScroll::On,
            None,
            0,
            cx.background_executor(),
            PathStyle::local(),
            bounds,
        )
        .subscribe(cx)
    });
    // A live stream always has a next line, and the last line of the grid is
    // where the cursor sits, so the lens holds it back until something follows
    // it. The tests are given that something.
    let bytes = format!("{output}{SENTINEL}")
        .replace('\n', "\r\n")
        .into_bytes();
    terminal.update(cx, |terminal, cx| {
        terminal.write_output(&bytes, cx);
    });

    let workspace = {
        let (multi_workspace, workspace_cx) =
            cx.add_window_view(|window, cx| MultiWorkspace::test_new(project, window, cx));
        multi_workspace.read_with(workspace_cx, |multi, _| multi.workspace().clone())
    };

    let (lens, cx) = cx.add_window_view({
        let terminal = terminal.clone();
        let workspace = workspace.downgrade();
        move |window, cx| LogLensView::new(terminal, workspace, "run".into(), window, cx)
    });
    cx.run_until_parked();
    let content = terminal.read_with(cx, |terminal, _| terminal.get_content());
    assert!(
        content.contains("Listening"),
        "the terminal must hold the output written to it: {content:?}"
    );
    draw(cx);
    (terminal, lens, workspace, cx)
}

/// A gpui test window never draws on its own, and painted bounds only exist
/// for the frame that was drawn.
fn draw(cx: &mut VisualTestContext) {
    cx.update(|window, cx| window.draw(cx).clear());
    cx.run_until_parked();
}

fn wide_bounds() -> TerminalBounds {
    TerminalBounds::new(
        px(5.),
        px(5.),
        Bounds {
            origin: GpuiPoint::default(),
            size: Size {
                width: px(2500.),
                height: px(200.),
            },
        },
    )
}

#[gpui::test]
async fn the_three_fields_identical_on_every_line_are_folded_into_the_header_and_absent_from_the_rows(
    cx: &mut TestAppContext,
) {
    let (_terminal, lens, _workspace, cx) = init_lens(cx, wide_bounds(), ZAP_CONSOLE).await;
    assert_eq!(
        lens.read_with(cx, |lens, _| lens.digest().rows().len()),
        2,
        "both lines were read as their own row"
    );

    for folded in ["version", "name", "revision"] {
        assert!(
            cx.debug_bounds(match folded {
                "version" => "LOG_LENS_HEADER_FIELD-version",
                "name" => "LOG_LENS_HEADER_FIELD-name",
                _ => "LOG_LENS_HEADER_FIELD-revision",
            })
            .is_some(),
            "{folded} is the same on every line, so the header must show it once"
        );
    }
    for row_field in [
        "LOG_LENS_FIELD-0-0-version",
        "LOG_LENS_FIELD-0-0-name",
        "LOG_LENS_FIELD-0-0-revision",
        "LOG_LENS_FIELD-1-0-version",
        "LOG_LENS_FIELD-1-0-name",
        "LOG_LENS_FIELD-1-0-revision",
    ] {
        assert!(
            cx.debug_bounds(row_field).is_none(),
            "{row_field} was folded into the header and must not be painted on the row"
        );
    }
    assert!(
        cx.debug_bounds("LOG_LENS_FIELD-0-0-items").is_some(),
        "a field that is not on every line still belongs on its row"
    );
    assert!(
        cx.debug_bounds("LOG_LENS_FIELD-1-0-domain_id").is_some(),
        "a field that is not on every line still belongs on its row"
    );
}

#[gpui::test]
async fn a_collapsed_row_expands_back_to_thirty_four_originals(cx: &mut TestAppContext) {
    let (_terminal, lens, _workspace, cx) =
        init_lens(cx, wide_bounds(), ZAP_CONSOLE_REPEATED).await;
    assert!(
        cx.debug_bounds("LOG_LENS_COUNT-0").is_some(),
        "the collapsed row must paint its count"
    );
    assert!(
        cx.debug_bounds("LOG_LENS_DIFFERENCE-0-0").is_some(),
        "the collapsed row must paint what differed"
    );
    assert!(
        cx.debug_bounds("LOG_LENS_MESSAGE-0-33").is_none(),
        "the 34th original is not painted while the row is collapsed"
    );

    let expander = cx
        .debug_bounds("LOG_LENS_EXPAND-0")
        .expect("the collapsed row must offer a way to expand");
    cx.simulate_click(expander.center(), Modifiers::none());
    cx.run_until_parked();
    draw(cx);

    assert_eq!(
        lens.read_with(cx, |lens, _| lens.expanded_rows().len()),
        1,
        "the click expanded the row"
    );
    for nth in [0usize, 17, 33] {
        assert!(
            cx.debug_bounds(match nth {
                0 => "LOG_LENS_MESSAGE-0-0",
                17 => "LOG_LENS_MESSAGE-0-17",
                _ => "LOG_LENS_MESSAGE-0-33",
            })
            .is_some(),
            "original {nth} of 34 must be painted once the row is expanded"
        );
    }
    assert!(
        cx.debug_bounds("LOG_LENS_COUNT-0").is_none(),
        "an expanded row no longer stands for a count"
    );
    assert_eq!(
        lens.read_with(cx, |lens, _| lens.digest().rows()[0].count()),
        34,
        "the originals were never discarded"
    );
}

#[gpui::test]
async fn the_caller_opens_the_file_at_that_line(cx: &mut TestAppContext) {
    let (_terminal, _lens, workspace, cx) = init_lens(cx, wide_bounds(), ZAP_CONSOLE).await;
    let caller = cx
        .debug_bounds("LOG_LENS_CALLER-1-0")
        .expect("the row must paint its caller");
    cx.simulate_click(caller.center(), Modifiers::none());
    cx.run_until_parked();

    let opened = workspace.read_with(cx, |workspace, cx| {
        workspace
            .active_item(cx)
            .and_then(|item| item.downcast::<editor::Editor>())
    });
    let opened = opened.expect("clicking the caller must open the file it names");
    let (path, row) = opened.update_in(cx, |editor, _, cx| {
        let path = editor
            .buffer()
            .read(cx)
            .as_singleton()
            .and_then(|buffer| buffer.read(cx).file().map(|file| file.path().to_string()));
        let row = editor
            .selections
            .newest::<language::Point>(&editor.display_snapshot(cx))
            .head()
            .row;
        (path, row)
    });
    assert_eq!(path.as_deref(), Some("translations/translations.go"));
    assert_eq!(row, 251, "line 252 is row 251 counted from zero");
}

#[gpui::test]
async fn a_line_the_terminal_wrapped_is_read_as_one_line(cx: &mut TestAppContext) {
    // The default bounds are a hundred columns, so every fixture line wraps
    // over several grid rows. `Terminal::get_content` rejoins them, because
    // alacritty only ends a row with a newline when it did not wrap.
    let (_terminal, lens, _workspace, cx) =
        init_lens(cx, TerminalBounds::default(), ZAP_CONSOLE).await;
    let (rows, message, fields) = lens.read_with(cx, |lens, _| {
        let digest = lens.digest();
        let event = digest.event(0).cloned();
        (
            digest.rows().len(),
            event.as_ref().map(|event| event.message.clone()),
            event.map(|event| event.fields.len()),
        )
    });
    assert_eq!(rows, 2, "two wrapped lines are still two rows");
    assert_eq!(message.as_deref(), Some("Cache loaded from file"));
    assert_eq!(fields, Some(5));
}

#[gpui::test]
async fn a_level_threshold_hides_the_lines_below_it(cx: &mut TestAppContext) {
    let (_terminal, lens, _workspace, cx) = init_lens(cx, wide_bounds(), ZAP_CONSOLE).await;
    assert!(cx.debug_bounds("LOG_LENS_MESSAGE-0-0").is_some());
    lens.update(cx, |lens, cx| {
        lens.set_threshold(Level::Info);
        cx.notify();
    });
    draw(cx);
    assert!(
        cx.debug_bounds("LOG_LENS_MESSAGE-0-0").is_none(),
        "the debug line is below the threshold"
    );
    assert!(
        cx.debug_bounds("LOG_LENS_MESSAGE-1-0").is_some(),
        "the info line is not"
    );
}
