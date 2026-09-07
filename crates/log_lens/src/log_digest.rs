use crate::log_reader::{Field, Level, LogEvent, LogLine, LogParser};
use collections::HashMap;

/// What a fresh read of the terminal turned out to be, relative to what was
/// read before.
#[derive(Debug, PartialEq, Eq)]
pub enum Advance {
    /// Nothing but the still-unfinished last line changed.
    Unchanged,
    /// These complete lines are new at the bottom.
    Appended(String),
    /// The text no longer continues what was read, so everything must be read
    /// again: the terminal was cleared, or scrolled every line that had been
    /// read out of its scrollback.
    Restarted(String),
}

/// Finds the part of the terminal's text that has not been read yet.
///
/// The terminal hands back its whole grid on every read, and a chatty service
/// makes that thousands of lines. Lines only ever arrive at the bottom and only
/// ever leave from the top, so the text already read is kept and the new read is
/// matched against it: whatever follows the match is the only text the readers
/// see.
///
/// Matching on text alone cannot separate a run of byte-identical lines from a
/// scroll through that run, so the least-scrolled reading is taken. A terminal
/// whose scrollback has filled up and is repeating one line exactly can
/// therefore under-count that line; nothing in the terminal's own text can tell
/// the two cases apart.
#[derive(Default)]
pub struct TerminalTail {
    consumed: Option<String>,
}

impl TerminalTail {
    /// Splits `content` into the complete lines that are new and the last,
    /// still-unfinished line. The unfinished line is never consumed, so a line
    /// the program is still writing is read again each time it grows.
    pub fn advance<'a>(&mut self, content: &'a str) -> (Advance, &'a str) {
        let content = without_trailing_blank_lines(content);
        let (complete, provisional) = match content.rfind('\n') {
            Some(at) => (&content[..at], &content[at + 1..]),
            None => ("", content),
        };
        let advance = self.advance_over_complete(complete);
        (advance, provisional)
    }

    fn advance_over_complete(&mut self, complete: &str) -> Advance {
        let consumed = self.consumed.replace(complete.to_string());
        let Some(consumed) = consumed.filter(|consumed| !consumed.is_empty()) else {
            return if complete.is_empty() {
                Advance::Unchanged
            } else {
                Advance::Restarted(complete.to_string())
            };
        };
        let mut from = 0;
        loop {
            let kept = &consumed[from..];
            if let Some(appended) = complete.strip_prefix(kept) {
                let appended = appended.strip_prefix('\n').unwrap_or(appended);
                return if appended.is_empty() {
                    Advance::Unchanged
                } else {
                    Advance::Appended(appended.to_string())
                };
            }
            match kept.find('\n') {
                Some(at) => from += at + 1,
                None => return Advance::Restarted(complete.to_string()),
            }
        }
    }

    pub fn forget(&mut self) {
        self.consumed = None;
    }
}

/// The terminal pads its screen with blank rows below the cursor, and those rows
/// fill in as output arrives. Dropping them keeps the read text a suffix-stable
/// stream, which is what lets the anchor above recognise it again.
fn without_trailing_blank_lines(content: &str) -> &str {
    let mut end = content.len();
    loop {
        let line_start = content[..end].rfind('\n').map(|at| at + 1).unwrap_or(0);
        if !content[line_start..end].trim().is_empty() {
            return &content[..end];
        }
        if line_start == 0 {
            return "";
        }
        end = line_start - 1;
    }
}

#[derive(Default)]
struct FieldTally {
    first_value: String,
    events_carrying_it: usize,
    varies: bool,
}

/// One rendered row: either a single line, or a run of consecutive events that
/// said the same thing.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Row {
    /// Indices into the digest's lines, in arrival order. Never emptied, so the
    /// originals of a collapsed run stay reachable.
    pub lines: Vec<usize>,
}

impl Row {
    pub fn count(&self) -> usize {
        self.lines.len()
    }

    pub fn is_collapsed(&self) -> bool {
        self.lines.len() > 1
    }
}

/// Everything the lens knows about the output read so far.
#[derive(Default)]
pub struct LogDigest {
    lines: Vec<LogLine>,
    rows: Vec<Row>,
    parser: LogParser,
    tally: HashMap<String, FieldTally>,
    /// Field names in the order they were first seen, so the header does not
    /// reshuffle as more output arrives.
    field_order: Vec<String>,
    read_count: usize,
    tallied_upto: usize,
    grouped_upto: usize,
}

impl LogDigest {
    pub fn lines(&self) -> &[LogLine] {
        &self.lines
    }

    pub fn rows(&self) -> &[Row] {
        &self.rows
    }

    pub fn line(&self, at: usize) -> Option<&LogLine> {
        self.lines.get(at)
    }

    pub fn event(&self, at: usize) -> Option<&LogEvent> {
        match self.lines.get(at)? {
            LogLine::Read(event) => Some(event),
            LogLine::Raw(_) => None,
        }
    }

    pub fn clear(&mut self) {
        self.lines.clear();
        self.rows.clear();
        self.tally.clear();
        self.field_order.clear();
        self.read_count = 0;
        self.tallied_upto = 0;
        self.grouped_upto = 0;
        self.parser.reset();
    }

    pub fn read(&mut self, text: &str) {
        let changed = self.parser.read_into(text, &mut self.lines);
        self.retally(changed.clone());
        self.regroup(changed);
    }

    /// A field is folded once every event read so far carries it with the same
    /// value. Absence counts as disagreement, because a header claiming a value
    /// for a line that never said it would be a lie.
    pub fn folded_fields(&self) -> Vec<Field> {
        if self.read_count == 0 {
            return Vec::new();
        }
        self.field_order
            .iter()
            .filter_map(|name| {
                let tally = self.tally.get(name)?;
                (!tally.varies && tally.events_carrying_it == self.read_count).then(|| Field {
                    name: name.clone(),
                    value: tally.first_value.clone(),
                })
            })
            .collect()
    }

    pub fn is_folded(&self, name: &str) -> bool {
        self.read_count > 0
            && self
                .tally
                .get(name)
                .is_some_and(|tally| !tally.varies && tally.events_carrying_it == self.read_count)
    }

    /// The fields of one event that still belong on its row.
    pub fn unfolded_fields<'a>(&self, event: &'a LogEvent) -> Vec<&'a Field> {
        event
            .fields
            .iter()
            .filter(|field| !self.is_folded(&field.name))
            .collect()
    }

    /// What differed across a collapsed run, one phrase per field that varied.
    pub fn differences(&self, row: &Row) -> Vec<String> {
        if !row.is_collapsed() {
            return Vec::new();
        }
        let mut names = Vec::new();
        for at in &row.lines {
            let Some(event) = self.event(*at) else {
                continue;
            };
            for field in &event.fields {
                if !names.contains(&field.name) {
                    names.push(field.name.clone());
                }
            }
        }
        names
            .into_iter()
            .filter_map(|name| self.difference_for(row, &name))
            .collect()
    }

    fn difference_for(&self, row: &Row, name: &str) -> Option<String> {
        let values: Vec<&str> = row
            .lines
            .iter()
            .filter_map(|at| self.event(*at))
            .filter_map(|event| event.field(name))
            .collect();
        if values.len() < 2 || values.iter().all(|value| *value == values[0]) {
            return None;
        }
        let mut distinct: Vec<&str> = Vec::new();
        for value in &values {
            if !distinct.contains(value) {
                distinct.push(value);
            }
        }
        if let Some(numbers) = contiguous_range(&distinct) {
            return Some(format!("{name} {}–{}", numbers.0, numbers.1));
        }
        Some(format!("{name} {} values", distinct.len()))
    }

    fn retally(&mut self, changed: std::ops::Range<usize>) {
        // A block growing into a row that was already tallied reopens the
        // range, but its fields were counted the first time round.
        for at in changed.start.max(self.tallied_upto)..changed.end {
            let Some(LogLine::Read(event)) = self.lines.get(at) else {
                continue;
            };
            self.read_count += 1;
            let fields = event.fields.clone();
            for field in fields {
                match self.tally.get_mut(&field.name) {
                    Some(tally) => {
                        tally.events_carrying_it += 1;
                        tally.varies |= tally.first_value != field.value;
                    }
                    None => {
                        self.field_order.push(field.name.clone());
                        self.tally.insert(
                            field.name,
                            FieldTally {
                                first_value: field.value,
                                events_carrying_it: 1,
                                varies: false,
                            },
                        );
                    }
                }
            }
        }
        self.tallied_upto = changed.end.max(self.tallied_upto);
    }

    fn regroup(&mut self, changed: std::ops::Range<usize>) {
        for at in changed.start.max(self.grouped_upto)..changed.end {
            if self.joins_previous_row(at) {
                if let Some(row) = self.rows.last_mut() {
                    row.lines.push(at);
                    continue;
                }
            }
            self.rows.push(Row { lines: vec![at] });
        }
        self.grouped_upto = changed.end.max(self.grouped_upto);
    }

    /// A run collapses on level, caller and message: the three things that say
    /// "this is the same event again". Fields are deliberately not part of the
    /// key, since the one that differs is what the count summarises.
    fn joins_previous_row(&self, at: usize) -> bool {
        let Some(previous) = self.rows.last().and_then(|row| row.lines.last().copied()) else {
            return false;
        };
        if previous >= at {
            return false;
        }
        let (Some(event), Some(previous_event)) = (self.event(at), self.event(previous)) else {
            return false;
        };
        event.block.is_empty()
            && previous_event.block.is_empty()
            && event.level == previous_event.level
            && event.caller == previous_event.caller
            && event.message == previous_event.message
    }

    pub fn matches_filter(&self, row: &Row, threshold: Level, needle: &str) -> bool {
        let needle = needle.trim().to_lowercase();
        row.lines.iter().any(|at| match self.lines.get(*at) {
            Some(LogLine::Read(event)) => {
                event.level.is_none_or(|level| level >= threshold)
                    && (needle.is_empty() || event.message.to_lowercase().contains(&needle))
            }
            Some(LogLine::Raw(text)) => needle.is_empty() || text.to_lowercase().contains(&needle),
            None => false,
        })
    }
}

/// `Some((low, high))` when the values are integers covering every step from
/// `low` to `high` with nothing missing.
fn contiguous_range(values: &[&str]) -> Option<(i64, i64)> {
    if values.len() < 2 {
        return None;
    }
    let mut numbers: Vec<i64> = values
        .iter()
        .map(|value| value.trim().parse::<i64>().ok())
        .collect::<Option<Vec<i64>>>()?;
    numbers.sort_unstable();
    numbers.dedup();
    let low = *numbers.first()?;
    let high = *numbers.last()?;
    let span = high.checked_sub(low)?.checked_add(1)?;
    (span == numbers.len() as i64).then_some((low, high))
}
