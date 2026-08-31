use db_client::connection::DatabaseDriver;
use db_client::schema::{CheckConstraintInfo, ColumnInfo, FkInfo, IndexInfo};
use gpui::{
    App, ClipboardItem, Context, DismissEvent, EventEmitter, FocusHandle, Focusable, SharedString,
    Window, actions, prelude::*,
};
use std::sync::Arc;
use ui::{Divider, Icon, Tooltip, cyberpunk, prelude::*};
use workspace::{Item, item::ItemEvent};

actions!(
    db_schema_diff,
    [
        /// Copies the migration script to the clipboard.
        CopySchemaDiffScript,
        /// Opens the migration script as a runnable SQL console.
        RunSchemaDiffScript,
        /// Closes the Compare Schema view.
        CloseSchemaDiff
    ]
);

use crate::modify_table::{
    CheckChange, ColumnChange, ForeignKeyChange, IndexChange, generate_alter_statements,
    generate_check_statements, generate_foreign_key_statements, generate_index_statements,
};

/// Diffs two independently-introspected column lists by name -- unlike
/// `modify_table`'s `diff_changes`, which tracks a single table's live edits
/// against its own original state, this compares two already-settled
/// schemas, so there is no rename detection (a same-named column with a
/// different type is a `Modify`; a name that only exists on one side is an
/// unambiguous `Add`/`Drop`).
pub fn diff_columns(from: &[ColumnInfo], to: &[ColumnInfo]) -> Vec<ColumnChange> {
    let mut changes = Vec::new();
    for to_column in to {
        match from.iter().find(|column| column.name == to_column.name) {
            None => changes.push(ColumnChange::Add {
                name: to_column.name.clone(),
                data_type: to_column.data_type.clone(),
                nullable: to_column.is_nullable,
            }),
            Some(from_column)
                if from_column.data_type != to_column.data_type
                    || from_column.is_nullable != to_column.is_nullable =>
            {
                changes.push(ColumnChange::Modify {
                    name: to_column.name.clone(),
                    data_type: to_column.data_type.clone(),
                    nullable: to_column.is_nullable,
                })
            }
            Some(_) => {}
        }
    }
    for from_column in from {
        if !to.iter().any(|column| column.name == from_column.name) {
            changes.push(ColumnChange::Drop {
                name: from_column.name.clone(),
            });
        }
    }
    changes
}

/// Diffs two index lists by name. An index present on both sides with a
/// different column list or uniqueness is reported as a `Drop` of the old
/// definition plus an `Add` of the new one -- no dialect supports modifying
/// an index in place, matching `modify_table`'s own editor semantics.
pub fn diff_indexes(from: &[IndexInfo], to: &[IndexInfo]) -> Vec<IndexChange> {
    let mut changes = Vec::new();
    for to_index in to {
        match from.iter().find(|index| index.name == to_index.name) {
            None => changes.push(IndexChange::Add {
                name: to_index.name.clone(),
                columns: to_index.columns.clone(),
                unique: to_index.unique,
            }),
            Some(from_index)
                if from_index.columns != to_index.columns
                    || from_index.unique != to_index.unique =>
            {
                changes.push(IndexChange::Drop {
                    name: from_index.name.clone(),
                });
                changes.push(IndexChange::Add {
                    name: to_index.name.clone(),
                    columns: to_index.columns.clone(),
                    unique: to_index.unique,
                });
            }
            Some(_) => {}
        }
    }
    for from_index in from {
        if !to.iter().any(|index| index.name == from_index.name) {
            changes.push(IndexChange::Drop {
                name: from_index.name.clone(),
            });
        }
    }
    changes
}

/// Diffs two foreign-key lists by name, same drop-and-readd treatment for a
/// same-named FK whose definition changed as `diff_indexes` uses.
pub fn diff_foreign_keys(from: &[FkInfo], to: &[FkInfo]) -> Vec<ForeignKeyChange> {
    let mut changes = Vec::new();
    for to_fk in to {
        match from.iter().find(|fk| fk.name == to_fk.name) {
            None => changes.push(ForeignKeyChange::Add {
                name: to_fk.name.clone(),
                from_column: to_fk.from_column.clone(),
                to_table: to_fk.to_table.clone(),
                to_column: to_fk.to_column.clone(),
            }),
            Some(from_fk)
                if from_fk.from_column != to_fk.from_column
                    || from_fk.to_table != to_fk.to_table
                    || from_fk.to_column != to_fk.to_column =>
            {
                changes.push(ForeignKeyChange::Drop {
                    name: from_fk.name.clone(),
                });
                changes.push(ForeignKeyChange::Add {
                    name: to_fk.name.clone(),
                    from_column: to_fk.from_column.clone(),
                    to_table: to_fk.to_table.clone(),
                    to_column: to_fk.to_column.clone(),
                });
            }
            Some(_) => {}
        }
    }
    for from_fk in from {
        if !to.iter().any(|fk| fk.name == from_fk.name) {
            changes.push(ForeignKeyChange::Drop {
                name: from_fk.name.clone(),
            });
        }
    }
    changes
}

/// Diffs two check-constraint lists by name, same drop-and-readd treatment
/// for a same-named check whose expression changed as `diff_indexes` uses.
pub fn diff_checks(from: &[CheckConstraintInfo], to: &[CheckConstraintInfo]) -> Vec<CheckChange> {
    let mut changes = Vec::new();
    for to_check in to {
        match from.iter().find(|check| check.name == to_check.name) {
            None => changes.push(CheckChange::Add {
                name: to_check.name.clone(),
                expression: to_check.expression.clone(),
            }),
            Some(from_check) if from_check.expression != to_check.expression => {
                changes.push(CheckChange::Drop {
                    name: from_check.name.clone(),
                });
                changes.push(CheckChange::Add {
                    name: to_check.name.clone(),
                    expression: to_check.expression.clone(),
                });
            }
            Some(_) => {}
        }
    }
    for from_check in from {
        if !to.iter().any(|check| check.name == from_check.name) {
            changes.push(CheckChange::Drop {
                name: from_check.name.clone(),
            });
        }
    }
    changes
}

/// The full structural diff between two tables' introspected schemas,
/// direction "from" (the target being altered) -> "to" (the desired shape).
#[derive(Debug, Clone, PartialEq)]
pub struct SchemaDiff {
    pub column_changes: Vec<ColumnChange>,
    pub index_changes: Vec<IndexChange>,
    pub fk_changes: Vec<ForeignKeyChange>,
    pub check_changes: Vec<CheckChange>,
}

/// One table's full structural snapshot, as introspected from a live
/// connection.
#[derive(Debug, Clone, Default)]
pub struct TableSchema {
    pub columns: Vec<ColumnInfo>,
    pub indexes: Vec<IndexInfo>,
    pub foreign_keys: Vec<FkInfo>,
    pub checks: Vec<CheckConstraintInfo>,
}

impl SchemaDiff {
    pub fn compute(from: &TableSchema, to: &TableSchema) -> Self {
        Self {
            column_changes: diff_columns(&from.columns, &to.columns),
            index_changes: diff_indexes(&from.indexes, &to.indexes),
            fk_changes: diff_foreign_keys(&from.foreign_keys, &to.foreign_keys),
            check_changes: diff_checks(&from.checks, &to.checks),
        }
    }

    pub fn is_empty(&self) -> bool {
        self.column_changes.is_empty()
            && self.index_changes.is_empty()
            && self.fk_changes.is_empty()
            && self.check_changes.is_empty()
    }

    /// The migration script that brings `table` (the "from" side) in line
    /// with the "to" side this diff was computed against, reusing the exact
    /// same per-dialect DDL generators Modify Table's editors use. Each
    /// statement is tagged with the object kind it came from so a UI can
    /// render a per-line marker without re-deriving the script a second time.
    pub fn categorized_script(
        &self,
        table: &str,
        driver: DatabaseDriver,
    ) -> Vec<(ScriptLineKind, String)> {
        let mut lines: Vec<(ScriptLineKind, String)> =
            generate_alter_statements(table, driver, &self.column_changes)
                .into_iter()
                .map(|statement| (ScriptLineKind::Column, statement))
                .collect();
        lines.extend(
            generate_index_statements(table, driver, &self.index_changes)
                .into_iter()
                .map(|statement| (ScriptLineKind::Index, statement)),
        );
        lines.extend(
            generate_foreign_key_statements(table, driver, &self.fk_changes)
                .into_iter()
                .map(|statement| (ScriptLineKind::ForeignKey, statement)),
        );
        lines.extend(
            generate_check_statements(table, driver, &self.check_changes)
                .into_iter()
                .map(|statement| (ScriptLineKind::Check, statement)),
        );
        lines
    }
}

/// A single line of the migration script, tagged by which object kind it
/// came from purely so the view can render a small colored marker -- the
/// statement text itself is already the authoritative content.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum ScriptLineKind {
    Column,
    Index,
    ForeignKey,
    Check,
}

/// Shows the structural diff between two tables as the migration script that
/// would bring the "from" table in line with the "to" table, with actions to
/// copy the script or open it as a real, runnable SQL console against the
/// "from" table's connection -- reusing the console's own execution path
/// rather than adding a second way to run SQL.
pub struct SchemaDiffView {
    focus_handle: FocusHandle,
    title: SharedString,
    lines: Vec<(ScriptLineKind, String)>,
    is_empty: bool,
    on_run: Arc<dyn Fn(String, &mut Window, &mut App)>,
}

impl SchemaDiffView {
    pub fn new(
        diff: &SchemaDiff,
        table: &str,
        driver: DatabaseDriver,
        title: SharedString,
        on_run: Arc<dyn Fn(String, &mut Window, &mut App)>,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let is_empty = diff.is_empty();
        let lines = diff.categorized_script(table, driver);
        Self {
            focus_handle: cx.focus_handle(),
            title,
            is_empty,
            lines,
            on_run,
        }
    }

    pub fn script_text(&self) -> String {
        self.lines
            .iter()
            .map(|(_, statement)| statement.clone())
            .collect::<Vec<_>>()
            .join("\n")
    }
}

impl EventEmitter<DismissEvent> for SchemaDiffView {}

impl Focusable for SchemaDiffView {
    fn focus_handle(&self, _cx: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl Item for SchemaDiffView {
    type Event = DismissEvent;

    fn tab_content_text(&self, _detail: usize, _cx: &App) -> SharedString {
        self.title.clone()
    }

    fn tab_icon(&self, _window: &Window, _cx: &App) -> Option<Icon> {
        Some(Icon::new(IconName::Diff))
    }

    fn to_item_events(_event: &Self::Event, f: &mut dyn FnMut(ItemEvent)) {
        f(ItemEvent::CloseItem);
    }
}

impl Render for SchemaDiffView {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let is_empty = self.is_empty;

        let rows: Vec<_> = self
            .lines
            .iter()
            .map(|(kind, statement)| {
                let (marker, color) = match kind {
                    ScriptLineKind::Column => ("COL", Color::Accent),
                    ScriptLineKind::Index => ("IDX", Color::Info),
                    ScriptLineKind::ForeignKey => ("FK", Color::Warning),
                    ScriptLineKind::Check => ("CHK", Color::Success),
                };
                h_flex()
                    .w_full()
                    .gap_2()
                    .px_1()
                    .items_start()
                    .child(
                        div()
                            .w(px(36.))
                            .flex_none()
                            .child(Label::new(marker).size(LabelSize::XSmall).color(color)),
                    )
                    .child(
                        div()
                            .flex_1()
                            .child(Label::new(statement.clone()).size(LabelSize::Small)),
                    )
                    .into_any_element()
            })
            .collect();

        v_flex()
            .key_context("SchemaDiff")
            .track_focus(&self.focus_handle)
            .size_full()
            .bg(cx.theme().colors().editor_background)
            .p_3()
            .gap_2()
            .on_action(cx.listener(|this, _: &CopySchemaDiffScript, _window, cx| {
                if !this.is_empty {
                    cx.write_to_clipboard(ClipboardItem::new_string(this.script_text()));
                }
            }))
            .on_action(cx.listener(|this, _: &RunSchemaDiffScript, window, cx| {
                if !this.is_empty {
                    (this.on_run.clone())(this.script_text(), window, cx);
                }
            }))
            .on_action(cx.listener(|_, _: &CloseSchemaDiff, _window, cx| cx.emit(DismissEvent)))
            .child(
                h_flex()
                    .w_full()
                    .justify_between()
                    .items_center()
                    .child(
                        h_flex()
                            .gap_3()
                            .items_center()
                            .child(Label::new("Compare Schema").size(LabelSize::Large))
                            .child(
                                Label::new(format!("{} statement(s)", self.lines.len()))
                                    .size(LabelSize::Small)
                                    .color(Color::Muted),
                            ),
                    )
                    .child(
                        h_flex()
                            .gap_2()
                            .child(
                                Button::new("schema-diff-copy", "Copy Script")
                                    .style(cyberpunk::Rank::Quiet.style())
                                    .disabled(is_empty)
                                    .on_click(cx.listener(move |this, _, _, cx| {
                                        cx.write_to_clipboard(ClipboardItem::new_string(
                                            this.script_text(),
                                        ));
                                    })),
                            )
                            .child(
                                Button::new("schema-diff-run", "Run Script…")
                                    .style(cyberpunk::Rank::Accent.style())
                                    .disabled(is_empty)
                                    .on_click(cx.listener(move |this, _, window, cx| {
                                        (this.on_run.clone())(this.script_text(), window, cx);
                                    })),
                            )
                            .child(
                                IconButton::new("schema-diff-close", IconName::Close)
                                    .style(cyberpunk::Rank::Neutral.style())
                                    .tooltip(Tooltip::text("Close"))
                                    .on_click(cx.listener(|_, _, _, cx| cx.emit(DismissEvent))),
                            ),
                    ),
            )
            .child(Divider::horizontal())
            .when(is_empty, |column| {
                column.child(
                    Label::new("No structural differences found")
                        .size(LabelSize::Small)
                        .color(Color::Muted),
                )
            })
            .when(!is_empty, |column| {
                column.child(
                    v_flex()
                        .id("schema-diff-rows")
                        .gap_0p5()
                        .flex_1()
                        .overflow_y_scroll()
                        .children(rows),
                )
            })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn column(name: &str, data_type: &str, nullable: bool) -> ColumnInfo {
        ColumnInfo {
            name: name.to_string(),
            data_type: data_type.to_string(),
            is_nullable: nullable,
            column_key: None,
            default_value: None,
            extra: String::new(),
        }
    }

    fn index(name: &str, columns: &[&str], unique: bool) -> IndexInfo {
        IndexInfo {
            name: name.to_string(),
            columns: columns.iter().map(|c| c.to_string()).collect(),
            unique,
            index_type: "BTREE".to_string(),
        }
    }

    fn fk(name: &str, from_column: &str, to_table: &str, to_column: &str) -> FkInfo {
        FkInfo {
            name: name.to_string(),
            from_column: from_column.to_string(),
            to_table: to_table.to_string(),
            to_column: to_column.to_string(),
        }
    }

    fn check(name: &str, expression: &str) -> CheckConstraintInfo {
        CheckConstraintInfo {
            name: name.to_string(),
            expression: expression.to_string(),
        }
    }

    #[test]
    fn diff_columns_detects_add_drop_and_modify() {
        let from = vec![
            column("id", "int", false),
            column("legacy_flag", "tinyint", true),
            column("name", "varchar(50)", true),
        ];
        let to = vec![
            column("id", "int", false),
            column("name", "varchar(255)", true),
            column("email", "varchar(255)", false),
        ];

        let changes = diff_columns(&from, &to);
        assert_eq!(changes.len(), 3);
        assert!(changes.contains(&ColumnChange::Add {
            name: "email".to_string(),
            data_type: "varchar(255)".to_string(),
            nullable: false,
        }));
        assert!(changes.contains(&ColumnChange::Drop {
            name: "legacy_flag".to_string(),
        }));
        assert!(changes.contains(&ColumnChange::Modify {
            name: "name".to_string(),
            data_type: "varchar(255)".to_string(),
            nullable: true,
        }));
    }

    #[test]
    fn diff_columns_reports_nothing_for_identical_schemas() {
        let columns = vec![column("id", "int", false)];
        assert!(diff_columns(&columns, &columns).is_empty());
    }

    #[test]
    fn diff_indexes_drops_and_readds_a_changed_index() {
        let from = vec![index("idx_email", &["email"], false)];
        let to = vec![index("idx_email", &["email"], true)];

        let changes = diff_indexes(&from, &to);
        assert_eq!(
            changes,
            vec![
                IndexChange::Drop {
                    name: "idx_email".to_string()
                },
                IndexChange::Add {
                    name: "idx_email".to_string(),
                    columns: vec!["email".to_string()],
                    unique: true,
                },
            ]
        );
    }

    #[test]
    fn diff_foreign_keys_detects_add_and_drop() {
        let from = vec![fk("fk_old", "customer_id", "customers", "id")];
        let to = vec![fk("fk_new", "customer_id", "accounts", "id")];

        let changes = diff_foreign_keys(&from, &to);
        assert_eq!(
            changes,
            vec![
                ForeignKeyChange::Add {
                    name: "fk_new".to_string(),
                    from_column: "customer_id".to_string(),
                    to_table: "accounts".to_string(),
                    to_column: "id".to_string(),
                },
                ForeignKeyChange::Drop {
                    name: "fk_old".to_string()
                },
            ]
        );
    }

    #[test]
    fn diff_checks_detects_changed_expression() {
        let from = vec![check("chk_age", "age >= 0")];
        let to = vec![check("chk_age", "age >= 18")];

        let changes = diff_checks(&from, &to);
        assert_eq!(
            changes,
            vec![
                CheckChange::Drop {
                    name: "chk_age".to_string()
                },
                CheckChange::Add {
                    name: "chk_age".to_string(),
                    expression: "age >= 18".to_string(),
                },
            ]
        );
    }

    #[test]
    fn categorized_script_reuses_modify_table_generators_and_orders_by_object_kind() {
        let from = TableSchema {
            columns: vec![column("id", "int", false)],
            indexes: Vec::new(),
            foreign_keys: Vec::new(),
            checks: Vec::new(),
        };
        let to = TableSchema {
            columns: vec![
                column("id", "int", false),
                column("email", "varchar(255)", false),
            ],
            indexes: vec![index("idx_email", &["email"], true)],
            foreign_keys: vec![fk("fk_customer", "customer_id", "customers", "id")],
            checks: vec![check("chk_email", "email <> ''")],
        };

        let diff = SchemaDiff::compute(&from, &to);
        assert!(!diff.is_empty());
        let script: Vec<String> = diff
            .categorized_script("users", DatabaseDriver::PostgreSQL)
            .into_iter()
            .map(|(_, statement)| statement)
            .collect();
        assert_eq!(
            script,
            vec![
                "ALTER TABLE \"users\" ADD COLUMN \"email\" varchar(255) NOT NULL;".to_string(),
                "CREATE UNIQUE INDEX \"idx_email\" ON \"users\" (\"email\");".to_string(),
                "ALTER TABLE \"users\" ADD CONSTRAINT \"fk_customer\" FOREIGN KEY (\"customer_id\") REFERENCES \"customers\" (\"id\");".to_string(),
                "ALTER TABLE \"users\" ADD CONSTRAINT \"chk_email\" CHECK (email <> '');".to_string(),
            ]
        );
    }

    // End-to-end proof that `categorized_script` -- schema diff's actual
    // entry point -- emits statements ClickHouse accepts: no `NOT NULL`
    // clause on the new column, no `UNIQUE` keyword and a `TYPE` on the new
    // index, and the foreign-key change dropped entirely (ClickHouse has no
    // FK concept, matching how SQLite's FK/CHECK generators are already
    // gated). All verified against a live ClickHouse 24.10 instance.
    #[test]
    fn categorized_script_emits_valid_clickhouse_statements_and_drops_the_unsupported_fk_change() {
        let from = TableSchema {
            columns: vec![column("id", "UInt32", false)],
            indexes: Vec::new(),
            foreign_keys: Vec::new(),
            checks: Vec::new(),
        };
        let to = TableSchema {
            columns: vec![
                column("id", "UInt32", false),
                column("email", "String", false),
            ],
            indexes: vec![index("idx_email", &["email"], true)],
            foreign_keys: vec![fk("fk_customer", "customer_id", "customers", "id")],
            checks: vec![check("chk_email", "email <> ''")],
        };

        let diff = SchemaDiff::compute(&from, &to);
        let script: Vec<String> = diff
            .categorized_script("users", DatabaseDriver::ClickHouse)
            .into_iter()
            .map(|(_, statement)| statement)
            .collect();
        assert_eq!(
            script,
            vec![
                "ALTER TABLE \"users\" ADD COLUMN \"email\" String;".to_string(),
                "CREATE INDEX \"idx_email\" ON \"users\" (\"email\") TYPE minmax;".to_string(),
                "ALTER TABLE \"users\" ADD CONSTRAINT \"chk_email\" CHECK (email <> '');"
                    .to_string(),
            ]
        );
    }

    #[test]
    fn identical_schemas_produce_an_empty_diff_and_script() {
        let schema = TableSchema {
            columns: vec![column("id", "int", false)],
            indexes: vec![index("idx_id", &["id"], true)],
            foreign_keys: Vec::new(),
            checks: Vec::new(),
        };
        let diff = SchemaDiff::compute(&schema, &schema);
        assert!(diff.is_empty());
        assert!(
            diff.categorized_script("t", DatabaseDriver::MySQL)
                .is_empty()
        );
    }
}
