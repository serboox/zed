use std::ops::Range;
use std::path::PathBuf;

use anyhow::{Context as _, Result};
use collections::HashMap;
use editor::Editor;
use gpui::{
    App, Context, Entity, EventEmitter, FocusHandle, Focusable, ScrollHandle, Task, WeakEntity,
    Window, actions,
};
use language::{Buffer, OffsetRangeExt as _};
use project::search::{SearchQuery, SearchResult};
use project::{Project, buffer_store::ProjectTransaction};
use text::Anchor;
use text::ToOffset as _;
use ui::prelude::*;
use ui::{Checkbox, Label, LabelSize, ScrollAxes, Scrollbars, ToggleState, Tooltip, WithScrollbar};
use util::ResultExt as _;
use workspace::Workspace;
use workspace::item::{Item, ItemEvent};

actions!(
    rename_preview,
    [
        /// Renames the symbol under the cursor, showing every place that would
        /// change before anything does.
        Toggle
    ]
);

/// How many places are listed. A name that occurs more often than this is one
/// no reader is going to check by eye, and a list that says it stopped is
/// better than one that pretends to be complete.
const AT_MOST: usize = 2000;

pub fn init(cx: &mut App) {
    cx.observe_new(|workspace: &mut Workspace, _, _| {
        workspace.register_action(|workspace, _: &Toggle, window, cx| {
            let Some(editor) = workspace
                .active_item(cx)
                .and_then(|item| item.downcast::<Editor>())
            else {
                return;
            };
            let Some(subject) = Subject::under_the_cursor(&editor, cx) else {
                return;
            };
            let project = workspace.project().clone();
            let handle = cx.entity().downgrade();
            let view = cx.new(|cx| RenamePreview::new(project, handle, subject, window, cx));
            workspace.add_item_to_active_pane(Box::new(view), None, true, window, cx);
        });
    })
    .detach();
}

/// What is being renamed: where the reader's cursor was, and the word it was
/// on. The position is kept as well as the word, because the language server
/// answers about a position and only the word can be searched for as text.
#[derive(Clone)]
pub struct Subject {
    pub buffer: Entity<Buffer>,
    pub at: Anchor,
    pub name: String,
}

impl Subject {
    /// The word the cursor sits in, or `None` where it sits on nothing that
    /// could be a name.
    pub fn under_the_cursor(editor: &Entity<Editor>, cx: &mut App) -> Option<Self> {
        editor.update(cx, |editor, cx| {
            let (buffer, at) = editor
                .buffer()
                .read(cx)
                .text_anchor_for_position(editor.selections.newest_anchor().head(), cx)?;
            let snapshot = buffer.read(cx).snapshot();
            let offset = at.to_offset(&snapshot);
            let text = snapshot.text();
            let is_name = |letter: char| letter.is_alphanumeric() || letter == '_';
            if offset > text.len() {
                return None;
            }
            let start = text[..offset]
                .char_indices()
                .rev()
                .take_while(|(_, letter)| is_name(*letter))
                .last()
                .map(|(at, _)| at)
                .unwrap_or(offset);
            let end = text[offset..]
                .char_indices()
                .take_while(|(_, letter)| is_name(*letter))
                .last()
                .map(|(at, letter)| offset + at + letter.len_utf8())
                .unwrap_or(offset);
            let name = text.get(start..end)?.to_string();
            if name.is_empty() || name.chars().next().is_some_and(|f| f.is_numeric()) {
                return None;
            }
            Some(Self { buffer, at, name })
        })
    }
}

/// Where one occurrence came from, which is the whole point of the preview:
/// the two are not equally trustworthy and must not be presented as though
/// they were.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Source {
    /// The language server named this position as a reference to the symbol.
    TheServer,
    /// The text matches and nothing more is known. It may be a comment, a
    /// string, or an unrelated symbol of the same name.
    OnlyTheText,
}

#[derive(Clone)]
pub struct Occurrence {
    pub buffer: Entity<Buffer>,
    pub path: PathBuf,
    pub range: Range<Anchor>,
    pub row: u32,
    pub line: String,
    pub source: Source,
    pub chosen: bool,
}

/// What the preview knows so far. The two answers arrive separately and at very
/// different speeds -- text search returns in milliseconds, a language server
/// can take a minute on a popular name -- so the surface has to be able to show
/// one without the other.
pub enum Gathering {
    Looking,
    Found {
        /// Whether the server has answered yet. Until it has, every occurrence
        /// is text-only, and saying so is the difference between "we found
        /// nothing verified" and "there is nothing verified to find".
        the_server_answered: bool,
        stopped_early: bool,
    },
    Failed(String),
}

pub struct RenamePreview {
    project: Entity<Project>,
    workspace: WeakEntity<Workspace>,
    focus_handle: FocusHandle,
    subject: Subject,
    new_name: Entity<Editor>,
    occurrences: Vec<Occurrence>,
    gathering: Gathering,
    /// Whether the project holds more than one definition under this name. The
    /// measured precision of matching by name alone depends entirely on this,
    /// so the reader is told rather than left to guess.
    name_means_one_thing: Option<bool>,
    applied: Option<ProjectTransaction>,
    scroll: ScrollHandle,
    _gathering_task: Option<Task<()>>,
}

impl RenamePreview {
    pub fn new(
        project: Entity<Project>,
        workspace: WeakEntity<Workspace>,
        subject: Subject,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let new_name = cx.new(|cx| {
            let mut editor = Editor::single_line(window, cx);
            editor.set_text(subject.name.clone(), window, cx);
            editor.select_all(&Default::default(), window, cx);
            editor
        });
        window.focus(&new_name.focus_handle(cx), cx);

        let mut this = Self {
            project,
            workspace,
            focus_handle: cx.focus_handle(),
            subject,
            new_name,
            occurrences: Vec::new(),
            gathering: Gathering::Looking,
            name_means_one_thing: None,
            applied: None,
            scroll: ScrollHandle::new(),
            _gathering_task: None,
        };
        this.look(window, cx);
        this
    }

    /// Asks both sides at once and shows whichever answers first.
    fn look(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let project = self.project.clone();
        let subject = self.subject.clone();
        self.name_means_one_thing = name_means_one_thing(&self.project, &subject.name, cx);
        self._gathering_task = Some(cx.spawn_in(window, async move |this, cx| {
            let text = text_occurrences(&project, &subject.name, cx).await;
            let text = match text {
                Ok(found) => found,
                Err(error) => {
                    this.update(cx, |this, cx| {
                        this.gathering = Gathering::Failed(format!("{error:#}"));
                        cx.notify();
                    })
                    .ok();
                    return;
                }
            };
            let stopped_early = text.len() >= AT_MOST;
            this.update(cx, |this, cx| {
                this.occurrences = text;
                this.gathering = Gathering::Found {
                    the_server_answered: false,
                    stopped_early,
                };
                cx.notify();
            })
            .ok();

            // The authoritative half, however long it takes. What it names is
            // promoted out of "only the text" and ticked.
            let from_the_server = server_occurrences(&project, &subject, cx).await;
            this.update(cx, |this, cx| {
                if let Some(verified) = from_the_server.log_err() {
                    this.mark_what_the_server_verified(&verified, cx);
                }
                this.gathering = Gathering::Found {
                    the_server_answered: true,
                    stopped_early,
                };
                cx.notify();
            })
            .ok();
        }));
    }

    /// Promotes every text match the server also named, and adds any the text
    /// search did not reach.
    fn mark_what_the_server_verified(
        &mut self,
        verified: &[(Entity<Buffer>, Range<Anchor>)],
        cx: &mut Context<Self>,
    ) {
        let mut unmatched: Vec<&(Entity<Buffer>, Range<Anchor>)> = Vec::new();
        for named in verified {
            let mut found_it = false;
            for occurrence in self.occurrences.iter_mut() {
                if occurrence.buffer == named.0 && same_place(&occurrence.range, &named.1, cx) {
                    occurrence.source = Source::TheServer;
                    occurrence.chosen = true;
                    found_it = true;
                    break;
                }
            }
            if !found_it {
                unmatched.push(named);
            }
        }
        for (buffer, range) in unmatched {
            if let Some(occurrence) = occurrence_of(buffer.clone(), range.clone(), cx) {
                self.occurrences.push(Occurrence {
                    source: Source::TheServer,
                    chosen: true,
                    ..occurrence
                });
            }
        }
        self.occurrences.sort_by(|left, right| {
            left.path
                .cmp(&right.path)
                .then_with(|| left.row.cmp(&right.row))
        });
    }

    /// Opens the file at that line, so a questionable occurrence can be looked
    /// at rather than guessed about. The second list exists precisely because
    /// some of what is in it should not be renamed, and deciding that needs the
    /// surrounding code, not one line of it.
    fn show_me(&mut self, at: usize, window: &mut Window, cx: &mut Context<Self>) {
        let Some(occurrence) = self.occurrences.get(at) else {
            return;
        };
        let Some(path) = self.project.read(cx).absolute_path(
            &project::ProjectPath {
                worktree_id: match occurrence.buffer.read(cx).file() {
                    Some(file) => file.worktree_id(cx),
                    None => return,
                },
                path: match occurrence.buffer.read(cx).file() {
                    Some(file) => file.path().clone(),
                    None => return,
                },
            },
            cx,
        ) else {
            return;
        };
        let row = occurrence.row.saturating_sub(1);
        let workspace = self.workspace.clone();
        cx.spawn_in(window, async move |_, cx| {
            let Ok(opened) = workspace.update_in(cx, |workspace, window, cx| {
                workspace.open_abs_path(path, Default::default(), window, cx)
            }) else {
                return;
            };
            // Awaited rather than dropped: opening is the task, and a dropped
            // task is a cancelled one.
            let Ok(item) = opened.await else {
                return;
            };
            let Some(editor) = item.downcast::<Editor>() else {
                return;
            };
            editor
                .update_in(cx, |editor, window, cx| {
                    editor.change_selections(
                        editor::SelectionEffects::default(),
                        window,
                        cx,
                        |selections| {
                            selections.select_ranges([
                                language::Point::new(row, 0)..language::Point::new(row, 0)
                            ]);
                        },
                    );
                })
                .log_err();
        })
        .detach();
    }

    fn toggle(&mut self, at: usize, cx: &mut Context<Self>) {
        if let Some(occurrence) = self.occurrences.get_mut(at) {
            occurrence.chosen = !occurrence.chosen;
            cx.notify();
        }
    }

    /// Applies every ticked occurrence, as one transaction per buffer, kept
    /// together so a single undo puts all of it back.
    pub fn apply(&mut self, cx: &mut Context<Self>) {
        if self.applied.is_some() {
            return;
        }
        let new_name = self.new_name.read(cx).text(cx);
        if new_name.trim().is_empty() || new_name == self.subject.name {
            return;
        }
        let mut by_buffer: HashMap<Entity<Buffer>, Vec<Range<Anchor>>> = HashMap::default();
        for occurrence in self.occurrences.iter().filter(|one| one.chosen) {
            by_buffer
                .entry(occurrence.buffer.clone())
                .or_default()
                .push(occurrence.range.clone());
        }
        if by_buffer.is_empty() {
            return;
        }

        let mut transaction = ProjectTransaction::default();
        for (buffer, mut ranges) in by_buffer {
            let edited = buffer.update(cx, |buffer, cx| {
                // Back to front, so an earlier edit cannot move a later one.
                ranges.sort_by(|left, right| right.start.cmp(&left.start, buffer));
                buffer.finalize_last_transaction();
                buffer.start_transaction();
                for range in ranges {
                    buffer.edit([(range, new_name.clone())], None, cx);
                }
                buffer.end_transaction(cx).and_then(|id| {
                    buffer.finalize_last_transaction();
                    buffer.get_transaction(id).cloned()
                })
            });
            if let Some(edited) = edited {
                transaction.0.insert(buffer, edited);
            }
        }
        self.applied = Some(transaction);
        cx.notify();
    }

    /// Puts every buffer back where it was, in one go.
    pub fn undo(&mut self, cx: &mut Context<Self>) {
        let Some(transaction) = self.applied.take() else {
            return;
        };
        for (buffer, edited) in transaction.0 {
            buffer.update(cx, |buffer, cx| {
                buffer.undo_transaction(edited.id, cx);
            });
        }
        cx.notify();
    }

    pub fn occurrences(&self) -> &[Occurrence] {
        &self.occurrences
    }

    pub fn has_been_applied(&self) -> bool {
        self.applied.is_some()
    }
}

/// Whether both ranges name the same stretch of the same buffer.
fn same_place(left: &Range<Anchor>, right: &Range<Anchor>, cx: &App) -> bool {
    let _ = cx;
    left.start == right.start && left.end == right.end
}

fn occurrence_of(buffer: Entity<Buffer>, range: Range<Anchor>, cx: &App) -> Option<Occurrence> {
    let snapshot = buffer.read(cx).snapshot();
    let point = range.to_point(&snapshot);
    let line = snapshot
        .text_for_range(
            language::Point::new(point.start.row, 0)
                ..language::Point::new(point.start.row, snapshot.line_len(point.start.row)),
        )
        .collect::<String>();
    let path = buffer
        .read(cx)
        .file()
        .map(|file| file.path().as_std_path().to_path_buf())
        .unwrap_or_default();
    Some(Occurrence {
        buffer,
        path,
        range,
        row: point.start.row + 1,
        line: line.trim().to_string(),
        source: Source::OnlyTheText,
        chosen: false,
    })
}

/// Every place the word occurs, as text and nothing more.
async fn text_occurrences(
    project: &Entity<Project>,
    name: &str,
    cx: &mut gpui::AsyncWindowContext,
) -> Result<Vec<Occurrence>> {
    let query = SearchQuery::text(
        name,
        true,
        true,
        false,
        Default::default(),
        Default::default(),
        false,
        None,
    )
    .context("building the search for the name")?;
    let results = project.update(cx, |project, cx| project.search(query, cx));
    let mut found = Vec::new();
    while let Ok(result) = results.rx.recv().await {
        if let SearchResult::Buffer { buffer, ranges } = result {
            for range in ranges {
                if found.len() >= AT_MOST {
                    return Ok(found);
                }
                if let Some(occurrence) =
                    cx.update(|_, cx| occurrence_of(buffer.clone(), range, cx))?
                {
                    found.push(occurrence);
                }
            }
        }
    }
    found.sort_by(|left, right| {
        left.path
            .cmp(&right.path)
            .then_with(|| left.row.cmp(&right.row))
    });
    Ok(found)
}

/// Every place the language server calls a reference to this symbol.
async fn server_occurrences(
    project: &Entity<Project>,
    subject: &Subject,
    cx: &mut gpui::AsyncWindowContext,
) -> Result<Vec<(Entity<Buffer>, Range<Anchor>)>> {
    let references = project
        .update(cx, |project, cx| {
            project.references(&subject.buffer, subject.at, cx)
        })
        .await?
        .unwrap_or_default();
    Ok(references
        .into_iter()
        .map(|found| (found.buffer, found.range))
        .collect())
}

/// Whether the project holds exactly one definition under this name, read from
/// the index the editor already keeps. `None` where there is no index to ask.
fn name_means_one_thing(project: &Entity<Project>, name: &str, cx: &App) -> Option<bool> {
    let index = symbol_index::of_project(project, cx)?;
    let carrying_the_name = index
        .read(cx)
        .candidates(name, 64)
        .into_iter()
        .filter(|found| found.name == name)
        .count();
    Some(carrying_the_name <= 1)
}

impl EventEmitter<ItemEvent> for RenamePreview {}

impl Focusable for RenamePreview {
    fn focus_handle(&self, _cx: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl Item for RenamePreview {
    type Event = ItemEvent;

    fn tab_content_text(&self, _detail: usize, _cx: &App) -> SharedString {
        format!("Rename {}", self.subject.name).into()
    }

    fn to_item_events(event: &Self::Event, f: &mut dyn FnMut(ItemEvent)) {
        f(*event)
    }
}

impl RenamePreview {
    fn render_row(&self, at: usize, occurrence: &Occurrence, cx: &Context<Self>) -> AnyElement {
        let verified = occurrence.source == Source::TheServer;
        h_flex()
            .id(("rename-occurrence", at))
            .debug_selector(move || format!("RENAME-ROW-{at}"))
            .w_full()
            .gap_2()
            .px_2()
            .py_0p5()
            .items_center()
            .child(
                Checkbox::new(
                    ("rename-occurrence-tick", at),
                    if occurrence.chosen {
                        ToggleState::Selected
                    } else {
                        ToggleState::Unselected
                    },
                )
                .on_click(cx.listener(move |this, _, _, cx| this.toggle(at, cx))),
            )
            .child(
                Label::new(format!("{}:{}", occurrence.path.display(), occurrence.row))
                    .size(LabelSize::Small)
                    .color(if verified {
                        Color::Default
                    } else {
                        Color::Muted
                    }),
            )
            .child(
                div()
                    .id(("rename-occurrence-line", at))
                    .flex_1()
                    .min_w_0()
                    .overflow_hidden()
                    .cursor_pointer()
                    .tooltip(Tooltip::text("Open this line"))
                    .on_click(cx.listener(move |this, _, window, cx| this.show_me(at, window, cx)))
                    .child(
                        Label::new(occurrence.line.clone())
                            .size(LabelSize::Small)
                            .color(Color::Muted),
                    ),
            )
            .into_any_element()
    }

    fn render_group(
        &self,
        title: &'static str,
        note: SharedString,
        source: Source,
        cx: &Context<Self>,
    ) -> Option<AnyElement> {
        let rows: Vec<AnyElement> = self
            .occurrences
            .iter()
            .enumerate()
            .filter(|(_, occurrence)| occurrence.source == source)
            .map(|(at, occurrence)| self.render_row(at, occurrence, cx))
            .collect();
        if rows.is_empty() {
            return None;
        }
        Some(
            v_flex()
                .w_full()
                .gap_1()
                .child(
                    ui::cyberpunk::dialog_section(format!("{title} · {}", rows.len()))
                        .child(Label::new(note).size(LabelSize::XSmall).color(Color::Muted)),
                )
                .children(rows)
                .into_any_element(),
        )
    }
}

impl Render for RenamePreview {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let ticked = self.occurrences.iter().filter(|one| one.chosen).count();
        let applied = self.applied.is_some();

        let status: SharedString = match &self.gathering {
            Gathering::Looking => "Looking…".into(),
            Gathering::Failed(why) => format!("Could not look: {why}").into(),
            Gathering::Found {
                the_server_answered,
                stopped_early,
            } => {
                let mut said = if *the_server_answered {
                    String::from("The language server has answered.")
                } else {
                    String::from(
                        "Waiting for the language server; everything below is text so far.",
                    )
                };
                if *stopped_early {
                    said.push_str(&format!(" Stopped at {AT_MOST} places."));
                }
                if let Some(one_thing) = self.name_means_one_thing {
                    said.push_str(if one_thing {
                        " This name is declared once in the project."
                    } else {
                        " This name is declared more than once, so a text match may be another symbol."
                    });
                }
                said.into()
            }
        };

        v_flex()
            .key_context("RenamePreview")
            .track_focus(&self.focus_handle)
            .size_full()
            .p_3()
            .gap_3()
            .bg(cx.theme().colors().editor_background)
            .child(
                h_flex()
                    .w_full()
                    .gap_2()
                    .items_center()
                    .child(ui::cyberpunk::dialog_title(
                        format!("Rename {}", self.subject.name),
                        cx,
                    ))
                    .child(div().flex_1())
                    .child(
                        Label::new(format!("{ticked} to change"))
                            .size(LabelSize::Small)
                            .color(Color::Muted),
                    ),
            )
            .child(ui::cyberpunk::dialog_field(
                "new name",
                false,
                cx,
                self.new_name.clone(),
            ))
            .child(
                Label::new(status)
                    .size(LabelSize::XSmall)
                    .color(Color::Muted),
            )
            .child(
                div()
                    .id("rename-preview-list")
                    .debug_selector(|| "rename-preview-list".to_string())
                    .flex_1()
                    .min_h_0()
                    .overflow_scroll()
                    .track_scroll(&self.scroll)
                    .child(
                        v_flex()
                            .w_full()
                            .gap_3()
                            .children(self.render_group(
                                "found by the language server",
                                "These are references to this symbol.".into(),
                                Source::TheServer,
                                cx,
                            ))
                            .children(self.render_group(
                                "found only in the text",
                                "Could be a comment, a string, or another symbol of the same name."
                                    .into(),
                                Source::OnlyTheText,
                                cx,
                            )),
                    )
                    .custom_scrollbars(
                        Scrollbars::always_visible(ScrollAxes::Vertical)
                            .tracked_scroll_handle(&self.scroll),
                        window,
                        cx,
                    ),
            )
            .child(
                ui::cyberpunk::dialog_footer()
                    .mx_neg_3()
                    .mb_neg_3()
                    .child(div().flex_1())
                    .when(applied, |footer| {
                        footer.child(
                            Button::new("rename-undo", "Undo")
                                .label_size(LabelSize::Small)
                                .style(ui::cyberpunk::Rank::Neutral.style())
                                .tooltip(Tooltip::text("Put every file back as it was"))
                                .on_click(cx.listener(|this, _, _, cx| this.undo(cx))),
                        )
                    })
                    .when(!applied, |footer| {
                        footer.child(
                            Button::new("rename-apply", "Rename")
                                .label_size(LabelSize::Small)
                                .style(ui::cyberpunk::Rank::Accent.style())
                                .disabled(ticked == 0)
                                .on_click(cx.listener(|this, _, _, cx| this.apply(cx))),
                        )
                    }),
            )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use gpui::{TestAppContext, VisualTestContext};
    use project::FakeFs;
    use serde_json::json;
    use std::time::Instant;

    fn init_test(cx: &mut TestAppContext) {
        cx.update(|cx| {
            let settings_store = settings::SettingsStore::test(cx);
            cx.set_global(settings_store);
            theme::init(theme::LoadThemes::JustBase, cx);
            editor::init(cx);
        });
    }

    /// A project of `files`, with the preview already open on the name at the
    /// first occurrence in the first file.
    async fn preview_over(
        files: serde_json::Value,
        name: &str,
        cx: &mut TestAppContext,
    ) -> (Entity<Project>, Entity<RenamePreview>, VisualTestContext) {
        init_test(cx);
        let fs = FakeFs::new(cx.executor());
        fs.insert_tree(util::path!("/project"), files).await;
        let project = Project::test(fs, [util::path!("/project").as_ref()], cx).await;

        let buffer = project
            .update(cx, |project, cx| {
                project.open_local_buffer(util::path!("/project/one.rs"), cx)
            })
            .await
            .expect("the first file opens");
        let at = buffer.read_with(cx, |buffer, _| {
            let text = buffer.text();
            let offset = text.find(name).expect("the name is in the first file");
            buffer.anchor_before(offset)
        });
        let subject = Subject {
            buffer,
            at,
            name: name.to_string(),
        };

        let window = cx.add_window(|_, _| gpui::Empty);
        let mut cx = VisualTestContext::from_window(window.into(), cx);
        let handle = cx.update(|_, cx| {
            let _ = cx;
            WeakEntity::<Workspace>::new_invalid()
        });
        let view = cx.update(|window, cx| {
            cx.new(|cx| RenamePreview::new(project.clone(), handle, subject, window, cx))
        });
        cx.run_until_parked();
        (project, view, cx)
    }

    /// Nothing the language server has not spoken for is ticked. The whole
    /// point of the second list is that renaming what is in it is the reader's
    /// decision, not the tool's -- a comment or a string that happens to hold
    /// the word must not be edited because nobody looked.
    #[gpui::test]
    async fn what_only_the_text_found_is_listed_apart_and_left_unticked(cx: &mut TestAppContext) {
        let (_project, view, cx) = preview_over(
            json!({
                "one.rs": "fn helper() {}\nfn call() { helper(); }\n",
                "notes.md": "the helper is described here\n",
            }),
            "helper",
            cx,
        )
        .await;

        view.read_with(&cx, |view, _| {
            assert!(
                !view.occurrences().is_empty(),
                "the text search found the name"
            );
            assert!(
                view.occurrences()
                    .iter()
                    .all(|one| one.source == Source::OnlyTheText),
                "with no language server answering, everything is text only"
            );
            assert!(
                view.occurrences().iter().all(|one| !one.chosen),
                "and none of it is ticked"
            );
            assert!(
                view.occurrences()
                    .iter()
                    .any(|one| one.path.ends_with("notes.md")),
                "including the word inside prose, which is exactly what must not \
                 be renamed silently"
            );
        });
    }

    /// The gate this step was written around: every file changes together, and
    /// one undo puts every one of them back.
    #[gpui::test]
    async fn every_file_changes_in_one_go_and_one_undo_puts_them_all_back(cx: &mut TestAppContext) {
        let (_project, view, mut cx) = preview_over(
            json!({
                "one.rs": "fn helper() {}\nfn call() { helper(); }\n",
                "two.rs": "fn other() { helper(); }\n",
            }),
            "helper",
            cx,
        )
        .await;

        let before: Vec<String> = view.read_with(&cx, |view, cx| {
            let mut buffers: Vec<Entity<Buffer>> = Vec::new();
            for occurrence in view.occurrences() {
                if !buffers.contains(&occurrence.buffer) {
                    buffers.push(occurrence.buffer.clone());
                }
            }
            buffers
                .iter()
                .map(|buffer| buffer.read(cx).text())
                .collect()
        });
        assert_eq!(before.len(), 2, "the name is in both files");

        view.update_in(&mut cx, |view, window, cx| {
            let _ = window;
            for at in 0..view.occurrences().len() {
                view.toggle(at, cx);
            }
            view.new_name
                .update(cx, |editor, cx| editor.set_text("assistant", window, cx));
            view.apply(cx);
        });
        cx.run_until_parked();

        let buffers: Vec<Entity<Buffer>> = view.read_with(&cx, |view, _| {
            let mut seen: Vec<Entity<Buffer>> = Vec::new();
            for occurrence in view.occurrences() {
                if !seen.contains(&occurrence.buffer) {
                    seen.push(occurrence.buffer.clone());
                }
            }
            seen
        });
        for buffer in &buffers {
            let text = buffer.read_with(&cx, |buffer, _| buffer.text());
            assert!(
                text.contains("assistant") && !text.contains("helper"),
                "every ticked place changed: {text:?}"
            );
        }
        assert!(view.read_with(&cx, |view, _| view.has_been_applied()));

        view.update(&mut cx, |view, cx| view.undo(cx));
        cx.run_until_parked();

        let after: Vec<String> = buffers
            .iter()
            .map(|buffer| buffer.read_with(&cx, |buffer, _| buffer.text()))
            .collect();
        assert_eq!(
            after, before,
            "one undo puts every file back exactly as it was"
        );
        assert!(!view.read_with(&cx, |view, _| view.has_been_applied()));
    }

    /// Two occurrences on the same line are the case a naive rename gets wrong:
    /// editing the first moves the second. The edits go back to front for that
    /// reason, and this is what says so.
    #[gpui::test]
    async fn two_places_on_one_line_both_change(cx: &mut TestAppContext) {
        let (_project, view, mut cx) = preview_over(
            json!({ "one.rs": "fn call() { helper(helper()); }\n" }),
            "helper",
            cx,
        )
        .await;

        view.update_in(&mut cx, |view, window, cx| {
            for at in 0..view.occurrences().len() {
                view.toggle(at, cx);
            }
            view.new_name
                .update(cx, |editor, cx| editor.set_text("aide", window, cx));
            view.apply(cx);
        });
        cx.run_until_parked();

        let text = view.read_with(&cx, |view, cx| view.occurrences()[0].buffer.read(cx).text());
        assert_eq!(text, "fn call() { aide(aide()); }\n");
    }

    /// The gate asks for the preview of a symbol with two hundred uses inside
    /// half a second. Measured over a project that has exactly that many, on
    /// the same path the real one takes -- a fake filesystem, so this bounds
    /// the work rather than the disk, and it is the work that was in question.
    #[gpui::test]
    async fn a_symbol_used_two_hundred_times_is_previewed_promptly(cx: &mut TestAppContext) {
        let body: String = (0..200)
            .map(|at| format!("fn call_{at}() {{ helper(); }}\n"))
            .collect();
        let started = Instant::now();
        let (_project, view, cx) = preview_over(
            json!({ "one.rs": format!("fn helper() {{}}\n{body}") }),
            "helper",
            cx,
        )
        .await;
        let took = started.elapsed();

        let how_many = view.read_with(&cx, |view, _| view.occurrences().len());
        assert!(
            how_many >= 200,
            "every use is listed, and the declaration besides: {how_many}"
        );
        assert!(
            took < std::time::Duration::from_millis(500),
            "the preview took {took:?}; the gate is half a second"
        );
    }
}
