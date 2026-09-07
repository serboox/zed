use crate::log_digest::{Advance, LogDigest, Row, TerminalTail};
use crate::log_reader::{Level, LogLine};
use collections::HashSet;
use editor::{Editor, EditorEvent};
use gpui::{
    AnyElement, App, Entity, EventEmitter, FocusHandle, Focusable, ScrollHandle, Subscription,
    Task, WeakEntity, Window,
};
use terminal::Terminal;
use ui::cyberpunk::{self, Severity};
use ui::{ScrollAxes, Scrollbars, WithScrollbar as _, prelude::*};
use workspace::{Item, Workspace};

#[cfg(not(test))]
use workspace::path_link::possible_open_target;
#[cfg(test)]
use workspace::path_link::{BackgroundPathChecks, possible_open_target_with_fs_checks};

/// Reads a run terminal's own output and lays it out.
///
/// The terminal is never touched: the lens is a second reader of the same text,
/// so stdin, ANSI colour, progress bars and full-screen programs keep working
/// in the terminal that remains the default.
pub struct LogLensView {
    terminal: Entity<Terminal>,
    workspace: WeakEntity<Workspace>,
    title: SharedString,
    digest: LogDigest,
    tail: TerminalTail,
    /// The last line of the grid, kept out of the digest until a newer line
    /// appears below it: a line the program is still writing must not be frozen
    /// into a row, and must not fold a field on its half-written value.
    provisional: Option<LogLine>,
    threshold: Level,
    filter: Entity<Editor>,
    expanded: HashSet<usize>,
    focus_handle: FocusHandle,
    scroll_handle: ScrollHandle,
    open_target: Task<()>,
    _subscriptions: Vec<Subscription>,
}

impl LogLensView {
    pub fn new(
        terminal: Entity<Terminal>,
        workspace: WeakEntity<Workspace>,
        title: SharedString,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let filter = cx.new(|cx| {
            let mut editor = Editor::single_line(window, cx);
            editor.set_placeholder_text("Filter messages", window, cx);
            editor
        });
        let mut subscriptions = vec![
            cx.subscribe(&terminal, |lens, _, event, cx| {
                if matches!(event, terminal::Event::Wakeup) {
                    lens.read_terminal(cx);
                }
            }),
            cx.subscribe(&filter, |_, _, event: &EditorEvent, cx| {
                if matches!(event, EditorEvent::BufferEdited) {
                    cx.notify();
                }
            }),
        ];
        subscriptions.push(cx.observe(&terminal, |lens, _, cx| lens.read_terminal(cx)));
        let mut lens = Self {
            terminal,
            workspace,
            title,
            digest: LogDigest::default(),
            tail: TerminalTail::default(),
            provisional: None,
            threshold: Level::Debug,
            filter,
            expanded: HashSet::default(),
            focus_handle: cx.focus_handle(),
            scroll_handle: ScrollHandle::new(),
            open_target: Task::ready(()),
            _subscriptions: subscriptions,
        };
        lens.read_terminal(cx);
        lens
    }

    pub fn reads(&self, terminal: &Entity<Terminal>) -> bool {
        self.terminal == *terminal
    }

    /// Reads whatever the terminal has that the lens has not seen. Only the new
    /// tail reaches the readers; the scrollback above it is already rows.
    pub fn read_terminal(&mut self, cx: &mut Context<Self>) {
        let content = self.terminal.read(cx).get_content();
        let (advance, provisional) = self.tail.advance(&content);
        let provisional = (!provisional.trim().is_empty()).then(|| {
            crate::log_reader::read_line(provisional)
                .map_or_else(|| LogLine::Raw(provisional.to_string()), LogLine::Read)
        });
        let changed = provisional != self.provisional;
        self.provisional = provisional;
        match advance {
            Advance::Unchanged => {
                if changed {
                    cx.notify();
                }
            }
            Advance::Appended(text) => {
                self.digest.read(&text);
                cx.notify();
            }
            Advance::Restarted(text) => {
                self.digest.clear();
                self.expanded.clear();
                self.digest.read(&text);
                cx.notify();
            }
        }
    }

    fn visible_rows(&self, cx: &App) -> Vec<usize> {
        let needle = self.filter.read(cx).text(cx);
        self.digest
            .rows()
            .iter()
            .enumerate()
            .filter(|(_, row)| self.digest.matches_filter(row, self.threshold, &needle))
            .map(|(at, _)| at)
            .collect()
    }

    fn open_caller(&mut self, caller: &str, window: &mut Window, cx: &mut Context<Self>) {
        let working_directory = self.terminal.read(cx).working_directory();
        let workspace = self.workspace.clone();
        let caller = caller.to_string();
        self.open_target = cx.spawn_in(window, async move |_, cx| {
            let Ok(found) = cx.update(|_, cx| {
                #[cfg(not(test))]
                {
                    possible_open_target(&workspace, &caller, working_directory.as_deref(), cx)
                }
                #[cfg(test)]
                {
                    possible_open_target_with_fs_checks(
                        &workspace,
                        &caller,
                        working_directory.as_deref(),
                        cx,
                        BackgroundPathChecks::LocalFileSystem,
                    )
                }
            }) else {
                return;
            };
            let Some(target) = found.await else {
                return;
            };
            if let Err(error) = editor::items::open_resolved_target(&workspace, &target, cx).await {
                log::warn!("log lens could not open {caller}: {error:#}");
            }
        });
    }
}

fn severity_of(level: Option<Level>) -> Severity {
    match level {
        Some(Level::Trace) | Some(Level::Debug) => Severity::Debug,
        Some(Level::Warn) => Severity::Warn,
        Some(Level::Error) | Some(Level::Fatal) => Severity::Error,
        Some(Level::Info) | None => Severity::Info,
    }
}

impl LogLensView {
    fn render_header(&self) -> AnyElement {
        let folded = self.digest.folded_fields();
        let mut header = h_flex()
            .id("log-lens-header")
            .flex_wrap()
            .flex_none()
            .gap(cyberpunk::SPACE_14)
            .px(cyberpunk::SPACE_14)
            .py(cyberpunk::SPACE_8)
            .border_b_1()
            .border_color(cyberpunk::border_dim())
            .bg(cyberpunk::surface())
            .child(
                Label::new(self.title.clone())
                    .color(Color::Custom(cyberpunk::text_primary()))
                    .into_any_element(),
            );
        for field in &folded {
            let name = field.name.clone();
            header = header.child(
                h_flex()
                    .debug_selector(move || format!("LOG_LENS_HEADER_FIELD-{name}"))
                    .gap(cyberpunk::SPACE_4)
                    .child(
                        Label::new(SharedString::from(field.name.clone()))
                            .size(LabelSize::Small)
                            .color(Color::Custom(cyberpunk::text_tertiary())),
                    )
                    .child(
                        Label::new(SharedString::from(field.value.clone()))
                            .size(LabelSize::Small)
                            .color(Color::Custom(cyberpunk::text_secondary())),
                    ),
            );
        }
        if folded.is_empty() {
            header = header.child(
                Label::new("No field is the same on every line")
                    .size(LabelSize::Small)
                    .color(Color::Custom(cyberpunk::text_tertiary())),
            );
        }
        header.into_any_element()
    }

    fn render_filters(&self, cx: &mut Context<Self>) -> AnyElement {
        let threshold = self.threshold;
        let buttons = Level::THRESHOLDS.map(|level| {
            let chosen = level == threshold;
            Button::new(("log-lens-threshold", level as usize), level.label())
                .label_size(LabelSize::Small)
                .style(if chosen {
                    cyberpunk::Rank::Accent.style()
                } else {
                    cyberpunk::Rank::Quiet.style()
                })
                .on_click(cx.listener(move |lens, _, _, cx| {
                    lens.threshold = level;
                    cx.notify();
                }))
                .into_any_element()
        });
        h_flex()
            .flex_none()
            .gap(cyberpunk::SPACE_8)
            .px(cyberpunk::SPACE_14)
            .py(cyberpunk::SPACE_8)
            .border_b_1()
            .border_color(cyberpunk::border_dim())
            .child(cyberpunk::segmented(buttons))
            .child(
                div()
                    .flex_1()
                    .min_w(px(120.))
                    .px(cyberpunk::SPACE_8)
                    .py(px(2.))
                    .rounded(cyberpunk::RADIUS)
                    .border_1()
                    .border_color(cyberpunk::border_raised())
                    .bg(cyberpunk::surface())
                    .child(self.filter.clone()),
            )
            .into_any_element()
    }

    fn render_row(&self, at: usize, row: &Row, cx: &mut Context<Self>) -> AnyElement {
        if self.expanded.contains(&at) {
            return v_flex()
                .children(
                    row.lines
                        .iter()
                        .filter_map(|line| self.digest.line(*line))
                        .enumerate()
                        .map(|(nth, line)| self.render_line(at, nth, row, line, cx)),
                )
                .into_any_element();
        }
        let Some(line) = row.lines.first().and_then(|line| self.digest.line(*line)) else {
            return div().into_any_element();
        };
        self.render_line(at, 0, row, line, cx)
    }

    fn render_line(
        &self,
        at: usize,
        nth: usize,
        row: &Row,
        line: &LogLine,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let first = nth == 0;
        let expanded = self.expanded.contains(&at);
        let collapsed = row.is_collapsed() && !expanded && first;
        let expandable = row.is_collapsed() && first;
        let key = format!("{at}-{nth}");
        let mut rendered = h_flex()
            .w_full()
            .items_start()
            .gap(cyberpunk::SPACE_8)
            .px(cyberpunk::SPACE_14)
            .py(px(2.))
            .hover(|style| style.bg(cyberpunk::row_hovered()));
        match line {
            LogLine::Raw(text) => {
                rendered = rendered.child(
                    div()
                        .debug_selector(move || format!("LOG_LENS_RAW-{key}"))
                        .flex_1()
                        .child(SharedString::from(text.clone()))
                        .text_color(cyberpunk::text_secondary()),
                );
            }
            LogLine::Read(event) => {
                let severity = severity_of(event.level);
                if let Some(time) = &event.time {
                    rendered = rendered.child(
                        Label::new(SharedString::from(time.clone()))
                            .size(LabelSize::Small)
                            .color(Color::Custom(cyberpunk::text_tertiary())),
                    );
                }
                if let Some(level) = event.level {
                    rendered = rendered.child(
                        Label::new(level.label())
                            .size(LabelSize::Small)
                            .color(Color::Custom(severity.text())),
                    );
                }
                let mut middle = h_flex().flex_1().flex_wrap().gap(cyberpunk::SPACE_8).child(
                    div()
                        .debug_selector({
                            let key = key.clone();
                            move || format!("LOG_LENS_MESSAGE-{key}")
                        })
                        .child(
                            Label::new(SharedString::from(event.message.clone()))
                                .color(Color::Custom(cyberpunk::text_primary())),
                        ),
                );
                if collapsed {
                    middle = middle.child(
                        div()
                            .debug_selector(move || format!("LOG_LENS_COUNT-{at}"))
                            .child(
                                Label::new(SharedString::from(format!("×{}", row.count())))
                                    .size(LabelSize::Small)
                                    .color(Color::Custom(cyberpunk::Accent::Cyan.bright())),
                            ),
                    );
                    for (which, difference) in self.digest.differences(row).into_iter().enumerate()
                    {
                        middle = middle.child(
                            div()
                                .debug_selector(move || format!("LOG_LENS_DIFFERENCE-{at}-{which}"))
                                .child(
                                    Label::new(SharedString::from(difference))
                                        .size(LabelSize::Small)
                                        .color(Color::Custom(cyberpunk::text_secondary())),
                                ),
                        );
                    }
                } else {
                    for field in self.digest.unfolded_fields(event) {
                        middle = middle.child(
                            h_flex()
                                .debug_selector({
                                    let key = key.clone();
                                    let name = field.name.clone();
                                    move || format!("LOG_LENS_FIELD-{key}-{name}")
                                })
                                .gap(cyberpunk::SPACE_4)
                                .child(
                                    Label::new(SharedString::from(field.name.clone()))
                                        .size(LabelSize::Small)
                                        .color(Color::Custom(cyberpunk::text_tertiary())),
                                )
                                .child(
                                    Label::new(SharedString::from(field.value.clone()))
                                        .size(LabelSize::Small)
                                        .color(Color::Custom(cyberpunk::text_secondary())),
                                ),
                        );
                    }
                }
                for (which, block_line) in event.block.iter().enumerate() {
                    middle = middle.child(
                        div()
                            .debug_selector({
                                let key = key.clone();
                                move || format!("LOG_LENS_BLOCK-{key}-{which}")
                            })
                            .w_full()
                            .child(SharedString::from(block_line.clone()))
                            .text_color(cyberpunk::text_secondary()),
                    );
                }
                rendered = rendered.child(middle);
                if let Some(caller) = &event.caller {
                    let caller = caller.clone();
                    rendered = rendered.child(
                        div()
                            .id(SharedString::from(format!("log-lens-caller-{key}")))
                            .debug_selector(move || format!("LOG_LENS_CALLER-{key}"))
                            .flex_none()
                            .cursor_pointer()
                            .child(
                                Label::new(SharedString::from(caller.clone()))
                                    .size(LabelSize::Small)
                                    .color(Color::Custom(cyberpunk::text_tertiary())),
                            )
                            .on_click(cx.listener(move |lens, _, window, cx| {
                                lens.open_caller(&caller, window, cx);
                            })),
                    );
                }
            }
        }
        if expandable {
            rendered = rendered.child(
                div()
                    .debug_selector(move || format!("LOG_LENS_EXPAND-{at}"))
                    .child(
                        IconButton::new(
                            SharedString::from(format!("log-lens-expand-{at}")),
                            if expanded {
                                IconName::ChevronUp
                            } else {
                                IconName::ChevronDown
                            },
                        )
                        .icon_size(IconSize::XSmall)
                        .style(cyberpunk::Rank::Quiet.style())
                        .on_click(cx.listener(move |lens, _, _, cx| {
                            if !lens.expanded.remove(&at) {
                                lens.expanded.insert(at);
                            }
                            cx.notify();
                        })),
                    ),
            );
        }
        rendered.into_any_element()
    }
}

impl Render for LogLensView {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let visible = self.visible_rows(cx);
        // Follow new output only while the reader has not scrolled away from
        // the bottom, so that reading older lines is not fought by the stream.
        let room_below = self.scroll_handle.max_offset().y + self.scroll_handle.offset().y;
        if !visible.is_empty() && room_below <= window.line_height() {
            self.scroll_handle.scroll_to_bottom();
        }
        let rows = visible
            .into_iter()
            .filter_map(|at| {
                let row = self.digest.rows().get(at)?.clone();
                Some(self.render_row(at, &row, cx))
            })
            .collect::<Vec<_>>();
        let provisional = self.provisional.clone().map(|line| {
            let at = self.digest.rows().len();
            self.render_line(at, 0, &Row { lines: Vec::new() }, &line, cx)
        });
        v_flex()
            .key_context("LogLens")
            .track_focus(&self.focus_handle)
            .size_full()
            .bg(cyberpunk::canvas())
            .text_color(cyberpunk::text_primary())
            .child(self.render_header())
            .child(self.render_filters(cx))
            .child(
                div()
                    .id("log-lens-rows")
                    .flex_1()
                    .min_h_0()
                    .overflow_x_hidden()
                    .overflow_y_scroll()
                    .track_scroll(&self.scroll_handle)
                    .children(rows)
                    .children(provisional)
                    .custom_scrollbars(
                        Scrollbars::new(ScrollAxes::Both)
                            .tracked_scroll_handle(&self.scroll_handle),
                        window,
                        cx,
                    ),
            )
    }
}

impl Focusable for LogLensView {
    fn focus_handle(&self, _cx: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl EventEmitter<()> for LogLensView {}

impl Item for LogLensView {
    type Event = ();

    fn tab_icon(&self, _window: &Window, _cx: &App) -> Option<Icon> {
        Some(Icon::new(IconName::ListTree))
    }

    fn tab_content_text(&self, _detail: usize, _cx: &App) -> SharedString {
        SharedString::from(format!("Log lens {}", self.title))
    }
}

#[cfg(test)]
impl LogLensView {
    pub fn digest(&self) -> &LogDigest {
        &self.digest
    }

    pub fn expanded_rows(&self) -> &HashSet<usize> {
        &self.expanded
    }

    pub fn set_threshold(&mut self, threshold: Level) {
        self.threshold = threshold;
    }
}
