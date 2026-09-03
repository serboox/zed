use crate::store::ApiClientStore;
use api_client::{CollectionId, EnvironmentId, Variable};
use editor::{Editor, EditorEvent};
use gpui::{
    App, Context, DismissEvent, Entity, EventEmitter, FocusHandle, Focusable, PromptLevel, Render,
    ScrollHandle, Subscription, Window,
};
use ui::{
    Checkbox, IconName, IconSize, Label, LabelSize, ScrollAxes, Scrollbars, ToggleState, Tooltip,
    WithScrollbar, cyberpunk, prelude::*,
};
use workspace::ModalView;

/// What a newly added environment is called until it is named. Selected for
/// editing the moment it appears, so the reader types over it.
const NEW_ENVIRONMENT_NAME: &str = "New environment";

/// How wide the column of environments stands.
const LIST_WIDTH: Pixels = px(200.);

/// Room at the end of every variable row for its three actions under one
/// frame, and the same room reserved by the heading above them. Fixed rather
/// than left to the contents: the three value columns can only line up with
/// the words naming them if what sits either side of them never changes width.
const ROW_ACTIONS_WIDTH: Pixels = px(84.);

/// One action inside that frame.
const ROW_ACTION_WIDTH: Pixels = px(26.);

/// How tall a value box stands at least.
const ROW_CELL_HEIGHT: Pixels = px(30.);

/// How many variables a scope holds, for the line under its name in the list.
fn how_many_variables(count: usize) -> String {
    match count {
        0 => "no variables".to_string(),
        1 => "1 variable".to_string(),
        _ => format!("{count} variables"),
    }
}

/// Which variable list is being edited. `Global` and `Environment` share the
/// sidebar list (opened via "Manage Environments..."); `Collection` is
/// opened directly on one collection's "Edit Variables..." context-menu
/// entry and has no sidebar -- its scope never changes for the lifetime of
/// the modal.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Scope {
    Global,
    Environment(EnvironmentId),
    Collection(CollectionId),
}

struct VariableRow {
    key_editor: Entity<Editor>,
    initial_value_editor: Entity<Editor>,
    current_value_editor: Entity<Editor>,
    enabled: bool,
    secret: bool,
    /// Whether a secret row's values are shown in the clear right now --
    /// starts `false` even for a pre-existing secret variable, matching
    /// `Variable::value_for_display()`'s own default-masked behavior.
    revealed: bool,
}

fn new_single_line_editor(
    placeholder: &'static str,
    initial_value: &str,
    window: &mut Window,
    cx: &mut App,
) -> Entity<Editor> {
    cx.new(|cx| {
        let mut editor = Editor::single_line(window, cx);
        editor.set_placeholder_text(placeholder, window, cx);
        if !initial_value.is_empty() {
            editor.set_text(initial_value, window, cx);
        }
        editor
    })
}

/// The "Manage Environments" / "Edit Variables" modal. One type covers both
/// entry points: with `show_scope_list` it presents Global plus every
/// `Environment` in a sidebar (create/rename/duplicate/delete); without it,
/// it is pinned to a single `Collection`'s variables with no sidebar at all.
pub struct EnvironmentEditorModal {
    focus_handle: FocusHandle,
    store: Entity<ApiClientStore>,
    show_scope_list: bool,
    scope: Scope,
    /// Editable name for the selected `Environment` (disabled for `Global`
    /// and unused for `Collection` scope, which shows a static title
    /// instead since a collection's name is edited from the panel tree).
    name_editor: Entity<Editor>,
    rows: Vec<VariableRow>,
    rows_scroll_handle: ScrollHandle,
    list_scroll_handle: ScrollHandle,
    /// The row editors' subscriptions, thrown away and rebuilt whenever the
    /// scope changes, because the rows themselves are.
    _subscriptions: Vec<Subscription>,
    /// Watching the name field, kept apart from the rows on purpose. It belongs
    /// to the dialog rather than to whichever environment is being looked at,
    /// and living in the same list as the rows meant the first switch of scope
    /// cleared it -- after which renaming quietly did nothing.
    _watching_the_name: Option<Subscription>,
}

impl EnvironmentEditorModal {
    /// Opens on the currently active environment (or Global, if none is
    /// active) with the full Global + environment-list sidebar.
    pub fn new_for_environments(
        store: Entity<ApiClientStore>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let scope = store
            .read(cx)
            .active_environment()
            .map(|environment| Scope::Environment(environment.id))
            .unwrap_or(Scope::Global);
        Self::new_impl(store, true, scope, window, cx)
    }

    /// Opens pinned to one collection's variables, no sidebar.
    pub fn new_for_collection(
        store: Entity<ApiClientStore>,
        collection_id: CollectionId,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        Self::new_impl(store, false, Scope::Collection(collection_id), window, cx)
    }

    fn new_impl(
        store: Entity<ApiClientStore>,
        show_scope_list: bool,
        scope: Scope,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let name_editor = new_single_line_editor("Environment name", "", window, cx);

        let mut this = Self {
            focus_handle: cx.focus_handle(),
            store,
            show_scope_list,
            scope,
            name_editor,
            rows: Vec::new(),
            rows_scroll_handle: ScrollHandle::new(),
            list_scroll_handle: ScrollHandle::new(),
            _subscriptions: Vec::new(),
            _watching_the_name: None,
        };
        this.rebuild_for_scope(window, cx);
        this.watch_name_editor(window, cx);
        this
    }

    fn scope_name(&self, cx: &App) -> String {
        let store = self.store.read(cx);
        match self.scope {
            Scope::Global => "Global".to_string(),
            Scope::Environment(id) => store
                .environments
                .iter()
                .find(|environment| environment.id == id)
                .map(|environment| environment.name.clone())
                .unwrap_or_default(),
            Scope::Collection(id) => store
                .collections
                .iter()
                .find(|collection| collection.id == id)
                .map(|collection| collection.name.clone())
                .unwrap_or_default(),
        }
    }

    fn scope_variables(&self, cx: &App) -> Vec<Variable> {
        let store = self.store.read(cx);
        match self.scope {
            Scope::Global => store.global_environment.variables.clone(),
            Scope::Environment(id) => store
                .environments
                .iter()
                .find(|environment| environment.id == id)
                .map(|environment| environment.variables.clone())
                .unwrap_or_default(),
            Scope::Collection(id) => store
                .collections
                .iter()
                .find(|collection| collection.id == id)
                .map(|collection| collection.variables.clone())
                .unwrap_or_default(),
        }
    }

    fn write_scope_variables(&self, variables: Vec<Variable>, cx: &mut Context<Self>) {
        let scope = self.scope;
        self.store.update(cx, |store, cx| match scope {
            Scope::Global => store.update_environment(None, cx, |environment| {
                environment.variables = variables;
            }),
            Scope::Environment(id) => store.update_environment(Some(id), cx, |environment| {
                environment.variables = variables;
            }),
            Scope::Collection(id) => store.update_collection(id, cx, |collection| {
                collection.variables = variables;
            }),
        });
    }

    /// Rebuilds `rows` (and the name field) from the store for the current
    /// `scope`. Called on open and whenever the sidebar selection changes --
    /// the modal owns editable copies of the variable rows while it is open,
    /// the same way `RequestView` owns editable copies of a request's
    /// fields, and persists on every edit rather than re-syncing live.
    fn rebuild_for_scope(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let name = self.scope_name(cx);
        self.name_editor.update(cx, |editor, cx| {
            editor.set_text(name, window, cx);
            editor.set_read_only(matches!(self.scope, Scope::Global));
        });

        let variables = self.scope_variables(cx);
        self.rows.clear();
        self._subscriptions.clear();
        for variable in variables {
            self.push_row(variable, window, cx);
        }
    }

    fn push_row(&mut self, variable: Variable, window: &mut Window, cx: &mut Context<Self>) {
        let key_editor = new_single_line_editor("key", &variable.key, window, cx);
        let initial_value_editor =
            new_single_line_editor("initial value", &variable.initial_value, window, cx);
        let current_value_editor =
            new_single_line_editor("current value", &variable.current_value, window, cx);
        initial_value_editor.update(cx, |editor, cx| editor.set_masked(variable.secret, cx));
        current_value_editor.update(cx, |editor, cx| editor.set_masked(variable.secret, cx));

        for editor in [&key_editor, &initial_value_editor, &current_value_editor] {
            let subscription = cx.subscribe(editor, |this, _, event: &EditorEvent, cx| {
                if matches!(event, EditorEvent::BufferEdited) {
                    this.persist_rows(cx);
                }
            });
            self._subscriptions.push(subscription);
        }

        self.rows.push(VariableRow {
            key_editor,
            initial_value_editor,
            current_value_editor,
            enabled: variable.enabled,
            secret: variable.secret,
            revealed: false,
        });
    }

    fn watch_name_editor(&mut self, _window: &mut Window, cx: &mut Context<Self>) {
        let subscription = cx.subscribe(
            &self.name_editor,
            |this, editor, event: &EditorEvent, cx| {
                if matches!(event, EditorEvent::BufferEdited)
                    && !matches!(this.scope, Scope::Global)
                {
                    let name = editor.read(cx).text(cx);
                    if name.trim().is_empty() {
                        return;
                    }
                    match this.scope {
                        Scope::Environment(id) => {
                            this.store.update(cx, |store, cx| {
                                store.update_environment(Some(id), cx, |environment| {
                                    environment.name = name;
                                });
                            });
                        }
                        Scope::Global | Scope::Collection(_) => {}
                    }
                }
            },
        );
        self._watching_the_name = Some(subscription);
    }

    fn row_variable(row: &VariableRow, cx: &App) -> Variable {
        Variable {
            key: row.key_editor.read(cx).text(cx),
            initial_value: row.initial_value_editor.read(cx).text(cx),
            current_value: row.current_value_editor.read(cx).text(cx),
            secret: row.secret,
            enabled: row.enabled,
        }
    }

    fn persist_rows(&self, cx: &mut Context<Self>) {
        let variables: Vec<Variable> = self
            .rows
            .iter()
            .map(|row| Self::row_variable(row, cx))
            .collect();
        self.write_scope_variables(variables, cx);
    }

    fn add_row(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.push_row(Variable::new(String::new(), String::new()), window, cx);
        self.persist_rows(cx);
        cx.notify();
    }

    fn remove_row(&mut self, index: usize, cx: &mut Context<Self>) {
        if index < self.rows.len() {
            self.rows.remove(index);
            self.persist_rows(cx);
            cx.notify();
        }
    }

    fn toggle_row_enabled(&mut self, index: usize, cx: &mut Context<Self>) {
        if let Some(row) = self.rows.get_mut(index) {
            row.enabled = !row.enabled;
            self.persist_rows(cx);
            cx.notify();
        }
    }

    fn toggle_row_secret(&mut self, index: usize, cx: &mut Context<Self>) {
        if let Some(row) = self.rows.get_mut(index) {
            row.secret = !row.secret;
            let masked = row.secret && !row.revealed;
            row.initial_value_editor
                .update(cx, |editor, cx| editor.set_masked(masked, cx));
            row.current_value_editor
                .update(cx, |editor, cx| editor.set_masked(masked, cx));
            self.persist_rows(cx);
            cx.notify();
        }
    }

    fn toggle_row_revealed(&mut self, index: usize, cx: &mut Context<Self>) {
        if let Some(row) = self.rows.get_mut(index) {
            row.revealed = !row.revealed;
            let masked = row.secret && !row.revealed;
            row.initial_value_editor
                .update(cx, |editor, cx| editor.set_masked(masked, cx));
            row.current_value_editor
                .update(cx, |editor, cx| editor.set_masked(masked, cx));
            cx.notify();
        }
    }

    fn select_scope(&mut self, scope: Scope, window: &mut Window, cx: &mut Context<Self>) {
        if self.scope == scope {
            return;
        }
        self.scope = scope;
        self.rebuild_for_scope(window, cx);
        cx.notify();
    }

    /// Adds one and moves to it, so the name field on the right is where it gets
    /// its name. Naming it before it exists, in a field of its own under the
    /// list, meant two places to type a name in one window.
    fn create_environment(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let id = self.store.update(cx, |store, cx| {
            store.create_environment(NEW_ENVIRONMENT_NAME.to_string(), cx)
        });
        self.select_scope(Scope::Environment(id), window, cx);
        self.name_editor.update(cx, |editor, cx| {
            editor.select_all(&Default::default(), window, cx)
        });
        window.focus(&self.name_editor.focus_handle(cx), cx);
    }

    /// Removes whichever environment is being looked at. Global is not one of
    /// them and cannot go.
    fn delete_chosen_environment(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Scope::Environment(id) = self.scope else {
            return;
        };
        self.delete_environment(id, window, cx);
    }

    fn duplicate_environment(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let source_name = self.scope_name(cx);
        let variables = self.scope_variables(cx);
        let new_name = format!("{source_name} Copy");
        let id = self
            .store
            .update(cx, |store, cx| store.create_environment(new_name, cx));
        self.store.update(cx, |store, cx| {
            store.update_environment(Some(id), cx, |environment| {
                environment.variables = variables;
            });
        });
        self.select_scope(Scope::Environment(id), window, cx);
    }

    fn delete_environment(
        &mut self,
        id: EnvironmentId,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let name = self
            .store
            .read(cx)
            .environments
            .iter()
            .find(|environment| environment.id == id)
            .map(|environment| environment.name.clone())
            .unwrap_or_default();
        let message =
            format!("Delete the environment \"{name}\" and its variables? This cannot be undone.");
        let answer = window.prompt(
            PromptLevel::Warning,
            &message,
            None,
            &["Cancel", "Delete"],
            cx,
        );
        cx.spawn_in(window, async move |this, cx| {
            // Cancel comes first, so deleting is the second button.
            if answer.await != Ok(1) {
                return;
            }
            this.update_in(cx, |this, window, cx| {
                this.store
                    .update(cx, |store, cx| store.delete_environment(id, cx));
                if this.scope == Scope::Environment(id) {
                    this.select_scope(Scope::Global, window, cx);
                } else {
                    cx.notify();
                }
            })
            .ok();
        })
        .detach();
    }

    fn cancel(&mut self, cx: &mut Context<Self>) {
        cx.emit(DismissEvent);
    }

    /// A small heading over a group of rows in the list on the left, so the
    /// one scope that is always read is not mistaken for one of the ones a
    /// reader made.
    fn group_heading(said: impl Into<SharedString>) -> impl IntoElement {
        h_flex()
            .w_full()
            .px_2()
            .pt_2()
            .pb_1()
            .child(Label::new(said).size(LabelSize::XSmall).color(Color::Muted))
    }

    /// The left half: everything there is to look at, and which of them is
    /// being looked at. The rows carry no controls of their own -- adding,
    /// copying and removing sit on the one frame above the list, so a reader
    /// looks for them in a single place rather than on every row.
    fn render_scope_list(&self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let store = self.store.read(cx);
        let global_variables = store.global_environment.variables.len();
        let in_use = store.active_environment().map(|environment| environment.id);
        let environments: Vec<(EnvironmentId, String, usize, bool)> = store
            .environments
            .iter()
            .map(|environment| {
                (
                    environment.id,
                    environment.name.clone(),
                    environment.variables.len(),
                    in_use == Some(environment.id),
                )
            })
            .collect();
        let how_many = environments.len();

        let mut list = v_flex()
            .id("environment-editor-list")
            .debug_selector(|| "environment-editor-list".to_string())
            // Kept at its own height inside the scrolling above it. A child
            // of a bounded column is squeezed to fit by default, so
            // twenty-five entries quietly compressed themselves into the
            // room available and there was never anything to scroll to.
            .flex_none()
            .gap_0p5()
            .child(Self::group_heading("ALWAYS ON"))
            .child(self.render_scope_entry(
                "Global",
                how_many_variables(global_variables),
                Scope::Global,
                cx,
                |this, window, cx| {
                    this.select_scope(Scope::Global, window, cx);
                },
            ))
            .child(Self::group_heading(match how_many {
                0 => "ENVIRONMENTS".to_string(),
                _ => format!("ENVIRONMENTS · {how_many}"),
            }));
        if environments.is_empty() {
            list = list.child(
                div().px_2().pb_1().child(
                    Label::new("None yet. Press + above to make one.")
                        .size(LabelSize::XSmall)
                        .color(Color::Muted),
                ),
            );
        }
        for (id, name, variables, is_in_use) in environments {
            let beneath = match is_in_use {
                true => format!("{} · in use", how_many_variables(variables)),
                false => how_many_variables(variables),
            };
            list = list.child(self.render_scope_entry(
                name,
                beneath,
                Scope::Environment(id),
                cx,
                move |this, window, cx| {
                    this.select_scope(Scope::Environment(id), window, cx);
                },
            ));
        }

        v_flex()
            .flex_none()
            .w(LIST_WIDTH)
            // A rule between the two halves, the way the run configurations
            // window separates its list from its form.
            .pr_2()
            .border_r_1()
            .border_color(cyberpunk::border_dim())
            // No `h_full` here: it resolves against a parent whose height is
            // *definite*, and the row this sits in takes its height from the
            // layout instead -- so `h_full` fell back to the height of the
            // contents, which is the very thing being bounded. A row stretches
            // its children to its own height anyway.
            // Allowed to be shorter than what is in it. Without this its least
            // height is the height of every environment there is, the row it
            // sits in grows to fit them, and the scrolling below has nothing to
            // scroll inside -- which is how eleven environments pushed the other
            // half of this window off the screen.
            .min_h_0()
            .child(
                div()
                    .id("environment-editor-list-scroll")
                    .debug_selector(|| "environment-editor-list-scroll".to_string())
                    .flex_1()
                    .min_h_0()
                    .overflow_scroll()
                    .track_scroll(&self.list_scroll_handle)
                    .child(list)
                    // Told which handle to follow. Left unsaid, the bars take a
                    // handle of their own, and the one the view holds -- the one
                    // that answers how far there is left to scroll -- is never
                    // updated at all. Two handles for one region also means the
                    // bar and the wheel can disagree about where the reader is.
                    .custom_scrollbars(
                        Scrollbars::always_visible(ScrollAxes::Vertical)
                            .tracked_scroll_handle(&self.list_scroll_handle),
                        window,
                        cx,
                    ),
            )
    }

    fn render_scope_entry(
        &self,
        label: impl Into<SharedString>,
        beneath: String,
        scope: Scope,
        cx: &Context<Self>,
        on_click: impl Fn(&mut Self, &mut Window, &mut Context<Self>) + 'static,
    ) -> impl IntoElement {
        let is_selected = self.scope == scope;
        let label = label.into();
        let name = SharedString::from(format!("environment-editor-entry-{label}"));
        h_flex()
            .id(name.clone())
            .debug_selector(move || name.to_string())
            .w_full()
            .px_2()
            .py_1()
            .gap_2()
            .items_center()
            .rounded(cyberpunk::RADIUS)
            .cursor_pointer()
            .when(is_selected, |row| row.bg(cyberpunk::row_chosen()))
            .when(!is_selected, |row| {
                row.hover(|row| row.bg(cyberpunk::row_hovered()))
            })
            // A stripe drawn on every row and coloured in on the chosen one,
            // rather than a border that only the chosen one carries: a border
            // that comes and goes moves the words beside it each time the
            // choice changes.
            .child(
                div()
                    .flex_none()
                    .w(px(2.))
                    .h(px(26.))
                    .rounded(cyberpunk::RADIUS)
                    .when(is_selected, |stripe| {
                        stripe.bg(cyberpunk::Accent::Cyan.border())
                    }),
            )
            .child(
                v_flex()
                    .flex_1()
                    .min_w_0()
                    .child(
                        Label::new(label)
                            .size(LabelSize::Small)
                            .color(match is_selected {
                                true => Color::Default,
                                false => Color::Muted,
                            }),
                    )
                    .child(
                        Label::new(beneath)
                            .size(LabelSize::XSmall)
                            .color(Color::Muted),
                    ),
            )
            .on_click(cx.listener(move |this, _, window, cx| on_click(this, window, cx)))
    }

    /// The words over the table. Three boxes of text side by side say nothing
    /// about which is which, and the difference between the two values is the
    /// whole point of keeping both.
    fn render_variables_heading(&self) -> impl IntoElement {
        let column = |said: &'static str, selector: &'static str, hint: &'static str| {
            div()
                .id(selector)
                .debug_selector(move || selector.to_string())
                .flex_1()
                .min_w_0()
                .px_2()
                .tooltip(Tooltip::text(hint))
                .child(Label::new(said).size(LabelSize::XSmall).color(Color::Muted))
        };
        h_flex()
            .id("variables-heading")
            .debug_selector(|| "variables-heading".to_string())
            .w_full()
            .flex_none()
            .gap_2()
            .items_center()
            .pb_1()
            .border_b_1()
            .border_color(cyberpunk::border_dim())
            .child(
                div()
                    .flex_none()
                    .w(Checkbox::container_size())
                    .flex()
                    .justify_center()
                    .child(Label::new("ON").size(LabelSize::XSmall).color(Color::Muted)),
            )
            .child(column(
                "KEY",
                "variables-heading-key",
                "The name a request writes as {{key}}",
            ))
            .child(column(
                "INITIAL VALUE",
                "variables-heading-initial",
                "What is written down and shared with everyone",
            ))
            .child(column(
                "CURRENT VALUE",
                "variables-heading-current",
                "What this machine actually sends, which may differ",
            ))
            .child(div().flex_none().w(ROW_ACTIONS_WIDTH))
    }

    /// One variable: whether it is sent, its name, the value that is written
    /// down and the value being used, then the three things that can be done
    /// to the row.
    fn render_row(&self, index: usize, cx: &Context<Self>) -> impl IntoElement {
        let ground = cx.theme().colors().editor_background;
        let row = &self.rows[index];
        let enabled = row.enabled;
        let secret = row.secret;
        let revealed = row.revealed;
        let cell = move |editor: &Entity<Editor>, column: &'static str| {
            div()
                .debug_selector(move || format!("variable-row-{column}-{index}"))
                .flex_1()
                .min_w_0()
                .flex()
                .items_center()
                // A minimum with the line centred in it rather than a fixed
                // height, the same rule every other field in this chrome
                // follows: a fixed box stops fitting the moment the text
                // scale moves.
                .min_h(ROW_CELL_HEIGHT)
                .px_2()
                .py_1()
                .rounded(cyberpunk::RADIUS)
                .border_1()
                .border_color(cyberpunk::border_dim())
                .bg(ground)
                // A variable that is switched off stays readable, but says so.
                .when(!enabled, |cell| cell.opacity(0.5))
                .child(editor.clone())
        };

        let actions = cyberpunk::segmented([
            div()
                .debug_selector(move || format!("variable-row-secret-{index}"))
                .child(
                    IconButton::new(
                        SharedString::from(format!("variable-row-secret-{index}")),
                        match secret {
                            true => IconName::Lock,
                            false => IconName::LockOff,
                        },
                    )
                    .icon_size(IconSize::XSmall)
                    .size(ButtonSize::Compact)
                    .width(ROW_ACTION_WIDTH)
                    // Cyan rather than amber: this chrome has exactly two
                    // accents and amber is not one of them.
                    .icon_color(match secret {
                        true => Color::Accent,
                        false => Color::Muted,
                    })
                    .tooltip(Tooltip::text(match secret {
                        true => "Stop hiding this value",
                        false => "Hide this value",
                    }))
                    .on_click(cx.listener(move |this, _, _, cx| this.toggle_row_secret(index, cx))),
                )
                .into_any_element(),
            div()
                .debug_selector(move || format!("variable-row-reveal-{index}"))
                .child(
                    IconButton::new(
                        SharedString::from(format!("variable-row-reveal-{index}")),
                        match revealed {
                            true => IconName::EyeOff,
                            false => IconName::Eye,
                        },
                    )
                    .icon_size(IconSize::XSmall)
                    .size(ButtonSize::Compact)
                    .width(ROW_ACTION_WIDTH)
                    // Always drawn, and dead unless the value is hidden. An
                    // action that appeared on some rows only made those rows
                    // one button wider, which slid their three value columns
                    // out of line with every other row's.
                    .disabled(!secret)
                    .tooltip(Tooltip::text(match revealed {
                        true => "Hide it again",
                        false => "Show what it says",
                    }))
                    .on_click(
                        cx.listener(move |this, _, _, cx| this.toggle_row_revealed(index, cx)),
                    ),
                )
                .into_any_element(),
            div()
                .debug_selector(move || format!("variable-row-remove-{index}"))
                .child(
                    IconButton::new(
                        SharedString::from(format!("variable-row-remove-{index}")),
                        IconName::Trash,
                    )
                    .icon_size(IconSize::XSmall)
                    .size(ButtonSize::Compact)
                    .width(ROW_ACTION_WIDTH)
                    .tooltip(Tooltip::text("Take this variable out"))
                    .on_click(cx.listener(move |this, _, _, cx| this.remove_row(index, cx))),
                )
                .into_any_element(),
        ]);

        h_flex()
            .id(SharedString::from(format!("variable-row-{index}")))
            .w_full()
            .gap_2()
            .items_center()
            .child(
                div()
                    .flex_none()
                    .w(Checkbox::container_size())
                    .flex()
                    .justify_center()
                    .child(
                        Checkbox::new(
                            SharedString::from(format!("variable-row-enabled-{index}")),
                            match enabled {
                                true => ToggleState::Selected,
                                false => ToggleState::Unselected,
                            },
                        )
                        .tooltip(Tooltip::text("Send this variable with requests"))
                        .on_click(
                            cx.listener(move |this, _, _, cx| this.toggle_row_enabled(index, cx)),
                        ),
                    ),
            )
            .child(cell(&row.key_editor, "key"))
            .child(cell(&row.initial_value_editor, "initial"))
            .child(cell(&row.current_value_editor, "current"))
            .child(
                div()
                    .flex_none()
                    .w(ROW_ACTIONS_WIDTH)
                    .flex()
                    .justify_end()
                    .child(actions),
            )
    }

    /// Nothing to show yet. An empty table saying so and no more leaves the
    /// reader to go looking for the plus, so the one thing there is to do is
    /// offered here instead.
    fn render_no_variables(&self, cx: &Context<Self>) -> impl IntoElement {
        v_flex()
            .id("variable-rows-empty")
            .debug_selector(|| "variable-rows-empty".to_string())
            .w_full()
            .py_6()
            .px_4()
            .gap_2()
            .items_center()
            .child(Label::new("No variables here yet.").size(LabelSize::Small))
            .child(
                Label::new(
                    "A variable holds a value in one place, and every request that names \
                     it picks up whatever it says.",
                )
                .size(LabelSize::XSmall)
                .color(Color::Muted),
            )
            .child(
                div()
                    .debug_selector(|| "variable-row-empty-add".to_string())
                    .child(
                        Button::new("variable-row-empty-add-button", "Add the first variable")
                            .label_size(LabelSize::Small)
                            // The accent is allowed here because nothing else
                            // on this half competes for it while the table is
                            // empty: it is the only thing there is to press.
                            .style(cyberpunk::Rank::Accent.style())
                            .on_click(cx.listener(|this, _, window, cx| this.add_row(window, cx))),
                    ),
            )
    }

    /// What the right half is looking at, said once above the variables. Only
    /// an environment has a name that is changed here; Global has none to
    /// change and a collection is renamed from the panel tree, so both of
    /// those read as a caption rather than as a box that looks typed-in and is
    /// not.
    fn render_scope_header(&self, cx: &Context<Self>) -> AnyElement {
        let caption = |said: &'static str, name: String, beneath: &'static str| {
            v_flex()
                .w_full()
                .gap_1()
                .child(Label::new(said).size(LabelSize::XSmall).color(Color::Muted))
                .child(Label::new(name).size(LabelSize::Small))
                .child(
                    Label::new(beneath)
                        .size(LabelSize::XSmall)
                        .color(Color::Muted),
                )
                .into_any_element()
        };
        match self.scope {
            Scope::Environment(_) => {
                cyberpunk::dialog_field("name", false, cx, self.name_editor.clone())
                    .into_any_element()
            }
            Scope::Global => caption(
                "SCOPE",
                "Global".to_string(),
                "Read by every request, whichever environment is on.",
            ),
            Scope::Collection(_) => caption(
                "COLLECTION",
                self.scope_name(cx),
                "Read by every request in this collection.",
            ),
        }
    }

    fn render_variable_panel(
        &self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let column = v_flex()
            .flex_1()
            .min_w_0()
            .min_h_0()
            .gap_2()
            .child(self.render_scope_header(cx))
            // The plus rides the rule that names the section, rather than
            // sitting alone on a line beneath it as a bare blue word.
            .child(
                cyberpunk::dialog_section("variables").child(
                    div()
                        .debug_selector(|| "variable-row-add".to_string())
                        .child(
                            IconButton::new("variable-row-add", IconName::Plus)
                                .icon_size(IconSize::XSmall)
                                .style(cyberpunk::Rank::Quiet.style())
                                .tooltip(Tooltip::text("Add a variable"))
                                .on_click(
                                    cx.listener(|this, _, window, cx| this.add_row(window, cx)),
                                ),
                        ),
                ),
            );

        let mut rows = v_flex().id("variable-rows").flex_none().w_full().gap_1();
        if self.rows.is_empty() {
            rows = rows.child(self.render_no_variables(cx));
        } else {
            // The heading scrolls with the rows rather than standing above
            // them: the bar reserves room on the right only while there is
            // something to scroll, so a heading kept outside would sit a
            // scrollbar's width wider than the rows it names, and would do so
            // exactly when the list is long enough to need one.
            rows = rows.child(self.render_variables_heading());
            for index in 0..self.rows.len() {
                rows = rows.child(self.render_row(index, cx));
            }
        }

        column.child(
            div()
                .id("environment-editor-rows-scroll")
                .debug_selector(|| "environment-editor-rows-scroll".to_string())
                .flex_1()
                .min_h_0()
                .overflow_scroll()
                .track_scroll(&self.rows_scroll_handle)
                .child(rows)
                .custom_scrollbars(
                    Scrollbars::always_visible(ScrollAxes::Vertical)
                        .tracked_scroll_handle(&self.rows_scroll_handle),
                    window,
                    cx,
                ),
        )
    }
}

impl EventEmitter<DismissEvent> for EnvironmentEditorModal {}

impl ModalView for EnvironmentEditorModal {}

impl Focusable for EnvironmentEditorModal {
    fn focus_handle(&self, _cx: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl Render for EnvironmentEditorModal {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let title = if self.show_scope_list {
            "Manage Environments"
        } else {
            "Edit Variables"
        };

        // The way out sits where every window in this editor keeps it -- top
        // right, on the row the window names itself. The row and the spacer
        // before the corner both come from the shared header, so the close
        // control lands in the same place here as in every other dialog.
        let title_bar = cyberpunk::dialog_header(title, cx)
            .debug_selector(|| "environment-editor-title-bar".to_string())
            .child(
                div()
                    .debug_selector(|| "environment-editor-dismiss".to_string())
                    .child(
                        IconButton::new("environment-editor-dismiss", IconName::Close)
                            .icon_size(IconSize::Small)
                            .style(cyberpunk::Rank::Quiet.style())
                            .tooltip(Tooltip::text("Close"))
                            .on_click(cx.listener(|this, _, _, cx| this.cancel(cx))),
                    ),
            );

        // Adding, copying and removing live on one frame above the list, the
        // way the run configurations window does it, rather than as a trash
        // icon on every row and a second name field under them. All three act
        // on whichever environment is being looked at, which is also the one
        // whose variables fill the other half -- so there is never a doubt
        // about what is about to be copied or removed.
        let can_delete = matches!(self.scope, Scope::Environment(_));
        // Each wrapped in a box that carries its own name: a button takes its
        // debug name from the icon it draws, and two buttons drawing different
        // icons in one frame still need telling apart from outside.
        let toolbar = cyberpunk::segmented([
            div()
                .debug_selector(|| "environment-editor-create".to_string())
                .child(
                    IconButton::new("environment-editor-create", IconName::Plus)
                        .icon_size(IconSize::Small)
                        .tooltip(Tooltip::text("Add an environment"))
                        .on_click(
                            cx.listener(|this, _, window, cx| this.create_environment(window, cx)),
                        ),
                )
                .into_any_element(),
            div()
                .debug_selector(|| "environment-editor-duplicate".to_string())
                .child(
                    IconButton::new("environment-editor-duplicate", IconName::Copy)
                        .icon_size(IconSize::Small)
                        .tooltip(Tooltip::text("Make a copy of this environment"))
                        .disabled(!can_delete)
                        .on_click(cx.listener(|this, _, window, cx| {
                            this.duplicate_environment(window, cx)
                        })),
                )
                .into_any_element(),
            div()
                .debug_selector(|| "environment-editor-delete".to_string())
                .child(
                    IconButton::new("environment-editor-delete", IconName::Dash)
                        .icon_size(IconSize::Small)
                        .tooltip(Tooltip::text("Remove this environment"))
                        .disabled(!can_delete)
                        .on_click(cx.listener(|this, _, window, cx| {
                            this.delete_chosen_environment(window, cx)
                        })),
                )
                .into_any_element(),
        ]);

        cyberpunk::dialog_shell(cx)
            .id("environment-editor-modal-root")
            .debug_selector(|| "environment-editor-modal-root".to_string())
            .key_context("EnvironmentEditorModal")
            .track_focus(&self.focus_handle)
            .on_action(cx.listener(|this, _: &menu::Cancel, _window, cx| this.cancel(cx)))
            .child(title_bar)
            .child(
                cyberpunk::dialog_body().child(
                    v_flex()
                        .flex_1()
                        // The two halves may each be shorter and narrower than
                        // what is in them; each scrolls its own contents
                        // instead.
                        .min_h_0()
                        .min_w_0()
                        .px_3()
                        .pb_2()
                        .gap_3()
                        .when(self.show_scope_list, |dialog| dialog.child(toolbar))
                        .child(
                            h_flex()
                                .flex_1()
                                // Stretched, not centred. A row centres its
                                // children by default here, and a centred child
                                // is given the height of its own contents rather
                                // than the height of the row -- so the column of
                                // environments stood 773px tall in a 480px
                                // window, centred on it, with its top above the
                                // window's own edge and nothing to scroll. Being
                                // allowed to shrink means nothing until it is
                                // first told how tall it may be.
                                .items_stretch()
                                .min_h_0()
                                .min_w_0()
                                .gap_3()
                                .when(self.show_scope_list, |el| {
                                    el.child(self.render_scope_list(window, cx))
                                })
                                .child(self.render_variable_panel(window, cx)),
                        ),
                ),
            )
            .child(
                cyberpunk::dialog_footer()
                    .child(
                        cyberpunk::dialog_footer_left().child(
                            Label::new("Write one as {{name}} in a URL, a header or a body.")
                                .size(LabelSize::XSmall)
                                .color(Color::Muted)
                                .truncate(),
                        ),
                    )
                    .child(cyberpunk::dialog_footer_spacer())
                    .child(
                        Button::new("environment-editor-close", "Close")
                            .label_size(LabelSize::Small)
                            .style(cyberpunk::Rank::Neutral.style())
                            .on_click(cx.listener(|this, _, _, cx| this.cancel(cx))),
                    ),
            )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::ApiClientStore;
    use gpui::{TestAppContext, VisualTestContext};

    fn init_test(cx: &mut TestAppContext) {
        cx.update(|cx| {
            let settings_store = settings::SettingsStore::test(cx);
            cx.set_global(settings_store);
            theme_settings::init(theme::LoadThemes::JustBase, cx);
            editor::init(cx);
        });
    }

    async fn build_environments_modal(
        cx: &mut TestAppContext,
    ) -> (
        Entity<ApiClientStore>,
        Entity<EnvironmentEditorModal>,
        VisualTestContext,
    ) {
        init_test(cx);
        let store = cx.new(|cx| ApiClientStore::new(cx));
        let window = cx.add_window({
            let store = store.clone();
            move |window, cx| EnvironmentEditorModal::new_for_environments(store, window, cx)
        });
        let mut cx = VisualTestContext::from_window(window.into(), cx);
        let view = window.root(&mut cx).unwrap();
        (store, view, cx)
    }

    async fn build_collection_modal(
        cx: &mut TestAppContext,
        collection_id: CollectionId,
        store: Entity<ApiClientStore>,
    ) -> (Entity<EnvironmentEditorModal>, VisualTestContext) {
        init_test(cx);
        let window = cx.add_window(move |window, cx| {
            EnvironmentEditorModal::new_for_collection(store, collection_id, window, cx)
        });
        let mut cx = VisualTestContext::from_window(window.into(), cx);
        let view = window.root(&mut cx).unwrap();
        (view, cx)
    }

    fn debug_center(
        cx: &mut VisualTestContext,
        selector: &'static str,
    ) -> gpui::Point<gpui::Pixels> {
        cx.debug_bounds(selector)
            .unwrap_or_else(|| panic!("expected debug bounds for {selector}"))
            .center()
    }

    fn draw(cx: &mut VisualTestContext) {
        cx.update(|window, cx| {
            window.refresh();
            let _ = window.draw(cx);
        });
        cx.run_until_parked();
    }

    /// Adding one and naming it are one motion now: the plus makes it, moves to
    /// it, and hands the reader the name field with the placeholder selected --
    /// so typing replaces it. Two places to type a name is what this replaced.
    #[gpui::test]
    async fn adding_an_environment_names_it_in_the_field_on_the_right(cx: &mut TestAppContext) {
        let (store, view, mut cx) = build_environments_modal(cx).await;
        draw(&mut cx);

        let create_button = debug_center(&mut cx, "environment-editor-create");
        cx.simulate_click(create_button, gpui::Modifiers::none());
        cx.run_until_parked();
        draw(&mut cx);
        store.read_with(&cx, |store, _| {
            assert_eq!(store.environments.len(), 1);
            assert_eq!(store.environments[0].name, NEW_ENVIRONMENT_NAME);
        });

        cx.simulate_input("Staging");
        cx.run_until_parked();
        store.read_with(&cx, |store, _| {
            assert_eq!(
                store.environments[0].name, "Staging",
                "typing goes straight into the name, over the placeholder"
            );
        });

        // And the one control that removes it is the one beside the plus. It
        // asks first, the way it always has.
        let delete_button = debug_center(&mut cx, "environment-editor-delete");
        cx.simulate_click(delete_button, gpui::Modifiers::none());
        cx.run_until_parked();
        cx.simulate_prompt_answer("Delete");
        cx.run_until_parked();
        view.read_with(&cx, |view, _| {
            assert!(
                matches!(view.scope, Scope::Global),
                "removing what was being looked at falls back to Global"
            );
        });
        store.read_with(&cx, |store, _| assert!(store.environments.is_empty()));
    }

    #[gpui::test]
    async fn editing_a_variable_row_through_the_real_editors_persists_to_the_environment(
        cx: &mut TestAppContext,
    ) {
        let (store, view, mut cx) = build_environments_modal(cx).await;
        let environment_id = store.update(&mut cx, |store, cx| {
            store.create_environment("Staging".into(), cx)
        });
        view.update_in(&mut cx, |view, window, cx| {
            view.select_scope(Scope::Environment(environment_id), window, cx);
        });
        draw(&mut cx);

        let add_button = debug_center(&mut cx, "variable-row-add");
        cx.simulate_click(add_button, gpui::Modifiers::none());
        cx.run_until_parked();

        let key_editor = view.read_with(&cx, |view, _| view.rows[0].key_editor.clone());
        let value_editor = view.read_with(&cx, |view, _| view.rows[0].current_value_editor.clone());
        view.update_in(&mut cx, |_, window, cx| {
            key_editor.update(cx, |editor, cx| editor.focus_handle(cx).focus(window, cx));
        });
        cx.simulate_input("base_url");
        cx.run_until_parked();
        view.update_in(&mut cx, |_, window, cx| {
            value_editor.update(cx, |editor, cx| editor.focus_handle(cx).focus(window, cx));
        });
        cx.simulate_input("https://staging.example.com");
        cx.run_until_parked();

        store.read_with(&cx, |store, _| {
            let environment = store
                .environments
                .iter()
                .find(|e| e.id == environment_id)
                .unwrap();
            assert_eq!(environment.variables.len(), 1);
            assert_eq!(environment.variables[0].key, "base_url");
            assert_eq!(
                environment.variables[0].current_value,
                "https://staging.example.com"
            );
        });
    }

    #[gpui::test]
    async fn toggling_secret_masks_and_unmasks_the_value_editor(cx: &mut TestAppContext) {
        let (store, view, mut cx) = build_environments_modal(cx).await;
        let environment_id = store.update(&mut cx, |store, cx| {
            store.create_environment("Staging".into(), cx)
        });
        store.update(&mut cx, |store, cx| {
            store.update_environment(Some(environment_id), cx, |environment| {
                environment
                    .variables
                    .push(Variable::new("token".into(), "super-secret".into()));
            });
        });
        view.update_in(&mut cx, |view, window, cx| {
            view.select_scope(Scope::Environment(environment_id), window, cx);
        });
        draw(&mut cx);

        assert!(!view.read_with(&cx, |view, _| view.rows[0].secret));

        let secret_toggle = debug_center(&mut cx, "variable-row-secret-0");
        cx.simulate_click(secret_toggle, gpui::Modifiers::none());
        cx.run_until_parked();

        let masked = view.read_with(&cx, |view, cx| {
            view.rows[0].current_value_editor.read(cx).is_masked(cx)
        });
        assert!(masked, "current value editor should be masked once secret");

        draw(&mut cx);
        let reveal_toggle = debug_center(&mut cx, "variable-row-reveal-0");
        cx.simulate_click(reveal_toggle, gpui::Modifiers::none());
        cx.run_until_parked();

        let revealed = view.read_with(&cx, |view, cx| {
            view.rows[0].current_value_editor.read(cx).is_masked(cx)
        });
        assert!(
            !revealed,
            "revealing should unmask the current value editor"
        );

        store.read_with(&cx, |store, _| {
            let environment = store
                .environments
                .iter()
                .find(|e| e.id == environment_id)
                .unwrap();
            assert!(environment.variables[0].secret);
        });
    }

    #[gpui::test]
    async fn removing_a_variable_row_removes_it_from_the_environment(cx: &mut TestAppContext) {
        let (store, view, mut cx) = build_environments_modal(cx).await;
        let environment_id = store.update(&mut cx, |store, cx| {
            store.create_environment("Staging".into(), cx)
        });
        store.update(&mut cx, |store, cx| {
            store.update_environment(Some(environment_id), cx, |environment| {
                environment
                    .variables
                    .push(Variable::new("a".into(), "1".into()));
            });
        });
        view.update_in(&mut cx, |view, window, cx| {
            view.select_scope(Scope::Environment(environment_id), window, cx);
        });
        draw(&mut cx);

        let remove_button = debug_center(&mut cx, "variable-row-remove-0");
        cx.simulate_click(remove_button, gpui::Modifiers::none());
        cx.run_until_parked();

        store.read_with(&cx, |store, _| {
            let environment = store
                .environments
                .iter()
                .find(|e| e.id == environment_id)
                .unwrap();
            assert!(environment.variables.is_empty());
        });
    }

    /// More environments than the window is tall must scroll inside their own
    /// column, not make the window grow until the half where variables are
    /// edited is off the screen -- which is what a reader with eleven
    /// environments saw.
    ///
    /// Measured on the painted boxes: the least height of a column is the
    /// height of everything in it unless the column is told it may be shorter,
    /// so before this the row grew and the scrolling below it had nothing to
    /// scroll inside.
    /// The dialog places itself the way every other one does, and nothing in it
    /// reaches past its own edge. Before this it positioned and sized itself by
    /// hand, which put it against the left of the window with its second column
    /// hanging off the right, where `overflow_hidden` then cut it off.
    ///
    /// Measured on painted boxes: a laid-out box exists whether or not it is
    /// then clipped away, so a child reaching past its parent is exactly the
    /// fault being tested.
    #[gpui::test]
    async fn nothing_in_the_dialog_reaches_past_its_own_edge(cx: &mut TestAppContext) {
        let (store, view, mut cx) = build_environments_modal(cx).await;
        let mut first = None;
        store.update(&mut cx, |store, cx| {
            for name in [
                "de-canary-gcp",
                "ams-prod-gcp",
                "QA Test POD",
                "qagke-stage",
            ] {
                let id = store.create_environment(name.to_string(), cx);
                first.get_or_insert(id);
            }
        });
        let first = first.expect("four environments were made");
        store.update(&mut cx, |store, cx| {
            store.update_environment(Some(first), cx, |environment| {
                environment.variables.push(Variable::new(
                    "base_url".into(),
                    "https://a-fairly-long-host-name.example.com/v1".into(),
                ));
            });
        });
        // Looked at, so the table on the right is a table rather than the
        // offer of a first variable: what must not reach past the edge is the
        // widest thing this window ever draws.
        view.update_in(&mut cx, |view, window, cx| {
            view.select_scope(Scope::Environment(first), window, cx);
        });
        cx.run_until_parked();
        draw(&mut cx);

        let dialog = cx
            .debug_bounds("environment-editor-modal-root")
            .expect("the dialog is painted");
        assert!(
            dialog.size.width <= cyberpunk::DIALOG_WIDTH + px(1.),
            "the dialog is {:?} wide; it asks for {:?}",
            dialog.size.width,
            cyberpunk::DIALOG_WIDTH
        );
        assert!(
            dialog.size.height <= cyberpunk::DIALOG_MAX_HEIGHT + px(1.),
            "the dialog is {:?} tall; {:?} is its ceiling",
            dialog.size.height,
            cyberpunk::DIALOG_MAX_HEIGHT
        );

        for name in [
            "environment-editor-list",
            "environment-editor-rows-scroll",
            "environment-editor-title-bar",
            "variable-row-add",
            "variables-heading",
            "variables-heading-current",
            "variable-row-current-0",
            "variable-row-remove-0",
        ] {
            let inside = cx
                .debug_bounds(name)
                .unwrap_or_else(|| panic!("{name} is painted"));
            assert!(
                inside.left() >= dialog.left()
                    && inside.right() <= dialog.right()
                    && inside.top() >= dialog.top()
                    && inside.bottom() <= dialog.bottom(),
                "{name} at {inside:?} reaches past the dialog at {dialog:?}"
            );
        }
    }

    #[gpui::test]
    async fn many_environments_scroll_inside_their_own_column(cx: &mut TestAppContext) {
        let (store, view, mut cx) = build_environments_modal(cx).await;
        store.update(&mut cx, |store, cx| {
            for at in 0..24 {
                store.create_environment(format!("environment-{at}"), cx);
            }
        });
        cx.run_until_parked();
        draw(&mut cx);

        let modal = cx
            .debug_bounds("environment-editor-modal-root")
            .expect("the window is painted");
        assert!(
            modal.size.height <= cyberpunk::DIALOG_MAX_HEIGHT + px(1.),
            "the window is {:?} tall against a ceiling of {:?}: its contents \
             made it grow",
            modal.size.height,
            cyberpunk::DIALOG_MAX_HEIGHT
        );

        let reach = view.read_with(&cx, |view, _| view.list_scroll_handle.max_offset());
        let list = cx.debug_bounds("environment-editor-list-scroll");
        let entries = cx.debug_bounds("environment-editor-list");
        assert!(
            reach.y > px(0.),
            "twenty-five environments in a window this size have to leave something to \
             scroll to, and the column reports {reach:?}. The window is {:?} at {:?}; \
             the column is painted {list:?} and what is in it {entries:?}",
            modal.size,
            modal.origin
        );

        // And the half where variables are edited is still inside the window.
        let rows = cx
            .debug_bounds("environment-editor-rows-scroll")
            .expect("the variables are painted");
        assert!(
            rows.right() <= modal.right() + px(1.) && rows.bottom() <= modal.bottom() + px(1.),
            "the variables reach {:?},{:?} in a window ending at {:?},{:?}",
            rows.right(),
            rows.bottom(),
            modal.right(),
            modal.bottom()
        );
    }

    #[gpui::test]
    async fn global_has_no_delete_button_in_the_scope_list(cx: &mut TestAppContext) {
        let (_store, _view, mut cx) = build_environments_modal(cx).await;
        draw(&mut cx);

        assert!(
            cx.debug_bounds("environment-editor-delete-global")
                .is_none(),
            "Global must never expose a delete affordance"
        );
    }

    #[gpui::test]
    async fn editing_collection_variables_through_the_pinned_modal_persists_to_the_collection(
        cx: &mut TestAppContext,
    ) {
        init_test(cx);
        let store = cx.new(|cx| ApiClientStore::new(cx));
        let collection_id =
            store.update(cx, |store, cx| store.create_collection("Demo".into(), cx));
        let (view, mut cx) = build_collection_modal(cx, collection_id, store.clone()).await;
        draw(&mut cx);

        let add_button = debug_center(&mut cx, "variable-row-add");
        cx.simulate_click(add_button, gpui::Modifiers::none());
        cx.run_until_parked();

        let key_editor = view.read_with(&cx, |view, _| view.rows[0].key_editor.clone());
        view.update_in(&mut cx, |_, window, cx| {
            key_editor.update(cx, |editor, cx| editor.focus_handle(cx).focus(window, cx));
        });
        cx.simulate_input("api_version");
        cx.run_until_parked();

        store.read_with(&cx, |store, _| {
            let collection = store
                .collections
                .iter()
                .find(|c| c.id == collection_id)
                .unwrap();
            assert_eq!(collection.variables.len(), 1);
            assert_eq!(collection.variables[0].key, "api_version");
        });
    }

    #[test]
    fn the_line_under_a_name_counts_in_words() {
        assert_eq!(how_many_variables(0), "no variables");
        assert_eq!(how_many_variables(1), "1 variable");
        assert_eq!(how_many_variables(4), "4 variables");
    }

    /// Three boxes of text side by side say nothing about which is which, so
    /// the table carries headings -- and a heading is worth nothing unless it
    /// stands over the column it names on every row, whatever state that row
    /// happens to be in.
    ///
    /// Measured on painted boxes. Hiding a value used to add an eye to that
    /// row and to no other, which made the row one button wider and slid its
    /// three values out of line with every other row's.
    #[gpui::test]
    async fn the_value_columns_line_up_with_their_headings(cx: &mut TestAppContext) {
        let (store, view, mut cx) = build_environments_modal(cx).await;
        let environment_id = store.update(&mut cx, |store, cx| {
            store.create_environment("Staging".into(), cx)
        });
        store.update(&mut cx, |store, cx| {
            store.update_environment(Some(environment_id), cx, |environment| {
                environment
                    .variables
                    .push(Variable::new("token".into(), "super-secret".into()));
                environment.variables.push(Variable::new(
                    "base_url".into(),
                    "https://staging.example.com".into(),
                ));
            });
        });
        view.update_in(&mut cx, |view, window, cx| {
            view.select_scope(Scope::Environment(environment_id), window, cx);
        });
        draw(&mut cx);

        // One of the two is hidden, which is the state that used to move it.
        let secret_toggle = debug_center(&mut cx, "variable-row-secret-0");
        cx.simulate_click(secret_toggle, gpui::Modifiers::none());
        cx.run_until_parked();
        draw(&mut cx);
        assert!(
            view.read_with(&cx, |view, _| view.rows[0].secret),
            "the first row is the hidden one"
        );

        for (heading, hidden_row, plain_row) in [
            (
                "variables-heading-key",
                "variable-row-key-0",
                "variable-row-key-1",
            ),
            (
                "variables-heading-initial",
                "variable-row-initial-0",
                "variable-row-initial-1",
            ),
            (
                "variables-heading-current",
                "variable-row-current-0",
                "variable-row-current-1",
            ),
        ] {
            let over = cx
                .debug_bounds(heading)
                .unwrap_or_else(|| panic!("{heading} is painted"));
            let hidden = cx
                .debug_bounds(hidden_row)
                .unwrap_or_else(|| panic!("{hidden_row} is painted"));
            let plain = cx
                .debug_bounds(plain_row)
                .unwrap_or_else(|| panic!("{plain_row} is painted"));

            assert_eq!(
                hidden.origin.x, plain.origin.x,
                "{hidden_row} starts at {:?} and {plain_row} at {:?}: a hidden row and a \
                 plain one disagree about where the column is",
                hidden.origin.x, plain.origin.x
            );
            assert_eq!(
                hidden.size.width, plain.size.width,
                "{hidden_row} is {:?} wide and {plain_row} {:?}",
                hidden.size.width, plain.size.width
            );
            // The heading has no border of its own, so a pixel of slack.
            assert!(
                (over.origin.x.as_f32() - hidden.origin.x.as_f32()).abs() <= 1.
                    && (over.right().as_f32() - hidden.right().as_f32()).abs() <= 1.,
                "{heading} spans {:?}..{:?} over a column spanning {:?}..{:?}",
                over.origin.x,
                over.right(),
                hidden.origin.x,
                hidden.right()
            );
        }

        let dialog = cx
            .debug_bounds("environment-editor-modal-root")
            .expect("the dialog is painted");
        let actions = cx
            .debug_bounds("variable-row-remove-0")
            .expect("the row's own actions are painted");
        assert!(
            actions.right() <= dialog.right(),
            "the row's actions reach {:?} in a dialog ending at {:?}",
            actions.right(),
            dialog.right()
        );
    }

    /// An empty half used to say "No variables yet." and leave the reader to
    /// go looking for the plus. It offers the first one instead.
    #[gpui::test]
    async fn an_empty_scope_offers_the_first_variable(cx: &mut TestAppContext) {
        let (store, view, mut cx) = build_environments_modal(cx).await;
        draw(&mut cx);

        assert!(
            cx.debug_bounds("variables-heading").is_none(),
            "there is nothing for a heading to stand over"
        );
        assert!(
            cx.debug_bounds("variable-rows-empty").is_some(),
            "the empty half says so"
        );

        let offer = debug_center(&mut cx, "variable-row-empty-add");
        cx.simulate_click(offer, gpui::Modifiers::none());
        cx.run_until_parked();
        draw(&mut cx);

        view.read_with(&cx, |view, _| assert_eq!(view.rows.len(), 1));
        store.read_with(&cx, |store, _| {
            assert_eq!(
                store.global_environment.variables.len(),
                1,
                "the row is written to the scope being looked at"
            );
        });
        assert!(
            cx.debug_bounds("variable-rows-empty").is_none(),
            "the offer goes once it has been taken"
        );
        assert!(
            cx.debug_bounds("variables-heading").is_some(),
            "and the headings arrive with the first row"
        );
    }

    /// Copying moved onto the frame above the list, beside adding and
    /// removing, instead of sitting as a lone word in the middle of the form.
    #[gpui::test]
    async fn the_toolbar_copies_the_environment_being_looked_at(cx: &mut TestAppContext) {
        let (store, view, mut cx) = build_environments_modal(cx).await;
        let environment_id = store.update(&mut cx, |store, cx| {
            store.create_environment("Staging".into(), cx)
        });
        store.update(&mut cx, |store, cx| {
            store.update_environment(Some(environment_id), cx, |environment| {
                environment.variables.push(Variable::new(
                    "base_url".into(),
                    "https://staging.example.com".into(),
                ));
            });
        });
        view.update_in(&mut cx, |view, window, cx| {
            view.select_scope(Scope::Environment(environment_id), window, cx);
        });
        draw(&mut cx);

        let duplicate = debug_center(&mut cx, "environment-editor-duplicate");
        cx.simulate_click(duplicate, gpui::Modifiers::none());
        cx.run_until_parked();

        store.read_with(&cx, |store, _| {
            assert_eq!(store.environments.len(), 2);
            let copy = store
                .environments
                .iter()
                .find(|environment| environment.id != environment_id)
                .expect("the copy exists");
            assert_eq!(copy.name, "Staging Copy");
            assert_eq!(copy.variables.len(), 1);
            assert_eq!(copy.variables[0].key, "base_url");
        });
        view.read_with(&cx, |view, _| {
            assert!(
                !matches!(view.scope, Scope::Environment(id) if id == environment_id),
                "the copy is what the reader is looking at afterwards"
            );
        });
    }

    /// A box that looks typed-in and is not is worse than a caption. Global
    /// has no name to change, so it does not get one; an environment does.
    #[gpui::test]
    async fn only_a_scope_that_can_be_renamed_shows_a_name_field(cx: &mut TestAppContext) {
        let (store, view, mut cx) = build_environments_modal(cx).await;
        draw(&mut cx);
        assert!(
            cx.debug_bounds("DIALOG-FIELD-name").is_none(),
            "Global has no name of its own to change"
        );

        let environment_id = store.update(&mut cx, |store, cx| {
            store.create_environment("Staging".into(), cx)
        });
        view.update_in(&mut cx, |view, window, cx| {
            view.select_scope(Scope::Environment(environment_id), window, cx);
        });
        draw(&mut cx);
        assert!(
            cx.debug_bounds("DIALOG-FIELD-name").is_some(),
            "an environment is renamed here"
        );
    }
    /// Where the way out is painted, not where the element tree says it was
    /// put. It belongs in the corner the reader reaches for, on the row the
    /// window names itself -- which is what the shared header is for.
    #[gpui::test]
    async fn the_way_out_is_painted_in_the_top_right_corner_of_the_header(cx: &mut TestAppContext) {
        let (_store, _view, mut cx) = build_environments_modal(cx).await;
        draw(&mut cx);

        let header = cx
            .debug_bounds("environment-editor-title-bar")
            .expect("the header is painted");
        let close = cx
            .debug_bounds("environment-editor-dismiss")
            .expect("the way out is painted");

        assert!(
            close.right() > header.left() + header.size.width * 0.85,
            "the way out ends at {:?} in a header spanning {:?}..{:?}, so it is not in the \
             corner",
            close.right(),
            header.left(),
            header.right()
        );
        assert!(
            close.top() >= header.top() - px(1.) && close.bottom() <= header.bottom() + px(1.),
            "the way out is painted {:?}..{:?} vertically, outside the header's own \
             {:?}..{:?}, so it is not on the naming row at all",
            close.top(),
            close.bottom(),
            header.top(),
            header.bottom()
        );
    }

    /// This window saves as the reader types, so its footer carries one
    /// action and that action is the way out. What is asserted is where the
    /// last action on the bar is painted: in the bottom-right corner of the
    /// surface, with the incidental note beside it held to the left.
    #[gpui::test]
    async fn the_last_footer_action_is_painted_in_the_bottom_right_corner(cx: &mut TestAppContext) {
        let (_store, _view, mut cx) = build_environments_modal(cx).await;
        draw(&mut cx);

        let shell = cx
            .debug_bounds("environment-editor-modal-root")
            .expect("the dialog is painted");
        let footer = cx
            .debug_bounds("DIALOG-FOOTER")
            .expect("the bar is painted");
        let action = cx
            .debug_bounds("BUTTON-Close")
            .expect("the action is painted");
        let beside = cx
            .debug_bounds("DIALOG-FOOTER-LEFT")
            .expect("what sits beside it is painted");

        assert!(
            action.right() > footer.left() + footer.size.width * 0.85,
            "the action ends at {:?} in a bar spanning {:?}..{:?}, left of the corner it \
             belongs in",
            action.right(),
            footer.left(),
            footer.right()
        );
        assert!(
            action.bottom() <= shell.bottom() + px(1.)
                && action.bottom() > shell.bottom() - footer.size.height - px(1.),
            "the action ends at {:?} in a window ending at {:?}: it is not on the bottom bar",
            action.bottom(),
            shell.bottom()
        );
        assert!(
            beside.right() <= action.left() + px(1.),
            "the note reaches {:?} and the action starts at {:?}, so the note has been \
             dragged into the corner with it",
            beside.right(),
            action.left()
        );
        assert!(
            beside.size.width <= footer.size.width * 0.5 + px(1.),
            "the note took {:?} of a {:?} bar, past the half it is allowed",
            beside.size.width,
            footer.size.width
        );
    }
}
