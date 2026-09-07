use chrono::{DateTime, FixedOffset, TimeZone as _, Utc};
use regex::Regex;
use std::sync::LazyLock;

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Level {
    Trace,
    Debug,
    Info,
    Warn,
    Error,
    Fatal,
}

impl Level {
    pub fn from_word(word: &str) -> Option<Self> {
        let word = word.trim();
        if word.is_empty() || word.len() > 11 {
            return None;
        }
        match word.to_ascii_uppercase().as_str() {
            "TRACE" | "TRC" | "VERBOSE" => Some(Level::Trace),
            "DEBUG" | "DBG" | "DEBU" | "FINE" => Some(Level::Debug),
            "INFO" | "INF" | "NOTICE" | "INFORMATION" => Some(Level::Info),
            "WARN" | "WARNING" | "WRN" => Some(Level::Warn),
            "ERROR" | "ERR" | "SEVERE" | "CRITICAL" | "CRIT" => Some(Level::Error),
            "FATAL" | "PANIC" | "DPANIC" | "EMERGENCY" => Some(Level::Fatal),
            _ => None,
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Level::Trace => "TRACE",
            Level::Debug => "DEBUG",
            Level::Info => "INFO",
            Level::Warn => "WARN",
            Level::Error => "ERROR",
            Level::Fatal => "FATAL",
        }
    }

    pub const THRESHOLDS: [Level; 4] = [Level::Debug, Level::Info, Level::Warn, Level::Error];
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Field {
    pub name: String,
    pub value: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LogEvent {
    /// The line exactly as it arrived, so that a row can always answer with the
    /// text it was built from.
    pub source: String,
    /// Time of day, already reduced to what a reader needs to compare two
    /// nearby lines. The date is dropped because every line of one run shares
    /// it, which is the same reason the folded fields leave the rows.
    pub time: Option<String>,
    pub level: Option<Level>,
    pub caller: Option<String>,
    pub message: String,
    pub fields: Vec<Field>,
    /// Verbatim lines that belong to this event rather than to a row of their
    /// own: a Python traceback, a JVM stack trace.
    pub block: Vec<String>,
}

impl LogEvent {
    pub fn field(&self, name: &str) -> Option<&str> {
        self.fields
            .iter()
            .find(|field| field.name == name)
            .map(|field| field.value.as_str())
    }
}

/// One row's worth of source text, either understood or handed back untouched.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LogLine {
    Read(LogEvent),
    /// A line no reader claimed, kept exactly as it arrived.
    Raw(String),
}

impl LogLine {
    /// The source text this line was built from, byte for byte.
    pub fn source_text(&self) -> String {
        match self {
            LogLine::Raw(text) => text.clone(),
            LogLine::Read(event) => {
                let mut text = event.source.clone();
                for line in &event.block {
                    text.push('\n');
                    text.push_str(line);
                }
                text
            }
        }
    }
}

/// Turns terminal text into rows, keeping enough state across calls to attach a
/// stack trace to the event it belongs to.
#[derive(Default)]
pub struct LogParser {
    block: Option<OpenBlock>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum BlockKind {
    /// `Traceback (most recent call last):`, its indented frames, and the one
    /// unindented exception line that closes it.
    PythonTraceback,
    /// A JVM exception header and its `at …` / `Caused by:` frames.
    JvmStackTrace,
}

struct OpenBlock {
    kind: BlockKind,
    /// Index of the row the block's lines are being appended to.
    row: usize,
    frames_seen: bool,
}

impl LogParser {
    /// Appends `text`'s lines to `rows`, returning the range of rows that
    /// changed. A block that is still open keeps growing into the row it
    /// started on, so the same row index can be returned twice.
    pub fn read_into(&mut self, text: &str, rows: &mut Vec<LogLine>) -> std::ops::Range<usize> {
        let mut lowest_changed = rows.len();
        for line in text.split('\n') {
            if let Some(row) = self.continue_block(line, rows) {
                lowest_changed = lowest_changed.min(row);
                continue;
            }
            match read_line(line) {
                Some(event) => {
                    rows.push(LogLine::Read(event));
                    self.block = None;
                }
                None => {
                    if let Some(row) = self.start_block(line, rows) {
                        lowest_changed = lowest_changed.min(row);
                    } else if !line.trim().is_empty() {
                        rows.push(LogLine::Raw(line.to_string()));
                    }
                }
            }
        }
        lowest_changed..rows.len()
    }

    /// Forgets the open block, so that a re-read of the whole terminal does not
    /// append to a row that no longer exists.
    pub fn reset(&mut self) {
        self.block = None;
    }

    fn start_block(&mut self, line: &str, rows: &mut [LogLine]) -> Option<usize> {
        let kind = block_kind_started_by(line)?;
        let row = last_read_row(rows)?;
        let LogLine::Read(event) = &mut rows[row] else {
            return None;
        };
        event.block.push(line.to_string());
        self.block = Some(OpenBlock {
            kind,
            row,
            frames_seen: false,
        });
        Some(row)
    }

    fn continue_block(&mut self, line: &str, rows: &mut [LogLine]) -> Option<usize> {
        let (kind, row, frames_seen) = {
            let block = self.block.as_ref()?;
            (block.kind, block.row, block.frames_seen)
        };
        let indented = line.starts_with(' ') || line.starts_with('\t');
        // The line that closes a Python traceback is the exception summary,
        // which is not indented. Requiring that no reader claims it keeps the
        // next log line of the run out of the block.
        let closes_python_traceback = kind == BlockKind::PythonTraceback
            && frames_seen
            && !indented
            && read_line(line).is_none();
        let continues = indented
            || line.starts_with("Caused by:")
            || (kind == BlockKind::JvmStackTrace && looks_like_jvm_exception(line))
            || closes_python_traceback;
        if !continues {
            self.block = None;
            return None;
        }
        let Some(LogLine::Read(event)) = rows.get_mut(row) else {
            self.block = None;
            return None;
        };
        event.block.push(line.to_string());
        if closes_python_traceback {
            self.block = None;
        } else if let Some(block) = self.block.as_mut() {
            block.frames_seen |= indented;
        }
        Some(row)
    }
}

fn last_read_row(rows: &[LogLine]) -> Option<usize> {
    let row = rows.len().checked_sub(1)?;
    matches!(rows[row], LogLine::Read(_)).then_some(row)
}

const PYTHON_TRACEBACK_HEADER: &str = "Traceback (most recent call last):";

fn block_kind_started_by(line: &str) -> Option<BlockKind> {
    if line.trim_end() == PYTHON_TRACEBACK_HEADER {
        return Some(BlockKind::PythonTraceback);
    }
    looks_like_jvm_exception(line).then_some(BlockKind::JvmStackTrace)
}

/// A JVM stack trace opens with a fully qualified throwable name, optionally
/// followed by a message. Requiring the dotted package keeps a bare English
/// sentence that happens to contain the word "Error" out of the block.
fn looks_like_jvm_exception(line: &str) -> bool {
    static JVM_EXCEPTION: LazyLock<Option<Regex>> = LazyLock::new(|| {
        Regex::new(r"^(?:Caused by: )?[a-zA-Z_][\w$]*(?:\.[a-zA-Z_][\w$]*)+(?:Exception|Error|Throwable)(?::.*)?$").ok()
    });
    JVM_EXCEPTION
        .as_ref()
        .is_some_and(|pattern| pattern.is_match(line))
}

/// Every reader in the order the design fixes, each certain or declining.
pub fn read_line(line: &str) -> Option<LogEvent> {
    if line.trim().is_empty() {
        return None;
    }
    read_zap_console(line)
        .or_else(|| read_json_line(line))
        .or_else(|| read_logfmt(line))
        .or_else(|| read_rust_tracing(line))
        .or_else(|| read_python_logging(line))
        .or_else(|| read_jvm_log(line))
}

fn read_zap_console(line: &str) -> Option<LogEvent> {
    let parts: Vec<&str> = line.split('\t').collect();
    if parts.len() < 4 {
        return None;
    }
    let time = read_timestamp_text(parts[0])?;
    let level = Level::from_word(parts[1])?;
    let caller = parts[2];
    if !looks_like_caller(caller) {
        return None;
    }
    let (message, fields) = match parts.len() {
        4 => (parts[3].to_string(), Vec::new()),
        _ => {
            let last = parts[parts.len() - 1];
            match json_object_fields(last) {
                Some(fields) => (parts[3..parts.len() - 1].join("\t"), fields),
                None => (parts[3..].join("\t"), Vec::new()),
            }
        }
    };
    Some(LogEvent {
        source: line.to_string(),
        time: Some(time),
        level: Some(level),
        caller: Some(caller.to_string()),
        message,
        fields,
        block: Vec::new(),
    })
}

fn read_json_line(line: &str) -> Option<LogEvent> {
    let trimmed = line.trim();
    if !trimmed.starts_with('{') || !trimmed.ends_with('}') {
        return None;
    }
    let serde_json::Value::Object(object) = serde_json::from_str(trimmed).ok()? else {
        return None;
    };
    let mut level = None;
    let mut time = None;
    let mut message = None;
    let mut caller = None;
    let mut fields = Vec::new();
    for (name, value) in object {
        match name.as_str() {
            "level" | "lvl" | "severity" if level.is_none() => {
                level = value.as_str().and_then(Level::from_word);
                if level.is_none() {
                    return None;
                }
            }
            "ts" | "time" | "timestamp" | "@t" if time.is_none() => {
                time = json_timestamp_text(&value);
            }
            "msg" | "message" if message.is_none() => {
                message = value.as_str().map(str::to_string);
            }
            "caller" | "logger" | "source" if caller.is_none() => {
                caller = value.as_str().map(str::to_string);
            }
            _ => fields.push(Field {
                name,
                value: json_field_text(&value),
            }),
        }
    }
    Some(LogEvent {
        source: line.to_string(),
        time,
        level: Some(level?),
        caller,
        message: message.unwrap_or_default(),
        fields,
        block: Vec::new(),
    })
}

fn read_logfmt(line: &str) -> Option<LogEvent> {
    let pairs = logfmt_pairs(line)?;
    let mut level = None;
    let mut time = None;
    let mut message = None;
    let mut caller = None;
    let mut fields = Vec::new();
    for (name, value) in pairs {
        match name.as_str() {
            "level" | "lvl" | "severity" if level.is_none() => {
                level = Level::from_word(&value);
                if level.is_none() {
                    return None;
                }
            }
            "ts" | "time" | "timestamp" if time.is_none() => {
                time = read_timestamp_text(&value);
            }
            "msg" | "message" if message.is_none() => message = Some(value),
            "caller" | "logger" | "source" if caller.is_none() => caller = Some(value),
            _ => fields.push(Field { name, value }),
        }
    }
    Some(LogEvent {
        source: line.to_string(),
        time,
        level: Some(level?),
        caller,
        message: message.unwrap_or_default(),
        fields,
        block: Vec::new(),
    })
}

/// Declines unless the whole line is `key=value` pairs, so that an ordinary
/// sentence containing one `=` is never claimed.
fn logfmt_pairs(line: &str) -> Option<Vec<(String, String)>> {
    let mut pairs = Vec::new();
    let mut rest = line.trim();
    if rest.is_empty() {
        return None;
    }
    while !rest.is_empty() {
        let equals = rest.find('=')?;
        let name = &rest[..equals];
        if !is_logfmt_key(name) {
            return None;
        }
        let after = &rest[equals + 1..];
        let (value, remainder) = if let Some(quoted) = after.strip_prefix('"') {
            let (value, remainder) = read_quoted(quoted)?;
            (value, remainder)
        } else {
            let end = after.find(' ').unwrap_or(after.len());
            (after[..end].to_string(), &after[end..])
        };
        pairs.push((name.to_string(), value));
        rest = remainder.trim_start_matches(' ');
    }
    (!pairs.is_empty()).then_some(pairs)
}

fn is_logfmt_key(name: &str) -> bool {
    let mut characters = name.chars();
    characters
        .next()
        .is_some_and(|first| first.is_ascii_alphabetic() || first == '_')
        && characters.all(|character| {
            character.is_ascii_alphanumeric() || matches!(character, '_' | '.' | '-')
        })
}

fn read_quoted(after_quote: &str) -> Option<(String, &str)> {
    let mut value = String::new();
    let mut characters = after_quote.char_indices();
    while let Some((at, character)) = characters.next() {
        match character {
            '"' => return Some((value, &after_quote[at + 1..])),
            '\\' => {
                let (_, escaped) = characters.next()?;
                value.push(match escaped {
                    'n' => '\n',
                    't' => '\t',
                    other => other,
                });
            }
            other => value.push(other),
        }
    }
    None
}

fn read_rust_tracing(line: &str) -> Option<LogEvent> {
    static PLAIN: LazyLock<Option<Regex>> = LazyLock::new(|| {
        Regex::new(r"^(?<ts>\S+)\s+(?<level>[A-Z]{4,7})\s+(?<target>[\w:./\-]+):\s(?<msg>.*)$").ok()
    });
    static BRACKETED: LazyLock<Option<Regex>> = LazyLock::new(|| {
        Regex::new(r"^\[(?<ts>\S+)\s+(?<level>[A-Z]{4,7})\s*(?<target>[^\]]*)\]\s*(?<msg>.*)$").ok()
    });
    for pattern in [PLAIN.as_ref()?, BRACKETED.as_ref()?] {
        let Some(captured) = pattern.captures(line) else {
            continue;
        };
        let Some(time) = read_timestamp_text(captured["ts"].trim()) else {
            continue;
        };
        let Some(level) = Level::from_word(&captured["level"]) else {
            continue;
        };
        let target = captured["target"].trim();
        return Some(LogEvent {
            source: line.to_string(),
            time: Some(time),
            level: Some(level),
            caller: (!target.is_empty()).then(|| target.to_string()),
            message: captured["msg"].to_string(),
            fields: Vec::new(),
            block: Vec::new(),
        });
    }
    None
}

fn read_python_logging(line: &str) -> Option<LogEvent> {
    static PATTERN: LazyLock<Option<Regex>> = LazyLock::new(|| {
        Regex::new(
            r"^\d{4}-\d{2}-\d{2} (?<time>\d{2}:\d{2}:\d{2})[,.](?<millis>\d{3}) (?<level>[A-Za-z]+) (?<logger>[\w.\-]+): (?<msg>.*)$",
        )
        .ok()
    });
    let captured = PATTERN.as_ref()?.captures(line)?;
    let level = Level::from_word(&captured["level"])?;
    Some(LogEvent {
        source: line.to_string(),
        time: Some(format!("{}.{}", &captured["time"], &captured["millis"])),
        level: Some(level),
        caller: Some(captured["logger"].to_string()),
        message: captured["msg"].to_string(),
        fields: Vec::new(),
        block: Vec::new(),
    })
}

fn read_jvm_log(line: &str) -> Option<LogEvent> {
    static PATTERN: LazyLock<Option<Regex>> = LazyLock::new(|| {
        Regex::new(
            r"^(?<ts>\d{2}:\d{2}:\d{2}[.,]\d{3}) \[(?<thread>[^\]]*)\] (?<level>[A-Za-z]+)\s+(?<logger>[\w.$]+) - (?<msg>.*)$",
        )
        .ok()
    });
    let captured = PATTERN.as_ref()?.captures(line)?;
    let level = Level::from_word(&captured["level"])?;
    let thread = captured["thread"].to_string();
    Some(LogEvent {
        source: line.to_string(),
        time: Some(captured["ts"].replace(',', ".")),
        level: Some(level),
        caller: Some(captured["logger"].to_string()),
        message: captured["msg"].to_string(),
        fields: vec![Field {
            name: "thread".to_string(),
            value: thread,
        }],
        block: Vec::new(),
    })
}

/// A caller is only accepted in the shape every logger writes it, `path:line`,
/// so that an ordinary tab-separated column is never mistaken for one.
fn looks_like_caller(text: &str) -> bool {
    let Some((path, line_number)) = text.rsplit_once(':') else {
        return false;
    };
    !path.is_empty()
        && !path.contains(char::is_whitespace)
        && !line_number.is_empty()
        && line_number.bytes().all(|byte| byte.is_ascii_digit())
}

/// Accepts RFC3339 both with and without the colon in the offset, because zap's
/// ISO8601 encoder writes `+0300` while `Z`-suffixed output is strict RFC3339.
fn read_timestamp_text(text: &str) -> Option<String> {
    if let Ok(parsed) = DateTime::parse_from_rfc3339(text) {
        return Some(time_of_day(&parsed));
    }
    for format in ["%Y-%m-%dT%H:%M:%S%.f%z", "%Y-%m-%d %H:%M:%S%.f%z"] {
        if let Ok(parsed) = DateTime::parse_from_str(text, format) {
            return Some(time_of_day(&parsed));
        }
    }
    None
}

fn time_of_day(moment: &DateTime<FixedOffset>) -> String {
    moment.format("%H:%M:%S%.3f").to_string()
}

fn json_timestamp_text(value: &serde_json::Value) -> Option<String> {
    if let Some(text) = value.as_str() {
        return read_timestamp_text(text);
    }
    let seconds = value.as_f64()?;
    if !seconds.is_finite() {
        return None;
    }
    let whole = seconds.trunc();
    let nanoseconds = ((seconds - whole) * 1e9).round().clamp(0., 999_999_999.) as u32;
    let moment = Utc
        .timestamp_opt(whole as i64, nanoseconds)
        .single()?
        .fixed_offset();
    Some(time_of_day(&moment))
}

fn json_object_fields(text: &str) -> Option<Vec<Field>> {
    let trimmed = text.trim();
    if !trimmed.starts_with('{') || !trimmed.ends_with('}') {
        return None;
    }
    let serde_json::Value::Object(object) = serde_json::from_str(trimmed).ok()? else {
        return None;
    };
    Some(
        object
            .into_iter()
            .map(|(name, value)| Field {
                name,
                value: json_field_text(&value),
            })
            .collect(),
    )
}

fn json_field_text(value: &serde_json::Value) -> String {
    match value {
        serde_json::Value::String(text) => text.clone(),
        other => other.to_string(),
    }
}
