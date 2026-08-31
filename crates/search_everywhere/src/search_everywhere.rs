use std::path::PathBuf;
use std::sync::Arc;

use fuzzy::{StringMatch, StringMatchCandidate};
use gpui::{
    Action, AnyElement, App, Context, DismissEvent, Entity, Task, WeakEntity, Window, actions,
};
use picker::{Picker, PickerDelegate, PreviewUpdate};
use project::{Project, ProjectPath, search::SearchQuery, search::SearchResult};
use ui::{ListItem, ListItemSpacing, prelude::*};
use util::ResultExt as _;
use workspace::Workspace;

actions!(
    search_everywhere,
    [
        /// Opens one window that searches the whole project: its files, the
        /// editor's own commands, and the text inside the files.
        Toggle
    ]
);

/// How many of each kind are offered before the reader is expected to narrow the
/// search rather than scroll it.
const AT_MOST: usize = 50;

/// How many files are read while searching their text. A project holds far more
/// than anyone reads in one look, and the point of the window is the first
/// answer, not the complete one -- the whole search is one press away.
const AT_MOST_IN_TEXT: usize = 40;

/// Which of the things a project holds the window is looking through.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Among {
    Everything,
    Files,
    Commands,
    Text,
}

impl Among {
    const ALL: [Among; 4] = [
        Among::Everything,
        Among::Files,
        Among::Commands,
        Among::Text,
    ];

    fn shown(self) -> &'static str {
        match self {
            Among::Everything => "All",
            Among::Files => "Files",
            Among::Commands => "Actions",
            Among::Text => "Text",
        }
    }

    fn asked_for(self) -> &'static str {
        match self {
            Among::Everything => "Search the project: a file, a command, or text inside a file…",
            Among::Files => "Find a file by its name…",
            Among::Commands => "Run a command…",
            Among::Text => "Find text anywhere in the project…",
        }
    }
}

/// One thing the window found.
enum Found {
    File {
        path: ProjectPath,
        name: String,
        /// The directory it is in, shown beside the name the way a browser shows
        /// a URL beside a title.
        at: String,
        absolute: Option<PathBuf>,
    },
    Command {
        shown: String,
        action: Box<dyn Action>,
    },
    Text {
        path: ProjectPath,
        name: String,
        at: String,
        line: u32,
        excerpt: String,
        absolute: Option<PathBuf>,
    },
}

impl Found {
    /// What the footer says about it: the whole path, which is the one thing a
    /// list of short names cannot tell you.
    fn where_it_is(&self) -> String {
        match self {
            Found::File { path, name, at, .. } | Found::Text { path, name, at, .. } => {
                let _ = path;
                match at.is_empty() {
                    true => name.clone(),
                    false => format!("{at}/{name}"),
                }
            }
            Found::Command { shown, .. } => shown.clone(),
        }
    }
}

pub fn init(cx: &mut App) {
    cx.observe_new(
        |workspace: &mut Workspace, _window, _: &mut Context<Workspace>| {
            workspace.register_action(|workspace, _: &Toggle, window, cx| {
                let project = workspace.project().clone();
                let handle = cx.entity().downgrade();
                workspace.toggle_modal(window, cx, move |window, cx| {
                    let delegate = SearchEverywhereDelegate::new(handle, project.clone(), cx);
                    let preview = picker_preview::editor_preview(project, window, cx);
                    // Wider than a plain picker: this window holds a tab strip, a
                    // path along its foot and a preview beside the list, and all
                    // three are there to be read at a glance.
                    // Sized as a share of the window rather than in pixels: this
                    // is the window a reader searches a whole project in, and on
                    // a large screen a fixed 1040 points is a slot in the middle
                    // of it. Two thirds across and two thirds down is what a
                    // JetBrains editor gives the same job.
                    Picker::uniform_list_with_preview(delegate, preview, window, cx)
                        .initial_width(picker::RelativeWidth::viewport(0.66))
                        .max_height(picker::RelativeHeight::viewport(0.68))
                });
            });
        },
    )
    .detach();
}

pub struct SearchEverywhereDelegate {
    workspace: WeakEntity<Workspace>,
    project: Entity<Project>,
    among: Among,
    /// Every file of the project, as the fuzzy match reads them. Collected once
    /// when the window opens: a project does not change under a search that
    /// lasts a few seconds, and collecting them on every keystroke is the one
    /// thing that would make this window feel slow.
    files: Vec<FileOfTheProject>,
    candidates: Vec<StringMatchCandidate>,
    found: Vec<Found>,
    selected: usize,
}

struct FileOfTheProject {
    path: ProjectPath,
    name: String,
    at: String,
    absolute: Option<PathBuf>,
}

impl SearchEverywhereDelegate {
    fn new(
        workspace: WeakEntity<Workspace>,
        project: Entity<Project>,
        cx: &mut Context<Picker<Self>>,
    ) -> Self {
        let files = files_of(&project, cx);
        let candidates = files
            .iter()
            .enumerate()
            .map(|(at, file)| StringMatchCandidate::new(at, &file.searched_as()))
            .collect();
        Self {
            workspace,
            project,
            among: Among::Everything,
            files,
            candidates,
            found: Vec::new(),
            selected: 0,
        }
    }

    fn look_among(&mut self, among: Among) {
        self.among = among;
        self.selected = 0;
    }

    fn matching_files(&self, matches: Vec<StringMatch>) -> Vec<Found> {
        matches
            .into_iter()
            .filter_map(|matched| {
                let file = self.files.get(matched.candidate_id)?;
                Some(Found::File {
                    path: file.path.clone(),
                    name: file.name.clone(),
                    at: file.at.clone(),
                    absolute: file.absolute.clone(),
                })
            })
            .collect()
    }

    fn matching_commands(&self, query: &str, window: &mut Window, cx: &mut App) -> Vec<Found> {
        let filter = command_palette_hooks::CommandPaletteFilter::try_global(cx);
        let asked = query.trim().to_lowercase();
        window
            .available_actions(cx)
            .into_iter()
            .filter_map(|action| {
                if filter.is_some_and(|filter| filter.is_hidden(&*action)) {
                    return None;
                }
                let shown = command_palette::humanize_action_name(action.name());
                (asked.is_empty() || shown.to_lowercase().contains(&asked)).then_some(
                    Found::Command {
                        shown,
                        action: action.boxed_clone(),
                    },
                )
            })
            .take(AT_MOST)
            .collect()
    }
}

/// Every file of every visible worktree, named and placed the way the list shows
/// them.
fn files_of(project: &Entity<Project>, cx: &App) -> Vec<FileOfTheProject> {
    let read = project.read(cx);
    let mut files = Vec::new();
    for worktree in read.visible_worktrees(cx) {
        let worktree = worktree.read(cx);
        let id = worktree.id();
        let root = worktree.abs_path().to_path_buf();
        for entry in worktree.entries(false, 0) {
            if !entry.is_file() {
                continue;
            }
            let relative = entry.path.as_std_path();
            let Some(name) = relative
                .file_name()
                .and_then(|name| name.to_str())
                .map(str::to_string)
            else {
                continue;
            };
            let at = relative
                .parent()
                .map(|parent| parent.to_string_lossy().replace('\\', "/"))
                .unwrap_or_default();
            files.push(FileOfTheProject {
                path: ProjectPath {
                    worktree_id: id,
                    path: entry.path.clone(),
                },
                name,
                at,
                absolute: Some(root.join(relative)),
            });
        }
    }
    files
}

impl FileOfTheProject {
    /// What the fuzzy match reads: the name and the path both, so a query naming
    /// a directory finds what is in it.
    fn searched_as(&self) -> String {
        match self.at.is_empty() {
            true => self.name.clone(),
            false => format!("{}/{}", self.at, self.name),
        }
    }
}

impl PickerDelegate for SearchEverywhereDelegate {
    type ListItem = ui::ListItem;

    fn name() -> &'static str {
        "SearchEverywhere"
    }

    fn match_count(&self) -> usize {
        self.found.len()
    }

    fn selected_index(&self) -> usize {
        self.selected
    }

    fn set_selected_index(
        &mut self,
        ix: usize,
        _window: &mut Window,
        _cx: &mut Context<Picker<Self>>,
    ) {
        self.selected = ix;
    }

    fn placeholder_text(&self, _window: &mut Window, _cx: &mut App) -> Arc<str> {
        self.among.asked_for().into()
    }

    fn try_get_preview_data_for_match(&self, _cx: &App) -> Option<PreviewUpdate> {
        match self.found.get(self.selected)? {
            Found::File { absolute, .. } | Found::Text { absolute, .. } => {
                Some(PreviewUpdate::from_path(absolute.clone()?))
            }
            Found::Command { .. } => None,
        }
    }

    fn update_matches(
        &mut self,
        query: String,
        window: &mut Window,
        cx: &mut Context<Picker<Self>>,
    ) -> Task<()> {
        let among = self.among;
        match among {
            Among::Commands => {
                self.found = self.matching_commands(&query, window, cx);
                self.selected = 0;
                Task::ready(())
            }
            Among::Files | Among::Everything => {
                let candidates = self.candidates.clone();
                let asked = query.clone();
                let background = cx.background_executor().clone();
                let commands = match among {
                    Among::Everything => self.matching_commands(&query, window, cx),
                    _ => Vec::new(),
                };
                cx.spawn_in(window, async move |picker, cx| {
                    let matches = fuzzy::match_strings(
                        &candidates,
                        &asked,
                        false,
                        true,
                        AT_MOST,
                        &Default::default(),
                        background,
                    )
                    .await;
                    picker
                        .update(cx, |picker, cx| {
                            let mut found = picker.delegate.matching_files(matches);
                            // Files first: a name typed into this window is a file
                            // far more often than it is a command.
                            found.extend(commands);
                            picker.delegate.found = found;
                            picker.delegate.selected = 0;
                            cx.notify();
                        })
                        .log_err();
                })
            }
            Among::Text => {
                if query.trim().is_empty() {
                    self.found = Vec::new();
                    self.selected = 0;
                    return Task::ready(());
                }
                let project = self.project.clone();
                cx.spawn_in(window, async move |picker, cx| {
                    let found = text_of(project, query, cx).await;
                    picker
                        .update(cx, |picker, cx| {
                            picker.delegate.found = found;
                            picker.delegate.selected = 0;
                            cx.notify();
                        })
                        .log_err();
                })
            }
        }
    }

    fn confirm(&mut self, secondary: bool, window: &mut Window, cx: &mut Context<Picker<Self>>) {
        let Some(found) = self.found.get(self.selected) else {
            return;
        };
        match found {
            Found::File { path, .. } | Found::Text { path, .. } => {
                let path = path.clone();
                let workspace = self.workspace.clone();
                cx.spawn_in(window, async move |_, cx| {
                    workspace
                        .update_in(cx, |workspace, window, cx| {
                            workspace.open_path(path, None, true, window, cx)
                        })
                        .log_err();
                })
                .detach();
            }
            Found::Command { action, .. } => {
                let action = action.boxed_clone();
                window.dispatch_action(action, cx);
            }
        }
        let _ = secondary;
        cx.emit(DismissEvent);
    }

    fn dismissed(&mut self, _window: &mut Window, _cx: &mut Context<Picker<Self>>) {}

    fn render_match(
        &self,
        ix: usize,
        selected: bool,
        _window: &mut Window,
        _cx: &mut Context<Picker<Self>>,
    ) -> Option<Self::ListItem> {
        let found = self.found.get(ix)?;
        let (icon, name, beside) = match found {
            Found::File { name, at, .. } => (IconName::File, name.clone(), at.clone()),
            Found::Command { shown, .. } => (IconName::Terminal, shown.clone(), String::new()),
            Found::Text {
                name,
                at,
                line,
                excerpt,
                ..
            } => (
                IconName::MagnifyingGlass,
                format!("{name}:{line}"),
                match at.is_empty() {
                    true => excerpt.clone(),
                    false => format!("{at}  ·  {excerpt}"),
                },
            ),
        };
        Some(
            ListItem::new(ix)
                .inset(true)
                .spacing(ListItemSpacing::Sparse)
                .toggle_state(selected)
                .child(
                    h_flex()
                        .w_full()
                        .min_w_0()
                        .gap_2()
                        .child(Icon::new(icon).size(IconSize::Small).color(Color::Muted))
                        .child(Label::new(name).single_line())
                        .when(!beside.is_empty(), |row| {
                            row.child(
                                div().min_w_0().child(
                                    Label::new(beside)
                                        .size(LabelSize::Small)
                                        .color(Color::Muted)
                                        .truncate(),
                                ),
                            )
                        }),
                ),
        )
    }

    fn render_header(
        &self,
        _window: &mut Window,
        cx: &mut Context<Picker<Self>>,
    ) -> Option<AnyElement> {
        let picker = cx.entity();
        let among = self.among;
        Some(
            h_flex()
                .id("search-everywhere-tabs")
                .debug_selector(|| "search-everywhere-tabs".to_string())
                .w_full()
                .gap_1()
                .px_2()
                .pb_1()
                .children(Among::ALL.into_iter().map(|one| {
                    let picker = picker.clone();
                    Button::new(("search-everywhere-tab", one as usize), one.shown())
                        .label_size(LabelSize::Small)
                        .toggle_state(one == among)
                        .selected_style(ButtonStyle::Filled)
                        .style(ButtonStyle::Outlined)
                        .on_click(move |_, window, cx| {
                            picker.update(cx, |picker, cx| {
                                picker.delegate.look_among(one);
                                picker.refresh_placeholder(window, cx);
                                picker.refresh(window, cx);
                            });
                        })
                }))
                .into_any_element(),
        )
    }

    fn render_footer(
        &self,
        _window: &mut Window,
        _cx: &mut Context<Picker<Self>>,
    ) -> Option<AnyElement> {
        // The whole path of what is highlighted. A list of short names cannot say
        // which of four files called `main.go` this is, and that is the one thing
        // the reader is deciding between.
        let where_it_is = self
            .found
            .get(self.selected)
            .map(Found::where_it_is)
            .unwrap_or_default();
        Some(
            h_flex()
                .w_full()
                .min_w_0()
                .px_2()
                .py_1()
                .justify_between()
                .child(
                    div().min_w_0().child(
                        Label::new(where_it_is)
                            .size(LabelSize::Small)
                            .color(Color::Muted)
                            .truncate(),
                    ),
                )
                .child(
                    Label::new("Enter to open")
                        .size(LabelSize::Small)
                        .color(Color::Muted),
                )
                .into_any_element(),
        )
    }
}

/// The text matches of the project, as the list shows them.
async fn text_of(
    project: Entity<Project>,
    query: String,
    cx: &mut gpui::AsyncWindowContext,
) -> Vec<Found> {
    let Ok(search) = SearchQuery::text(
        query,
        false,
        false,
        false,
        Default::default(),
        Default::default(),
        false,
        None,
    ) else {
        return Vec::new();
    };
    let results = project.update(cx, |project, cx| project.search(search, cx));
    // The handle is what keeps the search running; dropping it early stops it
    // before the first answer arrives.
    let project::SearchResults { task_handle, rx } = results;
    let mut found = Vec::new();
    while let Ok(result) = rx.recv().await {
        if found.len() >= AT_MOST_IN_TEXT {
            break;
        }
        let SearchResult::Buffer { buffer, ranges } = result else {
            continue;
        };
        let Some(range) = ranges.first().cloned() else {
            continue;
        };
        let read = project.update(cx, |_, cx| {
            let buffer = buffer.read(cx);
            let file = buffer.file()?;
            let snapshot = buffer.snapshot();
            let at = text::ToPoint::to_point(&range.start, &snapshot);
            let line: String = snapshot
                .text_for_range(
                    language::Point::new(at.row, 0)..language::Point::new(at.row, u32::MAX),
                )
                .collect();
            let path = file.path().as_std_path().to_path_buf();
            Some((
                ProjectPath {
                    worktree_id: file.worktree_id(cx),
                    path: file.path().clone(),
                },
                path,
                at.row + 1,
                line.trim().to_string(),
            ))
        });
        let Some((project_path, path, line, excerpt)) = read else {
            continue;
        };
        let name = path
            .file_name()
            .and_then(|name| name.to_str())
            .unwrap_or_default()
            .to_string();
        let at = path
            .parent()
            .map(|parent| parent.to_string_lossy().replace('\\', "/"))
            .unwrap_or_default();
        found.push(Found::Text {
            path: project_path,
            name,
            at,
            line,
            excerpt,
            absolute: None,
        });
    }
    drop(task_handle);
    found
}
