use std::ops::Range;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::{Context as _, Result};
use serde_json::Value;
use settings_json::{
    append_top_level_array_value_in_json_text, infer_json_indent_size, parse_json_with_comments,
    replace_top_level_array_value_in_json_text,
};
use task::{DebugScenario, TaskTemplate};

/// Which of the two files a configuration lives in.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Kind {
    /// `.zed/tasks.json`: something to run.
    Task,
    /// `.zed/debug.json`: something to debug.
    Debug,
}

impl Kind {
    pub fn file_name(self) -> &'static str {
        match self {
            Kind::Task => "tasks.json",
            Kind::Debug => "debug.json",
        }
    }

    /// What an empty file of this kind starts as, so a reader who opens it finds
    /// something to read rather than nothing.
    pub fn empty_file(self) -> &'static str {
        match self {
            Kind::Task => {
                "// The tasks of this project. Every one of them can also be\n\
                           // written here by hand -- the editor reads this file, and shows\n\
                           // whatever it says.\n[]\n"
            }
            Kind::Debug => "// The debug configurations of this project.\n[]\n",
        }
    }
}

/// One configuration as it is held in a file: what it says, and where in the file
/// it sits.
///
/// The place is the index in the file's array, because neither file gives its
/// entries an identity of their own -- two configurations may even carry the same
/// label. Everything that writes back therefore edits by index.
#[derive(Clone, Debug)]
pub struct Configuration {
    pub kind: Kind,
    pub at: usize,
    pub label: String,
    /// What it is, as the editor's own types understand it. None when the file
    /// holds something the editor cannot make sense of -- shown to the reader as
    /// it is rather than hidden.
    pub task: Option<TaskTemplate>,
    pub scenario: Option<DebugScenario>,
    /// The entry exactly as the file has it, so writing one configuration back
    /// never disturbs what the reader wrote in another.
    pub as_written: Value,
}

impl Configuration {
    /// What to show when there is no label worth showing.
    pub fn shown_label(&self) -> String {
        match self.label.trim().is_empty() {
            true => format!("(no name, {} in the file)", self.at + 1),
            false => self.label.clone(),
        }
    }
}

/// What a file of configurations holds, and what was wrong with it if anything
/// was.
#[derive(Clone, Debug, Default)]
pub struct FileContents {
    pub configurations: Vec<Configuration>,
    /// What the file says, kept so an edit can be written back into it without
    /// touching anything else in it.
    pub text: String,
    /// Set when the file could not be read as an array of configurations at all.
    /// The reader is told rather than shown an empty list.
    pub trouble: Option<String>,
}

/// Reads what `text` says. Comments and trailing commas are allowed, as they are
/// everywhere else in the editor's own configuration; an entry that makes no sense
/// is kept as it was written rather than dropped, so nothing the reader typed
/// disappears from the list.
pub fn read(kind: Kind, text: &str) -> FileContents {
    if text.trim().is_empty() {
        return FileContents {
            configurations: Vec::new(),
            text: text.to_string(),
            trouble: None,
        };
    }
    let entries: Vec<Value> = match parse_json_with_comments(text) {
        Ok(entries) => entries,
        Err(error) => {
            return FileContents {
                configurations: Vec::new(),
                text: text.to_string(),
                trouble: Some(format!("{error}")),
            };
        }
    };

    let configurations = entries
        .into_iter()
        .enumerate()
        .map(|(at, entry)| {
            let (label, task, scenario) = match kind {
                Kind::Task => {
                    let task: Option<TaskTemplate> = serde_json::from_value(entry.clone()).ok();
                    let label = task
                        .as_ref()
                        .map(|task| task.label.clone())
                        .or_else(|| label_in(&entry))
                        .unwrap_or_default();
                    (label, task, None)
                }
                Kind::Debug => {
                    let scenario: Option<DebugScenario> =
                        serde_json::from_value(entry.clone()).ok();
                    let label = scenario
                        .as_ref()
                        .map(|scenario| scenario.label.to_string())
                        .or_else(|| label_in(&entry))
                        .unwrap_or_default();
                    (label, None, scenario)
                }
            };
            Configuration {
                kind,
                at,
                label,
                task,
                scenario,
                as_written: entry,
            }
        })
        .collect();

    FileContents {
        configurations,
        text: text.to_string(),
        trouble: None,
    }
}

fn label_in(entry: &Value) -> Option<String> {
    entry
        .get("label")
        .and_then(Value::as_str)
        .map(str::to_string)
}

/// A task as it should be written: without the fields it never set, so the file
/// stays something a person can read. Writing every field of the type would bury
/// three lines the reader cares about under a dozen defaults.
pub fn task_as_written(task: &TaskTemplate) -> Result<Value> {
    let mut written = serde_json::to_value(task).context("writing the task")?;
    let default = serde_json::to_value(TaskTemplate::default()).context("the default task")?;
    if let (Value::Object(written_fields), Value::Object(default_fields)) = (&mut written, &default)
    {
        written_fields.retain(|name, value| {
            // Never dropped: without these two a task is not a task, and the
            // reader would be left with an entry the editor ignores.
            if name == "label" || name == "command" {
                return true;
            }
            default_fields.get(name) != Some(value)
        });
    }
    Ok(written)
}

/// A debug configuration as it should be written. Its own type already leaves out
/// what it did not set, and the adapter's own keys are flattened into it, so it is
/// written as it is.
pub fn scenario_as_written(scenario: &DebugScenario) -> Result<Value> {
    serde_json::to_value(scenario).context("writing the debug configuration")
}

/// Where the two files of a project live.
pub fn file_path(project_root: &Path, kind: Kind) -> PathBuf {
    project_root.join(".zed").join(kind.file_name())
}

/// The text a file should hold after `entry` is put at `at`, or appended when
/// `at` is None. Comments, spacing and everything else in the file are kept: this
/// is a file the reader may also be editing by hand.
pub fn text_with(text: &str, at: Option<usize>, entry: &Value) -> String {
    let text = match text.trim().is_empty() {
        true => "[]\n".to_string(),
        false => text.to_string(),
    };
    let indent = infer_json_indent_size(&text);
    let (range, replacement) = match at {
        Some(at) => replace_top_level_array_value_in_json_text(
            &text,
            &[] as &[&str],
            Some(entry),
            None,
            at,
            indent,
        ),
        None => append_top_level_array_value_in_json_text(&text, entry, indent),
    };
    with_replacement(&text, range, &replacement)
}

/// Where the entry that was read as `original` sits in `text` now, if it is still
/// there at all.
///
/// The index a view remembers is only a hint: the file may have been edited by
/// hand since it was read, and writing by a stale index would put one
/// configuration on top of another.
pub fn place_of(text: &str, at: usize, original: &Value) -> Option<usize> {
    let entries: Vec<Value> = parse_json_with_comments(text).ok()?;
    if entries.get(at) == Some(original) {
        return Some(at);
    }
    entries.iter().position(|entry| entry == original)
}

/// The text a file should hold once the entry at `at` is gone.
pub fn text_without(text: &str, at: usize) -> String {
    let indent = infer_json_indent_size(text);
    let (range, replacement) =
        replace_top_level_array_value_in_json_text(text, &[] as &[&str], None, None, at, indent);
    with_replacement(text, range, &replacement)
}

fn with_replacement(text: &str, range: Range<usize>, replacement: &str) -> String {
    let mut written = String::with_capacity(text.len() + replacement.len());
    written.push_str(&text[..range.start]);
    written.push_str(replacement);
    written.push_str(&text[range.end..]);
    written
}

/// Writes `text` to `path`, making the `.zed` directory if it is not there yet.
pub async fn write(fs: &Arc<dyn fs::Fs>, path: &Path, text: &str) -> Result<()> {
    if let Some(directory) = path.parent()
        && !fs.is_dir(directory).await
    {
        fs.create_dir(directory)
            .await
            .with_context(|| format!("making {}", directory.display()))?;
    }
    fs.atomic_write(path.to_path_buf(), text.to_string())
        .await
        .with_context(|| format!("writing {}", path.display()))
}

#[cfg(test)]
mod tests {
    use super::*;

    const A_FILE: &str = r#"// The project's tasks.
[
  {
    "label": "api server",
    "command": "go run ./cmd/api",
    // The port this one listens on.
    "env": { "PORT": "8080" }
  },
  {
    "label": "unit tests",
    "command": "go test ./..."
  }
]
"#;

    #[test]
    fn what_the_file_says_is_what_is_listed() {
        let read = read(Kind::Task, A_FILE);

        assert!(read.trouble.is_none(), "{:?}", read.trouble);
        assert_eq!(read.configurations.len(), 2);
        assert_eq!(read.configurations[0].label, "api server");
        assert_eq!(read.configurations[0].at, 0);
        assert_eq!(
            read.configurations[0]
                .task
                .as_ref()
                .expect("the first is a task the editor understands")
                .env
                .get("PORT")
                .map(String::as_str),
            Some("8080")
        );
        assert_eq!(read.configurations[1].label, "unit tests");
    }

    #[test]
    fn an_entry_the_editor_cannot_read_is_still_listed() {
        let read = read(
            Kind::Task,
            r#"[{"label": "fine", "command": "true"}, {"name": "wrong shape"}]"#,
        );

        assert_eq!(
            read.configurations.len(),
            2,
            "an entry the editor makes nothing of is still the reader's, and \
             hiding it would look like it had been deleted"
        );
        assert!(read.configurations[1].task.is_none());
        assert!(
            read.trouble.is_none(),
            "one entry the editor cannot read is not a broken file"
        );
    }

    #[test]
    fn a_file_that_is_not_a_list_says_so() {
        let read = read(Kind::Task, "{ \"label\": \"not a list\" }");

        assert!(read.configurations.is_empty());
        assert!(
            read.trouble.is_some(),
            "the reader has to be told, not shown an empty list as though the file \
             held nothing"
        );
    }

    #[test]
    fn writing_one_task_leaves_the_rest_of_the_file_alone() {
        let mut task = TaskTemplate {
            label: "unit tests".to_string(),
            command: "go test -race ./...".to_string(),
            ..TaskTemplate::default()
        };
        task.env.insert("CGO_ENABLED".to_string(), "1".to_string());

        let written = text_with(
            A_FILE,
            Some(1),
            &task_as_written(&task).expect("the task can be written"),
        );

        assert!(
            written.contains("// The project's tasks."),
            "the reader's own comment at the top has to survive: {written}"
        );
        assert!(
            written.contains("// The port this one listens on."),
            "and so does a comment inside another entry: {written}"
        );
        assert!(written.contains("go test -race ./..."));
        assert!(
            !written.contains("go test ./..."),
            "the entry that was replaced should be gone: {written}"
        );

        let read_back = read(Kind::Task, &written);
        assert_eq!(read_back.configurations.len(), 2);
        assert_eq!(read_back.configurations[1].label, "unit tests");
    }

    #[test]
    fn a_task_is_written_without_the_fields_it_never_set() {
        let task = TaskTemplate {
            label: "run".to_string(),
            command: "./run".to_string(),
            ..TaskTemplate::default()
        };

        let written = task_as_written(&task).expect("the task can be written");
        let mut fields: Vec<&str> = written
            .as_object()
            .expect("an object")
            .keys()
            .map(String::as_str)
            .collect();
        fields.sort();

        assert_eq!(
            fields,
            vec!["command", "label"],
            "a task nobody configured further is two lines, not sixteen: {written}"
        );
    }

    #[test]
    fn appending_to_an_empty_file_gives_a_list_of_one() {
        let task = TaskTemplate {
            label: "first".to_string(),
            command: "true".to_string(),
            ..TaskTemplate::default()
        };

        let written = text_with(
            Kind::Task.empty_file(),
            None,
            &task_as_written(&task).expect("the task can be written"),
        );

        let read_back = read(Kind::Task, &written);
        assert!(read_back.trouble.is_none(), "{:?}", read_back.trouble);
        assert_eq!(read_back.configurations.len(), 1);
        assert_eq!(read_back.configurations[0].label, "first");
    }

    #[test]
    fn taking_one_out_leaves_the_others_where_they_were() {
        let written = text_without(A_FILE, 0);

        let read_back = read(Kind::Task, &written);
        assert_eq!(read_back.configurations.len(), 1);
        assert_eq!(read_back.configurations[0].label, "unit tests");
        assert_eq!(
            read_back.configurations[0].at, 0,
            "what is left moves up, and the places written back have to follow"
        );
        assert!(written.contains("// The project's tasks."));
    }

    /// A file changes while it is being looked at. An entry is found by what it
    /// says, so an edit follows it when it moves and is refused when it is gone --
    /// writing by the old place would go over somebody else's configuration.
    #[test]
    fn an_entry_is_found_by_what_it_says_rather_than_where_it_was() {
        let read_back = read(Kind::Task, A_FILE);
        let tests = read_back.configurations[1].as_written.clone();

        assert_eq!(place_of(A_FILE, 1, &tests), Some(1));

        let moved = text_without(A_FILE, 0);
        assert_eq!(
            place_of(&moved, 1, &tests),
            Some(0),
            "the entry moved up, and that is where the edit belongs now"
        );

        let gone = text_without(&moved, 0);
        assert_eq!(
            place_of(&gone, 1, &tests),
            None,
            "it is not in the file at all, so nothing may be written by its old place"
        );
    }

    #[test]
    fn a_debug_configuration_keeps_the_keys_its_adapter_needs() {
        let read = read(
            Kind::Debug,
            r#"[{
                "label": "attach to api",
                "adapter": "Delve",
                "request": "attach",
                "processId": 1234,
                "somethingTheAdapterWants": true
            }]"#,
        );

        let scenario = read.configurations[0]
            .scenario
            .as_ref()
            .expect("the editor understands it");
        let written = scenario_as_written(scenario).expect("it can be written");

        for key in ["request", "processId", "somethingTheAdapterWants"] {
            assert!(
                written.get(key).is_some(),
                "the adapter's own key {key} has to survive being written back, or \
                 saving one field would take the rest of the configuration with it: \
                 {written}"
            );
        }
    }
}
