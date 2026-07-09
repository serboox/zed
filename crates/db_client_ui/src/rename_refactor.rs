/// A single place `name` was found referenced as a whole identifier, not as
/// a substring of a longer one (renaming `users` must not match
/// `users_archive`). `line` is 1-based; only meaningful for a text source
/// with real line breaks (console buffers), not for a single-line DDL/DB
/// source excerpt.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct NameUsage {
    pub line: usize,
    pub excerpt: String,
}

/// Finds every whole-word occurrence of `name` in `text`. This is a plain
/// identifier-boundary scan, not a real SQL tokenizer: it does not know
/// about string literals, comments, or quoted identifiers, so a table name
/// that happens to appear inside a string literal is reported as a usage
/// too. That is an accepted simplification for a first version -- a real
/// per-dialect tokenization pass would be needed to eliminate it, and this
/// codebase already has one in `sql_binder.rs` for structured SQL, but
/// applying it here would also require it to parse arbitrary, possibly
/// invalid, in-progress console text and DB-side routine/trigger source,
/// which is a substantially bigger scope than a rename usage preview needs.
pub(crate) fn find_name_usages(text: &str, name: &str) -> Vec<NameUsage> {
    if name.is_empty() {
        return Vec::new();
    }
    let name_bytes = name.as_bytes();
    let mut usages = Vec::new();
    for (line_idx, line) in text.lines().enumerate() {
        let bytes = line.as_bytes();
        let mut search_from = 0;
        while let Some(offset) = find_bytes(&bytes[search_from..], name_bytes) {
            let start = search_from + offset;
            let end = start + name_bytes.len();
            let before_is_boundary = start == 0 || !is_ident_byte(bytes[start - 1]);
            let after_is_boundary = end >= bytes.len() || !is_ident_byte(bytes[end]);
            if before_is_boundary && after_is_boundary {
                usages.push(NameUsage {
                    line: line_idx + 1,
                    excerpt: line.trim().to_string(),
                });
            }
            search_from = start + 1;
        }
    }
    usages
}

/// Replaces every whole-word occurrence of `old_name` with `new_name` in
/// `text`, using the exact same identifier-boundary rule as
/// `find_name_usages` so a buffer is only ever rewritten where a usage was
/// actually reported.
pub(crate) fn replace_name_usages(text: &str, old_name: &str, new_name: &str) -> String {
    if old_name.is_empty() {
        return text.to_string();
    }
    let old_bytes = old_name.as_bytes();
    let bytes = text.as_bytes();
    let mut result = String::with_capacity(text.len());
    let mut search_from = 0;
    let mut last_copied = 0;
    while let Some(offset) = find_bytes(&bytes[search_from..], old_bytes) {
        let start = search_from + offset;
        let end = start + old_bytes.len();
        let before_is_boundary = start == 0 || !is_ident_byte(bytes[start - 1]);
        let after_is_boundary = end >= bytes.len() || !is_ident_byte(bytes[end]);
        if before_is_boundary && after_is_boundary {
            result.push_str(&text[last_copied..start]);
            result.push_str(new_name);
            last_copied = end;
        }
        search_from = start + 1;
    }
    result.push_str(&text[last_copied..]);
    result
}

fn is_ident_byte(byte: u8) -> bool {
    byte.is_ascii_alphanumeric() || byte == b'_'
}

fn find_bytes(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    if needle.is_empty() || needle.len() > haystack.len() {
        return None;
    }
    haystack
        .windows(needle.len())
        .position(|window| window == needle)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn finds_whole_word_matches_only() {
        let text = "SELECT * FROM users WHERE id = 1;\nSELECT * FROM users_archive;";
        let usages = find_name_usages(text, "users");
        assert_eq!(
            usages.len(),
            1,
            "users_archive must not match a rename of users"
        );
        assert_eq!(usages[0].line, 1);
    }

    #[test]
    fn finds_every_occurrence_on_the_same_line() {
        let text = "UPDATE users SET name = 'x' WHERE users.id = 1;";
        let usages = find_name_usages(text, "users");
        assert_eq!(usages.len(), 2);
    }

    #[test]
    fn does_not_match_a_prefix_or_suffix_of_a_longer_identifier() {
        let text = "SELECT * FROM my_users JOIN users_v2 ON 1=1;";
        assert!(find_name_usages(text, "users").is_empty());
    }

    #[test]
    fn replace_rewrites_only_whole_word_matches() {
        let text = "UPDATE users SET name = 'x' WHERE users.id = 1; -- users_archive stays";
        let replaced = replace_name_usages(text, "users", "customers");
        assert_eq!(
            replaced,
            "UPDATE customers SET name = 'x' WHERE customers.id = 1; -- users_archive stays"
        );
    }

    #[test]
    fn replace_preserves_text_with_no_matches() {
        let text = "SELECT * FROM orders;";
        assert_eq!(replace_name_usages(text, "users", "customers"), text);
    }

    #[test]
    fn empty_name_matches_nothing() {
        assert!(find_name_usages("SELECT * FROM users;", "").is_empty());
    }

    #[test]
    fn matches_across_multiple_lines_with_correct_line_numbers() {
        let text = "-- comment\nSELECT *\nFROM users\nWHERE users.id = 1;";
        let usages = find_name_usages(text, "users");
        assert_eq!(
            usages.iter().map(|u| u.line).collect::<Vec<_>>(),
            vec![3, 4]
        );
    }
}
