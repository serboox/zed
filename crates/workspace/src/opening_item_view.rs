use std::time::{Duration, Instant};

use gpui::{EventEmitter, FocusHandle, Focusable, Task};
use project::ProjectPath;
use ui::prelude::*;
use util::size::format_file_size;

use crate::Item;

/// How often the card redraws, so the elapsed reading it shows stays truthful
/// without repainting the pane every frame.
const REDRAW_INTERVAL: Duration = Duration::from_millis(100);

/// One full left-to-right-and-back trip of the sweeping segment.
const SWEEP_PERIOD: Duration = Duration::from_millis(1400);

/// The share of the track the sweeping segment covers.
const SWEEP_WIDTH: f32 = 0.3;

/// Holds the tab of a file that is still being read, so clicking a big file
/// never looks like the editor ignored the click.
pub struct OpeningItemView {
    path: ProjectPath,
    /// The file's size on disk, when the worktree has already scanned it.
    total_bytes: Option<u64>,
    started_at: Instant,
    focus_handle: FocusHandle,
    _redraw: Task<()>,
}

impl OpeningItemView {
    pub fn new(path: ProjectPath, total_bytes: Option<u64>, cx: &mut Context<Self>) -> Self {
        let started_at = cx.background_executor().now();
        let redraw = cx.spawn(async move |this, cx| {
            loop {
                cx.background_executor().timer(REDRAW_INTERVAL).await;
                if this.update(cx, |_, cx| cx.notify()).is_err() {
                    return;
                }
            }
        });

        Self {
            path,
            total_bytes,
            started_at,
            focus_handle: cx.focus_handle(),
            _redraw: redraw,
        }
    }

    /// The file's size on disk, when the worktree has already scanned it. This
    /// is the one figure about the load that the editor can state as fact.
    pub fn total_bytes(&self) -> Option<u64> {
        self.total_bytes
    }

    fn file_name(&self) -> SharedString {
        SharedString::new(
            self.path
                .path
                .file_name()
                .unwrap_or(self.path.path.as_unix_str()),
        )
    }

    fn elapsed(&self, cx: &App) -> Duration {
        cx.background_executor()
            .now()
            .saturating_duration_since(self.started_at)
    }

    /// The track and its sweeping segment. Nothing between
    /// `Workspace::load_path` and the `Fs` trait reports how many bytes it has
    /// read, so the segment sweeps rather than claiming a percentage.
    fn progress_track(&self, elapsed: Duration, cx: &App) -> Div {
        let phase = (elapsed.as_secs_f32() / SWEEP_PERIOD.as_secs_f32()).fract() * 2.0;
        let travel = 1.0 - SWEEP_WIDTH;
        let offset = if phase <= 1.0 {
            phase * travel
        } else {
            (2.0 - phase) * travel
        };

        h_flex()
            .w_full()
            .h(px(4.))
            .rounded_full()
            .overflow_hidden()
            .bg(cx.theme().colors().element_background)
            .child(div().flex_none().w(relative(offset)))
            .child(
                div()
                    .flex_none()
                    .w(relative(SWEEP_WIDTH))
                    .h_full()
                    .rounded_full()
                    .bg(cx.theme().colors().text_accent),
            )
    }
}

impl Item for OpeningItemView {
    type Event = ();

    fn tab_content_text(&self, detail: usize, _: &App) -> SharedString {
        let path = self
            .path
            .path
            .last_n_components(detail + 1)
            .unwrap_or(&self.path.path);
        SharedString::new(path.as_unix_str())
    }

    fn tab_tooltip_text(&self, _: &App) -> Option<SharedString> {
        Some(SharedString::new(format!(
            "Opening {}",
            self.path.path.as_unix_str()
        )))
    }
}

impl EventEmitter<()> for OpeningItemView {}

impl Focusable for OpeningItemView {
    fn focus_handle(&self, _: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl Render for OpeningItemView {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let elapsed = self.elapsed(cx);
        let colors = cx.theme().colors();
        let card_border = colors.border;
        let card_background = colors.elevated_surface_background;
        let body_background = colors.editor_background;

        v_flex()
            .size_full()
            .track_focus(&self.focus_handle(cx))
            .flex_none()
            .justify_center()
            .overflow_hidden()
            .bg(body_background)
            .key_context("OpeningItem")
            .debug_selector(|| "opening-item-body".into())
            .child(
                h_flex().size_full().justify_center().child(
                    v_flex()
                        .w(px(320.))
                        .flex_none()
                        .gap_2()
                        .p_4()
                        .rounded_lg()
                        .border_1()
                        .border_color(card_border)
                        .bg(card_background)
                        .child(Label::new(self.file_name()))
                        .child(
                            self.progress_track(elapsed, cx)
                                .debug_selector(|| "opening-item-progress".into()),
                        )
                        .child(
                            h_flex()
                                .justify_between()
                                .child(
                                    Label::new(format!("{:.1}s", elapsed.as_secs_f32()))
                                        .size(LabelSize::Small)
                                        .color(Color::Muted),
                                )
                                .when_some(self.total_bytes, |row, total_bytes| {
                                    row.child(
                                        Label::new(format_file_size(total_bytes, false))
                                            .size(LabelSize::Small)
                                            .color(Color::Muted),
                                    )
                                }),
                        ),
                ),
            )
    }
}
