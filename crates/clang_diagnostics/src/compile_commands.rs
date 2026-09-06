use std::path::{Component, Path, PathBuf};

use serde::Deserialize;

/// One entry of a compilation database: a file, the directory the compiler was
/// run in, and the arguments it was run with.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Entry {
    pub directory: PathBuf,
    /// Absolute, resolved against `directory` if the database wrote it relative.
    pub file: PathBuf,
    pub arguments: Vec<String>,
}

/// The names a compilation database is found under, in the order they are
/// tried. The first is where a project that checks the database in puts it; the
/// rest are where the common generators leave it, and the reader is not asked
/// to symlink it into place before this works.
const WHERE_GENERATORS_LEAVE_IT: &[&str] = &[
    "compile_commands.json",
    "build/compile_commands.json",
    "out/compile_commands.json",
    "cmake-build-debug/compile_commands.json",
];

/// The compilation database covering a file, or nothing at all.
///
/// Searched from the file's own directory upwards to the worktree root, so a
/// repository holding several projects finds the nearest one rather than the
/// topmost. Nothing found is the ordinary case for most C there is, and the
/// caller's job is to be quiet about it: without a database a translation
/// unit's include paths and defines are unknown, and a parse without them
/// reports a screenful of missing headers that say nothing about the code.
pub fn where_the_database_is(
    file: &Path,
    root: &Path,
    exists: impl Fn(&Path) -> bool,
) -> Option<PathBuf> {
    let mut directory = file.parent()?;
    loop {
        for name in WHERE_GENERATORS_LEAVE_IT {
            let candidate = directory.join(name);
            if exists(&candidate) {
                return Some(candidate);
            }
        }
        if directory == root {
            return None;
        }
        directory = directory.parent()?;
    }
}

#[derive(Deserialize)]
struct Raw {
    directory: String,
    file: String,
    /// The two shapes the format allows. `arguments` is already split and is
    /// preferred; `command` is one line that has to be split the way a shell
    /// would, which is what most generators still write.
    #[serde(default)]
    arguments: Option<Vec<String>>,
    #[serde(default)]
    command: Option<String>,
}

/// The entries a compilation database holds.
///
/// A database that will not parse yields nothing rather than panicking, and one
/// unreadable entry does not cost the rest of them: a generator that adds a
/// field should lose that file, not the project.
pub fn entries_in(text: &str) -> Vec<Entry> {
    let Ok(raws) = serde_json::from_str::<Vec<serde_json::Value>>(text) else {
        return Vec::new();
    };
    raws.into_iter()
        .filter_map(|raw| {
            let raw = serde_json::from_value::<Raw>(raw).ok()?;
            let directory = PathBuf::from(&raw.directory);
            let arguments = match (raw.arguments, raw.command) {
                (Some(arguments), _) => arguments,
                (None, Some(command)) => split_as_a_shell_would(&command),
                (None, None) => return None,
            };
            Some(Entry {
                file: resolved(&directory, &raw.file),
                directory,
                arguments,
            })
        })
        .collect()
}

/// Splits a command line the way a shell would: quotes group, and a backslash
/// escapes the character after it outside single quotes.
///
/// Written out rather than split on whitespace because a path with a space in
/// it -- which is every Windows build and plenty of others -- would otherwise
/// become two arguments and take the include path with it.
fn split_as_a_shell_would(command: &str) -> Vec<String> {
    let mut arguments = Vec::new();
    let mut current = String::new();
    let mut started = false;
    let mut quote: Option<char> = None;
    let mut characters = command.chars();
    while let Some(character) = characters.next() {
        match (quote, character) {
            (Some('\''), '\'') | (Some('"'), '"') => quote = None,
            (None, '\'' | '"') => {
                quote = Some(character);
                started = true;
            }
            (None | Some('"'), '\\') => {
                if let Some(escaped) = characters.next() {
                    current.push(escaped);
                    started = true;
                }
            }
            (None, character) if character.is_whitespace() => {
                if started {
                    arguments.push(std::mem::take(&mut current));
                    started = false;
                }
            }
            (_, character) => {
                current.push(character);
                started = true;
            }
        }
    }
    if started {
        arguments.push(current);
    }
    arguments
}

/// The arguments to parse a file with, taken from the database, or nothing at
/// all where the database covers nothing near it.
///
/// A file the database names gets its own arguments. A file it does not -- and
/// a header is never in a compilation database, because a header is never
/// compiled on its own -- borrows them from the nearest file that is, which is
/// what a header included by that file was compiled with anyway.
pub fn arguments_for(entries: &[Entry], file: &Path) -> Option<Vec<String>> {
    let entry = entries
        .iter()
        .find(|entry| entry.file == file)
        .or_else(|| nearest_to(entries, file))?;
    Some(as_libclang_wants(entry, file))
}

/// The entry whose file sits nearest the given one.
///
/// Nearest is leading path components shared: a header in `src/net/` takes its
/// flags from a source file in `src/net/` before one in `src/`, and from one in
/// `src/` before one in a directory it shares nothing with. Where two sit
/// equally close, the one sharing the header's name wins -- `net.h` belongs to
/// `net.cpp`, and a directory holding a dozen sources otherwise hands out
/// whichever entry the generator happened to write last.
fn nearest_to<'e>(entries: &'e [Entry], file: &Path) -> Option<&'e Entry> {
    entries.iter().max_by_key(|entry| {
        (
            shared_components(&entry.file, file),
            entry.file.file_stem() == file.file_stem(),
        )
    })
}

fn shared_components(one: &Path, other: &Path) -> usize {
    one.components()
        .zip(other.components())
        .take_while(|(one, other)| one == other)
        .count()
}

/// The compiler's own arguments, made into ones `libclang` can be handed.
///
/// Three things change. The compiler's own name goes, because the argument list
/// starts at the first flag. Every path is made absolute, because the parse
/// runs in whatever directory the editor was started in and a relative `-I`
/// would resolve against that. And the file being compiled goes, because the
/// file to parse is passed separately and naming it twice makes it two inputs.
fn as_libclang_wants(entry: &Entry, parsing: &Path) -> Vec<String> {
    let mut wanted = Vec::new();
    let mut arguments = entry.arguments.iter().skip(1).peekable();
    while let Some(argument) = arguments.next() {
        if resolved(&entry.directory, argument) == entry.file {
            continue;
        }
        if let Some(flag) = TAKES_A_PATH_NEXT.iter().find(|flag| *flag == argument) {
            if let Some(path) = arguments.next() {
                wanted.push((*flag).to_string());
                wanted.push(absolute_form(&entry.directory, path));
                continue;
            }
        }
        if let Some(flag) = TAKES_A_PATH_JOINED
            .iter()
            .find(|flag| argument.len() > flag.len() && argument.starts_with(*flag))
        {
            let path = &argument[flag.len()..];
            wanted.push(format!("{flag}{}", absolute_form(&entry.directory, path)));
            continue;
        }
        wanted.push(argument.clone());
    }
    if let Some(language) = language_to_read_a_header_as(entry, parsing, &wanted) {
        wanted.push("-x".to_string());
        wanted.push(language.to_string());
    }
    wanted
}

/// Flags whose path is the argument after them.
const TAKES_A_PATH_NEXT: &[&str] = &[
    "-I",
    "-isystem",
    "-iquote",
    "-idirafter",
    "-iframework",
    "-include",
    "-imacros",
    "-isysroot",
    "--sysroot",
    "-B",
];

/// Flags whose path is written onto the flag itself.
const TAKES_A_PATH_JOINED: &[&str] = &[
    "-I",
    "-isystem",
    "-iquote",
    "-idirafter",
    "-iframework",
    "-F",
    "-B",
    "--sysroot=",
];

/// Suffixes that name a header rather than a source file.
const HEADER_SUFFIXES: &[&str] = &[
    "h", "hh", "hpp", "hxx", "h++", "H", "inl", "ipp", "cuh", "tcc",
];

/// The language to read a header as, where one has to be said.
///
/// A header is never in a compilation database and so always borrows another
/// file's arguments, and the front end decides a `.h` is C from its name alone.
/// A `.h` in a C++ project read as C fails on the first `class`, which is a
/// screenful of errors about a file that is perfectly good. What the header is
/// cannot be known from its own name, so it is taken from the file that lent it
/// its flags -- which is the file that includes it.
///
/// Left alone where the arguments already say, because a project that passed
/// `-x` meant it.
fn language_to_read_a_header_as(
    entry: &Entry,
    parsing: &Path,
    arguments: &[String],
) -> Option<&'static str> {
    if entry.file == parsing || arguments.iter().any(|argument| argument == "-x") {
        return None;
    }
    let suffix = parsing.extension()?.to_str()?;
    if !HEADER_SUFFIXES.contains(&suffix) {
        return None;
    }
    match entry.file.extension()?.to_str()? {
        "c" => Some("c-header"),
        _ => Some("c++-header"),
    }
}

/// A path as written, resolved against the directory the compiler ran in.
fn resolved(directory: &Path, path: &str) -> PathBuf {
    let path = Path::new(path);
    if path.is_absolute() {
        without_dot_segments(path)
    } else {
        without_dot_segments(&directory.join(path))
    }
}

fn absolute_form(directory: &Path, path: &str) -> String {
    resolved(directory, path).to_string_lossy().into_owned()
}

/// A path with `.` and `..` folded away, so that two spellings of the same
/// place compare equal.
///
/// Done by hand rather than with `canonicalize`, which touches the disk: a
/// database entry may name a generated file that has not been built yet, and a
/// path that fails to resolve must still compare against the one the editor
/// holds.
fn without_dot_segments(path: &Path) -> PathBuf {
    let mut folded = PathBuf::new();
    for component in path.components() {
        match component {
            Component::CurDir => {}
            Component::ParentDir => {
                if !folded.pop() {
                    folded.push(component);
                }
            }
            component => folded.push(component),
        }
    }
    folded
}

#[cfg(test)]
mod tests {
    use super::*;
    use pretty_assertions::assert_eq;

    /// A database as CMake writes one: `command` rather than `arguments`, paths
    /// relative to `directory`, and the file being compiled named inside the
    /// command as well as in its own field.
    const AS_CMAKE_WRITES_IT: &str = r#"[
      {
        "directory": "/project/build",
        "command": "/usr/bin/c++ -DFOO=1 -I../include -isystem /opt/qt/include -std=c++20 -o CMakeFiles/app.dir/src/net.cpp.o -c ../src/net.cpp",
        "file": "../src/net.cpp"
      },
      {
        "directory": "/project/build",
        "arguments": ["/usr/bin/cc", "-I../include", "-c", "/project/src/log.c", "-o", "log.o"],
        "file": "/project/src/log.c"
      }
    ]"#;

    fn database() -> Vec<Entry> {
        entries_in(AS_CMAKE_WRITES_IT)
    }

    #[test]
    fn a_file_the_database_names_gets_its_own_arguments() {
        let arguments =
            arguments_for(&database(), Path::new("/project/src/net.cpp")).expect("the entry");
        assert_eq!(
            arguments,
            vec![
                "-DFOO=1",
                "-I/project/include",
                "-isystem",
                "/opt/qt/include",
                "-std=c++20",
                "-o",
                "CMakeFiles/app.dir/src/net.cpp.o",
                "-c",
            ],
            "the compiler's own name and the file being compiled are both gone"
        );
    }

    /// Relative include paths resolve against the directory the compiler ran
    /// in, not against wherever the editor was started. Getting this wrong is
    /// silent: every project header goes missing and the file fills with errors
    /// about code that compiles perfectly.
    #[test]
    fn a_relative_include_path_is_made_absolute_against_the_compilers_directory() {
        let arguments =
            arguments_for(&database(), Path::new("/project/src/net.cpp")).expect("the entry");
        assert!(
            arguments.contains(&"-I/project/include".to_string()),
            "{arguments:?}"
        );
        assert!(
            !arguments.iter().any(|argument| argument.contains("..")),
            "{arguments:?}"
        );
    }

    /// A path already absolute is left as it is, and an already-split
    /// `arguments` list is read as readily as a `command` line.
    #[test]
    fn an_already_split_entry_reads_the_same_as_a_command_line() {
        let arguments =
            arguments_for(&database(), Path::new("/project/src/log.c")).expect("the entry");
        assert_eq!(arguments, vec!["-I/project/include", "-c", "-o", "log.o"]);
    }

    /// A header is never in a compilation database, because a header is never
    /// compiled on its own. It borrows the flags of the nearest file that is --
    /// which is what the file including it was compiled with.
    #[test]
    fn a_header_borrows_the_arguments_of_the_nearest_file_that_has_them() {
        let arguments =
            arguments_for(&database(), Path::new("/project/src/net.h")).expect("a neighbour");
        assert!(
            arguments.contains(&"-std=c++20".to_string()),
            "the C++ neighbour's flags, not the C one's: {arguments:?}"
        );
        assert_eq!(
            arguments.last().map(String::as_str),
            Some("c++-header"),
            "and it is read as C++, which its own name does not say"
        );
    }

    /// The neighbour decides the language. The same `.h` in a C project must
    /// not be read as C++, or every `class`-free file still fails on something
    /// else -- and a project that said `-x` itself is left alone.
    #[test]
    fn the_neighbour_decides_what_language_a_header_is_read_as() {
        let only_c = entries_in(
            r#"[{"directory": "/c", "arguments": ["cc", "-c", "/c/log.c"], "file": "/c/log.c"}]"#,
        );
        let arguments = arguments_for(&only_c, Path::new("/c/log.h")).expect("the neighbour");
        assert_eq!(arguments.last().map(String::as_str), Some("c-header"));

        let said_so = entries_in(
            r#"[{"directory": "/c", "arguments": ["cc", "-x", "objective-c", "/c/log.c"], "file": "/c/log.c"}]"#,
        );
        let arguments = arguments_for(&said_so, Path::new("/c/log.h")).expect("the neighbour");
        assert_eq!(
            arguments,
            vec!["-x", "objective-c"],
            "a project that said what the file is meant it"
        );
    }

    /// Nearest is measured in shared path components, so a header takes the
    /// flags of a file in its own directory over one further up.
    #[test]
    fn the_nearest_neighbour_wins_over_a_further_one() {
        let entries = entries_in(
            r#"[
              {"directory": "/p", "arguments": ["cc", "-DFAR", "/p/main.c"], "file": "/p/main.c"},
              {"directory": "/p", "arguments": ["cc", "-DNEAR", "/p/net/tcp.c"], "file": "/p/net/tcp.c"}
            ]"#,
        );
        let arguments = arguments_for(&entries, Path::new("/p/net/tcp.h")).expect("a neighbour");
        assert!(arguments.contains(&"-DNEAR".to_string()), "{arguments:?}");
    }

    /// A path with a space in it is one argument. Splitting on whitespace would
    /// make it two and take the include path with it.
    #[test]
    fn a_quoted_path_stays_one_argument() {
        let entries = entries_in(
            r#"[{"directory": "/p", "command": "cc \"-I/opt/My SDK/include\" -DA=\"b c\" /p/a.c", "file": "/p/a.c"}]"#,
        );
        let arguments = arguments_for(&entries, Path::new("/p/a.c")).expect("the entry");
        assert_eq!(arguments, vec!["-I/opt/My SDK/include", "-DA=b c"]);
    }

    /// A database that will not parse yields nothing rather than panicking. A
    /// half-written `compile_commands.json` is what a build in progress leaves
    /// behind, and it must not take the editor down.
    #[test]
    fn a_database_that_will_not_parse_yields_nothing() {
        for text in ["", "{}", "null", "[1, 2, 3]", "[{\"file\": \"a.c\"}]"] {
            assert!(entries_in(text).is_empty(), "{text:?}");
        }
        assert_eq!(arguments_for(&[], Path::new("/p/a.c")), None);
    }

    /// One entry a newer generator writes differently costs that file, not the
    /// project.
    #[test]
    fn one_unreadable_entry_does_not_cost_the_rest() {
        let entries = entries_in(
            r#"[
              {"directory": "/p", "arguments": ["cc", "/p/a.c"], "file": "/p/a.c"},
              {"file": "/p/b.c"},
              {"directory": "/p", "arguments": ["cc", "/p/c.c"], "file": "/p/c.c"}
            ]"#,
        );
        assert_eq!(entries.len(), 2);
    }

    /// The database is searched from the file upwards, so a repository holding
    /// several projects finds the nearest one rather than the topmost.
    #[test]
    fn the_nearest_database_is_found_and_the_search_stops_at_the_root() {
        let here = PathBuf::from("/repo/service/build/compile_commands.json");
        let found = where_the_database_is(
            Path::new("/repo/service/src/net.cpp"),
            Path::new("/repo"),
            |path| path == here,
        );
        assert_eq!(found, Some(here));

        let above = PathBuf::from("/compile_commands.json");
        assert_eq!(
            where_the_database_is(Path::new("/repo/src/a.c"), Path::new("/repo"), |path| path
                == above),
            None,
            "the search stops at the worktree root rather than walking out of it"
        );
    }

    /// No database anywhere is the ordinary case for most C there is, and it
    /// has to be harmless: nothing found, so nothing parsed and nothing said.
    #[test]
    fn no_database_anywhere_is_nothing_found_rather_than_a_fault() {
        assert_eq!(
            where_the_database_is(Path::new("/repo/src/a.c"), Path::new("/repo"), |_| false),
            None
        );
    }
}
