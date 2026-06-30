use db_client::connection::{ConnectionId, DatabaseDriver};
use db_client::schema::ColumnInfo;
use editor::Editor;
use gpui::{
    Context, DismissEvent, Entity, EventEmitter, FocusHandle, Focusable, Window, prelude::*,
};
use ui::{Checkbox, Divider, Tooltip, prelude::*};
use util::ResultExt;
use workspace::ModalView;

use crate::store::DatabaseStore;

fn quote_ident(name: &str, driver: DatabaseDriver) -> String {
    match driver {
        DatabaseDriver::MySQL => format!("`{}`", name.replace('`', "``")),
        _ => format!("\"{}\"", name.replace('"', "\"\"")),
    }
}

#[derive(Clone)]
struct ColumnDraft {
    original: Option<ColumnInfo>,
    name: Entity<Editor>,
    data_type: Entity<Editor>,
    nullable: bool,
    dropped: bool,
}

/// A pending structural change derived from the diff between the original
/// columns and the edited drafts. Kept separate from the editor state so the
/// SQL generation is pure and testable.
#[derive(Debug, Clone, PartialEq)]
pub enum ColumnChange {
    Add {
        name: String,
        data_type: String,
        nullable: bool,
    },
    Drop {
        name: String,
    },
    Rename {
        from: String,
        to: String,
    },
    Modify {
        name: String,
        data_type: String,
        nullable: bool,
    },
}

#[derive(Debug, Clone, PartialEq)]
struct DraftSnapshot {
    original: Option<(String, String, bool)>,
    name: String,
    data_type: String,
    nullable: bool,
    dropped: bool,
}

fn diff_changes(drafts: &[DraftSnapshot]) -> Vec<ColumnChange> {
    let mut changes = Vec::new();
    for draft in drafts {
        match &draft.original {
            None if !draft.dropped && !draft.name.trim().is_empty() => {
                changes.push(ColumnChange::Add {
                    name: draft.name.trim().to_string(),
                    data_type: draft.data_type.trim().to_string(),
                    nullable: draft.nullable,
                });
            }
            Some((original_name, original_type, original_nullable)) => {
                if draft.dropped {
                    changes.push(ColumnChange::Drop {
                        name: original_name.clone(),
                    });
                    continue;
                }
                let new_name = draft.name.trim();
                if !new_name.is_empty() && new_name != original_name {
                    changes.push(ColumnChange::Rename {
                        from: original_name.clone(),
                        to: new_name.to_string(),
                    });
                }
                let type_changed = draft.data_type.trim() != original_type;
                let nullable_changed = draft.nullable != *original_nullable;
                if type_changed || nullable_changed {
                    changes.push(ColumnChange::Modify {
                        name: if new_name.is_empty() {
                            original_name.clone()
                        } else {
                            new_name.to_string()
                        },
                        data_type: draft.data_type.trim().to_string(),
                        nullable: draft.nullable,
                    });
                }
            }
            _ => {}
        }
    }
    changes
}

/// Renders the ALTER statements for the given changes. MySQL collapses
/// rename+type into a single `CHANGE COLUMN`; other drivers use the standard
/// `RENAME COLUMN` / `ALTER COLUMN` forms (best effort for SQLite/ClickHouse,
/// which support a subset).
pub fn generate_alter_statements(
    table: &str,
    driver: DatabaseDriver,
    changes: &[ColumnChange],
) -> Vec<String> {
    let table_ident = quote_ident(table, driver);
    let null_clause = |nullable: bool| if nullable { "NULL" } else { "NOT NULL" };
    let mut statements = Vec::new();
    for change in changes {
        match change {
            ColumnChange::Add {
                name,
                data_type,
                nullable,
            } => {
                statements.push(format!(
                    "ALTER TABLE {table_ident} ADD COLUMN {} {} {};",
                    quote_ident(name, driver),
                    data_type,
                    null_clause(*nullable)
                ));
            }
            ColumnChange::Drop { name } => {
                statements.push(format!(
                    "ALTER TABLE {table_ident} DROP COLUMN {};",
                    quote_ident(name, driver)
                ));
            }
            ColumnChange::Rename { from, to } => {
                if driver != DatabaseDriver::MySQL {
                    statements.push(format!(
                        "ALTER TABLE {table_ident} RENAME COLUMN {} TO {};",
                        quote_ident(from, driver),
                        quote_ident(to, driver)
                    ));
                }
            }
            ColumnChange::Modify {
                name,
                data_type,
                nullable,
            } => match driver {
                DatabaseDriver::MySQL => statements.push(format!(
                    "ALTER TABLE {table_ident} MODIFY COLUMN {} {} {};",
                    quote_ident(name, driver),
                    data_type,
                    null_clause(*nullable)
                )),
                _ => {
                    statements.push(format!(
                        "ALTER TABLE {table_ident} ALTER COLUMN {} TYPE {};",
                        quote_ident(name, driver),
                        data_type
                    ));
                    let null_op = if *nullable { "DROP NOT NULL" } else { "SET NOT NULL" };
                    statements.push(format!(
                        "ALTER TABLE {table_ident} ALTER COLUMN {} {null_op};",
                        quote_ident(name, driver)
                    ));
                }
            },
        }
    }
    // MySQL handles rename together with type via CHANGE COLUMN; fold a
    // Rename followed by a Modify of the same column into one statement.
    if driver == DatabaseDriver::MySQL {
        statements = fold_mysql_rename_modify(&table_ident, driver, changes);
    }
    statements
}

fn fold_mysql_rename_modify(
    table_ident: &str,
    driver: DatabaseDriver,
    changes: &[ColumnChange],
) -> Vec<String> {
    let null_clause = |nullable: bool| if nullable { "NULL" } else { "NOT NULL" };
    let mut statements = Vec::new();
    let mut handled_modify_for: Vec<String> = Vec::new();
    for change in changes {
        if let ColumnChange::Rename { from, to } = change {
            let paired = changes.iter().find_map(|other| match other {
                ColumnChange::Modify {
                    name,
                    data_type,
                    nullable,
                } if name == to => Some((data_type.clone(), *nullable)),
                _ => None,
            });
            if let Some((data_type, nullable)) = paired {
                handled_modify_for.push(to.clone());
                statements.push(format!(
                    "ALTER TABLE {table_ident} CHANGE COLUMN {} {} {} {};",
                    quote_ident(from, driver),
                    quote_ident(to, driver),
                    data_type,
                    null_clause(nullable)
                ));
            } else {
                statements.push(format!(
                    "ALTER TABLE {table_ident} RENAME COLUMN {} TO {};",
                    quote_ident(from, driver),
                    quote_ident(to, driver)
                ));
            }
        }
    }
    for change in changes {
        match change {
            ColumnChange::Add {
                name,
                data_type,
                nullable,
            } => statements.push(format!(
                "ALTER TABLE {table_ident} ADD COLUMN {} {} {};",
                quote_ident(name, driver),
                data_type,
                null_clause(*nullable)
            )),
            ColumnChange::Drop { name } => statements.push(format!(
                "ALTER TABLE {table_ident} DROP COLUMN {};",
                quote_ident(name, driver)
            )),
            ColumnChange::Modify {
                name,
                data_type,
                nullable,
            } if !handled_modify_for.contains(name) => statements.push(format!(
                "ALTER TABLE {table_ident} MODIFY COLUMN {} {} {};",
                quote_ident(name, driver),
                data_type,
                null_clause(*nullable)
            )),
            _ => {}
        }
    }
    statements
}


pub struct ModifyTableView {
    focus_handle: FocusHandle,
    store: Entity<DatabaseStore>,
    connection_id: ConnectionId,
    driver: DatabaseDriver,
    database: String,
    table: String,
    drafts: Vec<ColumnDraft>,
    status: Option<SharedString>,
    busy: bool,
}

impl ModifyTableView {
    pub fn new(
        store: Entity<DatabaseStore>,
        connection_id: ConnectionId,
        driver: DatabaseDriver,
        database: String,
        table: String,
        columns: &[ColumnInfo],
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let drafts = columns
            .iter()
            .map(|column| Self::draft_from_column(column.clone(), window, cx))
            .collect();
        Self {
            focus_handle: cx.focus_handle(),
            store,
            connection_id,
            driver,
            database,
            table,
            drafts,
            status: None,
            busy: false,
        }
    }

    fn draft_from_column(
        column: ColumnInfo,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> ColumnDraft {
        let name = cx.new(|cx| {
            let mut editor = Editor::single_line(window, cx);
            editor.set_text(column.name.clone(), window, cx);
            editor
        });
        let data_type = cx.new(|cx| {
            let mut editor = Editor::single_line(window, cx);
            editor.set_text(column.data_type.clone(), window, cx);
            editor
        });
        let nullable = column.is_nullable;
        ColumnDraft {
            original: Some(column),
            name,
            data_type,
            nullable,
            dropped: false,
        }
    }

    fn add_blank_column(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let name = cx.new(|cx| {
            let mut editor = Editor::single_line(window, cx);
            editor.set_placeholder_text("column_name", window, cx);
            editor
        });
        let data_type = cx.new(|cx| {
            let mut editor = Editor::single_line(window, cx);
            editor.set_placeholder_text("type", window, cx);
            editor
        });
        self.drafts.push(ColumnDraft {
            original: None,
            name,
            data_type,
            nullable: true,
            dropped: false,
        });
        cx.notify();
    }

    fn snapshot(&self, cx: &Context<Self>) -> Vec<DraftSnapshot> {
        self.drafts
            .iter()
            .map(|draft| DraftSnapshot {
                original: draft.original.as_ref().map(|column| {
                    (
                        column.name.clone(),
                        column.data_type.clone(),
                        column.is_nullable,
                    )
                }),
                name: draft.name.read(cx).text(cx),
                data_type: draft.data_type.read(cx).text(cx),
                nullable: draft.nullable,
                dropped: draft.dropped,
            })
            .collect()
    }

    fn pending_statements(&self, cx: &Context<Self>) -> Vec<String> {
        let changes = diff_changes(&self.snapshot(cx));
        generate_alter_statements(&self.table, self.driver, &changes)
    }

    fn execute(&mut self, _window: &mut Window, cx: &mut Context<Self>) {
        let statements = self.pending_statements(cx);
        if statements.is_empty() {
            self.status = Some("No changes to apply.".into());
            cx.notify();
            return;
        }
        self.busy = true;
        self.status = Some("Applying changes…".into());
        let connection_id = self.connection_id;
        let database = self.database.clone();
        cx.notify();
        cx.spawn(async move |this, cx| {
            for statement in statements {
                let task = this.update(cx, |view, cx| {
                    view.store.update(cx, |store, cx| {
                        store.execute_query(connection_id, database.clone(), statement.clone(), cx)
                    })
                })?;
                if let Err(error) = task.await {
                    this.update(cx, |this, cx| {
                        this.busy = false;
                        this.status = Some(format!("Failed: {error}").into());
                        cx.notify();
                    })
                    .log_err();
                    return anyhow::Ok(());
                }
            }
            this.update(cx, |this, cx| {
                this.busy = false;
                cx.emit(DismissEvent);
            })
            .log_err();
            anyhow::Ok(())
        })
        .detach_and_log_err(cx);
    }

    fn render_column_row(
        &self,
        index: usize,
        draft: &ColumnDraft,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let dropped = draft.dropped;
        let nullable = draft.nullable;
        h_flex()
            .w_full()
            .gap_2()
            .items_center()
            .py_0p5()
            .when(dropped, |row| row.opacity(0.5))
            .child(div().w(px(160.)).child(draft.name.clone()))
            .child(div().w(px(140.)).child(draft.data_type.clone()))
            .child(
                Checkbox::new(("nullable", index), nullable.into())
                    .label("Nullable")
                    .on_click(cx.listener(move |this, _, _, cx| {
                        if let Some(draft) = this.drafts.get_mut(index) {
                            draft.nullable = !draft.nullable;
                            cx.notify();
                        }
                    })),
            )
            .child(
                IconButton::new(("drop-column", index), IconName::Trash)
                    .icon_size(IconSize::XSmall)
                    .tooltip(Tooltip::text(if dropped { "Keep column" } else { "Drop column" }))
                    .on_click(cx.listener(move |this, _, _, cx| {
                        if let Some(draft) = this.drafts.get_mut(index) {
                            draft.dropped = !draft.dropped;
                            cx.notify();
                        }
                    })),
            )
    }
}

impl EventEmitter<DismissEvent> for ModifyTableView {}

impl ModalView for ModifyTableView {}

impl Focusable for ModifyTableView {
    fn focus_handle(&self, _cx: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl Render for ModifyTableView {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let preview = self.pending_statements(cx);
        let preview_text: SharedString = if preview.is_empty() {
            "-- No pending changes".into()
        } else {
            preview.join("\n").into()
        };
        let rows: Vec<_> = self
            .drafts
            .iter()
            .enumerate()
            .map(|(index, draft)| self.render_column_row(index, draft, cx).into_any_element())
            .collect();
        let title = format!("Modify {}.{}", self.database, self.table);
        let busy = self.busy;

        v_flex()
            .key_context("ModifyTable")
            .track_focus(&self.focus_handle)
            .elevation_3(cx)
            .w(px(640.))
            .max_h(px(560.))
            .p_3()
            .gap_2()
            .child(crate::widgets::dialog_header(
                title,
                "close-modify",
                cx.listener(|_, _, _, cx| cx.emit(DismissEvent)),
            ))
            .child(Divider::horizontal())
            .child(
                v_flex()
                    .id("modify-columns")
                    .gap_0p5()
                    .max_h(px(280.))
                    .overflow_y_scroll()
                    .children(rows),
            )
            .child(
                Button::new("add-column", "Add Column")
                    .on_click(cx.listener(|this, _, window, cx| this.add_blank_column(window, cx))),
            )
            .child(Divider::horizontal())
            .child(Label::new("SQL Preview").size(LabelSize::Small).color(Color::Muted))
            .child(
                div()
                    .id("modify-sql-preview")
                    .w_full()
                    .max_h(px(120.))
                    .overflow_y_scroll()
                    .p_2()
                    .rounded_md()
                    .bg(cx.theme().colors().editor_background)
                    .font_family("monospace")
                    .child(Label::new(preview_text).size(LabelSize::Small)),
            )
            .when_some(self.status.clone(), |column, status| {
                column.child(Label::new(status).size(LabelSize::Small).color(Color::Muted))
            })
            .child(
                h_flex()
                    .w_full()
                    .justify_end()
                    .gap_2()
                    .child(
                        Button::new("cancel", "Cancel").on_click(cx.listener(|_, _, _, cx| {
                            cx.emit(DismissEvent);
                        })),
                    )
                    .child(
                        Button::new("execute", "Execute")
                            .style(ButtonStyle::Filled)
                            .disabled(busy)
                            .on_click(
                                cx.listener(|this, _, window, cx| this.execute(window, cx)),
                            ),
                    ),
            )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn col(name: &str, data_type: &str, nullable: bool) -> (String, String, bool) {
        (name.to_string(), data_type.to_string(), nullable)
    }

    fn draft(
        original: Option<(String, String, bool)>,
        name: &str,
        data_type: &str,
        nullable: bool,
        dropped: bool,
    ) -> DraftSnapshot {
        DraftSnapshot {
            original,
            name: name.to_string(),
            data_type: data_type.to_string(),
            nullable,
            dropped,
        }
    }

    #[test]
    fn add_column_generates_add_statement() {
        let changes = diff_changes(&[draft(None, "email", "varchar(255)", false, false)]);
        let sql = generate_alter_statements("users", DatabaseDriver::PostgreSQL, &changes);
        assert_eq!(
            sql,
            vec!["ALTER TABLE \"users\" ADD COLUMN \"email\" varchar(255) NOT NULL;"]
        );
    }

    #[test]
    fn drop_column_generates_drop_statement() {
        let changes = diff_changes(&[draft(
            Some(col("legacy", "int", true)),
            "legacy",
            "int",
            true,
            true,
        )]);
        let sql = generate_alter_statements("t", DatabaseDriver::PostgreSQL, &changes);
        assert_eq!(sql, vec!["ALTER TABLE \"t\" DROP COLUMN \"legacy\";"]);
    }

    #[test]
    fn rename_only_postgres_uses_rename_column() {
        let changes = diff_changes(&[draft(
            Some(col("old", "int", true)),
            "new",
            "int",
            true,
            false,
        )]);
        let sql = generate_alter_statements("t", DatabaseDriver::PostgreSQL, &changes);
        assert_eq!(
            sql,
            vec!["ALTER TABLE \"t\" RENAME COLUMN \"old\" TO \"new\";"]
        );
    }

    #[test]
    fn modify_type_postgres_emits_type_and_nullability() {
        let changes = diff_changes(&[draft(
            Some(col("amount", "int", true)),
            "amount",
            "bigint",
            false,
            false,
        )]);
        let sql = generate_alter_statements("t", DatabaseDriver::PostgreSQL, &changes);
        assert_eq!(
            sql,
            vec![
                "ALTER TABLE \"t\" ALTER COLUMN \"amount\" TYPE bigint;",
                "ALTER TABLE \"t\" ALTER COLUMN \"amount\" SET NOT NULL;",
            ]
        );
    }

    #[test]
    fn mysql_rename_and_modify_fold_into_change_column() {
        let changes = diff_changes(&[draft(
            Some(col("old", "int", true)),
            "new",
            "bigint",
            false,
            false,
        )]);
        let sql = generate_alter_statements("t", DatabaseDriver::MySQL, &changes);
        assert_eq!(
            sql,
            vec!["ALTER TABLE `t` CHANGE COLUMN `old` `new` bigint NOT NULL;"]
        );
    }

    #[test]
    fn mysql_modify_only_uses_modify_column() {
        let changes = diff_changes(&[draft(
            Some(col("amount", "int", false)),
            "amount",
            "bigint",
            false,
            false,
        )]);
        let sql = generate_alter_statements("t", DatabaseDriver::MySQL, &changes);
        assert_eq!(
            sql,
            vec!["ALTER TABLE `t` MODIFY COLUMN `amount` bigint NOT NULL;"]
        );
    }

    #[test]
    fn unchanged_column_yields_no_statements() {
        let changes = diff_changes(&[draft(
            Some(col("id", "int", false)),
            "id",
            "int",
            false,
            false,
        )]);
        let sql = generate_alter_statements("t", DatabaseDriver::MySQL, &changes);
        assert!(sql.is_empty());
    }
}
