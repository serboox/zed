use std::sync::Arc;

use db_client::{ConnectionId, DatabaseDriver};
use fuzzy::{StringMatch, StringMatchCandidate, match_strings};
use gpui::{
    App, Context, DismissEvent, Entity, EventEmitter, FocusHandle, Focusable, SharedString,
    Subscription, Task, WeakEntity, Window,
};
use picker::{Picker, PickerDelegate};
use ui::{Icon, IconName, Label, ListItem, ListItemSpacing, prelude::*};
use workspace::{ModalView, Workspace};

use crate::result_view::{ResultView, format_query_error};
use crate::store::{DatabaseStore, SchemaObjectKind, SchemaObjectRef};

/// A workspace modal wrapping the go-to-object fuzzy picker. Confirming an
/// entry opens that table/view's data grid (or its owning table's, for a
/// column entry) in the active pane.
pub struct GoToObjectPalette {
    picker: Entity<Picker<GoToObjectDelegate>>,
    _subscription: Subscription,
}

impl GoToObjectPalette {
    pub fn new(
        store: Entity<DatabaseStore>,
        workspace: WeakEntity<Workspace>,
        connection_id: ConnectionId,
        connection_label: SharedString,
        driver: DatabaseDriver,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let objects = store.read(cx).schema_objects(connection_id);
        let delegate = GoToObjectDelegate {
            store,
            workspace,
            connection_id,
            connection_label,
            driver,
            objects,
            matches: Vec::new(),
            selected_index: 0,
        };
        let picker = cx.new(|cx| Picker::uniform_list(delegate, window, cx));
        // The picker emits DismissEvent on confirm/escape; forward it so the
        // workspace's modal layer (which watches THIS view, not the inner
        // picker) actually closes the palette.
        let subscription = cx.subscribe(&picker, |_, _, _: &DismissEvent, cx| {
            cx.emit(DismissEvent);
        });
        Self {
            picker,
            _subscription: subscription,
        }
    }
}

impl ModalView for GoToObjectPalette {}

impl EventEmitter<DismissEvent> for GoToObjectPalette {}

impl Focusable for GoToObjectPalette {
    fn focus_handle(&self, cx: &App) -> FocusHandle {
        self.picker.focus_handle(cx)
    }
}

impl Render for GoToObjectPalette {
    fn render(&mut self, _window: &mut Window, _cx: &mut Context<Self>) -> impl IntoElement {
        v_flex().w(rems(34.)).child(self.picker.clone())
    }
}

pub struct GoToObjectDelegate {
    store: Entity<DatabaseStore>,
    workspace: WeakEntity<Workspace>,
    connection_id: ConnectionId,
    connection_label: SharedString,
    driver: DatabaseDriver,
    objects: Vec<SchemaObjectRef>,
    matches: Vec<StringMatch>,
    selected_index: usize,
}

impl GoToObjectDelegate {
    fn icon_for(kind: SchemaObjectKind) -> IconName {
        match kind {
            SchemaObjectKind::Database => IconName::DatabaseZap,
            SchemaObjectKind::Table => IconName::FileGeneric,
            SchemaObjectKind::View => IconName::Eye,
            SchemaObjectKind::Column => IconName::Hash,
        }
    }

    /// Opens the data grid for the entry's table (for a table/view entry) or
    /// its owning table (for a column entry). Database entries have nothing
    /// to page through and are ignored on confirm.
    fn open_data(&self, object: &SchemaObjectRef, window: &mut Window, cx: &mut App) {
        let Some(table) = &object.table else { return };
        let quoted_db = self.driver.quote_identifier(&object.database);
        let quoted_table = self.driver.quote_identifier(table);
        let sql = format!("SELECT * FROM {quoted_db}.{quoted_table} LIMIT 500");
        let connection_id = self.connection_id;
        let database = object.database.clone();
        let title = SharedString::from(table.clone());
        let task = self.store.update(cx, |store, cx| {
            store.execute_query(connection_id, database, sql, cx)
        });
        let store_weak = self.store.downgrade();
        let env_color = crate::panel::connection_env_color(&store_weak, connection_id, cx);
        let result_view = cx.new(|cx| ResultView::new(title, cx).with_env_color(env_color));
        let rv = result_view.clone();
        let workspace = self.workspace.clone();
        window
            .spawn(cx, async move |cx| {
                let outcome = task.await;
                rv.update(cx, |view, cx| match outcome {
                    Ok(result) => view.set_result(result, cx),
                    Err(err) => view.set_error(format_query_error(&err), cx),
                });
                workspace
                    .update_in(cx, |workspace, window, cx| {
                        workspace.add_item_to_active_pane(
                            Box::new(result_view),
                            None,
                            true,
                            window,
                            cx,
                        );
                    })
                    .ok();
            })
            .detach();
    }
}

impl PickerDelegate for GoToObjectDelegate {
    type ListItem = ListItem;

    fn name() -> &'static str {
        "go to database object"
    }

    fn match_count(&self) -> usize {
        self.matches.len()
    }

    fn selected_index(&self) -> usize {
        self.selected_index
    }

    fn set_selected_index(
        &mut self,
        ix: usize,
        _window: &mut Window,
        _: &mut Context<Picker<Self>>,
    ) {
        self.selected_index = ix;
    }

    fn placeholder_text(&self, _window: &mut Window, _cx: &mut App) -> Arc<str> {
        format!(
            "Go to database, table, view, or column in {}...",
            self.connection_label
        )
        .into()
    }

    fn update_matches(
        &mut self,
        query: String,
        window: &mut Window,
        cx: &mut Context<Picker<Self>>,
    ) -> Task<()> {
        let candidates: Vec<StringMatchCandidate> = self
            .objects
            .iter()
            .enumerate()
            .map(|(id, object)| StringMatchCandidate::new(id, &object.display_label()))
            .collect();

        let matches = if query.is_empty() {
            (0..self.objects.len())
                .map(|id| StringMatch {
                    candidate_id: id,
                    score: 0.,
                    positions: Vec::new(),
                    string: self.objects[id].display_label(),
                })
                .collect()
        } else {
            cx.foreground_executor().block_on(match_strings(
                &candidates,
                &query,
                true,
                true,
                200,
                &Default::default(),
                cx.background_executor().clone(),
            ))
        };

        self.matches = matches;
        self.selected_index = 0;
        cx.notify();
        let _ = window;
        Task::ready(())
    }

    fn confirm(&mut self, _secondary: bool, window: &mut Window, cx: &mut Context<Picker<Self>>) {
        let Some(m) = self.matches.get(self.selected_index) else {
            return;
        };
        let Some(object) = self.objects.get(m.candidate_id).cloned() else {
            return;
        };
        self.open_data(&object, window, cx);
        cx.emit(DismissEvent);
    }

    fn dismissed(&mut self, _: &mut Window, cx: &mut Context<Picker<Self>>) {
        cx.emit(DismissEvent);
    }

    fn render_match(
        &self,
        ix: usize,
        selected: bool,
        _: &mut Window,
        _cx: &mut Context<Picker<Self>>,
    ) -> Option<Self::ListItem> {
        let m = self.matches.get(ix)?;
        let object = self.objects.get(m.candidate_id)?;
        Some(
            ListItem::new(ix)
                .inset(true)
                .spacing(ListItemSpacing::Sparse)
                .toggle_state(selected)
                .start_slot(Icon::new(Self::icon_for(object.kind)))
                .child(Label::new(object.display_label())),
        )
    }
}
