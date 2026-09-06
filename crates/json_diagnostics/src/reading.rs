use std::ops::Range;

use collections::HashMap;
use serde_json::Value;

/// A document read from the buffer, and where each part of it came from.
///
/// A validator answers in JSON pointers -- `/languages/Rust/tab_size` -- and
/// the editor needs a byte range. Nothing but the text can convert one to the
/// other, so the reading that produces the value records both.
pub struct Document {
    pub value: Value,
    /// The whole of the value at each pointer, braces and brackets included.
    values: HashMap<String, Range<usize>>,
    /// The quoted name of each property, kept under that property's own
    /// pointer. A complaint about a property the schema does not allow
    /// belongs on the name, and the name is not part of the value.
    names: HashMap<String, Range<usize>>,
}

/// The one thing wrong with a document that could not be read at all.
///
/// One, not many: past the first unbalanced brace nothing further can be
/// said, and saying it anyway would bury the real fault under its
/// consequences.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Unreadable {
    pub message: String,
    pub range: Range<usize>,
}

impl Document {
    /// Reads a document, or nothing at all where the text holds no document
    /// -- an empty file, or one holding only comments. That is not a fault
    /// worth marking: there is simply nothing to check yet.
    ///
    /// Comments and trailing commas are read wherever they appear, in `.json`
    /// as much as in `.jsonc`. This editor's own `settings.json` and
    /// `keymap.json` are commented files with a `.json` name, and so are most
    /// of the files a schema applies to; calling their comments a syntax
    /// error would mark almost every file this checks.
    pub fn read(text: &str) -> Option<Result<Document, Unreadable>> {
        let mut reader = Reader {
            text,
            at: 0,
            depth: 0,
        };
        if let Err(unreadable) = reader.skip_trivia() {
            return Some(Err(unreadable));
        }
        if reader.at >= text.len() {
            return None;
        }
        Some(reader.whole_document())
    }

    /// The bytes the value at this pointer occupies.
    pub fn value_at(&self, pointer: &str) -> Option<Range<usize>> {
        self.values.get(pointer).cloned()
    }

    /// The bytes this property's quoted name occupies.
    pub fn name_at(&self, pointer: &str) -> Option<Range<usize>> {
        self.names.get(pointer).cloned()
    }

    /// Where to put a complaint about something a value is missing. The
    /// opening brace, rather than the whole value: an object that lacks a
    /// required property may be the entire file, and underlining the entire
    /// file says nothing about where to fix it.
    ///
    /// One byte is a whole character here: every value starts on `{`, `[`,
    /// `"`, a digit, `-`, or a keyword letter.
    pub fn opening_of(&self, pointer: &str) -> Option<Range<usize>> {
        let whole = self.value_at(pointer)?;
        Some(whole.start..whole.end.min(whole.start + 1))
    }
}

/// The pointer of a property of the value at `parent`, escaped the way RFC
/// 6901 asks and the way the validator's own pointers are escaped, so the two
/// agree on a property named `a/b`.
pub fn pointer_to(parent: &str, name: &str) -> String {
    let mut pointer = String::with_capacity(parent.len() + name.len() + 1);
    pointer.push_str(parent);
    pointer.push('/');
    for character in name.chars() {
        match character {
            '~' => pointer.push_str("~0"),
            '/' => pointer.push_str("~1"),
            character => pointer.push(character),
        }
    }
    pointer
}

struct Reader<'a> {
    text: &'a str,
    at: usize,
    /// How many values deep the reader presently is. A recursive descent over
    /// text nobody vouched for runs out of stack rather than out of patience,
    /// and a stack overflow aborts the process instead of unwinding.
    depth: usize,
}

/// As deep as a document may nest. `serde_json` stops at 128 for the same
/// reason, and nothing a reader edits by hand comes close.
const DEEPEST_NESTING: usize = 128;

impl<'a> Reader<'a> {
    fn whole_document(&mut self) -> Result<Document, Unreadable> {
        let mut values = HashMap::default();
        let mut names = HashMap::default();
        let value = self.value("", &mut values, &mut names)?;
        self.skip_trivia()?;
        if self.at < self.text.len() {
            return Err(self.wrong_here("expected the end of the document"));
        }
        Ok(Document {
            value,
            values,
            names,
        })
    }

    fn peek(&self) -> Option<u8> {
        self.text.as_bytes().get(self.at).copied()
    }

    fn wrong_here(&self, message: &str) -> Unreadable {
        let end = self
            .text
            .get(self.at..)
            .and_then(|rest| rest.chars().next())
            .map_or(self.at, |character| self.at + character.len_utf8());
        Unreadable {
            message: message.to_string(),
            range: self.at..end,
        }
    }

    fn wrong_at(&self, at: usize, message: &str) -> Unreadable {
        Unreadable {
            message: message.to_string(),
            range: at..self.text.len().min(at + 1),
        }
    }

    /// Whitespace and comments, both kinds. An unterminated block comment
    /// swallows the rest of the file, so it is a fault rather than a place to
    /// stop reading.
    fn skip_trivia(&mut self) -> Result<(), Unreadable> {
        loop {
            match self.peek() {
                Some(b' ' | b'\t' | b'\n' | b'\r') => self.at += 1,
                Some(b'/') => match self.text.as_bytes().get(self.at + 1) {
                    Some(b'/') => {
                        self.at = self.text[self.at..]
                            .find('\n')
                            .map_or(self.text.len(), |end| self.at + end);
                    }
                    Some(b'*') => {
                        let opened = self.at;
                        let Some(end) = self.text[self.at + 2..].find("*/") else {
                            return Err(self.wrong_at(opened, "unterminated comment"));
                        };
                        self.at += 2 + end + 2;
                    }
                    _ => return Ok(()),
                },
                _ => return Ok(()),
            }
        }
    }

    fn value(
        &mut self,
        pointer: &str,
        values: &mut HashMap<String, Range<usize>>,
        names: &mut HashMap<String, Range<usize>>,
    ) -> Result<Value, Unreadable> {
        self.skip_trivia()?;
        self.depth += 1;
        if self.depth > DEEPEST_NESTING {
            self.depth -= 1;
            return Err(self.wrong_here("nested deeper than this can read"));
        }
        let start = self.at;
        // Every arm below can fail, and the counter has to come back down on
        // those paths too: today an error ends the whole parse, but a caller
        // that recovered and read on with the same reader would watch the
        // depth climb until valid documents were refused.
        let value = match self.read_one(pointer, values, names) {
            Ok(value) => value,
            Err(unreadable) => {
                self.depth -= 1;
                return Err(unreadable);
            }
        };
        values.insert(pointer.to_string(), start..self.at);
        self.depth -= 1;
        Ok(value)
    }

    fn read_one(
        &mut self,
        pointer: &str,
        values: &mut HashMap<String, Range<usize>>,
        names: &mut HashMap<String, Range<usize>>,
    ) -> Result<Value, Unreadable> {
        let value = match self.peek() {
            Some(b'{') => self.object(pointer, values, names)?,
            Some(b'[') => self.array(pointer, values, names)?,
            Some(b'"') => Value::String(self.string()?.0),
            Some(b't') => self.keyword("true", Value::Bool(true))?,
            Some(b'f') => self.keyword("false", Value::Bool(false))?,
            Some(b'n') => self.keyword("null", Value::Null)?,
            Some(b'-' | b'0'..=b'9') => self.number()?,
            Some(_) => return Err(self.wrong_here("expected a value")),
            None => return Err(self.wrong_here("expected a value")),
        };
        Ok(value)
    }

    fn keyword(&mut self, word: &str, value: Value) -> Result<Value, Unreadable> {
        if !self.text[self.at..].starts_with(word) {
            return Err(self.wrong_here("expected a value"));
        }
        self.at += word.len();
        Ok(value)
    }

    fn number(&mut self) -> Result<Value, Unreadable> {
        let start = self.at;
        if self.peek() == Some(b'-') {
            self.at += 1;
        }
        while matches!(
            self.peek(),
            Some(b'0'..=b'9' | b'.' | b'e' | b'E' | b'+' | b'-')
        ) {
            self.at += 1;
        }
        serde_json::from_str::<serde_json::Number>(&self.text[start..self.at])
            .map(Value::Number)
            .map_err(|_| Unreadable {
                message: "this is not a number".to_string(),
                range: start..self.at,
            })
    }

    /// A quoted string, and the bytes it occupies including both quotes.
    fn string(&mut self) -> Result<(String, Range<usize>), Unreadable> {
        let start = self.at;
        self.at += 1;
        let mut read = String::new();
        loop {
            let Some(character) = self.text[self.at..].chars().next() else {
                return Err(self.wrong_at(start, "unterminated string"));
            };
            self.at += character.len_utf8();
            match character {
                '"' => return Ok((read, start..self.at)),
                '\\' => read.push(self.escape()?),
                character if (character as u32) < 0x20 => {
                    return Err(self.wrong_at(start, "unterminated string"));
                }
                character => read.push(character),
            }
        }
    }

    fn escape(&mut self) -> Result<char, Unreadable> {
        let at = self.at - 1;
        let Some(character) = self.text[self.at..].chars().next() else {
            return Err(self.wrong_at(at, "unfinished escape"));
        };
        self.at += character.len_utf8();
        Ok(match character {
            '"' => '"',
            '\\' => '\\',
            '/' => '/',
            'b' => '\u{8}',
            'f' => '\u{c}',
            'n' => '\n',
            'r' => '\r',
            't' => '\t',
            'u' => self.escaped_character(at)?,
            _ => return Err(self.wrong_at(at, "this is not an escape")),
        })
    }

    /// What a `\u` escape stands for, a surrogate pair counting as the one
    /// character it encodes. A half of a pair with no other half stands for
    /// nothing, and becomes the replacement character rather than a fault:
    /// what it is worth is a question for whoever wrote it, not for a
    /// schema.
    fn escaped_character(&mut self, at: usize) -> Result<char, Unreadable> {
        let first = self.four_hex_digits(at)?;
        if !(0xd800..0xdc00).contains(&first) {
            return Ok(char::from_u32(first).unwrap_or(char::REPLACEMENT_CHARACTER));
        }
        if !self.text[self.at..].starts_with("\\u") {
            return Ok(char::REPLACEMENT_CHARACTER);
        }
        self.at += 2;
        let second = self.four_hex_digits(at)?;
        if !(0xdc00..0xe000).contains(&second) {
            return Ok(char::REPLACEMENT_CHARACTER);
        }
        let combined = 0x10000 + ((first - 0xd800) << 10) + (second - 0xdc00);
        Ok(char::from_u32(combined).unwrap_or(char::REPLACEMENT_CHARACTER))
    }

    fn four_hex_digits(&mut self, at: usize) -> Result<u32, Unreadable> {
        let digits = self
            .text
            .get(self.at..self.at + 4)
            .ok_or_else(|| self.wrong_at(at, "unfinished escape"))?;
        let value = u32::from_str_radix(digits, 16)
            .map_err(|_| self.wrong_at(at, "this is not an escape"))?;
        self.at += 4;
        Ok(value)
    }

    fn object(
        &mut self,
        pointer: &str,
        values: &mut HashMap<String, Range<usize>>,
        names: &mut HashMap<String, Range<usize>>,
    ) -> Result<Value, Unreadable> {
        let opened = self.at;
        self.at += 1;
        let mut read = serde_json::Map::new();
        loop {
            self.skip_trivia()?;
            match self.peek() {
                // Reached on the first turn by an empty object, and on a
                // later one by a trailing comma. Both are read as written.
                Some(b'}') => {
                    self.at += 1;
                    break;
                }
                Some(b'"') => {}
                None => return Err(self.wrong_at(opened, "unterminated object")),
                Some(_) => return Err(self.wrong_here("expected a property name")),
            }
            let (name, where_named) = self.string()?;
            let inner = pointer_to(pointer, &name);
            names.insert(inner.clone(), where_named);
            self.skip_trivia()?;
            if self.peek() != Some(b':') {
                return Err(self.wrong_here("expected `:`"));
            }
            self.at += 1;
            read.insert(name, self.value(&inner, values, names)?);
            self.skip_trivia()?;
            match self.peek() {
                Some(b',') => self.at += 1,
                Some(b'}') => {
                    self.at += 1;
                    break;
                }
                None => return Err(self.wrong_at(opened, "unterminated object")),
                Some(_) => return Err(self.wrong_here("expected `,` or `}`")),
            }
        }
        Ok(Value::Object(read))
    }

    fn array(
        &mut self,
        pointer: &str,
        values: &mut HashMap<String, Range<usize>>,
        names: &mut HashMap<String, Range<usize>>,
    ) -> Result<Value, Unreadable> {
        let opened = self.at;
        self.at += 1;
        let mut read = Vec::new();
        loop {
            self.skip_trivia()?;
            match self.peek() {
                Some(b']') => {
                    self.at += 1;
                    break;
                }
                None => return Err(self.wrong_at(opened, "unterminated array")),
                Some(_) => {}
            }
            let inner = format!("{pointer}/{}", read.len());
            read.push(self.value(&inner, values, names)?);
            self.skip_trivia()?;
            match self.peek() {
                Some(b',') => self.at += 1,
                Some(b']') => {
                    self.at += 1;
                    break;
                }
                None => return Err(self.wrong_at(opened, "unterminated array")),
                Some(_) => return Err(self.wrong_here("expected `,` or `]`")),
            }
        }
        Ok(Value::Array(read))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use pretty_assertions::assert_eq;
    use serde_json::json;

    fn read(text: &str) -> Document {
        Document::read(text)
            .expect("the text holds a document")
            .expect("the document reads")
    }

    #[test]
    fn nesting_deeper_than_the_reader_takes_is_refused_rather_than_overflowing() {
        let deep = format!(
            "{}1{}",
            "[".repeat(DEEPEST_NESTING + 8),
            "]".repeat(DEEPEST_NESTING + 8)
        );
        let read = Document::read(&deep).expect("the text holds a document");
        assert!(read.is_err(), "a document this deep is refused");

        // And one just inside the limit still reads, so the cap is not simply
        // refusing everything nested at all.
        let shallow = format!("{}1{}", "[".repeat(8), "]".repeat(8));
        assert!(
            Document::read(&shallow)
                .expect("the text holds a document")
                .is_ok()
        );
    }

    #[test]
    fn a_document_reads_to_the_same_value_serde_would_read() {
        let text = r#"{"a": [1, 2.5, -3e2], "b": {"c": true, "d": null}, "e": "xé\n"}"#;
        assert_eq!(
            read(text).value,
            serde_json::from_str::<Value>(text).expect("serde reads it too")
        );
    }

    /// Comments and trailing commas are what a `.jsonc` file is for, and what
    /// this editor's own `.json` settings are full of. Neither changes the
    /// value, so neither can be a schema violation.
    #[test]
    fn comments_and_trailing_commas_are_read_and_change_nothing() {
        let commented = r#"
            // the whole file
            {
                /* about a */ "a": 1, // about a again
                "b": [1, 2,],
            }
        "#;
        assert_eq!(read(commented).value, json!({"a": 1, "b": [1, 2]}));
    }

    /// The span is the point of reading it this way. A value's range covers
    /// the value and nothing else, and a property's name is kept apart from
    /// it, because a complaint about the name belongs on the name.
    #[test]
    fn every_value_and_every_property_name_keeps_the_bytes_it_came_from() {
        let text = r#"{"a": {"b": [10, 20]}}"#;
        let document = read(text);

        for (pointer, expected) in [
            ("", r#"{"a": {"b": [10, 20]}}"#),
            ("/a", r#"{"b": [10, 20]}"#),
            ("/a/b", "[10, 20]"),
            ("/a/b/0", "10"),
            ("/a/b/1", "20"),
        ] {
            let range = document
                .value_at(pointer)
                .unwrap_or_else(|| panic!("no span for {pointer}"));
            assert_eq!(&text[range], expected, "the value at {pointer}");
        }

        let name = document.name_at("/a/b").expect("the name of `b`");
        assert_eq!(&text[name], r#""b""#);
    }

    /// A property named `a/b` is one property, not a path. The validator
    /// escapes it in its own pointers, and a reader that did not would look
    /// the span up under the wrong name and place the diagnostic elsewhere.
    #[test]
    fn a_name_holding_a_slash_or_a_tilde_is_escaped_the_way_the_validator_escapes_it() {
        assert_eq!(pointer_to("", "a/b"), "/a~1b");
        assert_eq!(pointer_to("/x", "a~b"), "/x/a~0b");

        let text = r#"{"a/b": 1}"#;
        let range = read(text).value_at("/a~1b").expect("the escaped pointer");
        assert_eq!(&text[range], "1");
    }

    /// A required property is missing, so there is no span for it. The
    /// opening brace of the object that lacks it is where the reader has to
    /// go, and it is one character rather than the whole file.
    #[test]
    fn a_missing_property_is_pointed_at_by_the_opening_brace_of_its_object() {
        let text = "{\n  \"a\": 1\n}";
        let opening = read(text).opening_of("").expect("the root object");
        assert_eq!(opening, 0..1);
        assert_eq!(&text[opening], "{");
    }

    /// Nothing to check is not a fault. A file the reader has just created,
    /// or one holding only a comment, has no value in it to measure against a
    /// schema, and marking it would put an error on every new file.
    #[test]
    fn a_document_that_is_only_whitespace_or_comments_is_nothing_rather_than_a_fault() {
        for text in ["", "   \n\t ", "// nothing yet\n", "/* nothing yet */"] {
            assert!(Document::read(text).is_none(), "{text:?}");
        }
    }

    /// One fault, at the place it is: everything after an unbalanced brace is
    /// a consequence of it, and reporting the consequences buries the cause.
    #[test]
    fn a_document_that_will_not_read_gives_one_fault_where_it_is() {
        let text = "{\"a\": 1,";
        let Some(Err(unreadable)) = Document::read(text) else {
            panic!("this should not read");
        };
        assert_eq!(unreadable.message, "unterminated object");
        assert_eq!(unreadable.range, 0..1);

        let text = "{\"a\": }";
        let Some(Err(unreadable)) = Document::read(text) else {
            panic!("this should not read");
        };
        assert_eq!(unreadable.message, "expected a value");
        assert_eq!(&text[unreadable.range], "}");
    }

    /// A comment that is opened and never closed swallows the rest of the
    /// file, which is a fault at the place it was opened rather than a place
    /// to quietly stop reading.
    #[test]
    fn an_unterminated_comment_is_a_fault_where_it_was_opened() {
        let text = "{ /* forever\n \"a\": 1 }";
        let Some(Err(unreadable)) = Document::read(text) else {
            panic!("this should not read");
        };
        assert_eq!(unreadable.message, "unterminated comment");
        assert_eq!(unreadable.range, 2..3);
    }

    /// Anything after the document is a fault: a second value pasted below
    /// the first is a common way to break a settings file, and reading only
    /// the first would silently check half of it.
    #[test]
    fn a_second_value_after_the_document_is_a_fault() {
        let Some(Err(unreadable)) = Document::read("{} []") else {
            panic!("this should not read");
        };
        assert_eq!(unreadable.message, "expected the end of the document");
        assert_eq!(unreadable.range, 3..4);
    }
}
