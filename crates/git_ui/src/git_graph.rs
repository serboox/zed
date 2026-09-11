pub use crate::commit_context_menu::{CopyCommitSha, CopyCommitTag, OpenCommitView};
use crate::{
    commit_context_menu::{CommitContextMenuData, CommitContextMenuSource, commit_context_menu},
    commit_tooltip::CommitAvatar,
    commit_view::CommitView,
    git_status_icon,
    project_diff::ProjectDiff,
};
use collections::{BTreeMap, HashMap, HashSet, IndexSet};
use editor::Editor;
use file_icons::FileIcons;
use git::{
    BuildCommitPermalinkParams, GitHostingProviderRegistry, GitRemote, Oid, ParsedGitRemote,
    parse_git_remote_url,
    repository::{
        CommitDiff, CommitFile, InitialGraphCommitData, LogOrder, LogSource, RepoPath,
        SearchCommitArgs,
    },
    status::{FileStatus, StatusCode, TrackedStatus},
};
use gpui::{
    Anchor, AnyElement, App, Bounds, ClickEvent, ClipboardItem, DefiniteLength, DismissEvent,
    DragMoveEvent, ElementId, Empty, Entity, EventEmitter, FocusHandle, Focusable, Hsla,
    MouseButton, MouseDownEvent, MouseMoveEvent, PathBuilder, Pixels, Point, ScrollHandle,
    ScrollStrategy, ScrollWheelEvent, SharedString, Subscription, Task, TextStyleRefinement,
    UniformListScrollHandle, WeakEntity, Window, actions, anchored, deferred, hsla, point,
    prelude::*, px, uniform_list,
};
use language::line_diff;
use markdown::{Markdown, MarkdownElement};
use menu::{Cancel, SelectFirst, SelectLast, SelectNext, SelectPrevious};
use picker::{Picker, PickerDelegate};
use project::{
    ProjectPath,
    git_store::{
        CommitDataState, GitGraphEvent, GitStore, GitStoreEvent, GraphDataResponse, Repository,
        RepositoryEvent, RepositoryId,
    },
};
use search::{
    SearchOption, SearchOptions, SearchSource, SelectNextMatch, SelectPreviousMatch,
    ToggleCaseSensitive, buffer_search,
};
use smallvec::{SmallVec, smallvec};
use std::{
    cell::Cell,
    ops::Range,
    rc::Rc,
    sync::{Arc, OnceLock},
    time::{Duration, Instant},
};

use theme::AccentColors;
use time::{OffsetDateTime, UtcOffset, format_description::BorrowedFormatItem};
use ui::{
    Chip, ColumnWidthConfig, CommonAnimationExt as _, ContextMenu, DiffStat, Divider,
    HighlightedLabel, IndentGuideColors, ListItem, ListItemSpacing, Table, TableInteractionState,
    Tooltip, WithScrollbar, prelude::*, table_row::TableRow,
};
use util::{ResultExt, debug_panic};
use workspace::{
    ModalView, Workspace,
    item::{Item, ItemEvent, TabTooltipContent},
};

const COMMIT_CIRCLE_RADIUS: Pixels = px(3.5);
const LANE_WIDTH: Pixels = px(16.0);
const LEFT_PADDING: Pixels = px(12.0);
const LINE_WIDTH: Pixels = px(1.5);
const RESIZE_HANDLE_WIDTH: f32 = 8.0;
const COPIED_STATE_DURATION: Duration = Duration::from_secs(2);
const COMMIT_TAG_LIST_WIDTH_IN_REMS: Rems = rems(10.);
const TREE_INDENT: f32 = 20.0;
const TABLE_COLUMN_COUNT: usize = 4;

struct CopiedState {
    copied_at: Option<Instant>,
}

impl CopiedState {
    fn new(_window: &mut Window, _cx: &mut Context<Self>) -> Self {
        Self { copied_at: None }
    }

    fn is_copied(&self) -> bool {
        self.copied_at
            .map(|t| t.elapsed() < COPIED_STATE_DURATION)
            .unwrap_or(false)
    }

    fn mark_copied(&mut self) {
        self.copied_at = Some(Instant::now());
    }
}

struct DraggedSplitHandle;

struct CommitTagPicker {
    picker: Entity<Picker<CommitTagPickerDelegate>>,
}

impl CommitTagPicker {
    fn new(tag_names: Vec<SharedString>, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let delegate = CommitTagPickerDelegate {
            picker: cx.entity().downgrade(),
            tag_names,
            selected_index: 0,
        };
        let picker = cx.new(|cx| {
            Picker::nonsearchable_uniform_list(delegate, window, cx)
                .initial_width(COMMIT_TAG_LIST_WIDTH_IN_REMS)
        });
        Self { picker }
    }
}

impl EventEmitter<DismissEvent> for CommitTagPicker {}
impl ModalView for CommitTagPicker {}

impl Focusable for CommitTagPicker {
    fn focus_handle(&self, cx: &App) -> FocusHandle {
        self.picker.focus_handle(cx)
    }
}

impl Render for CommitTagPicker {
    fn render(&mut self, _window: &mut Window, _cx: &mut Context<Self>) -> impl IntoElement {
        v_flex().child(self.picker.clone())
    }
}

struct CommitTagPickerDelegate {
    picker: WeakEntity<CommitTagPicker>,
    tag_names: Vec<SharedString>,
    selected_index: usize,
}

impl PickerDelegate for CommitTagPickerDelegate {
    type ListItem = ListItem;

    fn name() -> &'static str {
        "commit-tag"
    }

    fn placeholder_text(&self, _window: &mut Window, _cx: &mut App) -> Arc<str> {
        "Copy Tag".into()
    }

    fn match_count(&self) -> usize {
        self.tag_names.len()
    }

    fn selected_index(&self) -> usize {
        self.selected_index
    }

    fn set_selected_index(
        &mut self,
        ix: usize,
        _window: &mut Window,
        _cx: &mut Context<Picker<Self>>,
    ) {
        self.selected_index = ix;
    }

    fn update_matches(
        &mut self,
        _query: String,
        _window: &mut Window,
        _cx: &mut Context<Picker<Self>>,
    ) -> Task<()> {
        Task::ready(())
    }

    fn confirm(&mut self, _secondary: bool, window: &mut Window, cx: &mut Context<Picker<Self>>) {
        if let Some(tag_name) = self.tag_names.get(self.selected_index) {
            cx.write_to_clipboard(ClipboardItem::new_string(tag_name.to_string()));
        }
        self.dismissed(window, cx);
    }

    fn dismissed(&mut self, _window: &mut Window, cx: &mut Context<Picker<Self>>) {
        self.picker
            .update(cx, |_this, cx| cx.emit(DismissEvent))
            .ok();
    }

    fn render_match(
        &self,
        ix: usize,
        selected: bool,
        _window: &mut Window,
        _cx: &mut Context<Picker<Self>>,
    ) -> Option<Self::ListItem> {
        Some(
            ListItem::new(ix)
                .inset(true)
                .spacing(ListItemSpacing::Sparse)
                .toggle_state(selected)
                .child(Label::new(self.tag_names.get(ix)?.clone())),
        )
    }
}

#[derive(Clone)]
struct ChangedFileEntry {
    status: FileStatus,
    file_name: SharedString,
    dir_path: SharedString,
    repo_path: RepoPath,
}

impl ChangedFileEntry {
    fn from_commit_file(file: &CommitFile, _cx: &App) -> Self {
        let file_name: SharedString = file
            .path
            .file_name()
            .map(|n| n.to_string())
            .unwrap_or_default()
            .into();
        let dir_path: SharedString = file
            .path
            .parent()
            .map(|p| p.as_unix_str().to_string())
            .unwrap_or_default()
            .into();

        let status_code = match (&file.old_text, &file.new_text) {
            (None, Some(_)) => StatusCode::Added,
            (Some(_), None) => StatusCode::Deleted,
            _ => StatusCode::Modified,
        };

        let status = FileStatus::Tracked(TrackedStatus {
            index_status: status_code,
            worktree_status: StatusCode::Unmodified,
        });

        Self {
            status,
            file_name,
            dir_path,
            repo_path: file.path.clone(),
        }
    }

    fn open_in_commit_view(
        &self,
        commit_sha: &SharedString,
        repository: &WeakEntity<Repository>,
        workspace: &WeakEntity<Workspace>,
        window: &mut Window,
        cx: &mut App,
    ) {
        CommitView::open(
            commit_sha.to_string(),
            repository.clone(),
            workspace.clone(),
            None,
            Some(self.repo_path.clone()),
            window,
            cx,
        );
    }

    fn render(
        &self,
        ix: usize,
        depth: usize,
        directory_label: Option<SharedString>,
        commit_sha: SharedString,
        repository: WeakEntity<Repository>,
        workspace: WeakEntity<Workspace>,
        _cx: &App,
    ) -> AnyElement {
        let file_name = self.file_name.clone();
        let dir_path = self.dir_path.clone();

        ListItem::new(("changed-file", ix))
            .spacing(ListItemSpacing::Sparse)
            .indent_level(depth)
            .indent_step_size(px(TREE_INDENT))
            .start_slot(git_status_icon(self.status))
            .child(
                Label::new(file_name.clone())
                    .size(LabelSize::Small)
                    .truncate(),
            )
            .when_some(directory_label, |this, directory_label| {
                this.child(
                    Label::new(directory_label)
                        .size(LabelSize::Small)
                        .color(Color::Muted)
                        .truncate_start(),
                )
            })
            .tooltip({
                let meta = if dir_path.is_empty() {
                    file_name
                } else {
                    format!("{}/{}", dir_path, file_name).into()
                };
                move |_, cx| Tooltip::with_meta("View Changes", None, meta.clone(), cx)
            })
            .on_click({
                let entry = self.clone();
                move |_, window, cx| {
                    entry.open_in_commit_view(&commit_sha, &repository, &workspace, window, cx);
                }
            })
            .into_any_element()
    }
}

enum ChangedFileTreeEntry {
    Directory(ChangedFileDirectoryEntry),
    File(ChangedFileTreeStatusEntry),
}

struct ChangedFileTreeStatusEntry {
    entry: ChangedFileEntry,
    depth: usize,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
enum ChangedFilesViewMode {
    Flat,
    #[default]
    Tree,
}

impl ChangedFilesViewMode {
    fn toggled(self) -> Self {
        match self {
            Self::Flat => Self::Tree,
            Self::Tree => Self::Flat,
        }
    }

    fn is_tree(self) -> bool {
        matches!(self, Self::Tree)
    }
}

struct ChangedFileDirectoryEntry {
    path: RepoPath,
    name: SharedString,
    depth: usize,
    expanded: bool,
}

impl ChangedFileDirectoryEntry {
    fn render(&self, ix: usize, git_graph: WeakEntity<GitGraph>, cx: &App) -> AnyElement {
        let path = self.path.clone();
        let expanded = self.expanded;
        let folder_icon = FileIcons::get_folder_icon(expanded, path.as_std_path(), cx)
            .map(|icon| {
                Icon::from_path(icon)
                    .size(IconSize::Small)
                    .color(Color::Muted)
            })
            .unwrap_or_else(|| {
                let icon = if expanded {
                    IconName::FolderOpen
                } else {
                    IconName::Folder
                };
                Icon::new(icon).size(IconSize::Small).color(Color::Muted)
            });

        ListItem::new(("changed-file-dir", ix))
            .spacing(ListItemSpacing::Sparse)
            .indent_level(self.depth)
            .indent_step_size(px(TREE_INDENT))
            .start_slot(folder_icon)
            .child(
                Label::new(self.name.clone())
                    .size(LabelSize::Small)
                    .color(Color::Muted)
                    .truncate(),
            )
            .tooltip({
                let name = self.name.clone();
                move |_, cx| Tooltip::with_meta("Toggle Folder", None, name.clone(), cx)
            })
            .on_click(move |_, _, cx| {
                git_graph
                    .update(cx, |git_graph, cx| {
                        git_graph
                            .changed_files_expanded_dirs
                            .insert(path.clone(), !expanded);
                        cx.notify();
                    })
                    .ok();
            })
            .into_any_element()
    }
}

#[derive(Default)]
struct ChangedFileTreeNode {
    name: SharedString,
    path: Option<RepoPath>,
    children: BTreeMap<SharedString, ChangedFileTreeNode>,
    files: Vec<ChangedFileEntry>,
}

fn build_changed_file_tree_entries(
    mut files: Vec<ChangedFileEntry>,
    expanded_dirs: &HashMap<RepoPath, bool>,
) -> Vec<ChangedFileTreeEntry> {
    files.sort_by(|a, b| a.repo_path.cmp(&b.repo_path));

    let mut root = ChangedFileTreeNode::default();
    for file in files {
        let components: Vec<&str> = file.repo_path.components().collect();
        if components.is_empty() {
            root.files.push(file);
            continue;
        }

        let mut current = &mut root;
        let mut current_path = String::new();

        for (ix, component) in components.iter().enumerate() {
            if ix == components.len() - 1 {
                current.files.push(file.clone());
            } else {
                if !current_path.is_empty() {
                    current_path.push('/');
                }
                current_path.push_str(component);

                let Ok(dir_path) = RepoPath::new(&current_path) else {
                    continue;
                };
                let component = SharedString::from(component.to_string());

                current = current
                    .children
                    .entry(component.clone())
                    .or_insert_with(|| ChangedFileTreeNode {
                        name: component,
                        path: Some(dir_path),
                        ..Default::default()
                    });
            }
        }
    }

    flatten_changed_file_tree(&root, 0, expanded_dirs)
}

fn flatten_changed_file_tree(
    node: &ChangedFileTreeNode,
    depth: usize,
    expanded_dirs: &HashMap<RepoPath, bool>,
) -> Vec<ChangedFileTreeEntry> {
    let mut entries = Vec::new();

    for child in node.children.values() {
        let (terminal, name) = compact_changed_file_directory_chain(child);
        let Some(path) = terminal.path.clone().or_else(|| child.path.clone()) else {
            continue;
        };
        let expanded = *expanded_dirs.get(&path).unwrap_or(&true);
        let child_entries = flatten_changed_file_tree(terminal, depth + 1, expanded_dirs);

        entries.push(ChangedFileTreeEntry::Directory(ChangedFileDirectoryEntry {
            path,
            name,
            depth,
            expanded,
        }));

        if expanded {
            entries.extend(child_entries);
        }
    }

    entries.extend(
        node.files
            .iter()
            .cloned()
            .map(|entry| ChangedFileTreeEntry::File(ChangedFileTreeStatusEntry { entry, depth })),
    );
    entries
}

fn compact_changed_file_directory_chain(
    mut node: &ChangedFileTreeNode,
) -> (&ChangedFileTreeNode, SharedString) {
    let mut parts = vec![node.name.clone()];
    while node.files.is_empty() && node.children.len() == 1 {
        let Some(child) = node.children.values().next() else {
            continue;
        };
        if child.path.is_none() {
            break;
        }
        parts.push(child.name.clone());
        node = child;
    }
    (node, SharedString::from(parts.join("/")))
}

enum QueryState {
    Pending(SharedString),
    Confirmed((SharedString, Task<()>)),
    Empty,
}

impl QueryState {
    fn next_state(&mut self) {
        match self {
            Self::Confirmed((query, _)) => *self = Self::Pending(std::mem::take(query)),
            _ => {}
        };
    }
}

struct SearchState {
    case_sensitive: bool,
    editor: Entity<Editor>,
    state: QueryState,
    matches: IndexSet<Oid>,
    selected_index: Option<usize>,
}

struct SplitState {
    left_ratio: f32,
    visible_left_ratio: f32,
}

impl SplitState {
    fn new() -> Self {
        Self {
            left_ratio: 1.0,
            visible_left_ratio: 1.0,
        }
    }

    fn right_ratio(&self) -> f32 {
        1.0 - self.visible_left_ratio
    }

    fn on_drag_move(
        &mut self,
        drag_event: &DragMoveEvent<DraggedSplitHandle>,
        _window: &mut Window,
        _cx: &mut Context<Self>,
    ) {
        let drag_position = drag_event.event.position;
        let bounds = drag_event.bounds;
        let bounds_width = bounds.right() - bounds.left();

        let min_ratio = 0.1;
        let max_ratio = 0.9;

        let new_ratio = (drag_position.x - bounds.left()) / bounds_width;
        self.visible_left_ratio = new_ratio.clamp(min_ratio, max_ratio);
    }

    fn commit_ratio(&mut self) {
        self.left_ratio = self.visible_left_ratio;
    }

    fn on_double_click(&mut self) {
        self.left_ratio = 1.0;
        self.visible_left_ratio = 1.0;
    }
}

actions!(
    git_graph,
    [
        /// Opens the Git Graph Tab.
        Open,
        /// Focuses the search field.
        FocusSearch,
        /// Focuses the next git graph tab stop.
        FocusNextTabStop,
        /// Focuses the previous git graph tab stop.
        FocusPreviousTabStop,
        /// Selects a commit half a page above the current selection.
        ScrollUp,
        /// Selects a commit half a page below the current selection.
        ScrollDown,
        /// Toggles the selected commit's changed files between flat and tree views.
        ToggleChangedFilesView,
        /// Selects the first parent of the selected commit, staying on its branch.
        SelectFirstParent,
        /// Selects the commit that has the selected one as its first parent.
        SelectFirstChild,
    ]
);

/// Opens the Git Graph Tab at a specific commit.
#[derive(Clone, PartialEq, serde::Deserialize, schemars::JsonSchema, gpui::Action)]
#[action(namespace = git_graph)]
pub struct OpenAtCommit {
    pub sha: String,
}

fn timestamp_format() -> &'static [BorrowedFormatItem<'static>] {
    static FORMAT: OnceLock<Vec<BorrowedFormatItem<'static>>> = OnceLock::new();
    FORMAT.get_or_init(|| {
        time::format_description::parse_borrowed::<1>(
            "[day] [month repr:short] [year] [hour]:[minute]",
        )
        .unwrap_or_default()
    })
}

fn format_timestamp(timestamp: i64) -> String {
    let Ok(datetime) = OffsetDateTime::from_unix_timestamp(timestamp) else {
        return "Unknown".to_string();
    };

    let local_offset = UtcOffset::current_local_offset().unwrap_or(UtcOffset::UTC);
    let local_datetime = datetime.to_offset(local_offset);

    local_datetime
        .format(timestamp_format())
        .unwrap_or_default()
}

pub(crate) fn accent_colors_count(accents: &AccentColors) -> usize {
    accents.0.len()
}

#[derive(Copy, Clone, Debug)]
struct BranchColor(u8);

#[derive(Debug)]
enum LaneState {
    Empty,
    Active {
        child: Oid,
        parent: Oid,
        color: Option<BranchColor>,
        starting_row: usize,
        starting_col: usize,
        destination_column: Option<usize>,
        segments: SmallVec<[CommitLineSegment; 1]>,
    },
}

impl LaneState {
    fn to_commit_lines(
        &mut self,
        ending_row: usize,
        lane_column: usize,
        parent_column: usize,
        parent_color: BranchColor,
    ) -> Option<CommitLine> {
        let state = std::mem::replace(self, LaneState::Empty);

        match state {
            LaneState::Active {
                #[cfg_attr(not(test), allow(unused_variables))]
                parent,
                #[cfg_attr(not(test), allow(unused_variables))]
                child,
                color,
                starting_row,
                starting_col,
                destination_column,
                mut segments,
            } => {
                let final_destination = destination_column.unwrap_or(parent_column);
                let final_color = color.unwrap_or(parent_color);

                Some(CommitLine {
                    #[cfg(test)]
                    child,
                    #[cfg(test)]
                    parent,
                    child_column: starting_col,
                    full_interval: starting_row..ending_row,
                    color_idx: final_color.0 as usize,
                    segments: {
                        match segments.last_mut() {
                            Some(CommitLineSegment::Straight { to_row })
                                if *to_row == usize::MAX =>
                            {
                                if final_destination != lane_column {
                                    *to_row = ending_row - 1;

                                    let curved_line = CommitLineSegment::Curve {
                                        to_column: final_destination,
                                        on_row: ending_row,
                                        curve_kind: CurveKind::Checkout,
                                    };

                                    if *to_row == starting_row {
                                        let last_index = segments.len() - 1;
                                        segments[last_index] = curved_line;
                                    } else {
                                        segments.push(curved_line);
                                    }
                                } else {
                                    *to_row = ending_row;
                                }
                            }
                            Some(CommitLineSegment::Curve {
                                on_row,
                                to_column,
                                curve_kind,
                            }) if *on_row == usize::MAX => {
                                if *to_column == usize::MAX {
                                    *to_column = final_destination;
                                }
                                if matches!(curve_kind, CurveKind::Merge) {
                                    *on_row = starting_row + 1;
                                    if *on_row < ending_row {
                                        if *to_column != final_destination {
                                            segments.push(CommitLineSegment::Straight {
                                                to_row: ending_row - 1,
                                            });
                                            segments.push(CommitLineSegment::Curve {
                                                to_column: final_destination,
                                                on_row: ending_row,
                                                curve_kind: CurveKind::Checkout,
                                            });
                                        } else {
                                            segments.push(CommitLineSegment::Straight {
                                                to_row: ending_row,
                                            });
                                        }
                                    } else if *to_column != final_destination {
                                        segments.push(CommitLineSegment::Curve {
                                            to_column: final_destination,
                                            on_row: ending_row,
                                            curve_kind: CurveKind::Checkout,
                                        });
                                    }
                                } else {
                                    *on_row = ending_row;
                                    if *to_column != final_destination {
                                        segments.push(CommitLineSegment::Straight {
                                            to_row: ending_row,
                                        });
                                        segments.push(CommitLineSegment::Curve {
                                            to_column: final_destination,
                                            on_row: ending_row,
                                            curve_kind: CurveKind::Checkout,
                                        });
                                    }
                                }
                            }
                            Some(CommitLineSegment::Curve {
                                on_row, to_column, ..
                            }) => {
                                if *on_row < ending_row {
                                    if *to_column != final_destination {
                                        segments.push(CommitLineSegment::Straight {
                                            to_row: ending_row - 1,
                                        });
                                        segments.push(CommitLineSegment::Curve {
                                            to_column: final_destination,
                                            on_row: ending_row,
                                            curve_kind: CurveKind::Checkout,
                                        });
                                    } else {
                                        segments.push(CommitLineSegment::Straight {
                                            to_row: ending_row,
                                        });
                                    }
                                } else if *to_column != final_destination {
                                    segments.push(CommitLineSegment::Curve {
                                        to_column: final_destination,
                                        on_row: ending_row,
                                        curve_kind: CurveKind::Checkout,
                                    });
                                }
                            }
                            _ => {}
                        }

                        segments
                    },
                })
            }
            LaneState::Empty => None,
        }
    }

    fn is_empty(&self) -> bool {
        match self {
            LaneState::Empty => true,
            LaneState::Active { .. } => false,
        }
    }
}

/// What one row has to paint of the lanes crossing it.
///
/// The graph is drawn a row at a time rather than as one canvas behind the
/// list, so that a node can be a real element carrying an avatar and initials
/// and so that a row and its dot cannot drift apart: they are the same element.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct LanePaint {
    /// The column the line occupies where it meets the row above.
    pub from_column: usize,
    /// The column it occupies where it meets the row below.
    pub to_column: usize,
    pub color_idx: usize,
    /// The line begins at this row's commit instead of arriving from above, so
    /// it is drawn from the node down.
    pub starts_at_node: bool,
    /// The line ends at this row's commit instead of leaving below it.
    pub ends_at_node: bool,
}

impl LanePaint {
    pub(crate) fn bends(self) -> bool {
        self.from_column != self.to_column
    }
}

pub(crate) struct CommitEntry {
    pub data: Arc<InitialGraphCommitData>,
    pub lane: usize,
    pub color_idx: usize,
}

type ActiveLaneIdx = usize;

enum AllCommitCount {
    NotLoaded,
    Loading(usize),
    FullyLoaded(usize),
}

#[derive(Debug)]
enum CurveKind {
    Merge,
    Checkout,
}

#[derive(Debug)]
enum CommitLineSegment {
    Straight {
        to_row: usize,
    },
    Curve {
        to_column: usize,
        on_row: usize,
        curve_kind: CurveKind,
    },
}

#[derive(Debug)]
struct CommitLine {
    #[cfg(test)]
    child: Oid,
    #[cfg(test)]
    parent: Oid,
    child_column: usize,
    full_interval: Range<usize>,
    color_idx: usize,
    segments: SmallVec<[CommitLineSegment; 1]>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
struct CommitLineKey {
    child: Oid,
    parent: Oid,
}

pub(crate) struct GraphData {
    lane_states: SmallVec<[LaneState; 8]>,
    lane_colors: HashMap<ActiveLaneIdx, BranchColor>,
    parent_to_lanes: HashMap<Oid, SmallVec<[usize; 1]>>,
    next_color: BranchColor,
    accent_colors_count: usize,
    pub commits: Vec<Rc<CommitEntry>>,
    max_commit_count: AllCommitCount,
    pub max_lanes: usize,
    lines: Vec<Rc<CommitLine>>,
    active_commit_lines: HashMap<CommitLineKey, usize>,
    active_commit_lines_by_parent: HashMap<Oid, SmallVec<[usize; 1]>>,
    /// Which row each commit landed on, so an edge can be followed from a
    /// parent's name back to the row it was drawn on.
    row_of_commit: HashMap<Oid, usize>,
    /// The rows that name a commit as their parent. Built as commits arrive,
    /// because finding them later would be a walk over the whole history for
    /// every hover.
    rows_naming_parent: HashMap<Oid, SmallVec<[usize; 2]>>,
    /// Every distinct label in the history loaded so far, so the column can be
    /// made as wide as the longest one. Only branch tips and tags carry a
    /// label, so this stays short; past the cap a history is not being read by
    /// its labels anyway.
    pub label_names: Vec<SharedString>,
    /// What each row paints of the lanes crossing it, filled in as lines close.
    /// Built once per line rather than searched per frame: the painter used to
    /// scan every line in the history on every frame to find the few crossing
    /// the viewport.
    rows_paint: Vec<SmallVec<[LanePaint; 4]>>,
}

impl GraphData {
    pub(crate) fn new(accent_colors_count: usize) -> Self {
        GraphData {
            lane_states: SmallVec::default(),
            lane_colors: HashMap::default(),
            parent_to_lanes: HashMap::default(),
            next_color: BranchColor(0),
            accent_colors_count,
            commits: Vec::default(),
            max_commit_count: AllCommitCount::NotLoaded,
            max_lanes: 0,
            lines: Vec::default(),
            active_commit_lines: HashMap::default(),
            active_commit_lines_by_parent: HashMap::default(),
            row_of_commit: HashMap::default(),
            rows_naming_parent: HashMap::default(),
            label_names: Vec::new(),
            rows_paint: Vec::default(),
        }
    }

    /// Lays a finished line out over the rows it crosses.
    ///
    /// Called once, when the line closes; every row it covers already exists,
    /// because a line closes on its parent's row and parents arrive after their
    /// children.
    fn index_line_for_rows(&mut self, line: &CommitLine) {
        let rows = self.rows_paint.len();
        let resolved = line.segments.iter().all(|segment| match segment {
            CommitLineSegment::Straight { to_row } => *to_row < rows,
            CommitLineSegment::Curve { on_row, .. } => *on_row < rows,
        });
        // An unresolved sentinel means the line never found its parent. Half of
        // such a line is worse than none of it: it would end in mid-air.
        if !resolved || line.full_interval.end >= rows {
            return;
        }

        let color_idx = line.color_idx;
        let first_row = line.full_interval.start;
        let last_row = line.full_interval.end;
        let mut column = line.child_column;
        let mut row = first_row;
        // Every loop below stops here, so a sentinel that slipped past the
        // guard would cost one pass over the rows rather than a hang.
        let last = rows.saturating_sub(1);
        // A line leaves at most one mark per row. A segment's last row is the
        // next segment's first, and at that seam it is the bend that describes
        // the row: the straight only says where the line came in, which the
        // bend already records.
        let mut last_marked: Option<usize> = None;
        let mut mark = |rows_paint: &mut Vec<SmallVec<[LanePaint; 4]>>,
                        at: usize,
                        from_column: usize,
                        to_column: usize| {
            let Some(slot) = rows_paint.get_mut(at) else {
                return;
            };
            if last_marked == Some(at) {
                if let Some(previous) = slot.last_mut() {
                    previous.to_column = to_column;
                    previous.ends_at_node |= at == last_row;
                }
                return;
            }
            slot.push(LanePaint {
                from_column,
                to_column,
                color_idx,
                starts_at_node: at == first_row,
                ends_at_node: at == last_row,
            });
            last_marked = Some(at);
        };

        for segment in line.segments.iter() {
            match segment {
                CommitLineSegment::Straight { to_row } => {
                    for at in row..=(*to_row).min(last) {
                        mark(&mut self.rows_paint, at, column, column);
                    }
                    row = *to_row;
                }
                CommitLineSegment::Curve {
                    to_column,
                    on_row,
                    curve_kind,
                } => {
                    match curve_kind {
                        CurveKind::Merge => {
                            mark(&mut self.rows_paint, row, column, *to_column);
                            for at in (row + 1)..=(*on_row).min(last) {
                                mark(&mut self.rows_paint, at, *to_column, *to_column);
                            }
                        }
                        CurveKind::Checkout => {
                            for at in row..(*on_row).min(last) {
                                mark(&mut self.rows_paint, at, column, column);
                            }
                            mark(&mut self.rows_paint, *on_row, column, *to_column);
                        }
                    }
                    column = *to_column;
                    row = *on_row;
                }
            }
        }
    }

    /// Remembers the decorations of a commit, so the label column can be sized
    /// from the longest of them rather than from a guess.
    fn note_label_names(&mut self, ref_names: &[SharedString]) {
        /// A history with more labels than this is not read by its labels.
        const MAX_TRACKED: usize = 256;

        for name in ref_names {
            if self.label_names.len() >= MAX_TRACKED {
                return;
            }
            if !self.label_names.iter().any(|known| known == name) {
                self.label_names.push(name.clone());
            }
        }
    }

    /// What the given row paints of the lanes crossing it.
    pub(crate) fn lanes_at(&self, row: usize) -> &[LanePaint] {
        self.rows_paint
            .get(row)
            .map(|at| at.as_slice())
            .unwrap_or(&[])
    }

    /// Every row a branch tip reaches by following parents.
    ///
    /// Answered by one forward pass, because a parent always sits on a later
    /// row than its child in the layout these rows come from: a row that has
    /// been reached passes the mark on to its parents, and rows are visited in
    /// order. `budget` stops a history too large to answer for; the caller is
    /// expected to show nothing rather than a wrong answer.
    pub(crate) fn branch_of(&self, tip_row: usize, budget: usize) -> Option<HashSet<usize>> {
        if tip_row >= self.commits.len() {
            return None;
        }

        let mut reached: HashSet<usize> = HashSet::default();
        reached.insert(tip_row);
        let mut pending = 1usize;

        for row in tip_row..self.commits.len() {
            if pending == 0 {
                break;
            }
            if !reached.contains(&row) {
                continue;
            }
            pending -= 1;

            let commit = self.commits.get(row)?;
            for parent in commit.data.parents.iter() {
                let Some(parent_row) = self.row_of_commit.get(parent).copied() else {
                    // The parent has not been streamed in yet, so this is all
                    // of the branch that can be answered for.
                    continue;
                };
                if reached.insert(parent_row) {
                    if reached.len() > budget {
                        return None;
                    }
                    pending += 1;
                }
            }
        }

        Some(reached)
    }

    /// The nearest branch tip that has the given row in it, walking towards the
    /// children -- the answer to "which branch is this commit on" for a commit
    /// that carries no label of its own.
    pub(crate) fn nearest_tip(&self, row: usize, budget: usize) -> Option<usize> {
        if row >= self.commits.len() {
            return None;
        }

        let mut seen: HashSet<usize> = HashSet::default();
        let mut front = vec![row];
        seen.insert(row);

        for _ in 0..budget {
            let mut next = Vec::new();
            for at in front.drain(..) {
                let commit = self.commits.get(at)?;
                if at != row && !commit.data.ref_names.is_empty() {
                    return Some(at);
                }
                let Some(children) = self.rows_naming_parent.get(&commit.data.sha) else {
                    continue;
                };
                for child in children.iter() {
                    if seen.insert(*child) {
                        next.push(*child);
                    }
                }
            }
            if next.is_empty() {
                return None;
            }
            // Nearest first: the rows closest to this one are the ones a reader
            // would call the branch it is on.
            next.sort_unstable();
            front = next;
        }

        None
    }

    /// The rows holding the branch a merge brought in: everything the merge's
    /// other parents reach that its first parent does not.
    ///
    /// This is the question `git merge-base` answers, and it is answered the
    /// same way -- two fronts walked until they meet -- but in one forward pass
    /// rather than with a heap, because a parent always sits on a later row
    /// than its child in the layout these rows come from. Each row carries which
    /// fronts have reached it; a row both have reached is where the branch
    /// rejoins, and the walk stops descending through it. What only the second
    /// front reached is the branch.
    ///
    /// `None` means there is nothing to fold, or that the answer was not worth
    /// the wait or cannot be trusted: the merge brought in nothing the history
    /// did not already hold, more than `budget` rows belong to the branch, or a
    /// parent has not been streamed in yet. Refusing is the point -- hiding the wrong commits in a
    /// tool for reading history is worse than not hiding any.
    pub(crate) fn side_branch_of(&self, merge_row: usize, budget: usize) -> Option<HashSet<usize>> {
        const FROM_FIRST_PARENT: u8 = 1;
        const FROM_THE_REST: u8 = 2;
        const FROM_BOTH: u8 = FROM_FIRST_PARENT | FROM_THE_REST;

        let merge = self.commits.get(merge_row)?;
        if merge.data.parents.len() < 2 {
            return None;
        }

        let mut reached: HashMap<usize, u8> = HashMap::default();
        let mut pending = 0usize;
        let mark = |reached: &mut HashMap<usize, u8>, pending: &mut usize, row, front| {
            let seen = reached.entry(row).or_insert(0);
            if *seen & front == front {
                return;
            }
            if *seen == 0 {
                *pending += 1;
            }
            *seen |= front;
        };

        for (order, parent) in merge.data.parents.iter().enumerate() {
            let parent_row = *self.row_of_commit.get(parent)?;
            let front = match order {
                0 => FROM_FIRST_PARENT,
                _ => FROM_THE_REST,
            };
            mark(&mut reached, &mut pending, parent_row, front);
        }

        let mut branch_so_far = 0usize;
        let mut row = merge_row + 1;
        while pending > 0 && row < self.commits.len() {
            let Some(front) = reached.get(&row).copied() else {
                row += 1;
                continue;
            };
            pending -= 1;

            // Every row that sets a flag on this one sits earlier, so by the
            // time it is reached its fronts are settled and it can be counted
            // here -- which is what lets the budget stop the walk rather than
            // judge it after it has already read the whole branch.
            if front == FROM_THE_REST {
                branch_so_far += 1;
                if branch_so_far > budget {
                    return None;
                }
            }

            // Where the fronts meet is where the branch rejoined what it left;
            // nothing older than that belongs to the branch.
            if front != FROM_BOTH {
                for parent in self.commits[row].data.parents.iter() {
                    let Some(parent_row) = self.row_of_commit.get(parent).copied() else {
                        return None;
                    };
                    if parent_row > row {
                        mark(&mut reached, &mut pending, parent_row, front);
                    }
                }
            }
            row += 1;
        }

        let branch: HashSet<usize> = reached
            .into_iter()
            .filter(|(_, front)| *front == FROM_THE_REST)
            .map(|(row, _)| row)
            .collect();
        // A merge of something already in the history brings in no branch, and
        // an empty fold is not one: it would leave a mark saying nothing was
        // put away and a control offering to bring it back.
        match branch.is_empty() {
            true => None,
            false => Some(branch),
        }
    }

    /// The rows to leave lit while `row` is being looked at: the row itself,
    /// the rows its parents were drawn on, and the rows naming it as a parent.
    ///
    /// One step in each direction, deliberately, rather than the whole ancestry.
    /// A commit near the head descends from nearly everything, so lighting its
    /// full ancestry lights the whole history and tells the reader nothing. One
    /// step answers what is actually being asked -- what did this come from, and
    /// what came of it -- and stays that size however long the history grows.
    pub(crate) fn kin_of(&self, row: usize) -> HashSet<usize> {
        let mut kin = HashSet::default();
        let Some(commit) = self.commits.get(row) else {
            return kin;
        };
        kin.insert(row);
        for parent in commit.data.parents.iter() {
            if let Some(parent_row) = self.row_of_commit.get(parent) {
                kin.insert(*parent_row);
            }
        }
        if let Some(children) = self.rows_naming_parent.get(&commit.data.sha) {
            kin.extend(children.iter().copied());
        }
        kin
    }

    pub(crate) fn clear(&mut self) {
        self.lane_states.clear();
        self.lane_colors.clear();
        self.parent_to_lanes.clear();
        self.commits.clear();
        self.lines.clear();
        self.active_commit_lines.clear();
        self.active_commit_lines_by_parent.clear();
        self.row_of_commit.clear();
        self.rows_naming_parent.clear();
        self.rows_paint.clear();
        self.label_names.clear();
        self.next_color = BranchColor(0);
        self.max_commit_count = AllCommitCount::NotLoaded;
        self.max_lanes = 0;
    }

    fn first_empty_lane_idx(&mut self) -> ActiveLaneIdx {
        self.lane_states
            .iter()
            .position(LaneState::is_empty)
            .unwrap_or_else(|| {
                self.lane_states.push(LaneState::Empty);
                self.lane_states.len() - 1
            })
    }

    /// The colour a lane is drawn in, choosing one no live lane is already
    /// using when the lane is opening.
    ///
    /// Colour belongs to a branch, not to the slot the branch happens to sit
    /// in: a slot is reused by whatever comes next, and a reader who has
    /// learned that orange means one branch should not find an unrelated one
    /// wearing it further down the same column.
    fn get_lane_color(&mut self, lane_idx: ActiveLaneIdx) -> BranchColor {
        if let Some(color) = self.lane_colors.get(&lane_idx) {
            return *color;
        }

        let count = self.accent_colors_count.max(1) as u8;
        let in_use: HashSet<u8> = self.lane_colors.values().map(|color| color.0).collect();
        let mut candidate = self.next_color.0 % count;
        for _ in 0..count {
            if !in_use.contains(&candidate) {
                break;
            }
            candidate = (candidate + 1) % count;
        }

        self.next_color = BranchColor((candidate + 1) % count);
        self.lane_colors.insert(lane_idx, BranchColor(candidate));
        BranchColor(candidate)
    }

    pub(crate) fn add_commits(&mut self, commits: &[Arc<InitialGraphCommitData>]) {
        self.commits.reserve(commits.len());
        self.lines.reserve(commits.len() / 2);

        for commit in commits.iter() {
            let commit_row = self.commits.len();
            // Before anything else: a line closes on its parent's row, which is
            // this one, and it has nowhere to be recorded until the slot exists.
            self.rows_paint.push(SmallVec::new());
            self.note_label_names(&commit.ref_names);

            self.row_of_commit.insert(commit.sha, commit_row);
            for parent in commit.parents.iter() {
                self.rows_naming_parent
                    .entry(*parent)
                    .or_default()
                    .push(commit_row);
            }

            let commit_lane = self
                .parent_to_lanes
                .get(&commit.sha)
                .and_then(|lanes| lanes.iter().min().copied());

            let commit_lane = commit_lane.unwrap_or_else(|| self.first_empty_lane_idx());

            let commit_color = self.get_lane_color(commit_lane);

            if let Some(lanes) = self.parent_to_lanes.remove(&commit.sha) {
                for lane_column in lanes {
                    let state = &mut self.lane_states[lane_column];

                    if let LaneState::Active {
                        starting_row,
                        segments,
                        ..
                    } = state
                    {
                        if let Some(CommitLineSegment::Curve {
                            to_column,
                            curve_kind: CurveKind::Merge,
                            ..
                        }) = segments.first_mut()
                        {
                            let curve_row = *starting_row + 1;
                            let would_overlap =
                                if lane_column != commit_lane && curve_row < commit_row {
                                    self.commits[curve_row..commit_row]
                                        .iter()
                                        .any(|c| c.lane == commit_lane)
                                } else {
                                    false
                                };

                            if would_overlap {
                                *to_column = lane_column;
                            }
                        }
                    }

                    if let Some(commit_line) =
                        state.to_commit_lines(commit_row, lane_column, commit_lane, commit_color)
                    {
                        self.index_line_for_rows(&commit_line);
                        self.lines.push(Rc::new(commit_line));
                    }

                    // The lane is empty again. Its colour goes back to the pool
                    // so the next branch to take the slot gets one of its own.
                    // The commit's own lane is not free: the history continues
                    // down it in the same colour.
                    if lane_column != commit_lane {
                        self.lane_colors.remove(&lane_column);
                    }
                }
            }

            commit
                .parents
                .iter()
                .enumerate()
                .for_each(|(parent_idx, parent)| {
                    if parent_idx == 0 {
                        self.lane_states[commit_lane] = LaneState::Active {
                            parent: *parent,
                            child: commit.sha,
                            color: Some(commit_color),
                            starting_col: commit_lane,
                            starting_row: commit_row,
                            destination_column: None,
                            segments: smallvec![CommitLineSegment::Straight { to_row: usize::MAX }],
                        };

                        self.parent_to_lanes
                            .entry(*parent)
                            .or_default()
                            .push(commit_lane);
                    } else {
                        let new_lane = self.first_empty_lane_idx();

                        self.lane_states[new_lane] = LaneState::Active {
                            parent: *parent,
                            child: commit.sha,
                            color: None,
                            starting_col: commit_lane,
                            starting_row: commit_row,
                            destination_column: None,
                            segments: smallvec![CommitLineSegment::Curve {
                                to_column: usize::MAX,
                                on_row: usize::MAX,
                                curve_kind: CurveKind::Merge,
                            },],
                        };

                        self.parent_to_lanes
                            .entry(*parent)
                            .or_default()
                            .push(new_lane);
                    }
                });

            self.max_lanes = self.max_lanes.max(self.lane_states.len());

            self.commits.push(Rc::new(CommitEntry {
                data: commit.clone(),
                lane: commit_lane,
                color_idx: commit_color.0 as usize,
            }));
        }

        self.max_commit_count = AllCommitCount::Loading(self.commits.len());
    }
}

pub fn init(cx: &mut App) {
    workspace::register_serializable_item::<GitGraph>(cx);

    cx.observe_new(|workspace: &mut workspace::Workspace, _, _| {
        workspace.register_action_renderer(|div, workspace, window, cx| {
            div.when_some(
                resolve_file_history_target(workspace, window, cx),
                |div, (repo_id, log_source)| {
                    let git_store = workspace.project().read(cx).git_store().clone();
                    let workspace = workspace.weak_handle();

                    div.on_action(move |_: &git::FileHistory, window, cx| {
                        let git_store = git_store.clone();
                        workspace
                            .update(cx, |workspace, cx| {
                                open_or_reuse_graph(
                                    workspace,
                                    repo_id,
                                    git_store,
                                    log_source.clone(),
                                    None,
                                    window,
                                    cx,
                                );
                            })
                            .ok();
                    })
                },
            )
            .when(
                workspace.project().read(cx).active_repository(cx).is_some(),
                |div| {
                    let workspace = workspace.weak_handle();

                    div.on_action({
                        let workspace = workspace.clone();
                        move |_: &Open, window, cx| {
                            workspace
                                .update(cx, |workspace, cx| {
                                    let Some(repo) =
                                        workspace.project().read(cx).active_repository(cx)
                                    else {
                                        return;
                                    };
                                    let selected_repo_id = repo.read(cx).id;

                                    let git_store =
                                        workspace.project().read(cx).git_store().clone();
                                    open_or_reuse_graph(
                                        workspace,
                                        selected_repo_id,
                                        git_store,
                                        LogSource::All,
                                        None,
                                        window,
                                        cx,
                                    );
                                })
                                .ok();
                        }
                    })
                    .on_action(move |action: &OpenAtCommit, window, cx| {
                        let sha = action.sha.clone();
                        workspace
                            .update(cx, |workspace, cx| {
                                let Some(repo) = workspace.project().read(cx).active_repository(cx)
                                else {
                                    return;
                                };
                                let selected_repo_id = repo.read(cx).id;

                                let git_store = workspace.project().read(cx).git_store().clone();
                                open_or_reuse_graph(
                                    workspace,
                                    selected_repo_id,
                                    git_store,
                                    LogSource::All,
                                    Some(sha),
                                    window,
                                    cx,
                                );
                            })
                            .ok();
                    })
                },
            )
        });
    })
    .detach();
}

/// Resolves a `git::FileHistory` target from a known project path (used by
/// callers like `project_panel` that own a focused selection but cannot be
/// referenced from this module due to dependency direction).
pub fn resolve_file_history_target_from_project_path(
    workspace: &Workspace,
    project_path: &ProjectPath,
    cx: &App,
) -> Option<(RepositoryId, LogSource)> {
    let git_store = workspace.project().read(cx).git_store();
    let (repo, repo_path) = git_store
        .read(cx)
        .repository_and_path_for_project_path(project_path, cx)?;
    let log_source = if repo_path.is_empty() {
        LogSource::All
    } else {
        LogSource::Path(repo_path)
    };
    Some((repo.read(cx).id, log_source))
}

fn resolve_file_history_target(
    workspace: &Workspace,
    window: &Window,
    cx: &App,
) -> Option<(RepositoryId, LogSource)> {
    if let Some(panel) = workspace.panel::<crate::git_panel::GitPanel>(cx)
        && panel.read(cx).focus_handle(cx).contains_focused(window, cx)
        && let Some((repository, repo_path)) = panel.read(cx).selected_file_history_target()
    {
        return Some((repository.read(cx).id, LogSource::Path(repo_path)));
    }

    let editor = workspace.active_item_as::<Editor>(cx)?;

    let file = editor
        .read(cx)
        .file_at(editor.read(cx).selections.newest_anchor().head(), cx)?;
    let project_path = ProjectPath {
        worktree_id: file.worktree_id(cx),
        path: file.path().clone(),
    };

    let git_store = workspace.project().read(cx).git_store();
    let (repo, repo_path) = git_store
        .read(cx)
        .repository_and_path_for_project_path(&project_path, cx)?;
    Some((repo.read(cx).id, LogSource::Path(repo_path)))
}

pub fn open_or_reuse_graph(
    workspace: &mut Workspace,
    repo_id: RepositoryId,
    git_store: Entity<GitStore>,
    log_source: LogSource,
    sha: Option<String>,
    window: &mut Window,
    cx: &mut Context<Workspace>,
) {
    let existing = workspace.items_of_type::<GitGraph>(cx).find(|graph| {
        let graph = graph.read(cx);
        graph.repo_id == repo_id && graph.log_source == log_source
    });

    let git_graph = if let Some(existing) = existing {
        workspace.activate_item(&existing, true, true, window, cx);
        existing
    } else {
        let workspace_handle = workspace.weak_handle();
        let git_graph = cx.new(|cx| {
            GitGraph::new(
                repo_id,
                git_store,
                workspace_handle,
                Some(log_source),
                window,
                cx,
            )
        });
        workspace.add_item_to_active_pane(Box::new(git_graph.clone()), None, true, window, cx);
        git_graph
    };

    if let Some(sha) = sha {
        cx.defer(move |cx| {
            git_graph.update(cx, |graph, cx| {
                graph.select_commit_by_sha(sha.as_str(), cx);
            });
        });
    }
}

/// Everything the lane column needs to paint one frame of itself. A host
/// gathers this from wherever it keeps its own scrolling and selection, so the
/// column knows nothing about tables, panels or docks and can be hosted by any
/// of them.
pub(crate) struct GraphColumn {
    pub row_height: Pixels,
    pub first_visible_row: usize,
    pub visible_row_count: usize,
    /// How far the first visible row has already scrolled above the top edge.
    pub vertical_scroll_offset: Pixels,
    /// How far the lanes are shifted left in a column too narrow to hold them
    /// all. Zero wherever the column gets the width its lanes ask for.
    pub horizontal_scroll_offset: Pixels,
    pub width: Pixels,
    /// Set only by a host that does not paint its own rows. A host built from
    /// real row elements paints its backgrounds itself and leaves this `None`,
    /// or the column lays a second highlight over the first.
    pub highlight: Option<GraphRowHighlight>,
    /// Where the column landed, for a host that hit-tests it by hand.
    pub painted_at: Option<Rc<Cell<Option<Bounds<Pixels>>>>>,
    /// The rows joined to the one being looked at. Their lines are drawn heavier;
    /// nothing else changes. Fading everything else was tried and was wrong: in a
    /// list of thirty rows a highlight of three dims twenty-seven, and a pointer
    /// crossing the list makes the whole panel flash.
    pub lit: Option<Rc<HashSet<usize>>>,
}

/// Which rows the column paints a background behind.
pub(crate) struct GraphRowHighlight {
    pub hovered: Option<usize>,
    pub selected: Option<usize>,
    pub context_menu_target: Option<usize>,
    pub focused: bool,
}

/// Paints the lanes, the dots and the lines joining them for the rows now in
/// view. Rows are addressed by their index in `data`, which is also the row
/// index of whatever list the host paints beside this column -- the two stay
/// aligned only while both are laid out on the same row height.
pub(crate) fn render_graph_column(data: &GraphData, column: GraphColumn) -> impl IntoElement {
    let GraphColumn {
        row_height,
        first_visible_row,
        visible_row_count,
        vertical_scroll_offset,
        horizontal_scroll_offset,
        width,
        highlight,
        painted_at,
        lit,
    } = column;

    let loaded_commit_count = data.commits.len();
    let last_visible_row = first_visible_row + visible_row_count + 1;
    let viewport_range = first_visible_row.min(loaded_commit_count.saturating_sub(1))
        ..last_visible_row.min(loaded_commit_count);

    // Only what the viewport shows is copied out, and each row brings the lanes
    // crossing it with it. This used to filter every line in the history on
    // every frame, which on a hundred thousand commits cost milliseconds a
    // scroll; the rows now carry the answer.
    let rows: Vec<(Rc<CommitEntry>, SmallVec<[LanePaint; 4]>)> = viewport_range
        .filter_map(|row| {
            let commit = data.commits.get(row)?.clone();
            Some((commit, SmallVec::from_slice(data.lanes_at(row))))
        })
        .collect();

    let hovered_entry_idx = highlight.as_ref().and_then(|rows| rows.hovered);
    let selected_entry_idx = highlight.as_ref().and_then(|rows| rows.selected);
    let context_menu_target_index = highlight.as_ref().and_then(|rows| rows.context_menu_target);
    let is_focused = highlight.as_ref().is_some_and(|rows| rows.focused);
    let graph_canvas_bounds = painted_at;
    let max_lanes = data.max_lanes.max(1);

    gpui::canvas(
        move |_bounds, _window, _cx| {},
        move |bounds: Bounds<Pixels>, _: (), window: &mut Window, cx: &mut App| {
            if let Some(painted_at) = &graph_canvas_bounds {
                painted_at.set(Some(bounds));
            }

            let metrics = GraphMetrics::for_window(window);
            window.paint_layer(bounds, |window| {
                let accents = cx.theme().accents();
                let hover_bg = cx.theme().colors().element_hover.opacity(0.6);
                let selected_bg = match is_focused {
                    true => cx.theme().colors().element_selected,
                    false => cx.theme().colors().element_hover,
                };

                for (visible_row_idx, (commit, lanes)) in rows.iter().enumerate() {
                    let row = first_visible_row + visible_row_idx;
                    let row_y = bounds.origin.y + visible_row_idx as f32 * row_height
                        - vertical_scroll_offset;
                    let row_bounds = Bounds::new(
                        point(bounds.origin.x - horizontal_scroll_offset, row_y),
                        gpui::Size {
                            width: bounds.size.width,
                            height: row_height,
                        },
                    );

                    let is_hovered = hovered_entry_idx == Some(row);
                    let is_selected = selected_entry_idx == Some(row);
                    let is_context_menu_target = context_menu_target_index == Some(row);
                    if is_hovered || is_selected || is_context_menu_target {
                        let behind = Bounds::new(
                            point(bounds.origin.x, row_y),
                            gpui::Size {
                                width: bounds.size.width,
                                height: row_height,
                            },
                        );
                        let bg = match is_selected || is_context_menu_target {
                            true => selected_bg,
                            false => hover_bg,
                        };
                        window.paint_quad(gpui::fill(behind, bg));
                    }

                    // The highlight adds weight, it never takes light away: a
                    // commit joined to the one under the pointer is drawn
                    // heavier, and every other row is left as it was.
                    let emphasis = match &lit {
                        Some(lit) if lit.contains(&row) => 2.0,
                        _ => 1.0,
                    };

                    paint_row_lanes(
                        row_bounds,
                        lanes,
                        GraphRowPaint {
                            metrics,
                            first_lane: 0,
                            lane_cap: max_lanes,
                            connector: None,
                            emphasis,
                        },
                        accents,
                        window,
                    );

                    let colour = accents.color_for_index(commit.color_idx as u32);
                    let centre_x = row_bounds.origin.x + metrics.lane_center_in(commit.lane, 0);
                    draw_commit_circle(centre_x, row_y + row_height / 2.0, colour, window);
                }
            })
        },
    )
    .w(width)
    .h_full()
}

/// The sizes the graph is laid out at.
///
/// All of them follow one row height, and that follows the interface font, so
/// a reader who changes the font size moves the whole graph together instead of
/// tearing the dots off their rows. The ratios are taken from the reference the
/// design is matched against: node 0.71 of a row, lane step 0.79, label 0.64.
#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) struct GraphMetrics {
    pub row: Pixels,
    pub node: Pixels,
    pub lane: Pixels,
    pub label: Pixels,
    pub left_pad: Pixels,
}

impl GraphMetrics {
    pub(crate) fn new(line_height: Pixels, scale_factor: f32) -> Self {
        let scale = if scale_factor > 0.0 {
            scale_factor
        } else {
            1.0
        };
        let snap = |value: Pixels| (value * scale).round() / scale;
        // One line of text, plus the room a node needs around it. At the shipped
        // font this is 34, which is the row height the rest of the fork's
        // controls are laid out on.
        let row = snap(line_height + px(13.));
        Self {
            row,
            node: snap(row * 0.71),
            lane: snap(row * 0.79),
            label: snap(row * 0.64),
            left_pad: px(8.),
        }
    }

    pub(crate) fn for_window(window: &Window) -> Self {
        let line_height = window.text_style().line_height_in_pixels(window.rem_size());
        Self::new(line_height, window.scale_factor())
    }

    /// Where a lane's line runs, measured from the left edge of a graph cell
    /// showing lanes from `first` onwards.
    pub(crate) fn lane_center_in(self, column: usize, first: usize) -> Pixels {
        self.left_pad + self.lane * (column as f32 - first as f32) + self.lane / 2.0
    }

    /// How wide the graph column has to be to show `lanes` lanes in full.
    pub(crate) fn width_for(self, lanes: usize) -> Pixels {
        self.left_pad * 2.0 + self.lane * lanes.max(1) as f32
    }

    /// Whether a node of this size can carry two letters legibly.
    pub(crate) fn node_holds_initials(self) -> bool {
        self.node >= px(16.)
    }
}

/// Ink that can be read on `background`.
///
/// Lane colours come from the active theme and span the whole range of
/// lightness, so neither black nor white reads on all of them and the choice
/// has to be made per colour.
pub(crate) fn readable_on(background: Hsla) -> Hsla {
    match background.l > 0.55 {
        true => hsla(0., 0., 0.08, 1.),
        false => hsla(0., 0., 1., 1.),
    }
}

/// Up to two letters standing in for an author until a picture arrives, and
/// for good when none ever does.
pub(crate) fn initials_of(author: &str) -> SharedString {
    let mut letters = author
        .split_whitespace()
        .filter_map(|word| word.chars().find(|c| c.is_alphanumeric()))
        .map(|c| c.to_uppercase().to_string());
    match (letters.next(), letters.next()) {
        (Some(first), Some(second)) => SharedString::from(format!("{first}{second}")),
        (Some(first), None) => SharedString::from(first),
        _ => SharedString::from("?"),
    }
}

/// What a decoration in git's `%D` names.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) enum RefKind {
    /// The branch `HEAD` is on, or a detached `HEAD` itself.
    Head,
    Tag,
    Branch,
    Remote,
}

impl RefKind {
    pub(crate) fn icon(self) -> IconName {
        match self {
            RefKind::Head => IconName::Check,
            RefKind::Tag => IconName::Bookmark,
            RefKind::Branch => IconName::GitBranch,
            RefKind::Remote => IconName::Server,
        }
    }
}

/// Reads one decoration as what it is and what to call it.
///
/// `remote_names` is what tells `origin/main` from a branch that merely has a
/// slash in its name: git spells both the same way, so without the repository's
/// remotes there is nothing in the text to go on.
pub(crate) fn read_ref(
    decoration: &str,
    head_branch_name: Option<&str>,
    remote_names: &[SharedString],
) -> Option<(RefKind, SharedString)> {
    if let Some(tag) = decoration.strip_prefix("tag: ") {
        return (!tag.is_empty()).then(|| (RefKind::Tag, SharedString::from(tag.to_string())));
    }

    let name = decoration.strip_prefix("HEAD -> ").unwrap_or(decoration);
    if name.is_empty() {
        return None;
    }
    if name == "HEAD" {
        return Some((RefKind::Head, SharedString::from("HEAD")));
    }

    let kind = if head_branch_name == Some(name) {
        RefKind::Head
    } else if name.split_once('/').is_some_and(|(remote, rest)| {
        !rest.is_empty() && remote_names.iter().any(|known| known.as_ref() == remote)
    }) {
        RefKind::Remote
    } else {
        RefKind::Branch
    };

    Some((kind, SharedString::from(name.to_string())))
}

/// How much of a branch label survives at a given width.
///
/// The tail of a name is what tells two branches in one namespace apart; the
/// prefix almost never does. So the prefix is what goes first, a segment at a
/// time, and only when there is nothing left to squeeze is the tail cut.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum LabelMode {
    /// Full names, every icon, and a count for the rest.
    Full,
    /// Path segments squeezed to their first letter: `fix/10-async` reads
    /// `f/10-async`. One icon.
    Abbreviated,
    /// One label a row and a count, showing the tail of the name.
    Single,
    /// No column at all: a chip in front of the subject, an icon and a short
    /// tail, with the whole name in its tooltip.
    Inline,
}

impl LabelMode {
    /// Whether labels get a column of their own.
    pub(crate) fn has_a_column(self) -> bool {
        !matches!(self, LabelMode::Inline)
    }

    /// How many labels a row shows before the rest become a count.
    pub(crate) fn chips_a_row(self) -> usize {
        match self {
            LabelMode::Full => 2,
            _ => 1,
        }
    }
}

/// Where the age of a commit is shown.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum AgeShown {
    /// A column of its own, on every row.
    Column,
    /// Only where the reader is already looking, and on anything young enough
    /// that "when" is the question being asked.
    WhereItMatters,
    Nowhere,
}

/// What a row shows at the width it was given.
///
/// This is the order of concessions written down: what gives up its room, and
/// in which order, when there is not enough. The graph and the subject never
/// do -- the lanes are the reason to open a history at all, and a subject with
/// no dots beside it reads where dots with no subject do not.
#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) struct HistoryLayout {
    pub age: AgeShown,
    pub labels: LabelMode,
    /// How many lanes the graph column shows before the rest become rings at
    /// its edge.
    pub lane_cap: usize,
    /// What the subject keeps whatever else is asked for.
    pub subject_min: Pixels,
    /// What the label column ends up with, already clamped.
    pub label_width: Pixels,
}

/// One rung of the ladder, richest first.
struct Rung {
    age: AgeShown,
    labels: LabelMode,
    lane_cap: usize,
    subject_min_chars: f32,
}

/// The thresholds are not numbers picked off a mock-up: each rung states its
/// own minimums, and the width at which it stops fitting is worked out from
/// them on every frame.
const RUNGS: [Rung; 4] = [
    Rung {
        age: AgeShown::Column,
        labels: LabelMode::Full,
        lane_cap: 8,
        subject_min_chars: 24.,
    },
    Rung {
        age: AgeShown::WhereItMatters,
        labels: LabelMode::Abbreviated,
        lane_cap: 6,
        subject_min_chars: 24.,
    },
    Rung {
        age: AgeShown::Nowhere,
        labels: LabelMode::Single,
        lane_cap: 4,
        subject_min_chars: 16.,
    },
    Rung {
        age: AgeShown::Nowhere,
        labels: LabelMode::Inline,
        lane_cap: 3,
        subject_min_chars: 12.,
    },
];

/// What the age column takes when it has one.
pub(crate) const AGE_COLUMN_WIDTH: Pixels = px(90.);
/// A label column narrower than this says nothing, so it is not offered.
pub(crate) const LABEL_COLUMN_MIN: Pixels = px(120.);
/// However long the longest label is, the column stops here.
pub(crate) const LABEL_COLUMN_MAX_SHARE: f32 = 0.28;

/// Everything `fit` needs to know about what it is laying out.
#[derive(Debug, Clone, Copy)]
pub(crate) struct HistoryContents {
    pub metrics: GraphMetrics,
    /// The width of one character of the interface font.
    pub character: Pixels,
    /// How wide the longest label would be drawn in each mode.
    pub widest_label: [Pixels; 4],
    pub lanes: usize,
}

impl HistoryContents {
    fn label_width(&self, mode: LabelMode, available: Pixels) -> Pixels {
        if !mode.has_a_column() {
            return px(0.);
        }
        let wanted = self.widest_label[mode as usize];
        if wanted <= px(0.) {
            return px(0.);
        }
        wanted
            .max(LABEL_COLUMN_MIN)
            .min(available * LABEL_COLUMN_MAX_SHARE)
    }
}

/// Picks the richest row that fits in `available`.
pub(crate) fn fit(available: Pixels, contents: HistoryContents) -> HistoryLayout {
    let mut chosen = RUNGS.len() - 1;
    for (idx, rung) in RUNGS.iter().enumerate() {
        let age = match rung.age {
            AgeShown::Column => AGE_COLUMN_WIDTH,
            _ => px(0.),
        };
        let labels = contents.label_width(rung.labels, available);
        let graph = contents
            .metrics
            .width_for(rung.lane_cap.min(contents.lanes.max(1)));
        let subject = contents.character * rung.subject_min_chars;

        if age + labels + graph + subject <= available {
            chosen = idx;
            break;
        }
    }

    let rung = &RUNGS[chosen];
    HistoryLayout {
        age: rung.age,
        labels: rung.labels,
        lane_cap: rung.lane_cap,
        subject_min: contents.character * rung.subject_min_chars,
        label_width: contents.label_width(rung.labels, available),
    }
}

/// How wide a string is drawn in the interface font.
///
/// The label column is sized from its longest label rather than from a share of
/// the row, so that it neither clips a name nor leaves a hand's width of empty
/// column beside a history whose branches are all called `main`.
pub(crate) fn measure_text(window: &Window, text: &str) -> Pixels {
    if text.is_empty() {
        return px(0.);
    }
    let style = window.text_style();
    let font_size = style.font_size.to_pixels(window.rem_size());
    let run = gpui::TextRun {
        len: text.len(),
        font: style.font(),
        color: style.color,
        background_color: None,
        underline: None,
        strikethrough: None,
    };
    window
        .text_system()
        .layout_line(text, font_size, &[run], None)
        .width
}

/// What a chip adds around the name inside it: its icon, the gaps and its own
/// border.
pub(crate) fn chip_chrome(metrics: GraphMetrics, _kind: RefKind) -> Pixels {
    metrics.label + px(18.)
}

/// What a reader has asked the history to leave out.
///
/// Hiding a branch takes its rows out of the list rather than dimming them: a
/// reader filtering a history of a hundred thousand commits wants the scrolling
/// to get shorter, not the same scrolling with less to read.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct HistoryFilter {
    /// Branch tips whose rows are left out.
    pub hidden: Vec<SharedString>,
    /// One branch, and only it.
    pub solo: Option<SharedString>,
    /// Remote-tracking refs are not labelled. Their commits stay: a remote ref
    /// and a local branch of the same name usually name the same commit, and a
    /// reader hiding the remote one does not mean to lose the local one.
    pub hide_remotes: bool,
    /// The same for tags.
    pub hide_tags: bool,
}

impl HistoryFilter {
    pub(crate) fn is_on(&self) -> bool {
        !self.hidden.is_empty() || self.solo.is_some() || self.hide_remotes || self.hide_tags
    }

    /// Whether a label of this kind is drawn at all.
    pub(crate) fn shows(&self, kind: RefKind) -> bool {
        match kind {
            RefKind::Remote => !self.hide_remotes,
            RefKind::Tag => !self.hide_tags,
            _ => true,
        }
    }

    /// Whether the rows of a branch are left in.
    pub(crate) fn keeps(&self, name: &SharedString) -> bool {
        match &self.solo {
            Some(only) => only == name,
            None => !self.hidden.contains(name),
        }
    }
}

/// Which columns the table draws. `true` means hidden, which is what the
/// table's filter reads.
pub(crate) fn column_mask(layout: HistoryLayout) -> TableRow<bool> {
    TableRow::from_vec(
        vec![
            !layout.labels.has_a_column(),
            false,
            false,
            // A column only when every row has an age to put in it. Where the
            // age is shown on a row or two the age rides at the end of the
            // subject instead, because a column that is empty on nineteen rows
            // out of twenty is a column of nothing.
            !matches!(layout.age, AgeShown::Column),
        ],
        TABLE_COLUMN_COUNT,
    )
}

/// What a node needs to look for a picture of its author.
#[derive(Debug, Clone)]
pub(crate) struct CommitPortrait {
    pub sha: SharedString,
    pub author_email: Option<SharedString>,
    pub remote: Option<GitRemote>,
}

/// Where a node's circle starts, inside the column showing `lane_cap` lanes
/// from `first_lane`.
pub(crate) fn node_left(
    metrics: GraphMetrics,
    place: NodePlace,
    first_lane: usize,
    lane_cap: usize,
) -> Pixels {
    let column = match place {
        NodePlace::InLane(lane) => lane,
        NodePlace::BeforeTheEdge => first_lane,
        NodePlace::PastTheEdge => first_lane + lane_cap,
    };
    metrics.lane_center_in(column, first_lane) - metrics.node / 2.0
}

/// Whether this row shows its age.
///
/// A date on every row is noise in a list a reader scans by subject. Where
/// there is no room for a column of them it earns its place on the row the
/// reader picked, the row under the pointer, and on anything recent enough that
/// "when" is the question being asked.
pub(crate) fn age_is_shown(
    age: AgeShown,
    is_selected: bool,
    is_hovered: bool,
    is_fresh: bool,
) -> bool {
    match age {
        AgeShown::Column => true,
        AgeShown::WhereItMatters => is_selected || is_hovered || is_fresh,
        AgeShown::Nowhere => false,
    }
}

/// Whether a commit is recent enough that a reader is asking when rather than
/// which.
pub(crate) fn is_younger_than_a_day(timestamp: i64, now: OffsetDateTime) -> bool {
    const DAY: i64 = 24 * 60 * 60;
    let Ok(then) = OffsetDateTime::from_unix_timestamp(timestamp) else {
        return false;
    };
    (now - then).whole_seconds() < DAY
}

/// A branch name shortened by the rule rather than by the character count.
pub(crate) fn shorten_ref(name: &str, mode: LabelMode) -> SharedString {
    /// What an inline chip can hold of a name before its tooltip has to.
    const INLINE_TAIL: usize = 12;

    match mode {
        LabelMode::Full => SharedString::from(name.to_string()),
        LabelMode::Abbreviated => {
            let Some(cut) = name.rfind('/') else {
                return SharedString::from(name.to_string());
            };
            let mut shortened = String::with_capacity(name.len());
            for segment in name[..cut].split('/') {
                match segment.chars().next() {
                    Some(first) => shortened.push(first),
                    None => {}
                }
                shortened.push('/');
            }
            shortened.push_str(&name[cut + 1..]);
            SharedString::from(shortened)
        }
        LabelMode::Single => {
            let tail = name.rsplit('/').next().unwrap_or(name);
            SharedString::from(tail.to_string())
        }
        LabelMode::Inline => {
            let tail = name.rsplit('/').next().unwrap_or(name);
            if tail.chars().count() <= INLINE_TAIL {
                return SharedString::from(tail.to_string());
            }
            let kept: String = tail.chars().take(INLINE_TAIL - 1).collect();
            SharedString::from(format!("{kept}…"))
        }
    }
}

/// An age rather than a date: a reader scanning a history asks "how recent"
/// far more often than "which calendar day", and the answer fits in a column
/// four characters wide.
fn format_relative_timestamp(timestamp: i64, now: OffsetDateTime) -> String {
    const MINUTE: i64 = 60;
    const HOUR: i64 = 60 * MINUTE;
    const DAY: i64 = 24 * HOUR;
    const WEEK: i64 = 7 * DAY;
    const MONTH: i64 = 30 * DAY;
    const YEAR: i64 = 365 * DAY;

    let Ok(then) = OffsetDateTime::from_unix_timestamp(timestamp) else {
        return "—".to_string();
    };

    match (now - then).whole_seconds() {
        seconds if seconds < MINUTE => "now".to_string(),
        seconds if seconds < HOUR => format!("{}m", seconds / MINUTE),
        seconds if seconds < DAY => format!("{}h", seconds / HOUR),
        seconds if seconds < WEEK => format!("{}d", seconds / DAY),
        seconds if seconds < MONTH => format!("{}w", seconds / WEEK),
        seconds if seconds < YEAR => format!("{}mo", seconds / MONTH),
        seconds => format!("{}y", seconds / YEAR),
    }
}

/// Paints the lanes crossing one row, and the rule joining a labelled commit to
/// its ref chips.
///
/// Everything is measured from `bounds`, so the row cannot drift away from the
/// list it belongs to however the list is scrolled: there is no second scroll
/// offset to keep in step.
fn paint_row_lanes(
    bounds: Bounds<Pixels>,
    lanes: &[LanePaint],
    row: GraphRowPaint,
    accents: &AccentColors,
    window: &mut Window,
) {
    let GraphRowPaint {
        metrics,
        first_lane,
        lane_cap,
        connector,
        emphasis,
    } = row;
    let lane_center = |column: usize| metrics.lane_center_in(column, first_lane);
    let in_view = |column: usize| column >= first_lane && column < first_lane + lane_cap;
    let top = bounds.origin.y;
    let bottom = top + bounds.size.height;
    let center = top + bounds.size.height / 2.0;
    let node_gap = metrics.node / 2.0;
    let stroke = LINE_WIDTH * emphasis;

    if let Some((lane, color_idx)) = connector {
        let to_x = bounds.origin.x + lane_center(lane.clamp(first_lane, first_lane + lane_cap - 1))
            - node_gap;
        let mut builder = PathBuilder::stroke(px(1.));
        builder.move_to(point(bounds.origin.x, center));
        builder.line_to(point(to_x, center));
        builder.close();
        if let Ok(path) = builder.build() {
            window.paint_path(
                path,
                accents.color_for_index(color_idx as u32).opacity(0.35),
            );
        }
    }

    let mut by_color: BTreeMap<usize, Vec<PathBuilder>> = BTreeMap::new();

    for lane in lanes {
        // A line with both ends outside the window has nothing to draw in it.
        if !in_view(lane.from_column) && !in_view(lane.to_column) {
            continue;
        }
        let from_x = bounds.origin.x + lane_center(lane.from_column);
        let to_x = bounds.origin.x + lane_center(lane.to_column);
        let enters_at = if lane.starts_at_node {
            center + node_gap
        } else {
            top
        };
        let leaves_at = if lane.ends_at_node {
            center - node_gap
        } else {
            bottom
        };

        let mut builder = PathBuilder::stroke(stroke);

        if !lane.bends() {
            if leaves_at <= enters_at {
                continue;
            }
            builder.move_to(point(from_x, enters_at));
            builder.line_to(point(from_x, leaves_at));
        } else {
            let sideways = if to_x > from_x { 1.0 } else { -1.0 };
            let reach = (to_x - from_x).abs();
            let curve_width = (metrics.lane / 3.0).min(reach / 2.0);
            let curve_height = (metrics.row / 3.0).min(bounds.size.height / 2.0);

            // Where the line runs level, between the two quarter-turns.
            let (level_from, level_to) = if lane.starts_at_node {
                (from_x + node_gap * sideways, to_x - curve_width * sideways)
            } else if lane.ends_at_node {
                (from_x + curve_width * sideways, to_x - node_gap * sideways)
            } else {
                (
                    from_x + curve_width * sideways,
                    to_x - curve_width * sideways,
                )
            };
            // Two quarter-turns can ask for more room than the lanes leave
            // between them. Letting the level run go backwards would tear the
            // line in half; turning straight into the next turn keeps it whole.
            let level_to = match (level_to - level_from) * sideways < px(0.) {
                true => level_from,
                false => level_to,
            };

            if !lane.starts_at_node {
                let turn_from = point(from_x, center - curve_height);
                builder.move_to(point(from_x, top));
                builder.line_to(turn_from);
                builder.move_to(turn_from);
                builder.curve_to(point(level_from, center), point(from_x, center));
            }

            if level_to != level_from {
                builder.move_to(point(level_from, center));
                builder.line_to(point(level_to, center));
            }

            if !lane.ends_at_node {
                builder.move_to(point(level_to, center));
                builder.curve_to(point(to_x, center + curve_height), point(to_x, center));
                builder.move_to(point(to_x, center + curve_height));
                builder.line_to(point(to_x, bottom));
            }
        }

        builder.close();
        by_color.entry(lane.color_idx).or_default().push(builder);
    }

    for (color_idx, builders) in by_color {
        let color = accents.color_for_index(color_idx as u32);
        for builder in builders {
            if let Ok(path) = builder.build() {
                // Each colour gets its own layer so that two lines crossing do
                // not blend into a third colour that names no branch.
                window.paint_layer(bounds, |window| {
                    window.paint_path(path, color);
                });
            }
        }
    }
}

/// Where a commit sits relative to the lanes the column is showing.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum NodePlace {
    /// In view, in the lane it belongs to.
    InLane(usize),
    /// Off the left edge: drawn as a ring there, so the reader sees the commit
    /// exists and which branch it is on without seeing its lane.
    BeforeTheEdge,
    /// Off the right edge.
    PastTheEdge,
}

/// What one row of the graph column paints besides its lanes.
#[derive(Debug, Clone, Copy)]
pub(crate) struct GraphRowPaint {
    pub metrics: GraphMetrics,
    /// The leftmost lane the column is showing.
    pub first_lane: usize,
    /// How many lanes it shows before the rest become rings.
    pub lane_cap: usize,
    /// The lane and colour of a node that carries ref chips, which get a rule
    /// joining them to it.
    pub connector: Option<(usize, usize)>,
    /// Multiplier on the line width. Above one for a row the reader is on.
    pub emphasis: f32,
}

/// How wide a column must be to show `lanes` lanes in full.
pub(crate) fn graph_column_width(lanes: usize) -> Pixels {
    LEFT_PADDING * 2.0 + LANE_WIDTH * lanes.max(1) as f32
}

fn draw_commit_circle(center_x: Pixels, center_y: Pixels, color: Hsla, window: &mut Window) {
    let radius = COMMIT_CIRCLE_RADIUS;

    let mut builder = PathBuilder::fill();

    // Start at the rightmost point of the circle
    builder.move_to(point(center_x + radius, center_y));

    // Draw the circle using two arc_to calls (top half, then bottom half)
    builder.arc_to(
        point(radius, radius),
        px(0.),
        false,
        true,
        point(center_x - radius, center_y),
    );
    builder.arc_to(
        point(radius, radius),
        px(0.),
        false,
        true,
        point(center_x + radius, center_y),
    );
    builder.close();

    if let Ok(path) = builder.build() {
        window.paint_path(path, color);
    }
}

fn compute_diff_stats(diff: &CommitDiff) -> (usize, usize) {
    diff.files.iter().fold((0, 0), |(added, removed), file| {
        let old_text = file.old_text.as_deref().unwrap_or("");
        let new_text = file.new_text.as_deref().unwrap_or("");
        let hunks = line_diff(old_text, new_text);
        hunks
            .iter()
            .fold((added, removed), |(a, r), (old_range, new_range)| {
                (
                    a + (new_range.end - new_range.start) as usize,
                    r + (old_range.end - old_range.start) as usize,
                )
            })
    })
}

struct GitGraphContextMenu {
    menu: Entity<ContextMenu>,
    position: Point<Pixels>,
    target_entry_index: Option<usize>,
    _subscription: Subscription,
}

struct DetailPanelCommitMessage {
    sha: Oid,
    message: Entity<Markdown>,
    scroll_handle: ScrollHandle,
}

pub struct GitGraph {
    focus_handle: FocusHandle,
    search_state: SearchState,
    graph_data: GraphData,
    git_store: Entity<GitStore>,
    workspace: WeakEntity<Workspace>,
    context_menu: Option<GitGraphContextMenu>,
    table_interaction_state: Entity<TableInteractionState>,
    /// What the reader has asked the history to leave out.
    filter: HistoryFilter,
    /// The rows the filter leaves, in order, or `None` when it leaves them all.
    /// The list is laid out over this; everything else counts in rows of the
    /// history itself, and the two meet only where the list is addressed.
    kept_rows: Option<Rc<Vec<usize>>>,
    /// Every row the reader has picked with Ctrl or Shift, for the commands
    /// that act on more than one commit. Empty means the selection is the one
    /// row the card is showing.
    picked_rows: Vec<usize>,
    /// The commit the card is comparing the selected one against, picked with
    /// Shift. `None` means the card shows one commit, as it always has.
    compare_against: Option<usize>,
    /// The rows of the branch whose label is under the pointer. Everything
    /// else is dimmed while it is set, which is a deliberate question about one
    /// branch rather than something a pointer crossing the list can trigger.
    lit_branch: Option<Rc<HashSet<usize>>>,
    /// Where the graph's own scrollbar was painted, so a drag along it can be
    /// turned into a lane.
    graph_track: Rc<Cell<Option<Bounds<Pixels>>>>,
    /// The leftmost lane the graph column is showing. A history deeper than the
    /// column is wide is read by pushing this along rather than by squeezing
    /// the lanes until nothing can be told apart.
    graph_first_lane: Rc<Cell<usize>>,
    /// What the reader dragged the label column to, if they did. Manual beats
    /// automatic until they ask for the automatic back with a double click.
    column_override: Rc<Cell<Option<Pixels>>>,
    /// Where the history was last laid out. An element cannot know its own
    /// size until it has been laid out, so this is read a frame late; a change
    /// asks for one more frame, and the layout settles on the frame after the
    /// one that measured it rather than on the reader's next mouse move.
    measured: Rc<Cell<Option<Bounds<Pixels>>>>,
    /// The repository's remote names, read once. Empty until they arrive, which
    /// only means a remote-tracking ref is drawn as a plain branch until then.
    remote_names: Vec<SharedString>,
    _remote_names_task: Option<Task<()>>,
    selected_entry_idx: Option<usize>,
    hovered_entry_idx: Option<usize>,
    log_source: LogSource,
    log_order: LogOrder,
    selected_commit_diff: Option<CommitDiff>,
    selected_commit_diff_stats: Option<(usize, usize)>,
    _commit_diff_task: Option<Task<()>>,
    selected_commit_message: Option<DetailPanelCommitMessage>,
    _selected_commit_message_task: Option<Task<()>>,
    commit_details_split_state: Entity<SplitState>,
    repo_id: RepositoryId,
    changed_files_scroll_handle: UniformListScrollHandle,
    changed_files_view_mode: ChangedFilesViewMode,
    changed_files_expanded_dirs: HashMap<RepoPath, bool>,
    pending_select_sha: Option<Oid>,
}

impl GitGraph {
    fn invalidate_state(&mut self, cx: &mut Context<Self>) {
        self.graph_first_lane.set(0);
        self.lit_branch = None;
        self.kept_rows = None;
        self.graph_data.clear();
        self.search_state.matches.clear();
        self.search_state.selected_index = None;
        self.search_state.state.next_state();
        self.context_menu = None;
        cx.emit(ItemEvent::Edit);
        cx.notify();
    }

    /// Computes the height of a single commit row in the git graph.
    ///
    /// The returned value is snapped to the nearest physical pixel. This is
    /// required so that the canvas's float math and the `uniform_list` layout
    /// (which snaps to device pixels) agree on row positions; otherwise rows
    /// drift apart as the user scrolls when `ui_font_size` is fractional.
    fn row_height(window: &Window, _cx: &App) -> Pixels {
        GraphMetrics::for_window(window).row
    }

    fn visible_row_count(&self, window: &Window, cx: &App) -> usize {
        let row_height = Self::row_height(window, cx);
        let viewport_height = self
            .table_interaction_state
            .read(cx)
            .scroll_handle
            .0
            .borrow()
            .last_item_size
            .map_or(window.viewport_size().height, |size| size.item.height);

        ((viewport_height / row_height).ceil() as usize).min(self.graph_data.commits.len())
    }

    /// The share of the row each column gets, with the space of the columns this
    /// width cannot hold given back to the ones it can.
    /// How wide each of the four areas of a row is.
    ///
    /// Only the subject stretches. The graph takes exactly the room its lanes
    /// need, the age takes the room the longest age needs, and the labels take a
    /// What this history shows at the width it has, worked out from what it
    /// actually holds.
    fn history_layout(&self, window: &Window, cx: &App) -> HistoryLayout {
        let head = self.head_branch_name(cx);
        let metrics = GraphMetrics::for_window(window);
        let character = measure_text(window, "0");

        let mut widest_label = [px(0.); 4];
        for name in self.graph_data.label_names.iter() {
            let Some((kind, read)) = read_ref(name.as_ref(), head.as_deref(), &self.remote_names)
            else {
                continue;
            };
            for mode in [
                LabelMode::Full,
                LabelMode::Abbreviated,
                LabelMode::Single,
                LabelMode::Inline,
            ] {
                let text = shorten_ref(read.as_ref(), mode);
                let drawn = measure_text(window, text.as_ref()) + chip_chrome(metrics, kind);
                let slot = &mut widest_label[mode as usize];
                *slot = (*slot).max(drawn);
            }
        }

        fit(
            self.history_width(window, cx).max(px(1.)),
            HistoryContents {
                metrics,
                character,
                widest_label,
                lanes: self.graph_data.max_lanes,
            },
        )
    }

    /// How much of the row the lanes get: the step never changes, so the column
    /// is whatever the lanes it shows need. What is past the cap becomes rings
    /// at its edge rather than a graph squeezed until nothing can be told apart.
    fn graph_column_width(&self, window: &Window, layout: HistoryLayout) -> Pixels {
        let lanes = layout.lane_cap.min(self.graph_data.max_lanes.max(1));
        let rings = match self.graph_data.max_lanes > layout.lane_cap {
            true => GraphMetrics::for_window(window).lane,
            false => px(0.),
        };
        GraphMetrics::for_window(window).width_for(lanes) + rings
    }

    /// The sizes this history's rows are drawn at.
    fn row_metrics(&self, window: &Window, _cx: &App) -> GraphMetrics {
        GraphMetrics::for_window(window)
    }

    fn table_column_widths(
        &self,
        window: &Window,
        cx: &App,
        layout: HistoryLayout,
    ) -> Vec<DefiniteLength> {
        let container = self.history_width(window, cx).max(px(1.));
        let graph = self.graph_column_width(window, layout);
        let age = match layout.age {
            AgeShown::Column => AGE_COLUMN_WIDTH,
            _ => px(0.),
        };
        let labels = self
            .column_override
            .get()
            .unwrap_or(layout.label_width)
            .min((container - graph - age - layout.subject_min).max(px(0.)));
        let subject = (container - graph - age - labels).max(px(1.));
        let share = |width: Pixels| DefiniteLength::Fraction(width / container);

        // All four are shares of the same measured width. Mixing shares with
        // absolute widths only holds while the measurement is current: one
        // frame behind, the shares scale to the new width and the absolutes do
        // not, and the last column is pushed off the edge of the row.
        vec![share(labels), share(graph), share(subject), share(age)]
    }

    /// How far the lane window can be pushed before its right edge meets the
    /// last lane.
    fn furthest_lane(&self, layout: HistoryLayout) -> usize {
        self.graph_data
            .max_lanes
            .saturating_sub(layout.lane_cap.max(1))
    }

    /// Shift and the wheel walk the lane window. Without Shift the wheel is the
    /// list's, as it always was.
    fn handle_lane_scroll(
        &mut self,
        event: &ScrollWheelEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if !event.modifiers.shift {
            return;
        }
        let layout = self.history_layout(window, cx);
        let furthest = self.furthest_lane(layout);
        if furthest == 0 {
            return;
        }

        let delta = event.delta.pixel_delta(window.line_height());
        // A trackpad sends the sideways part, a wheel sends the vertical one.
        let step = match delta.x.abs() > delta.y.abs() {
            true => delta.x,
            false => delta.y,
        };
        let at = self.graph_first_lane.get();
        let next = match step < px(0.) {
            true => (at + 1).min(furthest),
            false => at.saturating_sub(1),
        };
        if next != at {
            self.graph_first_lane.set(next);
            cx.stop_propagation();
            cx.notify();
        }
    }

    /// Turns a point along the graph's scrollbar into the lane it names.
    fn scrub_lanes_to(&mut self, at: Point<Pixels>, window: &Window, cx: &mut Context<Self>) {
        let Some(track) = self.graph_track.get() else {
            return;
        };
        if track.size.width <= px(0.) {
            return;
        }
        let layout = self.history_layout(window, cx);
        let furthest = self.furthest_lane(layout);
        if furthest == 0 {
            return;
        }

        let along = ((at.x - track.origin.x) / track.size.width).clamp(0., 1.);
        let lane = (along * furthest as f32).round() as usize;
        if lane != self.graph_first_lane.get() {
            self.graph_first_lane.set(lane.min(furthest));
            cx.notify();
        }
    }

    /// The graph's own scrollbar, shown only when there is more graph than the
    /// column can hold.
    fn render_lane_scrollbar(
        &self,
        layout: HistoryLayout,
        window: &Window,
        cx: &mut Context<Self>,
    ) -> Option<AnyElement> {
        const TRACK_HEIGHT: Pixels = px(8.);

        let furthest = self.furthest_lane(layout);
        if furthest == 0 {
            self.graph_track.set(None);
            return None;
        }

        let container = self.history_width(window, cx).max(px(1.));
        let graph = self.graph_column_width(window, layout);
        let left = self
            .column_override
            .get()
            .unwrap_or(layout.label_width)
            .min(container);
        let shown = layout.lane_cap.max(1) as f32 / self.graph_data.max_lanes.max(1) as f32;
        let at = self.graph_first_lane.get() as f32 / furthest as f32;
        let measured = self.graph_track.clone();

        Some(
            div()
                .id("graph-lane-scrollbar")
                .debug_selector(|| "GRAPH_LANE_SCROLLBAR".to_string())
                .absolute()
                .bottom_0()
                .left(left)
                .w(graph)
                .h(TRACK_HEIGHT)
                .bg(cx.theme().colors().scrollbar_track_background)
                .child(
                    gpui::canvas(
                        move |bounds: Bounds<Pixels>, _window: &mut Window, _cx: &mut App| {
                            measured.set(Some(bounds));
                        },
                        |_, _: (), _, _| {},
                    )
                    .absolute()
                    .size_full(),
                )
                .child(
                    div()
                        .absolute()
                        .top_0()
                        .bottom_0()
                        .left(relative(at * (1.0 - shown)))
                        .w(relative(shown))
                        .rounded_sm()
                        .bg(cx.theme().colors().scrollbar_thumb_background),
                )
                .on_mouse_down(
                    MouseButton::Left,
                    cx.listener(|this, event: &MouseDownEvent, window, cx| {
                        this.scrub_lanes_to(event.position, window, cx);
                        cx.stop_propagation();
                    }),
                )
                .on_mouse_move(cx.listener(|this, event: &MouseMoveEvent, window, cx| {
                    if event.pressed_button == Some(MouseButton::Left) {
                        this.scrub_lanes_to(event.position, window, cx);
                    }
                }))
                .into_any_element(),
        )
    }

    /// The row for what has not been committed yet.
    ///
    /// A history that starts at the last commit is missing the work its reader
    /// is in the middle of. This row says how much there is and opens it.
    ///
    /// It is pinned above the list rather than being its first item: making it
    /// an item would shift every index the selection, the keyboard, the search
    /// and the scrolling are written in terms of, and a history that selects
    /// the commit below the one that was clicked is worse than a row that does
    /// not scroll away.
    fn render_working_tree_row(
        &self,
        layout: HistoryLayout,
        window: &Window,
        cx: &mut Context<Self>,
    ) -> Option<AnyElement> {
        let repository = self.get_repository(cx)?;
        let changed = repository.read(cx).snapshot().status_summary().count;
        if changed == 0 {
            return None;
        }

        let metrics = self.row_metrics(window, cx);
        let container = self.history_width(window, cx).max(px(1.));
        let labels = self
            .column_override
            .get()
            .unwrap_or(layout.label_width)
            .min(container);
        let first_lane = self.graph_first_lane.get();
        let lane_cap = layout.lane_cap.max(1);
        let lane = self
            .graph_data
            .commits
            .first()
            .map(|commit| commit.lane)
            .unwrap_or(0);
        let place = match lane >= first_lane && lane < first_lane + lane_cap {
            true => NodePlace::InLane(lane),
            false => NodePlace::PastTheEdge,
        };
        let colour = cx.theme().colors().text_accent;
        let subject = match changed {
            1 => "1 changed file".to_string(),
            many => format!("{many} changed files"),
        };
        let workspace = self.workspace.clone();

        Some(
            h_flex()
                .id("working-tree-row")
                .debug_selector(|| "GRAPH_WORKING_TREE".to_string())
                .h(metrics.row)
                .w_full()
                .flex_none()
                .cursor_pointer()
                .hover(|this| this.bg(cx.theme().colors().element_hover.opacity(0.6)))
                .child(div().w(labels).h_full().flex_none())
                .child(
                    div()
                        .relative()
                        .h_full()
                        .w(self.graph_column_width(window, layout))
                        .flex_none()
                        .overflow_hidden()
                        .child(
                            div()
                                .absolute()
                                .left(node_left(metrics, place, first_lane, lane_cap))
                                .top((metrics.row - metrics.node) / 2.0)
                                .size(metrics.node)
                                .rounded_full()
                                // Dashed, because it is not a commit: there is
                                // nothing here to check out, revert or copy.
                                .border_1()
                                .border_dashed()
                                .border_color(colour),
                        ),
                )
                .child(
                    h_flex()
                        .flex_1()
                        .min_w_0()
                        .items_center()
                        .gap_1()
                        .px_1()
                        .overflow_hidden()
                        .child(
                            div()
                                .flex_none()
                                .w(px(2.))
                                .h(metrics.label)
                                .rounded_sm()
                                .bg(colour.opacity(0.7)),
                        )
                        .child(Label::new(subject).color(Color::Accent).truncate()),
                )
                .on_click(cx.listener(move |_, _, window, cx| {
                    workspace
                        .update(cx, |workspace, cx| {
                            ProjectDiff::deploy_at(workspace, None, window, cx);
                        })
                        .ok();
                }))
                .into_any_element(),
        )
    }

    /// A history larger than this is not worth walking for a hover; nothing is
    /// dimmed rather than the wrong thing being dimmed.
    const BRANCH_WALK_BUDGET: usize = 20_000;

    /// The commit this one came from along its own branch: its first parent.
    ///
    /// Walking by the first parent keeps to the branch a reader is following,
    /// where the arrow keys walk the rows in the order the log printed them and
    /// wander into whatever was merged in.
    fn first_parent_of(&self, idx: usize) -> Option<usize> {
        let commit = self.graph_data.commits.get(idx)?;
        let parent = commit.data.parents.first()?;
        self.graph_data.row_of_commit.get(parent).copied()
    }

    /// The other way: the nearest commit that has this one as its first parent.
    fn first_child_of(&self, idx: usize) -> Option<usize> {
        let commit = self.graph_data.commits.get(idx)?;
        self.graph_data
            .rows_naming_parent
            .get(&commit.data.sha)?
            .iter()
            .copied()
            .filter(|row| {
                self.graph_data
                    .commits
                    .get(*row)
                    .and_then(|child| child.data.parents.first())
                    == Some(&commit.data.sha)
            })
            // Children sit above their parents, so the nearest is the last one
            // before this row.
            .filter(|row| *row < idx)
            .max()
    }

    fn select_first_parent(
        &mut self,
        _: &SelectFirstParent,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(parent) = self
            .selected_entry_idx
            .and_then(|idx| self.first_parent_of(idx))
        else {
            return;
        };
        self.select_entry(parent, ScrollStrategy::Center, cx);
    }

    fn select_first_child(
        &mut self,
        _: &SelectFirstChild,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(child) = self
            .selected_entry_idx
            .and_then(|idx| self.first_child_of(idx))
        else {
            return;
        };
        self.select_entry(child, ScrollStrategy::Center, cx);
    }

    /// Moves the working tree to a branch, asking first if there is work in it
    /// that a checkout would carry along or refuse over.
    fn check_out(&mut self, name: SharedString, window: &mut Window, cx: &mut Context<Self>) {
        let Some(repository) = self.get_repository(cx) else {
            return;
        };
        let dirty = repository.read(cx).snapshot().status_summary().count > 0;

        fn switch(name: SharedString, repository: Entity<Repository>, cx: &mut App) {
            let answer = repository.update(cx, |repository, _| {
                repository.change_branch(name.to_string())
            });
            cx.spawn(async move |_| {
                if let Ok(Err(error)) = answer.await {
                    log::error!("failed to check out the branch: {error:#}");
                }
            })
            .detach();
        }

        if !dirty {
            switch(name, repository, cx);
            return;
        }

        let answer = window.prompt(
            gpui::PromptLevel::Warning,
            "There is work in this tree that has not been committed.",
            Some("Stash it and check the branch out, or stay where you are."),
            &["Stash and Check Out", "Cancel"],
            cx,
        );
        cx.spawn(async move |this, cx| {
            if answer.await.ok() != Some(0) {
                return;
            }
            let stashed = this
                .update(cx, |this, cx| {
                    this.get_repository(cx)
                        .map(|repository| repository.update(cx, |repo, cx| repo.stash_all(cx)))
                })
                .ok()
                .flatten();
            if let Some(stashed) = stashed
                && stashed.await.is_err()
            {
                return;
            }
            this.update(cx, |this, cx| {
                if let Some(repository) = this.get_repository(cx) {
                    switch(name, repository, cx);
                }
            })
            .ok();
        })
        .detach();
    }

    /// What is being left out, and the one click that puts it all back.
    ///
    /// Shown only while something is filtered: a row of switches that are all
    /// off most of the time is a row spent saying nothing.
    fn render_filter_bar(&self, cx: &mut Context<Self>) -> Option<AnyElement> {
        let filter = &self.filter;
        let mut says = Vec::new();
        if let Some(solo) = &filter.solo {
            says.push(format!("only {solo}"));
        }
        match filter.hidden.len() {
            0 => {}
            1 => says.push(format!("{} hidden", filter.hidden[0])),
            many => says.push(format!("{many} branches hidden")),
        }
        if filter.hide_remotes {
            says.push("remotes unlabelled".to_string());
        }
        if filter.hide_tags {
            says.push("tags unlabelled".to_string());
        }
        if says.is_empty() {
            return None;
        }

        Some(
            h_flex()
                .id("history-filter-bar")
                .debug_selector(|| "GRAPH_FILTER_BAR".to_string())
                .w_full()
                .flex_none()
                .items_center()
                .gap_2()
                .px_2()
                .py_1()
                .bg(cx.theme().colors().element_selected.opacity(0.5))
                .child(
                    Label::new(says.join(" · "))
                        .size(LabelSize::Small)
                        .color(Color::Accent),
                )
                .child(div().flex_1())
                .child(
                    IconButton::new("clear-history-filter", IconName::Close)
                        .icon_size(IconSize::XSmall)
                        .tooltip(Tooltip::text("Show everything again"))
                        .on_click(cx.listener(|this, _, _window, cx| this.clear_filter(cx))),
                )
                .into_any_element(),
        )
    }

    /// The two switches that are about whole kinds of label rather than one
    /// branch.
    fn render_label_switches(&self, cx: &mut Context<Self>) -> impl IntoElement {
        h_flex()
            .flex_none()
            .gap_0p5()
            .items_center()
            .child(
                IconButton::new("hide-remotes", IconName::Server)
                    .icon_size(IconSize::XSmall)
                    .icon_color(match self.filter.hide_remotes {
                        true => Color::Disabled,
                        false => Color::Muted,
                    })
                    .tooltip(Tooltip::text(match self.filter.hide_remotes {
                        true => "Label remote branches",
                        false => "Stop labelling remote branches",
                    }))
                    .on_click(cx.listener(|this, _, _window, cx| {
                        this.filter.hide_remotes = !this.filter.hide_remotes;
                        this.apply_filter(cx);
                    })),
            )
            .child(
                IconButton::new("hide-tags", IconName::Bookmark)
                    .icon_size(IconSize::XSmall)
                    .icon_color(match self.filter.hide_tags {
                        true => Color::Disabled,
                        false => Color::Muted,
                    })
                    .tooltip(Tooltip::text(match self.filter.hide_tags {
                        true => "Label tags",
                        false => "Stop labelling tags",
                    }))
                    .on_click(cx.listener(|this, _, _window, cx| {
                        this.filter.hide_tags = !this.filter.hide_tags;
                        this.apply_filter(cx);
                    })),
            )
    }

    /// Works out which rows the filter leaves, and hands the list its new
    /// length. Called whenever the filter changes or the history grows.
    fn apply_filter(&mut self, cx: &mut Context<Self>) {
        if !self.filter.is_on() {
            self.kept_rows = None;
            cx.notify();
            return;
        }

        let head = self.head_branch_name(cx);
        // Every branch the filter has an opinion about, and the rows behind it.
        let mut keep: Option<HashSet<usize>> = None;
        let mut drop: HashSet<usize> = HashSet::default();
        let mut gave_up = false;

        for row in 0..self.graph_data.commits.len() {
            let Some(commit) = self.graph_data.commits.get(row) else {
                break;
            };
            if commit.data.ref_names.is_empty() {
                continue;
            }
            let names: Vec<SharedString> = commit
                .data
                .ref_names
                .iter()
                .filter_map(|name| {
                    read_ref(name.as_ref(), head.as_deref(), &self.remote_names)
                        .map(|(_, read)| read)
                })
                .collect();

            for name in names {
                let Some(reached) = self.graph_data.branch_of(row, Self::BRANCH_WALK_BUDGET) else {
                    gave_up = true;
                    continue;
                };
                match self.filter.solo.as_ref() {
                    // Soloing one branch decides the whole of what is kept.
                    Some(only) if *only == name => {
                        keep.get_or_insert_with(HashSet::default).extend(reached);
                    }
                    Some(_) => {}
                    None if !self.filter.keeps(&name) => drop.extend(reached),
                    None => {}
                }
            }
        }

        // A history too large to walk keeps everything rather than losing rows
        // the reader did not ask to lose.
        if gave_up && keep.is_none() {
            self.kept_rows = None;
            cx.notify();
            return;
        }

        let kept: Vec<usize> = (0..self.graph_data.commits.len())
            .filter(|row| match &keep {
                Some(keep) => keep.contains(row),
                None => !drop.contains(row),
            })
            .collect();

        self.kept_rows = Some(Rc::new(kept));
        // The selection is in rows of the history, and the row it names may
        // have just been filtered away.
        if let Some(selected) = self.selected_entry_idx
            && self.list_row_of(selected).is_none()
        {
            self.selected_entry_idx = None;
            self.compare_against = None;
        }
        let picked: Vec<usize> = self
            .picked_rows
            .iter()
            .copied()
            .filter(|row| self.list_row_of(*row).is_some())
            .collect();
        self.picked_rows = picked;
        cx.notify();
    }

    /// How many rows the list has.
    fn rows_in_the_list(&self, all: usize) -> usize {
        match &self.kept_rows {
            Some(kept) => kept.len(),
            None => all,
        }
    }

    /// The row of the history the list is showing at this position.
    fn row_at(&self, in_the_list: usize) -> Option<usize> {
        match &self.kept_rows {
            Some(kept) => kept.get(in_the_list).copied(),
            None => (in_the_list < self.graph_data.commits.len()).then_some(in_the_list),
        }
    }

    /// Where in the list a row of the history is, if the filter left it.
    fn list_row_of(&self, row: usize) -> Option<usize> {
        match &self.kept_rows {
            Some(kept) => kept.binary_search(&row).ok(),
            None => (row < self.graph_data.commits.len()).then_some(row),
        }
    }

    /// Hides a branch, or brings it back.
    fn toggle_hidden(&mut self, name: SharedString, cx: &mut Context<Self>) {
        match self.filter.hidden.iter().position(|hidden| *hidden == name) {
            Some(at) => {
                self.filter.hidden.remove(at);
            }
            None => self.filter.hidden.push(name),
        }
        self.apply_filter(cx);
    }

    /// Leaves one branch on screen, or puts the rest back.
    fn toggle_solo(&mut self, name: SharedString, cx: &mut Context<Self>) {
        self.filter.solo = match self.filter.solo.as_ref() == Some(&name) {
            true => None,
            false => Some(name),
        };
        self.apply_filter(cx);
    }

    /// Puts every row back.
    fn clear_filter(&mut self, cx: &mut Context<Self>) {
        self.filter = HistoryFilter::default();
        self.apply_filter(cx);
    }

    /// The commits a command opened over `idx` should act on, oldest last,
    /// which is the order the history is drawn in.
    fn picked_commits(&self, idx: usize) -> Vec<Oid> {
        let mut rows = match self.picked_rows.contains(&idx) {
            true => self.picked_rows.clone(),
            false => vec![idx],
        };
        rows.sort_unstable();
        rows.iter()
            .filter_map(|row| self.graph_data.commits.get(*row))
            .map(|commit| commit.data.sha)
            .collect()
    }

    /// Adds or removes one row from what is picked.
    fn toggle_picked(&mut self, idx: usize) {
        match self.picked_rows.iter().position(|row| *row == idx) {
            Some(at) => {
                self.picked_rows.remove(at);
            }
            None => self.picked_rows.push(idx),
        }
    }

    /// Picks everything between the selection and `idx`, which is what a reader
    /// means by holding Shift over a list.
    fn pick_through(&mut self, idx: usize) {
        let Some(from) = self.selected_entry_idx else {
            self.picked_rows = vec![idx];
            return;
        };
        let (first, last) = match from <= idx {
            true => (from, idx),
            false => (idx, from),
        };
        self.picked_rows = (first..=last).collect();
    }

    /// Lights the branch a label names, and dims everything that is not in it.
    fn light_branch(&mut self, tip_row: Option<usize>, cx: &mut Context<Self>) {
        let lit = tip_row.and_then(|row| {
            self.graph_data
                .branch_of(row, Self::BRANCH_WALK_BUDGET)
                .map(Rc::new)
        });
        let changed = match (&self.lit_branch, &lit) {
            (None, None) => false,
            (Some(was), Some(now)) => !Rc::ptr_eq(was, now) && **was != **now,
            _ => true,
        };
        if changed {
            self.lit_branch = lit;
            cx.notify();
        }
    }

    /// The branch a commit is on when it carries no label of its own, shown as
    /// a translucent chip so it does not read as a label the commit has.
    fn ghost_branch(&self, idx: usize) -> Option<(RefKind, SharedString)> {
        let commit = self.graph_data.commits.get(idx)?;
        if !commit.data.ref_names.is_empty() {
            return None;
        }
        let tip = self.graph_data.nearest_tip(idx, Self::BRANCH_WALK_BUDGET)?;
        let head = None;
        self.graph_data
            .commits
            .get(tip)?
            .data
            .ref_names
            .iter()
            .find_map(|name| read_ref(name.as_ref(), head, &self.remote_names))
    }

    /// The remote a picture of an author can be asked for, if the host has any.
    fn avatar_remote(&self, cx: &mut Context<Self>) -> Option<GitRemote> {
        let repository = self.get_repository(cx)?;
        repository.update(cx, |repository, cx| {
            let remote_url = repository.default_remote_url()?;
            let registry = GitHostingProviderRegistry::default_global(cx);
            let (provider, parsed) = parse_git_remote_url(registry, &remote_url)?;
            Some(GitRemote {
                host: provider,
                owner: parsed.owner.into(),
                repo: parsed.repo.into(),
            })
        })
    }

    /// The branch `HEAD` is on, which decides which label is the current one.
    fn head_branch_name(&self, cx: &App) -> Option<SharedString> {
        self.get_repository(cx).and_then(|repository| {
            repository
                .read(cx)
                .snapshot()
                .branch
                .as_ref()
                .map(|branch| SharedString::from(branch.name().to_string()))
        })
    }

    /// How wide the history is, for deciding what fits in it.
    fn history_width(&self, window: &Window, _cx: &App) -> Pixels {
        // Nothing until the history has been laid out once. Guessing narrow
        // there would show the narrow layout for a frame and then swap it,
        // which reads as a flicker.
        match self.measured.get() {
            Some(bounds) if bounds.size.width > px(0.) => bounds.size.width,
            _ => window.viewport_size().width,
        }
    }

    /// Records how wide the history was laid out, and asks for one more frame
    /// when that changes, so the columns follow the width they were actually
    /// given rather than the width of the frame before. Without it a window
    /// dragged to a new size and then left alone keeps the old set of columns
    /// until the reader touches something.
    fn measure_history_width(&self) -> impl IntoElement {
        let measured = self.measured.clone();
        gpui::canvas(
            move |bounds: Bounds<Pixels>, window: &mut Window, _cx: &mut App| {
                if measured.get().map(|was| was.size.width) != Some(bounds.size.width) {
                    measured.set(Some(bounds));
                    window.request_animation_frame();
                }
            },
            |_, _: (), _, _| {},
        )
        .absolute()
        .size_full()
    }

    /// The border between the labels and the graph, which the reader can drag.
    ///
    /// What they drag it to is remembered and beats what the width would have
    /// chosen, until they ask for the automatic back with a double click.
    fn render_label_divider(
        &self,
        layout: HistoryLayout,
        cx: &mut Context<Self>,
    ) -> Option<AnyElement> {
        /// Wide enough to hit without being wide enough to see.
        const GRIP: Pixels = px(6.);

        if !layout.labels.has_a_column() {
            return None;
        }
        let at = self.column_override.get().unwrap_or(layout.label_width);
        if at <= px(0.) {
            return None;
        }

        Some(
            div()
                .id("label-column-divider")
                .debug_selector(|| "GRAPH_LABEL_DIVIDER".to_string())
                .absolute()
                .top_0()
                .bottom_0()
                .left(at - GRIP / 2.0)
                .w(GRIP)
                .cursor_col_resize()
                .on_mouse_down(
                    MouseButton::Left,
                    cx.listener(|this, event: &MouseDownEvent, window, cx| {
                        match event.click_count >= 2 {
                            // A second click gives the width back to the layout.
                            true => this.column_override.set(None),
                            false => this.drag_label_divider(event.position, window, cx),
                        }
                        cx.stop_propagation();
                        cx.notify();
                    }),
                )
                .on_mouse_move(cx.listener(|this, event: &MouseMoveEvent, window, cx| {
                    if event.pressed_button == Some(MouseButton::Left) {
                        this.drag_label_divider(event.position, window, cx);
                    }
                }))
                .into_any_element(),
        )
    }

    /// Puts the border where the reader dropped it, within what the row can give.
    fn drag_label_divider(&mut self, at: Point<Pixels>, window: &Window, cx: &mut Context<Self>) {
        let Some(bounds) = self.measured.get() else {
            return;
        };
        let layout = self.history_layout(window, cx);
        let graph = self.graph_column_width(window, layout);
        let widest = (bounds.size.width - graph - layout.subject_min).max(LABEL_COLUMN_MIN);
        let wanted = (at.x - bounds.origin.x).clamp(LABEL_COLUMN_MIN, widest);

        if self.column_override.get() != Some(wanted) {
            self.column_override.set(Some(wanted));
            cx.notify();
        }
    }

    pub fn new(
        repo_id: RepositoryId,
        git_store: Entity<GitStore>,
        workspace: WeakEntity<Workspace>,
        log_source: Option<LogSource>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let focus_handle = cx.focus_handle();
        cx.on_focus(&focus_handle, window, |_, _, cx| cx.notify())
            .detach();

        let accent_colors = cx.theme().accents();
        let graph = GraphData::new(accent_colors_count(accent_colors));
        let log_source = log_source.unwrap_or_default();
        let log_order = LogOrder::default();

        cx.subscribe(&git_store, |this, _, event, cx| match event {
            GitStoreEvent::RepositoryUpdated(updated_repo_id, repo_event, _) => {
                if this.repo_id == *updated_repo_id {
                    if let Some(repository) = this.get_repository(cx) {
                        this.on_repository_event(repository, repo_event, cx);
                    }
                }
            }
            _ => {}
        })
        .detach();

        let search_editor = cx.new(|cx| {
            let mut editor = Editor::single_line(window, cx);
            editor.set_placeholder_text("Search commits…", window, cx);
            editor
        });

        let table_interaction_state = cx.new(|cx| {
            let mut state = TableInteractionState::new(cx);
            state.focus_handle = state.focus_handle.tab_index(1).tab_stop(true);
            state
        });

        let mut row_height = Self::row_height(window, cx);

        cx.observe_global_in::<settings::SettingsStore>(window, move |this, window, cx| {
            let new_row_height = Self::row_height(window, cx);
            if new_row_height != row_height {
                // The `uniform_list` powering the table caches the item size
                // from its last layout; invalidate it so it re-measures with
                // the new row height on the next frame.
                this.table_interaction_state.update(cx, |state, _cx| {
                    state.scroll_handle.0.borrow_mut().last_item_size = None;
                });
                row_height = new_row_height;
                cx.notify();
            }
        })
        .detach();

        let mut this = GitGraph {
            focus_handle,
            git_store,
            search_state: SearchState {
                case_sensitive: false,
                editor: search_editor,
                matches: IndexSet::default(),
                selected_index: None,
                state: QueryState::Empty,
            },
            workspace,
            graph_data: graph,
            _commit_diff_task: None,
            context_menu: None,
            table_interaction_state,
            filter: HistoryFilter::default(),
            kept_rows: None,
            picked_rows: Vec::new(),
            compare_against: None,
            lit_branch: None,
            graph_track: Rc::new(Cell::new(None)),
            graph_first_lane: Rc::new(Cell::new(0)),
            column_override: Rc::new(Cell::new(None)),
            measured: Rc::new(Cell::new(None)),
            remote_names: Vec::new(),
            _remote_names_task: None,
            selected_entry_idx: None,
            hovered_entry_idx: None,
            selected_commit_diff: None,
            selected_commit_diff_stats: None,
            selected_commit_message: None,
            _selected_commit_message_task: None,
            log_source,
            log_order,
            commit_details_split_state: cx.new(|_cx| SplitState::new()),
            repo_id,
            changed_files_scroll_handle: UniformListScrollHandle::new(),
            changed_files_view_mode: ChangedFilesViewMode::default(),
            changed_files_expanded_dirs: HashMap::default(),
            pending_select_sha: None,
        };

        this.fetch_initial_graph_data(cx);
        this.fetch_remote_names(cx);
        this
    }

    /// Learns the repository's remote names, so that `origin/main` can be told
    /// apart from a branch whose name happens to contain a slash.
    fn fetch_remote_names(&mut self, cx: &mut Context<Self>) {
        let Some(repository) = self.get_repository(cx) else {
            return;
        };
        let remotes = repository.update(cx, |repository, _cx| repository.remote_urls());
        self._remote_names_task = Some(cx.spawn(async move |this, cx| {
            let Ok(Ok(remotes)) = remotes.await else {
                return;
            };
            this.update(cx, |this, cx| {
                let mut names: Vec<SharedString> = remotes
                    .into_keys()
                    .map(|name| SharedString::from(name))
                    .collect();
                names.sort();
                this.remote_names = names;
                cx.notify();
            })
            .ok();
        }));
    }

    fn on_repository_event(
        &mut self,
        repository: Entity<Repository>,
        event: &RepositoryEvent,
        cx: &mut Context<Self>,
    ) {
        match event {
            RepositoryEvent::GraphEvent((source, order), event)
                if source == &self.log_source && order == &self.log_order =>
            {
                match event {
                    GitGraphEvent::FullyLoaded => {
                        if let Some(pending_sha_index) =
                            self.pending_select_sha.take().and_then(|oid| {
                                repository
                                    .read(cx)
                                    .get_graph_data(source.clone(), *order)
                                    .and_then(|data| data.commit_oid_to_index.get(&oid).copied())
                            })
                        {
                            self.select_entry(pending_sha_index, ScrollStrategy::Nearest, cx);
                        }
                        let count = match self.graph_data.max_commit_count {
                            AllCommitCount::FullyLoaded(count) | AllCommitCount::Loading(count) => {
                                count
                            }
                            AllCommitCount::NotLoaded => 0,
                        };
                        self.graph_data.max_commit_count = AllCommitCount::FullyLoaded(count);
                        cx.notify();
                    }
                    GitGraphEvent::LoadingError => {
                        cx.notify();
                    }
                    GitGraphEvent::CountUpdated(commit_count) => {
                        let old_count = self.graph_data.commits.len();
                        // The rows a filter leaves have to keep up with the rows
                        // arriving, or a filtered history stops growing where
                        // the reader cannot see why.
                        let refilter = self.filter.is_on();

                        if let Some(pending_selection_index) =
                            repository.update(cx, |repository, cx| {
                                let GraphDataResponse {
                                    commits,
                                    is_loading,
                                    error: _,
                                } = repository.graph_data(
                                    source.clone(),
                                    *order,
                                    old_count..*commit_count,
                                    cx,
                                );
                                self.graph_data.add_commits(commits);

                                let pending_sha_index = self.pending_select_sha.and_then(|oid| {
                                    repository.get_graph_data(source.clone(), *order).and_then(
                                        |data| data.commit_oid_to_index.get(&oid).copied(),
                                    )
                                });

                                if !is_loading && pending_sha_index.is_none() {
                                    self.pending_select_sha.take();
                                }

                                pending_sha_index
                            })
                        {
                            self.select_entry(pending_selection_index, ScrollStrategy::Nearest, cx);
                            self.pending_select_sha.take();
                        }

                        if refilter {
                            self.apply_filter(cx);
                        }
                        cx.notify();
                    }
                }
            }
            RepositoryEvent::HeadChanged | RepositoryEvent::BranchListChanged => {
                // Only invalidate if we scanned atleast once,
                // meaning we are not inside the initial repo loading state
                // NOTE: this fixes an loading performance regression
                if repository.read(cx).scan_id > 1 {
                    self.pending_select_sha = None;
                    self.invalidate_state(cx);
                }
            }
            RepositoryEvent::StashEntriesChanged if self.log_source == LogSource::All => {
                // Stash entries initial's scan id is 2, so we don't want to invalidate the graph before that
                if repository.read(cx).scan_id > 2 {
                    self.pending_select_sha = None;
                    self.invalidate_state(cx);
                }
            }
            RepositoryEvent::GraphEvent(_, _) => {}
            _ => {}
        }
    }

    fn fetch_initial_graph_data(&mut self, cx: &mut App) {
        if let Some(repository) = self.get_repository(cx) {
            repository.update(cx, |repository, cx| {
                let commits = repository
                    .graph_data(self.log_source.clone(), self.log_order, 0..usize::MAX, cx)
                    .commits;
                self.graph_data.add_commits(commits);
            });
        }
    }

    fn get_repository(&self, cx: &App) -> Option<Entity<Repository>> {
        let git_store = self.git_store.read(cx);
        git_store.repositories().get(&self.repo_id).cloned()
    }

    /// Checks whether a ref name from git's `%D` decoration
    ///  format refers to the currently checked-out branch.
    /// Renders one ref chip for the commit at `commit_idx`. The chip carries a
    /// right-click handler so a custom command can be resolved against the ref
    /// the reader actually clicked, not just against the commit.
    fn render_ref_chip(
        &self,
        kind: RefKind,
        name: &SharedString,
        accent_color: Hsla,
        mode: LabelMode,
        commit_idx: usize,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let is_head = kind == RefKind::Head;
        let is_hidden = self.filter.hidden.contains(name);
        let is_solo = self.filter.solo.as_ref() == Some(name);
        let shortened = shorten_ref(name.as_ref(), mode);
        let chip = Chip::new(shortened.clone())
            .label_size(LabelSize::Small)
            .truncate()
            .icon(kind.icon())
            .map(|chip| match is_head {
                true => chip
                    .bg_color(accent_color.opacity(0.25))
                    .border_color(accent_color.opacity(0.5)),
                false => chip
                    .bg_color(accent_color.opacity(0.08))
                    .border_color(accent_color.opacity(0.25)),
            });

        let ref_name = name.clone();
        h_flex()
            .id(ElementId::Name(
                format!("ref-chip-{commit_idx}-{name}").into(),
            ))
            .gap_0p5()
            .items_center()
            .cursor_pointer()
            .when(is_hidden, |this| this.opacity(0.5))
            .child(chip)
            .on_hover(cx.listener(move |this, hovering: &bool, _window, cx| {
                this.light_branch(hovering.then_some(commit_idx), cx);
            }))
            .child(
                // The eye carries the state a reader set: a hidden branch is
                // greyed, a soloed one wears the accent, and both say so on the
                // label rather than in a list somewhere else.
                IconButton::new(
                    ElementId::Name(format!("hide-{commit_idx}-{name}").into()),
                    match is_hidden {
                        true => IconName::EyeOff,
                        false => IconName::Eye,
                    },
                )
                .icon_size(IconSize::XSmall)
                .icon_color(match (is_hidden, is_solo) {
                    (true, _) => Color::Disabled,
                    (_, true) => Color::Accent,
                    _ => Color::Muted,
                })
                .tooltip(Tooltip::text(match is_hidden {
                    true => "Show this branch",
                    false => "Hide this branch",
                }))
                .on_click({
                    let ref_name = ref_name.clone();
                    cx.listener(move |this, _, _window, cx| {
                        this.toggle_hidden(ref_name.clone(), cx);
                        cx.stop_propagation();
                    })
                }),
            )
            .on_click(cx.listener({
                let ref_name = ref_name.clone();
                move |this, event: &ClickEvent, window, cx| {
                    match event.click_count() >= 2 {
                        // Twice on a label is the shortest way to ask for the
                        // branch it names.
                        true => this.check_out(ref_name.clone(), window, cx),
                        false => this.select_entry(commit_idx, ScrollStrategy::Center, cx),
                    }
                    cx.stop_propagation();
                }
            }))
            // Whatever the rule left of the name, the whole of it is one hover
            // away: a shortened label that cannot be read in full is a label
            // that names nothing.
            .when(shortened != *name, |this| {
                this.tooltip(Tooltip::text(name.clone()))
            })
            .on_mouse_down(
                MouseButton::Right,
                cx.listener(move |this, event: &MouseDownEvent, window, cx| {
                    this.deploy_entry_context_menu(
                        event.position,
                        commit_idx,
                        Some(ref_name.clone()),
                        window,
                        cx,
                    );
                    cx.stop_propagation();
                }),
            )
            .into_any_element()
    }

    /// What a commit is labelled with, in the order a reader looks for it.
    fn refs_of(&self, idx: usize, head_branch_name: Option<&str>) -> Vec<(RefKind, SharedString)> {
        let Some(commit) = self.graph_data.commits.get(idx) else {
            return Vec::new();
        };
        let mut refs: Vec<(RefKind, SharedString)> = commit
            .data
            .ref_names
            .iter()
            .filter_map(|decoration| {
                read_ref(decoration.as_ref(), head_branch_name, &self.remote_names)
            })
            .filter(|(kind, _)| self.filter.shows(*kind))
            .collect();
        refs.sort_by_key(|(kind, _)| *kind);
        refs
    }

    /// The ref chips for a commit, packed against the graph so that a label and
    /// its node read as one thing.
    fn render_refs_cell(
        &self,
        idx: usize,
        metrics: GraphMetrics,
        accent_color: Hsla,
        head_branch_name: Option<&str>,
        mode: LabelMode,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let refs = self.refs_of(idx, head_branch_name);
        if refs.is_empty() {
            return div().h(metrics.row).into_any_element();
        }

        // A commit on a release day can carry a dozen tags. Past what the width
        // allows the chips stop naming anything and start eating the subject,
        // so the rest are counted instead and named in the count's tooltip.
        let shown = refs.len().min(mode.chips_a_row());
        let rest: Vec<&str> = refs[shown..]
            .iter()
            .map(|(_, name)| name.as_ref())
            .collect();
        let overflow = (!rest.is_empty()).then(|| (rest.len(), rest.join(", ")));

        h_flex()
            .h(metrics.row)
            .w_full()
            .items_center()
            .justify_end()
            .gap_1()
            .pl_1()
            .overflow_hidden()
            .debug_selector(move || format!("GRAPH_REFS-{idx}"))
            .children(refs[..shown].iter().map(|(kind, name)| {
                h_flex()
                    .h(metrics.label)
                    .items_center()
                    // Without this the chip keeps its natural width and the row
                    // overflows to the left, cutting the start of a branch name
                    // -- the one end of it a reader cannot do without.
                    .min_w_0()
                    .overflow_hidden()
                    .child(self.render_ref_chip(*kind, name, accent_color, mode, idx, cx))
                    .into_any_element()
            }))
            .when_some(overflow, |this, (count, names)| {
                this.child(
                    div()
                        .flex_none()
                        .id(ElementId::NamedInteger("more-refs".into(), idx as u64))
                        .h(metrics.label)
                        .flex()
                        .items_center()
                        .child(
                            Chip::new(format!("+{count}"))
                                .label_size(LabelSize::Small)
                                .bg_color(accent_color.opacity(0.08))
                                .border_color(accent_color.opacity(0.25)),
                        )
                        .tooltip(Tooltip::text(names)),
                )
            })
            .into_any_element()
    }

    /// The branch a commit is on, drawn hollow so it does not read as a label
    /// the commit carries.
    fn render_ghost_chip(
        &self,
        idx: usize,
        metrics: GraphMetrics,
        accent_color: Hsla,
        kind: RefKind,
        name: &SharedString,
        layout: HistoryLayout,
    ) -> AnyElement {
        h_flex()
            .h(metrics.row)
            .w_full()
            .items_center()
            .justify_end()
            .gap_1()
            .pl_1()
            .overflow_hidden()
            .debug_selector(move || format!("GRAPH_GHOST-{idx}"))
            .child(
                h_flex()
                    .h(metrics.label)
                    .items_center()
                    .min_w_0()
                    .overflow_hidden()
                    .opacity(0.55)
                    .child(
                        Chip::new(shorten_ref(name.as_ref(), layout.labels))
                            .label_size(LabelSize::Small)
                            .truncate()
                            .icon(kind.icon())
                            .bg_color(accent_color.opacity(0.04))
                            .border_color(accent_color.opacity(0.18)),
                    ),
            )
            .into_any_element()
    }

    /// One row of the graph: the lanes crossing it, and the commit's own node.
    ///
    /// The node is a real element rather than a dot on a canvas, so that it can
    /// carry the author and so that it cannot drift away from its row -- the row
    /// is its parent.
    fn render_graph_cell(
        &self,
        idx: usize,
        metrics: GraphMetrics,
        layout: HistoryLayout,
        author: &SharedString,
        portrait: Option<CommitPortrait>,
        window: &mut Window,
        cx: &mut App,
    ) -> AnyElement {
        // The selected row's lines are drawn heavier. Hover is deliberately left
        // alone: a weight that changes under a moving pointer reads as the whole
        // list flickering.
        const SELECTED_LINE_WEIGHT: f32 = 1.6;

        let Some(commit) = self.graph_data.commits.get(idx) else {
            return div().h(metrics.row).into_any_element();
        };

        let lanes: SmallVec<[LanePaint; 4]> = SmallVec::from_slice(self.graph_data.lanes_at(idx));
        let node_lane = commit.lane;
        let first_lane = self.graph_first_lane.get();
        let lane_cap = layout.lane_cap.max(1);
        let place = if node_lane < first_lane {
            NodePlace::BeforeTheEdge
        } else if node_lane >= first_lane + lane_cap {
            NodePlace::PastTheEdge
        } else {
            NodePlace::InLane(node_lane)
        };
        let node_color = cx
            .theme()
            .accents()
            .color_for_index(commit.color_idx as u32);
        let paint = GraphRowPaint {
            metrics,
            first_lane,
            lane_cap,
            connector: (!commit.data.ref_names.is_empty()).then_some((node_lane, commit.color_idx)),
            emphasis: match self.selected_entry_idx == Some(idx) {
                true => SELECTED_LINE_WEIGHT,
                false => 1.0,
            },
        };
        let initials = initials_of(author);
        let author = author.clone();

        div()
            .relative()
            .h(metrics.row)
            .w_full()
            .overflow_hidden()
            .debug_selector(move || format!("GRAPH_CELL-{idx}"))
            .child(
                gpui::canvas(
                    |_, _, _| {},
                    move |bounds, _: (), window: &mut Window, cx: &mut App| {
                        paint_row_lanes(bounds, &lanes, paint, cx.theme().accents(), window);
                    },
                )
                .absolute()
                .size_full(),
            )
            .child(
                div()
                    .id(ElementId::NamedInteger("graph-node".into(), idx as u64))
                    .debug_selector(move || format!("GRAPH_NODE-{idx}"))
                    .absolute()
                    .left(node_left(metrics, place, first_lane, lane_cap))
                    .top((metrics.row - metrics.node) / 2.0)
                    .size(metrics.node)
                    .rounded_full()
                    .flex()
                    .items_center()
                    .justify_center()
                    // A commit whose lane the column is not showing is a ring
                    // rather than a dot: the reader sees that it is there and
                    // which branch it is on, and that its lane is elsewhere.
                    .map(|this| match place {
                        NodePlace::InLane(_) => this.bg(node_color),
                        _ => this.border_2().border_color(node_color),
                    })
                    .when(metrics.node_holds_initials(), |this| {
                        this.child(Label::new(initials).size(LabelSize::XSmall).color(
                            Color::Custom(match place {
                                NodePlace::InLane(_) => readable_on(node_color),
                                _ => node_color,
                            }),
                        ))
                    })
                    // The initials are drawn first and stay drawn: a picture
                    // that never arrives leaves a node that still says whose
                    // commit it is, rather than a grey circle.
                    .children(portrait.and_then(|portrait| {
                        let avatar = CommitAvatar::new(
                            &portrait.sha,
                            portrait.author_email,
                            portrait.remote.as_ref(),
                        )
                        .avatar(window, cx)?;
                        Some(
                            div()
                                .absolute()
                                .size_full()
                                .rounded_full()
                                .overflow_hidden()
                                .child(avatar.size(metrics.node)),
                        )
                    }))
                    .when(!author.is_empty(), |this| {
                        this.tooltip(Tooltip::text(author))
                    }),
            )
            .into_any_element()
    }

    fn render_table_rows(
        &mut self,
        range: Range<usize>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Vec<Vec<AnyElement>> {
        let repository = self.get_repository(cx);

        let head_branch_name = self.head_branch_name(cx);
        let layout = self.history_layout(window, cx);
        let age_has_a_column = matches!(layout.age, AgeShown::Column);
        let remote = self.avatar_remote(cx);
        let metrics = self.row_metrics(window, cx);
        let now = OffsetDateTime::now_utc();

        // We fetch data outside the visible viewport to avoid loading entries when
        // users scroll through the git graph
        if let Some(repository) = repository.as_ref() {
            const FETCH_RANGE: usize = 100;
            repository.update(cx, |repository, cx| {
                self.graph_data.commits[range.start.saturating_sub(FETCH_RANGE)
                    ..(range.end + FETCH_RANGE)
                        .min(self.graph_data.commits.len().saturating_sub(1))]
                    .iter()
                    .for_each(|commit| {
                        repository.fetch_commit_data(commit.data.sha, false, cx);
                    });
            });
        }

        range
            .map(|in_the_list| {
                let Some((commit, repository, idx)) = self
                    .row_at(in_the_list)
                    .and_then(|idx| self.graph_data.commits.get(idx).map(|commit| (commit, idx)))
                    .zip(repository.as_ref())
                    .map(|((commit, idx), repository)| (commit, repository, idx))
                else {
                    return (0..TABLE_COLUMN_COUNT)
                        .map(|_| div().h(metrics.row).into_any_element())
                        .collect();
                };

                let data = repository.update(cx, |repository, cx| {
                    repository
                        .fetch_commit_data(commit.data.sha, false, cx)
                        .clone()
                });

                let subject: SharedString;
                let author_name: SharedString;
                let age: SharedString;
                let committed_on: Option<SharedString>;
                let is_fresh: bool;

                let mut author_email: Option<SharedString> = None;

                if let CommitDataState::Loaded(ref data) = data {
                    subject = data.subject.clone();
                    author_name = data.author_name.clone();
                    author_email =
                        (!data.author_email.is_empty()).then(|| data.author_email.clone());
                    age = format_relative_timestamp(data.commit_timestamp, now).into();
                    committed_on = Some(format_timestamp(data.commit_timestamp).into());
                    is_fresh = is_younger_than_a_day(data.commit_timestamp, now);
                } else {
                    subject = "Loading…".into();
                    author_name = "".into();
                    age = "".into();
                    committed_on = None;
                    is_fresh = false;
                }

                let accent_colors = cx.theme().accents();
                let accent_color = accent_colors
                    .0
                    .get(commit.color_idx)
                    .copied()
                    .unwrap_or_else(|| accent_colors.0.first().copied().unwrap_or_default());

                let is_selected = self.selected_entry_idx == Some(idx);
                let is_matched = self.search_state.matches.contains(&commit.data.sha);
                // A date on every row is noise in a list a reader scans by
                // subject. It earns its place where the reader is already
                // looking, and on anything recent enough that "when" is the
                // question being asked.
                let shows_age = age_is_shown(
                    layout.age,
                    is_selected,
                    self.hovered_entry_idx == Some(idx),
                    is_fresh,
                );

                let subject_label = if is_matched {
                    let query = match &self.search_state.state {
                        QueryState::Confirmed((query, _)) => Some(query.clone()),
                        _ => None,
                    };
                    let highlight_ranges = query
                        .and_then(|q| {
                            let ranges = if self.search_state.case_sensitive {
                                subject
                                    .match_indices(q.as_str())
                                    .map(|(start, matched)| start..start + matched.len())
                                    .collect::<Vec<_>>()
                            } else {
                                let q = q.to_lowercase();
                                let subject_lower = subject.to_lowercase();

                                subject_lower
                                    .match_indices(&q)
                                    .filter_map(|(start, matched)| {
                                        let end = start + matched.len();
                                        subject.is_char_boundary(start).then_some(()).and_then(
                                            |_| subject.is_char_boundary(end).then_some(start..end),
                                        )
                                    })
                                    .collect::<Vec<_>>()
                            };

                            (!ranges.is_empty()).then_some(ranges)
                        })
                        .unwrap_or_default();
                    HighlightedLabel::from_ranges(subject, highlight_ranges)
                        .when(!is_selected, |c| c.color(Color::Muted))
                        .truncate()
                        .into_any_element()
                } else {
                    Label::new(subject)
                        .when(!is_selected, |c| c.color(Color::Muted))
                        .truncate()
                        .into_any_element()
                };

                // A history too narrow for a column of labels still has to say
                // which commit is a branch tip, so the label comes back as a
                // chip in front of the subject.
                // A commit with no label of its own still belongs to a branch,
                // and a reader who has just picked it is asking which.
                let ghost = (is_selected && layout.labels.has_a_column())
                    .then(|| self.ghost_branch(idx))
                    .flatten();

                let inline_label = (!layout.labels.has_a_column()).then(|| {
                    self.render_refs_cell(
                        idx,
                        metrics,
                        accent_color,
                        head_branch_name.as_deref(),
                        layout.labels,
                        cx,
                    )
                });

                vec![
                    match (layout.labels.has_a_column(), ghost) {
                        (true, None) => self.render_refs_cell(
                            idx,
                            metrics,
                            accent_color,
                            head_branch_name.as_deref(),
                            layout.labels,
                            cx,
                        ),
                        (true, Some((kind, name))) => {
                            self.render_ghost_chip(idx, metrics, accent_color, kind, &name, layout)
                        }
                        (false, _) => div().h(metrics.row).into_any_element(),
                    },
                    self.render_graph_cell(
                        idx,
                        metrics,
                        layout,
                        &author_name,
                        // Only for the rows on screen: a picture for a commit
                        // nobody is looking at is a request nobody asked for.
                        author_email.map(|author_email| CommitPortrait {
                            sha: commit.data.sha.to_string().into(),
                            author_email: Some(author_email),
                            remote: remote.clone(),
                        }),
                        window,
                        cx,
                    ),
                    h_flex()
                        .id(ElementId::NamedInteger("commit-subject".into(), idx as u64))
                        .debug_selector(move || format!("GRAPH_SUBJECT-{idx}"))
                        .h(metrics.row)
                        .w_full()
                        .items_center()
                        .gap_1()
                        .px_1()
                        .overflow_hidden()
                        // The tick carries the branch colour into the text, so
                        // a reader following one branch down a long history can
                        // keep to it without crossing back to the graph.
                        .child(
                            div()
                                .flex_none()
                                .w(px(2.))
                                .h(metrics.label)
                                .rounded_sm()
                                .bg(accent_color.opacity(0.7)),
                        )
                        .children(inline_label)
                        .child(
                            h_flex()
                                .flex_1()
                                .min_w_0()
                                .overflow_hidden()
                                .child(subject_label),
                        )
                        .when(shows_age && !age_has_a_column, |this| {
                            this.child(
                                Label::new(age.clone())
                                    .size(LabelSize::Small)
                                    .color(Color::Muted),
                            )
                        })
                        .into_any_element(),
                    h_flex()
                        .id(ElementId::NamedInteger("commit-age".into(), idx as u64))
                        .debug_selector(move || format!("GRAPH_AGE-{idx}"))
                        .h(metrics.row)
                        .w_full()
                        .items_center()
                        .justify_end()
                        .px_1()
                        .overflow_hidden()
                        .when(shows_age && age_has_a_column, |this| {
                            this.child(
                                Label::new(age)
                                    .size(LabelSize::Small)
                                    .color(Color::Muted)
                                    .truncate(),
                            )
                        })
                        .when_some(committed_on, |this, on| this.tooltip(Tooltip::text(on)))
                        .into_any_element(),
                ]
            })
            .collect()
    }

    fn cancel(&mut self, _: &Cancel, _window: &mut Window, cx: &mut Context<Self>) {
        self.selected_entry_idx = None;
        self.compare_against = None;
        self.picked_rows.clear();
        self.selected_commit_diff = None;
        self.selected_commit_diff_stats = None;
        self.changed_files_expanded_dirs.clear();
        cx.emit(ItemEvent::Edit);
        cx.notify();
    }

    fn select_first(&mut self, _: &SelectFirst, _window: &mut Window, cx: &mut Context<Self>) {
        self.select_entry(0, ScrollStrategy::Nearest, cx);
    }

    fn select_prev(&mut self, _: &SelectPrevious, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(selected_entry_idx) = &self.selected_entry_idx {
            self.select_entry(
                selected_entry_idx.saturating_sub(1),
                ScrollStrategy::Nearest,
                cx,
            );
        } else {
            self.select_first(&SelectFirst, window, cx);
        }
    }

    fn select_next(&mut self, _: &SelectNext, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(selected_entry_idx) = &self.selected_entry_idx {
            self.select_entry(
                selected_entry_idx
                    .saturating_add(1)
                    .min(self.graph_data.commits.len().saturating_sub(1)),
                ScrollStrategy::Nearest,
                cx,
            );
        } else {
            self.select_prev(&SelectPrevious, window, cx);
        }
    }

    fn select_last(&mut self, _: &SelectLast, _window: &mut Window, cx: &mut Context<Self>) {
        self.select_entry(
            self.graph_data.commits.len().saturating_sub(1),
            ScrollStrategy::Nearest,
            cx,
        );
    }

    fn scroll_up(&mut self, _: &ScrollUp, window: &mut Window, cx: &mut Context<Self>) {
        let step = (self.visible_row_count(window, cx) / 2).max(1);
        let target_idx = self.selected_entry_idx.unwrap_or(0).saturating_sub(step);

        self.select_entry(target_idx, ScrollStrategy::Nearest, cx);
    }

    fn scroll_down(&mut self, _: &ScrollDown, window: &mut Window, cx: &mut Context<Self>) {
        let Some(last_entry_idx) = self.graph_data.commits.len().checked_sub(1) else {
            return;
        };

        let step = (self.visible_row_count(window, cx) / 2).max(1);
        let target_idx = self
            .selected_entry_idx
            .unwrap_or(0)
            .saturating_add(step)
            .min(last_entry_idx);

        self.select_entry(target_idx, ScrollStrategy::Nearest, cx);
    }

    fn confirm(&mut self, _: &menu::Confirm, window: &mut Window, cx: &mut Context<Self>) {
        self.open_selected_commit_view(window, cx);
    }

    fn toggle_changed_files_view(
        &mut self,
        _: &ToggleChangedFilesView,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.changed_files_view_mode = self.changed_files_view_mode.toggled();
        self.changed_files_scroll_handle
            .scroll_to_item(0, ScrollStrategy::Top);
        cx.notify();
    }

    fn search(&mut self, query: SharedString, cx: &mut Context<Self>) {
        let Some(repo) = self.get_repository(cx) else {
            return;
        };

        self.search_state.matches.clear();
        self.search_state.selected_index = None;
        self.search_state.editor.update(cx, |editor, _cx| {
            editor.set_text_style_refinement(Default::default());
        });

        if query.as_str().is_empty() {
            self.search_state.state = QueryState::Empty;
            cx.notify();
            return;
        }

        let (request_tx, request_rx) = async_channel::unbounded::<Oid>();

        repo.update(cx, |repo, cx| {
            repo.search_commits(
                self.log_source.clone(),
                SearchCommitArgs {
                    query: query.clone(),
                    case_sensitive: self.search_state.case_sensitive,
                },
                request_tx,
                cx,
            );
        });

        let search_task = cx.spawn(async move |this, cx| {
            while let Ok(first_oid) = request_rx.recv().await {
                let mut pending_oids = vec![first_oid];
                while let Ok(oid) = request_rx.try_recv() {
                    pending_oids.push(oid);
                }

                this.update(cx, |this, cx| {
                    if this.search_state.selected_index.is_none() {
                        this.search_state.selected_index = Some(0);
                        this.select_commit_by_sha(first_oid, cx);
                    }

                    this.search_state.matches.extend(pending_oids);
                    cx.notify();
                })
                .ok();
            }

            this.update(cx, |this, cx| {
                if this.search_state.matches.is_empty() {
                    this.search_state.editor.update(cx, |editor, cx| {
                        editor.set_text_style_refinement(TextStyleRefinement {
                            color: Some(Color::Error.color(cx)),
                            ..Default::default()
                        });
                    });
                }
            })
            .ok();
        });

        self.search_state.state = QueryState::Confirmed((query, search_task));
        cx.emit(ItemEvent::Edit);
    }

    fn confirm_search(&mut self, _: &menu::Confirm, _window: &mut Window, cx: &mut Context<Self>) {
        let query = self.search_state.editor.read(cx).text(cx).into();
        self.search(query, cx);
    }

    fn activate_search_editor_if_focused(&self, window: &mut Window, cx: &mut Context<Self>) {
        self.search_state.editor.update(cx, |editor, cx| {
            if editor.is_focused(window) {
                editor.select_all(&Default::default(), window, cx);
                editor.show_cursor(cx);
            }
        });
    }

    fn focus_next_tab_stop(
        &mut self,
        _: &FocusNextTabStop,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        window.focus_next(cx);
        self.activate_search_editor_if_focused(window, cx);
        cx.stop_propagation();
        cx.notify();
    }

    fn focus_previous_tab_stop(
        &mut self,
        _: &FocusPreviousTabStop,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        window.focus_prev(cx);
        self.activate_search_editor_if_focused(window, cx);
        cx.stop_propagation();
        cx.notify();
    }

    fn select_entry(
        &mut self,
        idx: usize,
        scroll_strategy: ScrollStrategy,
        cx: &mut Context<Self>,
    ) {
        if self.selected_entry_idx == Some(idx) || idx >= self.graph_data.commits.len() {
            debug_assert!(
                idx < self.graph_data.commits.len(),
                "attempted to select out of bounds index: {idx}, commits.len: {}",
                self.graph_data.commits.len()
            );
            return;
        }

        self.selected_entry_idx = Some(idx);
        self.selected_commit_diff = None;
        self.selected_commit_diff_stats = None;
        self.changed_files_expanded_dirs.clear();
        self.changed_files_scroll_handle
            .scroll_to_item(0, ScrollStrategy::Top);
        self.table_interaction_state.update(cx, |state, cx| {
            state.scroll_handle.scroll_to_item(idx, scroll_strategy);
            cx.notify();
        });

        let Some(commit) = self.graph_data.commits.get(idx) else {
            return;
        };

        let Some(repository) = self.get_repository(cx) else {
            return;
        };

        let commit_message_handle = commit.data.sha;
        let diff_handle = commit.data.sha.to_string();
        let against = self
            .compare_against
            .filter(|row| *row != idx)
            .and_then(|row| self.graph_data.commits.get(row))
            .map(|commit| commit.data.sha.to_string());

        self.load_selected_commit_message(cx, &commit_message_handle, &repository);

        let diff_receiver = repository.update(cx, |repo, _| match against {
            // Two commits picked with Shift: what is different between them,
            // not what either of them changed on its own.
            Some(against) => repo.load_diff_between(against, diff_handle),
            None => repo.load_commit_diff(diff_handle),
        });

        self._commit_diff_task = Some(cx.spawn(async move |this, cx| {
            if let Ok(Ok(diff)) = diff_receiver.await {
                this.update(cx, |this, cx| {
                    let stats = compute_diff_stats(&diff);
                    this.selected_commit_diff = Some(diff);
                    this.selected_commit_diff_stats = Some(stats);
                    cx.notify();
                })
                .ok();
            }
        }));

        cx.emit(ItemEvent::Edit);
        cx.notify();
    }

    fn load_selected_commit_message(
        &mut self,
        cx: &mut Context<'_, Self>,
        sha: &Oid,
        repository: &Entity<Repository>,
    ) {
        if self
            .selected_commit_message
            .as_ref()
            .is_some_and(|old| old.sha == *sha)
        {
            return;
        }

        self._selected_commit_message_task = None;
        match repository.update(cx, |repo, cx| {
            repo.fetch_commit_data(*sha, true, cx).clone()
        }) {
            CommitDataState::Loaded(commit_data) => {
                self.set_selected_commit_message(cx, commit_data.sha, commit_data.message.clone());
            }
            CommitDataState::Loading(Some(receiver)) => {
                self._selected_commit_message_task = Some(cx.spawn(async move |this, cx| {
                    if let Ok(commit_data) = receiver.await {
                        this.update(cx, |this, cx| {
                            this.set_selected_commit_message(
                                cx,
                                commit_data.sha,
                                commit_data.message.clone(),
                            );
                        })
                        .log_err();
                    }
                }))
            }
            _ => {
                debug_panic!(
                    "Fetched commit data asynchronously, but was not given a listener or cached commit data."
                );
            }
        };
    }

    fn set_selected_commit_message(
        &mut self,
        cx: &mut Context<'_, GitGraph>,
        sha: Oid,
        message: SharedString,
    ) {
        let languages = self
            .workspace
            .read_with(cx, |workspace, cx| {
                workspace.project().read(cx).languages().clone()
            })
            .log_err();
        self.selected_commit_message = Some(DetailPanelCommitMessage {
            sha,
            message: cx.new(|cx| Markdown::new(message, languages, None, cx)),
            scroll_handle: ScrollHandle::new(),
        });
        self._selected_commit_message_task = None;
        cx.notify();
    }

    fn select_previous_match(&mut self, cx: &mut Context<Self>) {
        if self.search_state.matches.is_empty() {
            return;
        }

        let mut prev_selection = self.search_state.selected_index.unwrap_or_default();

        if prev_selection == 0 {
            prev_selection = self.search_state.matches.len() - 1;
        } else {
            prev_selection -= 1;
        }

        let Some(&oid) = self.search_state.matches.get_index(prev_selection) else {
            return;
        };

        self.search_state.selected_index = Some(prev_selection);
        self.select_commit_by_sha(oid, cx);
    }

    fn select_next_match(&mut self, cx: &mut Context<Self>) {
        if self.search_state.matches.is_empty() {
            return;
        }

        let mut next_selection = self
            .search_state
            .selected_index
            .map(|index| index + 1)
            .unwrap_or_default();

        if next_selection >= self.search_state.matches.len() {
            next_selection = 0;
        }

        let Some(&oid) = self.search_state.matches.get_index(next_selection) else {
            return;
        };

        self.search_state.selected_index = Some(next_selection);
        self.select_commit_by_sha(oid, cx);
    }

    pub fn set_repo_id(&mut self, repo_id: RepositoryId, cx: &mut Context<Self>) {
        if repo_id != self.repo_id
            && self
                .git_store
                .read(cx)
                .repositories()
                .contains_key(&repo_id)
        {
            self.repo_id = repo_id;
            self.invalidate_state(cx);
        }
    }

    pub fn select_commit_by_sha(&mut self, sha: impl TryInto<Oid>, cx: &mut Context<Self>) {
        fn inner(this: &mut GitGraph, oid: Oid, cx: &mut Context<GitGraph>) {
            let Some(selected_repository) = this.get_repository(cx) else {
                return;
            };

            let Some(index) = selected_repository
                .read(cx)
                .get_graph_data(this.log_source.clone(), this.log_order)
                .and_then(|data| data.commit_oid_to_index.get(&oid))
                .copied()
            else {
                this.pending_select_sha = Some(oid);
                return;
            };

            this.pending_select_sha = None;
            this.select_entry(index, ScrollStrategy::Center, cx);
        }

        if let Ok(oid) = sha.try_into() {
            inner(self, oid, cx);
        }
    }

    fn open_selected_commit_view(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(selected_entry_index) = self.selected_entry_idx else {
            return;
        };

        self.open_commit_view(selected_entry_index, window, cx);
    }

    fn open_commit_view(
        &mut self,
        entry_index: usize,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(commit_entry) = self.graph_data.commits.get(entry_index) else {
            return;
        };

        let Some(repository) = self.get_repository(cx) else {
            return;
        };

        CommitView::open(
            commit_entry.data.sha.to_string(),
            repository.downgrade(),
            self.workspace.clone(),
            None,
            None,
            window,
            cx,
        );
    }

    fn copy_commit_sha(&mut self, entry_index: usize, cx: &mut Context<Self>) {
        let Some(commit) = self.graph_data.commits.get(entry_index) else {
            return;
        };
        cx.write_to_clipboard(ClipboardItem::new_string(commit.data.sha.to_string()));
    }

    fn copy_selected_commit_sha(
        &mut self,
        _: &CopyCommitSha,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(selected_entry_index) = self.selected_entry_idx else {
            return;
        };
        self.copy_commit_sha(selected_entry_index, cx);
    }

    fn copy_commit_tag(&mut self, entry_index: usize, window: &mut Window, cx: &mut Context<Self>) {
        let Some(commit) = self.graph_data.commits.get(entry_index) else {
            return;
        };

        let tag_names = commit
            .data
            .tag_names()
            .into_iter()
            .map(|tag_name| SharedString::from(tag_name.to_string()))
            .collect::<Vec<_>>();

        match tag_names.as_slice() {
            [] => {}
            [tag_name] => cx.write_to_clipboard(ClipboardItem::new_string(tag_name.to_string())),
            _ => {
                self.workspace
                    .update(cx, |workspace, cx| {
                        workspace.toggle_modal(window, cx, |window, cx| {
                            CommitTagPicker::new(tag_names, window, cx)
                        });
                    })
                    .ok();
            }
        }
    }

    fn copy_selected_commit_tag(
        &mut self,
        _: &CopyCommitTag,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(selected_entry_index) = self.selected_entry_idx else {
            return;
        };
        self.copy_commit_tag(selected_entry_index, window, cx);
    }

    fn deploy_entry_context_menu(
        &mut self,
        position: Point<Pixels>,
        index: usize,
        ref_name: Option<SharedString>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(commit) = self.graph_data.commits.get(index) else {
            return;
        };
        let repository = self
            .get_repository(cx)
            .map(|repository| repository.downgrade());
        let context_menu = commit_context_menu(
            CommitContextMenuData {
                sha: commit.data.sha,
                tag_names: commit
                    .data
                    .tag_names()
                    .into_iter()
                    .map(|tag_name| SharedString::from(tag_name.to_string()))
                    .collect(),
                selected: self.picked_commits(index),
            },
            CommitContextMenuSource::GitGraph,
            ref_name,
            self.focus_handle.clone(),
            repository,
            self.workspace.clone(),
            Some(Rc::new({
                let graph = cx.weak_entity();
                move |name: SharedString, cx: &mut App| {
                    graph
                        .update(cx, |graph, cx| graph.toggle_solo(name.clone(), cx))
                        .ok();
                }
            })),
            window,
            cx,
        );
        self.set_context_menu(context_menu, position, Some(index), window, cx);
    }

    fn set_context_menu(
        &mut self,
        context_menu: Entity<ContextMenu>,
        position: Point<Pixels>,
        target_entry_index: Option<usize>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        window.focus(&context_menu.focus_handle(cx), cx);

        let subscription = cx.subscribe_in(
            &context_menu,
            window,
            |this, _, _: &DismissEvent, window, cx| {
                if this.context_menu.as_ref().is_some_and(|context_menu| {
                    context_menu
                        .menu
                        .focus_handle(cx)
                        .contains_focused(window, cx)
                }) {
                    cx.focus_self(window);
                }
                this.context_menu.take();
                cx.notify();
            },
        );
        self.context_menu = Some(GitGraphContextMenu {
            menu: context_menu,
            position,
            target_entry_index,
            _subscription: subscription,
        });
        cx.notify();
    }

    fn render_search_bar(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let color = cx.theme().colors();
        let query_focus_handle = self
            .search_state
            .editor
            .focus_handle(cx)
            .tab_index(1)
            .tab_stop(true);
        let search_options = {
            let mut options = SearchOptions::NONE;
            options.set(
                SearchOptions::CASE_SENSITIVE,
                self.search_state.case_sensitive,
            );
            options
        };

        h_flex()
            .key_context("GitGraphSearchBar")
            .tab_index(1)
            .tab_group()
            .tab_stop(false)
            .w_full()
            .p_1p5()
            .gap_1p5()
            .border_b_1()
            .border_color(color.border_variant)
            .child(
                h_flex()
                    .h_8()
                    .flex_1()
                    .min_w_0()
                    .px_1p5()
                    .gap_1()
                    .track_focus(&query_focus_handle)
                    .border_1()
                    .border_color(color.border_variant)
                    .rounded_md()
                    .bg(color.toolbar_background)
                    .on_action(cx.listener(Self::confirm_search))
                    .child(self.search_state.editor.clone())
                    .child(SearchOption::CaseSensitive.as_button(
                        search_options,
                        SearchSource::Buffer,
                        query_focus_handle,
                    )),
            )
            .child(self.render_label_switches(cx))
            .child(
                h_flex()
                    .min_w_64()
                    .gap_1()
                    .child({
                        let focus_handle = self.focus_handle.clone();
                        IconButton::new("git-graph-search-prev", IconName::ChevronLeft)
                            .shape(ui::IconButtonShape::Square)
                            .icon_size(IconSize::Small)
                            .tooltip(move |_, cx| {
                                Tooltip::for_action_in(
                                    "Select Previous Match",
                                    &SelectPreviousMatch,
                                    &focus_handle,
                                    cx,
                                )
                            })
                            .map(|this| {
                                if self.search_state.matches.is_empty() {
                                    this.disabled(true)
                                } else {
                                    this.disabled(false).on_click(cx.listener(|this, _, _, cx| {
                                        this.select_previous_match(cx);
                                    }))
                                }
                            })
                    })
                    .child({
                        let focus_handle = self.focus_handle.clone();
                        IconButton::new("git-graph-search-next", IconName::ChevronRight)
                            .shape(ui::IconButtonShape::Square)
                            .icon_size(IconSize::Small)
                            .tooltip(move |_, cx| {
                                Tooltip::for_action_in(
                                    "Select Next Match",
                                    &SelectNextMatch,
                                    &focus_handle,
                                    cx,
                                )
                            })
                            .map(|this| {
                                if self.search_state.matches.is_empty() {
                                    this.disabled(true)
                                } else {
                                    this.disabled(false).on_click(cx.listener(|this, _, _, cx| {
                                        this.select_next_match(cx);
                                    }))
                                }
                            })
                    })
                    .child(
                        h_flex()
                            .gap_1p5()
                            .child(
                                Label::new(format!(
                                    "{}/{}",
                                    self.search_state
                                        .selected_index
                                        .map(|index| index + 1)
                                        .unwrap_or(0),
                                    self.search_state.matches.len()
                                ))
                                .size(LabelSize::Small)
                                .when(self.search_state.matches.is_empty(), |this| {
                                    this.color(Color::Disabled)
                                }),
                            )
                            .when(
                                matches!(
                                    &self.search_state.state,
                                    QueryState::Confirmed((_, task)) if !task.is_ready()
                                ),
                                |this| {
                                    this.child(
                                        Icon::new(IconName::ArrowCircle)
                                            .color(Color::Accent)
                                            .size(IconSize::Small)
                                            .with_rotate_animation(2)
                                            .into_any_element(),
                                    )
                                },
                            ),
                    ),
            )
    }

    fn render_loading_spinner(&self, cx: &App) -> AnyElement {
        let rems = TextSize::Large.rems(cx);
        Icon::new(IconName::LoadCircle)
            .size(IconSize::Custom(rems))
            .color(Color::Accent)
            .with_rotate_animation(3)
            .into_any_element()
    }

    fn render_commit_detail_panel(
        &self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let Some(selected_idx) = self.selected_entry_idx else {
            return Empty.into_any_element();
        };

        let Some(commit_entry) = self.graph_data.commits.get(selected_idx) else {
            return Empty.into_any_element();
        };

        let Some(repository) = self.get_repository(cx) else {
            return Empty.into_any_element();
        };

        let data = repository.update(cx, |repository, cx| {
            repository
                .fetch_commit_data(commit_entry.data.sha, false, cx)
                .clone()
        });

        let full_sha: SharedString = commit_entry.data.sha.to_string().into();

        let head_branch_name: Option<SharedString> = repository
            .read(cx)
            .snapshot()
            .branch
            .as_ref()
            .map(|branch| SharedString::from(branch.name().to_string()));

        let accent_colors = cx.theme().accents();
        let accent_color = accent_colors
            .0
            .get(commit_entry.color_idx)
            .copied()
            .unwrap_or_else(|| accent_colors.0.first().copied().unwrap_or_default());

        let (author_name, author_email, commit_timestamp) = match &data {
            CommitDataState::Loaded(data) => (
                data.author_name.clone(),
                data.author_email.clone(),
                Some(data.commit_timestamp),
            ),
            CommitDataState::Loading(_) => ("Loading…".into(), "".into(), None),
        };

        let date_string = commit_timestamp
            .and_then(|ts| OffsetDateTime::from_unix_timestamp(ts).ok())
            .map(|datetime| {
                let local_offset = UtcOffset::current_local_offset().unwrap_or(UtcOffset::UTC);
                let local_datetime = datetime.to_offset(local_offset);
                let format = time::format_description::parse_borrowed::<1>(
                    "[month repr:short] [day], [year]",
                )
                .ok();
                format
                    .and_then(|f| local_datetime.format(&f).ok())
                    .unwrap_or_default()
            })
            .unwrap_or_default();

        let remote = repository.update(cx, |repo, cx| {
            let remote_url = repo.default_remote_url()?;
            let provider_registry = GitHostingProviderRegistry::default_global(cx);
            let (provider, parsed) = parse_git_remote_url(provider_registry, &remote_url)?;
            Some(GitRemote {
                host: provider,
                owner: parsed.owner.into(),
                repo: parsed.repo.into(),
            })
        });

        let avatar = {
            let author_email_for_avatar = if author_email.is_empty() {
                None
            } else {
                Some(author_email.clone())
            };

            CommitAvatar::new(&full_sha, author_email_for_avatar, remote.as_ref())
                .size(px(32.))
                .render(window, cx)
        };

        let compared_with = self
            .compare_against
            .filter(|row| *row != selected_idx)
            .and_then(|row| self.graph_data.commits.get(row))
            .map(|commit| commit.data.sha.display_short());
        let changed_files_count = self
            .selected_commit_diff
            .as_ref()
            .map(|diff| diff.files.len())
            .unwrap_or(0);

        let (total_lines_added, total_lines_removed) =
            self.selected_commit_diff_stats.unwrap_or((0, 0));

        let changed_file_entries: Vec<ChangedFileEntry> = self
            .selected_commit_diff
            .as_ref()
            .map(|diff| {
                let mut files = diff.files.iter().collect::<Vec<_>>();
                if !self.changed_files_view_mode.is_tree() {
                    files.sort_by_key(|file| file.status());
                }
                files
                    .into_iter()
                    .map(|file| ChangedFileEntry::from_commit_file(file, cx))
                    .collect()
            })
            .unwrap_or_default();
        let changed_file_entries = Rc::new(changed_file_entries);
        let tree_entries: Rc<Vec<ChangedFileTreeEntry>> = if self.changed_files_view_mode.is_tree()
        {
            Rc::new(build_changed_file_tree_entries(
                changed_file_entries.as_ref().clone(),
                &self.changed_files_expanded_dirs,
            ))
        } else {
            Rc::default()
        };

        let is_tree_view = self.changed_files_view_mode.is_tree();
        let view_toggle = IconButton::new("toggle-changed-files-view", IconName::ListTree)
            .icon_size(IconSize::Small)
            .toggle_state(self.changed_files_view_mode.is_tree())
            .tooltip({
                let tooltip = if is_tree_view {
                    "Show Flat View"
                } else {
                    "Show Tree View"
                };
                move |_, cx| Tooltip::for_action(tooltip, &ToggleChangedFilesView, cx)
            })
            .on_click(cx.listener(|this, _, _window, cx| {
                this.changed_files_view_mode = this.changed_files_view_mode.toggled();
                this.changed_files_scroll_handle
                    .scroll_to_item(0, ScrollStrategy::Top);
                cx.notify();
            }));

        v_flex()
            .min_w(px(300.))
            .h_full()
            .bg(cx.theme().colors().editor_background)
            .flex_basis(DefiniteLength::Fraction(
                self.commit_details_split_state.read(cx).right_ratio(),
            ))
            .child(
                v_flex()
                    .relative()
                    .w_full()
                    .p_2()
                    .gap_2()
                    .child(
                        div().absolute().top_2().right_2().child(
                            IconButton::new("close-detail", IconName::Close)
                                .icon_size(IconSize::Small)
                                .on_click(cx.listener(move |this, _, _, cx| {
                                    this.selected_entry_idx = None;
                                    this.selected_commit_diff = None;
                                    this.selected_commit_diff_stats = None;
                                    this.selected_commit_message = None;
                                    this._selected_commit_message_task = None;
                                    this.changed_files_expanded_dirs.clear();
                                    this._commit_diff_task = None;
                                    cx.notify();
                                })),
                        ),
                    )
                    .child(
                        v_flex()
                            .py_1()
                            .w_full()
                            .items_center()
                            .child(avatar)
                            .child(Label::new(author_name).mt_1p5())
                            .child(
                                Label::new(date_string)
                                    .color(Color::Muted)
                                    .size(LabelSize::Small),
                            ),
                    )
                    .children({
                        let refs = self.refs_of(selected_idx, head_branch_name.as_deref());
                        (!refs.is_empty()).then(|| {
                            h_flex()
                                .gap_1()
                                .flex_wrap()
                                .justify_center()
                                .children(refs.iter().map(|(kind, name)| {
                                    self.render_ref_chip(
                                        *kind,
                                        name,
                                        accent_color,
                                        LabelMode::Full,
                                        selected_idx,
                                        cx,
                                    )
                                }))
                        })
                    })
                    .child(
                        v_flex()
                            .ml_neg_1()
                            .gap_1p5()
                            .when(!author_email.is_empty(), |this| {
                                let copied_state: Entity<CopiedState> = window.use_keyed_state(
                                    "author-email-copy",
                                    cx,
                                    CopiedState::new,
                                );
                                let is_copied = copied_state.read(cx).is_copied();

                                let (icon, icon_color, tooltip_label) = if is_copied {
                                    (IconName::Check, Color::Success, "Email Copied!")
                                } else {
                                    (IconName::Envelope, Color::Muted, "Copy Email")
                                };

                                let copy_email = author_email.clone();
                                let author_email_for_tooltip = author_email.clone();

                                this.child(
                                    Button::new("author-email-copy", author_email.clone())
                                        .start_icon(
                                            Icon::new(icon).size(IconSize::Small).color(icon_color),
                                        )
                                        .label_size(LabelSize::Small)
                                        .truncate(true)
                                        .color(Color::Muted)
                                        .tooltip(move |_, cx| {
                                            Tooltip::with_meta(
                                                tooltip_label,
                                                None,
                                                author_email_for_tooltip.clone(),
                                                cx,
                                            )
                                        })
                                        .on_click(move |_, _, cx| {
                                            copied_state.update(cx, |state, _cx| {
                                                state.mark_copied();
                                            });
                                            cx.write_to_clipboard(ClipboardItem::new_string(
                                                copy_email.to_string(),
                                            ));
                                            let state_id = copied_state.entity_id();
                                            cx.spawn(async move |cx| {
                                                cx.background_executor()
                                                    .timer(COPIED_STATE_DURATION)
                                                    .await;
                                                cx.update(|cx| {
                                                    cx.notify(state_id);
                                                })
                                            })
                                            .detach();
                                        }),
                                )
                            })
                            .child({
                                let copy_sha = full_sha.clone();
                                let copied_state: Entity<CopiedState> =
                                    window.use_keyed_state("sha-copy", cx, CopiedState::new);
                                let is_copied = copied_state.read(cx).is_copied();

                                let (icon, icon_color, tooltip_label) = if is_copied {
                                    (IconName::Check, Color::Success, "Commit SHA Copied!")
                                } else {
                                    (IconName::Hash, Color::Muted, "Copy Commit SHA")
                                };

                                Button::new("sha-button", &full_sha)
                                    .start_icon(
                                        Icon::new(icon).size(IconSize::Small).color(icon_color),
                                    )
                                    .label_size(LabelSize::Small)
                                    .truncate(true)
                                    .color(Color::Muted)
                                    .tooltip({
                                        let full_sha = full_sha.clone();
                                        move |_, cx| {
                                            Tooltip::with_meta(
                                                tooltip_label,
                                                None,
                                                full_sha.clone(),
                                                cx,
                                            )
                                        }
                                    })
                                    .on_click(move |_, _, cx| {
                                        copied_state.update(cx, |state, _cx| {
                                            state.mark_copied();
                                        });
                                        cx.write_to_clipboard(ClipboardItem::new_string(
                                            copy_sha.to_string(),
                                        ));
                                        let state_id = copied_state.entity_id();
                                        cx.spawn(async move |cx| {
                                            cx.background_executor()
                                                .timer(COPIED_STATE_DURATION)
                                                .await;
                                            cx.update(|cx| {
                                                cx.notify(state_id);
                                            })
                                        })
                                        .detach();
                                    })
                            })
                            .when_some(remote.clone(), |this, remote| {
                                let provider_name = remote.host.name();
                                let icon = crate::get_provider_icon(provider_name.as_str());
                                let parsed_remote = ParsedGitRemote {
                                    owner: remote.owner.as_ref().into(),
                                    repo: remote.repo.as_ref().into(),
                                };
                                let params = BuildCommitPermalinkParams {
                                    sha: full_sha.as_ref(),
                                };
                                let url = remote
                                    .host
                                    .build_commit_permalink(&parsed_remote, params)
                                    .to_string();

                                this.child(
                                    Button::new(
                                        "view-on-provider",
                                        format!("View on {}", provider_name),
                                    )
                                    .start_icon(
                                        Icon::new(icon).size(IconSize::Small).color(Color::Muted),
                                    )
                                    .label_size(LabelSize::Small)
                                    .truncate(true)
                                    .color(Color::Muted)
                                    .on_click(
                                        move |_, _, cx| {
                                            cx.open_url(&url);
                                        },
                                    ),
                                )
                            }),
                    ),
            )
            .child(Divider::horizontal())
            .child(self.render_commit_message(window, cx))
            .child(Divider::horizontal())
            .child(
                v_flex()
                    .min_w_0()
                    .flex_1()
                    .overflow_hidden()
                    .child(
                        h_flex()
                            .p_2()
                            .pr_3()
                            .pb_1()
                            .gap_1()
                            .w_full()
                            .justify_between()
                            .child(
                                h_flex()
                                    .gap_1()
                                    .child(
                                        Label::new(format!(
                                            "{} Changed {}",
                                            changed_files_count,
                                            if changed_files_count == 1 {
                                                "File"
                                            } else {
                                                "Files"
                                            }
                                        ))
                                        .size(LabelSize::Small)
                                        .color(Color::Muted),
                                    )
                                    // A comparison has to say what it is
                                    // comparing, or it reads as the changes of
                                    // the commit that happens to be selected.
                                    .children(compared_with.map(|other| {
                                        h_flex().gap_1().child(Divider::vertical()).child(
                                            Label::new(format!("since {other}"))
                                                .size(LabelSize::Small)
                                                .color(Color::Accent),
                                        )
                                    }))
                                    .child(Divider::vertical())
                                    .child(view_toggle),
                            )
                            .child(DiffStat::new(
                                "commit-diff-stat",
                                total_lines_added,
                                total_lines_removed,
                            )),
                    )
                    .child(
                        div()
                            .id("changed-files-container")
                            .flex_1()
                            .min_h_0()
                            .child({
                                let flat_entries = changed_file_entries;

                                let entry_count = if is_tree_view {
                                    tree_entries.len()
                                } else {
                                    flat_entries.len()
                                };
                                let commit_sha = full_sha.clone();
                                let repository = repository.downgrade();
                                let workspace = self.workspace.clone();
                                let git_graph = cx.weak_entity();
                                let indent_tree_entries = tree_entries.clone();

                                uniform_list(
                                    "changed-files-list",
                                    entry_count,
                                    move |range, _window, cx| {
                                        range
                                            .map(|ix| {
                                                if is_tree_view {
                                                    match &tree_entries[ix] {
                                                        ChangedFileTreeEntry::Directory(entry) => {
                                                            entry.render(ix, git_graph.clone(), cx)
                                                        }
                                                        ChangedFileTreeEntry::File(entry) => {
                                                            entry.entry.render(
                                                                ix,
                                                                entry.depth,
                                                                None,
                                                                commit_sha.clone(),
                                                                repository.clone(),
                                                                workspace.clone(),
                                                                cx,
                                                            )
                                                        }
                                                    }
                                                } else {
                                                    let directory_label = (!flat_entries[ix]
                                                        .dir_path
                                                        .is_empty())
                                                    .then(|| flat_entries[ix].dir_path.clone());
                                                    flat_entries[ix].render(
                                                        ix,
                                                        0,
                                                        directory_label,
                                                        commit_sha.clone(),
                                                        repository.clone(),
                                                        workspace.clone(),
                                                        cx,
                                                    )
                                                }
                                            })
                                            .collect()
                                    },
                                )
                                .when(is_tree_view, |list| {
                                    list.with_decoration(
                                        ui::indent_guides(
                                            px(TREE_INDENT),
                                            IndentGuideColors::panel(cx),
                                        )
                                        .with_left_offset(
                                            ui::LIST_ITEM_INDENT_GUIDE_LEFT_OFFSET - px(2.),
                                        )
                                        .with_compute_indents_fn(
                                            cx.entity(),
                                            move |_, range, _window, _cx| {
                                                range
                                                    .map(|ix| match indent_tree_entries.get(ix) {
                                                        Some(ChangedFileTreeEntry::Directory(
                                                            entry,
                                                        )) => entry.depth,
                                                        Some(ChangedFileTreeEntry::File(entry)) => {
                                                            entry.depth
                                                        }
                                                        None => 0,
                                                    })
                                                    .collect()
                                            },
                                        ),
                                    )
                                })
                                .size_full()
                                .track_scroll(&self.changed_files_scroll_handle)
                            })
                            .vertical_scrollbar_for(&self.changed_files_scroll_handle, window, cx),
                    ),
            )
            .child(Divider::horizontal())
            .child(
                h_flex().p_1p5().w_full().child(
                    Button::new("view-commit", "View Commit")
                        .full_width()
                        .start_icon(
                            Icon::new(IconName::GitCommit)
                                .size(IconSize::Small)
                                .color(Color::Muted),
                        )
                        .style(ButtonStyle::OutlinedGhost)
                        .on_click(cx.listener(|this, _, window, cx| {
                            this.open_selected_commit_view(window, cx);
                        })),
                ),
            )
            .into_any_element()
    }

    fn handle_entry_click(
        &mut self,
        entry_idx: usize,
        event: &ClickEvent,
        scroll_strategy: ScrollStrategy,
        focus_handle: Option<&FocusHandle>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        // Right-clicks open the context menu, not the details panel.
        if event.is_right_click() {
            return;
        }

        if let Some(focus_handle) = focus_handle {
            focus_handle.focus(window, cx);
        }

        let modifiers = event.modifiers();
        match (modifiers.shift, modifiers.secondary()) {
            // Shift reaches from the selection to here, which is both the run
            // of commits a command should act on and the pair the card compares.
            (true, _) => {
                self.compare_against = self.selected_entry_idx.filter(|at| *at != entry_idx);
                self.pick_through(entry_idx);
            }
            // Ctrl (or Cmd) adds one commit to what is picked without moving
            // the run.
            (false, true) => {
                self.compare_against = None;
                self.toggle_picked(entry_idx);
            }
            (false, false) => {
                self.compare_against = None;
                self.picked_rows.clear();
            }
        }

        self.select_entry(entry_idx, scroll_strategy, cx);

        if event.click_count() >= 2 {
            self.open_commit_view(entry_idx, window, cx);
        }
    }

    fn handle_entry_secondary_mouse_down(
        &mut self,
        entry_idx: usize,
        event: &MouseDownEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.deploy_entry_context_menu(event.position, entry_idx, None, window, cx);
        cx.stop_propagation();
    }

    fn commit_count_and_loading_state(&mut self, cx: &mut Context<Self>) -> (usize, bool) {
        match self.graph_data.max_commit_count {
            AllCommitCount::FullyLoaded(count) => (count, false),
            AllCommitCount::Loading(count) => {
                let is_loading = self
                    .get_repository(cx)
                    .map(|repository| {
                        repository.update(cx, |repository, cx| {
                            repository
                                .graph_data(self.log_source.clone(), self.log_order, 0..0, cx)
                                .is_loading
                        })
                    })
                    .unwrap_or(false);

                (count, is_loading)
            }
            AllCommitCount::NotLoaded => {
                let (commit_count, is_loading) = if let Some(repository) = self.get_repository(cx) {
                    repository.update(cx, |repository, cx| {
                        // Start loading the graph data if we haven't started already
                        let GraphDataResponse {
                            commits,
                            is_loading,
                            error: _,
                        } = repository.graph_data(
                            self.log_source.clone(),
                            self.log_order,
                            0..usize::MAX,
                            cx,
                        );
                        self.graph_data.add_commits(commits);
                        (commits.len(), is_loading)
                    })
                } else {
                    (0, false)
                };

                (commit_count, is_loading)
            }
        }
    }

    fn render_commit_view_resize_handle(
        &self,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        div()
            .id("commit-view-split-resize-container")
            .relative()
            .h_full()
            .flex_shrink_0()
            .w(px(1.))
            .bg(cx.theme().colors().border_variant)
            .child(
                div()
                    .id("commit-view-split-resize-handle")
                    .absolute()
                    .left(px(-RESIZE_HANDLE_WIDTH / 2.0))
                    .w(px(RESIZE_HANDLE_WIDTH))
                    .h_full()
                    .cursor_col_resize()
                    .block_mouse_except_scroll()
                    .on_click(cx.listener(|this, event: &ClickEvent, _window, cx| {
                        if event.click_count() >= 2 {
                            this.commit_details_split_state.update(cx, |state, _| {
                                state.on_double_click();
                            });
                        }
                        cx.stop_propagation();
                    }))
                    .on_drag(DraggedSplitHandle, |_, _, _, cx| cx.new(|_| gpui::Empty)),
            )
            .into_any_element()
    }

    fn render_commit_message(
        &self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let Some(DetailPanelCommitMessage {
            message,
            scroll_handle,
            ..
        }) = self.selected_commit_message.as_ref()
        else {
            return Empty.into_any_element();
        };

        let message_style = editor::hover_markdown_style(window, cx);
        let rem_size = window.rem_size();
        let line_height = message_style
            .base_text_style
            .line_height_in_pixels(rem_size);

        div()
            // Using grid over flexbox because the structure of this side
            // panel prvents taffy from calculating a concrete width correctly,
            // which causes problems with text reflow when using flexbox.
            // grid, on the other hand, doesn't appear to give taffy the same
            // problems.
            .w_full()
            .py_2()
            .pl_2()
            .grid()
            .grid_cols(1)
            .gap_1()
            .child(
                div()
                    .relative()
                    .w_full()
                    .child(
                        div()
                            .id("commit-message")
                            .text_sm()
                            .w_full()
                            .max_h(line_height * 12.)
                            .overflow_y_scroll()
                            .track_scroll(scroll_handle)
                            .child(MarkdownElement::new(message.clone(), message_style)),
                    )
                    .vertical_scrollbar_for(scroll_handle, window, cx),
            )
            .into_any_element()
    }
}

impl Render for GitGraph {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        // This happens when we changed branches, we should refresh our search as well
        if let QueryState::Pending(query) = &mut self.search_state.state {
            let query = std::mem::take(query);
            self.search_state.state = QueryState::Empty;
            self.search(query, cx);
        }
        let (commit_count, is_loading) = self.commit_count_and_loading_state(cx);

        let error = self.get_repository(cx).and_then(|repo| {
            repo.read(cx)
                .get_graph_data(self.log_source.clone(), self.log_order)
                .and_then(|data| data.error.clone())
        });

        let content = if commit_count == 0 {
            let message = if let Some(error) = &error {
                format!("Error loading: {}", error)
            } else if is_loading {
                "Loading".to_string()
            } else {
                "No commits found".to_string()
            };
            let label = Label::new(message)
                .color(Color::Muted)
                .size(LabelSize::Large);

            h_flex()
                .size_full()
                .gap_1()
                .justify_center()
                .child(label)
                .when(is_loading && error.is_none(), |this| {
                    this.child(self.render_loading_spinner(cx))
                })
        } else {
            let layout = self.history_layout(window, cx);
            let rows_in_the_list = self.rows_in_the_list(commit_count);
            let column_filter = column_mask(layout);
            let table_width_config =
                ColumnWidthConfig::explicit(self.table_column_widths(window, cx, layout));

            let row_height = Self::row_height(window, cx);
            let selected_entry_idx = self.selected_entry_idx;
            let hovered_entry_idx = self.hovered_entry_idx;
            let context_menu_target_index = self
                .context_menu
                .as_ref()
                .and_then(|menu| menu.target_entry_index);
            let weak_self = cx.weak_entity();
            let focus_handle = self.focus_handle.clone();
            let table_focus_handle = self.table_interaction_state.read(cx).focus_handle.clone();
            let lit_branch = self.lit_branch.clone();
            let picked_rows = self.picked_rows.clone();

            let commits_table = Table::new(TABLE_COLUMN_COUNT)
                .interactable(&self.table_interaction_state)
                .disable_base_style()
                .hide_row_borders()
                .hide_row_hover()
                .width_config(table_width_config)
                .column_filter(column_filter)
                .map_row(move |(in_the_list, row), window, cx| {
                    // The list counts the rows it was given; everything else in
                    // the view counts rows of the history.
                    let index = weak_self
                        .upgrade()
                        .and_then(|graph| graph.read(cx).row_at(in_the_list))
                        .unwrap_or(in_the_list);
                    let is_selected = selected_entry_idx == Some(index);
                    let is_hovered = hovered_entry_idx == Some(index);
                    let is_context_menu_target = context_menu_target_index == Some(index);
                    let table_focus_handle = table_focus_handle.clone();
                    let is_focused =
                        focus_handle.is_focused(window) || table_focus_handle.is_focused(window);
                    let weak = weak_self.clone();
                    let weak_for_hover = weak.clone();
                    let weak_for_context_menu = weak.clone();

                    let hover_bg = cx.theme().colors().element_hover.opacity(0.6);
                    let selected_bg = if is_focused {
                        cx.theme().colors().element_selected
                    } else {
                        cx.theme().colors().element_hover
                    };
                    // Faint enough to be read as "which branch" rather than as
                    // "look here": the selection and the pointer are what say
                    // look here, and they paint over this.
                    let band = weak.upgrade().and_then(|graph| {
                        let commit = graph.read(cx).graph_data.commits.get(index)?.clone();
                        Some(
                            cx.theme()
                                .accents()
                                .color_for_index(commit.color_idx as u32)
                                .opacity(0.05),
                        )
                    });

                    let in_the_question = lit_branch
                        .as_ref()
                        .map(|lit| lit.contains(&index))
                        .unwrap_or(true);

                    let is_picked = picked_rows.contains(&index);

                    row.h(row_height)
                        .cursor_pointer()
                        .when(is_picked && !is_selected, |row| row.bg(hover_bg))
                        // Asked about one branch, the rest of the history steps
                        // back. Only a label can ask: a pointer crossing the
                        // rows must never make the whole list flash.
                        .when(!in_the_question, |row| row.opacity(0.35))
                        .when_some(band, |row, band| row.bg(band))
                        .when(is_selected || is_context_menu_target, |row| {
                            row.bg(selected_bg)
                        })
                        .when(
                            is_hovered && !is_selected && !is_context_menu_target,
                            |row| row.bg(hover_bg),
                        )
                        .on_hover(move |&is_hovered, _, cx| {
                            weak_for_hover
                                .update(cx, |this, cx| {
                                    if is_hovered {
                                        if this.hovered_entry_idx != Some(index) {
                                            this.hovered_entry_idx = Some(index);
                                            cx.notify();
                                        }
                                    } else if this.hovered_entry_idx == Some(index) {
                                        this.hovered_entry_idx = None;
                                        cx.notify();
                                    }
                                })
                                .ok();
                        })
                        .on_click(move |event, window, cx| {
                            weak.update(cx, |this, cx| {
                                this.handle_entry_click(
                                    index,
                                    event,
                                    ScrollStrategy::Center,
                                    Some(&table_focus_handle),
                                    window,
                                    cx,
                                );
                            })
                            .ok();
                        })
                        .on_mouse_down(
                            MouseButton::Right,
                            move |event: &MouseDownEvent, window, cx| {
                                weak_for_context_menu
                                    .update(cx, |this, cx| {
                                        this.handle_entry_secondary_mouse_down(
                                            index, event, window, cx,
                                        );
                                    })
                                    .ok();
                            },
                        )
                        .into_any_element()
                })
                .uniform_list(
                    "git-graph-commits",
                    rows_in_the_list,
                    cx.processor(Self::render_table_rows),
                );

            h_flex()
                .size_full()
                .child(
                    v_flex()
                        .flex_1()
                        .min_w_0()
                        .size_full()
                        .children(self.render_working_tree_row(layout, window, cx))
                        .child(
                            div()
                                .relative()
                                .flex_1()
                                .w_full()
                                .overflow_hidden()
                                .on_scroll_wheel(cx.listener(Self::handle_lane_scroll))
                                .child(self.measure_history_width())
                                .child(
                                    div()
                                        .tab_index(2)
                                        .tab_group()
                                        .tab_stop(false)
                                        .size_full()
                                        .child(commits_table),
                                )
                                .children(self.render_label_divider(layout, cx))
                                .children(self.render_lane_scrollbar(layout, window, cx)),
                        ),
                )
                .on_drag_move::<DraggedSplitHandle>(cx.listener(|this, event, window, cx| {
                    this.commit_details_split_state.update(cx, |state, cx| {
                        state.on_drag_move(event, window, cx);
                    });
                }))
                .on_drop::<DraggedSplitHandle>(cx.listener(|this, _event, _window, cx| {
                    this.commit_details_split_state.update(cx, |state, _cx| {
                        state.commit_ratio();
                    });
                }))
                .when(self.selected_entry_idx.is_some(), |this| {
                    this.child(self.render_commit_view_resize_handle(window, cx))
                        .child(self.render_commit_detail_panel(window, cx))
                })
        };

        div()
            .key_context("GitGraph")
            .track_focus(&self.focus_handle)
            .size_full()
            .bg(cx.theme().colors().editor_background)
            .on_action(cx.listener(Self::select_first_parent))
            .on_action(cx.listener(Self::select_first_child))
            .on_action(cx.listener(|this, _: &OpenCommitView, window, cx| {
                this.open_selected_commit_view(window, cx);
            }))
            .on_action(cx.listener(Self::copy_selected_commit_sha))
            .on_action(cx.listener(Self::copy_selected_commit_tag))
            .on_action(cx.listener(Self::cancel))
            .on_action(cx.listener(|this, _: &FocusSearch, window, cx| {
                this.search_state
                    .editor
                    .update(cx, |editor, cx| editor.focus_handle(cx).focus(window, cx));
                this.activate_search_editor_if_focused(window, cx);
            }))
            .on_action(cx.listener(Self::select_first))
            .on_action(cx.listener(Self::select_prev))
            .on_action(cx.listener(Self::select_next))
            .on_action(cx.listener(Self::select_last))
            .on_action(cx.listener(Self::scroll_up))
            .on_action(cx.listener(Self::scroll_down))
            .on_action(cx.listener(Self::confirm))
            .on_action(cx.listener(Self::toggle_changed_files_view))
            .on_action(cx.listener(Self::focus_next_tab_stop))
            .on_action(cx.listener(Self::focus_previous_tab_stop))
            .on_action(cx.listener(|this, _: &SelectNextMatch, _window, cx| {
                this.select_next_match(cx);
            }))
            .on_action(cx.listener(|this, _: &SelectPreviousMatch, _window, cx| {
                this.select_previous_match(cx);
            }))
            .on_action(cx.listener(|this, _: &ToggleCaseSensitive, _window, cx| {
                this.search_state.case_sensitive = !this.search_state.case_sensitive;
                this.search_state.state.next_state();
                cx.emit(ItemEvent::Edit);
                cx.notify();
            }))
            .child(
                v_flex()
                    .size_full()
                    .child(self.render_search_bar(cx))
                    .children(self.render_filter_bar(cx))
                    .child(div().flex_1().child(content)),
            )
            .children(self.context_menu.as_ref().map(|context_menu| {
                deferred(
                    anchored()
                        .position(context_menu.position)
                        .anchor(Anchor::TopLeft)
                        .child(context_menu.menu.clone()),
                )
                .with_priority(1)
            }))
            .on_action(cx.listener(|_, _: &buffer_search::Deploy, window, cx| {
                window.dispatch_action(Box::new(FocusSearch), cx);
                cx.stop_propagation();
            }))
    }
}

impl EventEmitter<ItemEvent> for GitGraph {}

impl Focusable for GitGraph {
    fn focus_handle(&self, _cx: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl Item for GitGraph {
    type Event = ItemEvent;

    fn tab_icon(&self, _window: &Window, _cx: &App) -> Option<Icon> {
        Some(Icon::new(IconName::GitGraph))
    }

    fn tab_tooltip_content(&self, cx: &App) -> Option<TabTooltipContent> {
        let repo_name = self.get_repository(cx).and_then(|repo| {
            repo.read(cx)
                .work_directory_abs_path
                .file_name()
                .map(|name| name.to_string_lossy().to_string())
        });
        let path_history_path = match &self.log_source {
            LogSource::Path(path) => Some(path.as_unix_str().to_string()),
            _ => None,
        };

        Some(TabTooltipContent::Custom(Box::new(Tooltip::element({
            move |_, _| {
                v_flex()
                    .child(Label::new(if path_history_path.is_some() {
                        "Path History"
                    } else {
                        "Git Graph"
                    }))
                    .when_some(path_history_path.clone(), |this, path| {
                        this.child(Label::new(path).color(Color::Muted).size(LabelSize::Small))
                    })
                    .when_some(repo_name.clone(), |this, name| {
                        this.child(Label::new(name).color(Color::Muted).size(LabelSize::Small))
                    })
                    .into_any_element()
            }
        }))))
    }

    fn tab_content_text(&self, _detail: usize, cx: &App) -> SharedString {
        if let LogSource::Path(path) = &self.log_source {
            return path
                .as_ref()
                .file_name()
                .map(|name| SharedString::from(name.to_string()))
                .unwrap_or_else(|| SharedString::from(path.as_unix_str().to_string()));
        }

        self.get_repository(cx)
            .and_then(|repo| {
                repo.read(cx)
                    .work_directory_abs_path
                    .file_name()
                    .map(|name| name.to_string_lossy().to_string())
            })
            .map_or_else(|| "Git Graph".into(), |name| SharedString::from(name))
    }

    fn show_toolbar(&self) -> bool {
        false
    }

    fn to_item_events(event: &Self::Event, f: &mut dyn FnMut(ItemEvent)) {
        f(*event)
    }
}

impl workspace::SerializableItem for GitGraph {
    fn serialized_item_kind() -> &'static str {
        "GitGraph"
    }

    fn cleanup(
        workspace_id: workspace::WorkspaceId,
        alive_items: Vec<workspace::ItemId>,
        _window: &mut Window,
        cx: &mut App,
    ) -> Task<gpui::Result<()>> {
        workspace::delete_unloaded_items(
            alive_items,
            workspace_id,
            "git_graphs",
            &persistence::GitGraphsDb::global(cx),
            cx,
        )
    }

    fn deserialize(
        project: Entity<project::Project>,
        workspace: WeakEntity<Workspace>,
        workspace_id: workspace::WorkspaceId,
        item_id: workspace::ItemId,
        window: &mut Window,
        cx: &mut App,
    ) -> Task<gpui::Result<Entity<Self>>> {
        let db = persistence::GitGraphsDb::global(cx);
        let Some((
            repo_work_path,
            log_source_type,
            log_source_value,
            log_order,
            selected_sha,
            search_query,
            search_case_sensitive,
        )) = db.get_git_graph(item_id, workspace_id).ok().flatten()
        else {
            return Task::ready(Err(anyhow::anyhow!("No git graph to deserialize")));
        };

        let state = persistence::SerializedGitGraphState {
            log_source_type,
            log_source_value,
            log_order,
            selected_sha,
            search_query,
            search_case_sensitive,
        };

        let window_handle = window.window_handle();
        let project = project.read(cx);
        let git_store = project.git_store().clone();
        let wait = project.wait_for_initial_scan(cx);

        cx.spawn(async move |cx| {
            wait.await;

            cx.update_window(window_handle, |_, window, cx| {
                let path = repo_work_path.as_path();

                let repositories = git_store.read(cx).repositories();
                let repo_id = repositories.iter().find_map(|(&repo_id, repo)| {
                    if repo.read(cx).snapshot().work_directory_abs_path.as_ref() == path {
                        Some(repo_id)
                    } else {
                        None
                    }
                });

                let Some(repo_id) = repo_id else {
                    return Err(anyhow::anyhow!("Repository not found for path: {:?}", path));
                };

                let log_source = persistence::deserialize_log_source(&state);
                let log_order = persistence::deserialize_log_order(&state);

                let git_graph = cx.new(|cx| {
                    let mut graph =
                        GitGraph::new(repo_id, git_store, workspace, Some(log_source), window, cx);
                    graph.log_order = log_order;

                    if let Some(sha) = &state.selected_sha {
                        graph.select_commit_by_sha(sha.as_str(), cx);
                    }

                    graph
                });

                git_graph.update(cx, |graph, cx| {
                    graph.search_state.case_sensitive =
                        state.search_case_sensitive.unwrap_or(false);

                    if let Some(query) = &state.search_query
                        && !query.is_empty()
                    {
                        graph
                            .search_state
                            .editor
                            .update(cx, |editor, cx| editor.set_text(query.as_str(), window, cx));
                        graph.search(query.clone().into(), cx);
                    }
                });

                Ok(git_graph)
            })?
        })
    }

    fn serialize(
        &mut self,
        workspace: &mut Workspace,
        item_id: workspace::ItemId,
        _closing: bool,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Option<Task<gpui::Result<()>>> {
        let workspace_id = workspace.database_id()?;
        let repo = self.get_repository(cx)?;
        let repo_working_path = repo
            .read(cx)
            .snapshot()
            .work_directory_abs_path
            .to_string_lossy()
            .to_string();

        let selected_sha = self
            .selected_entry_idx
            .and_then(|idx| self.graph_data.commits.get(idx))
            .map(|commit| commit.data.sha.to_string());

        let search_query = self.search_state.editor.read(cx).text(cx);
        let search_query = if search_query.is_empty() {
            None
        } else {
            Some(search_query)
        };

        let log_source_type = Some(persistence::serialize_log_source_type(&self.log_source));
        let log_source_value = persistence::serialize_log_source_value(&self.log_source);
        let log_order = Some(persistence::serialize_log_order(&self.log_order));
        let search_case_sensitive = Some(self.search_state.case_sensitive);

        let db = persistence::GitGraphsDb::global(cx);
        Some(cx.background_spawn(async move {
            db.save_git_graph(
                item_id,
                workspace_id,
                repo_working_path,
                log_source_type,
                log_source_value,
                log_order,
                selected_sha,
                search_query,
                search_case_sensitive,
            )
            .await
        }))
    }

    fn should_serialize(&self, event: &Self::Event) -> bool {
        match event {
            ItemEvent::UpdateTab | ItemEvent::Edit => true,
            _ => false,
        }
    }
}

mod persistence {
    use std::{path::PathBuf, str::FromStr};

    use db::{
        query,
        sqlez::{domain::Domain, thread_safe_connection::ThreadSafeConnection},
        sqlez_macros::sql,
    };
    use git::{
        Oid,
        repository::{LogOrder, LogSource, RepoPath},
    };
    use workspace::WorkspaceDb;

    pub struct GitGraphsDb(ThreadSafeConnection);

    impl Domain for GitGraphsDb {
        const NAME: &str = stringify!(GitGraphsDb);

        const MIGRATIONS: &[&str] = &[
            sql!(
                CREATE TABLE git_graphs (
                    workspace_id INTEGER,
                    item_id INTEGER UNIQUE,
                    is_open INTEGER DEFAULT FALSE,

                    PRIMARY KEY(workspace_id, item_id),
                    FOREIGN KEY(workspace_id) REFERENCES workspaces(workspace_id)
                    ON DELETE CASCADE
                ) STRICT;
            ),
            sql!(
                ALTER TABLE git_graphs ADD COLUMN repo_working_path TEXT;
            ),
            sql!(
                ALTER TABLE git_graphs ADD COLUMN log_source_type TEXT;
                ALTER TABLE git_graphs ADD COLUMN log_source_value TEXT;
                ALTER TABLE git_graphs ADD COLUMN log_order TEXT;
                ALTER TABLE git_graphs ADD COLUMN selected_sha TEXT;
                ALTER TABLE git_graphs ADD COLUMN search_query TEXT;
                ALTER TABLE git_graphs ADD COLUMN search_case_sensitive INTEGER;
            ),
            sql!(
                ALTER TABLE git_graphs ADD COLUMN hidden_columns INTEGER;
            ),
        ];
    }

    db::static_connection!(GitGraphsDb, [WorkspaceDb]);

    pub const LOG_SOURCE_ALL: i32 = 0;
    pub const LOG_SOURCE_BRANCH: i32 = 1;
    pub const LOG_SOURCE_SHA: i32 = 2;
    pub const LOG_SOURCE_PATH: i32 = 3;

    pub const LOG_ORDER_DATE: i32 = 0;
    pub const LOG_ORDER_TOPO: i32 = 1;
    pub const LOG_ORDER_AUTHOR_DATE: i32 = 2;
    pub const LOG_ORDER_REVERSE: i32 = 3;

    pub fn serialize_log_source_type(log_source: &LogSource) -> i32 {
        match log_source {
            LogSource::All => LOG_SOURCE_ALL,
            LogSource::Branch(_) => LOG_SOURCE_BRANCH,
            LogSource::Sha(_) => LOG_SOURCE_SHA,
            LogSource::Path(_) => LOG_SOURCE_PATH,
        }
    }

    pub fn serialize_log_source_value(log_source: &LogSource) -> Option<String> {
        match log_source {
            LogSource::All => None,
            LogSource::Branch(branch) => Some(branch.to_string()),
            LogSource::Sha(oid) => Some(oid.to_string()),
            LogSource::Path(path) => Some(path.as_unix_str().to_string()),
        }
    }

    pub fn serialize_log_order(log_order: &LogOrder) -> i32 {
        match log_order {
            LogOrder::DateOrder => LOG_ORDER_DATE,
            LogOrder::TopoOrder => LOG_ORDER_TOPO,
            LogOrder::AuthorDateOrder => LOG_ORDER_AUTHOR_DATE,
            LogOrder::ReverseChronological => LOG_ORDER_REVERSE,
        }
    }

    pub fn deserialize_log_source(state: &SerializedGitGraphState) -> LogSource {
        match state.log_source_type {
            Some(LOG_SOURCE_ALL) => LogSource::All,
            Some(LOG_SOURCE_BRANCH) => state
                .log_source_value
                .as_ref()
                .map(|v| LogSource::Branch(v.clone().into()))
                .unwrap_or_default(),
            Some(LOG_SOURCE_SHA) => state
                .log_source_value
                .as_ref()
                .and_then(|v| Oid::from_str(v).ok())
                .map(LogSource::Sha)
                .unwrap_or_default(),
            Some(LOG_SOURCE_PATH) => state
                .log_source_value
                .as_ref()
                .and_then(|v| RepoPath::new(v).ok())
                .map(LogSource::Path)
                .unwrap_or_default(),
            None | Some(_) => LogSource::default(),
        }
    }

    pub fn deserialize_log_order(state: &SerializedGitGraphState) -> LogOrder {
        match state.log_order {
            Some(LOG_ORDER_DATE) => LogOrder::DateOrder,
            Some(LOG_ORDER_TOPO) => LogOrder::TopoOrder,
            Some(LOG_ORDER_AUTHOR_DATE) => LogOrder::AuthorDateOrder,
            Some(LOG_ORDER_REVERSE) => LogOrder::ReverseChronological,
            _ => LogOrder::default(),
        }
    }

    #[derive(Debug, Default, Clone)]
    pub struct SerializedGitGraphState {
        pub log_source_type: Option<i32>,
        pub log_source_value: Option<String>,
        pub log_order: Option<i32>,
        pub selected_sha: Option<String>,
        pub search_query: Option<String>,
        pub search_case_sensitive: Option<bool>,
    }

    impl GitGraphsDb {
        query! {
            pub async fn save_git_graph(
                item_id: workspace::ItemId,
                workspace_id: workspace::WorkspaceId,
                repo_working_path: String,
                log_source_type: Option<i32>,
                log_source_value: Option<String>,
                log_order: Option<i32>,
                selected_sha: Option<String>,
                search_query: Option<String>,
                search_case_sensitive: Option<bool>
            ) -> Result<()> {
                INSERT OR REPLACE INTO git_graphs(
                    item_id, workspace_id, repo_working_path,
                    log_source_type, log_source_value, log_order,
                    selected_sha, search_query, search_case_sensitive
                )
                VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?)
            }
        }

        query! {
            pub fn get_git_graph(
                item_id: workspace::ItemId,
                workspace_id: workspace::WorkspaceId
            ) -> Result<Option<(
                PathBuf,
                Option<i32>,
                Option<String>,
                Option<i32>,
                Option<String>,
                Option<String>,
                Option<bool>
            )>> {
                SELECT
                    repo_working_path,
                    log_source_type,
                    log_source_value,
                    log_order,
                    selected_sha,
                    search_query,
                    search_case_sensitive
                FROM git_graphs
                WHERE item_id = ? AND workspace_id = ?
            }
        }
    }
}

#[cfg(any(test, feature = "test-support"))]
impl GitGraph {
    pub fn search_for_test(&mut self, query: SharedString, cx: &mut Context<Self>) {
        self.search(query, cx);
    }

    pub fn search_matches_for_test(&self) -> Vec<Oid> {
        self.search_state.matches.iter().copied().collect()
    }

    pub fn initial_commit_data_for_test(&self) -> Vec<Arc<InitialGraphCommitData>> {
        self.graph_data
            .commits
            .iter()
            .map(|commit| commit.data.clone())
            .collect()
    }

    pub fn commit_count_and_loading_state_for_test(
        &mut self,
        cx: &mut Context<Self>,
    ) -> (usize, bool) {
        self.commit_count_and_loading_state(cx)
    }

    pub fn log_source_for_test(&self) -> &LogSource {
        &self.log_source
    }
}

/// Generates a random commit DAG suitable for testing git graph rendering.
///
/// The commits are ordered newest-first (like git log output), so:
/// - Index 0 = most recent commit (HEAD)
/// - Last index = oldest commit (root, has no parents)
/// - Parents of commit at index I must have index > I
///
/// When `adversarial` is true, generates complex topologies with many branches
/// and octopus merges. Otherwise generates more realistic linear histories
/// with occasional branches.
#[cfg(any(test, feature = "test-support"))]
pub fn generate_random_commit_dag(
    rng: &mut rand::rngs::StdRng,
    num_commits: usize,
    adversarial: bool,
) -> Vec<Arc<InitialGraphCommitData>> {
    use rand::Rng as _;

    if num_commits == 0 {
        return Vec::new();
    }

    let mut commits: Vec<Arc<InitialGraphCommitData>> = Vec::with_capacity(num_commits);
    let oids: Vec<Oid> = (0..num_commits).map(|_| Oid::random(rng)).collect();

    for i in 0..num_commits {
        let sha = oids[i];

        let parents = if i == num_commits - 1 {
            smallvec![]
        } else {
            generate_parents_from_oids(rng, &oids, i, num_commits, adversarial)
        };

        let ref_names = if i == 0 {
            vec!["HEAD".into(), "main".into()]
        } else if adversarial && rng.random_bool(0.1) {
            vec![format!("branch-{i}").into()]
        } else {
            Vec::new()
        };

        commits.push(Arc::new(InitialGraphCommitData {
            sha,
            parents,
            ref_names,
        }));
    }

    commits
}

#[cfg(any(test, feature = "test-support"))]
fn generate_parents_from_oids(
    rng: &mut rand::rngs::StdRng,
    oids: &[Oid],
    current_idx: usize,
    num_commits: usize,
    adversarial: bool,
) -> SmallVec<[Oid; 1]> {
    use rand::{Rng as _, seq::SliceRandom as _};

    let remaining = num_commits - current_idx - 1;
    if remaining == 0 {
        return smallvec![];
    }

    if adversarial {
        let merge_chance = 0.4;
        let octopus_chance = 0.15;

        if remaining >= 3 && rng.random_bool(octopus_chance) {
            let num_parents = rng.random_range(3..=remaining.min(5));
            let mut parent_indices: Vec<usize> = (current_idx + 1..num_commits).collect();
            parent_indices.shuffle(rng);
            parent_indices
                .into_iter()
                .take(num_parents)
                .map(|idx| oids[idx])
                .collect()
        } else if remaining >= 2 && rng.random_bool(merge_chance) {
            let mut parent_indices: Vec<usize> = (current_idx + 1..num_commits).collect();
            parent_indices.shuffle(rng);
            parent_indices
                .into_iter()
                .take(2)
                .map(|idx| oids[idx])
                .collect()
        } else {
            let parent_idx = rng.random_range(current_idx + 1..num_commits);
            smallvec![oids[parent_idx]]
        }
    } else {
        let merge_chance = 0.15;
        let skip_chance = 0.1;

        if remaining >= 2 && rng.random_bool(merge_chance) {
            let first_parent = current_idx + 1;
            let second_parent = rng.random_range(current_idx + 2..num_commits);
            smallvec![oids[first_parent], oids[second_parent]]
        } else if rng.random_bool(skip_chance) && remaining >= 2 {
            let skip = rng.random_range(1..remaining.min(3));
            smallvec![oids[current_idx + 1 + skip]]
        } else {
            smallvec![oids[current_idx + 1]]
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use anyhow::{Context, Result, bail};
    use collections::{HashMap, HashSet};
    use fs::FakeFs;
    use git::Oid;
    use git::repository::{CommitData, InitialGraphCommitData};
    use gpui::{TestAppContext, UpdateGlobal, VisualTestContext};
    use project::git_store::{GitStoreEvent, RepositoryEvent};
    use project::{
        GIT_COMMAND_TASK_TAG, Project, TaskSourceKind, task_store::TaskSettingsLocation,
    };
    use rand::prelude::*;
    use serde_json::json;
    use settings::{SettingsStore, ThemeSettingsContent};
    use smallvec::{SmallVec, smallvec};
    use std::path::Path;
    use std::sync::{Arc, Mutex};

    fn init_test(cx: &mut TestAppContext) {
        cx.update(|cx| {
            let settings_store = SettingsStore::test(cx);
            cx.set_global(settings_store);
            theme_settings::init(theme::LoadThemes::JustBase, cx);
            language_model::init(cx);
            crate::init(cx);
        });
    }

    fn build_oid_to_row_map(graph: &GraphData) -> HashMap<Oid, usize> {
        graph
            .commits
            .iter()
            .enumerate()
            .map(|(idx, entry)| (entry.data.sha, idx))
            .collect()
    }

    fn verify_commit_order(
        graph: &GraphData,
        commits: &[Arc<InitialGraphCommitData>],
    ) -> Result<()> {
        if graph.commits.len() != commits.len() {
            bail!(
                "Commit count mismatch: graph has {} commits, expected {}",
                graph.commits.len(),
                commits.len()
            );
        }

        for (idx, (graph_commit, expected_commit)) in
            graph.commits.iter().zip(commits.iter()).enumerate()
        {
            if graph_commit.data.sha != expected_commit.sha {
                bail!(
                    "Commit order mismatch at index {}: graph has {:?}, expected {:?}",
                    idx,
                    graph_commit.data.sha,
                    expected_commit.sha
                );
            }
        }

        Ok(())
    }

    fn verify_line_endpoints(graph: &GraphData, oid_to_row: &HashMap<Oid, usize>) -> Result<()> {
        for line in &graph.lines {
            let child_row = *oid_to_row
                .get(&line.child)
                .context("Line references non-existent child commit")?;

            let parent_row = *oid_to_row
                .get(&line.parent)
                .context("Line references non-existent parent commit")?;

            if child_row >= parent_row {
                bail!(
                    "child_row ({}) must be < parent_row ({})",
                    child_row,
                    parent_row
                );
            }

            if line.full_interval.start != child_row {
                bail!(
                    "full_interval.start ({}) != child_row ({})",
                    line.full_interval.start,
                    child_row
                );
            }

            if line.full_interval.end != parent_row {
                bail!(
                    "full_interval.end ({}) != parent_row ({})",
                    line.full_interval.end,
                    parent_row
                );
            }

            if let Some(last_segment) = line.segments.last() {
                let segment_end_row = match last_segment {
                    CommitLineSegment::Straight { to_row } => *to_row,
                    CommitLineSegment::Curve { on_row, .. } => *on_row,
                };

                if segment_end_row != line.full_interval.end {
                    bail!(
                        "last segment ends at row {} but full_interval.end is {}",
                        segment_end_row,
                        line.full_interval.end
                    );
                }
            }
        }

        Ok(())
    }

    fn verify_column_correctness(
        graph: &GraphData,
        oid_to_row: &HashMap<Oid, usize>,
    ) -> Result<()> {
        for line in &graph.lines {
            let child_row = *oid_to_row
                .get(&line.child)
                .context("Line references non-existent child commit")?;

            let parent_row = *oid_to_row
                .get(&line.parent)
                .context("Line references non-existent parent commit")?;

            let child_lane = graph.commits[child_row].lane;
            if line.child_column != child_lane {
                bail!(
                    "child_column ({}) != child's lane ({})",
                    line.child_column,
                    child_lane
                );
            }

            let mut current_column = line.child_column;
            for segment in &line.segments {
                if let CommitLineSegment::Curve { to_column, .. } = segment {
                    current_column = *to_column;
                }
            }

            let parent_lane = graph.commits[parent_row].lane;
            if current_column != parent_lane {
                bail!(
                    "ending column ({}) != parent's lane ({})",
                    current_column,
                    parent_lane
                );
            }
        }

        Ok(())
    }

    fn verify_segment_continuity(graph: &GraphData) -> Result<()> {
        for line in &graph.lines {
            if line.segments.is_empty() {
                bail!("Line has no segments");
            }

            let mut current_row = line.full_interval.start;

            for (idx, segment) in line.segments.iter().enumerate() {
                let segment_end_row = match segment {
                    CommitLineSegment::Straight { to_row } => *to_row,
                    CommitLineSegment::Curve { on_row, .. } => *on_row,
                };

                if segment_end_row < current_row {
                    bail!(
                        "segment {} ends at row {} which is before current row {}",
                        idx,
                        segment_end_row,
                        current_row
                    );
                }

                current_row = segment_end_row;
            }
        }

        Ok(())
    }

    fn verify_line_overlaps(graph: &GraphData) -> Result<()> {
        for line in &graph.lines {
            let child_row = line.full_interval.start;

            let mut current_column = line.child_column;
            let mut current_row = child_row;

            for segment in &line.segments {
                match segment {
                    CommitLineSegment::Straight { to_row } => {
                        for row in (current_row + 1)..*to_row {
                            if row < graph.commits.len() {
                                let commit_at_row = &graph.commits[row];
                                if commit_at_row.lane == current_column {
                                    bail!(
                                        "straight segment from row {} to {} in column {} passes through commit {:?} at row {}",
                                        current_row,
                                        to_row,
                                        current_column,
                                        commit_at_row.data.sha,
                                        row
                                    );
                                }
                            }
                        }
                        current_row = *to_row;
                    }
                    CommitLineSegment::Curve {
                        to_column, on_row, ..
                    } => {
                        current_column = *to_column;
                        current_row = *on_row;
                    }
                }
            }
        }

        Ok(())
    }

    fn verify_keep_shared_parents_on_leftmost_lane(graph: &GraphData) -> Result<()> {
        let mut active_lane_parents: Vec<Option<Oid>> = Vec::new();
        let mut parent_to_lanes: HashMap<Oid, SmallVec<[usize; 1]>> = HashMap::default();

        for (row, entry) in graph.commits.iter().enumerate() {
            let pending_lanes = parent_to_lanes.remove(&entry.data.sha).unwrap_or_default();

            if pending_lanes.len() > 1
                && let Some(expected_lane) = pending_lanes.iter().copied().min()
                && entry.lane != expected_lane
            {
                bail!(
                    "commit {:?} at row {} uses lane {}, but shared parent should use leftmost pending lane {} from {:?}",
                    entry.data.sha,
                    row,
                    entry.lane,
                    expected_lane,
                    pending_lanes
                );
            }

            for lane in pending_lanes {
                let Some(active_lane_parent) = active_lane_parents.get_mut(lane) else {
                    bail!(
                        "commit {:?} at row {} was pending on missing lane {}",
                        entry.data.sha,
                        row,
                        lane
                    );
                };

                if *active_lane_parent != Some(entry.data.sha) {
                    bail!(
                        "commit {:?} at row {} was pending on lane {}, but that lane points to {:?}",
                        entry.data.sha,
                        row,
                        lane,
                        active_lane_parent
                    );
                }

                *active_lane_parent = None;
            }

            for (parent_index, parent) in entry.data.parents.iter().enumerate() {
                let lane = if parent_index == 0 {
                    entry.lane
                } else if let Some(empty_lane) =
                    active_lane_parents.iter().position(Option::is_none)
                {
                    empty_lane
                } else {
                    active_lane_parents.push(None);
                    active_lane_parents.len() - 1
                };

                if lane >= active_lane_parents.len() {
                    active_lane_parents.resize(lane + 1, None);
                }

                active_lane_parents[lane] = Some(*parent);
                parent_to_lanes.entry(*parent).or_default().push(lane);
            }
        }

        Ok(())
    }

    fn verify_coverage(graph: &GraphData) -> Result<()> {
        let mut expected_edges: HashSet<(Oid, Oid)> = HashSet::default();
        for entry in &graph.commits {
            for parent in &entry.data.parents {
                expected_edges.insert((entry.data.sha, *parent));
            }
        }

        let mut found_edges: HashSet<(Oid, Oid)> = HashSet::default();
        for line in &graph.lines {
            let edge = (line.child, line.parent);

            if !found_edges.insert(edge) {
                bail!(
                    "Duplicate line found for edge {:?} -> {:?}",
                    line.child,
                    line.parent
                );
            }

            if !expected_edges.contains(&edge) {
                bail!(
                    "Orphan line found: {:?} -> {:?} is not in the commit graph",
                    line.child,
                    line.parent
                );
            }
        }

        for (child, parent) in &expected_edges {
            if !found_edges.contains(&(*child, *parent)) {
                bail!("Missing line for edge {:?} -> {:?}", child, parent);
            }
        }

        assert_eq!(
            expected_edges.symmetric_difference(&found_edges).count(),
            0,
            "The symmetric difference should be zero"
        );

        Ok(())
    }

    fn verify_merge_line_optimality(
        graph: &GraphData,
        oid_to_row: &HashMap<Oid, usize>,
    ) -> Result<()> {
        for line in &graph.lines {
            let first_segment = line.segments.first();
            let is_merge_line = matches!(
                first_segment,
                Some(CommitLineSegment::Curve {
                    curve_kind: CurveKind::Merge,
                    ..
                })
            );

            if !is_merge_line {
                continue;
            }

            let child_row = *oid_to_row
                .get(&line.child)
                .context("Line references non-existent child commit")?;

            let parent_row = *oid_to_row
                .get(&line.parent)
                .context("Line references non-existent parent commit")?;

            let parent_lane = graph.commits[parent_row].lane;

            let Some(CommitLineSegment::Curve { to_column, .. }) = first_segment else {
                continue;
            };

            let curves_directly_to_parent = *to_column == parent_lane;

            if !curves_directly_to_parent {
                continue;
            }

            let curve_row = child_row + 1;
            let has_commits_in_path = graph.commits[curve_row..parent_row]
                .iter()
                .any(|c| c.lane == parent_lane);

            if has_commits_in_path {
                bail!(
                    "Merge line from {:?} to {:?} curves directly to parent lane {} but there are commits in that lane between rows {} and {}",
                    line.child,
                    line.parent,
                    parent_lane,
                    curve_row,
                    parent_row
                );
            }

            let curve_ends_at_parent = curve_row == parent_row;

            if curve_ends_at_parent {
                if line.segments.len() != 1 {
                    bail!(
                        "Merge line from {:?} to {:?} curves directly to parent (curve_row == parent_row), but has {} segments instead of 1 [MergeCurve]",
                        line.child,
                        line.parent,
                        line.segments.len()
                    );
                }
            } else {
                if line.segments.len() != 2 {
                    bail!(
                        "Merge line from {:?} to {:?} curves directly to parent lane without overlap, but has {} segments instead of 2 [MergeCurve, Straight]",
                        line.child,
                        line.parent,
                        line.segments.len()
                    );
                }

                let is_straight_segment = matches!(
                    line.segments.get(1),
                    Some(CommitLineSegment::Straight { .. })
                );

                if !is_straight_segment {
                    bail!(
                        "Merge line from {:?} to {:?} curves directly to parent lane without overlap, but second segment is not a Straight segment",
                        line.child,
                        line.parent
                    );
                }
            }
        }

        Ok(())
    }

    fn verify_all_invariants(
        graph: &GraphData,
        commits: &[Arc<InitialGraphCommitData>],
    ) -> Result<()> {
        let oid_to_row = build_oid_to_row_map(graph);

        verify_commit_order(graph, commits).context("commit order")?;
        verify_line_endpoints(graph, &oid_to_row).context("line endpoints")?;
        verify_column_correctness(graph, &oid_to_row).context("column correctness")?;
        verify_segment_continuity(graph).context("segment continuity")?;
        verify_merge_line_optimality(graph, &oid_to_row).context("merge line optimality")?;
        verify_keep_shared_parents_on_leftmost_lane(graph)
            .context("keep shared parents on leftmost lane")?;
        verify_coverage(graph).context("coverage")?;
        verify_line_overlaps(graph).context("line overlaps")?;
        Ok(())
    }

    /// A fold has to take away the branch that was merged in and nothing else.
    /// The commit both sides descend from is where the branch rejoined what it
    /// left, and it stays: it belonged to the history before the branch existed.
    #[test]
    fn a_fold_takes_the_branch_and_stops_where_it_rejoined() {
        let sha = |byte: u8| Oid::from_bytes(&[byte; 20]).unwrap();
        let (merge, mainline, tip, middle, base) = (sha(5), sha(4), sha(3), sha(2), sha(1));

        let of = |sha, parents: Vec<Oid>| {
            Arc::new(InitialGraphCommitData {
                sha,
                parents: parents.into_iter().collect(),
                ref_names: vec![],
            })
        };
        let commits = vec![
            of(merge, vec![mainline, tip]),
            of(mainline, vec![base]),
            of(tip, vec![middle]),
            of(middle, vec![base]),
            of(base, vec![]),
        ];

        let mut graph = GraphData::new(6);
        graph.add_commits(&commits);

        let mut folded: Vec<usize> = graph
            .side_branch_of(0, 100)
            .expect("the branch is small and every parent is loaded")
            .into_iter()
            .collect();
        folded.sort();
        assert_eq!(
            folded,
            vec![2, 3],
            "the two commits of the branch go, and nothing else"
        );

        assert_eq!(
            graph.side_branch_of(0, 1),
            None,
            "a branch larger than the budget is refused rather than half-taken"
        );
        assert_eq!(
            graph.side_branch_of(1, 100),
            None,
            "a commit that merged nothing has no branch to fold"
        );
        assert_eq!(
            graph.side_branch_of(99, 100),
            None,
            "and neither has a row that is not there"
        );

        // A merge of something the history already held brings in no branch.
        // Reporting an empty one would leave a mark saying nothing was put away
        // beside a control offering to bring it back.
        let already_held = vec![
            of(sha(9), vec![mainline, base]),
            of(mainline, vec![base]),
            of(base, vec![]),
        ];
        let mut graph = GraphData::new(6);
        graph.add_commits(&already_held);
        assert_eq!(
            graph.side_branch_of(0, 100),
            None,
            "a merge that brought in nothing has nothing to fold away"
        );
    }

    /// What a hover lights: the commit, where it came from, and what came of
    /// it. One step each way and no further -- the test that matters here is
    /// the one asserting the grandparent stays dark, because a set that walked
    /// the whole ancestry would light nearly every row of a real history and
    /// mean nothing.
    #[test]
    fn kin_stops_one_step_from_the_commit() {
        let sha = |byte: u8| Oid::from_bytes(&[byte; 20]).unwrap();
        let (merge, mainline, side, root) = (sha(4), sha(3), sha(2), sha(1));

        // Newest first, the order `git log` reports.
        let commits = vec![
            Arc::new(InitialGraphCommitData {
                sha: merge,
                parents: smallvec![mainline, side],
                ref_names: vec![],
            }),
            Arc::new(InitialGraphCommitData {
                sha: mainline,
                parents: smallvec![root],
                ref_names: vec![],
            }),
            Arc::new(InitialGraphCommitData {
                sha: side,
                parents: smallvec![root],
                ref_names: vec![],
            }),
            Arc::new(InitialGraphCommitData {
                sha: root,
                parents: smallvec![],
                ref_names: vec![],
            }),
        ];

        let mut graph = GraphData::new(6);
        graph.add_commits(&commits);

        let lit = |row: usize| {
            let mut rows: Vec<usize> = graph.kin_of(row).into_iter().collect();
            rows.sort();
            rows
        };

        assert_eq!(
            lit(0),
            vec![0, 1, 2],
            "the merge lights itself and both sides it joined"
        );
        assert!(
            !graph.kin_of(0).contains(&3),
            "the root is the merge's grandparent, and stays dark"
        );
        assert_eq!(
            lit(3),
            vec![1, 2, 3],
            "the root lights itself and the two commits naming it"
        );
        assert_eq!(
            lit(1),
            vec![0, 1, 3],
            "a commit in the middle lights its parent below and its child above"
        );
    }

    #[test]
    fn test_git_graph_merge_commits() {
        let mut rng = StdRng::seed_from_u64(42);

        let oid1 = Oid::random(&mut rng);
        let oid2 = Oid::random(&mut rng);
        let oid3 = Oid::random(&mut rng);
        let oid4 = Oid::random(&mut rng);

        let commits = vec![
            Arc::new(InitialGraphCommitData {
                sha: oid1,
                parents: smallvec![oid2, oid3],
                ref_names: vec!["HEAD".into()],
            }),
            Arc::new(InitialGraphCommitData {
                sha: oid2,
                parents: smallvec![oid4],
                ref_names: vec![],
            }),
            Arc::new(InitialGraphCommitData {
                sha: oid3,
                parents: smallvec![oid4],
                ref_names: vec![],
            }),
            Arc::new(InitialGraphCommitData {
                sha: oid4,
                parents: smallvec![],
                ref_names: vec![],
            }),
        ];

        let mut graph_data = GraphData::new(8);
        graph_data.add_commits(&commits);

        if let Err(error) = verify_all_invariants(&graph_data, &commits) {
            panic!("Graph invariant violation for merge commits:\n{}", error);
        }
    }

    #[test]
    fn test_git_graph_linear_commits() {
        let mut rng = StdRng::seed_from_u64(42);

        let oid1 = Oid::random(&mut rng);
        let oid2 = Oid::random(&mut rng);
        let oid3 = Oid::random(&mut rng);

        let commits = vec![
            Arc::new(InitialGraphCommitData {
                sha: oid1,
                parents: smallvec![oid2],
                ref_names: vec!["HEAD".into()],
            }),
            Arc::new(InitialGraphCommitData {
                sha: oid2,
                parents: smallvec![oid3],
                ref_names: vec![],
            }),
            Arc::new(InitialGraphCommitData {
                sha: oid3,
                parents: smallvec![],
                ref_names: vec![],
            }),
        ];

        let mut graph_data = GraphData::new(8);
        graph_data.add_commits(&commits);

        if let Err(error) = verify_all_invariants(&graph_data, &commits) {
            panic!("Graph invariant violation for linear commits:\n{}", error);
        }
    }

    #[test]
    fn test_git_graph_random_commits() {
        for seed in 0..100 {
            let mut rng = StdRng::seed_from_u64(seed);

            let adversarial = rng.random_bool(0.2);
            let num_commits = if adversarial {
                rng.random_range(10..100)
            } else {
                rng.random_range(5..50)
            };

            let commits = generate_random_commit_dag(&mut rng, num_commits, adversarial);

            assert_eq!(
                num_commits,
                commits.len(),
                "seed={}: Generate random commit dag didn't generate the correct amount of commits",
                seed
            );

            let mut graph_data = GraphData::new(8);
            graph_data.add_commits(&commits);

            if let Err(error) = verify_all_invariants(&graph_data, &commits) {
                panic!(
                    "Graph invariant violation (seed={}, adversarial={}, num_commits={}):\n{:#}",
                    seed, adversarial, num_commits, error
                );
            }
        }
    }

    // The full integration test has less iterations because it's significantly slower
    // than the random commit test
    #[gpui::test(iterations = 10)]
    async fn test_git_graph_random_integration(mut rng: StdRng, cx: &mut TestAppContext) {
        init_test(cx);

        let adversarial = rng.random_bool(0.2);
        let num_commits = if adversarial {
            rng.random_range(10..100)
        } else {
            rng.random_range(5..50)
        };

        let commits = generate_random_commit_dag(&mut rng, num_commits, adversarial);

        let fs = FakeFs::new(cx.executor());
        fs.insert_tree(
            Path::new("/project"),
            json!({
                ".git": {},
                "file.txt": "content",
            }),
        )
        .await;

        fs.set_graph_commits(Path::new("/project/.git"), commits.clone());

        let project = Project::test(fs.clone(), [Path::new("/project")], cx).await;
        cx.run_until_parked();

        let repository = project.read_with(cx, |project, cx| {
            project
                .active_repository(cx)
                .expect("should have a repository")
        });

        repository.update(cx, |repo, cx| {
            repo.graph_data(LogSource::default(), LogOrder::default(), 0..usize::MAX, cx);
        });
        cx.run_until_parked();

        let graph_commits: Vec<Arc<InitialGraphCommitData>> = repository.update(cx, |repo, cx| {
            repo.graph_data(LogSource::default(), LogOrder::default(), 0..usize::MAX, cx)
                .commits
                .to_vec()
        });

        let mut graph_data = GraphData::new(8);
        graph_data.add_commits(&graph_commits);

        if let Err(error) = verify_all_invariants(&graph_data, &commits) {
            panic!(
                "Graph invariant violation (adversarial={}, num_commits={}):\n{:#}",
                adversarial, num_commits, error
            );
        }
    }

    #[gpui::test]
    async fn test_empty_nested_repository_graph_stops_loading(cx: &mut TestAppContext) {
        init_test(cx);

        let fs = FakeFs::new(cx.executor());
        fs.insert_tree(
            Path::new("/project"),
            json!({
                "repo_a": {
                    ".git": {},
                    "file_a.txt": "content",
                },
                "repo_b": {
                    ".git": {},
                    "file_b.txt": "content",
                },
            }),
        )
        .await;

        let project = Project::test(fs.clone(), [Path::new("/project")], cx).await;
        project
            .update(cx, |project, cx| project.git_scans_complete(cx))
            .await;
        cx.run_until_parked();

        let repository = project.read_with(cx, |project, cx| {
            assert_eq!(project.repositories(cx).len(), 2);
            project
                .active_repository(cx)
                .expect("should have an active repository")
        });

        let (multi_workspace, cx) = cx.add_window_view(|window, cx| {
            workspace::MultiWorkspace::test_new(project.clone(), window, cx)
        });
        let workspace = multi_workspace.read_with(&*cx, |multi, _| multi.workspace().downgrade());
        let git_graph = cx.new_window_entity(|window, cx| {
            GitGraph::new(
                repository.read(cx).id,
                project.read(cx).git_store().clone(),
                workspace,
                None,
                window,
                cx,
            )
        });
        cx.run_until_parked();

        let (commit_count, is_loading) = git_graph.update(cx, |graph, cx| {
            graph.commit_count_and_loading_state_for_test(cx)
        });

        assert_eq!(commit_count, 0);
        assert!(!is_loading, "empty graph data should stop loading");
    }

    #[gpui::test]
    async fn test_initial_graph_data_not_cleared_on_initial_loading(cx: &mut TestAppContext) {
        init_test(cx);

        let fs = FakeFs::new(cx.executor());
        fs.insert_tree(
            Path::new("/project"),
            json!({
                ".git": {},
                "file.txt": "content",
            }),
        )
        .await;

        let mut rng = StdRng::seed_from_u64(42);
        let commits = generate_random_commit_dag(&mut rng, 10, false);
        fs.set_graph_commits(Path::new("/project/.git"), commits.clone());

        let project = Project::test(fs.clone(), [Path::new("/project")], cx).await;
        let observed_repository_events = Arc::new(Mutex::new(Vec::new()));
        project.update(cx, |project, cx| {
            let observed_repository_events = observed_repository_events.clone();
            cx.subscribe(project.git_store(), move |_, _, event, _| {
                if let GitStoreEvent::RepositoryUpdated(_, repository_event, true) = event {
                    observed_repository_events
                        .lock()
                        .expect("repository event mutex should be available")
                        .push(repository_event.clone());
                }
            })
            .detach();
        });

        let repository = project.read_with(cx, |project, cx| {
            project
                .active_repository(cx)
                .expect("should have a repository")
        });

        repository.update(cx, |repo, cx| {
            repo.graph_data(LogSource::default(), LogOrder::default(), 0..usize::MAX, cx);
        });

        project
            .update(cx, |project, cx| project.git_scans_complete(cx))
            .await;
        cx.run_until_parked();

        let observed_repository_events = observed_repository_events
            .lock()
            .expect("repository event mutex should be available");
        assert!(
            observed_repository_events
                .iter()
                .any(|event| matches!(event, RepositoryEvent::HeadChanged)),
            "initial repository scan should emit HeadChanged"
        );
        let commit_count_after = repository.read_with(cx, |repo, _| {
            repo.get_graph_data(LogSource::default(), LogOrder::default())
                .map(|data| data.commit_data.len())
                .unwrap()
        });
        assert_eq!(
            commits.len(),
            commit_count_after,
            "initial_graph_data should remain populated after events emitted by initial repository scan"
        );
    }

    #[gpui::test]
    async fn test_initial_graph_data_propagates_error(cx: &mut TestAppContext) {
        init_test(cx);

        let fs = FakeFs::new(cx.executor());
        fs.insert_tree(
            Path::new("/project"),
            json!({
                ".git": {},
                "file.txt": "content",
            }),
        )
        .await;

        fs.set_graph_error(
            Path::new("/project/.git"),
            Some("fatal: bad default revision 'HEAD'".to_string()),
        );

        let project = Project::test(fs.clone(), [Path::new("/project")], cx).await;

        let repository = project.read_with(cx, |project, cx| {
            project
                .active_repository(cx)
                .expect("should have a repository")
        });

        repository.update(cx, |repo, cx| {
            repo.graph_data(LogSource::default(), LogOrder::default(), 0..usize::MAX, cx);
        });

        cx.run_until_parked();

        let error = repository.read_with(cx, |repo, _| {
            repo.get_graph_data(LogSource::default(), LogOrder::default())
                .and_then(|data| data.error.clone())
        });

        assert!(
            error.is_some(),
            "graph data should contain an error after initial_graph_data fails"
        );
        let error_message = error.unwrap();
        assert!(
            error_message.contains("bad default revision"),
            "error should contain the git error message, got: {}",
            error_message
        );
    }

    #[gpui::test]
    async fn test_graph_data_repopulated_from_cache_after_repo_switch(cx: &mut TestAppContext) {
        init_test(cx);

        let fs = FakeFs::new(cx.executor());
        fs.insert_tree(
            Path::new("/project_a"),
            json!({
                ".git": {},
                "file.txt": "content",
            }),
        )
        .await;
        fs.insert_tree(
            Path::new("/project_b"),
            json!({
                ".git": {},
                "other.txt": "content",
            }),
        )
        .await;

        let mut rng = StdRng::seed_from_u64(42);
        let commits = generate_random_commit_dag(&mut rng, 10, false);
        fs.set_graph_commits(Path::new("/project_a/.git"), commits.clone());

        let project = Project::test(
            fs.clone(),
            [Path::new("/project_a"), Path::new("/project_b")],
            cx,
        )
        .await;
        cx.run_until_parked();

        let (first_repository, second_repository) = project.read_with(cx, |project, cx| {
            let mut first_repository = None;
            let mut second_repository = None;

            for repository in project.repositories(cx).values() {
                let work_directory_abs_path = &repository.read(cx).work_directory_abs_path;
                if work_directory_abs_path.as_ref() == Path::new("/project_a") {
                    first_repository = Some(repository.clone());
                } else if work_directory_abs_path.as_ref() == Path::new("/project_b") {
                    second_repository = Some(repository.clone());
                }
            }

            (
                first_repository.expect("should have repository for /project_a"),
                second_repository.expect("should have repository for /project_b"),
            )
        });
        first_repository.update(cx, |repository, cx| repository.set_as_active_repository(cx));
        cx.run_until_parked();

        let (multi_workspace, cx) = cx.add_window_view(|window, cx| {
            workspace::MultiWorkspace::test_new(project.clone(), window, cx)
        });

        let workspace_weak =
            multi_workspace.read_with(&*cx, |multi, _| multi.workspace().downgrade());
        let git_graph = cx.new_window_entity(|window, cx| {
            GitGraph::new(
                first_repository.read(cx).id,
                project.read(cx).git_store().clone(),
                workspace_weak,
                None,
                window,
                cx,
            )
        });
        cx.run_until_parked();

        // Verify initial graph data is loaded
        let initial_commit_count =
            git_graph.read_with(&*cx, |graph, _| graph.graph_data.commits.len());
        assert!(
            initial_commit_count > 0,
            "graph data should have been loaded, got 0 commits"
        );

        git_graph.update(cx, |graph, cx| {
            graph.set_repo_id(second_repository.read(cx).id, cx)
        });
        cx.run_until_parked();

        let commit_count_after_clear =
            git_graph.read_with(&*cx, |graph, _| graph.graph_data.commits.len());
        assert_eq!(
            commit_count_after_clear, 0,
            "graph_data should be cleared after switching away"
        );

        git_graph.update(cx, |graph, cx| {
            graph.set_repo_id(first_repository.read(cx).id, cx)
        });
        cx.run_until_parked();

        cx.draw(
            point(px(0.), px(0.)),
            gpui::size(px(1200.), px(800.)),
            |_, _| git_graph.clone().into_any_element(),
        );
        cx.run_until_parked();

        // Verify graph data is reloaded from repository cache on switch back
        let reloaded_commit_count =
            git_graph.read_with(&*cx, |graph, _| graph.graph_data.commits.len());
        assert_eq!(
            reloaded_commit_count,
            commits.len(),
            "graph data should be reloaded after switching back"
        );
    }

    #[gpui::test]
    async fn test_file_history_action_uses_git_panel_and_editor_sources(cx: &mut TestAppContext) {
        init_test(cx);

        let fs = FakeFs::new(cx.executor());
        fs.insert_tree(
            Path::new(util::path!("/project")),
            json!({
                ".git": {},
                "tracked1.txt": "tracked 1",
                "tracked2.txt": "tracked 2",
            }),
        )
        .await;
        fs.set_status_for_repo(
            Path::new(util::path!("/project/.git")),
            &[
                ("tracked1.txt", StatusCode::Modified.worktree()),
                ("tracked2.txt", StatusCode::Modified.worktree()),
            ],
        );

        let commits = vec![Arc::new(InitialGraphCommitData {
            sha: Oid::from_bytes(&[1; 20]).unwrap(),
            parents: smallvec![],
            ref_names: vec!["HEAD".into(), "refs/heads/main".into()],
        })];
        fs.set_graph_commits(Path::new(util::path!("/project/.git")), commits);

        let project = Project::test(fs.clone(), [Path::new(util::path!("/project"))], cx).await;
        cx.run_until_parked();

        let repository = project.read_with(cx, |project, cx| {
            project
                .active_repository(cx)
                .expect("should have active repository")
        });
        let tracked1_repo_path = RepoPath::new(&"tracked1.txt").unwrap();
        let tracked2_repo_path = RepoPath::new(&"tracked2.txt").unwrap();
        let tracked1 = repository
            .read_with(cx, |repository, cx| {
                repository.repo_path_to_project_path(&tracked1_repo_path, cx)
            })
            .expect("tracked1 should resolve to project path");
        let tracked2 = repository
            .read_with(cx, |repository, cx| {
                repository.repo_path_to_project_path(&tracked2_repo_path, cx)
            })
            .expect("tracked2 should resolve to project path");

        let workspace_window = cx.add_window(|window, cx| {
            workspace::MultiWorkspace::test_new(project.clone(), window, cx)
        });
        let workspace = workspace_window
            .read_with(cx, |multi, _| multi.workspace().clone())
            .expect("workspace should exist");

        let (weak_workspace, async_window_cx) = workspace_window
            .update(cx, |multi, window, cx| {
                (multi.workspace().downgrade(), window.to_async(cx))
            })
            .expect("window should be available");
        cx.background_executor.allow_parking();
        let git_panel = cx
            .foreground_executor()
            .clone()
            .block_test(crate::git_panel::GitPanel::load(
                weak_workspace,
                async_window_cx,
            ))
            .expect("git panel should load");
        cx.background_executor.forbid_parking();

        workspace_window
            .update(cx, |multi, window, cx| {
                let workspace = multi.workspace();
                workspace.update(cx, |workspace, cx| {
                    workspace.add_panel(git_panel.clone(), window, cx);
                });
            })
            .expect("workspace window should be available");
        cx.executor().advance_clock(Duration::from_millis(100));
        cx.run_until_parked();

        workspace_window
            .update(cx, |_, window, cx| {
                git_panel.update(cx, |panel, cx| {
                    panel.select_entry_by_path(tracked1.clone(), window, cx);
                });
                git_panel.update(cx, |panel, cx| {
                    panel.focus_handle(cx).focus(window, cx);
                });
            })
            .expect("workspace window should be available");
        cx.run_until_parked();
        workspace_window
            .update(cx, |_, window, cx| {
                window.dispatch_action(Box::new(git::FileHistory), cx);
            })
            .expect("workspace window should be available");
        cx.run_until_parked();

        workspace.read_with(cx, |workspace, cx| {
            let graphs = workspace.items_of_type::<GitGraph>(cx).collect::<Vec<_>>();
            assert_eq!(graphs.len(), 1);
            assert_eq!(
                graphs[0].read(cx).log_source,
                LogSource::Path(tracked1_repo_path.clone())
            );
        });

        workspace_window
            .update(cx, |_, window, cx| {
                git_panel.update(cx, |panel, cx| {
                    panel.select_entry_by_path(tracked1.clone(), window, cx);
                });
                git_panel.update(cx, |panel, cx| {
                    panel.focus_handle(cx).focus(window, cx);
                });
            })
            .expect("workspace window should be available");
        cx.run_until_parked();
        workspace_window
            .update(cx, |_, window, cx| {
                window.dispatch_action(Box::new(git::FileHistory), cx);
            })
            .expect("workspace window should be available");
        cx.run_until_parked();

        workspace.read_with(cx, |workspace, cx| {
            let graphs = workspace.items_of_type::<GitGraph>(cx).collect::<Vec<_>>();
            assert_eq!(graphs.len(), 1);
            assert_eq!(
                graphs[0].read(cx).log_source,
                LogSource::Path(tracked1_repo_path.clone())
            );
        });

        let tracked1_buffer = project
            .update(cx, |project, cx| project.open_buffer(tracked1.clone(), cx))
            .await
            .expect("tracked1 buffer should open");
        let tracked2_buffer = project
            .update(cx, |project, cx| project.open_buffer(tracked2.clone(), cx))
            .await
            .expect("tracked2 buffer should open");
        workspace_window
            .update(cx, |multi, window, cx| {
                let workspace = multi.workspace();
                let multibuffer = cx.new(|cx| {
                    let mut multibuffer = editor::MultiBuffer::new(language::Capability::ReadWrite);
                    multibuffer.set_excerpts_for_buffer(
                        tracked1_buffer.clone(),
                        [Default::default()..tracked1_buffer.read(cx).max_point()],
                        0,
                        cx,
                    );
                    multibuffer.set_excerpts_for_buffer(
                        tracked2_buffer.clone(),
                        [Default::default()..tracked2_buffer.read(cx).max_point()],
                        0,
                        cx,
                    );
                    multibuffer
                });
                let editor = cx.new(|cx| {
                    Editor::for_multibuffer(multibuffer, Some(project.clone()), window, cx)
                });
                workspace.update(cx, |workspace, cx| {
                    workspace.add_item_to_active_pane(
                        Box::new(editor.clone()),
                        None,
                        true,
                        window,
                        cx,
                    );
                });
                editor.update(cx, |editor, cx| {
                    let snapshot = editor.buffer().read(cx).snapshot(cx);
                    let second_excerpt_point = snapshot
                        .range_for_buffer(tracked2_buffer.read(cx).remote_id())
                        .expect("tracked2 excerpt should exist")
                        .start;
                    let anchor = snapshot.anchor_before(second_excerpt_point);
                    editor.change_selections(
                        editor::SelectionEffects::no_scroll(),
                        window,
                        cx,
                        |selections| {
                            selections.select_anchor_ranges([anchor..anchor]);
                        },
                    );
                    window.focus(&editor.focus_handle(cx), cx);
                });
            })
            .expect("workspace window should be available");
        cx.run_until_parked();

        workspace_window
            .update(cx, |_, window, cx| {
                window.dispatch_action(Box::new(git::FileHistory), cx);
            })
            .expect("workspace window should be available");
        cx.run_until_parked();

        workspace.read_with(cx, |workspace, cx| {
            let graphs = workspace.items_of_type::<GitGraph>(cx).collect::<Vec<_>>();
            assert_eq!(graphs.len(), 2);
            let latest = graphs
                .into_iter()
                .max_by_key(|graph| graph.entity_id())
                .expect("expected a git graph");
            assert_eq!(
                latest.read(cx).log_source,
                LogSource::Path(tracked2_repo_path)
            );
        });
    }

    #[gpui::test]
    fn test_serialized_state_roundtrip(_cx: &mut TestAppContext) {
        use persistence::SerializedGitGraphState;

        let path = RepoPath::new(&"src/main.rs").unwrap();
        let sha = Oid::from_bytes(&[0xab; 20]).unwrap();

        let state = SerializedGitGraphState {
            log_source_type: Some(persistence::LOG_SOURCE_PATH),
            log_source_value: Some("src/main.rs".to_string()),
            log_order: Some(persistence::LOG_ORDER_TOPO),
            selected_sha: Some(sha.to_string()),
            search_query: Some("fix bug".to_string()),
            search_case_sensitive: Some(true),
        };

        assert_eq!(
            persistence::deserialize_log_source(&state),
            LogSource::Path(path)
        );
        assert!(matches!(
            persistence::deserialize_log_order(&state),
            LogOrder::TopoOrder
        ));
        assert_eq!(
            state.selected_sha.as_deref(),
            Some(sha.to_string()).as_deref()
        );
        assert_eq!(state.search_query.as_deref(), Some("fix bug"));
        assert_eq!(state.search_case_sensitive, Some(true));

        let all_state = SerializedGitGraphState {
            log_source_type: Some(persistence::LOG_SOURCE_ALL),
            log_source_value: None,
            log_order: Some(persistence::LOG_ORDER_DATE),
            selected_sha: None,
            search_query: None,
            search_case_sensitive: None,
        };
        assert_eq!(
            persistence::deserialize_log_source(&all_state),
            LogSource::All
        );
        assert!(matches!(
            persistence::deserialize_log_order(&all_state),
            LogOrder::DateOrder
        ));

        let branch_state = SerializedGitGraphState {
            log_source_type: Some(persistence::LOG_SOURCE_BRANCH),
            log_source_value: Some("refs/heads/main".to_string()),
            ..Default::default()
        };
        assert_eq!(
            persistence::deserialize_log_source(&branch_state),
            LogSource::Branch("refs/heads/main".into())
        );

        let sha_state = SerializedGitGraphState {
            log_source_type: Some(persistence::LOG_SOURCE_SHA),
            log_source_value: Some(sha.to_string()),
            ..Default::default()
        };
        assert_eq!(
            persistence::deserialize_log_source(&sha_state),
            LogSource::Sha(sha)
        );

        let empty_state = SerializedGitGraphState::default();
        assert_eq!(
            persistence::deserialize_log_source(&empty_state),
            LogSource::All
        );
        assert!(matches!(
            persistence::deserialize_log_order(&empty_state),
            LogOrder::DateOrder
        ));
    }

    #[gpui::test]
    async fn test_git_graph_state_persists_across_serialization_roundtrip(cx: &mut TestAppContext) {
        init_test(cx);

        let fs = FakeFs::new(cx.executor());
        fs.insert_tree(
            Path::new("/project"),
            json!({
                ".git": {},
                "file.txt": "content",
            }),
        )
        .await;

        let mut rng = StdRng::seed_from_u64(99);
        let commits = generate_random_commit_dag(&mut rng, 20, false);
        fs.set_graph_commits(Path::new("/project/.git"), commits.clone());

        let project = Project::test(fs.clone(), [Path::new("/project")], cx).await;
        cx.run_until_parked();

        let repository = project.read_with(cx, |project, cx| {
            project
                .active_repository(cx)
                .expect("should have a repository")
        });

        let (multi_workspace, cx) = cx.add_window_view(|window, cx| {
            workspace::MultiWorkspace::test_new(project.clone(), window, cx)
        });
        let workspace_weak =
            multi_workspace.read_with(&*cx, |multi, _| multi.workspace().downgrade());

        let git_graph = cx.new_window_entity(|window, cx| {
            GitGraph::new(
                repository.read(cx).id,
                project.read(cx).git_store().clone(),
                workspace_weak.clone(),
                None,
                window,
                cx,
            )
        });
        cx.run_until_parked();

        cx.draw(
            point(px(0.), px(0.)),
            gpui::size(px(1200.), px(800.)),
            |_, _| git_graph.clone().into_any_element(),
        );
        cx.run_until_parked();

        let commit_count = git_graph.read_with(&*cx, |graph, _| graph.graph_data.commits.len());
        assert!(commit_count > 0, "graph should have loaded commits, got 0");

        let target_sha = commits[5].sha;
        git_graph.update(cx, |graph, _| {
            graph.selected_entry_idx = Some(5);
        });

        let selected_sha = git_graph.read_with(&*cx, |graph, _| {
            graph
                .selected_entry_idx
                .and_then(|idx| graph.graph_data.commits.get(idx))
                .map(|c| c.data.sha.to_string())
        });
        assert_eq!(selected_sha, Some(target_sha.to_string()));

        let item_id = workspace::ItemId::from(999_u64);
        let workspace_db = cx.read(|cx| workspace::WorkspaceDb::global(cx));
        let workspace_id = workspace_db
            .next_id()
            .await
            .expect("should create workspace id");
        let db = cx.read(|cx| persistence::GitGraphsDb::global(cx));
        db.save_git_graph(
            item_id,
            workspace_id,
            "/project".to_string(),
            Some(persistence::LOG_SOURCE_ALL),
            None,
            Some(persistence::LOG_ORDER_DATE),
            selected_sha.clone(),
            Some("some query".to_string()),
            Some(true),
        )
        .await
        .expect("save should succeed");

        let restored_graph = cx
            .update(|window, cx| {
                <GitGraph as workspace::SerializableItem>::deserialize(
                    project.clone(),
                    workspace_weak,
                    workspace_id,
                    item_id,
                    window,
                    cx,
                )
            })
            .await
            .expect("deserialization should succeed");
        cx.run_until_parked();

        cx.draw(
            point(px(0.), px(0.)),
            gpui::size(px(1200.), px(800.)),
            |_, _| restored_graph.clone().into_any_element(),
        );
        cx.run_until_parked();

        let restored_commit_count =
            restored_graph.read_with(&*cx, |graph, _| graph.graph_data.commits.len());
        assert_eq!(
            restored_commit_count, commit_count,
            "restored graph should have the same number of commits"
        );

        restored_graph.read_with(&*cx, |graph, _| {
            assert_eq!(
                graph.log_source,
                LogSource::All,
                "log_source should be restored"
            );

            let restored_selected_sha = graph
                .selected_entry_idx
                .and_then(|idx| graph.graph_data.commits.get(idx))
                .map(|c| c.data.sha.to_string());
            assert_eq!(
                restored_selected_sha, selected_sha,
                "selected commit should be restored via pending_select_sha"
            );

            assert_eq!(
                graph.search_state.case_sensitive, true,
                "search case sensitivity should be restored"
            );
        });

        restored_graph.read_with(&*cx, |graph, cx| {
            let editor_text = graph.search_state.editor.read(cx).text(cx);
            assert_eq!(
                editor_text, "some query",
                "search query text should be restored in editor"
            );
        });
    }

    #[gpui::test]
    async fn test_git_graph_search_matches_commit_hash_prefix(cx: &mut TestAppContext) {
        init_test(cx);

        let fs = FakeFs::new(cx.executor());
        fs.insert_tree(
            Path::new("/project"),
            json!({
                ".git": {},
                "file.txt": "content",
            }),
        )
        .await;

        let first_sha = Oid::from_bytes(&[1; 20]).unwrap();
        let target_sha = Oid::from_bytes(&[2; 20]).unwrap();
        let third_sha = Oid::from_bytes(&[3; 20]).unwrap();
        let commits = vec![
            Arc::new(InitialGraphCommitData {
                sha: first_sha,
                parents: smallvec![target_sha],
                ref_names: vec!["HEAD".into(), "refs/heads/main".into()],
            }),
            Arc::new(InitialGraphCommitData {
                sha: target_sha,
                parents: smallvec![third_sha],
                ref_names: vec![],
            }),
            Arc::new(InitialGraphCommitData {
                sha: third_sha,
                parents: smallvec![],
                ref_names: vec![],
            }),
        ];
        fs.set_graph_commits(Path::new("/project/.git"), commits);
        fs.set_commit_data(
            Path::new("/project/.git"),
            [
                (
                    CommitData {
                        sha: first_sha,
                        parents: smallvec![target_sha],
                        author_name: "Author".into(),
                        author_email: "author@example.com".into(),
                        commit_timestamp: 1,
                        subject: "Add feature".into(),
                        message: "Add feature".into(),
                    },
                    false,
                ),
                (
                    CommitData {
                        sha: target_sha,
                        parents: smallvec![third_sha],
                        author_name: "Author".into(),
                        author_email: "author@example.com".into(),
                        commit_timestamp: 2,
                        subject: "Fix branch loading".into(),
                        message: "Fix branch loading".into(),
                    },
                    false,
                ),
                (
                    CommitData {
                        sha: third_sha,
                        parents: smallvec![],
                        author_name: "Author".into(),
                        author_email: "author@example.com".into(),
                        commit_timestamp: 3,
                        subject: "Update docs".into(),
                        message: "Update docs".into(),
                    },
                    false,
                ),
            ],
        );

        let project = Project::test(fs.clone(), [Path::new("/project")], cx).await;
        cx.run_until_parked();

        let repository = project.read_with(cx, |project, cx| {
            project
                .active_repository(cx)
                .expect("should have a repository")
        });
        let (multi_workspace, cx) = cx.add_window_view(|window, cx| {
            workspace::MultiWorkspace::test_new(project.clone(), window, cx)
        });
        let workspace_weak =
            multi_workspace.read_with(&*cx, |multi, _| multi.workspace().downgrade());
        let git_graph = cx.new_window_entity(|window, cx| {
            GitGraph::new(
                repository.read(cx).id,
                project.read(cx).git_store().clone(),
                workspace_weak,
                None,
                window,
                cx,
            )
        });
        cx.run_until_parked();

        git_graph.update(cx, |graph, cx| {
            graph.search_for_test("0202020".into(), cx);
        });
        cx.run_until_parked();

        git_graph.read_with(&*cx, |graph, _| {
            assert_eq!(graph.search_matches_for_test(), vec![target_sha]);
            let selected_sha = graph
                .selected_entry_idx
                .and_then(|idx| graph.graph_data.commits.get(idx))
                .map(|commit| commit.data.sha);
            assert_eq!(selected_sha, Some(target_sha));
        });

        git_graph.update(cx, |graph, cx| {
            graph.search_for_test("docs".into(), cx);
        });
        cx.run_until_parked();

        git_graph.read_with(&*cx, |graph, _| {
            assert_eq!(graph.search_matches_for_test(), vec![third_sha]);
        });
    }

    #[gpui::test]
    async fn test_graph_data_reloaded_after_stash_change(cx: &mut TestAppContext) {
        init_test(cx);

        let fs = FakeFs::new(cx.executor());
        fs.insert_tree(
            Path::new("/project"),
            json!({
                ".git": {},
                "file.txt": "content",
            }),
        )
        .await;

        let initial_head = Oid::from_bytes(&[1; 20]).unwrap();
        let initial_stash = Oid::from_bytes(&[2; 20]).unwrap();
        let updated_head = Oid::from_bytes(&[3; 20]).unwrap();
        let updated_stash = Oid::from_bytes(&[4; 20]).unwrap();

        fs.set_graph_commits(
            Path::new("/project/.git"),
            vec![
                Arc::new(InitialGraphCommitData {
                    sha: initial_head,
                    parents: smallvec![initial_stash],
                    ref_names: vec!["HEAD".into(), "refs/heads/main".into()],
                }),
                Arc::new(InitialGraphCommitData {
                    sha: initial_stash,
                    parents: smallvec![],
                    ref_names: vec!["refs/stash".into()],
                }),
            ],
        );
        fs.with_git_state(Path::new("/project/.git"), true, |state| {
            state.stash_entries = git::stash::GitStash {
                entries: vec![git::stash::StashEntry {
                    index: 0,
                    oid: initial_stash,
                    message: "initial stash".to_string(),
                    branch: Some("main".to_string()),
                    timestamp: 1,
                }]
                .into(),
            };
        })
        .unwrap();

        let project = Project::test(fs.clone(), [Path::new("/project")], cx).await;
        cx.run_until_parked();

        let repository = project.read_with(cx, |project, cx| {
            project
                .active_repository(cx)
                .expect("should have a repository")
        });

        let (multi_workspace, cx) = cx.add_window_view(|window, cx| {
            workspace::MultiWorkspace::test_new(project.clone(), window, cx)
        });
        let workspace_weak =
            multi_workspace.read_with(&*cx, |multi, _| multi.workspace().downgrade());
        let git_graph = cx.new_window_entity(|window, cx| {
            GitGraph::new(
                repository.read(cx).id,
                project.read(cx).git_store().clone(),
                workspace_weak,
                None,
                window,
                cx,
            )
        });
        cx.run_until_parked();

        let initial_shas = git_graph.read_with(&*cx, |graph, _| {
            graph
                .graph_data
                .commits
                .iter()
                .map(|commit| commit.data.sha)
                .collect::<Vec<_>>()
        });
        assert_eq!(initial_shas, vec![initial_head, initial_stash]);

        fs.set_graph_commits(
            Path::new("/project/.git"),
            vec![
                Arc::new(InitialGraphCommitData {
                    sha: updated_head,
                    parents: smallvec![updated_stash],
                    ref_names: vec!["HEAD".into(), "refs/heads/main".into()],
                }),
                Arc::new(InitialGraphCommitData {
                    sha: updated_stash,
                    parents: smallvec![],
                    ref_names: vec!["refs/stash".into()],
                }),
            ],
        );
        fs.with_git_state(Path::new("/project/.git"), true, |state| {
            state.stash_entries = git::stash::GitStash {
                entries: vec![git::stash::StashEntry {
                    index: 0,
                    oid: updated_stash,
                    message: "updated stash".to_string(),
                    branch: Some("main".to_string()),
                    timestamp: 1,
                }]
                .into(),
            };
        })
        .unwrap();

        project
            .update(cx, |project, cx| project.git_scans_complete(cx))
            .await;
        cx.run_until_parked();

        cx.draw(
            point(px(0.), px(0.)),
            gpui::size(px(1200.), px(800.)),
            |_, _| git_graph.clone().into_any_element(),
        );
        cx.run_until_parked();

        let reloaded_shas = git_graph.read_with(&*cx, |graph, _| {
            graph
                .graph_data
                .commits
                .iter()
                .map(|commit| commit.data.sha)
                .collect::<Vec<_>>()
        });
        assert_eq!(reloaded_shas, vec![updated_head, updated_stash]);
    }

    #[gpui::test]
    async fn test_row_height_matches_uniform_list_item_height(cx: &mut TestAppContext) {
        init_test(cx);

        cx.update(|cx| {
            SettingsStore::update_global(cx, |store, cx| {
                store.update_user_settings(cx, |settings| {
                    *settings.theme = ThemeSettingsContent {
                        ui_font_size: Some(12.7.into()),
                        ..Default::default()
                    }
                });
            })
        });

        let fs = FakeFs::new(cx.executor());
        fs.insert_tree(
            Path::new("/project"),
            serde_json::json!({
                ".git": {},
                "file.txt": "content",
            }),
        )
        .await;

        let mut rng = StdRng::seed_from_u64(99);
        let commits = generate_random_commit_dag(&mut rng, 20, false);
        fs.set_graph_commits(Path::new("/project/.git"), commits);

        let project = Project::test(fs.clone(), [Path::new("/project")], cx).await;
        cx.run_until_parked();

        let repository = project.read_with(cx, |project, cx| {
            project
                .active_repository(cx)
                .expect("should have a repository")
        });

        let (multi_workspace, cx) = cx.add_window_view(|window, cx| {
            workspace::MultiWorkspace::test_new(project.clone(), window, cx)
        });

        let workspace_weak =
            multi_workspace.read_with(&*cx, |multi, _| multi.workspace().downgrade());

        let git_graph = cx.new_window_entity(|window, cx| {
            GitGraph::new(
                repository.read(cx).id,
                project.read(cx).git_store().clone(),
                workspace_weak,
                None,
                window,
                cx,
            )
        });
        cx.run_until_parked();

        cx.draw(
            point(px(0.), px(0.)),
            gpui::size(px(1200.), px(800.)),
            |_, _| git_graph.clone().into_any_element(),
        );
        cx.run_until_parked();

        git_graph.update_in(cx, |graph, window, cx| {
            let commit_count = graph.graph_data.commits.len();
            assert!(
                commit_count > 0,
                "need at least one commit to measure item height"
            );

            let table_state = graph.table_interaction_state.read(cx);
            let item_size = table_state.scroll_handle.0.borrow().last_item_size.expect(
                "uniform_list should have populated last_item_size after draw(); \
                     the table has not been laid out",
            );

            let measured_item_height = item_size.contents.height / commit_count as f32;
            let computed_row_height = GitGraph::row_height(window, cx);

            assert_eq!(
                computed_row_height, measured_item_height,
                "GitGraph::row_height ({}) must exactly match the height that \
                 uniform_list measured for each table row ({}). \
                 A mismatch means the canvas and table rows will drift when scrolling.",
                computed_row_height, measured_item_height,
            );
        });
    }

    #[gpui::test]
    async fn test_copy_selected_commit_tag_with_one_tag_copies_to_clipboard(
        cx: &mut TestAppContext,
    ) {
        init_test(cx);

        let fs = FakeFs::new(cx.executor());
        fs.insert_tree(
            Path::new("/project"),
            serde_json::json!({
                ".git": {},
                "file.txt": "content",
            }),
        )
        .await;

        let commit_sha = Oid::from_bytes(&[1; 20]).unwrap();
        let commits = vec![Arc::new(InitialGraphCommitData {
            sha: commit_sha,
            parents: smallvec![],
            ref_names: vec![
                SharedString::from("HEAD -> main"),
                SharedString::from("origin/main"),
                SharedString::from("tag: v1.0.0"),
            ],
        })];
        fs.set_graph_commits(Path::new("/project/.git"), commits);

        let project = Project::test(fs.clone(), [Path::new("/project")], cx).await;
        cx.run_until_parked();

        let repository = project.read_with(cx, |project, cx| {
            project
                .active_repository(cx)
                .expect("should have a repository")
        });

        let (multi_workspace, cx) = cx.add_window_view(|window, cx| {
            workspace::MultiWorkspace::test_new(project.clone(), window, cx)
        });
        let workspace = multi_workspace.read_with(&*cx, |multi, _| multi.workspace().clone());
        let workspace_weak = workspace.downgrade();

        let git_graph = cx.new_window_entity(|window, cx| {
            GitGraph::new(
                repository.read(cx).id,
                project.read(cx).git_store().clone(),
                workspace_weak,
                None,
                window,
                cx,
            )
        });
        cx.run_until_parked();

        git_graph.update_in(cx, |graph, window, cx| {
            assert_eq!(graph.graph_data.commits.len(), 1);
            graph.selected_entry_idx = Some(0);
            graph.copy_selected_commit_tag(&CopyCommitTag, window, cx);
        });

        assert_eq!(
            cx.read_from_clipboard().and_then(|item| item.text()),
            Some("v1.0.0".to_string())
        );
    }

    #[gpui::test]
    async fn test_copy_selected_commit_tag_with_multiple_tags_opens_picker_and_copies_selected_tag(
        cx: &mut TestAppContext,
    ) {
        init_test(cx);

        let fs = FakeFs::new(cx.executor());
        fs.insert_tree(
            Path::new("/project"),
            serde_json::json!({
                ".git": {},
                "file.txt": "content",
            }),
        )
        .await;

        let commit_sha = Oid::from_bytes(&[1; 20]).unwrap();
        let commits = vec![Arc::new(InitialGraphCommitData {
            sha: commit_sha,
            parents: smallvec![],
            ref_names: vec![
                SharedString::from("HEAD -> main"),
                SharedString::from("origin/main"),
                SharedString::from("tag: v1.0.0"),
                SharedString::from("tag: v1.1.0"),
            ],
        })];
        fs.set_graph_commits(Path::new("/project/.git"), commits);

        let project = Project::test(fs.clone(), [Path::new("/project")], cx).await;
        cx.run_until_parked();

        let repository = project.read_with(cx, |project, cx| {
            project
                .active_repository(cx)
                .expect("should have a repository")
        });

        let (multi_workspace, cx) = cx.add_window_view(|window, cx| {
            workspace::MultiWorkspace::test_new(project.clone(), window, cx)
        });
        let workspace = multi_workspace.read_with(&*cx, |multi, _| multi.workspace().clone());
        let workspace_weak = workspace.downgrade();

        let git_graph = cx.new_window_entity(|window, cx| {
            GitGraph::new(
                repository.read(cx).id,
                project.read(cx).git_store().clone(),
                workspace_weak,
                None,
                window,
                cx,
            )
        });
        cx.run_until_parked();

        git_graph.update_in(cx, |graph, window, cx| {
            assert_eq!(graph.graph_data.commits.len(), 1);
            graph.selected_entry_idx = Some(0);
            graph.copy_selected_commit_tag(&CopyCommitTag, window, cx);
        });

        // Ensure that nothing has been copied at this point
        assert_eq!(cx.read_from_clipboard().and_then(|item| item.text()), None);

        let picker = workspace.update(cx, |workspace, cx| {
            workspace
                .active_modal::<CommitTagPicker>(cx)
                .expect("commit tag picker is not open")
                .read(cx)
                .picker
                .clone()
        });

        picker.read_with(cx, |picker, _| {
            assert_eq!(picker.delegate.selected_index, 0);
            assert_eq!(
                picker.delegate.tag_names,
                [SharedString::from("v1.0.0"), SharedString::from("v1.1.0")]
            );
        });

        cx.dispatch_action(menu::Confirm);
        cx.run_until_parked();

        assert_eq!(
            cx.read_from_clipboard().and_then(|item| item.text()),
            Some("v1.0.0".to_string())
        );
    }

    #[gpui::test]
    async fn test_open_at_commit_reuses_loaded_graph(cx: &mut TestAppContext) {
        init_test(cx);

        let fs = FakeFs::new(cx.executor());
        fs.insert_tree(
            Path::new("/project"),
            json!({ ".git": {}, "file.txt": "content" }),
        )
        .await;

        let first_sha = Oid::from_bytes(&[1; 20]).expect("valid commit SHA");
        let second_sha = Oid::from_bytes(&[2; 20]).expect("valid commit SHA");
        fs.set_graph_commits(
            Path::new("/project/.git"),
            vec![
                Arc::new(InitialGraphCommitData {
                    sha: second_sha,
                    parents: smallvec![first_sha],
                    ref_names: vec!["HEAD -> main".into()],
                }),
                Arc::new(InitialGraphCommitData {
                    sha: first_sha,
                    parents: smallvec![],
                    ref_names: Vec::new(),
                }),
            ],
        );
        fs.set_commit_data(
            Path::new("/project/.git"),
            [first_sha, second_sha].map(|sha| {
                (
                    CommitData {
                        sha,
                        parents: smallvec![],
                        author_name: "Author".into(),
                        author_email: "author@example.com".into(),
                        commit_timestamp: 1_700_000_000,
                        subject: "Commit subject".into(),
                        message: "Commit message".into(),
                    },
                    false,
                )
            }),
        );

        let project = Project::test(fs, [Path::new("/project")], cx).await;
        cx.run_until_parked();

        let repository = project.read_with(cx, |project, cx| {
            project
                .active_repository(cx)
                .expect("should have a repository")
        });
        let (multi_workspace, cx) = cx.add_window_view(|window, cx| {
            workspace::MultiWorkspace::test_new(project.clone(), window, cx)
        });
        let workspace = multi_workspace.read_with(&*cx, |multi, _| multi.workspace().clone());
        let git_graph = cx.new_window_entity(|window, cx| {
            GitGraph::new(
                repository.read(cx).id,
                project.read(cx).git_store().clone(),
                workspace.downgrade(),
                None,
                window,
                cx,
            )
        });
        workspace.update_in(cx, |workspace, window, cx| {
            workspace.add_item_to_active_pane(Box::new(git_graph.clone()), None, true, window, cx);
        });
        cx.run_until_parked();

        git_graph.update(cx, |graph, cx| {
            graph.select_commit_by_sha(first_sha, cx);
        });
        cx.run_until_parked();
        git_graph.update(cx, |graph, cx| {
            graph.select_commit_by_sha(second_sha, cx);
        });
        cx.run_until_parked();

        workspace.update_in(cx, |workspace, window, cx| {
            open_or_reuse_graph(
                workspace,
                repository.read(cx).id,
                project.read(cx).git_store().clone(),
                LogSource::All,
                Some(first_sha.to_string()),
                window,
                cx,
            );
        });
        cx.run_until_parked();

        git_graph.read_with(&*cx, |graph, _| {
            assert_eq!(graph.selected_entry_idx, Some(1));
        });
    }

    #[gpui::test]
    async fn test_git_graph_navigation(cx: &mut TestAppContext) {
        init_test(cx);

        let fs = FakeFs::new(cx.executor());
        fs.insert_tree(
            Path::new("/project"),
            serde_json::json!({
                ".git": {},
                "file.txt": "content",
            }),
        )
        .await;

        let mut rng = StdRng::seed_from_u64(42);
        let commits = generate_random_commit_dag(&mut rng, 10, false);
        fs.set_graph_commits(Path::new("/project/.git"), commits);

        let project = Project::test(fs.clone(), [Path::new("/project")], cx).await;
        cx.run_until_parked();

        let repository = project.read_with(cx, |project, cx| {
            project
                .active_repository(cx)
                .expect("should have a repository")
        });

        let (multi_workspace, cx) = cx.add_window_view(|window, cx| {
            workspace::MultiWorkspace::test_new(project.clone(), window, cx)
        });

        let workspace = multi_workspace.read_with(&*cx, |multi, _| multi.workspace().clone());
        let workspace_weak = workspace.downgrade();

        let git_graph = cx.new_window_entity(|window, cx| {
            GitGraph::new(
                repository.read(cx).id,
                project.read(cx).git_store().clone(),
                workspace_weak,
                None,
                window,
                cx,
            )
        });
        cx.run_until_parked();

        workspace.update_in(cx, |workspace, window, cx| {
            workspace.add_item_to_active_pane(Box::new(git_graph.clone()), None, true, window, cx);
        });
        cx.run_until_parked();

        git_graph.update_in(cx, |graph, window, cx| {
            graph.focus_handle(cx).focus(window, cx);
        });
        cx.run_until_parked();

        cx.draw(
            point(px(0.), px(0.)),
            gpui::size(px(1200.), px(800.)),
            |_, _| multi_workspace.clone().into_any_element(),
        );
        cx.run_until_parked();

        git_graph.update_in(cx, |graph, window, cx| {
            graph.focus_handle(cx).focus(window, cx);
        });
        cx.run_until_parked();

        git_graph.read_with(&*cx, |graph, _| {
            assert_eq!(graph.graph_data.commits.len(), 10);
        });
        git_graph.read_with(&*cx, |graph, _| {
            assert_eq!(graph.selected_entry_idx, None);
        });

        git_graph.update_in(cx, |graph, window, cx| {
            graph.select_first(&menu::SelectFirst, window, cx);
        });
        cx.run_until_parked();
        git_graph.read_with(&*cx, |graph, _| {
            assert_eq!(graph.selected_entry_idx, Some(0));
        });

        let scroll_step = git_graph.update_in(cx, |graph, window, cx| {
            (graph.visible_row_count(window, cx) / 2).max(1)
        });

        cx.dispatch_action(ScrollDown);
        cx.run_until_parked();
        git_graph.read_with(&*cx, |graph, _| {
            assert_eq!(graph.selected_entry_idx, Some(scroll_step));
        });

        cx.dispatch_action(ScrollUp);
        cx.run_until_parked();
        git_graph.read_with(&*cx, |graph, _| {
            assert_eq!(graph.selected_entry_idx, Some(0));
        });

        git_graph.update_in(cx, |graph, window, cx| {
            graph.select_next(&menu::SelectNext, window, cx);
        });
        cx.run_until_parked();
        git_graph.read_with(&*cx, |graph, _| {
            assert_eq!(graph.selected_entry_idx, Some(1));
        });

        git_graph.update_in(cx, |graph, window, cx| {
            graph.select_prev(&menu::SelectPrevious, window, cx);
        });
        cx.run_until_parked();
        git_graph.read_with(&*cx, |graph, _| {
            assert_eq!(graph.selected_entry_idx, Some(0));
        });

        git_graph.update_in(cx, |graph, window, cx| {
            graph.select_last(&menu::SelectLast, window, cx);
        });
        cx.run_until_parked();
        git_graph.read_with(&*cx, |graph, _| {
            assert_eq!(graph.selected_entry_idx, Some(9));
        });

        cx.dispatch_action(ScrollDown);
        cx.run_until_parked();
        git_graph.read_with(&*cx, |graph, _| {
            assert_eq!(graph.selected_entry_idx, Some(9));
        });

        git_graph.update_in(cx, |graph, window, cx| {
            graph.select_next(&menu::SelectNext, window, cx);
        });
        cx.run_until_parked();
        git_graph.read_with(&*cx, |graph, _| {
            assert_eq!(graph.selected_entry_idx, Some(9));
        });

        git_graph.update_in(cx, |graph, window, cx| {
            graph.select_prev(&menu::SelectPrevious, window, cx);
        });
        cx.run_until_parked();
        git_graph.read_with(&*cx, |graph, _| {
            assert_eq!(graph.selected_entry_idx, Some(8));
        });

        git_graph.update(cx, |graph, cx| {
            graph.selected_entry_idx = None;
            cx.notify();
        });
        cx.run_until_parked();
        git_graph.update_in(cx, |graph, window, cx| {
            graph.select_prev(&menu::SelectPrevious, window, cx);
        });
        cx.run_until_parked();
        git_graph.read_with(&*cx, |graph, _| {
            assert_eq!(graph.selected_entry_idx, Some(0));
        });

        git_graph.update(cx, |graph, cx| {
            graph.selected_entry_idx = None;
            cx.notify();
        });
        cx.run_until_parked();
        git_graph.update_in(cx, |graph, window, cx| {
            graph.select_next(&menu::SelectNext, window, cx);
        });
        cx.run_until_parked();
        git_graph.read_with(&*cx, |graph, _| {
            assert_eq!(graph.selected_entry_idx, Some(0));
        });
    }

    #[gpui::test]
    async fn test_global_git_command_task_runs_from_context_menu(cx: &mut TestAppContext) {
        init_test(cx);

        let fs = FakeFs::new(cx.executor());
        fs.insert_tree(
            Path::new("/project"),
            json!({
                ".git": {},
                "file.txt": "content",
            }),
        )
        .await;

        let commit_sha = Oid::try_from("abcdef1234567890abcdef1234567890abcdef12")
            .expect("commit SHA should be valid");
        fs.set_graph_commits(
            Path::new("/project/.git"),
            vec![Arc::new(InitialGraphCommitData {
                sha: commit_sha,
                parents: SmallVec::new(),
                ref_names: Vec::new(),
            })],
        );

        let project = Project::test(fs.clone(), [Path::new("/project")], cx).await;
        cx.run_until_parked();

        let repository = project.read_with(cx, |project, cx| {
            project
                .active_repository(cx)
                .expect("project should have an active repository")
        });
        let task_inventory = project.read_with(cx, |project, cx| {
            project
                .task_store()
                .read(cx)
                .task_inventory()
                .cloned()
                .expect("project should have a task inventory")
        });

        task_inventory.update(cx, |inventory, _| {
            inventory
                .update_file_based_tasks(
                    TaskSettingsLocation::Global(Path::new("/tasks.json")),
                    Some(
                        &serde_json::to_string(&json!([
                            // Tagged global task that should be scheduled from the Git graph context menu.
                            {
                                "label": "Git Show $ZED_GIT_SHA_SHORT",
                                "command": "git",
                                "args": ["show", "$ZED_GIT_SHA"],
                                "cwd": "$ZED_GIT_REPOSITORY_PATH",
                                "env": {
                                    "REPOSITORY": "$ZED_GIT_REPOSITORY_NAME",
                                },
                                "tags": [GIT_COMMAND_TASK_TAG],
                            },
                            // Untagged task that should not appear in the Git graph context menu.
                            {
                                "label": "Git Status",
                                "command": "git",
                                "args": ["status"],
                            },
                            // Tagged task that still should not appear because Git graph task contexts
                            // do not provide editor-specific variables.
                            {
                                "label": "Print File $ZED_FILE",
                                "command": "echo",
                                "args": ["$ZED_FILE"],
                                "tags": [GIT_COMMAND_TASK_TAG],
                            },
                        ]))
                        .expect("tasks JSON should serialize"),
                    ),
                )
                .expect("tasks should parse");
        });

        let (multi_workspace, cx) = cx.add_window_view(|window, cx| {
            workspace::MultiWorkspace::test_new(project.clone(), window, cx)
        });
        let workspace = multi_workspace.read_with(&*cx, |multi_workspace, _| {
            multi_workspace.workspace().clone()
        });
        let workspace_weak = workspace.downgrade();

        let git_graph = cx.new_window_entity(|window, cx| {
            GitGraph::new(
                repository.read(cx).id,
                project.read(cx).git_store().clone(),
                workspace_weak,
                None,
                window,
                cx,
            )
        });
        workspace.update_in(cx, |workspace, window, cx| {
            workspace.add_item_to_active_pane(Box::new(git_graph.clone()), None, true, window, cx);
        });
        cx.run_until_parked();

        git_graph.update_in(cx, |git_graph, window, cx| {
            assert_eq!(git_graph.graph_data.commits.len(), 1);
            git_graph.deploy_entry_context_menu(point(px(20.), px(20.)), 0, None, window, cx);
        });
        cx.run_until_parked();

        let context_menu = git_graph.read_with(&*cx, |git_graph, _| {
            git_graph
                .context_menu
                .as_ref()
                .expect("context menu should be open")
                .menu
                .clone()
        });
        context_menu.update_in(cx, |context_menu, window, cx| {
            context_menu
                .select_last(window, cx)
                .expect("custom Git task should be selectable");
            context_menu.confirm(&menu::Confirm, window, cx);
        });
        cx.run_until_parked();

        let (task_source_kind, resolved_task) = task_inventory.read_with(&*cx, |inventory, _| {
            inventory
                .last_scheduled_task(None)
                .expect("custom Git task should be scheduled")
        });

        assert!(
            matches!(task_source_kind, TaskSourceKind::AbsPath { .. }),
            "scheduled task should come from global tasks"
        );
        assert_eq!(resolved_task.resolved_label, "Git Show abcdef1");
        assert_eq!(resolved_task.resolved.command, Some("git".to_string()));
        assert_eq!(
            resolved_task.resolved.args,
            vec![
                "show".to_string(),
                "abcdef1234567890abcdef1234567890abcdef12".to_string(),
            ]
        );
        assert_eq!(
            resolved_task.resolved.cwd,
            Some(Path::new("/project").to_path_buf())
        );
        assert_eq!(
            resolved_task.resolved.env.get("REPOSITORY"),
            Some(&"project".to_string())
        );
    }

    #[gpui::test]
    async fn test_global_git_command_task_runs_from_ref_context_menu(cx: &mut TestAppContext) {
        init_test(cx);

        let fs = FakeFs::new(cx.executor());
        fs.insert_tree(
            Path::new("/project"),
            json!({
                ".git": {},
                "file.txt": "content",
            }),
        )
        .await;

        let commit_sha = Oid::try_from("abcdef1234567890abcdef1234567890abcdef12")
            .expect("commit SHA should be valid");
        fs.set_graph_commits(
            Path::new("/project/.git"),
            vec![Arc::new(InitialGraphCommitData {
                sha: commit_sha,
                parents: SmallVec::new(),
                ref_names: vec!["HEAD -> feature-x".into()],
            })],
        );

        let project = Project::test(fs.clone(), [Path::new("/project")], cx).await;
        cx.run_until_parked();

        let repository = project.read_with(cx, |project, cx| {
            project
                .active_repository(cx)
                .expect("project should have an active repository")
        });
        let task_inventory = project.read_with(cx, |project, cx| {
            project
                .task_store()
                .read(cx)
                .task_inventory()
                .cloned()
                .expect("project should have a task inventory")
        });

        task_inventory.update(cx, |inventory, _| {
            inventory
                .update_file_based_tasks(
                    TaskSettingsLocation::Global(Path::new("/tasks.json")),
                    Some(
                        &serde_json::to_string(&json!([
                            {
                                "label": "Check out $ZED_GIT_REF",
                                "command": "git",
                                "args": ["checkout", "$ZED_GIT_REF"],
                                "cwd": "$ZED_GIT_REPOSITORY_PATH",
                                "tags": [GIT_COMMAND_TASK_TAG],
                            },
                        ]))
                        .expect("tasks JSON should serialize"),
                    ),
                )
                .expect("tasks should parse");
        });

        let (multi_workspace, cx) = cx.add_window_view(|window, cx| {
            workspace::MultiWorkspace::test_new(project.clone(), window, cx)
        });
        let workspace = multi_workspace.read_with(&*cx, |multi_workspace, _| {
            multi_workspace.workspace().clone()
        });
        let workspace_weak = workspace.downgrade();

        let git_graph = cx.new_window_entity(|window, cx| {
            GitGraph::new(
                repository.read(cx).id,
                project.read(cx).git_store().clone(),
                workspace_weak,
                None,
                window,
                cx,
            )
        });
        workspace.update_in(cx, |workspace, window, cx| {
            workspace.add_item_to_active_pane(Box::new(git_graph.clone()), None, true, window, cx);
        });
        cx.run_until_parked();

        git_graph.update_in(cx, |git_graph, window, cx| {
            assert_eq!(git_graph.graph_data.commits.len(), 1);
            git_graph.deploy_entry_context_menu(
                point(px(20.), px(20.)),
                0,
                Some("feature-x".into()),
                window,
                cx,
            );
        });
        cx.run_until_parked();

        let context_menu = git_graph.read_with(&*cx, |git_graph, _| {
            git_graph
                .context_menu
                .as_ref()
                .expect("context menu should be open")
                .menu
                .clone()
        });
        context_menu.update_in(cx, |context_menu, window, cx| {
            context_menu
                .select_last(window, cx)
                .expect("custom Git task should be selectable");
            context_menu.confirm(&menu::Confirm, window, cx);
        });
        cx.run_until_parked();

        let (_task_source_kind, resolved_task) = task_inventory.read_with(&*cx, |inventory, _| {
            inventory
                .last_scheduled_task(None)
                .expect("custom Git task should be scheduled")
        });

        assert_eq!(resolved_task.resolved_label, "Check out feature-x");
        assert_eq!(
            resolved_task.resolved.args,
            vec!["checkout".to_string(), "feature-x".to_string()]
        );
    }

    #[test]
    fn test_a_decoration_is_read_as_what_it_is() {
        let remotes = vec![SharedString::from("origin"), SharedString::from("fork")];
        let read = |decoration: &str| read_ref(decoration, Some("main"), &remotes);

        assert_eq!(read("HEAD -> main"), Some((RefKind::Head, "main".into())));
        assert_eq!(read("main"), Some((RefKind::Head, "main".into())));
        assert_eq!(read("release"), Some((RefKind::Branch, "release".into())));
        assert_eq!(read("tag: v1.0"), Some((RefKind::Tag, "v1.0".into())));
        assert_eq!(
            read("origin/main"),
            Some((RefKind::Remote, "origin/main".into()))
        );
        assert_eq!(
            read("fork/topic"),
            Some((RefKind::Remote, "fork/topic".into()))
        );

        // A slash is not enough to make a ref remote: this is a local branch,
        // and there is no remote called "feature".
        assert_eq!(
            read("feature/cache"),
            Some((RefKind::Branch, "feature/cache".into()))
        );

        // A detached HEAD still has to be shown; it is where the reader is.
        assert_eq!(read("HEAD"), Some((RefKind::Head, "HEAD".into())));
        assert_eq!(read(""), None);
        assert_eq!(read("tag: "), None);

        // With no remotes known yet, a remote-tracking ref reads as a branch
        // rather than disappearing.
        assert_eq!(
            read_ref("origin/main", Some("main"), &[]),
            Some((RefKind::Branch, "origin/main".into()))
        );
    }

    #[test]
    fn test_labels_are_ordered_by_what_a_reader_looks_for_first() {
        let mut kinds = vec![
            RefKind::Remote,
            RefKind::Branch,
            RefKind::Tag,
            RefKind::Head,
        ];
        kinds.sort();
        assert_eq!(
            kinds,
            vec![
                RefKind::Head,
                RefKind::Tag,
                RefKind::Branch,
                RefKind::Remote
            ]
        );
    }

    #[gpui::test]
    async fn test_commit_message_rendered_as_markdown(cx: &mut TestAppContext) {
        init_test(cx);

        let fs = FakeFs::new(cx.executor());
        fs.insert_tree(
            Path::new("/project"),
            json!({ ".git": {}, "file.txt": "content" }),
        )
        .await;

        let commit_sha = Oid::from_bytes(&[1; 20]).unwrap();
        let commits = vec![Arc::new(InitialGraphCommitData {
            sha: commit_sha,
            parents: smallvec![],
            ref_names: vec!["HEAD -> main".into()],
        })];
        fs.set_graph_commits(Path::new("/project/.git"), commits);
        fs.set_commit_data(
            Path::new("/project/.git"),
            [(
                CommitData {
                    sha: commit_sha,
                    parents: smallvec![],
                    author_name: "Author".into(),
                    author_email: "author@example.com".into(),
                    commit_timestamp: 1_700_000_000,
                    subject: "Fix crash".into(),
                    message: "Fix crash\n\nThis fixes a crash that occurred when...".into(),
                },
                false,
            )],
        );

        let project = Project::test(fs.clone(), [Path::new("/project")], cx).await;
        cx.run_until_parked();

        let repository = project.read_with(cx, |project, cx| {
            project
                .active_repository(cx)
                .expect("should have a repository")
        });

        let (multi_workspace, cx) = cx.add_window_view(|window, cx| {
            workspace::MultiWorkspace::test_new(project.clone(), window, cx)
        });
        let workspace_weak =
            multi_workspace.read_with(&*cx, |multi, _| multi.workspace().downgrade());

        let git_graph = cx.new_window_entity(|window, cx| {
            GitGraph::new(
                repository.read(cx).id,
                project.read(cx).git_store().clone(),
                workspace_weak,
                None,
                window,
                cx,
            )
        });
        cx.run_until_parked();

        // Select the commit to trigger loading the commit message
        git_graph.update_in(cx, |graph, window, cx| {
            graph.select_first(&menu::SelectFirst, window, cx);
        });
        cx.run_until_parked();

        // Verify the commit message was loaded as markdown
        git_graph.read_with(&*cx, |graph, app| {
            let message = graph
                .selected_commit_message
                .as_ref()
                .expect("selected_commit_message should be Some");
            assert_eq!(message.sha, commit_sha);
            let source = message.message.read_with(app, |m, _| m.source().to_owned());
            assert!(source.contains("Fix crash"));
            assert!(source.contains("This fixes a crash"));
        });
    }

    #[gpui::test]
    async fn test_long_commit_message_is_constrained_to_scroll_viewport(cx: &mut TestAppContext) {
        init_test(cx);

        let fs = FakeFs::new(cx.executor());
        fs.insert_tree(
            Path::new("/project"),
            json!({ ".git": {}, "file.txt": "content" }),
        )
        .await;

        let commit_sha = Oid::from_bytes(&[1; 20]).expect("commit SHA should be valid");
        let commits = vec![Arc::new(InitialGraphCommitData {
            sha: commit_sha,
            parents: smallvec![],
            ref_names: vec!["HEAD -> main".into()],
        })];
        fs.set_graph_commits(Path::new("/project/.git"), commits);

        let message = (0..40)
            .map(|line_number| {
                format!(
                    "Line {line_number}: This commit message is long enough to require scrolling."
                )
            })
            .collect::<Vec<_>>()
            .join("\n\n");
        fs.set_commit_data(
            Path::new("/project/.git"),
            [(
                CommitData {
                    sha: commit_sha,
                    parents: smallvec![],
                    author_name: "Author".into(),
                    author_email: "author@example.com".into(),
                    commit_timestamp: 1_700_000_000,
                    subject: "Long commit message".into(),
                    message: message.into(),
                },
                false,
            )],
        );

        let project = Project::test(fs.clone(), [Path::new("/project")], cx).await;
        cx.run_until_parked();

        let repository = project.read_with(cx, |project, cx| {
            project
                .active_repository(cx)
                .expect("should have a repository")
        });
        let (multi_workspace, cx) = cx.add_window_view(|window, cx| {
            workspace::MultiWorkspace::test_new(project.clone(), window, cx)
        });
        let workspace_weak =
            multi_workspace.read_with(&*cx, |multi, _| multi.workspace().downgrade());
        let git_graph = cx.new_window_entity(|window, cx| {
            GitGraph::new(
                repository.read(cx).id,
                project.read(cx).git_store().clone(),
                workspace_weak,
                None,
                window,
                cx,
            )
        });
        cx.run_until_parked();

        git_graph.update_in(cx, |graph, window, cx| {
            graph.select_first(&menu::SelectFirst, window, cx);
        });
        cx.run_until_parked();

        git_graph.update(cx, |graph, cx| {
            graph.selected_commit_diff = Some(CommitDiff {
                files: vec![CommitFile {
                    path: RepoPath::new("file.txt").expect("repository path should be valid"),
                    old_text: Some("content".into()),
                    new_text: Some("updated content".into()),
                    is_binary: false,
                }],
            });
            graph.selected_commit_diff_stats = Some((1, 1));
            cx.notify();
        });

        cx.draw(
            point(px(0.), px(0.)),
            gpui::size(px(1200.), px(800.)),
            |_, _| git_graph.clone().into_any_element(),
        );
        cx.run_until_parked();

        let (message_scroll_handle, changed_files_scroll_handle) =
            git_graph.read_with(&*cx, |graph, _| {
                (
                    graph
                        .selected_commit_message
                        .as_ref()
                        .expect("selected commit message should be loaded")
                        .scroll_handle
                        .clone(),
                    graph.changed_files_scroll_handle.clone(),
                )
            });
        let maximum_message_height = git_graph.update_in(cx, |_, window, cx| {
            editor::hover_markdown_style(window, cx)
                .base_text_style
                .line_height_in_pixels(window.rem_size())
                * 12.
        });
        let message_bounds = message_scroll_handle.bounds();
        let changed_files_bounds = changed_files_scroll_handle.0.borrow().base_handle.bounds();

        assert!(
            message_bounds.size.height <= maximum_message_height,
            "commit message viewport height ({}) should not exceed its maximum ({})",
            message_bounds.size.height,
            maximum_message_height,
        );
        assert!(
            message_scroll_handle.max_offset().y > px(0.),
            "long commit message should be scrollable"
        );
        assert!(
            message_bounds.bottom() <= changed_files_bounds.top(),
            "commit message viewport {message_bounds:?} should not overlap changed files {changed_files_bounds:?}"
        );
    }

    #[gpui::test]
    async fn test_commit_message_not_reloaded_for_same_sha(cx: &mut TestAppContext) {
        init_test(cx);

        let fs = FakeFs::new(cx.executor());
        fs.insert_tree(
            Path::new("/project"),
            json!({ ".git": {}, "file.txt": "content" }),
        )
        .await;

        let commit_sha = Oid::from_bytes(&[1; 20]).unwrap();
        let commits = vec![Arc::new(InitialGraphCommitData {
            sha: commit_sha,
            parents: smallvec![],
            ref_names: vec!["HEAD -> main".into()],
        })];
        fs.set_graph_commits(Path::new("/project/.git"), commits);
        fs.set_commit_data(
            Path::new("/project/.git"),
            [(
                CommitData {
                    sha: commit_sha,
                    parents: smallvec![],
                    author_name: "Author".into(),
                    author_email: "author@example.com".into(),
                    commit_timestamp: 1_700_000_000,
                    subject: "Fix crash".into(),
                    message: "Fix crash\n\nBody text.".into(),
                },
                false,
            )],
        );

        let project = Project::test(fs.clone(), [Path::new("/project")], cx).await;
        cx.run_until_parked();

        let repository = project.read_with(cx, |project, cx| {
            project
                .active_repository(cx)
                .expect("should have a repository")
        });

        let (multi_workspace, cx) = cx.add_window_view(|window, cx| {
            workspace::MultiWorkspace::test_new(project.clone(), window, cx)
        });
        let workspace_weak =
            multi_workspace.read_with(&*cx, |multi, _| multi.workspace().downgrade());

        let git_graph = cx.new_window_entity(|window, cx| {
            GitGraph::new(
                repository.read(cx).id,
                project.read(cx).git_store().clone(),
                workspace_weak,
                None,
                window,
                cx,
            )
        });
        cx.run_until_parked();

        // Select the commit to load the message
        git_graph.update_in(cx, |graph, window, cx| {
            graph.select_first(&menu::SelectFirst, window, cx);
        });
        cx.run_until_parked();

        // Verify message is loaded
        let message_entity_id = git_graph.read_with(&*cx, |graph, _| {
            graph
                .selected_commit_message
                .as_ref()
                .map(|m| m.message.entity_id())
        });
        assert!(message_entity_id.is_some());

        // Select the same commit again to trigger the early-return logic
        git_graph.update_in(cx, |graph, window, cx| {
            graph.select_first(&menu::SelectFirst, window, cx);
        });
        cx.run_until_parked();

        // Verify the message entity is the same (not replaced)
        git_graph.read_with(&*cx, |graph, _| {
            let new_entity_id = graph
                .selected_commit_message
                .as_ref()
                .map(|m| m.message.entity_id());
            assert_eq!(message_entity_id, new_entity_id);
        });
    }

    /// Builds a history over a random DAG and draws it once at `size`.
    ///
    /// Every layout assertion in this file goes through here rather than
    /// through the layout code directly: what a reader complains about is what
    /// was painted, and only a real draw can be measured.
    async fn drawn_history(
        cx: &mut TestAppContext,
        commits: Vec<Arc<InitialGraphCommitData>>,
        size: gpui::Size<Pixels>,
    ) -> (Entity<GitGraph>, &mut VisualTestContext) {
        drawn_history_with_changes(cx, commits, &[], size).await
    }

    /// The same, over a working tree with something in it.
    async fn drawn_history_with_changes<'a>(
        cx: &'a mut TestAppContext,
        commits: Vec<Arc<InitialGraphCommitData>>,
        changed: &[(&str, FileStatus)],
        size: gpui::Size<Pixels>,
    ) -> (Entity<GitGraph>, &'a mut VisualTestContext) {
        let fs = FakeFs::new(cx.executor());
        fs.insert_tree(
            Path::new("/project"),
            serde_json::json!({
                ".git": {},
                "file.txt": "content",
            }),
        )
        .await;

        fs.set_graph_commits(Path::new("/project/.git"), commits);
        // Committed and unchanged, so that a working tree is only dirty when a
        // test says it is.
        fs.set_head_and_index_for_repo(
            Path::new("/project/.git"),
            &[("file.txt", "content".to_string())],
        );
        if !changed.is_empty() {
            fs.set_status_for_repo(Path::new("/project/.git"), changed);
        }

        let project = Project::test(fs.clone(), [Path::new("/project")], cx).await;
        cx.run_until_parked();

        let repository = project.read_with(cx, |project, cx| {
            project
                .active_repository(cx)
                .expect("should have a repository")
        });

        let (multi_workspace, cx) = cx.add_window_view(|window, cx| {
            workspace::MultiWorkspace::test_new(project.clone(), window, cx)
        });
        let workspace_weak =
            multi_workspace.read_with(&*cx, |multi, _| multi.workspace().downgrade());

        let git_graph = cx.new_window_entity(|window, cx| {
            GitGraph::new(
                repository.read(cx).id,
                project.read(cx).git_store().clone(),
                workspace_weak,
                None,
                window,
                cx,
            )
        });
        cx.run_until_parked();

        // Twice: the first draw is what tells the container how wide it is, and
        // the layout the second one picks depends on that answer.
        for _ in 0..2 {
            cx.draw(point(px(0.), px(0.)), size, |_, _| {
                git_graph.clone().into_any_element()
            });
            cx.run_until_parked();
        }

        (git_graph, cx)
    }

    fn place_of(lane: usize, layout: HistoryLayout) -> NodePlace {
        match lane >= layout.lane_cap {
            true => NodePlace::PastTheEdge,
            false => NodePlace::InLane(lane),
        }
    }

    fn selector(name: &str, idx: usize) -> &'static str {
        Box::leak(format!("{name}-{idx}").into_boxed_str())
    }

    /// How many rows a pane of this height has to have painted before a test is
    /// entitled to say it looked at the list.
    ///
    /// Without a floor, a regression that stops the list rendering leaves one
    /// row on screen and every measurement passes over it.
    fn rows_a_pane_must_paint(pane_height: Pixels, row_height: Pixels) -> usize {
        /// The search field and the frame around the list.
        const CHROME: Pixels = px(160.);
        (((pane_height - CHROME) / row_height).floor() as usize).max(1)
    }

    #[gpui::test]
    fn test_graph_metrics_all_follow_one_row_height(_cx: &mut TestAppContext) {
        let metrics = GraphMetrics::new(px(21.), 1.0);

        assert_eq!(metrics.row, px(34.), "one line of text plus its chrome");
        assert_eq!(metrics.node, px(24.));
        assert_eq!(metrics.lane, px(27.));
        assert_eq!(metrics.label, px(22.));

        // Every size has to move together with the row, or the dots leave their
        // rows the moment the reader changes the interface font.
        let larger = GraphMetrics::new(px(28.), 1.0);
        assert!(larger.row > metrics.row);
        assert!(larger.node > metrics.node);
        assert!(larger.lane > metrics.lane);
        assert!(larger.label > metrics.label);

        assert!(
            metrics.lane_center_in(1, 0) - metrics.lane_center_in(0, 0) == metrics.lane,
            "lanes are one lane step apart"
        );
        assert!(
            metrics.node < metrics.lane,
            "a node wider than its lane step would touch the node beside it"
        );
        assert!(
            metrics.lane_center_in(0, 0) - metrics.node / 2.0 >= px(0.),
            "the first node is cut off by the left edge of its column"
        );
    }

    #[gpui::test]
    fn test_metrics_land_on_whole_device_pixels(_cx: &mut TestAppContext) {
        // A fractional row height and the list's own snapping disagree, and the
        // graph walks away from its rows as the reader scrolls.
        for scale in [1.0f32, 1.5, 2.0] {
            for line_height in [px(15.3), px(21.), px(24.7)] {
                let metrics = GraphMetrics::new(line_height, scale);
                for value in [metrics.row, metrics.node, metrics.lane, metrics.label] {
                    let device = f32::from(value) * scale;
                    assert!(
                        (device - device.round()).abs() < 0.001,
                        "{value:?} at scale {scale} is {device} device pixels, not a whole number"
                    );
                }
            }
        }
    }

    #[gpui::test]
    fn test_initials_stand_in_for_an_author(_cx: &mut TestAppContext) {
        assert_eq!(initials_of("Ada Lovelace").as_ref(), "AL");
        assert_eq!(initials_of("ada").as_ref(), "A");
        assert_eq!(initials_of("  ada   byron   king  ").as_ref(), "AB");
        assert_eq!(initials_of("").as_ref(), "?");
        assert_eq!(initials_of("   ").as_ref(), "?");
        assert_eq!(initials_of("Ада Лавлейс").as_ref(), "АЛ");
        // A name that starts with punctuation still has to yield a letter.
        assert_eq!(initials_of("(bot) release").as_ref(), "BR");
    }

    #[gpui::test]
    fn test_ink_is_readable_on_every_lane_colour(_cx: &mut TestAppContext) {
        for step in 0..=20 {
            let lightness = step as f32 / 20.0;
            let background = hsla(0.6, 0.7, lightness, 1.);
            let ink = readable_on(background);
            assert!(
                (ink.l - lightness).abs() > 0.4,
                "ink at l={} is not readable on a lane at l={lightness}",
                ink.l
            );
        }
    }

    #[gpui::test]
    fn test_timestamps_read_as_ages(_cx: &mut TestAppContext) {
        let now = OffsetDateTime::from_unix_timestamp(1_000_000_000).expect("a valid instant");
        let ago = |seconds: i64| format_relative_timestamp(1_000_000_000 - seconds, now);

        assert_eq!(ago(0), "now");
        assert_eq!(ago(59), "now");
        assert_eq!(ago(60), "1m");
        assert_eq!(ago(59 * 60), "59m");
        assert_eq!(ago(60 * 60), "1h");
        assert_eq!(ago(23 * 60 * 60), "23h");
        assert_eq!(ago(24 * 60 * 60), "1d");
        assert_eq!(ago(6 * 24 * 60 * 60), "6d");
        assert_eq!(ago(7 * 24 * 60 * 60), "1w");
        assert_eq!(ago(29 * 24 * 60 * 60), "4w");
        assert_eq!(ago(30 * 24 * 60 * 60), "1mo");
        assert_eq!(ago(364 * 24 * 60 * 60), "12mo");
        assert_eq!(ago(365 * 24 * 60 * 60), "1y");

        // A commit stamped in the future is a real thing on a machine whose
        // clock has been corrected; it must not read as an enormous age.
        assert_eq!(ago(-5_000), "now");
    }

    fn contents_for_test(widest: Pixels, lanes: usize) -> HistoryContents {
        HistoryContents {
            metrics: GraphMetrics::new(px(21.), 1.0),
            character: px(8.),
            widest_label: [widest; 4],
            lanes,
        }
    }

    #[gpui::test]
    fn test_what_a_row_gives_up_first(_cx: &mut TestAppContext) {
        let contents = contents_for_test(px(180.), 12);

        // Wide enough for everything.
        let roomy = fit(px(1400.), contents);
        assert_eq!(roomy.age, AgeShown::Column);
        assert_eq!(roomy.labels, LabelMode::Full);
        assert_eq!(roomy.lane_cap, 8);

        // The order of concessions: the age column goes before the labels are
        // touched, the labels shorten before they are dropped to one, and the
        // column itself is the last thing to go.
        let mut seen = Vec::new();
        let mut width = px(1400.);
        while width > px(200.) {
            let layout = fit(width, contents);
            let rung = (layout.age, layout.labels, layout.lane_cap);
            if seen.last() != Some(&rung) {
                seen.push(rung);
            }
            width -= px(10.);
        }

        assert_eq!(
            seen,
            vec![
                (AgeShown::Column, LabelMode::Full, 8),
                (AgeShown::WhereItMatters, LabelMode::Abbreviated, 6),
                (AgeShown::Nowhere, LabelMode::Single, 4),
                (AgeShown::Nowhere, LabelMode::Inline, 3),
            ],
            "a narrowing history did not give its room up in the written order"
        );
    }

    #[gpui::test]
    fn test_the_thresholds_follow_what_is_in_the_history(_cx: &mut TestAppContext) {
        // The same width, two histories: one whose labels are short and one
        // whose labels are long. The long one has to give up sooner. A ladder
        // of fixed pixel thresholds cannot tell these apart.
        let gives_up_at = |widest: Pixels| {
            let mut width = px(1600.);
            while width > px(100.)
                && fit(width, contents_for_test(widest, 12)).labels == LabelMode::Full
            {
                width -= px(10.);
            }
            width
        };
        assert!(
            gives_up_at(px(340.)) > gives_up_at(px(60.)),
            "a history of long branch names gives up its labels no sooner than \
             one of short names ({:?} against {:?})",
            gives_up_at(px(340.)),
            gives_up_at(px(60.))
        );

        // And a history with no labels at all spends nothing on them.
        let bare = fit(px(900.), contents_for_test(px(0.), 12));
        assert_eq!(bare.label_width, px(0.));
    }

    #[gpui::test]
    fn test_the_label_column_is_clamped_at_both_ends(_cx: &mut TestAppContext) {
        // Narrower than the floor: a column this narrow names nothing.
        let tiny = fit(px(1400.), contents_for_test(px(20.), 2));
        assert_eq!(tiny.label_width, LABEL_COLUMN_MIN);

        // Wider than the share: the labels stop, the subject keeps the rest.
        let huge = fit(px(1400.), contents_for_test(px(900.), 2));
        assert_eq!(huge.label_width, px(1400.) * LABEL_COLUMN_MAX_SHARE);
    }

    #[gpui::test]
    fn test_a_name_loses_its_prefix_before_its_tail(_cx: &mut TestAppContext) {
        let name = "fix/10-async";
        assert_eq!(shorten_ref(name, LabelMode::Full).as_ref(), "fix/10-async");
        assert_eq!(
            shorten_ref(name, LabelMode::Abbreviated).as_ref(),
            "f/10-async"
        );
        assert_eq!(shorten_ref(name, LabelMode::Single).as_ref(), "10-async");
        assert_eq!(shorten_ref(name, LabelMode::Inline).as_ref(), "10-async");

        // Several segments all squeeze, and the last one never does.
        assert_eq!(
            shorten_ref("team/fix/10-async", LabelMode::Abbreviated).as_ref(),
            "t/f/10-async"
        );
        // A name with no prefix has nothing to squeeze.
        assert_eq!(shorten_ref("main", LabelMode::Abbreviated).as_ref(), "main");
        // Only the tail that will not fit is cut, and it says so.
        assert_eq!(
            shorten_ref("feature/a-very-long-branch-name", LabelMode::Inline).as_ref(),
            "a-very-long…"
        );
        assert_eq!(shorten_ref("", LabelMode::Abbreviated).as_ref(), "");
    }

    #[gpui::test]
    fn test_where_an_age_earns_its_place(_cx: &mut TestAppContext) {
        // With a column of its own, every row has one.
        for row in [(false, false, false), (true, false, false)] {
            assert!(age_is_shown(AgeShown::Column, row.0, row.1, row.2));
        }

        // Without one, only where the reader is looking or where the commit is
        // recent enough for "when" to be the question.
        assert!(!age_is_shown(AgeShown::WhereItMatters, false, false, false));
        assert!(age_is_shown(AgeShown::WhereItMatters, true, false, false));
        assert!(age_is_shown(AgeShown::WhereItMatters, false, true, false));
        assert!(age_is_shown(AgeShown::WhereItMatters, false, false, true));

        // And where there is no room at all, nowhere.
        assert!(!age_is_shown(AgeShown::Nowhere, true, true, true));
    }

    #[gpui::test]
    fn test_yesterday_is_fresh_and_last_week_is_not(_cx: &mut TestAppContext) {
        let now = OffsetDateTime::from_unix_timestamp(1_000_000_000).expect("a valid instant");
        let ago = |seconds: i64| is_younger_than_a_day(1_000_000_000 - seconds, now);

        assert!(ago(0));
        assert!(ago(23 * 60 * 60));
        assert!(!ago(24 * 60 * 60));
        assert!(!ago(7 * 24 * 60 * 60));
    }

    #[gpui::test]
    fn test_one_graph_gives_one_set_of_colours_however_it_arrives(_cx: &mut TestAppContext) {
        for seed in 0..8u64 {
            let mut rng = StdRng::seed_from_u64(seed);
            let commits = generate_random_commit_dag(&mut rng, 90, true);

            let mut whole = GraphData::new(13);
            whole.add_commits(&commits);
            let at_once: Vec<(usize, usize)> = whole
                .commits
                .iter()
                .map(|commit| (commit.lane, commit.color_idx))
                .collect();

            // The same history, streamed in the chunks a repository sends.
            for chunk in [1usize, 7, 30] {
                let mut streamed = GraphData::new(13);
                for part in commits.chunks(chunk) {
                    streamed.add_commits(part);
                }
                let in_parts: Vec<(usize, usize)> = streamed
                    .commits
                    .iter()
                    .map(|commit| (commit.lane, commit.color_idx))
                    .collect();

                assert_eq!(
                    at_once, in_parts,
                    "seed {seed}: a history arriving in chunks of {chunk} is not \
                     drawn the same as one that arrived whole"
                );
            }
        }
    }

    #[gpui::test]
    fn test_a_reused_lane_does_not_reuse_its_colour(_cx: &mut TestAppContext) {
        // Two unrelated side branches, one after the other, both of which land
        // in the second lane because the first one has been given up by then.
        let mut rng = StdRng::seed_from_u64(21);
        let oids: Vec<Oid> = (0..7).map(|_| Oid::random(&mut rng)).collect();
        let of = |idx: usize, parents: SmallVec<[Oid; 1]>| {
            Arc::new(InitialGraphCommitData {
                sha: oids[idx],
                parents,
                ref_names: vec![],
            })
        };
        let commits = vec![
            of(0, smallvec![oids[1], oids[2]]),
            of(1, smallvec![oids[3]]),
            of(2, smallvec![oids[3]]),
            of(3, smallvec![oids[4], oids[5]]),
            of(4, smallvec![oids[6]]),
            of(5, smallvec![oids[6]]),
            of(6, smallvec![]),
        ];

        let mut graph = GraphData::new(7);
        graph.add_commits(&commits);

        let first_branch = &graph.commits[2];
        let second_branch = &graph.commits[5];
        assert_eq!(
            first_branch.lane, second_branch.lane,
            "the fixture was meant to put both side branches in the same lane"
        );
        assert_ne!(
            first_branch.color_idx, second_branch.color_idx,
            "two unrelated branches sharing a lane are drawn in the same colour"
        );
    }

    #[gpui::test]
    fn test_no_two_live_lanes_share_a_colour(_cx: &mut TestAppContext) {
        for seed in 0..12u64 {
            let mut rng = StdRng::seed_from_u64(seed);
            let commits = generate_random_commit_dag(&mut rng, 80, true);

            // A palette wider than the history is deep, so a collision can only
            // come from the assignment and never from running out of colours.
            let mut graph = GraphData::new(64);
            for commit in commits.chunks(7) {
                graph.add_commits(commit);
                let mut seen: HashSet<u8> = HashSet::default();
                for (lane, color) in graph.lane_colors.iter() {
                    assert!(
                        seen.insert(color.0),
                        "seed {seed}: lane {lane} is drawn in colour {}, \
                         which another live lane already uses",
                        color.0
                    );
                }
            }
        }
    }

    /// What a frame of graph costs on a history large enough for it to matter.
    ///
    /// The painter used to ask every line in the history whether it crossed the
    /// viewport, on every frame; the rows now carry the answer. This measures
    /// both over the same history and refuses the old cost, so the scan cannot
    /// come back unnoticed.
    #[gpui::test]
    fn test_a_frame_of_graph_does_not_read_the_whole_history(_cx: &mut TestAppContext) {
        use std::time::Instant;

        const COMMITS: usize = 50_000;
        const VIEWPORT: usize = 40;
        const FRAMES: usize = 200;

        let mut rng = StdRng::seed_from_u64(101);
        let commits = generate_random_commit_dag(&mut rng, COMMITS, false);
        let mut graph = GraphData::new(13);
        graph.add_commits(&commits);
        assert_eq!(graph.commits.len(), COMMITS);
        assert!(
            graph.lines.len() > COMMITS / 4,
            "the fixture has too few lines to measure"
        );

        // What the rows now answer.
        let started = Instant::now();
        let mut counted = 0usize;
        for frame in 0..FRAMES {
            let first = (frame * 211) % (COMMITS - VIEWPORT);
            for row in first..first + VIEWPORT {
                counted += graph.lanes_at(row).len();
            }
        }
        let by_row = started.elapsed();

        // What it used to cost: every line asked about every frame.
        let started = Instant::now();
        let mut scanned = 0usize;
        for frame in 0..FRAMES {
            let first = (frame * 211) % (COMMITS - VIEWPORT);
            let viewport = first..first + VIEWPORT;
            scanned += graph
                .lines
                .iter()
                .filter(|line| {
                    line.full_interval.start <= viewport.end
                        && line.full_interval.end >= viewport.start
                })
                .count();
        }
        let by_scan = started.elapsed();

        assert!(
            counted > 0 && scanned > 0,
            "neither way found anything to draw"
        );
        println!(
            "graph frame cost over {COMMITS} commits, {FRAMES} frames of {VIEWPORT} rows: \
             by row {by_row:?}, by scan {by_scan:?}"
        );

        // A generous bound: the point is the shape of the cost, not the
        // machine. Reading the rows is constant in the size of the history;
        // scanning is linear in it.
        assert!(
            by_row * 10 < by_scan,
            "reading the rows costs {by_row:?} against the old scan's {by_scan:?}; \
             the index is not paying for itself"
        );
    }

    #[gpui::test]
    fn test_lane_index_draws_every_line_without_a_break(_cx: &mut TestAppContext) {
        for seed in 0..24u64 {
            let mut rng = StdRng::seed_from_u64(seed);
            let commits = generate_random_commit_dag(&mut rng, 60, true);
            let mut graph = GraphData::new(8);
            graph.add_commits(&commits);

            let rows = graph.commits.len();
            assert_eq!(rows, 60);
            assert!(
                (0..rows).any(|row| !graph.lanes_at(row).is_empty()),
                "seed {seed}: the index came out empty, so the graph would paint nothing"
            );

            for row in 0..rows {
                for paint in graph.lanes_at(row) {
                    assert!(
                        paint.from_column < graph.max_lanes && paint.to_column < graph.max_lanes,
                        "seed {seed}: row {row} paints a lane outside the {} it has",
                        graph.max_lanes
                    );

                    if paint.ends_at_node {
                        continue;
                    }
                    let next = graph.lanes_at(row + 1);
                    assert!(
                        next.iter().any(|below| {
                            below.color_idx == paint.color_idx
                                && below.from_column == paint.to_column
                                && !below.starts_at_node
                        }),
                        "seed {seed}: the line leaving row {row} in column {} \
                         has nothing to meet on row {}",
                        paint.to_column,
                        row + 1
                    );
                }
            }

            // Every line begins on its commit and ends on its parent, and the
            // index has to say so on exactly those two rows.
            for line in graph.lines.iter() {
                let start = line.full_interval.start;
                let end = line.full_interval.end;
                assert!(
                    graph.lanes_at(start).iter().any(|paint| {
                        paint.starts_at_node
                            && paint.from_column == line.child_column
                            && paint.color_idx == line.color_idx
                    }),
                    "seed {seed}: no line leaves the commit on row {start}"
                );
                assert!(
                    graph
                        .lanes_at(end)
                        .iter()
                        .any(|paint| paint.ends_at_node && paint.color_idx == line.color_idx),
                    "seed {seed}: no line lands on the commit on row {end}"
                );
            }
        }
    }

    #[gpui::test]
    fn test_an_unresolved_line_is_not_half_drawn(_cx: &mut TestAppContext) {
        // A line whose parent has not been streamed in yet still carries the
        // sentinel it was opened with. Drawing the part of it that is known
        // would end it in mid-air, pointing at a commit that is not on screen.
        let mut graph = GraphData::new(6);
        graph.add_commits(&unlabelled_commits(6));

        let before: Vec<Vec<LanePaint>> = (0..6).map(|row| graph.lanes_at(row).to_vec()).collect();
        assert!(
            before.iter().any(|paints| !paints.is_empty()),
            "the fixture was meant to index some lines, or this proves nothing"
        );

        let mut rng = StdRng::seed_from_u64(4);
        let mut unresolved = |on_row: usize| CommitLine {
            child: Oid::random(&mut rng),
            parent: Oid::random(&mut rng),
            child_column: 0,
            full_interval: 0..on_row,
            color_idx: 3,
            segments: smallvec![
                CommitLineSegment::Straight { to_row: 2 },
                CommitLineSegment::Curve {
                    to_column: 1,
                    on_row,
                    curve_kind: CurveKind::Merge,
                },
            ],
        };

        // A parent below the rows loaded so far, and the sentinel a line is
        // opened with before its parent has been seen at all.
        for on_row in [99, usize::MAX] {
            let line = unresolved(on_row);
            graph.index_line_for_rows(&line);
            let after: Vec<Vec<LanePaint>> =
                (0..6).map(|row| graph.lanes_at(row).to_vec()).collect();
            assert_eq!(
                before, after,
                "half of a line whose parent is on row {on_row} was drawn anyway"
            );
        }
    }

    /// A chain of commits with no labels at all on any of them.
    fn unlabelled_commits(count: usize) -> Vec<Arc<InitialGraphCommitData>> {
        let mut rng = StdRng::seed_from_u64(3);
        let oids: Vec<Oid> = (0..count).map(|_| Oid::random(&mut rng)).collect();
        (0..count)
            .map(|idx| {
                Arc::new(InitialGraphCommitData {
                    sha: oids[idx],
                    parents: match oids.get(idx + 1) {
                        Some(parent) => smallvec![*parent],
                        None => smallvec![],
                    },
                    ref_names: vec![],
                })
            })
            .collect()
    }

    #[gpui::test]
    async fn test_the_history_measures_itself_and_not_the_window(cx: &mut TestAppContext) {
        init_test(cx);
        // The window a test opens is maximised to whatever the platform says;
        // what the layout must follow is the width the history was drawn at.
        for width in [px(420.), px(760.), px(1240.)] {
            let (git_graph, cx) =
                drawn_history(cx, unlabelled_commits(20), gpui::size(width, px(700.))).await;
            let measured =
                git_graph.update_in(cx, |graph, window, cx| graph.history_width(window, cx));
            assert!(
                (measured - width).abs() < px(12.),
                "drawn at {width:?}, the history thinks it is {measured:?} wide; \
                 everything that follows the width follows the wrong number"
            );
        }
    }

    #[gpui::test]
    async fn test_a_resize_settles_without_waiting_for_the_reader(cx: &mut TestAppContext) {
        init_test(cx);
        let (git_graph, cx) =
            drawn_history(cx, unlabelled_commits(20), gpui::size(px(1240.), px(700.))).await;

        let fit_now = |cx: &mut VisualTestContext| {
            git_graph.update_in(cx, |graph, window, cx| graph.history_layout(window, cx).age)
        };
        assert_eq!(fit_now(cx), AgeShown::Column);

        // Counting frames says nothing -- other views ask for them too. What
        // the history must do is mark itself for drawing again, so that is what
        // is counted, and a frame at an unchanged width is the control.
        let redraws = Rc::new(Cell::new(0usize));
        let _subscription = cx.update(|_, cx| {
            let redraws = redraws.clone();
            cx.observe(&git_graph, move |_, _| redraws.set(redraws.get() + 1))
        });

        let draw_and_settle = |cx: &mut VisualTestContext, width: Pixels| {
            cx.draw(
                point(px(0.), px(0.)),
                gpui::size(width, px(700.)),
                |_, _| git_graph.clone().into_any_element(),
            );
            let before = redraws.get();
            cx.update(|window, cx| window.simulate_next_frame(cx));
            redraws.get() - before
        };

        // The very first measurement is a change too, so its request is drained
        // before the control frame is taken.
        draw_and_settle(cx, px(1240.));
        let quiet_frame = draw_and_settle(cx, px(1240.));
        let frame_that_resized = draw_and_settle(cx, px(300.));

        assert_eq!(
            fit_now(cx),
            AgeShown::WhereItMatters,
            "after one frame at 300 the history still thinks it is as wide as it was"
        );
        assert_eq!(
            quiet_frame, 0,
            "a frame at an unchanged width asked to be drawn again, \
             so the control says nothing about the frame that resized"
        );
        assert!(
            frame_that_resized > 0,
            "the width changed and the history did not ask to be drawn again; \
             a window dragged narrower and then left alone keeps the old columns"
        );
    }

    #[gpui::test]
    async fn test_the_graph_takes_only_the_room_its_lanes_need(cx: &mut TestAppContext) {
        init_test(cx);
        // Two lanes: a straight chain with one side branch merged back in.
        let mut rng = StdRng::seed_from_u64(5);
        let oids: Vec<Oid> = (0..5).map(|_| Oid::random(&mut rng)).collect();
        let commit = |idx: usize, parents: SmallVec<[Oid; 1]>| {
            Arc::new(InitialGraphCommitData {
                sha: oids[idx],
                parents,
                ref_names: vec![],
            })
        };
        let commits = vec![
            commit(0, smallvec![oids[1], oids[2]]),
            commit(1, smallvec![oids[3]]),
            commit(2, smallvec![oids[3]]),
            commit(3, smallvec![oids[4]]),
            commit(4, smallvec![]),
        ];

        let (git_graph, cx) = drawn_history(cx, commits, gpui::size(px(1200.), px(700.))).await;

        let (metrics, lanes) = git_graph.update_in(cx, |graph, window, cx| {
            (graph.row_metrics(window, cx), graph.graph_data.max_lanes)
        });
        assert!(lanes >= 2, "the fixture was meant to open a second lane");

        let cell = cx
            .debug_bounds(selector("GRAPH_CELL", 0))
            .expect("the first row should have been painted");
        assert_eq!(
            cell.size.width,
            metrics.width_for(lanes),
            "the graph column is {} wide for {lanes} lanes, which need {}",
            cell.size.width,
            metrics.width_for(lanes)
        );

        // And nothing sits between the lanes and the subject.
        let subject = cx
            .debug_bounds(selector("GRAPH_SUBJECT", 0))
            .expect("the first row should have a subject");
        assert!(
            (subject.origin.x - (cell.origin.x + cell.size.width)).abs() < px(1.),
            "the subject starts at {} but the graph ends at {}",
            subject.origin.x,
            cell.origin.x + cell.size.width
        );
    }

    #[gpui::test]
    async fn test_a_history_with_no_labels_spends_no_room_on_them(cx: &mut TestAppContext) {
        init_test(cx);
        let (git_graph, cx) =
            drawn_history(cx, unlabelled_commits(12), gpui::size(px(1200.), px(700.))).await;

        git_graph.read_with(&*cx, |graph, _| {
            assert!(
                graph.graph_data.label_names.is_empty(),
                "the fixture was meant to carry no labels"
            );
        });

        assert!(
            cx.debug_bounds(selector("GRAPH_REFS", 0)).is_none(),
            "an unlabelled commit should not paint a label"
        );
        let cell = cx
            .debug_bounds(selector("GRAPH_CELL", 0))
            .expect("the first row should have been painted");
        assert!(
            cell.origin.x < px(2.),
            "with nothing to label, the graph should start at the left edge, not at {}",
            cell.origin.x
        );
    }

    /// A history whose lanes run deeper than any column will show.
    fn deep_commits(branches: usize) -> Vec<Arc<InitialGraphCommitData>> {
        let mut rng = StdRng::seed_from_u64(9);
        let trunk: Vec<Oid> = (0..branches + 2).map(|_| Oid::random(&mut rng)).collect();
        let tips: Vec<Oid> = (0..branches).map(|_| Oid::random(&mut rng)).collect();

        let mut commits = Vec::new();
        // Every trunk commit merges in a branch of its own, so each one opens a
        // lane that stays open until the very bottom.
        for idx in 0..branches {
            commits.push(Arc::new(InitialGraphCommitData {
                sha: trunk[idx],
                parents: smallvec![trunk[idx + 1], tips[idx]],
                ref_names: vec![],
            }));
        }
        commits.push(Arc::new(InitialGraphCommitData {
            sha: trunk[branches],
            parents: smallvec![trunk[branches + 1]],
            ref_names: vec![],
        }));
        for tip in tips.iter() {
            commits.push(Arc::new(InitialGraphCommitData {
                sha: *tip,
                parents: smallvec![trunk[branches + 1]],
                ref_names: vec![],
            }));
        }
        commits.push(Arc::new(InitialGraphCommitData {
            sha: trunk[branches + 1],
            parents: smallvec![],
            ref_names: vec![],
        }));
        commits
    }

    #[gpui::test]
    async fn test_a_dragged_column_border_beats_the_layout(cx: &mut TestAppContext) {
        init_test(cx);
        let mut rng = StdRng::seed_from_u64(13);
        let commits = generate_random_commit_dag(&mut rng, 20, true);
        let (git_graph, cx) = drawn_history(cx, commits, gpui::size(px(1400.), px(800.))).await;

        let chosen = git_graph.update_in(cx, |graph, window, cx| {
            graph.history_layout(window, cx).label_width
        });
        assert!(chosen > px(0.), "the fixture was meant to carry labels");

        let border = cx
            .debug_bounds("GRAPH_LABEL_DIVIDER")
            .expect("a column of labels should offer a border to drag");
        assert!(
            (border.origin.x + border.size.width / 2.0 - chosen).abs() < px(1.),
            "the border is drawn at {} where the column ends at {chosen}",
            border.origin.x + border.size.width / 2.0
        );

        // Dragging it puts the column where the reader left it.
        let wanted = chosen + px(90.);
        git_graph.update_in(cx, |graph, window, cx| {
            graph.drag_label_divider(point(wanted, px(200.)), window, cx);
        });
        let after = git_graph.read_with(&*cx, |graph, _| graph.column_override.get());
        assert_eq!(
            after,
            Some(wanted),
            "the column did not follow the border it was dragged by"
        );

        // And it stays there when the layout would have chosen otherwise.
        let widths = git_graph.update_in(cx, |graph, window, cx| {
            let layout = graph.history_layout(window, cx);
            graph.table_column_widths(window, cx, layout)
        });
        let DefiniteLength::Fraction(labels) = widths[0] else {
            panic!("the label column should be a share of the row");
        };
        assert!(
            (labels * 1400. - f32::from(wanted)).abs() < 2.,
            "the row gives the labels {} where the reader asked for {wanted}",
            labels * 1400.
        );

        // A second click on the border gives the width back to the layout.
        git_graph.update_in(cx, |graph, _window, _cx| graph.column_override.set(None));
        let back = git_graph.update_in(cx, |graph, window, cx| {
            graph.history_layout(window, cx).label_width
        });
        assert_eq!(back, chosen, "the automatic width did not come back");
    }

    #[gpui::test]
    fn test_a_branch_reaches_everything_behind_its_tip(_cx: &mut TestAppContext) {
        // main merges a side branch; the side branch's tip reaches its own two
        // commits and the base they came from, and nothing of main's own.
        let mut rng = StdRng::seed_from_u64(31);
        let oids: Vec<Oid> = (0..6).map(|_| Oid::random(&mut rng)).collect();
        let of = |idx: usize, parents: SmallVec<[Oid; 1]>| {
            Arc::new(InitialGraphCommitData {
                sha: oids[idx],
                parents,
                ref_names: vec![],
            })
        };
        let commits = vec![
            of(0, smallvec![oids[1], oids[2]]),
            of(1, smallvec![oids[4]]),
            of(2, smallvec![oids[3]]),
            of(3, smallvec![oids[4]]),
            of(4, smallvec![oids[5]]),
            of(5, smallvec![]),
        ];

        let mut graph = GraphData::new(8);
        graph.add_commits(&commits);

        let mut side: Vec<usize> = graph
            .branch_of(2, 100)
            .expect("a branch this small has an answer")
            .into_iter()
            .collect();
        side.sort();
        assert_eq!(
            side,
            vec![2, 3, 4, 5],
            "the tip of the side branch does not reach what is behind it"
        );

        // The merge reaches everything, because everything is behind it.
        let all = graph.branch_of(0, 100).expect("the merge has an answer");
        assert_eq!(all.len(), 6);

        // And a history too large to answer for says so rather than guessing.
        assert_eq!(graph.branch_of(0, 2), None);
        assert_eq!(graph.branch_of(99, 100), None);
    }

    #[gpui::test]
    fn test_the_nearest_branch_a_commit_is_on(_cx: &mut TestAppContext) {
        let mut rng = StdRng::seed_from_u64(37);
        let oids: Vec<Oid> = (0..5).map(|_| Oid::random(&mut rng)).collect();
        let of = |idx: usize, parents: SmallVec<[Oid; 1]>, refs: Vec<SharedString>| {
            Arc::new(InitialGraphCommitData {
                sha: oids[idx],
                parents,
                ref_names: refs,
            })
        };
        let commits = vec![
            of(0, smallvec![oids[1]], vec!["main".into()]),
            of(1, smallvec![oids[2]], vec![]),
            of(2, smallvec![oids[3]], vec![]),
            of(3, smallvec![oids[4]], vec!["v1.0".into()]),
            of(4, smallvec![], vec![]),
        ];

        let mut graph = GraphData::new(8);
        graph.add_commits(&commits);

        // Walking towards the children, the first label above row 2 is main.
        assert_eq!(graph.nearest_tip(2, 100), Some(0));
        // Row 4 is below a tag, and the tag is the nearer of the two.
        assert_eq!(graph.nearest_tip(4, 100), Some(3));
        // A labelled row is not its own answer.
        assert_eq!(graph.nearest_tip(0, 100), None);
    }

    #[gpui::test]
    async fn test_walking_by_the_first_parent_keeps_to_one_branch(cx: &mut TestAppContext) {
        init_test(cx);
        // A merge whose first parent is main and whose second is a side branch.
        let mut rng = StdRng::seed_from_u64(41);
        let oids: Vec<Oid> = (0..5).map(|_| Oid::random(&mut rng)).collect();
        let of = |idx: usize, parents: SmallVec<[Oid; 1]>| {
            Arc::new(InitialGraphCommitData {
                sha: oids[idx],
                parents,
                ref_names: vec![],
            })
        };
        let commits = vec![
            of(0, smallvec![oids[1], oids[2]]),
            of(1, smallvec![oids[3]]),
            of(2, smallvec![oids[3]]),
            of(3, smallvec![oids[4]]),
            of(4, smallvec![]),
        ];

        let (git_graph, cx) = drawn_history(cx, commits, gpui::size(px(1200.), px(700.))).await;

        git_graph.update_in(cx, |graph, _window, cx| {
            graph.select_entry(0, ScrollStrategy::Nearest, cx);
            // The arrow keys would walk to row 1, which is main, and to row 2,
            // which is the branch that was merged in. The first parent is the
            // one that keeps to the branch being read.
            assert_eq!(graph.first_parent_of(0), Some(1));
            assert_eq!(graph.first_parent_of(1), Some(3));
            // And back the other way.
            assert_eq!(graph.first_child_of(1), Some(0));
            // Row 2 was merged in, so nothing has it as a first parent.
            assert_eq!(graph.first_child_of(2), None);
            assert_eq!(graph.first_parent_of(4), None);
        });
    }

    /// main with a side branch, and a remote-tracking ref of the same name as
    /// the local branch -- the case the reference client is criticised for
    /// getting wrong.
    fn labelled_commits() -> Vec<Arc<InitialGraphCommitData>> {
        let mut rng = StdRng::seed_from_u64(53);
        let oids: Vec<Oid> = (0..6).map(|_| Oid::random(&mut rng)).collect();
        let of = |idx: usize, parents: SmallVec<[Oid; 1]>, refs: Vec<SharedString>| {
            Arc::new(InitialGraphCommitData {
                sha: oids[idx],
                parents,
                ref_names: refs,
            })
        };
        vec![
            of(
                0,
                smallvec![oids[1], oids[2]],
                vec!["main".into(), "origin/main".into()],
            ),
            of(1, smallvec![oids[4]], vec![]),
            of(2, smallvec![oids[3]], vec!["feature".into()]),
            of(3, smallvec![oids[4]], vec![]),
            of(4, smallvec![oids[5]], vec!["tag: v1.0".into()]),
            of(5, smallvec![], vec![]),
        ]
    }

    #[gpui::test]
    async fn test_hiding_a_branch_takes_its_rows_out(cx: &mut TestAppContext) {
        init_test(cx);
        let (git_graph, cx) =
            drawn_history(cx, labelled_commits(), gpui::size(px(1400.), px(800.))).await;

        git_graph.update_in(cx, |graph, _window, cx| {
            let all = graph.graph_data.commits.len();
            assert_eq!(graph.rows_in_the_list(all), all);

            // The side branch goes, and what it shares with main stays: rows 4
            // and 5 are behind both.
            graph.toggle_hidden("feature".into(), cx);
            let kept: Vec<usize> = (0..graph.rows_in_the_list(all))
                .filter_map(|at| graph.row_at(at))
                .collect();
            assert_eq!(
                kept,
                vec![0, 1],
                "hiding one branch took rows that other branches reach too"
            );

            // And it comes back.
            graph.toggle_hidden("feature".into(), cx);
            assert_eq!(graph.rows_in_the_list(all), all);
        });
    }

    #[gpui::test]
    async fn test_soloing_a_branch_leaves_only_it(cx: &mut TestAppContext) {
        init_test(cx);
        let (git_graph, cx) =
            drawn_history(cx, labelled_commits(), gpui::size(px(1400.), px(800.))).await;

        git_graph.update_in(cx, |graph, _window, cx| {
            let all = graph.graph_data.commits.len();
            graph.select_entry(1, ScrollStrategy::Nearest, cx);

            graph.toggle_solo("feature".into(), cx);
            let kept: Vec<usize> = (0..graph.rows_in_the_list(all))
                .filter_map(|at| graph.row_at(at))
                .collect();
            assert_eq!(
                kept,
                vec![2, 3, 4, 5],
                "soloing a branch did not leave exactly what is behind its tip"
            );

            // The selection named a row the filter took away, so it lets go
            // rather than pointing at a row that is no longer in the list.
            assert_eq!(graph.selected_entry_idx, None);

            graph.toggle_solo("feature".into(), cx);
            assert_eq!(graph.rows_in_the_list(all), all);
        });
    }

    #[gpui::test]
    async fn test_unlabelling_remotes_leaves_the_local_branch_alone(cx: &mut TestAppContext) {
        init_test(cx);
        let (git_graph, cx) =
            drawn_history(cx, labelled_commits(), gpui::size(px(1400.), px(800.))).await;

        git_graph.update_in(cx, |graph, _window, cx| {
            graph.remote_names = vec!["origin".into()];

            let labels = |graph: &GitGraph| -> Vec<(RefKind, SharedString)> {
                graph.refs_of(0, Some("main"))
            };
            assert_eq!(labels(graph).len(), 2, "the fixture needs both labels");

            graph.filter.hide_remotes = true;
            graph.apply_filter(cx);

            let left = labels(graph);
            assert_eq!(
                left,
                vec![(RefKind::Head, "main".into())],
                "unlabelling the remotes took the local branch of the same name with it"
            );
            // And no row went anywhere: this is about labels, not commits.
            assert_eq!(
                graph.rows_in_the_list(graph.graph_data.commits.len()),
                graph.graph_data.commits.len()
            );

            graph.filter.hide_tags = true;
            graph.apply_filter(cx);
            assert!(
                graph.refs_of(4, Some("main")).is_empty(),
                "the tag is still labelled"
            );
        });
    }

    #[gpui::test]
    async fn test_picking_more_than_one_commit(cx: &mut TestAppContext) {
        init_test(cx);
        let (git_graph, cx) =
            drawn_history(cx, unlabelled_commits(10), gpui::size(px(1200.), px(800.))).await;

        git_graph.update_in(cx, |graph, _window, cx| {
            let sha_at = |graph: &GitGraph, row: usize| graph.graph_data.commits[row].data.sha;

            // With nothing picked, a command is about the row it was opened on.
            assert_eq!(graph.picked_commits(3), vec![sha_at(graph, 3)]);

            // Ctrl adds rows one at a time, and a command gets them oldest last
            // -- the order the history is drawn in, which is the order they
            // have to be replayed in.
            graph.select_entry(2, ScrollStrategy::Nearest, cx);
            graph.toggle_picked(5);
            graph.toggle_picked(2);
            graph.toggle_picked(8);
            assert_eq!(
                graph.picked_commits(5),
                vec![sha_at(graph, 2), sha_at(graph, 5), sha_at(graph, 8)]
            );

            // Picking the same row again puts it back.
            graph.toggle_picked(5);
            assert_eq!(
                graph.picked_commits(2),
                vec![sha_at(graph, 2), sha_at(graph, 8)]
            );

            // Shift reaches from the selection to the row, whichever way round.
            graph.select_entry(6, ScrollStrategy::Nearest, cx);
            graph.pick_through(3);
            assert_eq!(
                graph.picked_commits(4),
                vec![
                    sha_at(graph, 3),
                    sha_at(graph, 4),
                    sha_at(graph, 5),
                    sha_at(graph, 6)
                ]
            );

            // A command opened on a row that is not picked is about that row
            // alone, whatever else is picked.
            assert_eq!(graph.picked_commits(9), vec![sha_at(graph, 9)]);
        });
    }

    #[gpui::test]
    async fn test_the_work_in_progress_row(cx: &mut TestAppContext) {
        init_test(cx);

        // Nothing uncommitted: the history starts at its first commit.
        {
            let (_graph, cx) =
                drawn_history(cx, unlabelled_commits(8), gpui::size(px(1200.), px(700.))).await;
            assert!(
                cx.debug_bounds("GRAPH_WORKING_TREE").is_none(),
                "a clean working tree drew a row for work that is not there"
            );
        }

        let (_graph, cx) = drawn_history_with_changes(
            cx,
            unlabelled_commits(8),
            &[
                ("file.txt", FileStatus::Untracked),
                ("other.txt", FileStatus::Untracked),
            ],
            gpui::size(px(1200.), px(700.)),
        )
        .await;

        let wip = cx
            .debug_bounds("GRAPH_WORKING_TREE")
            .expect("two changed files should be worth a row");
        let first_commit = cx
            .debug_bounds(selector("GRAPH_CELL", 0))
            .expect("the first commit should have been painted");
        assert!(
            wip.origin.y + wip.size.height <= first_commit.origin.y + px(0.6),
            "the work in progress is drawn at {} and the first commit at {}; \
             what is not committed yet belongs above what is",
            wip.origin.y,
            first_commit.origin.y
        );
        assert_eq!(
            wip.size.height, first_commit.size.height,
            "the row for uncommitted work is not the height of a row"
        );
    }

    #[gpui::test]
    async fn test_a_lane_past_the_edge_becomes_a_ring(cx: &mut TestAppContext) {
        init_test(cx);
        let (git_graph, cx) =
            drawn_history(cx, deep_commits(12), gpui::size(px(1000.), px(900.))).await;

        let (metrics, layout, lanes) = git_graph.update_in(cx, |graph, window, cx| {
            (
                graph.row_metrics(window, cx),
                graph.history_layout(window, cx),
                graph
                    .graph_data
                    .commits
                    .iter()
                    .map(|commit| commit.lane)
                    .collect::<Vec<_>>(),
            )
        });
        assert!(
            lanes.iter().copied().max().unwrap_or(0) >= layout.lane_cap,
            "the fixture was meant to run deeper than the column shows"
        );

        let mut past_the_edge = Vec::new();
        for idx in 0..lanes.len() {
            let (Some(cell), Some(node)) = (
                cx.debug_bounds(selector("GRAPH_CELL", idx)),
                cx.debug_bounds(selector("GRAPH_NODE", idx)),
            ) else {
                continue;
            };

            // Whatever its lane, a commit is drawn inside the column: a dot has
            // to be somewhere the reader can see it.
            assert!(
                node.origin.x >= cell.origin.x - px(0.6)
                    && node.origin.x + node.size.width <= cell.origin.x + cell.size.width + px(0.6),
                "row {idx} in lane {} is drawn outside the graph column",
                lanes[idx]
            );

            if lanes[idx] >= layout.lane_cap {
                past_the_edge.push((idx, node.origin.x - cell.origin.x));
            }
        }

        assert!(
            past_the_edge.len() >= 2,
            "only {} rows were past the edge; the fixture proves nothing",
            past_the_edge.len()
        );
        let edge = metrics.lane_center_in(layout.lane_cap, 0) - metrics.node / 2.0;
        for (idx, at) in past_the_edge {
            assert!(
                (at - edge).abs() < px(0.6),
                "row {idx} is past the edge but is drawn at {at} rather than on it, at {edge}"
            );
        }
    }

    #[gpui::test]
    async fn test_shift_and_the_wheel_walk_the_lane_window(cx: &mut TestAppContext) {
        init_test(cx);
        let (git_graph, cx) =
            drawn_history(cx, deep_commits(12), gpui::size(px(1000.), px(900.))).await;

        let furthest = git_graph.update_in(cx, |graph, window, cx| {
            let layout = graph.history_layout(window, cx);
            graph.furthest_lane(layout)
        });
        assert!(furthest > 0, "the fixture has nothing to scroll to");

        fn wheel(shift: bool) -> ScrollWheelEvent {
            ScrollWheelEvent {
                position: point(px(400.), px(300.)),
                delta: gpui::ScrollDelta::Pixels(point(px(0.), px(-60.))),
                modifiers: gpui::Modifiers {
                    shift,
                    ..Default::default()
                },
                touch_phase: gpui::TouchPhase::Moved,
            }
        }
        assert_eq!(
            git_graph.read_with(&*cx, |graph, _| graph.graph_first_lane.get()),
            0
        );

        // Without Shift the wheel belongs to the list, as it always has.
        git_graph.update_in(cx, |graph, window, cx| {
            graph.handle_lane_scroll(&wheel(false), window, cx);
        });
        assert_eq!(
            git_graph.read_with(&*cx, |graph, _| graph.graph_first_lane.get()),
            0,
            "a plain wheel moved the lanes sideways"
        );

        git_graph.update_in(cx, |graph, window, cx| {
            graph.handle_lane_scroll(&wheel(true), window, cx);
        });
        assert_eq!(
            git_graph.read_with(&*cx, |graph, _| graph.graph_first_lane.get()),
            1,
            "Shift and the wheel did not walk the lane window"
        );

        // And it stops where the last lane meets the right edge.
        for _ in 0..(furthest + 5) {
            git_graph.update_in(cx, |graph, window, cx| {
                graph.handle_lane_scroll(&wheel(true), window, cx);
            });
        }
        assert_eq!(
            git_graph.read_with(&*cx, |graph, _| graph.graph_first_lane.get()),
            furthest,
            "the lane window ran past the last lane"
        );
    }

    #[gpui::test]
    async fn test_every_node_sits_on_its_own_row(cx: &mut TestAppContext) {
        init_test(cx);
        let mut rng = StdRng::seed_from_u64(7);
        let commits = generate_random_commit_dag(&mut rng, 40, true);
        let (git_graph, cx) = drawn_history(cx, commits, gpui::size(px(1200.), px(800.))).await;

        let (metrics, layout, lanes) = git_graph.update_in(cx, |graph, window, cx| {
            (
                graph.row_metrics(window, cx),
                graph.history_layout(window, cx),
                graph
                    .graph_data
                    .commits
                    .iter()
                    .map(|commit| commit.lane)
                    .collect::<Vec<_>>(),
            )
        });
        let row_height = metrics.row;
        let drawn_rows = lanes.len();
        assert!(drawn_rows > 0);

        let mut measured = 0;
        let mut labels_drawn = 0;
        for idx in 0..drawn_rows {
            let Some(cell) = cx.debug_bounds(selector("GRAPH_CELL", idx)) else {
                continue;
            };
            let node = cx
                .debug_bounds(selector("GRAPH_NODE", idx))
                .unwrap_or_else(|| panic!("row {idx} drew a graph cell with no node in it"));
            measured += 1;

            assert_eq!(
                cell.size.height, row_height,
                "row {idx}: the graph cell is {} tall where the list lays out {}",
                cell.size.height, row_height
            );
            assert!(
                (node.center().y - cell.center().y).abs() < px(0.6),
                "row {idx}: the node is centred at {} and its row at {}",
                node.center().y,
                cell.center().y
            );
            let expected_x = cell.origin.x
                + node_left(metrics, place_of(lanes[idx], layout), 0, layout.lane_cap);
            assert!(
                (node.origin.x - expected_x).abs() < px(0.6),
                "row {idx}: the node starts at {} where its lane puts it at {expected_x}",
                node.origin.x
            );
            assert_eq!(
                node.size.width, node.size.height,
                "row {idx}: the node is not round"
            );

            for name in ["GRAPH_SUBJECT", "GRAPH_AGE"] {
                if let Some(bounds) = cx.debug_bounds(selector(name, idx)) {
                    assert_eq!(
                        bounds.size.height, row_height,
                        "row {idx}: {name} is {} tall, the row is {row_height}",
                        bounds.size.height
                    );
                }
            }

            // A label names one commit. Drawn beside any other row it names the
            // wrong one, which is worse than not being drawn at all.
            if let Some(label) = cx.debug_bounds(selector("GRAPH_REFS", idx)) {
                labels_drawn += 1;
                assert!(
                    (label.center().y - cell.center().y).abs() < px(0.6),
                    "row {idx}: its label is centred at {} and the commit it names at {}",
                    label.center().y,
                    cell.center().y
                );
            }
        }

        assert!(
            labels_drawn > 0,
            "no row carried a label, so nothing proved a label lands on its own row"
        );
        let wanted = rows_a_pane_must_paint(px(800.), row_height).min(drawn_rows);
        assert!(
            measured >= wanted,
            "only {measured} of the {wanted} rows a pane this tall holds were painted; \
             the list is not rendering, and every check above passed over what is missing"
        );
    }

    #[gpui::test]
    async fn test_the_history_fits_every_width_it_is_given(cx: &mut TestAppContext) {
        init_test(cx);
        let mut rng = StdRng::seed_from_u64(11);
        let commits = generate_random_commit_dag(&mut rng, 30, true);
        let (git_graph, cx) = drawn_history(cx, commits, gpui::size(px(1200.), px(800.))).await;

        let mut width = px(200.);
        while width <= px(1600.) {
            let size = gpui::size(width, px(600.));
            for _ in 0..2 {
                cx.draw(point(px(0.), px(0.)), size, |_, _| {
                    git_graph.clone().into_any_element()
                });
                cx.run_until_parked();
            }

            let (layout, metrics, lanes) = git_graph.update_in(cx, |graph, window, cx| {
                (
                    graph.history_layout(window, cx),
                    graph.row_metrics(window, cx),
                    graph
                        .graph_data
                        .commits
                        .iter()
                        .map(|commit| commit.lane)
                        .collect::<Vec<_>>(),
                )
            });

            let mut rows_seen = 0;
            for idx in 0..30 {
                let Some(cell) = cx.debug_bounds(selector("GRAPH_CELL", idx)) else {
                    continue;
                };
                rows_seen += 1;

                assert!(
                    cx.debug_bounds(selector("GRAPH_SUBJECT", idx)).is_some(),
                    "at {width:?} the subject went missing from row {idx}"
                );

                let age = cx.debug_bounds(selector("GRAPH_AGE", idx));
                assert_eq!(
                    age.is_some(),
                    matches!(layout.age, AgeShown::Column),
                    "at {width:?} ({:?}) row {idx} disagrees about having an age column",
                    layout.age
                );

                for name in ["GRAPH_CELL", "GRAPH_SUBJECT", "GRAPH_AGE", "GRAPH_REFS"] {
                    let Some(bounds) = cx.debug_bounds(selector(name, idx)) else {
                        continue;
                    };
                    assert!(
                        bounds.origin.x >= px(-0.01)
                            && bounds.origin.x + bounds.size.width <= width + px(0.01),
                        "at {width:?} {name} on row {idx} runs from {} to {}, past the edge",
                        bounds.origin.x,
                        bounds.origin.x + bounds.size.width
                    );
                }

                assert!(
                    cell.size.width > px(0.),
                    "at {width:?} the graph column collapsed to nothing"
                );

                // The areas of a row are laid side by side. One drawn over
                // another is a label on a subject or a subject on an age, and
                // no width should ever produce it.
                let mut drawn: Vec<(&str, Bounds<Pixels>)> = Vec::new();
                let columns: &[&str] = match layout.labels.has_a_column() {
                    true => &["GRAPH_REFS", "GRAPH_CELL", "GRAPH_SUBJECT", "GRAPH_AGE"],
                    // Too narrow for a column, the label rides inside the
                    // subject, so it is checked against that instead.
                    false => &["GRAPH_CELL", "GRAPH_SUBJECT", "GRAPH_AGE"],
                };
                for name in columns {
                    if let Some(bounds) = cx.debug_bounds(selector(name, idx)) {
                        drawn.push((name, bounds));
                    }
                }
                if !layout.labels.has_a_column()
                    && let (Some(label), Some(subject)) = (
                        cx.debug_bounds(selector("GRAPH_REFS", idx)),
                        cx.debug_bounds(selector("GRAPH_SUBJECT", idx)),
                    )
                {
                    assert!(
                        label.origin.x >= subject.origin.x - px(0.6)
                            && label.origin.x + label.size.width
                                <= subject.origin.x + subject.size.width + px(0.6),
                        "at {width:?} on row {idx} the inline label is not inside the subject"
                    );
                }
                for pair in drawn.windows(2) {
                    let (before, after) = (pair[0], pair[1]);
                    assert!(
                        before.1.origin.x + before.1.size.width <= after.1.origin.x + px(0.6),
                        "at {width:?} on row {idx} {} ends at {} and {} starts at {}",
                        before.0,
                        before.1.origin.x + before.1.size.width,
                        after.0,
                        after.1.origin.x
                    );
                }

                let node = cx
                    .debug_bounds(selector("GRAPH_NODE", idx))
                    .unwrap_or_else(|| panic!("at {width:?} row {idx} painted no node"));
                let expected_x = cell.origin.x
                    + node_left(metrics, place_of(lanes[idx], layout), 0, layout.lane_cap);
                assert!(
                    (node.origin.x - expected_x).abs() < px(0.6),
                    "at {width:?} the node on row {idx} starts at {} \
                     where its lane puts it at {expected_x}",
                    node.origin.x
                );

                // Whenever the lanes fit at all, no commit may be left without a
                // dot: a row whose node is off the edge reads as if nothing
                // happened on it.
                let shown = (lanes.iter().copied().max().unwrap_or(0) + 1).min(layout.lane_cap);
                if metrics.width_for(shown) <= cell.size.width + px(0.6) {
                    assert!(
                        node.origin.x >= cell.origin.x - px(0.6)
                            && node.origin.x + node.size.width
                                <= cell.origin.x + cell.size.width + px(0.6),
                        "at {width:?} the node on row {idx} is outside its column"
                    );
                }
            }

            let wanted = rows_a_pane_must_paint(px(600.), metrics.row).min(lanes.len());
            assert!(
                rows_seen >= wanted,
                "at {width:?} only {rows_seen} of the {wanted} rows a pane this tall \
                 holds were painted"
            );

            width += px(20.);
        }
    }
}
