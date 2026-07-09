use crate::store::ApiClientStore;
use api_client::{CollectionId, EnvironmentId, Variable};
use editor::{Editor, EditorEvent};
use gpui::{
    App, Context, DismissEvent, Entity, EventEmitter, FocusHandle, Focusable, Render, ScrollHandle,
    Subscription, Window,
};
use ui::{
    Checkbox, Icon, IconName, IconSize, Label, LabelSize, ScrollAxes, Scrollbars, ToggleState,
    WithScrollbar, prelude::*,
};
use workspace::ModalView;

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
    new_environment_name_editor: Entity<Editor>,
    rows: Vec<VariableRow>,
    rows_scroll_handle: ScrollHandle,
    list_scroll_handle: ScrollHandle,
    _subscriptions: Vec<Subscription>,
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
        let new_environment_name_editor =
            new_single_line_editor("New environment name", "", window, cx);

        let mut this = Self {
            focus_handle: cx.focus_handle(),
            store,
            show_scope_list,
            scope,
            name_editor,
            new_environment_name_editor,
            rows: Vec::new(),
            rows_scroll_handle: ScrollHandle::new(),
            list_scroll_handle: ScrollHandle::new(),
            _subscriptions: Vec::new(),
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
        self._subscriptions.push(subscription);
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

    fn create_environment(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let name = self
            .new_environment_name_editor
            .read(cx)
            .text(cx)
            .trim()
            .to_string();
        if name.is_empty() {
            return;
        }
        let id = self
            .store
            .update(cx, |store, cx| store.create_environment(name, cx));
        self.new_environment_name_editor
            .update(cx, |editor, cx| editor.set_text(String::new(), window, cx));
        self.select_scope(Scope::Environment(id), window, cx);
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
        self.store
            .update(cx, |store, cx| store.delete_environment(id, cx));
        if self.scope == Scope::Environment(id) {
            self.select_scope(Scope::Global, window, cx);
        } else {
            cx.notify();
        }
    }

    fn cancel(&mut self, cx: &mut Context<Self>) {
        cx.emit(DismissEvent);
    }

    fn render_scope_list(&self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let environments: Vec<(EnvironmentId, String)> = self
            .store
            .read(cx)
            .environments
            .iter()
            .map(|environment| (environment.id, environment.name.clone()))
            .collect();

        let mut list =
            v_flex()
                .id("environment-editor-list")
                .gap_0p5()
                .child(
                    self.render_scope_entry("Global", Scope::Global, cx, |this, window, cx| {
                        this.select_scope(Scope::Global, window, cx);
                    }),
                );
        for (id, name) in environments {
            list = list.child(self.render_scope_entry(
                name,
                Scope::Environment(id),
                cx,
                move |this, window, cx| {
                    this.select_scope(Scope::Environment(id), window, cx);
                },
            ));
        }

        v_flex()
            .w(px(200.))
            .h_full()
            .gap_2()
            .child(
                div()
                    .id("environment-editor-list-scroll")
                    .flex_1()
                    .overflow_scroll()
                    .track_scroll(&self.list_scroll_handle)
                    .child(list)
                    .custom_scrollbars(
                        Scrollbars::always_visible(ScrollAxes::Vertical),
                        window,
                        cx,
                    ),
            )
            .child({
                let colors = cx.theme().colors();
                h_flex()
                    .gap_1()
                    .child(
                        div()
                            .flex_1()
                            .px_2()
                            .py_1()
                            .rounded_md()
                            .border_1()
                            .border_color(colors.border)
                            .bg(colors.background)
                            .child(self.new_environment_name_editor.clone()),
                    )
                    .child(
                        div()
                            .id("environment-editor-create-hitbox")
                            .debug_selector(|| "environment-editor-create".to_string())
                            .child(
                                Button::new("environment-editor-create", "New")
                                    .style(ButtonStyle::Subtle)
                                    .on_click(cx.listener(|this, _, window, cx| {
                                        this.create_environment(window, cx);
                                    })),
                            ),
                    )
            })
    }

    fn render_scope_entry(
        &self,
        label: impl Into<SharedString>,
        scope: Scope,
        cx: &Context<Self>,
        on_click: impl Fn(&mut Self, &mut Window, &mut Context<Self>) + 'static,
    ) -> impl IntoElement {
        let colors = cx.theme().colors();
        let is_selected = self.scope == scope;
        let label = label.into();
        let is_global = matches!(scope, Scope::Global);
        let mut row = h_flex()
            .id(SharedString::from(format!(
                "environment-editor-entry-{label}"
            )))
            .w_full()
            .px_2()
            .py_1()
            .gap_2()
            .items_center()
            .rounded_md()
            .cursor_pointer()
            .when(is_selected, |el| el.bg(colors.element_selected))
            .when(!is_selected, |el| {
                el.hover(|el| el.bg(colors.element_hover))
            })
            .child(
                Label::new(label)
                    .size(LabelSize::Small)
                    .color(if is_selected {
                        Color::Default
                    } else {
                        Color::Muted
                    }),
            )
            .on_click(cx.listener(move |this, _, window, cx| on_click(this, window, cx)));
        if !is_global {
            let Scope::Environment(id) = scope else {
                return row;
            };
            row = row.child(div().flex_1()).child(
                div()
                    .id(SharedString::from(format!(
                        "environment-editor-delete-{id}"
                    )))
                    .debug_selector(move || format!("environment-editor-delete-{id}"))
                    .cursor_pointer()
                    .child(Icon::new(IconName::Trash).size(IconSize::Small))
                    .on_click(cx.listener(move |this, _, window, cx| {
                        this.delete_environment(id, window, cx);
                    })),
            );
        }
        row
    }

    fn render_row(&self, index: usize, cx: &Context<Self>) -> impl IntoElement {
        let colors = cx.theme().colors();
        let row = &self.rows[index];
        let enabled = row.enabled;
        let secret = row.secret;
        let revealed = row.revealed;
        h_flex()
            .id(SharedString::from(format!("variable-row-{index}")))
            .w_full()
            .gap_2()
            .items_center()
            .child(
                Checkbox::new(
                    SharedString::from(format!("variable-row-enabled-{index}")),
                    if enabled {
                        ToggleState::Selected
                    } else {
                        ToggleState::Unselected
                    },
                )
                .on_click(cx.listener(move |this, _, _, cx| this.toggle_row_enabled(index, cx))),
            )
            .child(
                div()
                    .flex_1()
                    .px_2()
                    .py_1()
                    .rounded_md()
                    .border_1()
                    .border_color(colors.border)
                    .bg(colors.background)
                    .child(row.key_editor.clone()),
            )
            .child(
                div()
                    .flex_1()
                    .px_2()
                    .py_1()
                    .rounded_md()
                    .border_1()
                    .border_color(colors.border)
                    .bg(colors.background)
                    .child(row.initial_value_editor.clone()),
            )
            .child(
                div()
                    .flex_1()
                    .px_2()
                    .py_1()
                    .rounded_md()
                    .border_1()
                    .border_color(colors.border)
                    .bg(colors.background)
                    .child(row.current_value_editor.clone()),
            )
            .child(
                div()
                    .id(SharedString::from(format!("variable-row-secret-{index}")))
                    .debug_selector(move || format!("variable-row-secret-{index}"))
                    .cursor_pointer()
                    .child(
                        Icon::new(if secret {
                            IconName::Lock
                        } else {
                            IconName::LockOff
                        })
                        .size(IconSize::Small)
                        .color(if secret {
                            Color::Warning
                        } else {
                            Color::Muted
                        }),
                    )
                    .on_click(cx.listener(move |this, _, _, cx| this.toggle_row_secret(index, cx))),
            )
            .when(secret, |el| {
                el.child(
                    div()
                        .id(SharedString::from(format!("variable-row-reveal-{index}")))
                        .debug_selector(move || format!("variable-row-reveal-{index}"))
                        .cursor_pointer()
                        .child(
                            Icon::new(if revealed {
                                IconName::EyeOff
                            } else {
                                IconName::Eye
                            })
                            .size(IconSize::Small),
                        )
                        .on_click(
                            cx.listener(move |this, _, _, cx| this.toggle_row_revealed(index, cx)),
                        ),
                )
            })
            .child(
                div()
                    .id(SharedString::from(format!("variable-row-remove-{index}")))
                    .debug_selector(move || format!("variable-row-remove-{index}"))
                    .cursor_pointer()
                    .child(Icon::new(IconName::Trash).size(IconSize::Small))
                    .on_click(cx.listener(move |this, _, _, cx| this.remove_row(index, cx))),
            )
    }

    fn render_variable_panel(
        &self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let colors = cx.theme().colors();
        let title = if self.show_scope_list {
            None
        } else {
            Some(format!("Variables for {}", self.scope_name(cx)))
        };

        let mut column = v_flex().flex_1().gap_2();
        if let Some(title) = title {
            column = column.child(Label::new(title).size(LabelSize::Large));
        } else {
            column = column.child(
                div()
                    .px_2()
                    .py_1()
                    .rounded_md()
                    .border_1()
                    .border_color(colors.border)
                    .bg(colors.background)
                    .child(self.name_editor.clone()),
            );
            if !matches!(self.scope, Scope::Global) {
                column = column.child(
                    Button::new("environment-editor-duplicate", "Duplicate")
                        .style(ButtonStyle::Subtle)
                        .on_click(cx.listener(|this, _, window, cx| {
                            this.duplicate_environment(window, cx);
                        })),
                );
            }
        }

        let mut rows = v_flex().id("variable-rows").gap_2();
        for index in 0..self.rows.len() {
            rows = rows.child(self.render_row(index, cx));
        }
        rows = rows.child(
            div()
                .id("variable-row-add")
                .debug_selector(|| "variable-row-add".to_string())
                .cursor_pointer()
                .child(
                    Label::new("Add Variable")
                        .size(LabelSize::Small)
                        .color(Color::Accent),
                )
                .on_click(cx.listener(|this, _, window, cx| this.add_row(window, cx))),
        );

        column.child(
            div()
                .id("environment-editor-rows-scroll")
                .flex_1()
                .overflow_scroll()
                .track_scroll(&self.rows_scroll_handle)
                .child(rows)
                .custom_scrollbars(Scrollbars::always_visible(ScrollAxes::Vertical), window, cx),
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
        let colors = cx.theme().colors();
        let title = if self.show_scope_list {
            "Manage Environments"
        } else {
            "Edit Variables"
        };

        v_flex()
            .key_context("EnvironmentEditorModal")
            .track_focus(&self.focus_handle)
            .on_action(cx.listener(|this, _: &menu::Cancel, _window, cx| this.cancel(cx)))
            .w(px(680.))
            .h(px(480.))
            .p_3()
            .gap_3()
            .bg(colors.elevated_surface_background)
            .rounded_lg()
            .border_1()
            .border_color(colors.border)
            .child(Label::new(title).size(LabelSize::Large))
            .child(
                h_flex()
                    .flex_1()
                    .gap_3()
                    .when(self.show_scope_list, |el| {
                        el.child(self.render_scope_list(window, cx))
                    })
                    .child(self.render_variable_panel(window, cx)),
            )
            .child(
                h_flex().justify_end().child(
                    Button::new("environment-editor-close", "Close")
                        .style(ButtonStyle::Filled)
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

    #[gpui::test]
    async fn creating_an_environment_through_the_new_environment_field_adds_it_to_the_store(
        cx: &mut TestAppContext,
    ) {
        let (store, view, mut cx) = build_environments_modal(cx).await;
        draw(&mut cx);

        let new_name_editor =
            view.read_with(&cx, |view, _| view.new_environment_name_editor.clone());
        view.update_in(&mut cx, |_, window, cx| {
            new_name_editor.update(cx, |editor, cx| editor.focus_handle(cx).focus(window, cx));
        });
        cx.simulate_input("Staging");
        cx.run_until_parked();

        let create_button = debug_center(&mut cx, "environment-editor-create");
        cx.simulate_click(create_button, gpui::Modifiers::none());
        cx.run_until_parked();

        store.read_with(&cx, |store, _| {
            assert_eq!(store.environments.len(), 1);
            assert_eq!(store.environments[0].name, "Staging");
        });
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
}
