use std::{ops::Range, sync::LazyLock};

use itertools::Itertools as _;
use log_lens::{Level, read_line};
use regex::Regex;
use terminal::{Color, IndexedCell, NamedColor};

const RECENT_LINES_TO_JUDGE: usize = 24;
const JSON_START_CANDIDATES: usize = 4;
const LEVEL_WORD_CANDIDATES: usize = 3;
const TAB_WIDTH: usize = 8;

static TIME: LazyLock<Option<Regex>> = LazyLock::new(|| {
    Regex::new(
        r"^[\s\[<]*(\d{4}-\d{2}-\d{2}[T ]\d{2}:\d{2}:\d{2}(?:[.,]\d+)?(?:Z|[+-]\d{2}:?\d{2})?|\d{2}:\d{2}:\d{2}(?:[.,]\d+)?)",
    )
    .ok()
});

static CALLER: LazyLock<Option<Regex>> =
    LazyLock::new(|| Regex::new(r"[\w./\\-]+\.[A-Za-z]{1,5}:\d+").ok());

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Part {
    Time,
    Level(Level),
    Caller,
    Key,
    Text,
    Number,
    Keyword,
}

impl Part {
    fn color(self) -> NamedColor {
        match self {
            Part::Time => NamedColor::BrightBlack,
            Part::Level(Level::Trace | Level::Debug) => NamedColor::Blue,
            Part::Level(Level::Info) => NamedColor::Green,
            Part::Level(Level::Warn) => NamedColor::Yellow,
            Part::Level(Level::Error | Level::Fatal) => NamedColor::Red,
            Part::Caller => NamedColor::Cyan,
            Part::Key => NamedColor::BrightBlue,
            Part::Text => NamedColor::Green,
            Part::Number => NamedColor::Yellow,
            Part::Keyword => NamedColor::Magenta,
        }
    }
}

/// A key whose value is not painted as a plain one, because of what it holds.
#[derive(Clone, Copy, PartialEq, Eq)]
enum SpecialKey {
    Level,
    Time,
}

impl SpecialKey {
    fn of(key: &str) -> Option<Self> {
        match key {
            "level" | "lvl" | "severity" => Some(Self::Level),
            "time" | "ts" | "timestamp" => Some(Self::Time),
            _ => None,
        }
    }

    fn part_of(special: Option<Self>, value: &str, otherwise: Option<Part>) -> Option<Part> {
        match special {
            Some(Self::Level) => Level::from_word(value).map(Part::Level).or(otherwise),
            Some(Self::Time) => Some(Part::Time),
            None => otherwise,
        }
    }
}

#[derive(Clone)]
struct RowCell {
    character: char,
    zero_width: Vec<char>,
    /// Whether the cell only holds the place of the other half of a wide
    /// character, and so is no text.
    placeholder: bool,
    /// Whether the cell is free to be recoloured: the program that wrote it
    /// chose no colour of its own.
    plain: bool,
}

impl RowCell {
    fn blank() -> Self {
        Self {
            character: ' ',
            zero_width: Vec::new(),
            placeholder: false,
            plain: true,
        }
    }
}

pub struct Row {
    cells: Vec<RowCell>,
    /// Whether the text goes on in the next row, because the line was wider
    /// than the terminal.
    wraps: bool,
}

pub fn rows_of(cells: &[IndexedCell], columns: usize) -> Vec<Row> {
    cells
        .iter()
        .chunk_by(|cell| cell.point.line)
        .into_iter()
        .map(|(_, row_cells)| {
            let mut row = Row {
                cells: vec![RowCell::blank(); columns],
                wraps: false,
            };
            for cell in row_cells {
                if let Some(slot) = row.cells.get_mut(cell.point.column) {
                    slot.character = cell.character();
                    slot.zero_width = cell.zerowidth().map(<[char]>::to_vec).unwrap_or_default();
                    slot.placeholder = cell.is_wide_char_placeholder();
                    slot.plain = matches!(cell.foreground(), Color::Named(NamedColor::Foreground))
                        && !cell.is_inverse();
                }
                row.wraps |= cell.wraps_to_next_row();
            }
            row
        })
        .collect()
}

/// Whether the lines at the bottom of the screen are in a format the log lens
/// reads, which is when colouring them has something to go by.
pub fn looks_structured(rows: &[Row]) -> bool {
    logical_lines(rows)
        .into_iter()
        .rev()
        .filter_map(|range| rows.get(range).map(logical_text))
        .map(|(text, _)| text)
        .filter(|text| !text.is_empty())
        .take(RECENT_LINES_TO_JUDGE)
        .any(|text| read_line(&text).is_some())
}

/// The colour each cell of the screen is to be painted in, where the text on
/// it is a log line the log lens reads and the program chose no colour for it.
pub struct LogColors {
    columns: usize,
    colors: Vec<Option<NamedColor>>,
}

impl LogColors {
    pub fn paint(rows: &[Row], columns: usize) -> Self {
        let mut colors = vec![None; rows.len() * columns];
        for range in logical_lines(rows) {
            let first_row = range.start;
            let Some(line_rows) = rows.get(range) else {
                continue;
            };
            let (text, positions) = logical_text(line_rows);
            let Some(painted) = paint_line(&text) else {
                continue;
            };
            for (part, position) in painted.into_iter().zip(positions) {
                let Some(part) = part else {
                    continue;
                };
                let (row_in_line, column) = position;
                let row_index = first_row + row_in_line;
                let is_plain = rows
                    .get(row_index)
                    .and_then(|row| row.cells.get(column))
                    .is_some_and(|cell| cell.plain);
                if is_plain && let Some(slot) = colors.get_mut(row_index * columns + column) {
                    *slot = Some(part.color());
                }
            }
        }
        Self { columns, colors }
    }

    pub fn color_at(&self, row: usize, column: usize) -> Option<Color> {
        if column >= self.columns {
            return None;
        }
        self.colors
            .get(row * self.columns + column)
            .copied()
            .flatten()
            .map(Color::Named)
    }
}

fn logical_lines(rows: &[Row]) -> Vec<Range<usize>> {
    let mut lines = Vec::new();
    let mut first = 0;
    for (index, row) in rows.iter().enumerate() {
        if !row.wraps {
            lines.push(first..index + 1);
            first = index + 1;
        }
    }
    if first < rows.len() {
        lines.push(first..rows.len());
    }
    lines
}

/// The text of the rows as one line, and the row (counted from the first one)
/// and column of each of its characters.
///
/// This reads the cells the way alacritty reads them into text: a tab is one
/// character followed by the blank cells up to the next tab stop, which are left
/// out, a wide character is followed by a cell that holds its place and is left
/// out too, and the characters of no width come right after the one they belong
/// to. The log lens reads the fields of a zap line by the tabs between them, and
/// takes a stray blank for a break in a value.
fn logical_text(rows: &[Row]) -> (String, Vec<(usize, usize)>) {
    let mut characters = Vec::new();
    let mut positions = Vec::new();
    let mut in_tab_gap = false;
    for (row_index, row) in rows.iter().enumerate() {
        for (column, cell) in row.cells.iter().enumerate() {
            if in_tab_gap && cell.character == ' ' && !column.is_multiple_of(TAB_WIDTH) {
                continue;
            }
            in_tab_gap = cell.character == '\t';
            if cell.placeholder {
                continue;
            }
            for character in std::iter::once(cell.character).chain(cell.zero_width.iter().copied())
            {
                characters.push(character);
                positions.push((row_index, column));
            }
        }
    }
    let length = characters
        .iter()
        .rposition(|character| !character.is_whitespace())
        .map_or(0, |last| last + 1);
    characters.truncate(length);
    positions.truncate(length);
    (characters.into_iter().collect(), positions)
}

/// The part of every character of `line`, or `None` where the log lens does not
/// read the line, so that ordinary output is left as it is.
fn paint_line(line: &str) -> Option<Vec<Option<Part>>> {
    read_line(line)?;
    let chars: Vec<char> = line.chars().collect();
    let mut painted = vec![None; chars.len()];
    let time_end = paint_time(line, &mut painted);
    let header_end = paint_level(&chars, time_end, &mut painted);
    if !paint_json(&chars, &mut painted) {
        paint_logfmt(&chars, header_end, &mut painted);
    }
    paint_callers(line, &mut painted);
    Some(painted)
}

fn paint_time(line: &str, painted: &mut [Option<Part>]) -> usize {
    let Some(time) = TIME
        .as_ref()
        .and_then(|pattern| pattern.captures(line))
        .and_then(|captures| captures.get(1))
    else {
        return 0;
    };
    let end = char_index(line, time.end());
    fill(painted, char_index(line, time.start())..end, Part::Time);
    end
}

/// The level is the first word after the time, or, once the header carries
/// something else first (a thread, a bracket), an upper-case word soon after.
fn paint_level(chars: &[char], from: usize, painted: &mut [Option<Part>]) -> usize {
    let mut cursor = from;
    for candidate in 0..LEVEL_WORD_CANDIDATES {
        while chars
            .get(cursor)
            .is_some_and(|character| !character.is_ascii_alphabetic() && *character != '{')
        {
            cursor += 1;
        }
        let start = cursor;
        while chars.get(cursor).is_some_and(char::is_ascii_alphabetic) {
            cursor += 1;
        }
        let Some(word) = chars
            .get(start..cursor)
            .filter(|word| !word.is_empty())
            .map(|word| word.iter().collect::<String>())
        else {
            break;
        };
        let upper_case = word.chars().all(|character| character.is_ascii_uppercase());
        if (candidate == 0 || upper_case)
            && let Some(level) = Level::from_word(&word)
        {
            fill(painted, start..cursor, Part::Level(level));
            return cursor;
        }
    }
    from
}

fn paint_json(chars: &[char], painted: &mut [Option<Part>]) -> bool {
    for start in chars
        .iter()
        .positions(|character| *character == '{')
        .take(JSON_START_CANDIDATES)
    {
        let candidate: String = chars.iter().skip(start).collect();
        if matches!(
            serde_json::from_str::<serde_json::Value>(&candidate),
            Ok(serde_json::Value::Object(_))
        ) {
            paint_json_tokens(chars, start, painted);
            return true;
        }
    }
    false
}

fn paint_json_tokens(chars: &[char], start: usize, painted: &mut [Option<Part>]) {
    let mut cursor = start;
    let mut special_key = None;
    while let Some(&character) = chars.get(cursor) {
        match character {
            '"' => {
                let end = string_end(chars, cursor);
                let content: String = chars
                    .get(cursor + 1..end.saturating_sub(1))
                    .map(|content| content.iter().collect())
                    .unwrap_or_default();
                let is_key = chars
                    .get(end..)
                    .and_then(|rest| rest.iter().find(|character| !character.is_whitespace()))
                    == Some(&':');
                let part = if is_key {
                    special_key = SpecialKey::of(&content);
                    Some(Part::Key)
                } else {
                    SpecialKey::part_of(special_key.take(), &content, Some(Part::Text))
                };
                if let Some(part) = part {
                    fill(painted, cursor..end, part);
                }
                cursor = end;
            }
            '-' | '0'..='9' => {
                let start = cursor;
                while chars.get(cursor).is_some_and(|character| {
                    character.is_ascii_digit() || "-+.eE".contains(*character)
                }) {
                    cursor += 1;
                }
                if let Some(part) = SpecialKey::part_of(special_key.take(), "", Some(Part::Number))
                {
                    fill(painted, start..cursor, part);
                }
            }
            'a'..='z' => {
                let start = cursor;
                while chars.get(cursor).is_some_and(char::is_ascii_lowercase) {
                    cursor += 1;
                }
                let word: String = chars
                    .get(start..cursor)
                    .map(|word| word.iter().collect())
                    .unwrap_or_default();
                let keyword =
                    matches!(word.as_str(), "true" | "false" | "null").then_some(Part::Keyword);
                if let Some(part) = SpecialKey::part_of(special_key.take(), "", keyword) {
                    fill(painted, start..cursor, part);
                }
            }
            _ => cursor += 1,
        }
    }
}

/// Where the string that opens at `open` ends, one past its closing quote.
fn string_end(chars: &[char], open: usize) -> usize {
    let mut cursor = open + 1;
    while let Some(&character) = chars.get(cursor) {
        match character {
            '\\' => cursor += 2,
            '"' => return cursor + 1,
            _ => cursor += 1,
        }
    }
    chars.len()
}

fn paint_logfmt(chars: &[char], from: usize, painted: &mut [Option<Part>]) {
    let mut cursor = from;
    while cursor < chars.len() {
        while chars
            .get(cursor)
            .is_some_and(|character| character.is_whitespace())
        {
            cursor += 1;
        }
        let token_start = cursor;
        while chars
            .get(cursor)
            .is_some_and(|character| !character.is_whitespace() && *character != '=')
        {
            cursor += 1;
        }
        let key = chars
            .get(token_start..cursor)
            .filter(|key| {
                chars.get(cursor) == Some(&'=')
                    && key
                        .first()
                        .is_some_and(|first| first.is_ascii_alphabetic() || *first == '_')
                    && key.iter().all(|character| {
                        character.is_alphanumeric() || matches!(character, '_' | '.' | '-')
                    })
            })
            .map(|key| key.iter().collect::<String>());
        let Some(key) = key else {
            while chars
                .get(cursor)
                .is_some_and(|character| !character.is_whitespace())
            {
                cursor += 1;
            }
            continue;
        };
        fill(painted, token_start..cursor, Part::Key);
        cursor += 1;
        let value_start = cursor;
        let quoted = chars.get(cursor) == Some(&'"');
        let value_end = if quoted {
            string_end(chars, cursor)
        } else {
            while chars
                .get(cursor)
                .is_some_and(|character| !character.is_whitespace())
            {
                cursor += 1;
            }
            cursor
        };
        let value: String = chars
            .get(value_start..value_end)
            .map(|value| value.iter().collect::<String>())
            .unwrap_or_default();
        let value = value.trim_matches('"');
        let plain_part = if quoted {
            Some(Part::Text)
        } else if value.parse::<f64>().is_ok() {
            Some(Part::Number)
        } else if matches!(value, "true" | "false" | "null") {
            Some(Part::Keyword)
        } else {
            None
        };
        if let Some(part) = SpecialKey::part_of(SpecialKey::of(&key), value, plain_part) {
            fill(painted, value_start..value_end, part);
        }
        cursor = value_end;
    }
}

fn paint_callers(line: &str, painted: &mut [Option<Part>]) {
    let Some(pattern) = CALLER.as_ref() else {
        return;
    };
    for found in pattern.find_iter(line) {
        fill(
            painted,
            char_index(line, found.start())..char_index(line, found.end()),
            Part::Caller,
        );
    }
}

fn char_index(line: &str, byte_offset: usize) -> usize {
    line.get(..byte_offset)
        .map_or(0, |prefix| prefix.chars().count())
}

fn fill(painted: &mut [Option<Part>], range: Range<usize>, part: Part) {
    for slot in painted.iter_mut().take(range.end).skip(range.start) {
        *slot = Some(part);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn row(text: &str, columns: usize, wraps: bool) -> Row {
        let mut cells: Vec<RowCell> = text
            .chars()
            .map(|character| RowCell {
                character,
                ..RowCell::blank()
            })
            .collect();
        cells.resize(columns, RowCell::blank());
        Row { cells, wraps }
    }

    /// A row where every character of three bytes in UTF-8, which is what the
    /// wide ones of these tests are, takes two cells: itself and the one that
    /// holds the place of its other half.
    fn row_with_wide_characters(text: &str, columns: usize) -> Row {
        let mut cells = Vec::new();
        for character in text.chars() {
            cells.push(RowCell {
                character,
                ..RowCell::blank()
            });
            if character.len_utf8() == 3 {
                cells.push(RowCell {
                    placeholder: true,
                    ..RowCell::blank()
                });
            }
        }
        cells.resize(columns, RowCell::blank());
        Row {
            cells,
            wraps: false,
        }
    }

    /// The row a terminal holds after a program wrote `parts` with a tab between
    /// them: the tab in the first cell of its gap, then blank cells up to the
    /// next tab stop.
    fn tabbed(parts: &[&str], columns: usize) -> Row {
        let mut text = String::new();
        for (index, part) in parts.iter().enumerate() {
            text.push_str(part);
            if index + 1 < parts.len() {
                text.push('\t');
                while !text.chars().count().is_multiple_of(TAB_WIDTH) {
                    text.push(' ');
                }
            }
        }
        row(&text, columns, false)
    }

    fn painted(line: &str) -> Vec<Option<Part>> {
        paint_line(line).expect("the log lens reads this line")
    }

    fn part_at(line: &str, needle: &str) -> Option<Part> {
        let painted = painted(line);
        let start = line.find(needle).expect("the needle is in the line");
        painted
            .get(char_index(line, start))
            .copied()
            .expect("the needle is within the line")
    }

    const ZAP_LINE: &str = "2026-10-28T09:01:09.123+0300\tINFO\tserver/main.go:264\tlistening\t{\"port\": 8080, \"tls\": false, \"name\": \"api\"}";

    #[test]
    fn a_zap_console_line_is_painted_part_by_part() {
        assert_eq!(part_at(ZAP_LINE, "2026"), Some(Part::Time));
        assert_eq!(part_at(ZAP_LINE, "INFO"), Some(Part::Level(Level::Info)));
        assert_eq!(part_at(ZAP_LINE, "server"), Some(Part::Caller));
        assert_eq!(part_at(ZAP_LINE, "listening"), None);
        assert_eq!(part_at(ZAP_LINE, "\"port\""), Some(Part::Key));
        assert_eq!(part_at(ZAP_LINE, "8080"), Some(Part::Number));
        assert_eq!(part_at(ZAP_LINE, "false"), Some(Part::Keyword));
        assert_eq!(part_at(ZAP_LINE, "\"api\""), Some(Part::Text));
    }

    #[test]
    fn the_level_decides_the_colour_of_its_word() {
        for (word, color) in [
            ("DEBUG", NamedColor::Blue),
            ("INFO", NamedColor::Green),
            ("WARN", NamedColor::Yellow),
            ("ERROR", NamedColor::Red),
            ("FATAL", NamedColor::Red),
        ] {
            let line = format!("2026-10-28 09:01:09,123 {word} app.main: something happened");
            assert_eq!(
                part_at(&line, word).map(Part::color),
                Some(color),
                "{word} has its own colour"
            );
        }
    }

    #[test]
    fn a_json_line_is_painted_by_its_keys_and_values() {
        let line = r#"{"level":"error","ts":1761647000.5,"msg":"boom","retry":true,"cause":null}"#;
        assert_eq!(part_at(line, "\"level\""), Some(Part::Key));
        assert_eq!(part_at(line, "\"error\""), Some(Part::Level(Level::Error)));
        assert_eq!(part_at(line, "1761647000.5"), Some(Part::Time));
        assert_eq!(part_at(line, "\"boom\""), Some(Part::Text));
        assert_eq!(part_at(line, "true"), Some(Part::Keyword));
        assert_eq!(part_at(line, "null"), Some(Part::Keyword));
    }

    #[test]
    fn a_quote_in_a_json_string_does_not_end_it() {
        let line = r#"{"level":"info","msg":"said \"hi\" twice","n":3}"#;
        assert_eq!(part_at(line, "twice"), Some(Part::Text));
        assert_eq!(part_at(line, "\"n\""), Some(Part::Key));
        assert_eq!(part_at(line, "3"), Some(Part::Number));
    }

    #[test]
    fn a_logfmt_line_is_painted_by_its_keys_and_values() {
        let line = "time=2026-10-28T09:01:09Z level=warn msg=\"disk is low\" free=12 caller=store/disk.go:88";
        assert_eq!(part_at(line, "level"), Some(Part::Key));
        assert_eq!(part_at(line, "warn"), Some(Part::Level(Level::Warn)));
        assert_eq!(part_at(line, "\"disk"), Some(Part::Text));
        assert_eq!(part_at(line, "12"), Some(Part::Number));
        assert_eq!(part_at(line, "store"), Some(Part::Caller));
    }

    #[test]
    fn a_line_the_log_lens_does_not_read_is_left_alone() {
        assert!(paint_line("$ cargo build --release").is_none());
        assert!(paint_line("Compiling terminal_view v0.1.0").is_none());
        assert!(paint_line("").is_none());
    }

    #[test]
    fn the_screen_is_judged_by_its_recent_lines() {
        let columns = 160;
        let structured = [
            row("$ ./run-api", columns, false),
            row(ZAP_LINE, columns, false),
            row("", columns, false),
        ];
        assert!(looks_structured(&structured));

        let ordinary = [
            row("$ ./run-api", columns, false),
            row("Compiling terminal_view v0.1.0", columns, false),
        ];
        assert!(!looks_structured(&ordinary));
    }

    /// A line longer than the terminal is wide takes several rows, and the
    /// parts of it that sit on the lower ones are painted all the same.
    #[test]
    fn a_wrapped_line_is_painted_across_its_rows() {
        let columns = 40;
        let characters: Vec<char> = ZAP_LINE.chars().collect();
        let rows: Vec<Row> = characters
            .chunks(columns)
            .enumerate()
            .map(|(index, chunk)| {
                let continues = (index + 1) * columns < characters.len();
                row(&chunk.iter().collect::<String>(), columns, continues)
            })
            .collect();
        assert!(rows.len() > 2, "the line has to need several rows");

        let colors = LogColors::paint(&rows, columns);

        let key_position = ZAP_LINE.find("\"port\"").expect("the key is in the line");
        let key_column = char_index(ZAP_LINE, key_position);
        assert_eq!(
            colors.color_at(key_column / columns, key_column % columns),
            Some(Color::Named(NamedColor::BrightBlue)),
            "the key on a lower row has its colour"
        );
        assert_eq!(
            colors.color_at(0, 0),
            Some(Color::Named(NamedColor::BrightBlack)),
            "and the time on the first row keeps its own"
        );
    }

    /// What the grid holds for a tabbed line has blank cells where the tabs
    /// were. The log lens reads a zap line by its tabs, so the blank cells must
    /// not be taken for text, and the parts after them must still land on the
    /// right cells.
    #[test]
    fn the_blank_cells_of_a_tab_are_left_out_of_the_text() {
        let columns = 160;
        let rows = [tabbed(
            &[
                "2026-10-28T09:01:09.123+0300",
                "INFO",
                "server/main.go:264",
                "listening",
                r#"{"port": 8080}"#,
            ],
            columns,
        )];
        assert!(looks_structured(&rows));

        let colors = LogColors::paint(&rows, columns);

        assert_eq!(
            colors.color_at(0, 32),
            Some(Color::Named(NamedColor::Green)),
            "the level sits at the first stop after the time"
        );
        assert_eq!(colors.color_at(0, 30), None, "the gap is left as it is");
        assert_eq!(
            colors.color_at(0, 40),
            Some(Color::Named(NamedColor::Cyan)),
            "the caller sits at the next stop"
        );
        assert_eq!(
            colors.color_at(0, 81),
            Some(Color::Named(NamedColor::BrightBlue)),
            "and the key of the fields after the message"
        );
    }

    /// A wide character is followed by a cell that only holds its place. Taken
    /// for text, that cell would split the value, and the log lens declines a
    /// logfmt line that has a stray word in it.
    #[test]
    fn the_place_holder_of_a_wide_character_is_left_out_of_the_text() {
        let columns = 40;
        let rows = [row_with_wide_characters(
            r#"level=info msg="启动""#,
            columns,
        )];
        assert!(looks_structured(&rows));

        let colors = LogColors::paint(&rows, columns);

        assert_eq!(
            colors.color_at(0, 0),
            Some(Color::Named(NamedColor::BrightBlue)),
            "the key of the first pair"
        );
        assert_eq!(
            colors.color_at(0, 16),
            Some(Color::Named(NamedColor::Green)),
            "the first wide character is part of the quoted value"
        );
        assert_eq!(
            colors.color_at(0, 18),
            Some(Color::Named(NamedColor::Green)),
            "and so is the second, past the cell that holds the place of the first"
        );
        assert_eq!(
            colors.color_at(0, 17),
            None,
            "the cell that only holds a place is no text and is left as it is"
        );
    }

    #[test]
    fn a_character_of_no_width_follows_the_one_it_belongs_to() {
        let mut accented = row("level=info msg=cafe", 30, false);
        if let Some(cell) = accented.cells.get_mut(18) {
            cell.zero_width = vec!['\u{301}'];
        }

        let (text, positions) = logical_text(&[accented]);

        assert_eq!(text, "level=info msg=cafe\u{301}");
        assert_eq!(positions.len(), text.chars().count());
        assert_eq!(positions.last(), Some(&(0, 18)));
    }

    #[test]
    fn a_cell_the_program_coloured_is_left_as_it_is() {
        let columns = 160;
        let mut rows = [row(ZAP_LINE, columns, false)];
        let level_column = ZAP_LINE.find("INFO").expect("the level is in the line");
        if let Some(cell) = rows[0].cells.get_mut(level_column) {
            cell.plain = false;
        }

        let colors = LogColors::paint(&rows, columns);

        assert_eq!(colors.color_at(0, level_column), None);
        assert_eq!(
            colors.color_at(0, level_column + 1),
            Some(Color::Named(NamedColor::Green)),
            "the rest of the word is still painted"
        );
    }

    #[test]
    fn nothing_is_painted_on_a_screen_of_no_width() {
        let colors = LogColors::paint(&[row(ZAP_LINE, 0, false)], 0);
        assert_eq!(colors.color_at(0, 0), None);
    }
}
