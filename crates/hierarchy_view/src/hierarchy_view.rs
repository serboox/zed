use anyhow::Result;
use clang_diagnostics::{NamedType, Place, Related, Relatives};
use editor::{Editor, SelectionEffects, scroll::Autoscroll};
use gpui::{
    Action, AnyElement, App, AppContext as _, AsyncApp, AsyncWindowContext, Context, ElementId,
    Entity, EventEmitter, FocusHandle, Focusable, InteractiveElement, IntoElement, ParentElement,
    Pixels, Render, SharedString, Styled, Subscription, UniformListScrollHandle, WeakEntity,
    Window, actions, div, px, uniform_list,
};
use language::{Anchor, Location, PointUtf16, SymbolKind};
use project::{
    Project,
    hierarchies::{
        CallHierarchyItem, HierarchyOutcome, TypeHierarchyItem, incoming_calls, outgoing_calls,
        prepare_call_hierarchy, prepare_type_hierarchy, subtypes, supertypes,
    },
    lsp_store::LspStore,
};
use std::ops::Range;
use std::path::Path;
use symbol_index::{
    SymbolIndex,
    call_hierarchy::{self, Called},
};
use ui::{Icon, IconButton, IconName, IconSize, Label, LabelSize, Tooltip, cyberpunk, prelude::*};
use util::ResultExt as _;
use workspace::{
    Workspace,
    dock::{DockPosition, Panel, PanelEvent},
};

actions!(
    hierarchy_view,
    [
        /// Shows what calls the symbol under the cursor.
        ShowIncomingCalls,
        /// Shows what the symbol under the cursor calls.
        ShowOutgoingCalls,
        /// Shows the supertypes of the symbol under the cursor.
        ShowSupertypes,
        /// Shows the subtypes of the symbol under the cursor.
        ShowSubtypes,
        /// Toggles focus on the call & type hierarchy panel.
        ToggleFocus,
    ]
);

const HIERARCHY_PANEL_KEY: &str = "HierarchyPanel";

pub fn init(cx: &mut App) {
    cx.observe_new(|workspace: &mut Workspace, _, _| {
        workspace.register_action(|workspace, _: &ShowIncomingCalls, window, cx| {
            HierarchyPanel::start(workspace, Direction::IncomingCalls, window, cx);
        });
        workspace.register_action(|workspace, _: &ShowOutgoingCalls, window, cx| {
            HierarchyPanel::start(workspace, Direction::OutgoingCalls, window, cx);
        });
        workspace.register_action(|workspace, _: &ShowSupertypes, window, cx| {
            HierarchyPanel::start(workspace, Direction::Supertypes, window, cx);
        });
        workspace.register_action(|workspace, _: &ShowSubtypes, window, cx| {
            HierarchyPanel::start(workspace, Direction::Subtypes, window, cx);
        });
        workspace.register_action(|workspace, _: &ToggleFocus, window, cx| {
            workspace.toggle_panel_focus::<HierarchyPanel>(window, cx);
        });
    })
    .detach();
}

/// Which direction the whole tree currently reads, and (implicitly, since a
/// call item and a type item are different shapes) which kind of hierarchy it
/// is. A reader flips between the two members of a pair -- incoming/outgoing,
/// super/sub -- without ever crossing from one pair to the other, since the
/// items in the tree only answer one kind of question.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Direction {
    IncomingCalls,
    OutgoingCalls,
    Supertypes,
    Subtypes,
}

impl Direction {
    fn is_call(self) -> bool {
        matches!(self, Direction::IncomingCalls | Direction::OutgoingCalls)
    }

    fn title(self) -> &'static str {
        match self {
            Direction::IncomingCalls => "Incoming Calls",
            Direction::OutgoingCalls => "Outgoing Calls",
            Direction::Supertypes => "Supertypes",
            Direction::Subtypes => "Subtypes",
        }
    }

    /// The other member of this direction's pair. Flipping never crosses from
    /// a call direction to a type direction, since the tree it applies to is
    /// homogeneous.
    fn flip(self) -> Direction {
        match self {
            Direction::IncomingCalls => Direction::OutgoingCalls,
            Direction::OutgoingCalls => Direction::IncomingCalls,
            Direction::Supertypes => Direction::Subtypes,
            Direction::Subtypes => Direction::Supertypes,
        }
    }

    /// Wording for `HierarchyOutcome::Unsupported`. Deliberately says "not
    /// available here" rather than "the language does not support this": the
    /// same outcome also covers a shared project, where the real language
    /// server may well support the request but this window has no local one
    /// to ask (see the module doc comment).
    fn unsupported_message(self) -> SharedString {
        let what = if self.is_call() { "Call" } else { "Type" };
        SharedString::from(format!(
            "{what} hierarchy is not available here: no reachable language server offers it for this file."
        ))
    }

    /// Wording for `HierarchyOutcome::NoResults`, kept visibly different from
    /// `unsupported_message` -- that difference is the whole point of the
    /// milestone this panel exists for.
    fn empty_message(self) -> SharedString {
        SharedString::from(match self {
            Direction::IncomingCalls => "Nothing calls this.",
            Direction::OutgoingCalls => "This calls nothing.",
            Direction::Supertypes => "No supertypes.",
            Direction::Subtypes => "No subtypes.",
        })
    }
}

/// What the panel says above a subtype list the compiler's own front end
/// answered.
///
/// The front end reads one translation unit: the file the reader is in, and
/// every header it reaches. A class derived from in some other file of the
/// project is not in that program and cannot be found in it, and a list that
/// left the reader to assume otherwise would be worse than no list -- they
/// would take a missing subclass for a subclass that does not exist.
const WHAT_THE_FRONT_END_LOOKED_AT_FOR_SUBTYPES: &str = concat!(
    "From the compiler front end, which read only this file and the headers ",
    "it includes. Classes deriving from this elsewhere in the project are not listed.",
);

/// One row's worth of display data, converted from whichever of the two
/// protocol-layer item types it came from. Kept separate from `RowSource` so
/// the tree and rendering code never have to match on call-vs-type to read a
/// name, a kind or a location.
#[derive(Clone)]
struct HierarchyRow {
    name: SharedString,
    /// Already the word a reader sees: a server item and an index item name
    /// what they are in different vocabularies, and nothing below here has to
    /// know which one it is holding.
    kind: SharedString,
    location: Location,
    selection_range: Range<Anchor>,
    source: RowSource,
}

/// The original, typed item, kept verbatim so a row can be handed straight
/// back to `incoming_calls`/`outgoing_calls`/`supertypes`/`subtypes` when it
/// is expanded, without reconstructing anything.
#[derive(Clone)]
enum RowSource {
    Call(CallHierarchyItem),
    Type(TypeHierarchyItem),
    /// A row the project's own index worked out, with no server asked. The
    /// name is all it takes to ask again, since that is how the index is
    /// asked in the first place; `None` for a call written at file scope,
    /// which names no declaration to ask about.
    Indexed(Option<SharedString>),
    /// A type the compiler's own front end named, with no server asked. The
    /// place its declaration is written at is all it takes to ask again, and
    /// it is a place in some file the translation unit reaches rather than in
    /// the file the reader started from.
    Clang(Place),
}

impl From<&CallHierarchyItem> for HierarchyRow {
    fn from(item: &CallHierarchyItem) -> Self {
        Self {
            name: item.name.clone(),
            kind: SharedString::from(kind_label(item.kind)),
            location: item.location.clone(),
            selection_range: item.selection_range.clone(),
            source: RowSource::Call(item.clone()),
        }
    }
}

impl From<&TypeHierarchyItem> for HierarchyRow {
    fn from(item: &TypeHierarchyItem) -> Self {
        Self {
            name: item.name.clone(),
            kind: SharedString::from(kind_label(item.kind)),
            location: item.location.clone(),
            selection_range: item.selection_range.clone(),
            source: RowSource::Type(item.clone()),
        }
    }
}

/// One node of the tree. `expansion` is only ever advanced forward by a user
/// action (`toggle_expand`) or reset back to `Collapsed` by a direction flip
/// -- it is never fetched eagerly.
struct Node {
    row: HierarchyRow,
    expansion: Expansion,
}

impl Node {
    fn new(row: HierarchyRow) -> Self {
        Self {
            row,
            expansion: Expansion::Collapsed,
        }
    }
}

enum Expansion {
    Collapsed,
    Loading,
    /// The three-way outcome of asking the server for this node's own
    /// children, kept distinguishable all the way down the tree, not just at
    /// the root.
    Loaded(HierarchyOutcome<Node>),
    /// The request itself failed (a real error, not a capability answer). Kept
    /// apart from `Loaded(NoResults)` so a transient failure is never shown as
    /// though the server had genuinely answered "nothing".
    Failed(SharedString),
}

/// What the panel is currently showing.
enum Content {
    /// Nothing has been asked for yet.
    Empty,
    Loading {
        direction: Direction,
    },
    Ready {
        direction: Direction,
        outcome: HierarchyOutcome<Node>,
        /// The buffer whose translation unit answered this tree, where the
        /// compiler's own front end answered it. A type question is asked
        /// inside one translation unit, so every row in such a tree is asked
        /// about through the same buffer -- and that is also the limit the
        /// panel has to admit to for subtypes.
        clang_origin: Option<Entity<language::Buffer>>,
    },
    Failed {
        direction: Direction,
        message: SharedString,
    },
}

pub struct HierarchyPanel {
    workspace: WeakEntity<Workspace>,
    project: Entity<Project>,
    focus_handle: FocusHandle,
    position: DockPosition,
    active: bool,
    scroll_handle: UniformListScrollHandle,
    content: Content,
    _subscriptions: Vec<Subscription>,
}

impl HierarchyPanel {
    pub async fn load(
        workspace: WeakEntity<Workspace>,
        mut cx: AsyncWindowContext,
    ) -> Result<Entity<Self>> {
        workspace.update_in(&mut cx, |workspace, window, cx| {
            Self::new(workspace, window, cx)
        })
    }

    fn new(
        workspace: &mut Workspace,
        _window: &mut Window,
        cx: &mut Context<Workspace>,
    ) -> Entity<Self> {
        let project = workspace.project().clone();
        let workspace_handle = cx.entity().downgrade();
        cx.new(|cx| Self {
            workspace: workspace_handle,
            project,
            focus_handle: cx.focus_handle(),
            position: DockPosition::Right,
            active: false,
            scroll_handle: UniformListScrollHandle::new(),
            content: Content::Empty,
            _subscriptions: Vec::new(),
        })
    }

    /// Shared entry point for all four actions: finds the symbol under the
    /// cursor in the active editor's buffer, reveals the panel, and starts a
    /// fresh tree from there.
    fn start(
        workspace: &mut Workspace,
        direction: Direction,
        window: &mut Window,
        cx: &mut Context<Workspace>,
    ) {
        let Some(editor) = workspace.active_item_as::<Editor>(cx) else {
            return;
        };
        let Some((buffer, position)) = editor.update(cx, |editor, cx| {
            let snapshot = editor.display_snapshot(cx);
            let head = editor.selections.newest::<PointUtf16>(&snapshot).head();
            editor
                .buffer()
                .read(cx)
                .as_singleton()
                .map(|buffer| (buffer, head))
        }) else {
            return;
        };

        let Some(panel) = workspace.focus_panel::<HierarchyPanel>(window, cx) else {
            return;
        };
        panel.update(cx, |panel, cx| {
            panel.begin(direction, buffer, position, cx);
        });
    }

    fn begin(
        &mut self,
        direction: Direction,
        buffer: Entity<language::Buffer>,
        position: PointUtf16,
        cx: &mut Context<Self>,
    ) {
        self.content = Content::Loading { direction };
        cx.notify();

        let lsp_store = self.project.read(cx).lsp_store();
        let project = self.project.downgrade();
        let index = self.index(cx);
        cx.spawn(async move |this, cx| {
            let mut result = if direction.is_call() {
                prepare_call_hierarchy(&lsp_store, &buffer, position, cx)
                    .await
                    .map(root_outcome_from_call)
            } else {
                prepare_type_hierarchy(&lsp_store, &buffer, position, cx)
                    .await
                    .map(root_outcome_from_type)
            };
            // The server first and unchanged. Anything else answers only into
            // its silence, which is what `Unsupported` means -- no reachable
            // server offers this for this file. Which source that is follows
            // from the question: the index records declarations and the places
            // names occur, so it answers about calls, and nothing it holds is
            // an edge between two types; the compiler's own front end holds
            // the class graph, and answers about types for the two languages
            // it is a front end for.
            let mut clang_origin = None;
            if let Ok(HierarchyOutcome::Unsupported) = &result {
                if direction.is_call() {
                    if let Some(index) = index {
                        result =
                            Ok(root_from_the_index(&project, &index, &buffer, position, cx).await);
                    }
                } else if let Some(outcome) =
                    root_from_clang(&project, &buffer, position, direction, cx).await
                {
                    clang_origin = Some(buffer.clone());
                    result = Ok(outcome);
                }
            }
            this.update(cx, |this, cx| {
                this.content = match result {
                    Ok(outcome) => Content::Ready {
                        direction,
                        outcome,
                        clang_origin,
                    },
                    Err(error) => Content::Failed {
                        direction,
                        message: SharedString::from(format!("{error:#}")),
                    },
                };
                cx.notify();
            })
            .log_err();
        })
        .detach();
    }

    /// Re-roots the tree in the paired direction: the root items stay (a
    /// direction flip never re-asks "prepare"), but every already-fetched
    /// child is discarded rather than kept alongside the new ones, since it
    /// answered the wrong question.
    fn flip_direction(&mut self, cx: &mut Context<Self>) {
        let Content::Ready {
            direction, outcome, ..
        } = &mut self.content
        else {
            return;
        };
        *direction = direction.flip();
        if let HierarchyOutcome::Found(nodes) = outcome {
            for node in nodes {
                node.expansion = Expansion::Collapsed;
            }
        }
        cx.notify();
    }

    fn toggle_expand(&mut self, path: Vec<usize>, cx: &mut Context<Self>) {
        let Content::Ready { outcome, .. } = &mut self.content else {
            return;
        };
        let Some(node) = node_at_mut(outcome, &path) else {
            return;
        };
        if matches!(node.expansion, Expansion::Collapsed) {
            self.request_children(path, cx);
        } else if !matches!(node.expansion, Expansion::Loading) {
            node.expansion = Expansion::Collapsed;
            cx.notify();
        }
    }

    /// Fetches children for exactly the node at `path` -- the whole reason a
    /// path identifies one node instead of re-fetching the tree is so this
    /// request stays scoped to it.
    fn request_children(&mut self, path: Vec<usize>, cx: &mut Context<Self>) {
        let Content::Ready {
            direction,
            outcome,
            clang_origin,
        } = &mut self.content
        else {
            return;
        };
        let direction = *direction;
        let clang_origin = clang_origin.clone();
        let Some(node) = node_at_mut(outcome, &path) else {
            return;
        };
        let source = node.row.source.clone();
        node.expansion = Expansion::Loading;
        cx.notify();

        let lsp_store = self.project.read(cx).lsp_store();
        let project = self.project.downgrade();
        let index = self.index(cx);
        cx.spawn(async move |this, cx| {
            let result = fetch_children(
                &lsp_store,
                &project,
                &index,
                &clang_origin,
                direction,
                &source,
                cx,
            )
            .await;
            this.update(cx, |this, cx| {
                let Content::Ready { outcome, .. } = &mut this.content else {
                    return;
                };
                if let Some(node) = node_at_mut(outcome, &path) {
                    node.expansion = match result {
                        Ok(outcome) => Expansion::Loaded(outcome),
                        Err(error) => Expansion::Failed(SharedString::from(format!("{error:#}"))),
                    };
                }
                cx.notify();
            })
            .log_err();
        })
        .detach();
    }

    /// The project's own index, where one has been built for it. `None` in a
    /// window over a project nothing indexes -- a remote project, or one
    /// opened before the index had a chance to start -- and the panel is then
    /// exactly what it was before the index answered anything.
    fn index(&self, cx: &App) -> Option<WeakEntity<SymbolIndex>> {
        symbol_index::of_project(&self.project, cx).map(|index| index.downgrade())
    }

    fn open_row(&self, row: &HierarchyRow, window: &mut Window, cx: &mut Context<Self>) {
        let workspace = self.workspace.clone();
        let buffer = row.location.buffer.clone();
        let range = row.selection_range.clone();
        cx.spawn_in(window, async move |_this, cx| {
            workspace
                .update_in(cx, |workspace, window, cx| {
                    let pane = workspace.active_pane().clone();
                    let editor = workspace.open_project_item::<Editor>(
                        pane, buffer, true, true, true, true, window, cx,
                    );
                    editor.update(cx, |editor, cx| {
                        let multibuffer_snapshot = editor.buffer().read(cx).snapshot(cx);
                        let (Some(start), Some(end)) = (
                            multibuffer_snapshot.anchor_in_buffer(range.start),
                            multibuffer_snapshot.anchor_in_buffer(range.end),
                        ) else {
                            return;
                        };
                        editor.change_selections(
                            SelectionEffects::scroll(Autoscroll::center()),
                            window,
                            cx,
                            |selections| selections.select_ranges([start..end]),
                        );
                    });
                })
                .log_err();
        })
        .detach();
    }

    fn render_toolbar(&self, cx: &mut Context<Self>) -> AnyElement {
        let title = match &self.content {
            Content::Empty => "Hierarchy".to_string(),
            Content::Loading { direction }
            | Content::Failed { direction, .. }
            | Content::Ready { direction, .. } => direction.title().to_string(),
        };

        let mut row = h_flex()
            .id("hierarchy-view-toolbar")
            .debug_selector(|| "hierarchy-view-toolbar".to_string())
            .w_full()
            .items_center()
            .justify_between()
            .px_2()
            .py_1()
            .border_b_1()
            .border_color(cx.theme().colors().border)
            .child(Label::new(title).size(LabelSize::Small));

        if let Content::Ready { direction, .. } = &self.content {
            let direction = *direction;
            let (first_icon, first_label, first_active, second_icon, second_label, second_active) =
                if direction.is_call() {
                    (
                        IconName::ArrowUp,
                        "Incoming Calls",
                        direction == Direction::IncomingCalls,
                        IconName::ArrowDown,
                        "Outgoing Calls",
                        direction == Direction::OutgoingCalls,
                    )
                } else {
                    (
                        IconName::ArrowUp,
                        "Supertypes",
                        direction == Direction::Supertypes,
                        IconName::ArrowDown,
                        "Subtypes",
                        direction == Direction::Subtypes,
                    )
                };
            row = row.child(cyberpunk::segmented(vec![
                div()
                    .id("hierarchy-view-direction-first")
                    .debug_selector(|| "hierarchy-view-direction-first".to_string())
                    .child(
                        IconButton::new("hierarchy-view-direction-first", first_icon)
                            .icon_size(IconSize::Small)
                            .toggle_state(first_active)
                            .tooltip(Tooltip::text(first_label))
                            .on_click(cx.listener(move |this, _, _, cx| {
                                if !first_active {
                                    this.flip_direction(cx);
                                }
                            })),
                    )
                    .into_any_element(),
                div()
                    .id("hierarchy-view-direction-second")
                    .debug_selector(|| "hierarchy-view-direction-second".to_string())
                    .child(
                        IconButton::new("hierarchy-view-direction-second", second_icon)
                            .icon_size(IconSize::Small)
                            .toggle_state(second_active)
                            .tooltip(Tooltip::text(second_label))
                            .on_click(cx.listener(move |this, _, _, cx| {
                                if !second_active {
                                    this.flip_direction(cx);
                                }
                            })),
                    )
                    .into_any_element(),
            ]));
        }

        row.into_any_element()
    }

    fn render_row(&self, flat: &FlatRow, cx: &mut Context<Self>) -> AnyElement {
        match flat {
            FlatRow::Status { depth, text } => div()
                .id(ElementId::from(SharedString::from(format!(
                    "hierarchy-status-{depth}-{text}"
                ))))
                .debug_selector(|| "hierarchy-view-status".to_string())
                .pl(px(*depth as f32 * 16. + 8.))
                .py_1()
                .child(
                    Label::new(text.clone())
                        .size(LabelSize::Small)
                        .color(Color::Muted),
                )
                .into_any_element(),
            FlatRow::Item {
                path,
                depth,
                row,
                expansion,
            } => {
                let path_string = path
                    .iter()
                    .map(|index| index.to_string())
                    .collect::<Vec<_>>()
                    .join("-");
                let item_id =
                    ElementId::from(SharedString::from(format!("hierarchy-row-{path_string}")));
                let caret = match expansion {
                    ExpansionGlyph::Collapsed => IconName::ChevronRight,
                    ExpansionGlyph::Loading => IconName::ArrowCircle,
                    ExpansionGlyph::Expanded => IconName::ChevronDown,
                };
                let toggle_path = path.clone();
                let opened_row = row.clone();
                let file_name = file_name_of(&row.location, cx);
                let row_name = row.name.clone();

                div()
                    .id(item_id)
                    .debug_selector(move || format!("hierarchy-row:{row_name}"))
                    .pl(px(*depth as f32 * 16.))
                    .py_1()
                    .cursor_pointer()
                    .hover(|style| style.bg(cx.theme().colors().ghost_element_hover))
                    .on_click(cx.listener(move |this, _event, window, cx| {
                        this.open_row(&opened_row, window, cx);
                    }))
                    .child(
                        h_flex()
                            .items_center()
                            .gap_1()
                            .child(
                                div()
                                    .id(ElementId::from(SharedString::from(format!(
                                        "hierarchy-toggle-{path_string}"
                                    ))))
                                    .debug_selector(|| "hierarchy-row-toggle".to_string())
                                    .on_click(cx.listener(
                                        move |this, _event: &gpui::ClickEvent, _window, cx| {
                                            cx.stop_propagation();
                                            this.toggle_expand(toggle_path.clone(), cx);
                                        },
                                    ))
                                    .child(
                                        Icon::new(caret).size(IconSize::XSmall).color(Color::Muted),
                                    ),
                            )
                            .child(Label::new(row.name.clone()).size(LabelSize::Small))
                            .child(
                                Label::new(row.kind.clone())
                                    .size(LabelSize::XSmall)
                                    .color(Color::Muted),
                            )
                            .child(
                                Label::new(file_name)
                                    .size(LabelSize::XSmall)
                                    .color(Color::Muted),
                            ),
                    )
                    .into_any_element()
            }
        }
    }
}

fn file_name_of(location: &Location, cx: &App) -> SharedString {
    location
        .buffer
        .read(cx)
        .file()
        .map(|file| SharedString::from(file.file_name(cx).to_string()))
        .unwrap_or_else(|| SharedString::from("unsaved"))
}

fn kind_label(kind: SymbolKind) -> &'static str {
    match kind {
        SymbolKind::File => "file",
        SymbolKind::Module => "module",
        SymbolKind::Namespace => "namespace",
        SymbolKind::Package => "package",
        SymbolKind::Class => "class",
        SymbolKind::Method => "method",
        SymbolKind::Property => "property",
        SymbolKind::Field => "field",
        SymbolKind::Constructor => "constructor",
        SymbolKind::Enum => "enum",
        SymbolKind::Interface => "interface",
        SymbolKind::Function => "function",
        SymbolKind::Variable => "variable",
        SymbolKind::Constant => "constant",
        SymbolKind::String => "string",
        SymbolKind::Number => "number",
        SymbolKind::Boolean => "boolean",
        SymbolKind::Array => "array",
        SymbolKind::Object => "object",
        SymbolKind::Key => "key",
        SymbolKind::Null => "null",
        SymbolKind::EnumMember => "enum member",
        SymbolKind::Struct => "struct",
        SymbolKind::Event => "event",
        SymbolKind::Operator => "operator",
        SymbolKind::TypeParameter => "type parameter",
    }
}

enum ExpansionGlyph {
    Collapsed,
    Loading,
    Expanded,
}

enum FlatRow {
    Item {
        path: Vec<usize>,
        depth: usize,
        row: HierarchyRow,
        expansion: ExpansionGlyph,
    },
    Status {
        depth: usize,
        text: SharedString,
    },
}

fn flatten_content(content: &Content) -> Vec<FlatRow> {
    let mut rows = Vec::new();
    match content {
        Content::Empty => rows.push(FlatRow::Status {
            depth: 0,
            text: SharedString::from(
                "Place the cursor on a symbol, then choose a hierarchy action.",
            ),
        }),
        Content::Loading { .. } => rows.push(FlatRow::Status {
            depth: 0,
            text: SharedString::from("Loading…"),
        }),
        Content::Failed { message, .. } => rows.push(FlatRow::Status {
            depth: 0,
            text: SharedString::from(format!("Request failed: {message}")),
        }),
        Content::Ready {
            direction,
            outcome,
            clang_origin,
        } => {
            if clang_origin.is_some() && *direction == Direction::Subtypes {
                rows.push(FlatRow::Status {
                    depth: 0,
                    text: SharedString::from(WHAT_THE_FRONT_END_LOOKED_AT_FOR_SUBTYPES),
                });
            }
            flatten_outcome(outcome, 0, &mut Vec::new(), *direction, &mut rows);
        }
    }
    rows
}

fn flatten_outcome(
    outcome: &HierarchyOutcome<Node>,
    depth: usize,
    path: &mut Vec<usize>,
    direction: Direction,
    rows: &mut Vec<FlatRow>,
) {
    match outcome {
        HierarchyOutcome::Unsupported => rows.push(FlatRow::Status {
            depth,
            text: direction.unsupported_message(),
        }),
        HierarchyOutcome::NoResults => rows.push(FlatRow::Status {
            depth,
            text: direction.empty_message(),
        }),
        HierarchyOutcome::Found(nodes) => {
            for (index, node) in nodes.iter().enumerate() {
                path.push(index);
                let glyph = match &node.expansion {
                    Expansion::Collapsed => ExpansionGlyph::Collapsed,
                    Expansion::Loading => ExpansionGlyph::Loading,
                    Expansion::Loaded(_) | Expansion::Failed(_) => ExpansionGlyph::Expanded,
                };
                rows.push(FlatRow::Item {
                    path: path.clone(),
                    depth,
                    row: node.row.clone(),
                    expansion: glyph,
                });
                match &node.expansion {
                    Expansion::Collapsed => {}
                    Expansion::Loading => rows.push(FlatRow::Status {
                        depth: depth + 1,
                        text: SharedString::from("Loading…"),
                    }),
                    Expansion::Failed(message) => rows.push(FlatRow::Status {
                        depth: depth + 1,
                        text: SharedString::from(format!("Request failed: {message}")),
                    }),
                    Expansion::Loaded(child_outcome) => {
                        flatten_outcome(child_outcome, depth + 1, path, direction, rows);
                    }
                }
                path.pop();
            }
        }
    }
}

fn node_at_mut<'a>(
    outcome: &'a mut HierarchyOutcome<Node>,
    path: &[usize],
) -> Option<&'a mut Node> {
    let HierarchyOutcome::Found(nodes) = outcome else {
        return None;
    };
    let (&first, rest) = path.split_first()?;
    let node = nodes.get_mut(first)?;
    if rest.is_empty() {
        Some(node)
    } else {
        match &mut node.expansion {
            Expansion::Loaded(child_outcome) => node_at_mut(child_outcome, rest),
            _ => None,
        }
    }
}

async fn fetch_children(
    lsp_store: &Entity<LspStore>,
    project: &WeakEntity<Project>,
    index: &Option<WeakEntity<SymbolIndex>>,
    clang_origin: &Option<Entity<language::Buffer>>,
    direction: Direction,
    source: &RowSource,
    cx: &mut gpui::AsyncApp,
) -> Result<HierarchyOutcome<Node>> {
    match (direction, source) {
        // A call written inside no declaration names none to ask about, and
        // saying so is the whole reason it is a row of its own.
        (_, RowSource::Indexed(None)) => Ok(HierarchyOutcome::NoResults),
        (Direction::IncomingCalls | Direction::OutgoingCalls, RowSource::Indexed(Some(name))) => {
            children_from_the_index(project, index, direction, name, cx).await
        }
        (Direction::Supertypes | Direction::Subtypes, RowSource::Clang(at)) => {
            Ok(children_from_clang(project, clang_origin, at, direction, cx).await)
        }
        (Direction::IncomingCalls, RowSource::Call(item)) => {
            let outcome = incoming_calls(lsp_store, item, cx).await?;
            Ok(match outcome {
                HierarchyOutcome::Unsupported => HierarchyOutcome::Unsupported,
                HierarchyOutcome::NoResults => HierarchyOutcome::NoResults,
                HierarchyOutcome::Found(calls) => HierarchyOutcome::Found(
                    calls
                        .iter()
                        .map(|call| Node::new(HierarchyRow::from(&call.from)))
                        .collect(),
                ),
            })
        }
        (Direction::OutgoingCalls, RowSource::Call(item)) => {
            let outcome = outgoing_calls(lsp_store, item, cx).await?;
            Ok(match outcome {
                HierarchyOutcome::Unsupported => HierarchyOutcome::Unsupported,
                HierarchyOutcome::NoResults => HierarchyOutcome::NoResults,
                HierarchyOutcome::Found(calls) => HierarchyOutcome::Found(
                    calls
                        .iter()
                        .map(|call| Node::new(HierarchyRow::from(&call.to)))
                        .collect(),
                ),
            })
        }
        (Direction::Supertypes, RowSource::Type(item)) => {
            supertypes(lsp_store, item, cx).await.map(map_type_outcome)
        }
        (Direction::Subtypes, RowSource::Type(item)) => {
            subtypes(lsp_store, item, cx).await.map(map_type_outcome)
        }
        _ => anyhow::bail!("hierarchy direction does not match the item's own kind"),
    }
}

fn map_type_outcome(outcome: HierarchyOutcome<TypeHierarchyItem>) -> HierarchyOutcome<Node> {
    match outcome {
        HierarchyOutcome::Unsupported => HierarchyOutcome::Unsupported,
        HierarchyOutcome::NoResults => HierarchyOutcome::NoResults,
        HierarchyOutcome::Found(items) => HierarchyOutcome::Found(
            items
                .iter()
                .map(|item| Node::new(HierarchyRow::from(item)))
                .collect(),
        ),
    }
}

fn root_outcome_from_call(outcome: HierarchyOutcome<CallHierarchyItem>) -> HierarchyOutcome<Node> {
    match outcome {
        HierarchyOutcome::Unsupported => HierarchyOutcome::Unsupported,
        HierarchyOutcome::NoResults => HierarchyOutcome::NoResults,
        HierarchyOutcome::Found(items) => HierarchyOutcome::Found(
            items
                .iter()
                .map(|item| Node::new(HierarchyRow::from(item)))
                .collect(),
        ),
    }
}

fn root_outcome_from_type(outcome: HierarchyOutcome<TypeHierarchyItem>) -> HierarchyOutcome<Node> {
    map_type_outcome(outcome)
}

/// The root the index answers with: the declaration the cursor is on.
///
/// `Unsupported` where the index will not say, which leaves the panel showing
/// exactly the message it showed before any of this existed.
async fn root_from_the_index(
    project: &WeakEntity<Project>,
    index: &WeakEntity<SymbolIndex>,
    buffer: &Entity<language::Buffer>,
    position: PointUtf16,
    cx: &mut AsyncApp,
) -> HierarchyOutcome<Node> {
    let asked = cx.update(|cx| {
        let index = index.upgrade()?;
        call_hierarchy::declaration_under(&index, buffer, position, cx)
    });
    let Some(called) = asked else {
        return HierarchyOutcome::Unsupported;
    };
    match node_from_the_index(project, &called, cx).await {
        Some(node) => HierarchyOutcome::Found(vec![node]),
        None => HierarchyOutcome::Unsupported,
    }
}

/// One row's children, out of the index. Type directions never reach here:
/// only a call direction ever produces an indexed row to expand.
async fn children_from_the_index(
    project: &WeakEntity<Project>,
    index: &Option<WeakEntity<SymbolIndex>>,
    direction: Direction,
    name: &str,
    cx: &mut AsyncApp,
) -> Result<HierarchyOutcome<Node>> {
    let Some(index) = index.as_ref().and_then(|index| index.upgrade()) else {
        return Ok(HierarchyOutcome::Unsupported);
    };
    let asked = cx.update(|cx| match direction {
        Direction::IncomingCalls => call_hierarchy::incoming(&index, name, cx),
        Direction::OutgoingCalls => call_hierarchy::outgoing(&index, name, cx),
        // The index records declarations and the places names occur, not edges
        // between types, so it has nothing to say about a type hierarchy and
        // says nothing rather than approximating one by name.
        Direction::Supertypes | Direction::Subtypes => None,
    });
    let Some(asked) = asked else {
        return Ok(HierarchyOutcome::Unsupported);
    };

    let mut nodes = Vec::new();
    for called in asked.await {
        if let Some(node) = node_from_the_index(project, &called, cx).await {
            nodes.push(node);
        }
    }
    Ok(found_or_nothing(nodes))
}

/// What a list of children means. An empty list is `NoResults` and never
/// `Found(vec![])`: the first says "nothing derives from this" in words the
/// reader can see, and the second draws an expanded row with nothing under it,
/// which reads as a lookup that broke.
fn found_or_nothing(nodes: Vec<Node>) -> HierarchyOutcome<Node> {
    if nodes.is_empty() {
        HierarchyOutcome::NoResults
    } else {
        HierarchyOutcome::Found(nodes)
    }
}

/// What the compiler's own front end says the reader is asking about: the
/// type under the cursor, as the one root of the tree.
///
/// `None` where the front end has nothing to say -- a language it is not a
/// front end for, a project with no compilation database, a cursor that is not
/// on a type. The panel then keeps the message it already had, because an
/// empty type hierarchy and a question that was never answered are not the
/// same thing to a reader deciding whether to believe the panel.
async fn root_from_clang(
    project: &WeakEntity<Project>,
    buffer: &Entity<language::Buffer>,
    position: PointUtf16,
    direction: Direction,
    cx: &mut AsyncApp,
) -> Option<HierarchyOutcome<Node>> {
    let relatives = ask_clang(
        project,
        buffer,
        clang_diagnostics::Target::Under(position),
        direction,
        cx,
    )
    .await?;
    let node = node_from_clang(project, &relatives.subject, cx).await?;
    Some(HierarchyOutcome::Found(vec![node]))
}

/// One row's children, out of the compiler's own front end.
///
/// Asked through the buffer the tree started from, because that is the
/// translation unit the whole tree is read out of: a base class declared in a
/// header is a place inside it, and asking about that place through some other
/// file would be asking about a different program.
async fn children_from_clang(
    project: &WeakEntity<Project>,
    clang_origin: &Option<Entity<language::Buffer>>,
    at: &Place,
    direction: Direction,
    cx: &mut AsyncApp,
) -> HierarchyOutcome<Node> {
    let Some(origin) = clang_origin else {
        return HierarchyOutcome::Unsupported;
    };
    let asked = ask_clang(
        project,
        origin,
        clang_diagnostics::Target::Declared(at.clone()),
        direction,
        cx,
    )
    .await;
    let Some(relatives) = asked else {
        return HierarchyOutcome::Unsupported;
    };

    let mut nodes = Vec::new();
    for related in &relatives.related {
        if let Some(node) = node_from_clang(project, related, cx).await {
            nodes.push(node);
        }
    }
    found_or_nothing(nodes)
}

async fn ask_clang(
    project: &WeakEntity<Project>,
    buffer: &Entity<language::Buffer>,
    target: clang_diagnostics::Target,
    direction: Direction,
    cx: &mut AsyncApp,
) -> Option<Relatives> {
    let asked = cx.update(|cx| {
        let project = project.upgrade()?;
        clang_diagnostics::ask_about_types(&project, buffer, target, related(direction)?, cx)
    });
    asked?.await
}

/// Which way the front end reads the relation for a panel direction, and
/// nothing for a call direction: the front end is asked about types only, and
/// calls have a source of their own.
fn related(direction: Direction) -> Option<Related> {
    match direction {
        Direction::Supertypes => Some(Related::Bases),
        Direction::Subtypes => Some(Related::Derived),
        Direction::IncomingCalls | Direction::OutgoingCalls => None,
    }
}

/// Opens the file the front end named so the row has somewhere to send a
/// reader, and names the row in C++'s own vocabulary rather than the
/// protocol's.
async fn node_from_clang(
    project: &WeakEntity<Project>,
    named: &NamedType,
    cx: &mut AsyncApp,
) -> Option<Node> {
    let location = locate_in_file(project, &named.at.file, named.at.offset, named.end, cx).await?;
    let selection_range = location.range.clone();
    Some(Node::new(HierarchyRow {
        name: SharedString::from(named.name.clone()),
        kind: SharedString::from(named.kind.clone()),
        location,
        selection_range,
        source: RowSource::Clang(named.at.clone()),
    }))
}

/// The span of a declaration's name, as somewhere a reader can be sent.
///
/// The offsets were measured against the file the front end read, which is the
/// buffer's text for the file the reader is in and the file on disk for every
/// header. Both are clamped: a header the reader has edited since has moved
/// under them, and a row that lands a little short is better than one that
/// panics.
async fn locate_in_file(
    project: &WeakEntity<Project>,
    path: &Path,
    start: usize,
    end: usize,
    cx: &mut AsyncApp,
) -> Option<Location> {
    let opened = project
        .update(cx, |project, cx| project.open_local_buffer(path, cx))
        .log_err()?
        .await
        .log_err()?;
    let range = opened.read_with(cx, |opened, _| {
        let snapshot = opened.snapshot();
        let length = snapshot.len();
        snapshot.anchor_before(start.min(length))..snapshot.anchor_after(end.min(length))
    });
    Some(Location {
        buffer: opened,
        range,
    })
}

/// Opens the file the index named so the row has somewhere to send a reader,
/// which is the one thing the index cannot work out on its own.
async fn node_from_the_index(
    project: &WeakEntity<Project>,
    called: &Called,
    cx: &mut AsyncApp,
) -> Option<Node> {
    let location = call_hierarchy::locate(project.clone(), called, cx).await?;
    let name = SharedString::from(called.name.clone());
    let selection_range = location.range.clone();
    let source = if called.at_file_scope() {
        RowSource::Indexed(None)
    } else {
        RowSource::Indexed(Some(name.clone()))
    };
    Some(Node::new(HierarchyRow {
        name,
        kind: SharedString::from(called.kind.clone()),
        location,
        selection_range,
        source,
    }))
}

impl Render for HierarchyPanel {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let rows = flatten_content(&self.content);
        let row_count = rows.len();

        v_flex()
            .key_context("HierarchyPanel")
            .track_focus(&self.focus_handle)
            .size_full()
            .bg(cx.theme().colors().panel_background)
            .child(self.render_toolbar(cx))
            .child(
                div()
                    .id("hierarchy-view-scroll")
                    .debug_selector(|| "hierarchy-view-scroll".to_string())
                    .flex_1()
                    .min_h_0()
                    .child(
                        uniform_list(
                            "hierarchy-view-rows",
                            row_count,
                            cx.processor(move |this, range: Range<usize>, _window, cx| {
                                rows.get(range)
                                    .map(|slice| {
                                        slice
                                            .iter()
                                            .map(|row| this.render_row(row, cx))
                                            .collect::<Vec<_>>()
                                    })
                                    .unwrap_or_default()
                            }),
                        )
                        .size_full()
                        .track_scroll(&self.scroll_handle),
                    ),
            )
    }
}

impl Focusable for HierarchyPanel {
    fn focus_handle(&self, _cx: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl EventEmitter<PanelEvent> for HierarchyPanel {}

impl Panel for HierarchyPanel {
    /// After the outline panel and the database one, before nothing: a call
    /// tree is opened deliberately, not something a reader wants in front of
    /// them by default.
    fn activation_priority(&self) -> u32 {
        10
    }

    fn persistent_name() -> &'static str {
        "HierarchyPanel"
    }

    fn panel_key() -> &'static str {
        HIERARCHY_PANEL_KEY
    }

    fn position(&self, _window: &Window, _cx: &App) -> DockPosition {
        self.position
    }

    fn position_is_valid(&self, position: DockPosition) -> bool {
        matches!(position, DockPosition::Left | DockPosition::Right)
    }

    fn set_position(
        &mut self,
        position: DockPosition,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.position = position;
        cx.notify();
    }

    fn default_size(&self, _window: &Window, _cx: &App) -> Pixels {
        px(360.)
    }

    fn icon(&self, _window: &Window, _cx: &App) -> Option<ui::IconName> {
        Some(IconName::ListTree)
    }

    fn icon_tooltip(&self, _window: &Window, _cx: &App) -> Option<&'static str> {
        Some("Call & Type Hierarchy")
    }

    fn toggle_action(&self) -> Box<dyn Action> {
        Box::new(ToggleFocus)
    }

    fn set_active(&mut self, active: bool, _window: &mut Window, _cx: &mut Context<Self>) {
        self.active = active;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use fs::FakeFs;
    use futures::StreamExt as _;
    use gpui::{TestAppContext, VisualTestContext, WindowHandle};
    use language::{FakeLspAdapter, rust_lang};
    use serde_json::json;
    use settings::SettingsStore;
    use std::sync::{
        Arc,
        atomic::{AtomicBool, AtomicUsize, Ordering},
    };
    use util::path;
    use workspace::MultiWorkspace;

    fn init_test(cx: &mut TestAppContext) {
        cx.update(|cx| {
            let settings = SettingsStore::test(cx);
            cx.set_global(settings);
            theme_settings::init(theme::LoadThemes::JustBase, cx);
            release_channel::init(semver::Version::new(0, 0, 0), cx);
            editor::init(cx);
            super::init(cx);
        });
    }

    fn call_item(name: &str, uri: lsp::Uri, line: u32) -> lsp::CallHierarchyItem {
        let range = lsp::Range::new(lsp::Position::new(line, 0), lsp::Position::new(line, 10));
        lsp::CallHierarchyItem {
            name: name.to_string(),
            kind: lsp::SymbolKind::FUNCTION,
            tags: None,
            detail: None,
            uri,
            range,
            selection_range: range,
            data: None,
        }
    }

    /// Sets up a workspace with the panel loaded, and registers the fake Rust
    /// language server *before* anything opens a buffer -- a buffer only
    /// picks up a language server that is already registered by the time it
    /// is opened, so this order matters.
    async fn open_workspace_with_panel(
        capable: bool,
        cx: &mut TestAppContext,
    ) -> (
        WindowHandle<MultiWorkspace>,
        Entity<Workspace>,
        Entity<HierarchyPanel>,
        Entity<Project>,
        futures::channel::mpsc::UnboundedReceiver<lsp::FakeLanguageServer>,
    ) {
        let fs = FakeFs::new(cx.executor());
        fs.insert_tree(
            path!("/dir"),
            json!({
                "a.rs": "fn callee() {}\nfn caller() { callee(); }\n",
            }),
        )
        .await;
        let project = Project::test(fs, [path!("/dir").as_ref()], cx).await;

        let language_registry = project.read_with(cx, |project, _| project.languages().clone());
        language_registry.add(rust_lang());
        let fake_language_servers = language_registry.register_fake_lsp(
            "Rust",
            FakeLspAdapter {
                capabilities: lsp::ServerCapabilities {
                    call_hierarchy_provider: capable
                        .then_some(lsp::CallHierarchyServerCapability::Simple(true)),
                    ..Default::default()
                },
                ..Default::default()
            },
        );

        let window =
            cx.add_window(|window, cx| MultiWorkspace::test_new(project.clone(), window, cx));
        let workspace = window
            .read_with(cx, |mw, _| mw.workspace().clone())
            .unwrap();
        let workspace_weak = workspace.downgrade();

        let panel = window
            .update(cx, |_, window, cx| {
                cx.spawn_in(window, async move |_this, cx| {
                    HierarchyPanel::load(workspace_weak, cx.clone()).await
                })
            })
            .unwrap()
            .await
            .expect("failed to load hierarchy panel");

        window
            .update(cx, |multi_workspace, window, cx| {
                multi_workspace.workspace().update(cx, |workspace, cx| {
                    workspace.add_panel(panel.clone(), window, cx);
                });
            })
            .unwrap();

        (window, workspace, panel, project, fake_language_servers)
    }

    async fn open_editor_at_cursor(
        workspace: &Entity<Workspace>,
        project: &Entity<Project>,
        cx: &mut VisualTestContext,
    ) {
        let (buffer, _handle) = project
            .update(cx, |project, cx| {
                project.open_local_buffer_with_lsp(path!("/dir/a.rs"), cx)
            })
            .await
            .unwrap();

        workspace.update_in(cx, |workspace, window, cx| {
            let pane = workspace.active_pane().clone();
            let editor = workspace
                .open_project_item::<Editor>(pane, buffer, true, true, true, true, window, cx);
            editor.update(cx, |editor, cx| {
                editor.change_selections(SelectionEffects::no_scroll(), window, cx, |s| {
                    // Column 3 lands inside "callee" on the first line.
                    s.select_ranges([language::Point::new(0, 3)..language::Point::new(0, 3)]);
                });
            });
        });
    }

    #[gpui::test]
    async fn incoming_calls_shows_two_rows_and_opens_the_clicked_one(cx: &mut TestAppContext) {
        init_test(cx);
        let (window, workspace, panel, project, mut fake_language_servers) =
            open_workspace_with_panel(true, cx).await;
        let cx = &mut VisualTestContext::from_window(window.into(), cx);
        open_editor_at_cursor(&workspace, &project, cx).await;

        let fake_server = fake_language_servers.next().await.unwrap();
        cx.run_until_parked();

        let callee_uri = lsp::Uri::from_file_path(path!("/dir/a.rs")).unwrap();
        let prepare_item = call_item("callee", callee_uri, 0);
        fake_server.set_request_handler::<lsp::request::CallHierarchyPrepare, _, _>({
            let prepare_item = prepare_item;
            move |_, _| {
                let prepare_item = prepare_item.clone();
                async move { Ok(Some(vec![prepare_item])) }
            }
        });

        let caller_uri = lsp::Uri::from_file_path(path!("/dir/a.rs")).unwrap();
        let caller_a = call_item("caller_a", caller_uri.clone(), 1);
        let caller_b = call_item("caller_b", caller_uri, 1);
        fake_server.set_request_handler::<lsp::request::CallHierarchyIncomingCalls, _, _>({
            let caller_a = caller_a;
            let caller_b = caller_b;
            move |_, _| {
                let caller_a = caller_a.clone();
                let caller_b = caller_b.clone();
                async move {
                    Ok(Some(vec![
                        lsp::CallHierarchyIncomingCall {
                            from: caller_a,
                            from_ranges: vec![],
                        },
                        lsp::CallHierarchyIncomingCall {
                            from: caller_b,
                            from_ranges: vec![],
                        },
                    ]))
                }
            }
        });

        // Asked only once the server can answer: a handler registered after
        // the request has gone out leaves it unanswered, and the panel then
        // has nothing to show.
        workspace.update_in(cx, |workspace, window, cx| {
            HierarchyPanel::start(workspace, Direction::IncomingCalls, window, cx);
        });
        cx.run_until_parked();

        cx.run_until_parked();

        panel.read_with(cx, |panel, _| {
            let Content::Ready { outcome, .. } = &panel.content else {
                panic!("expected the panel to be ready");
            };
            let HierarchyOutcome::Found(nodes) = outcome else {
                panic!("expected the root to be found");
            };
            assert_eq!(nodes.len(), 1);
            let root = &nodes[0];
            assert_eq!(root.row.name.as_ref(), "callee");
        });

        // Expand the (only) root to see its incoming calls.
        panel.update(cx, |panel, cx| {
            panel.toggle_expand(vec![0], cx);
        });
        cx.run_until_parked();

        panel.read_with(cx, |panel, _| {
            let Content::Ready { outcome, .. } = &panel.content else {
                panic!("expected the panel to be ready");
            };
            let HierarchyOutcome::Found(nodes) = outcome else {
                panic!("expected the root to be found");
            };
            let Expansion::Loaded(HierarchyOutcome::Found(children)) = &nodes[0].expansion else {
                panic!("expected the root's children to be loaded");
            };
            assert_eq!(children.len(), 2);
            assert_eq!(children[0].row.name.as_ref(), "caller_a");
            assert_eq!(children[1].row.name.as_ref(), "caller_b");
        });

        // Clicking a row opens the file the item is in, at the item's own
        // selection range.
        let clicked_row = panel.read_with(cx, |panel, _| {
            let Content::Ready { outcome, .. } = &panel.content else {
                unreachable!()
            };
            let HierarchyOutcome::Found(nodes) = outcome else {
                unreachable!()
            };
            let Expansion::Loaded(HierarchyOutcome::Found(children)) = &nodes[0].expansion else {
                unreachable!()
            };
            children[0].row.clone()
        });
        workspace.update_in(cx, |_, window, cx| {
            panel.update(cx, |panel, cx| panel.open_row(&clicked_row, window, cx));
        });
        cx.run_until_parked();

        let editor = workspace.read_with(cx, |workspace, cx| {
            workspace
                .active_item_as::<Editor>(cx)
                .expect("no active editor")
        });
        let opened_text = editor.read_with(cx, |editor, cx| editor.text(cx));
        let cursor_row = editor.update(cx, |editor, cx| {
            let snapshot = editor.display_snapshot(cx);
            editor
                .selections
                .newest::<language::Point>(&snapshot)
                .head()
                .row
        });
        // Both rows live in the same file in this test's fixture, so the
        // meaningful assertions are that the opened item is genuinely the
        // clicked one, and that the cursor actually lands on caller_a's own
        // line (row 1) rather than merely some editor being active.
        assert_eq!(clicked_row.name.as_ref(), "caller_a");
        assert_eq!(
            cursor_row, 1,
            "clicking should move the cursor to caller_a's own line"
        );
        assert!(opened_text.contains("fn caller()"));
    }

    #[gpui::test]
    async fn unsupported_server_shows_the_unsupported_message_and_sends_no_request(
        cx: &mut TestAppContext,
    ) {
        init_test(cx);
        let (window, workspace, panel, project, mut fake_language_servers) =
            open_workspace_with_panel(false, cx).await;
        let cx = &mut VisualTestContext::from_window(window.into(), cx);
        open_editor_at_cursor(&workspace, &project, cx).await;

        let fake_server = fake_language_servers.next().await.unwrap();
        cx.run_until_parked();

        let request_received = Arc::new(AtomicBool::new(false));
        fake_server.set_request_handler::<lsp::request::CallHierarchyPrepare, _, _>({
            let request_received = request_received.clone();
            move |_, _| {
                request_received.store(true, Ordering::SeqCst);
                async { Ok(None) }
            }
        });

        // Asked only once the server can answer: a handler registered after
        // the request has gone out leaves it unanswered, and the panel then
        // has nothing to show.
        workspace.update_in(cx, |workspace, window, cx| {
            HierarchyPanel::start(workspace, Direction::IncomingCalls, window, cx);
        });
        cx.run_until_parked();

        cx.run_until_parked();

        let message = panel.read_with(cx, |panel, _| {
            let Content::Ready { outcome, .. } = &panel.content else {
                panic!("expected the panel to be ready");
            };
            let HierarchyOutcome::Unsupported = outcome else {
                panic!("expected the outcome to be Unsupported");
            };
            flatten_content(&panel.content)
        });
        let FlatRow::Status { text, .. } = &message[0] else {
            panic!("expected a status row");
        };
        assert_eq!(text, &Direction::IncomingCalls.unsupported_message());
        assert!(
            !request_received.load(Ordering::SeqCst),
            "prepareCallHierarchy must not be sent when the server does not advertise the capability"
        );
    }

    #[gpui::test]
    async fn empty_result_shows_a_different_message_than_unsupported(cx: &mut TestAppContext) {
        init_test(cx);
        let (window, workspace, panel, project, mut fake_language_servers) =
            open_workspace_with_panel(true, cx).await;
        let cx = &mut VisualTestContext::from_window(window.into(), cx);
        open_editor_at_cursor(&workspace, &project, cx).await;

        let fake_server = fake_language_servers.next().await.unwrap();
        cx.run_until_parked();

        fake_server.set_request_handler::<lsp::request::CallHierarchyPrepare, _, _>(|_, _| async {
            Ok(None)
        });

        // Asked only once the server can answer: a handler registered after
        // the request has gone out leaves it unanswered, and the panel then
        // has nothing to show.
        workspace.update_in(cx, |workspace, window, cx| {
            HierarchyPanel::start(workspace, Direction::IncomingCalls, window, cx);
        });
        cx.run_until_parked();

        cx.run_until_parked();

        let message = panel.read_with(cx, |panel, _| {
            let Content::Ready { outcome, .. } = &panel.content else {
                panic!("expected the panel to be ready");
            };
            let HierarchyOutcome::NoResults = outcome else {
                panic!("expected the outcome to be NoResults");
            };
            flatten_content(&panel.content)
        });
        let FlatRow::Status { text, .. } = &message[0] else {
            panic!("expected a status row");
        };
        assert_eq!(text, &Direction::IncomingCalls.empty_message());
        assert_ne!(
            text,
            &Direction::IncomingCalls.unsupported_message(),
            "the empty-result message must read differently from the unsupported message"
        );
    }

    #[gpui::test]
    async fn expanding_a_row_requests_only_that_row_children(cx: &mut TestAppContext) {
        init_test(cx);
        let (window, workspace, panel, project, mut fake_language_servers) =
            open_workspace_with_panel(true, cx).await;
        let cx = &mut VisualTestContext::from_window(window.into(), cx);
        open_editor_at_cursor(&workspace, &project, cx).await;

        let fake_server = fake_language_servers.next().await.unwrap();
        cx.run_until_parked();

        let uri = lsp::Uri::from_file_path(path!("/dir/a.rs")).unwrap();
        let root_a = call_item("root_a", uri.clone(), 0);
        let root_b = call_item("root_b", uri.clone(), 1);
        fake_server.set_request_handler::<lsp::request::CallHierarchyPrepare, _, _>({
            let root_a = root_a;
            let root_b = root_b;
            move |_, _| {
                let root_a = root_a.clone();
                let root_b = root_b.clone();
                async move { Ok(Some(vec![root_a, root_b])) }
            }
        });
        cx.run_until_parked();

        let requested_names = Arc::new(std::sync::Mutex::new(Vec::<String>::new()));
        let call_count = Arc::new(AtomicUsize::new(0));
        fake_server.set_request_handler::<lsp::request::CallHierarchyIncomingCalls, _, _>({
            let requested_names = requested_names.clone();
            let call_count = call_count.clone();
            move |params, _| {
                requested_names.lock().unwrap().push(params.item.name);
                call_count.fetch_add(1, Ordering::SeqCst);
                let callee = call_item("someone_who_calls_it", uri.clone(), 0);
                async move {
                    Ok(Some(vec![lsp::CallHierarchyIncomingCall {
                        from: callee,
                        from_ranges: vec![],
                    }]))
                }
            }
        });

        // Asked only once the server can answer: a handler registered after
        // the request has gone out leaves it unanswered, and the panel then
        // has nothing to show.
        workspace.update_in(cx, |workspace, window, cx| {
            HierarchyPanel::start(workspace, Direction::IncomingCalls, window, cx);
        });
        cx.run_until_parked();

        // Expand only the second root; the first root's siblings must not be
        // asked for.
        panel.update(cx, |panel, cx| {
            panel.toggle_expand(vec![1], cx);
        });
        cx.run_until_parked();

        assert_eq!(call_count.load(Ordering::SeqCst), 1);
        assert_eq!(requested_names.lock().unwrap().as_slice(), ["root_b"]);

        panel.read_with(cx, |panel, _| {
            let Content::Ready { outcome, .. } = &panel.content else {
                panic!("expected the panel to be ready");
            };
            let HierarchyOutcome::Found(nodes) = outcome else {
                panic!("expected the root to be found");
            };
            assert!(matches!(nodes[0].expansion, Expansion::Collapsed));
            let Expansion::Loaded(HierarchyOutcome::Found(children)) = &nodes[1].expansion else {
                panic!("expected root_b's children to be loaded");
            };
            assert_eq!(children.len(), 1);
            assert_eq!(children[0].row.name.as_ref(), "someone_who_calls_it");
        });
    }

    #[gpui::test]
    async fn flipping_direction_re_roots_instead_of_appending(cx: &mut TestAppContext) {
        init_test(cx);
        let (window, workspace, panel, project, mut fake_language_servers) =
            open_workspace_with_panel(true, cx).await;
        let cx = &mut VisualTestContext::from_window(window.into(), cx);
        open_editor_at_cursor(&workspace, &project, cx).await;

        let fake_server = fake_language_servers.next().await.unwrap();
        cx.run_until_parked();

        let uri = lsp::Uri::from_file_path(path!("/dir/a.rs")).unwrap();
        let root = call_item("root", uri.clone(), 0);
        fake_server.set_request_handler::<lsp::request::CallHierarchyPrepare, _, _>({
            let root = root;
            move |_, _| {
                let root = root.clone();
                async move { Ok(Some(vec![root])) }
            }
        });
        cx.run_until_parked();

        let a_caller = call_item("a_caller", uri, 0);
        fake_server.set_request_handler::<lsp::request::CallHierarchyIncomingCalls, _, _>({
            let a_caller = a_caller;
            move |_, _| {
                let a_caller = a_caller.clone();
                async move {
                    Ok(Some(vec![lsp::CallHierarchyIncomingCall {
                        from: a_caller,
                        from_ranges: vec![],
                    }]))
                }
            }
        });

        // Asked only once the server can answer: a handler registered after
        // the request has gone out leaves it unanswered, and the panel then
        // has nothing to show.
        workspace.update_in(cx, |workspace, window, cx| {
            HierarchyPanel::start(workspace, Direction::IncomingCalls, window, cx);
        });
        cx.run_until_parked();
        panel.update(cx, |panel, cx| {
            panel.toggle_expand(vec![0], cx);
        });
        cx.run_until_parked();

        panel.read_with(cx, |panel, _| {
            let Content::Ready { outcome, .. } = &panel.content else {
                unreachable!()
            };
            let HierarchyOutcome::Found(nodes) = outcome else {
                unreachable!()
            };
            assert!(matches!(nodes[0].expansion, Expansion::Loaded(_)));
        });

        panel.update(cx, |panel, cx| {
            panel.flip_direction(cx);
        });
        cx.run_until_parked();

        panel.read_with(cx, |panel, _| {
            let Content::Ready {
                direction, outcome, ..
            } = &panel.content
            else {
                unreachable!()
            };
            assert_eq!(*direction, Direction::OutgoingCalls);
            let HierarchyOutcome::Found(nodes) = outcome else {
                unreachable!()
            };
            assert_eq!(nodes.len(), 1);
            assert_eq!(nodes[0].row.name.as_ref(), "root");
            // Re-rooted, not appended to: the old incoming-call child is gone,
            // and the node is collapsed again rather than holding both the
            // old and a new set of children.
            assert!(matches!(nodes[0].expansion, Expansion::Collapsed));
        });
    }

    const SHOP: &str = "pub fn take_stock() -> u32 {\n    1\n}\n";
    const STORE: &str = "pub fn open_up() {\n    take_stock();\n}\n";

    struct RealProject {
        project_at: tempfile::TempDir,
        _index_at: tempfile::TempDir,
        window: WindowHandle<MultiWorkspace>,
        workspace: Entity<Workspace>,
        panel: Entity<HierarchyPanel>,
        project: Entity<Project>,
        servers: Option<futures::channel::mpsc::UnboundedReceiver<lsp::FakeLanguageServer>>,
    }

    /// A workspace with the panel loaded over a project that exists twice at
    /// one path: the editor's own in-memory filesystem, which every test here
    /// uses, and a real directory, because the index walks the disk with the
    /// standard library rather than through the editor. Its index is built
    /// over the real one, so the panel can be asked a question no server is
    /// there to answer.
    ///
    /// `capable` says what the file's language server offers. `None` registers
    /// no language server at all -- which is what this fork ships with, and the
    /// case the index exists for.
    async fn open_workspace_over_a_real_project(
        capable: Option<bool>,
        cx: &mut TestAppContext,
    ) -> RealProject {
        let project_at = tempfile::tempdir().expect("a directory to put a project in");
        let mut tree = serde_json::Map::new();
        for (name, contents) in [("shop.rs", SHOP), ("store.rs", STORE)] {
            std::fs::write(project_at.path().join(name), contents).expect("a project file on disk");
            tree.insert(
                name.to_string(),
                serde_json::Value::String(contents.to_string()),
            );
        }
        let fs = FakeFs::new(cx.executor());
        fs.insert_tree(project_at.path(), serde_json::Value::Object(tree))
            .await;
        let project = Project::test(fs, [project_at.path()], cx).await;

        let servers = capable.map(|capable| {
            let language_registry = project.read_with(cx, |project, _| project.languages().clone());
            language_registry.add(rust_lang());
            language_registry.register_fake_lsp(
                "Rust",
                FakeLspAdapter {
                    capabilities: lsp::ServerCapabilities {
                        call_hierarchy_provider: capable
                            .then_some(lsp::CallHierarchyServerCapability::Simple(true)),
                        ..Default::default()
                    },
                    ..Default::default()
                },
            )
        });

        let index_at = tempfile::tempdir().expect("a directory for the index's own files");
        cx.update(|cx| {
            symbol_index::ensure_index_at(
                project.clone(),
                index_at.path().join("symbol_index"),
                cx,
            );
        });

        let window =
            cx.add_window(|window, cx| MultiWorkspace::test_new(project.clone(), window, cx));
        let workspace = window
            .read_with(cx, |mw, _| mw.workspace().clone())
            .unwrap();
        let workspace_weak = workspace.downgrade();
        let panel = window
            .update(cx, |_, window, cx| {
                cx.spawn_in(window, async move |_this, cx| {
                    HierarchyPanel::load(workspace_weak, cx.clone()).await
                })
            })
            .unwrap()
            .await
            .expect("failed to load hierarchy panel");
        window
            .update(cx, |multi_workspace, window, cx| {
                multi_workspace.workspace().update(cx, |workspace, cx| {
                    workspace.add_panel(panel.clone(), window, cx);
                });
            })
            .unwrap();

        // The index builds in the background, and a question asked before the
        // build has finished is asked of an empty one.
        cx.run_until_parked();

        RealProject {
            project_at,
            _index_at: index_at,
            window,
            workspace,
            panel,
            project,
            servers,
        }
    }

    /// Opens the declaring file and puts the cursor inside `take_stock`'s own
    /// name, which is where a reader would ask a hierarchy question from.
    async fn open_the_declaration_at_the_cursor(
        workspace: &Entity<Workspace>,
        project: &Entity<Project>,
        at: &std::path::Path,
        cx: &mut VisualTestContext,
    ) {
        let (buffer, _handle) = project
            .update(cx, |project, cx| {
                project.open_local_buffer_with_lsp(at.join("shop.rs"), cx)
            })
            .await
            .unwrap();
        workspace.update_in(cx, |workspace, window, cx| {
            let pane = workspace.active_pane().clone();
            let editor = workspace
                .open_project_item::<Editor>(pane, buffer, true, true, true, true, window, cx);
            editor.update(cx, |editor, cx| {
                editor.change_selections(SelectionEffects::no_scroll(), window, cx, |s| {
                    // Column 9 lands inside "take_stock" on the first line.
                    s.select_ranges([language::Point::new(0, 9)..language::Point::new(0, 9)]);
                });
            });
        });
    }

    fn root_of(panel: &HierarchyPanel) -> &Node {
        let Content::Ready { outcome, .. } = &panel.content else {
            panic!("expected the panel to be ready");
        };
        let HierarchyOutcome::Found(nodes) = outcome else {
            panic!("expected the root to be found");
        };
        assert_eq!(nodes.len(), 1, "one declaration under the cursor");
        &nodes[0]
    }

    #[gpui::test]
    async fn the_index_answers_incoming_calls_where_no_server_does(cx: &mut TestAppContext) {
        init_test(cx);
        let held = open_workspace_over_a_real_project(None, cx).await;
        let (workspace, panel, project) = (
            held.workspace.clone(),
            held.panel.clone(),
            held.project.clone(),
        );
        let root_path = held.project_at.path().to_path_buf();
        let cx = &mut VisualTestContext::from_window(held.window.into(), cx);
        open_the_declaration_at_the_cursor(&workspace, &project, &root_path, cx).await;

        workspace.update_in(cx, |workspace, window, cx| {
            HierarchyPanel::start(workspace, Direction::IncomingCalls, window, cx);
        });
        cx.run_until_parked();

        panel.read_with(cx, |panel, _| {
            let root = root_of(panel);
            assert_eq!(root.row.name.as_ref(), "take_stock");
            assert!(
                matches!(root.row.source, RowSource::Indexed(Some(_))),
                "with no server, the root comes from the index"
            );
        });

        panel.update(cx, |panel, cx| {
            panel.toggle_expand(vec![0], cx);
        });
        cx.run_until_parked();

        panel.read_with(cx, |panel, _| {
            let Expansion::Loaded(HierarchyOutcome::Found(children)) = &root_of(panel).expansion
            else {
                panic!("expected the root's incoming calls to be loaded");
            };
            let named: Vec<&str> = children
                .iter()
                .map(|child| child.row.name.as_ref())
                .collect();
            assert_eq!(
                named,
                vec!["open_up"],
                "the calling function is named, not the line it calls from"
            );
        });
    }

    #[gpui::test]
    async fn a_server_that_answers_leaves_the_index_unasked(cx: &mut TestAppContext) {
        init_test(cx);
        let mut held = open_workspace_over_a_real_project(Some(true), cx).await;
        let (workspace, panel, project) = (
            held.workspace.clone(),
            held.panel.clone(),
            held.project.clone(),
        );
        let root_path = held.project_at.path().to_path_buf();
        let mut servers = held
            .servers
            .take()
            .expect("a language server was asked for");
        let cx = &mut VisualTestContext::from_window(held.window.into(), cx);
        open_the_declaration_at_the_cursor(&workspace, &project, &root_path, cx).await;

        let fake_server = servers.next().await.unwrap();
        cx.run_until_parked();

        let uri = lsp::Uri::from_file_path(root_path.join("shop.rs")).unwrap();
        let prepared = call_item("what_the_server_says", uri, 0);
        fake_server.set_request_handler::<lsp::request::CallHierarchyPrepare, _, _>({
            move |_, _| {
                let prepared = prepared.clone();
                async move { Ok(Some(vec![prepared])) }
            }
        });

        workspace.update_in(cx, |workspace, window, cx| {
            HierarchyPanel::start(workspace, Direction::IncomingCalls, window, cx);
        });
        cx.run_until_parked();

        panel.read_with(cx, |panel, _| {
            let root = root_of(panel);
            assert_eq!(
                root.row.name.as_ref(),
                "what_the_server_says",
                "the server's own answer, unchanged"
            );
            assert!(
                matches!(root.row.source, RowSource::Call(_)),
                "the index stays silent while the server answers"
            );
        });
    }

    /// The type directions reach the compiler's own front end only for the two
    /// languages it is a front end for. In a Rust file with a server that does
    /// not offer a type hierarchy, nothing else answers and the panel says
    /// exactly what it said before any of this existed.
    #[gpui::test]
    async fn a_language_that_is_not_c_or_cpp_gets_no_type_hierarchy(cx: &mut TestAppContext) {
        init_test(cx);
        let (window, workspace, panel, project, mut fake_language_servers) =
            open_workspace_with_panel(false, cx).await;
        let cx = &mut VisualTestContext::from_window(window.into(), cx);
        open_editor_at_cursor(&workspace, &project, cx).await;

        let _fake_server = fake_language_servers.next().await.unwrap();
        cx.run_until_parked();

        for direction in [Direction::Supertypes, Direction::Subtypes] {
            workspace.update_in(cx, |workspace, window, cx| {
                HierarchyPanel::start(workspace, direction, window, cx);
            });
            cx.run_until_parked();

            let rows = panel.read_with(cx, |panel, _| {
                let Content::Ready {
                    outcome,
                    clang_origin,
                    ..
                } = &panel.content
                else {
                    panic!("expected the panel to be ready");
                };
                let HierarchyOutcome::Unsupported = outcome else {
                    panic!("expected {direction:?} to be Unsupported for a Rust buffer");
                };
                assert!(
                    clang_origin.is_none(),
                    "the front end must not be asked about a language it is not a front end for",
                );
                flatten_content(&panel.content)
            });
            let FlatRow::Status { text, .. } = &rows[0] else {
                panic!("expected a status row");
            };
            assert_eq!(text, &direction.unsupported_message());
        }
    }

    /// A subtype list the front end answered says what it looked at, because
    /// what it looked at is one translation unit and not the project. The note
    /// belongs to that direction alone: a base clause names every base, so
    /// supertypes need no caveat.
    #[gpui::test]
    async fn a_subtype_list_from_the_front_end_says_what_it_looked_at(cx: &mut TestAppContext) {
        init_test(cx);
        let buffer = cx.new(|cx| language::Buffer::local("class Thing {};\n", cx));

        let note_shown = |direction: Direction, clang_origin: Option<Entity<language::Buffer>>| {
            let content = Content::Ready {
                direction,
                outcome: HierarchyOutcome::NoResults,
                clang_origin,
            };
            flatten_content(&content).into_iter().any(|row| {
                matches!(row, FlatRow::Status { text, .. }
                    if text.as_ref() == WHAT_THE_FRONT_END_LOOKED_AT_FOR_SUBTYPES)
            })
        };

        assert!(
            note_shown(Direction::Subtypes, Some(buffer.clone())),
            "a front-end subtype list must admit what it looked at",
        );
        assert!(
            !note_shown(Direction::Supertypes, Some(buffer)),
            "a base clause names every base, so supertypes need no caveat",
        );
        assert!(
            !note_shown(Direction::Subtypes, None),
            "a language server's own subtype list is not qualified by this note",
        );
    }

    /// An empty list of children is `NoResults` and never `Found(vec![])`. The
    /// first is a sentence the reader can see; the second draws an expanded row
    /// with nothing under it, which reads as a lookup that broke.
    #[test]
    fn no_children_is_nothing_and_not_an_empty_list() {
        assert!(matches!(
            found_or_nothing(Vec::new()),
            HierarchyOutcome::NoResults
        ));
    }

    /// The front end is asked about types and never about calls, which have a
    /// source of their own.
    #[test]
    fn only_the_type_directions_reach_the_front_end() {
        assert_eq!(related(Direction::Supertypes), Some(Related::Bases));
        assert_eq!(related(Direction::Subtypes), Some(Related::Derived));
        assert_eq!(related(Direction::IncomingCalls), None);
        assert_eq!(related(Direction::OutgoingCalls), None);
    }
}
