use crate::store::DatabaseStore;
use editor::{CompletionContext, CompletionProvider, Editor};
use gpui::{App, Context, Entity, Task, WeakEntity, Window};
use language::{Anchor, Buffer, CodeLabel, ToOffset};
use project::{Completion, CompletionDisplayOptions, CompletionResponse, CompletionSource};
use std::cell::RefCell;
use std::ops::Range;
use std::rc::Rc;

const SQL_KEYWORDS: &[&str] = &[
    "SELECT", "FROM", "WHERE", "JOIN", "LEFT", "RIGHT", "INNER", "OUTER", "FULL",
    "ON", "AND", "OR", "NOT", "IN", "IS", "NULL", "AS", "DISTINCT", "ORDER",
    "GROUP", "BY", "HAVING", "LIMIT", "OFFSET", "INSERT", "INTO", "VALUES",
    "UPDATE", "SET", "DELETE", "CREATE", "TABLE", "INDEX", "DROP", "ALTER",
    "ADD", "COLUMN", "PRIMARY", "KEY", "UNIQUE", "REFERENCES", "FOREIGN",
    "CONSTRAINT", "DEFAULT", "AUTO_INCREMENT", "SHOW", "DESCRIBE",
    "USE", "DATABASES", "TABLES", "COLUMNS", "LIKE", "BETWEEN", "EXISTS",
    "UNION", "ALL", "CASE", "WHEN", "THEN", "ELSE", "END", "CAST", "COUNT",
    "SUM", "AVG", "MIN", "MAX", "IFNULL", "IF", "NOW", "DATE",
    "TIMESTAMP", "YEAR", "MONTH", "DAY", "CONCAT", "SUBSTRING", "LENGTH",
    "UPPER", "LOWER", "TRIM", "REPLACE", "ROUND", "FLOOR", "CEIL",
];

pub struct SqlCompletionProvider {
    store: WeakEntity<DatabaseStore>,
}

impl SqlCompletionProvider {
    pub fn new(store: WeakEntity<DatabaseStore>) -> Self {
        Self { store }
    }

    fn is_sql_buffer(buffer: &Entity<Buffer>, cx: &App) -> bool {
        buffer
            .read(cx)
            .language()
            .map(|lang| lang.name().as_ref().to_lowercase() == "sql")
            .unwrap_or(false)
    }

    fn extract_prefix(buffer: &Entity<Buffer>, position: Anchor, cx: &App) -> String {
        let snapshot = buffer.read(cx).snapshot();
        let offset = position.to_offset(&snapshot);
        let text: String = snapshot.text_for_range(0..offset).collect();
        text.chars()
            .rev()
            .take_while(|c| c.is_alphanumeric() || *c == '_')
            .collect::<String>()
            .chars()
            .rev()
            .collect()
    }

    fn extract_table_qualifier(buffer: &Entity<Buffer>, position: Anchor, cx: &App) -> Option<String> {
        let snapshot = buffer.read(cx).snapshot();
        let offset = position.to_offset(&snapshot);
        let text: String = snapshot.text_for_range(0..offset).collect();
        // Check if there is `<table>.` before the cursor (after stripping the current word)
        let before_word = text.trim_end_matches(|c: char| c.is_alphanumeric() || c == '_');
        if let Some(before_dot) = before_word.strip_suffix('.') {
            let table_name: String = before_dot
                .chars()
                .rev()
                .take_while(|c| c.is_alphanumeric() || *c == '_')
                .collect::<String>()
                .chars()
                .rev()
                .collect();
            if !table_name.is_empty() {
                return Some(table_name);
            }
        }
        None
    }

    fn compute_replace_range(buffer: &Entity<Buffer>, position: Anchor, cx: &App) -> Range<Anchor> {
        let snapshot = buffer.read(cx).snapshot();
        let offset = position.to_offset(&snapshot);
        let text_before: String = snapshot.text_for_range(0..offset).collect();
        let start = snapshot.anchor_before(offset - trailing_identifier_byte_len(&text_before));
        let end = snapshot.anchor_after(offset);
        start..end
    }

    fn make_completion(text: String, replace_range: Range<Anchor>) -> Completion {
        Completion {
            replace_range,
            new_text: text.clone(),
            label: CodeLabel::plain(text, None),
            documentation: None,
            source: CompletionSource::Custom,
            icon_path: None,
            icon_color: None,
            match_start: None,
            snippet_deduplication_key: None,
            insert_text_mode: None,
            confirm: None,
            group: None,
        }
    }
}

impl CompletionProvider for SqlCompletionProvider {
    fn completions(
        &self,
        buffer: &Entity<Buffer>,
        buffer_position: Anchor,
        _trigger: CompletionContext,
        _window: &mut Window,
        cx: &mut Context<Editor>,
    ) -> Task<anyhow::Result<Vec<CompletionResponse>>> {
        if !Self::is_sql_buffer(buffer, cx) {
            return Task::ready(Ok(vec![]));
        }

        let store_entity = match self.store.upgrade() {
            Some(s) => s,
            None => return Task::ready(Ok(vec![])),
        };

        let prefix = Self::extract_prefix(buffer, buffer_position, cx).to_lowercase();
        let table_qualifier = Self::extract_table_qualifier(buffer, buffer_position, cx);
        let replace_range = Self::compute_replace_range(buffer, buffer_position, cx);

        let store = store_entity.read(cx);
        let mut completions: Vec<Completion> = Vec::new();

        if let Some(table_name) = table_qualifier {
            // Column completions for `<table>.<prefix>`
            for conn in store.connections() {
                for ((_db, tbl), columns) in &conn.expanded_tables {
                    if tbl.to_lowercase() == table_name.to_lowercase() {
                        for col in columns {
                            if col.name.to_lowercase().starts_with(&prefix) {
                                completions.push(Self::make_completion(
                                    col.name.clone(),
                                    replace_range.clone(),
                                ));
                            }
                        }
                    }
                }
            }
        } else {
            // Table name completions from loaded schema
            for conn in store.connections() {
                if let Some(databases) = &conn.databases {
                    for db in databases {
                        if db.name.to_lowercase().starts_with(&prefix) {
                            completions.push(Self::make_completion(
                                db.name.clone(),
                                replace_range.clone(),
                            ));
                        }
                    }
                }
                for ((_db, table), _) in &conn.expanded_tables {
                    if table.to_lowercase().starts_with(&prefix) {
                        completions.push(Self::make_completion(
                            table.clone(),
                            replace_range.clone(),
                        ));
                    }
                }
                for tables in conn.expanded_databases.values() {
                    for table in tables {
                        if table.name.to_lowercase().starts_with(&prefix) {
                            completions.push(Self::make_completion(
                                table.name.clone(),
                                replace_range.clone(),
                            ));
                        }
                    }
                }
            }
            // SQL keyword completions
            for &keyword in SQL_KEYWORDS {
                if keyword.to_lowercase().starts_with(&prefix) {
                    completions.push(Self::make_completion(
                        keyword.to_string(),
                        replace_range.clone(),
                    ));
                }
            }
        }

        completions.sort_by(|a, b| a.new_text.cmp(&b.new_text));
        completions.dedup_by(|a, b| a.new_text == b.new_text);

        Task::ready(Ok(vec![CompletionResponse {
            completions,
            display_options: CompletionDisplayOptions::default(),
            is_incomplete: false,
        }]))
    }

    fn is_completion_trigger(
        &self,
        buffer: &Entity<Buffer>,
        _position: language::Anchor,
        text: &str,
        trigger_in_words: bool,
        cx: &mut Context<Editor>,
    ) -> bool {
        if !Self::is_sql_buffer(buffer, cx) {
            return false;
        }
        text == "." || trigger_in_words
    }

    fn resolve_completions(
        &self,
        _buffer: Entity<Buffer>,
        _completion_indices: Vec<usize>,
        _completions: Rc<RefCell<Box<[Completion]>>>,
        _cx: &mut Context<Editor>,
    ) -> Task<anyhow::Result<bool>> {
        Task::ready(Ok(false))
    }

    fn sort_completions(&self) -> bool {
        true
    }

    fn filter_completions(&self) -> bool {
        true
    }
}

// Byte length of the trailing identifier in `text`. Summing per-char byte
// lengths keeps a derived start offset on a UTF-8 boundary; mixing a byte
// offset with a char count lands mid-character and panics the rope.
fn trailing_identifier_byte_len(text: &str) -> usize {
    text.chars()
        .rev()
        .take_while(|c| c.is_alphanumeric() || *c == '_')
        .map(|c| c.len_utf8())
        .sum()
}

pub fn install_on_editor(editor: Entity<Editor>, store: WeakEntity<DatabaseStore>, cx: &mut App) {
    editor.update(cx, |editor, cx| {
        let is_sql = editor
            .language_at(language::Point::new(0, 0), cx)
            .map(|lang| lang.name().as_ref().to_lowercase() == "sql")
            .unwrap_or(false);
        if is_sql {
            let provider = Rc::new(SqlCompletionProvider::new(store));
            editor.set_completion_provider(Some(provider));
        }
    });
}

#[cfg(test)]
mod tests {
    use super::trailing_identifier_byte_len;

    #[test]
    fn ascii_identifier_length_is_byte_count() {
        assert_eq!(trailing_identifier_byte_len("SELECT * FROM use"), 3);
        assert_eq!(trailing_identifier_byte_len("col_name"), 8);
        assert_eq!(trailing_identifier_byte_len(""), 0);
    }

    #[test]
    fn stops_at_non_identifier_char() {
        assert_eq!(trailing_identifier_byte_len("a.b"), 1);
        assert_eq!(trailing_identifier_byte_len("SELECT "), 0);
        assert_eq!(trailing_identifier_byte_len("t1.col"), 3);
    }

    #[test]
    fn multibyte_identifier_counts_bytes_not_chars() {
        // Each Cyrillic char is 2 bytes; subtracting this from a byte offset
        // must stay on a char boundary (the original crash).
        assert_eq!(trailing_identifier_byte_len("таблица"), 14);
        assert_eq!(trailing_identifier_byte_len("SELECT поле"), 8);
    }
}
