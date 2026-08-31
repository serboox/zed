use std::collections::HashSet;
use std::path::PathBuf;
use std::sync::Arc;

use editor::{Editor, SelectionEffects, scroll::Autoscroll};
use fuzzy::{StringMatch, StringMatchCandidate};
use gpui::{
    Action, AnyElement, App, Context, DismissEvent, Entity, Task, TaskExt as _, WeakEntity, Window,
    actions,
};
use picker::{Picker, PickerDelegate, PreviewUpdate};
use project::{Project, ProjectPath, search::SearchQuery, search::SearchResult};
use ui::{ListItem, ListItemSpacing, prelude::*};
use util::ResultExt as _;
use util::rel_path::RelPath;
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
    Symbols,
}

impl Among {
    const ALL: [Among; 5] = [
        Among::Everything,
        Among::Files,
        Among::Commands,
        Among::Text,
        Among::Symbols,
    ];

    fn shown(self) -> &'static str {
        match self {
            Among::Everything => "All",
            Among::Files => "Files",
            Among::Commands => "Actions",
            Among::Text => "Text",
            Among::Symbols => "Symbols",
        }
    }

    fn asked_for(self) -> &'static str {
        match self {
            Among::Everything => "Search the project: a file, a command, text, or a symbol…",
            Among::Files => "Find a file by its name…",
            Among::Commands => "Run a command…",
            Among::Text => "Find text anywhere in the project…",
            Among::Symbols => "Find a function, a type, or another symbol by its name…",
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
    Symbol {
        path: ProjectPath,
        name: String,
        kind: String,
        at: String,
        /// One-based, as the index itself counts lines.
        line: u32,
        absolute: Option<PathBuf>,
    },
}

impl Found {
    /// What the footer says about it: the whole path, which is the one thing a
    /// list of short names cannot tell you.
    fn where_it_is(&self) -> String {
        match self {
            Found::File { path, name, at, .. }
            | Found::Text { path, name, at, .. }
            | Found::Symbol { path, name, at, .. } => {
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
    /// What the symbol index answered from on the last query that asked it,
    /// so the footer can say why an empty symbols list is empty. `None`
    /// before any query has asked, and while a source other than symbols is
    /// being shown on its own.
    symbols_state: Option<symbol_index::State>,
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
            symbols_state: None,
        }
    }

    fn look_among(&mut self, among: Among) {
        self.among = among;
        self.selected = 0;
    }

    /// What the symbol index says about itself, when that is the reason a
    /// list of symbols is empty rather than "nothing matched". `None` when
    /// there is nothing to say: a source other than symbols is showing on
    /// its own, or the index answered `Ready`, in which case an empty list
    /// really does mean no matches.
    fn symbols_status(&self) -> Option<String> {
        if !matches!(self.among, Among::Symbols | Among::Everything) {
            return None;
        }
        match self.symbols_state.as_ref()? {
            symbol_index::State::NotBuilt => {
                Some("Symbols: no index for this project.".to_string())
            }
            symbol_index::State::Building => Some("Symbols: still building the index…".to_string()),
            symbol_index::State::Ready { .. } => None,
            symbol_index::State::Failed { reason } => {
                Some(format!("Symbols: the index failed -- {reason}"))
            }
        }
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

    /// Symbols whose name matches `query`, ranked by the same matcher every
    /// other kind in this window is ranked by. `SymbolIndex::candidates`
    /// deliberately does not rank -- see its own doc -- exactly so that
    /// ranking happens once, here, the same way `project_symbols` ranks what
    /// the language server found: one matcher for every list in the editor,
    /// not a second opinion per source.
    ///
    /// The second element of the pair says which state the index answered
    /// from, whether or not it found anything -- `None` only when there is
    /// no index for this project at all, which reads the same as `NotBuilt`
    /// to a caller.
    fn matching_symbols(
        &self,
        query: &str,
        cx: &mut App,
    ) -> (Vec<Found>, Option<symbol_index::State>) {
        let Some(index) = symbol_index::of_project(&self.project, cx) else {
            return (Vec::new(), None);
        };
        let state = index.read(cx).state().clone();
        let Some(worktree_id) = index.read(cx).worktree_id() else {
            return (Vec::new(), Some(state));
        };
        if query.trim().is_empty() {
            return (Vec::new(), Some(state));
        }
        let worktree_root = self
            .project
            .read(cx)
            .worktree_for_id(worktree_id, cx)
            .map(|worktree| worktree.read(cx).abs_path().to_path_buf());

        // A generous pool for the matcher to rank, not the final list: the
        // index's own filter is a cheap first pass, and the shared matcher
        // decides what is actually shown and in what order.
        let pool = index.read(cx).candidates(query, AT_MOST * 4);
        let candidates: Vec<StringMatchCandidate> = pool
            .iter()
            .enumerate()
            .map(|(id, definition)| StringMatchCandidate::new(id, &definition.name))
            .collect();
        let matches = cx.foreground_executor().block_on(fuzzy::match_strings(
            &candidates,
            query,
            false,
            true,
            AT_MOST,
            &Default::default(),
            cx.background_executor().clone(),
        ));

        let found = matches
            .into_iter()
            .filter_map(|matched| {
                let definition = pool.get(matched.candidate_id)?;
                let path = RelPath::from_unix_str(&definition.path).ok()?;
                let at = path
                    .as_std_path()
                    .parent()
                    .map(|parent| parent.to_string_lossy().replace('\\', "/"))
                    .unwrap_or_default();
                let absolute = worktree_root
                    .as_ref()
                    .map(|root| root.join(path.as_std_path()));
                Some(Found::Symbol {
                    path: ProjectPath {
                        worktree_id,
                        path: path.into(),
                    },
                    name: definition.name.clone(),
                    kind: definition.kind.clone(),
                    at,
                    line: definition.line,
                    absolute,
                })
            })
            .collect();
        (found, Some(state))
    }
}

/// Every file of every visible worktree, named and placed the way the list shows
/// them.
///
/// A worktree nested inside another visible one -- a subdirectory opened as
/// its own root alongside the project that already contains it, which this
/// fork's multi-root workspaces allow -- has every one of its files reported
/// twice: once by its own traversal, once by the outer worktree's, each
/// under a different `worktree_id` even though it is the same file at the
/// same place on disk. What decides whether two entries are really the same
/// file is the absolute path, never the display string two different
/// worktrees might render differently -- so that is what is deduplicated on
/// here, once, before a duplicate can ever reach the candidates the matcher
/// ranks.
fn files_of(project: &Entity<Project>, cx: &App) -> Vec<FileOfTheProject> {
    let read = project.read(cx);
    let mut files = Vec::new();
    let mut seen = HashSet::new();
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
            let absolute = root.join(relative);
            if !seen.insert(absolute.clone()) {
                continue;
            }
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
                absolute: Some(absolute),
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

/// Opens `path` in the workspace, moving to `line` afterward if one is
/// given -- the one way this window opens anything, shared by files, text
/// matches and symbols alike, so that confirming any of them is one path
/// through the code rather than one per kind.
fn open_path_in_workspace(
    path: ProjectPath,
    line: Option<u32>,
    workspace: WeakEntity<Workspace>,
    window: &mut Window,
    cx: &mut Context<Picker<SearchEverywhereDelegate>>,
) {
    cx.spawn_in(window, async move |_, cx| {
        // `open_path` itself returns the task that does the actual opening --
        // loading the buffer, then adding it to a pane. Handing that task
        // straight to `.log_err()` without awaiting it here would log only
        // whether the workspace was still around to be asked, then drop the
        // task it returned before anything in it had run, cancelling the
        // open silently. Awaiting it inside this same async block is what
        // actually drives it to completion.
        let item = workspace
            .update_in(cx, |workspace, window, cx| {
                workspace.open_path(path, None, true, window, cx)
            })?
            .await?;
        if let Some(line) = line {
            workspace.update_in(cx, |_workspace, window, cx| {
                let Some(editor) = item.downcast::<Editor>() else {
                    return;
                };
                editor.update(cx, |editor, cx| {
                    let point = language::Point::new(line.saturating_sub(1), 0);
                    editor.change_selections(
                        SelectionEffects::scroll(Autoscroll::center()),
                        window,
                        cx,
                        |selections| selections.select_ranges([point..point]),
                    );
                });
            })?;
        }
        anyhow::Ok(())
    })
    .detach_and_log_err(cx);
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
            Found::File { absolute, .. }
            | Found::Text { absolute, .. }
            | Found::Symbol { absolute, .. } => Some(PreviewUpdate::from_path(absolute.clone()?)),
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
                self.symbols_state = None;
                self.selected = 0;
                Task::ready(())
            }
            Among::Symbols => {
                let (found, state) = self.matching_symbols(&query, cx);
                self.found = found;
                self.symbols_state = state;
                self.selected = 0;
                Task::ready(())
            }
            Among::Files => {
                let candidates = self.candidates.clone();
                let asked = query.clone();
                let background = cx.background_executor().clone();
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
                            picker.delegate.found = picker.delegate.matching_files(matches);
                            picker.delegate.symbols_state = None;
                            picker.delegate.selected = 0;
                            cx.notify();
                        })
                        .log_err();
                })
            }
            Among::Everything => {
                let candidates = self.candidates.clone();
                let asked = query.clone();
                let background = cx.background_executor().clone();
                let commands = self.matching_commands(&query, window, cx);
                let (symbols, symbols_state) = self.matching_symbols(&query, cx);
                let project = self.project.clone();
                let text_query = query.clone();
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
                    // The cheap kinds are shown the moment they are ready and
                    // are not made to wait for the expensive one. Searching the
                    // text of a whole project takes as long as it takes, and a
                    // combined list that waited for it would show nothing at all
                    // meanwhile -- which reads as "nothing matched" rather than
                    // "still looking".
                    //
                    // Files first: a name typed into this window is a file far
                    // more often than anything else. Symbols next, since they
                    // are also "go to this code", then commands.
                    picker
                        .update(cx, |picker, cx| {
                            let mut found = picker.delegate.matching_files(matches);
                            found.extend(symbols);
                            found.extend(commands);
                            picker.delegate.found = found;
                            picker.delegate.symbols_state = symbols_state;
                            picker.delegate.selected = 0;
                            cx.notify();
                        })
                        .log_err();

                    if text_query.trim().is_empty() {
                        return;
                    }
                    let text = text_of(project, text_query, cx).await;
                    if text.is_empty() {
                        return;
                    }
                    // Appended, not substituted: whatever the reader is already
                    // looking at stays where it is, and the loosest kind of
                    // match arrives underneath it.
                    picker
                        .update(cx, |picker, cx| {
                            picker.delegate.found.extend(text);
                            cx.notify();
                        })
                        .log_err();
                })
            }
            Among::Text => {
                self.symbols_state = None;
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
                open_path_in_workspace(path.clone(), None, self.workspace.clone(), window, cx);
            }
            Found::Symbol { path, line, .. } => {
                open_path_in_workspace(
                    path.clone(),
                    Some(*line),
                    self.workspace.clone(),
                    window,
                    cx,
                );
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
            Found::Symbol {
                name,
                kind,
                path,
                at,
                line,
                ..
            } => {
                let file_name = path
                    .path
                    .as_std_path()
                    .file_name()
                    .and_then(|name| name.to_str())
                    .unwrap_or_default();
                let where_it_is = match at.is_empty() {
                    true => format!("{file_name}:{line}"),
                    false => format!("{at}/{file_name}:{line}"),
                };
                (IconName::Code, format!("{name}  ·  {kind}"), where_it_is)
            }
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
        // An empty list of symbols has to say whether that means "no
        // matches" or "there is no index to search yet" -- the whole reason
        // `SymbolIndex::state` exists as an enum rather than a boolean. Shown
        // only in place of an empty list, never over real results: a reader
        // who already has matches does not need to be told about the index
        // that also, separately, has none of its own to add.
        if self.found.is_empty()
            && let Some(status) = self.symbols_status()
        {
            return Some(
                h_flex()
                    .w_full()
                    .min_w_0()
                    .px_2()
                    .py_1()
                    .child(
                        Label::new(status)
                            .size(LabelSize::Small)
                            .color(Color::Muted),
                    )
                    .into_any_element(),
            );
        }

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
            // To the row's own end, not to the largest column there is: the rope
            // rejects a column past the row it is given, so asking for one is a
            // panic rather than a clamp.
            let line: String = snapshot
                .text_for_range(
                    language::Point::new(at.row, 0)
                        ..language::Point::new(at.row, snapshot.line_len(at.row)),
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

#[cfg(test)]
mod tests {
    use super::*;
    use editor::Editor;
    use gpui::{TestAppContext, VisualTestContext};
    use menu::Confirm;
    use project::FakeFs;
    use serde_json::json;
    use settings::SettingsStore;
    use util::path;
    use workspace::MultiWorkspace;

    fn init_test(cx: &mut TestAppContext) {
        cx.update(|cx| {
            let store = SettingsStore::test(cx);
            cx.set_global(store);
            theme_settings::init(theme::LoadThemes::JustBase, cx);
            release_channel::init(semver::Version::new(0, 0, 0), cx);
            editor::init(cx);
            crate::init(cx);
            // Deliberately not `symbol_index::init`: its own `cx.observe_new`
            // would register a project's index at the real, shared
            // `paths::database_dir()` the moment a workspace opens. Tests
            // that need an index call `symbol_index::ensure_index_at`
            // directly with a scratch directory instead, before opening the
            // window that would otherwise race it there.
        });
    }

    /// A project that exists twice over at one and the same path: the editor's
    /// side is the deterministic in-memory filesystem, and the index's side is a
    /// real directory, because the symbol index walks and reads the disk itself
    /// rather than going through the editor's filesystem abstraction. Giving
    /// both the same absolute path is what lets one test cover the whole chain.
    async fn a_project_on_disk(
        files: &[(&str, &str)],
        cx: &mut TestAppContext,
    ) -> (tempfile::TempDir, Entity<Project>) {
        let held = tempfile::tempdir().expect("a directory to put a project in");
        let mut tree = serde_json::Map::new();
        for (name, contents) in files {
            std::fs::write(held.path().join(name), contents).expect("a project file on disk");
            tree.insert(
                (*name).to_string(),
                serde_json::Value::String((*contents).to_string()),
            );
        }
        let fs = FakeFs::new(cx.executor());
        fs.insert_tree(held.path(), serde_json::Value::Object(tree))
            .await;
        let project = Project::test(fs, [held.path()], cx).await;
        (held, project)
    }

    /// A project's index, kept at a scratch directory rather than the real
    /// `paths::database_dir()` `symbol_index::init` would otherwise use.
    /// Registered before the window opens, so `symbol_index::of_project`
    /// already finds it the same way it would find one `init` had built.
    fn index_for_test(
        project: Entity<Project>,
        cx: &mut TestAppContext,
    ) -> (tempfile::TempDir, Entity<symbol_index::SymbolIndex>) {
        let held = tempfile::tempdir().expect("a directory for the index's own files");
        let index_dir = held.path().join("symbol_index");
        let index = cx.update(|cx| symbol_index::ensure_index_at(project, index_dir, cx));
        (held, index)
    }

    /// Opens a real workspace over `project` and toggles the real window open
    /// through the real `Toggle` action, exactly as a keybinding would.
    fn build_picker(
        project: Entity<Project>,
        cx: &mut TestAppContext,
    ) -> (
        Entity<Picker<SearchEverywhereDelegate>>,
        Entity<Workspace>,
        &mut VisualTestContext,
    ) {
        let (multi_workspace, cx) =
            cx.add_window_view(|window, cx| MultiWorkspace::test_new(project, window, cx));
        let workspace = multi_workspace.read_with(cx, |mw, _| mw.workspace().clone());
        cx.dispatch_action(Toggle);
        let picker = workspace.update(cx, |workspace, cx| {
            workspace
                .active_modal::<Picker<SearchEverywhereDelegate>>(cx)
                .expect("the window did not open")
        });
        (picker, workspace, cx)
    }

    /// The bug this guards against: `open_path` returns the task that does
    /// the actual opening, and handing that straight to `.log_err()` without
    /// awaiting it drops -- and so cancels -- the open before it does
    /// anything. Confirmed through the real action, not by calling
    /// `delegate.confirm` directly, and asserted on the workspace's own
    /// active item rather than on anything the delegate remembers about
    /// itself.
    #[gpui::test]
    async fn confirming_a_result_opens_that_file(cx: &mut TestAppContext) {
        init_test(cx);
        let (_on_disk, project) =
            a_project_on_disk(&[("findme.rs", "pub fn findme() {}\n")], cx).await;

        let (picker, workspace, cx) = build_picker(project, cx);
        cx.simulate_input("findme");
        cx.run_until_parked();
        picker.read_with(cx, |picker, _| {
            assert!(
                !picker.delegate.found.is_empty(),
                "the file should have been matched before confirming it"
            );
        });

        cx.dispatch_action(Confirm);
        cx.run_until_parked();

        workspace.read_with(cx, |workspace, cx| {
            let active = workspace
                .active_item_as::<Editor>(cx)
                .expect("confirming the result should have opened an editor");
            assert_eq!(active.read(cx).title(cx), "findme.rs");
        });
    }

    /// The bug this guards against: a subdirectory opened as its own visible
    /// worktree, alongside a project that already contains it, reports every
    /// one of its files twice -- once under each worktree's own traversal.
    /// Added child first, then parent: `find_or_create_worktree` folds a
    /// later path into an existing worktree that already covers it, so
    /// adding them the other way around is what keeps the two genuinely
    /// separate, overlapping worktrees the pre-fix code duplicates a file
    /// across.
    #[gpui::test]
    async fn a_file_reachable_through_two_worktrees_appears_once(cx: &mut TestAppContext) {
        init_test(cx);
        let fs = FakeFs::new(cx.executor());
        fs.insert_tree(
            path!("/project"),
            json!({ "subdir": { "shared.rs": "pub fn shared_symbol() {}\n" } }),
        )
        .await;
        let project = Project::test(
            fs,
            [
                path!("/project/subdir").as_ref(),
                path!("/project").as_ref(),
            ],
            cx,
        )
        .await;
        assert_eq!(
            project.read_with(cx, |project, cx| project.visible_worktrees(cx).count()),
            2,
            "the fixture must genuinely have two overlapping worktrees, or this test proves nothing"
        );

        let (picker, _workspace, cx) = build_picker(project, cx);
        cx.simulate_input("shared");
        cx.run_until_parked();

        picker.read_with(cx, |picker, _| {
            let names: Vec<&str> = picker
                .delegate
                .found
                .iter()
                .filter_map(|found| match found {
                    Found::File { name, .. } => Some(name.as_str()),
                    _ => None,
                })
                .collect();
            assert_eq!(
                names,
                vec!["shared.rs"],
                "shared.rs is reachable through both worktrees and must still appear once: {names:?}"
            );
        });
    }

    #[gpui::test]
    async fn typing_a_symbol_name_finds_it_and_confirming_opens_its_file(cx: &mut TestAppContext) {
        init_test(cx);
        let (_on_disk, project) = a_project_on_disk(
            &[("lib.rs", "pub fn first() {}\n\npub fn take_stock() {}\n")],
            cx,
        )
        .await;
        let (_held, index) = index_for_test(project.clone(), cx);
        cx.run_until_parked();
        index.read_with(cx, |index, _| {
            assert!(
                matches!(index.state(), symbol_index::State::Ready { .. }),
                "expected the index to be ready, found {:?}",
                index.state()
            );
        });

        let (picker, workspace, cx) = build_picker(project, cx);
        picker.update_in(cx, |picker, window, cx| {
            picker.delegate.look_among(Among::Symbols);
            picker.refresh_placeholder(window, cx);
        });
        cx.simulate_input("takestock");
        cx.run_until_parked();
        picker.read_with(cx, |picker, _| {
            let names: Vec<&str> = picker
                .delegate
                .found
                .iter()
                .filter_map(|found| match found {
                    Found::Symbol { name, .. } => Some(name.as_str()),
                    _ => None,
                })
                .collect();
            assert_eq!(names, vec!["take_stock"], "{names:?}");
        });

        cx.dispatch_action(Confirm);
        cx.run_until_parked();

        workspace.read_with(cx, |workspace, cx| {
            let active = workspace
                .active_item_as::<Editor>(cx)
                .expect("confirming the symbol should have opened an editor");
            assert_eq!(active.read(cx).title(cx), "lib.rs");
        });
    }

    #[gpui::test]
    async fn an_empty_symbol_list_explains_itself_rather_than_reading_as_no_matches(
        cx: &mut TestAppContext,
    ) {
        init_test(cx);
        // A project with no local worktree: there is nothing to index, so the
        // state is settled before the test starts and cannot change under it.
        // A project that really is being indexed would finish building the
        // moment the test pumps the executor, and the assertion would then be
        // about timing rather than about what the window says.
        let fs = FakeFs::new(cx.executor());
        let project = Project::test(fs, [] as [&std::path::Path; 0], cx).await;
        let (_held, index) = index_for_test(project.clone(), cx);
        cx.run_until_parked();
        index.read_with(cx, |index, _| {
            assert!(
                matches!(index.state(), symbol_index::State::NotBuilt),
                "expected NotBuilt, found {:?}",
                index.state()
            );
        });

        let (picker, _workspace, cx) = build_picker(project, cx);
        picker.update_in(cx, |picker, window, cx| {
            picker.delegate.look_among(Among::Symbols);
            picker.refresh_placeholder(window, cx);
        });
        cx.simulate_input("takestock");
        cx.run_until_parked();

        picker.read_with(cx, |picker, _| {
            assert!(picker.delegate.found.is_empty(), "there is nothing to find");
            // What `render_footer` shows in place of the empty list. An empty
            // result with no explanation reads as "no matches", which is a
            // different and wrong answer.
            let status = picker.delegate.symbols_status();
            assert!(
                status
                    .as_deref()
                    .is_some_and(|status| status.contains("no index")),
                "{status:?}"
            );
        });
    }

    #[gpui::test]
    async fn the_symbols_tab_shows_only_symbols_and_everything_shows_them_too(
        cx: &mut TestAppContext,
    ) {
        init_test(cx);
        let (_on_disk, project) =
            a_project_on_disk(&[("find_symbol.rs", "pub fn find_symbol() {}\n")], cx).await;
        let (_held, index) = index_for_test(project.clone(), cx);
        cx.run_until_parked();
        index.read_with(cx, |index, _| {
            assert!(matches!(index.state(), symbol_index::State::Ready { .. }));
        });

        let (picker, _workspace, cx) = build_picker(project, cx);

        // The Symbols tab: a query that also names the file itself must not
        // let the file result leak in.
        picker.update_in(cx, |picker, window, cx| {
            picker.delegate.look_among(Among::Symbols);
            picker.refresh_placeholder(window, cx);
        });
        cx.simulate_input("findsymbol");
        cx.run_until_parked();
        picker.read_with(cx, |picker, _| {
            assert!(
                picker
                    .delegate
                    .found
                    .iter()
                    .all(|found| matches!(found, Found::Symbol { .. })),
                "the Symbols tab must show only symbols: {:?}",
                picker
                    .delegate
                    .found
                    .iter()
                    .map(Found::where_it_is)
                    .collect::<Vec<_>>()
            );
            assert!(
                picker.delegate.found.iter().any(
                    |found| matches!(found, Found::Symbol { name, .. } if name == "find_symbol")
                ),
                "{:?}",
                picker
                    .delegate
                    .found
                    .iter()
                    .map(Found::where_it_is)
                    .collect::<Vec<_>>()
            );
        });

        // Everything: the same query now also matches the file by name, and
        // both kinds must be present together.
        //
        // The query is set rather than typed again: typing appends to what is
        // already in the field, and the second `findsymbol` would have made the
        // query `findsymbolfindsymbol`, which matches nothing at all.
        picker.update_in(cx, |picker, window, cx| {
            picker.delegate.look_among(Among::Everything);
            picker.refresh_placeholder(window, cx);
            picker.set_query("findsymbol", window, cx);
        });
        cx.run_until_parked();
        picker.read_with(cx, |picker, _| {
            let has_file = picker
                .delegate
                .found
                .iter()
                .any(|found| matches!(found, Found::File { .. }));
            let has_symbol = picker
                .delegate
                .found
                .iter()
                .any(|found| matches!(found, Found::Symbol { .. }));
            assert!(
                has_file && has_symbol,
                "Everything must show symbols alongside the other kinds: {:?}",
                picker
                    .delegate
                    .found
                    .iter()
                    .map(Found::where_it_is)
                    .collect::<Vec<_>>()
            );
        });
    }

    #[gpui::test]
    async fn ranking_puts_the_closer_matching_symbol_first(cx: &mut TestAppContext) {
        init_test(cx);
        let (_on_disk, project) = a_project_on_disk(
            &[(
                "lib.rs",
                "pub fn take_stock() {}\npub fn the_whole_pass() {}\n",
            )],
            cx,
        )
        .await;
        let (_held, index) = index_for_test(project.clone(), cx);
        cx.run_until_parked();

        let (picker, _workspace, cx) = build_picker(project, cx);
        picker.update_in(cx, |picker, window, cx| {
            picker.delegate.look_among(Among::Symbols);
            picker.refresh_placeholder(window, cx);
        });
        // Matches both `take_stock` and `the_whole_pass` (t-...-s), but is a
        // much closer match for the first: this is the shared matcher's own
        // ranking, not an artifact of the order the index happened to return
        // them in.
        cx.simulate_input("tstock");
        cx.run_until_parked();

        picker.read_with(cx, |picker, _| {
            let names: Vec<&str> = picker
                .delegate
                .found
                .iter()
                .filter_map(|found| match found {
                    Found::Symbol { name, .. } => Some(name.as_str()),
                    _ => None,
                })
                .collect();
            assert_eq!(
                names.first().copied(),
                Some("take_stock"),
                "the closer match must be ranked first: {names:?}"
            );
        });
        let _ = index;
    }
}
