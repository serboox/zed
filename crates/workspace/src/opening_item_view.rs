use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};

use fs::FileReadProgress;
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

/// Which part of the wait the reader is sitting in. Taken once per frame, so
/// the words, the bar and the byte count cannot disagree about it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OpeningStep {
    /// Bytes are still coming off the disk.
    Reading { bytes_read: u64, total_bytes: u64 },
    /// Every byte is in, and the work that turns them into an editor is not
    /// finished. Also what is said when nothing reports the read at all.
    PreparingEditor,
}

impl OpeningStep {
    pub fn label(self) -> &'static str {
        match self {
            Self::Reading { .. } => "Reading…",
            Self::PreparingEditor => "Preparing the editor…",
        }
    }

    /// What the card says about size, beside the elapsed time.
    pub fn size_line(self, file_bytes: Option<u64>) -> Option<String> {
        match self {
            Self::Reading {
                bytes_read,
                total_bytes,
            } => Some(format!(
                "{} of {}",
                format_file_size(bytes_read, false),
                format_file_size(total_bytes, false)
            )),
            Self::PreparingEditor => file_bytes.map(|bytes| format_file_size(bytes, false)),
        }
    }

    /// How much of the track is filled in. A wait that reports no size gets
    /// nothing here, and is drawn with the sweeping segment instead.
    pub fn fraction_read(self) -> Option<f32> {
        match self {
            Self::Reading {
                bytes_read,
                total_bytes,
            } if total_bytes > 0 => {
                Some((bytes_read as f64 / total_bytes as f64).clamp(0.0, 1.0) as f32)
            }
            _ => None,
        }
    }

    fn debug_selector(self) -> &'static str {
        match self {
            Self::Reading { .. } => "opening-item-step-reading",
            Self::PreparingEditor => "opening-item-step-preparing",
        }
    }
}

/// Holds the tab of a file that is still being read, so clicking a big file
/// never looks like the editor ignored the click.
pub struct OpeningItemView {
    path: ProjectPath,
    /// Where the file sits on disk, which is the key the reader publishes its
    /// byte count under.
    abs_path: Option<PathBuf>,
    /// The file's size on disk, when the worktree has already scanned it.
    total_bytes: Option<u64>,
    started_at: Instant,
    focus_handle: FocusHandle,
    _redraw: Task<()>,
}

impl OpeningItemView {
    pub fn new(
        path: ProjectPath,
        abs_path: Option<PathBuf>,
        total_bytes: Option<u64>,
        cx: &mut Context<Self>,
    ) -> Self {
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
            abs_path,
            total_bytes,
            started_at,
            focus_handle: cx.focus_handle(),
            _redraw: redraw,
        }
    }

    /// The read of this file, while it is still bringing bytes in. A read that
    /// has delivered its last byte is left out: what is waited on after that is
    /// the editor being built, which is a different thing to say.
    fn read_in_flight(&self) -> Option<Arc<FileReadProgress>> {
        self.abs_path
            .as_deref()
            .and_then(fs::file_read_progress)
            .filter(|read| !read.is_finished())
    }

    pub fn step(&self) -> OpeningStep {
        match self.read_in_flight() {
            Some(read) => OpeningStep::Reading {
                bytes_read: read.bytes_read(),
                total_bytes: read.total_bytes(),
            },
            None => OpeningStep::PreparingEditor,
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

    fn track(cx: &App) -> Div {
        h_flex()
            .w_full()
            .h(px(4.))
            .rounded_full()
            .overflow_hidden()
            .bg(cx.theme().colors().element_background)
    }

    /// The track filled to how much of the file is in.
    fn filled_track(fraction_read: f32, cx: &App) -> Div {
        Self::track(cx).child(
            div()
                .flex_none()
                .w(relative(fraction_read))
                .h_full()
                .rounded_full()
                .bg(cx.theme().colors().text_accent)
                .debug_selector(|| "opening-item-progress-fill".into()),
        )
    }

    /// The track and its sweeping segment, for a wait whose length nothing
    /// reports. The segment sweeps rather than claiming a percentage.
    fn sweeping_track(elapsed: Duration, cx: &App) -> Div {
        let phase = (elapsed.as_secs_f32() / SWEEP_PERIOD.as_secs_f32()).fract() * 2.0;
        let travel = 1.0 - SWEEP_WIDTH;
        let offset = if phase <= 1.0 {
            phase * travel
        } else {
            (2.0 - phase) * travel
        };

        Self::track(cx)
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
        let step = self.step();
        let step_selector = step.debug_selector();
        let size_line = step.size_line(self.total_bytes);
        let track = match step.fraction_read() {
            Some(fraction_read) => Self::filled_track(fraction_read, cx),
            None => Self::sweeping_track(elapsed, cx),
        };

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
                            h_flex()
                                .w_full()
                                .debug_selector(move || step_selector.into())
                                .child(
                                    Label::new(step.label())
                                        .size(LabelSize::Small)
                                        .color(Color::Muted),
                                ),
                        )
                        .child(track.debug_selector(|| "opening-item-progress".into()))
                        .child(
                            h_flex()
                                .justify_between()
                                .child(
                                    Label::new(format!("{:.1}s", elapsed.as_secs_f32()))
                                        .size(LabelSize::Small)
                                        .color(Color::Muted),
                                )
                                .when_some(size_line, |row, size_line| {
                                    row.child(
                                        h_flex()
                                            .debug_selector(|| "opening-item-size".into())
                                            .child(
                                                Label::new(size_line)
                                                    .size(LabelSize::Small)
                                                    .color(Color::Muted),
                                            ),
                                    )
                                }),
                        ),
                ),
            )
    }
}
