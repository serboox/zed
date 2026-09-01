use db_client::connection::{ConnectionId, DatabaseDriver};
use db_client::schema::{CheckConstraintInfo, ColumnInfo, FkInfo, IndexInfo};
use editor::Editor;
use gpui::{
    Context, DismissEvent, Entity, EventEmitter, FocusHandle, Focusable, PromptLevel, Window,
    prelude::*,
};
use ui::{Checkbox, Divider, ElevationIndex, Tooltip, cyberpunk, prelude::*};
use util::ResultExt;
use workspace::ModalView;

use crate::store::DatabaseStore;

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
/// `RENAME COLUMN` / `ALTER COLUMN` forms (best effort for SQLite, which
/// supports a subset). ClickHouse reuses this same `ADD`/`DROP`/`RENAME
/// COLUMN` and `ALTER COLUMN ... TYPE` wording -- verified against a live
/// ClickHouse 24.10 instance -- but never takes a `NULL`/`NOT NULL` clause:
/// nullability lives in the type itself (`Nullable(T)`), so `supports_null_clause`
/// suppresses it for ClickHouse alone.
pub fn generate_alter_statements(
    table: &str,
    driver: DatabaseDriver,
    changes: &[ColumnChange],
) -> Vec<String> {
    if driver == DatabaseDriver::Cassandra {
        return generate_cassandra_alter_statements(table, changes);
    }
    let table_ident = driver.quote_identifier(table);
    let supports_null_clause = driver != DatabaseDriver::ClickHouse;
    let null_clause = |nullable: bool| if nullable { "NULL" } else { "NOT NULL" };
    let mut statements = Vec::new();
    for change in changes {
        match change {
            ColumnChange::Add {
                name,
                data_type,
                nullable,
            } => {
                statements.push(if supports_null_clause {
                    format!(
                        "ALTER TABLE {table_ident} ADD COLUMN {} {} {};",
                        driver.quote_identifier(name),
                        data_type,
                        null_clause(*nullable)
                    )
                } else {
                    format!(
                        "ALTER TABLE {table_ident} ADD COLUMN {} {};",
                        driver.quote_identifier(name),
                        data_type
                    )
                });
            }
            ColumnChange::Drop { name } => {
                statements.push(format!(
                    "ALTER TABLE {table_ident} DROP COLUMN {};",
                    driver.quote_identifier(name)
                ));
            }
            ColumnChange::Rename { from, to } => {
                if driver != DatabaseDriver::MySQL {
                    statements.push(format!(
                        "ALTER TABLE {table_ident} RENAME COLUMN {} TO {};",
                        driver.quote_identifier(from),
                        driver.quote_identifier(to)
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
                    driver.quote_identifier(name),
                    data_type,
                    null_clause(*nullable)
                )),
                DatabaseDriver::ClickHouse => statements.push(format!(
                    "ALTER TABLE {table_ident} ALTER COLUMN {} TYPE {};",
                    driver.quote_identifier(name),
                    data_type
                )),
                _ => {
                    statements.push(format!(
                        "ALTER TABLE {table_ident} ALTER COLUMN {} TYPE {};",
                        driver.quote_identifier(name),
                        data_type
                    ));
                    let null_op = if *nullable {
                        "DROP NOT NULL"
                    } else {
                        "SET NOT NULL"
                    };
                    statements.push(format!(
                        "ALTER TABLE {table_ident} ALTER COLUMN {} {null_op};",
                        driver.quote_identifier(name)
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

/// CQL's `ALTER TABLE` has no `COLUMN` keyword and no per-column `NULL`/`NOT
/// NULL` clause (Cassandra has no column-level nullability constraint), and
/// its `RENAME`/`ALTER ... TYPE` forms drop the `COLUMN`/`ALTER COLUMN`
/// wording the generic dialect above uses — reusing that branch for
/// Cassandra would emit statements CQL rejects outright.
fn generate_cassandra_alter_statements(table: &str, changes: &[ColumnChange]) -> Vec<String> {
    let table_ident = DatabaseDriver::Cassandra.quote_identifier(table);
    changes
        .iter()
        .map(|change| match change {
            ColumnChange::Add {
                name, data_type, ..
            } => format!(
                "ALTER TABLE {table_ident} ADD {} {};",
                DatabaseDriver::Cassandra.quote_identifier(name),
                data_type
            ),
            ColumnChange::Drop { name } => format!(
                "ALTER TABLE {table_ident} DROP {};",
                DatabaseDriver::Cassandra.quote_identifier(name)
            ),
            ColumnChange::Rename { from, to } => format!(
                "ALTER TABLE {table_ident} RENAME {} TO {};",
                DatabaseDriver::Cassandra.quote_identifier(from),
                DatabaseDriver::Cassandra.quote_identifier(to)
            ),
            ColumnChange::Modify {
                name, data_type, ..
            } => format!(
                "ALTER TABLE {table_ident} ALTER {} TYPE {};",
                DatabaseDriver::Cassandra.quote_identifier(name),
                data_type
            ),
        })
        .collect()
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
                    driver.quote_identifier(from),
                    driver.quote_identifier(to),
                    data_type,
                    null_clause(nullable)
                ));
            } else {
                statements.push(format!(
                    "ALTER TABLE {table_ident} RENAME COLUMN {} TO {};",
                    driver.quote_identifier(from),
                    driver.quote_identifier(to)
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
                driver.quote_identifier(name),
                data_type,
                null_clause(*nullable)
            )),
            ColumnChange::Drop { name } => statements.push(format!(
                "ALTER TABLE {table_ident} DROP COLUMN {};",
                driver.quote_identifier(name)
            )),
            ColumnChange::Modify {
                name,
                data_type,
                nullable,
            } if !handled_modify_for.contains(name) => statements.push(format!(
                "ALTER TABLE {table_ident} MODIFY COLUMN {} {} {};",
                driver.quote_identifier(name),
                data_type,
                null_clause(*nullable)
            )),
            _ => {}
        }
    }
    statements
}

#[derive(Debug, Clone, PartialEq)]
pub enum IndexChange {
    Add {
        name: String,
        columns: Vec<String>,
        unique: bool,
    },
    Drop {
        name: String,
    },
}

#[derive(Debug, Clone, PartialEq)]
pub enum ForeignKeyChange {
    Add {
        name: String,
        from_column: String,
        to_table: String,
        to_column: String,
    },
    Drop {
        name: String,
    },
}

#[derive(Debug, Clone, PartialEq)]
pub enum CheckChange {
    Add { name: String, expression: String },
    Drop { name: String },
}

#[derive(Debug, Clone, PartialEq)]
struct IndexDraftSnapshot {
    original_name: Option<String>,
    name: String,
    columns_csv: String,
    unique: bool,
    dropped: bool,
}

fn index_diff_changes(drafts: &[IndexDraftSnapshot]) -> Vec<IndexChange> {
    let mut changes = Vec::new();
    for draft in drafts {
        match &draft.original_name {
            None if !draft.dropped && !draft.name.trim().is_empty() => {
                let columns: Vec<String> = draft
                    .columns_csv
                    .split(',')
                    .map(|s| s.trim().to_string())
                    .filter(|s| !s.is_empty())
                    .collect();
                if !columns.is_empty() {
                    changes.push(IndexChange::Add {
                        name: draft.name.trim().to_string(),
                        columns,
                        unique: draft.unique,
                    });
                }
            }
            // Editing an existing index's columns or uniqueness in place isn't
            // supported by any dialect directly; drop it and add a
            // replacement instead.
            Some(original_name) if draft.dropped => {
                changes.push(IndexChange::Drop {
                    name: original_name.clone(),
                });
            }
            _ => {}
        }
    }
    changes
}

pub fn generate_index_statements(
    table: &str,
    driver: DatabaseDriver,
    changes: &[IndexChange],
) -> Vec<String> {
    if driver == DatabaseDriver::Cassandra {
        return generate_cassandra_index_statements(table, changes);
    }
    if driver == DatabaseDriver::ClickHouse {
        return generate_clickhouse_index_statements(table, changes);
    }
    let table_ident = driver.quote_identifier(table);
    changes
        .iter()
        .map(|change| match change {
            IndexChange::Add {
                name,
                columns,
                unique,
            } => {
                let cols = columns
                    .iter()
                    .map(|c| driver.quote_identifier(c))
                    .collect::<Vec<_>>()
                    .join(", ");
                let unique_kw = if *unique { "UNIQUE " } else { "" };
                format!(
                    "CREATE {unique_kw}INDEX {} ON {table_ident} ({cols});",
                    driver.quote_identifier(name)
                )
            }
            IndexChange::Drop { name } => match driver {
                DatabaseDriver::MySQL => format!(
                    "ALTER TABLE {table_ident} DROP INDEX {};",
                    driver.quote_identifier(name)
                ),
                _ => format!("DROP INDEX {};", driver.quote_identifier(name)),
            },
        })
        .collect()
}

/// CQL's `CREATE INDEX` has no `UNIQUE` modifier (Cassandra secondary
/// indexes carry no uniqueness concept) and only ever indexes a single
/// column, so a multi-column request becomes one `CREATE INDEX` per column
/// rather than the single composite-index statement the generic SQL branch
/// above would emit. `DROP INDEX name;` (no `ON table`) is already valid CQL
/// and identical to the generic non-MySQL branch, so it's reused as-is.
fn generate_cassandra_index_statements(table: &str, changes: &[IndexChange]) -> Vec<String> {
    let table_ident = DatabaseDriver::Cassandra.quote_identifier(table);
    changes
        .iter()
        .flat_map(|change| -> Vec<String> {
            match change {
                IndexChange::Add { name, columns, .. } => columns
                    .iter()
                    .enumerate()
                    .map(|(position, column)| {
                        let index_name = if columns.len() == 1 {
                            name.clone()
                        } else {
                            format!("{name}_{position}")
                        };
                        format!(
                            "CREATE INDEX {} ON {table_ident} ({});",
                            DatabaseDriver::Cassandra.quote_identifier(&index_name),
                            DatabaseDriver::Cassandra.quote_identifier(column)
                        )
                    })
                    .collect(),
                IndexChange::Drop { name } => vec![format!(
                    "DROP INDEX {};",
                    DatabaseDriver::Cassandra.quote_identifier(name)
                )],
            }
        })
        .collect()
}

/// ClickHouse secondary indexes are data-skipping indexes, not uniqueness
/// constraints, so `CREATE UNIQUE INDEX` isn't a thing there -- it's rejected
/// outright (`CREATE UNIQUE INDEX is not supported`, verified against a live
/// ClickHouse 24.10 instance) -- and `CREATE INDEX` requires an explicit
/// `TYPE`, unlike the generic branch above which supplies neither. `minmax`
/// is used as the type since it accepts any orderable column and `IndexInfo`
/// carries no ClickHouse-specific index-type hint to pick a narrower one
/// from. Its `DROP INDEX` form is `ALTER TABLE ... DROP INDEX name`, matching
/// MySQL's `ALTER TABLE ... DROP INDEX` rather than the generic non-MySQL
/// `DROP INDEX name;` form, which ClickHouse's parser rejects for wanting an
/// `ON table` clause it doesn't otherwise accept in that position.
fn generate_clickhouse_index_statements(table: &str, changes: &[IndexChange]) -> Vec<String> {
    let table_ident = DatabaseDriver::ClickHouse.quote_identifier(table);
    changes
        .iter()
        .map(|change| match change {
            IndexChange::Add { name, columns, .. } => {
                let cols = columns
                    .iter()
                    .map(|c| DatabaseDriver::ClickHouse.quote_identifier(c))
                    .collect::<Vec<_>>()
                    .join(", ");
                format!(
                    "CREATE INDEX {} ON {table_ident} ({cols}) TYPE minmax;",
                    DatabaseDriver::ClickHouse.quote_identifier(name)
                )
            }
            IndexChange::Drop { name } => format!(
                "ALTER TABLE {table_ident} DROP INDEX {};",
                DatabaseDriver::ClickHouse.quote_identifier(name)
            ),
        })
        .collect()
}

#[derive(Debug, Clone, PartialEq)]
struct FkDraftSnapshot {
    original_name: Option<String>,
    name: String,
    from_column: String,
    to_table: String,
    to_column: String,
    dropped: bool,
}

fn fk_diff_changes(drafts: &[FkDraftSnapshot]) -> Vec<ForeignKeyChange> {
    let mut changes = Vec::new();
    for draft in drafts {
        match &draft.original_name {
            None if !draft.dropped
                && !draft.name.trim().is_empty()
                && !draft.from_column.trim().is_empty()
                && !draft.to_table.trim().is_empty()
                && !draft.to_column.trim().is_empty() =>
            {
                changes.push(ForeignKeyChange::Add {
                    name: draft.name.trim().to_string(),
                    from_column: draft.from_column.trim().to_string(),
                    to_table: draft.to_table.trim().to_string(),
                    to_column: draft.to_column.trim().to_string(),
                });
            }
            Some(original_name) if draft.dropped => {
                changes.push(ForeignKeyChange::Drop {
                    name: original_name.clone(),
                });
            }
            _ => {}
        }
    }
    changes
}

// SQLite can only declare foreign keys at table-creation time; adding or
// dropping one on an existing table would require recreating the table,
// which is out of scope here, so it emits no statements for SQLite.
// ClickHouse has no foreign-key concept at all: `ADD CONSTRAINT ... FOREIGN
// KEY` is a syntax error there (verified against a live ClickHouse 24.10
// instance -- its `ADD CONSTRAINT` only accepts `CHECK`/`ASSUME`), so it also
// emits no statements.
pub fn generate_foreign_key_statements(
    table: &str,
    driver: DatabaseDriver,
    changes: &[ForeignKeyChange],
) -> Vec<String> {
    if matches!(driver, DatabaseDriver::SQLite | DatabaseDriver::ClickHouse) {
        return Vec::new();
    }
    let table_ident = driver.quote_identifier(table);
    changes
        .iter()
        .map(|change| match change {
            ForeignKeyChange::Add {
                name,
                from_column,
                to_table,
                to_column,
            } => format!(
                "ALTER TABLE {table_ident} ADD CONSTRAINT {} FOREIGN KEY ({}) REFERENCES {} ({});",
                driver.quote_identifier(name),
                driver.quote_identifier(from_column),
                driver.quote_identifier(to_table),
                driver.quote_identifier(to_column),
            ),
            ForeignKeyChange::Drop { name } => match driver {
                DatabaseDriver::MySQL => format!(
                    "ALTER TABLE {table_ident} DROP FOREIGN KEY {};",
                    driver.quote_identifier(name)
                ),
                _ => format!(
                    "ALTER TABLE {table_ident} DROP CONSTRAINT {};",
                    driver.quote_identifier(name)
                ),
            },
        })
        .collect()
}

#[derive(Debug, Clone, PartialEq)]
struct CheckDraftSnapshot {
    original_name: Option<String>,
    name: String,
    expression: String,
    dropped: bool,
}

fn check_diff_changes(drafts: &[CheckDraftSnapshot]) -> Vec<CheckChange> {
    let mut changes = Vec::new();
    for draft in drafts {
        match &draft.original_name {
            None if !draft.dropped
                && !draft.name.trim().is_empty()
                && !draft.expression.trim().is_empty() =>
            {
                changes.push(CheckChange::Add {
                    name: draft.name.trim().to_string(),
                    expression: draft.expression.trim().to_string(),
                });
            }
            Some(original_name) if draft.dropped => {
                changes.push(CheckChange::Drop {
                    name: original_name.clone(),
                });
            }
            _ => {}
        }
    }
    changes
}

// SQLite cannot add or drop a CHECK constraint on an existing table without
// recreating it, which is out of scope here, so it emits no statements for
// SQLite.
pub fn generate_check_statements(
    table: &str,
    driver: DatabaseDriver,
    changes: &[CheckChange],
) -> Vec<String> {
    if driver == DatabaseDriver::SQLite {
        return Vec::new();
    }
    let table_ident = driver.quote_identifier(table);
    changes
        .iter()
        .map(|change| match change {
            CheckChange::Add { name, expression } => format!(
                "ALTER TABLE {table_ident} ADD CONSTRAINT {} CHECK ({expression});",
                driver.quote_identifier(name)
            ),
            CheckChange::Drop { name } => match driver {
                DatabaseDriver::MySQL => format!(
                    "ALTER TABLE {table_ident} DROP CHECK {};",
                    driver.quote_identifier(name)
                ),
                _ => format!(
                    "ALTER TABLE {table_ident} DROP CONSTRAINT {};",
                    driver.quote_identifier(name)
                ),
            },
        })
        .collect()
}

#[derive(Clone)]
struct IndexDraft {
    original: Option<IndexInfo>,
    name: Entity<Editor>,
    columns: Entity<Editor>,
    unique: bool,
    dropped: bool,
}

#[derive(Clone)]
struct FkDraft {
    original: Option<FkInfo>,
    name: Entity<Editor>,
    from_column: Entity<Editor>,
    to_table: Entity<Editor>,
    to_column: Entity<Editor>,
    dropped: bool,
}

#[derive(Clone)]
struct CheckDraft {
    original: Option<CheckConstraintInfo>,
    name: Entity<Editor>,
    expression: Entity<Editor>,
    dropped: bool,
}

pub struct ModifyTableView {
    focus_handle: FocusHandle,
    store: Entity<DatabaseStore>,
    connection_id: ConnectionId,
    driver: DatabaseDriver,
    database: String,
    table: String,
    drafts: Vec<ColumnDraft>,
    index_drafts: Vec<IndexDraft>,
    fk_drafts: Vec<FkDraft>,
    check_drafts: Vec<CheckDraft>,
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
        indexes: &[IndexInfo],
        foreign_keys: &[FkInfo],
        checks: &[CheckConstraintInfo],
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let drafts = columns
            .iter()
            .map(|column| Self::draft_from_column(column.clone(), window, cx))
            .collect();
        let index_drafts = indexes
            .iter()
            .map(|index| Self::index_draft_from_info(index.clone(), window, cx))
            .collect();
        let fk_drafts = foreign_keys
            .iter()
            .map(|fk| Self::fk_draft_from_info(fk.clone(), window, cx))
            .collect();
        let check_drafts = checks
            .iter()
            .map(|check| Self::check_draft_from_info(check.clone(), window, cx))
            .collect();
        Self {
            focus_handle: cx.focus_handle(),
            store,
            connection_id,
            driver,
            database,
            table,
            drafts,
            index_drafts,
            fk_drafts,
            check_drafts,
            status: None,
            busy: false,
        }
    }

    fn index_draft_from_info(
        index: IndexInfo,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> IndexDraft {
        let name = cx.new(|cx| {
            let mut editor = Editor::single_line(window, cx);
            editor.set_text(index.name.clone(), window, cx);
            editor
        });
        let columns = cx.new(|cx| {
            let mut editor = Editor::single_line(window, cx);
            editor.set_text(index.columns.join(", "), window, cx);
            editor
        });
        let unique = index.unique;
        IndexDraft {
            original: Some(index),
            name,
            columns,
            unique,
            dropped: false,
        }
    }

    fn add_blank_index(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let name = cx.new(|cx| {
            let mut editor = Editor::single_line(window, cx);
            editor.set_placeholder_text("index_name", window, cx);
            editor
        });
        let columns = cx.new(|cx| {
            let mut editor = Editor::single_line(window, cx);
            editor.set_placeholder_text("column1, column2", window, cx);
            editor
        });
        self.index_drafts.push(IndexDraft {
            original: None,
            name,
            columns,
            unique: false,
            dropped: false,
        });
        cx.notify();
    }

    fn fk_draft_from_info(fk: FkInfo, window: &mut Window, cx: &mut Context<Self>) -> FkDraft {
        let name = cx.new(|cx| {
            let mut editor = Editor::single_line(window, cx);
            editor.set_text(fk.name.clone(), window, cx);
            editor
        });
        let from_column = cx.new(|cx| {
            let mut editor = Editor::single_line(window, cx);
            editor.set_text(fk.from_column.clone(), window, cx);
            editor
        });
        let to_table = cx.new(|cx| {
            let mut editor = Editor::single_line(window, cx);
            editor.set_text(fk.to_table.clone(), window, cx);
            editor
        });
        let to_column = cx.new(|cx| {
            let mut editor = Editor::single_line(window, cx);
            editor.set_text(fk.to_column.clone(), window, cx);
            editor
        });
        FkDraft {
            original: Some(fk),
            name,
            from_column,
            to_table,
            to_column,
            dropped: false,
        }
    }

    fn add_blank_fk(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let make = |placeholder: &'static str, window: &mut Window, cx: &mut Context<Self>| {
            cx.new(|cx| {
                let mut editor = Editor::single_line(window, cx);
                editor.set_placeholder_text(placeholder, window, cx);
                editor
            })
        };
        self.fk_drafts.push(FkDraft {
            original: None,
            name: make("fk_name", window, cx),
            from_column: make("column", window, cx),
            to_table: make("referenced_table", window, cx),
            to_column: make("referenced_column", window, cx),
            dropped: false,
        });
        cx.notify();
    }

    fn check_draft_from_info(
        check: CheckConstraintInfo,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> CheckDraft {
        let name = cx.new(|cx| {
            let mut editor = Editor::single_line(window, cx);
            editor.set_text(check.name.clone(), window, cx);
            editor
        });
        let expression = cx.new(|cx| {
            let mut editor = Editor::single_line(window, cx);
            editor.set_text(check.expression.clone(), window, cx);
            editor
        });
        CheckDraft {
            original: Some(check),
            name,
            expression,
            dropped: false,
        }
    }

    fn add_blank_check(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let name = cx.new(|cx| {
            let mut editor = Editor::single_line(window, cx);
            editor.set_placeholder_text("check_name", window, cx);
            editor
        });
        let expression = cx.new(|cx| {
            let mut editor = Editor::single_line(window, cx);
            editor.set_placeholder_text("expression, e.g. amount >= 0", window, cx);
            editor
        });
        self.check_drafts.push(CheckDraft {
            original: None,
            name,
            expression,
            dropped: false,
        });
        cx.notify();
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

    fn index_snapshot(&self, cx: &Context<Self>) -> Vec<IndexDraftSnapshot> {
        self.index_drafts
            .iter()
            .map(|draft| IndexDraftSnapshot {
                original_name: draft.original.as_ref().map(|index| index.name.clone()),
                name: draft.name.read(cx).text(cx),
                columns_csv: draft.columns.read(cx).text(cx),
                unique: draft.unique,
                dropped: draft.dropped,
            })
            .collect()
    }

    fn fk_snapshot(&self, cx: &Context<Self>) -> Vec<FkDraftSnapshot> {
        self.fk_drafts
            .iter()
            .map(|draft| FkDraftSnapshot {
                original_name: draft.original.as_ref().map(|fk| fk.name.clone()),
                name: draft.name.read(cx).text(cx),
                from_column: draft.from_column.read(cx).text(cx),
                to_table: draft.to_table.read(cx).text(cx),
                to_column: draft.to_column.read(cx).text(cx),
                dropped: draft.dropped,
            })
            .collect()
    }

    fn check_snapshot(&self, cx: &Context<Self>) -> Vec<CheckDraftSnapshot> {
        self.check_drafts
            .iter()
            .map(|draft| CheckDraftSnapshot {
                original_name: draft.original.as_ref().map(|check| check.name.clone()),
                name: draft.name.read(cx).text(cx),
                expression: draft.expression.read(cx).text(cx),
                dropped: draft.dropped,
            })
            .collect()
    }

    fn pending_statements(&self, cx: &Context<Self>) -> Vec<String> {
        let mut statements =
            generate_alter_statements(&self.table, self.driver, &diff_changes(&self.snapshot(cx)));
        statements.extend(generate_index_statements(
            &self.table,
            self.driver,
            &index_diff_changes(&self.index_snapshot(cx)),
        ));
        statements.extend(generate_foreign_key_statements(
            &self.table,
            self.driver,
            &fk_diff_changes(&self.fk_snapshot(cx)),
        ));
        statements.extend(generate_check_statements(
            &self.table,
            self.driver,
            &check_diff_changes(&self.check_snapshot(cx)),
        ));
        statements
    }

    fn execute(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let statements = self.pending_statements(cx);
        if statements.is_empty() {
            self.status = Some("No changes to apply.".into());
            cx.notify();
            return;
        }
        if statements
            .iter()
            .any(|statement| statement.contains("DROP "))
        {
            let msg = format!(
                "Apply {} pending change(s) to {}.{}? This includes one or more DROP operations and cannot be undone.",
                statements.len(),
                self.database,
                self.table,
            );
            let receiver =
                window.prompt(PromptLevel::Warning, &msg, None, &["Cancel", "Apply"], cx);
            cx.spawn_in(window, async move |this, cx| {
                if receiver.await == Ok(1) {
                    this.update(cx, |this, cx| this.run_statements(statements, cx))?;
                }
                anyhow::Ok(())
            })
            .detach_and_log_err(cx);
            return;
        }
        self.run_statements(statements, cx);
    }

    fn run_statements(&mut self, statements: Vec<String>, cx: &mut Context<Self>) {
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
            .child(crate::widgets::text_field(&draft.name, cx).w(px(160.)))
            .child(crate::widgets::text_field(&draft.data_type, cx).w(px(140.)))
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
                    .tooltip(Tooltip::text(if dropped {
                        "Keep column"
                    } else {
                        "Drop column"
                    }))
                    .on_click(cx.listener(move |this, _, _, cx| {
                        if let Some(draft) = this.drafts.get_mut(index) {
                            draft.dropped = !draft.dropped;
                            cx.notify();
                        }
                    })),
            )
    }

    fn render_index_row(
        &self,
        index: usize,
        draft: &IndexDraft,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let dropped = draft.dropped;
        let unique = draft.unique;
        let is_existing = draft.original.is_some();
        h_flex()
            .w_full()
            .gap_2()
            .items_center()
            .py_0p5()
            .when(dropped, |row| row.opacity(0.5))
            .child(crate::widgets::text_field(&draft.name, cx).w(px(160.)))
            .child(crate::widgets::text_field(&draft.columns, cx).w(px(220.)))
            .child(
                Checkbox::new(("index-unique", index), unique.into())
                    .label("Unique")
                    .disabled(is_existing)
                    .on_click(cx.listener(move |this, _, _, cx| {
                        if let Some(draft) = this.index_drafts.get_mut(index) {
                            draft.unique = !draft.unique;
                            cx.notify();
                        }
                    })),
            )
            .child(
                IconButton::new(("drop-index", index), IconName::Trash)
                    .icon_size(IconSize::XSmall)
                    .tooltip(Tooltip::text(if dropped {
                        "Keep index"
                    } else {
                        "Drop index"
                    }))
                    .on_click(cx.listener(move |this, _, _, cx| {
                        if let Some(draft) = this.index_drafts.get_mut(index) {
                            draft.dropped = !draft.dropped;
                            cx.notify();
                        }
                    })),
            )
    }

    fn render_fk_row(
        &self,
        index: usize,
        draft: &FkDraft,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let dropped = draft.dropped;
        h_flex()
            .w_full()
            .gap_2()
            .items_center()
            .py_0p5()
            .when(dropped, |row| row.opacity(0.5))
            .child(crate::widgets::text_field(&draft.name, cx).w(px(120.)))
            .child(crate::widgets::text_field(&draft.from_column, cx).w(px(100.)))
            .child(Label::new("→").size(LabelSize::Small).color(Color::Muted))
            .child(crate::widgets::text_field(&draft.to_table, cx).w(px(120.)))
            .child(crate::widgets::text_field(&draft.to_column, cx).w(px(100.)))
            .child(
                IconButton::new(("drop-fk", index), IconName::Trash)
                    .icon_size(IconSize::XSmall)
                    .tooltip(Tooltip::text(if dropped {
                        "Keep foreign key"
                    } else {
                        "Drop foreign key"
                    }))
                    .on_click(cx.listener(move |this, _, _, cx| {
                        if let Some(draft) = this.fk_drafts.get_mut(index) {
                            draft.dropped = !draft.dropped;
                            cx.notify();
                        }
                    })),
            )
    }

    fn render_check_row(
        &self,
        index: usize,
        draft: &CheckDraft,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let dropped = draft.dropped;
        h_flex()
            .w_full()
            .gap_2()
            .items_center()
            .py_0p5()
            .when(dropped, |row| row.opacity(0.5))
            .child(crate::widgets::text_field(&draft.name, cx).w(px(160.)))
            .child(crate::widgets::text_field(&draft.expression, cx).w(px(220.)))
            .child(
                IconButton::new(("drop-check", index), IconName::Trash)
                    .icon_size(IconSize::XSmall)
                    .tooltip(Tooltip::text(if dropped {
                        "Keep check constraint"
                    } else {
                        "Drop check constraint"
                    }))
                    .on_click(cx.listener(move |this, _, _, cx| {
                        if let Some(draft) = this.check_drafts.get_mut(index) {
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
        let index_rows: Vec<_> = self
            .index_drafts
            .iter()
            .enumerate()
            .map(|(index, draft)| self.render_index_row(index, draft, cx).into_any_element())
            .collect();
        let fk_rows: Vec<_> = self
            .fk_drafts
            .iter()
            .enumerate()
            .map(|(index, draft)| self.render_fk_row(index, draft, cx).into_any_element())
            .collect();
        let check_rows: Vec<_> = self
            .check_drafts
            .iter()
            .enumerate()
            .map(|(index, draft)| self.render_check_row(index, draft, cx).into_any_element())
            .collect();
        let title = format!("Modify {}.{}", self.database, self.table);
        let busy = self.busy;

        v_flex()
            .key_context("ModifyTable")
            .track_focus(&self.focus_handle)
            .cyberpunk_surface()
            .shadow(ElevationIndex::ModalSurface.shadow(cx))
            .w(px(640.))
            .max_h(px(560.))
            .p_3()
            .gap_2()
            .child(crate::widgets::dialog_header(
                title,
                "close-modify",
                cx.listener(|_, _, _, cx| cx.emit(DismissEvent)),
                cx,
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
                    .style(cyberpunk::Rank::Quiet.style())
                    .on_click(cx.listener(|this, _, window, cx| this.add_blank_column(window, cx))),
            )
            .child(Divider::horizontal())
            .child(
                Label::new("Indexes")
                    .size(LabelSize::Small)
                    .color(Color::Muted),
            )
            .child(
                v_flex()
                    .id("modify-indexes")
                    .gap_0p5()
                    .max_h(px(140.))
                    .overflow_y_scroll()
                    .children(index_rows),
            )
            .child(
                Button::new("add-index", "Add Index")
                    .style(cyberpunk::Rank::Quiet.style())
                    .on_click(cx.listener(|this, _, window, cx| this.add_blank_index(window, cx))),
            )
            .child(Divider::horizontal())
            .child(
                Label::new("Foreign Keys")
                    .size(LabelSize::Small)
                    .color(Color::Muted),
            )
            .child(
                v_flex()
                    .id("modify-fks")
                    .gap_0p5()
                    .max_h(px(140.))
                    .overflow_y_scroll()
                    .children(fk_rows),
            )
            .child(
                Button::new("add-fk", "Add Foreign Key")
                    .style(cyberpunk::Rank::Quiet.style())
                    .on_click(cx.listener(|this, _, window, cx| this.add_blank_fk(window, cx))),
            )
            .child(Divider::horizontal())
            .child(
                Label::new("Check Constraints")
                    .size(LabelSize::Small)
                    .color(Color::Muted),
            )
            .child(
                v_flex()
                    .id("modify-checks")
                    .gap_0p5()
                    .max_h(px(140.))
                    .overflow_y_scroll()
                    .children(check_rows),
            )
            .child(
                Button::new("add-check", "Add Check Constraint")
                    .style(cyberpunk::Rank::Quiet.style())
                    .on_click(cx.listener(|this, _, window, cx| this.add_blank_check(window, cx))),
            )
            .child(Divider::horizontal())
            .child(
                Label::new("SQL Preview")
                    .size(LabelSize::Small)
                    .color(Color::Muted),
            )
            .child(
                div()
                    .id("modify-sql-preview")
                    .w_full()
                    .max_h(px(120.))
                    .overflow_y_scroll()
                    .p_2()
                    .rounded_none()
                    .border_1()
                    .border_color(cyberpunk::border_dim())
                    .bg(cyberpunk::surface())
                    .font_family("monospace")
                    .child(Label::new(preview_text).size(LabelSize::Small)),
            )
            .when_some(self.status.clone(), |column, status| {
                column.child(
                    Label::new(status)
                        .size(LabelSize::Small)
                        .color(Color::Muted),
                )
            })
            .child(
                h_flex()
                    .w_full()
                    .justify_end()
                    .gap_2()
                    .child(
                        Button::new("cancel", "Cancel")
                            .style(cyberpunk::Rank::Neutral.style())
                            .on_click(cx.listener(|_, _, _, cx| {
                                cx.emit(DismissEvent);
                            })),
                    )
                    .child(
                        Button::new("execute", "Execute")
                            .style(ButtonStyle::OutlinedCustom(
                                cyberpunk::Accent::Cyan.border(),
                            ))
                            .disabled(busy)
                            .on_click(cx.listener(|this, _, window, cx| this.execute(window, cx))),
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

    fn index_draft(
        original_name: Option<&str>,
        name: &str,
        columns_csv: &str,
        unique: bool,
        dropped: bool,
    ) -> IndexDraftSnapshot {
        IndexDraftSnapshot {
            original_name: original_name.map(str::to_string),
            name: name.to_string(),
            columns_csv: columns_csv.to_string(),
            unique,
            dropped,
        }
    }

    #[test]
    fn add_index_generates_create_index_statement() {
        let changes = index_diff_changes(&[index_draft(None, "idx_email", "email", true, false)]);
        let sql = generate_index_statements("users", DatabaseDriver::PostgreSQL, &changes);
        assert_eq!(
            sql,
            vec!["CREATE UNIQUE INDEX \"idx_email\" ON \"users\" (\"email\");"]
        );
    }

    #[test]
    fn add_composite_index_lists_all_columns_in_order() {
        let changes = index_diff_changes(&[index_draft(
            None,
            "idx_name",
            "last_name, first_name",
            false,
            false,
        )]);
        let sql = generate_index_statements("users", DatabaseDriver::MySQL, &changes);
        assert_eq!(
            sql,
            vec!["CREATE INDEX `idx_name` ON `users` (`last_name`, `first_name`);"]
        );
    }

    #[test]
    fn drop_index_uses_alter_table_drop_index_on_mysql() {
        let changes =
            index_diff_changes(&[index_draft(Some("idx_old"), "idx_old", "", false, true)]);
        let sql = generate_index_statements("t", DatabaseDriver::MySQL, &changes);
        assert_eq!(sql, vec!["ALTER TABLE `t` DROP INDEX `idx_old`;"]);
    }

    #[test]
    fn drop_index_uses_bare_drop_index_on_postgres_and_sqlite() {
        let changes =
            index_diff_changes(&[index_draft(Some("idx_old"), "idx_old", "", false, true)]);
        assert_eq!(
            generate_index_statements("t", DatabaseDriver::PostgreSQL, &changes),
            vec!["DROP INDEX \"idx_old\";"]
        );
        assert_eq!(
            generate_index_statements("t", DatabaseDriver::SQLite, &changes),
            vec!["DROP INDEX \"idx_old\";"]
        );
    }

    #[test]
    fn cassandra_add_column_omits_the_column_keyword_and_nullability_clause() {
        let changes = diff_changes(&[draft(None, "email", "text", false, false)]);
        let sql = generate_alter_statements("users", DatabaseDriver::Cassandra, &changes);
        assert_eq!(sql, vec!["ALTER TABLE \"users\" ADD \"email\" text;"]);
    }

    #[test]
    fn cassandra_drop_column_omits_the_column_keyword() {
        let changes = diff_changes(&[draft(
            Some(col("legacy", "int", true)),
            "legacy",
            "int",
            true,
            true,
        )]);
        let sql = generate_alter_statements("t", DatabaseDriver::Cassandra, &changes);
        assert_eq!(sql, vec!["ALTER TABLE \"t\" DROP \"legacy\";"]);
    }

    #[test]
    fn cassandra_rename_omits_the_column_keyword() {
        let changes = diff_changes(&[draft(
            Some(col("old", "int", true)),
            "new",
            "int",
            true,
            false,
        )]);
        let sql = generate_alter_statements("t", DatabaseDriver::Cassandra, &changes);
        assert_eq!(sql, vec!["ALTER TABLE \"t\" RENAME \"old\" TO \"new\";"]);
    }

    #[test]
    fn cassandra_modify_type_has_no_alter_column_wording_or_nullability_clause() {
        let changes = diff_changes(&[draft(
            Some(col("amount", "int", true)),
            "amount",
            "bigint",
            false,
            false,
        )]);
        let sql = generate_alter_statements("t", DatabaseDriver::Cassandra, &changes);
        assert_eq!(sql, vec!["ALTER TABLE \"t\" ALTER \"amount\" TYPE bigint;"]);
    }

    #[test]
    fn cassandra_index_has_no_unique_modifier() {
        let changes = index_diff_changes(&[index_draft(None, "idx_email", "email", true, false)]);
        let sql = generate_index_statements("users", DatabaseDriver::Cassandra, &changes);
        assert_eq!(
            sql,
            vec!["CREATE INDEX \"idx_email\" ON \"users\" (\"email\");"]
        );
    }

    #[test]
    fn cassandra_composite_index_becomes_one_statement_per_column() {
        let changes = index_diff_changes(&[index_draft(
            None,
            "idx_name",
            "last_name, first_name",
            false,
            false,
        )]);
        let sql = generate_index_statements("users", DatabaseDriver::Cassandra, &changes);
        assert_eq!(
            sql,
            vec![
                "CREATE INDEX \"idx_name_0\" ON \"users\" (\"last_name\");",
                "CREATE INDEX \"idx_name_1\" ON \"users\" (\"first_name\");",
            ]
        );
    }

    #[test]
    fn cassandra_drop_index_matches_the_generic_non_mysql_form() {
        let changes =
            index_diff_changes(&[index_draft(Some("idx_old"), "idx_old", "", false, true)]);
        let sql = generate_index_statements("t", DatabaseDriver::Cassandra, &changes);
        assert_eq!(sql, vec!["DROP INDEX \"idx_old\";"]);
    }

    // Before this fix, ClickHouse fell through to the generic non-Cassandra
    // branch, which emits `ADD COLUMN ... NOT NULL`/`NULL` -- both rejected
    // by ClickHouse's parser (`Nullable(T)` is the only way it expresses
    // nullability), verified against a live ClickHouse 24.10 instance.
    #[test]
    fn clickhouse_add_column_omits_the_nullability_clause() {
        let changes = diff_changes(&[draft(None, "email", "String", false, false)]);
        let sql = generate_alter_statements("users", DatabaseDriver::ClickHouse, &changes);
        assert_eq!(
            sql,
            vec!["ALTER TABLE \"users\" ADD COLUMN \"email\" String;"]
        );
    }

    #[test]
    fn clickhouse_drop_and_rename_column_match_the_generic_non_mysql_form() {
        let dropped = diff_changes(&[draft(
            Some(col("legacy", "String", true)),
            "legacy",
            "String",
            true,
            true,
        )]);
        assert_eq!(
            generate_alter_statements("t", DatabaseDriver::ClickHouse, &dropped),
            vec!["ALTER TABLE \"t\" DROP COLUMN \"legacy\";"]
        );

        let renamed = diff_changes(&[draft(
            Some(col("old", "String", true)),
            "new",
            "String",
            true,
            false,
        )]);
        assert_eq!(
            generate_alter_statements("t", DatabaseDriver::ClickHouse, &renamed),
            vec!["ALTER TABLE \"t\" RENAME COLUMN \"old\" TO \"new\";"]
        );
    }

    // Before this fix, ClickHouse's Modify emitted a second `ALTER COLUMN ...
    // SET/DROP NOT NULL;` statement (the generic non-MySQL branch's
    // nullability half), which is a syntax error in ClickHouse -- it has no
    // per-column nullability operation to set or drop, only the `TYPE`
    // change itself (verified against a live ClickHouse 24.10 instance).
    #[test]
    fn clickhouse_modify_type_emits_only_the_type_change_no_nullability_statement() {
        let changes = diff_changes(&[draft(
            Some(col("amount", "UInt32", true)),
            "amount",
            "Int64",
            false,
            false,
        )]);
        let sql = generate_alter_statements("t", DatabaseDriver::ClickHouse, &changes);
        assert_eq!(
            sql,
            vec!["ALTER TABLE \"t\" ALTER COLUMN \"amount\" TYPE Int64;"]
        );
    }

    // Before this fix, ClickHouse fell through to the generic branch, which
    // emits `CREATE UNIQUE INDEX` (ClickHouse: "CREATE UNIQUE INDEX is not
    // supported") and `CREATE INDEX` with no `TYPE` (ClickHouse: "CREATE
    // INDEX without TYPE is forbidden") -- both confirmed against a live
    // ClickHouse 24.10 instance.
    #[test]
    fn clickhouse_index_has_no_unique_modifier_and_carries_a_type() {
        let changes = index_diff_changes(&[index_draft(None, "idx_email", "email", true, false)]);
        let sql = generate_index_statements("users", DatabaseDriver::ClickHouse, &changes);
        assert_eq!(
            sql,
            vec!["CREATE INDEX \"idx_email\" ON \"users\" (\"email\") TYPE minmax;"]
        );
    }

    #[test]
    fn clickhouse_composite_index_stays_one_statement_with_all_columns() {
        let changes = index_diff_changes(&[index_draft(
            None,
            "idx_name",
            "last_name, first_name",
            false,
            false,
        )]);
        let sql = generate_index_statements("users", DatabaseDriver::ClickHouse, &changes);
        assert_eq!(
            sql,
            vec![
                "CREATE INDEX \"idx_name\" ON \"users\" (\"last_name\", \"first_name\") TYPE minmax;"
            ]
        );
    }

    // Before this fix, ClickHouse fell through to the generic non-MySQL
    // branch's bare `DROP INDEX name;`, which ClickHouse's parser rejects
    // ("Expected ON") -- verified against a live ClickHouse 24.10 instance.
    #[test]
    fn clickhouse_drop_index_uses_alter_table_drop_index() {
        let changes =
            index_diff_changes(&[index_draft(Some("idx_old"), "idx_old", "", false, true)]);
        let sql = generate_index_statements("t", DatabaseDriver::ClickHouse, &changes);
        assert_eq!(sql, vec!["ALTER TABLE \"t\" DROP INDEX \"idx_old\";"]);
    }

    // Before this fix, ClickHouse fell through to the generic branch's `ADD
    // CONSTRAINT ... FOREIGN KEY ...`, a syntax error there -- ClickHouse's
    // `ADD CONSTRAINT` only accepts `CHECK`/`ASSUME` (verified against a live
    // ClickHouse 24.10 instance), matching how SQLite is already gated below.
    #[test]
    fn foreign_key_statements_are_not_generated_for_clickhouse() {
        let changes =
            fk_diff_changes(&[fk_draft(None, "fk_owner", "owner_id", "users", "id", false)]);
        assert!(
            generate_foreign_key_statements("orders", DatabaseDriver::ClickHouse, &changes)
                .is_empty()
        );
    }

    fn fk_draft(
        original_name: Option<&str>,
        name: &str,
        from_column: &str,
        to_table: &str,
        to_column: &str,
        dropped: bool,
    ) -> FkDraftSnapshot {
        FkDraftSnapshot {
            original_name: original_name.map(str::to_string),
            name: name.to_string(),
            from_column: from_column.to_string(),
            to_table: to_table.to_string(),
            to_column: to_column.to_string(),
            dropped,
        }
    }

    #[test]
    fn add_foreign_key_generates_add_constraint_statement() {
        let changes =
            fk_diff_changes(&[fk_draft(None, "fk_owner", "owner_id", "users", "id", false)]);
        let sql = generate_foreign_key_statements("orders", DatabaseDriver::PostgreSQL, &changes);
        assert_eq!(
            sql,
            vec![
                "ALTER TABLE \"orders\" ADD CONSTRAINT \"fk_owner\" FOREIGN KEY (\"owner_id\") REFERENCES \"users\" (\"id\");"
            ]
        );
    }

    #[test]
    fn drop_foreign_key_differs_between_mysql_and_postgres() {
        let changes = fk_diff_changes(&[fk_draft(Some("fk_owner"), "fk_owner", "", "", "", true)]);
        assert_eq!(
            generate_foreign_key_statements("orders", DatabaseDriver::MySQL, &changes),
            vec!["ALTER TABLE `orders` DROP FOREIGN KEY `fk_owner`;"]
        );
        assert_eq!(
            generate_foreign_key_statements("orders", DatabaseDriver::PostgreSQL, &changes),
            vec!["ALTER TABLE \"orders\" DROP CONSTRAINT \"fk_owner\";"]
        );
    }

    #[test]
    fn foreign_key_statements_are_not_generated_for_sqlite() {
        let changes =
            fk_diff_changes(&[fk_draft(None, "fk_owner", "owner_id", "users", "id", false)]);
        assert!(
            generate_foreign_key_statements("orders", DatabaseDriver::SQLite, &changes).is_empty()
        );
    }

    fn check_draft(
        original_name: Option<&str>,
        name: &str,
        expression: &str,
        dropped: bool,
    ) -> CheckDraftSnapshot {
        CheckDraftSnapshot {
            original_name: original_name.map(str::to_string),
            name: name.to_string(),
            expression: expression.to_string(),
            dropped,
        }
    }

    #[test]
    fn add_check_constraint_generates_add_constraint_statement() {
        let changes = check_diff_changes(&[check_draft(None, "chk_amount", "amount >= 0", false)]);
        let sql = generate_check_statements("orders", DatabaseDriver::PostgreSQL, &changes);
        assert_eq!(
            sql,
            vec!["ALTER TABLE \"orders\" ADD CONSTRAINT \"chk_amount\" CHECK (amount >= 0);"]
        );
    }

    #[test]
    fn drop_check_constraint_differs_between_mysql_and_postgres() {
        let changes =
            check_diff_changes(&[check_draft(Some("chk_amount"), "chk_amount", "", true)]);
        assert_eq!(
            generate_check_statements("orders", DatabaseDriver::MySQL, &changes),
            vec!["ALTER TABLE `orders` DROP CHECK `chk_amount`;"]
        );
        assert_eq!(
            generate_check_statements("orders", DatabaseDriver::PostgreSQL, &changes),
            vec!["ALTER TABLE \"orders\" DROP CONSTRAINT \"chk_amount\";"]
        );
    }

    #[test]
    fn check_constraint_statements_are_not_generated_for_sqlite() {
        let changes = check_diff_changes(&[check_draft(None, "chk_amount", "amount >= 0", false)]);
        assert!(generate_check_statements("orders", DatabaseDriver::SQLite, &changes).is_empty());
    }
}
