use anyhow::Result;
use editor::{Editor, SelectionEffects, scroll::Autoscroll};
use gpui::{
    Action, AnyElement, App, AppContext as _, AsyncWindowContext, Context, ElementId, Entity,
    EventEmitter, FocusHandle, Focusable, InteractiveElement, IntoElement, ParentElement, Pixels,
    Render, SharedString, Styled, Subscription, UniformListScrollHandle, WeakEntity, Window,
    actions, div, px, uniform_list,
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

/// One row's worth of display data, converted from whichever of the two
/// protocol-layer item types it came from. Kept separate from `RowSource` so
/// the tree and rendering code never have to match on call-vs-type to read a
/// name, a kind or a location.
#[derive(Clone)]
struct HierarchyRow {
    name: SharedString,
    kind: SymbolKind,
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
}

impl From<&CallHierarchyItem> for HierarchyRow {
    fn from(item: &CallHierarchyItem) -> Self {
        Self {
            name: item.name.clone(),
            kind: item.kind,
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
            kind: item.kind,
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
        cx.spawn(async move |this, cx| {
            let result = if direction.is_call() {
                prepare_call_hierarchy(&lsp_store, &buffer, position, cx)
                    .await
                    .map(root_outcome_from_call)
            } else {
                prepare_type_hierarchy(&lsp_store, &buffer, position, cx)
                    .await
                    .map(root_outcome_from_type)
            };
            this.update(cx, |this, cx| {
                this.content = match result {
                    Ok(outcome) => Content::Ready { direction, outcome },
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
        let Content::Ready { direction, outcome } = &mut self.content else {
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
        let Content::Ready { direction, outcome } = &mut self.content else {
            return;
        };
        let direction = *direction;
        let Some(node) = node_at_mut(outcome, &path) else {
            return;
        };
        let source = node.row.source.clone();
        node.expansion = Expansion::Loading;
        cx.notify();

        let lsp_store = self.project.read(cx).lsp_store();
        cx.spawn(async move |this, cx| {
            let result = fetch_children(&lsp_store, direction, &source, cx).await;
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
                                Label::new(kind_label(row.kind))
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
        Content::Ready { direction, outcome } => {
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
    direction: Direction,
    source: &RowSource,
    cx: &mut gpui::AsyncApp,
) -> Result<HierarchyOutcome<Node>> {
    match (direction, source) {
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
            let Content::Ready { direction, outcome } = &panel.content else {
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
}
