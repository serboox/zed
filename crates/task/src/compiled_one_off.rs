use std::path::{Path, PathBuf};

use collections::HashMap;
use sha2::{Digest, Sha256};
use util::paths::home_dir;

use crate::VariableName;

/// The name the shell reads the source file's path from.
///
/// Not one of the editor's own names. The editor fills its variables in as plain
/// text, so a path with a quote, a `$` or a backtick in it would end the command
/// and start one nobody asked for; a name the editor does not know is left alone,
/// reaches the shell as a parameter, and a parameter's value is never read as
/// part of the command.
pub const SOURCE: &str = "RUN_CONFIGURATION_SOURCE";

/// The name the shell reads the resolved cache root from.
///
/// Worked out here, in Rust, rather than by the shell: which folder that is
/// depends on the OS, and telling two configurations apart depends on the
/// compiler this one was called with, neither of which a shell one-liner can
/// do the way it reads `SOURCE`'s value. Passed through the environment for
/// the same reason `SOURCE` is: a home directory is not guaranteed free of
/// characters a shell would read as more command.
const CACHE_ROOT: &str = "RUN_CONFIGURATION_CACHE_ROOT";

/// Zed's own cache directory, one location per OS.
///
/// `task` does not depend on the `paths` crate, so this mirrors
/// `paths::temp_dir()`'s per-platform rules locally rather than calling it.
fn cache_dir() -> PathBuf {
    let xdg_cache_home = std::env::var_os("XDG_CACHE_HOME").map(PathBuf::from);
    let local_app_data = std::env::var_os("LOCALAPPDATA").map(PathBuf::from);
    cache_dir_for(
        std::env::consts::OS,
        home_dir(),
        xdg_cache_home.as_deref(),
        local_app_data.as_deref(),
    )
}

/// The logic behind [`cache_dir`], with everything that depends on the
/// environment taken as a parameter, so it can be tested without touching the
/// real one.
fn cache_dir_for(
    target_os: &str,
    home: &Path,
    xdg_cache_home: Option<&Path>,
    local_app_data: Option<&Path>,
) -> PathBuf {
    match target_os {
        "macos" => home.join("Library").join("Caches").join("Zed"),
        "windows" => match local_app_data {
            Some(dir) => dir.join("Zed"),
            None => home.join("AppData").join("Local").join("Zed"),
        },
        _ => match xdg_cache_home {
            Some(dir) => dir.join("zed"),
            None => home.join(".cache").join("zed"),
        },
    }
}

/// What tells two configurations that build the same source file apart.
///
/// A hash of the compiler command rather than the command itself: a command
/// can hold a character a path segment cannot, and the compiler is the only
/// part of a configuration's identity this module is ever given.
fn compiler_key(compiler: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(compiler.as_bytes());
    hex::encode(hasher.finalize())
}

/// How a language that has to be compiled first is built and run in one go:
/// the command, its arguments, and the two variables the shell needs.
///
/// The program is built into the editor's own cache, under a folder for the
/// compiler this configuration builds with and then the path of the file it
/// was built from. Not inside the project: a build directory there shows up in
/// the file tree, in search and in go-to-file, for a file nobody asked to see.
/// The cache is outside every worktree, so none of that happens and no
/// `.gitignore` is needed either. The compiler's own folder keeps two
/// configurations that build the same source file from building over one
/// another.
///
/// The cache root is resolved once, here, since it does not depend on which
/// file is being built. The rest of the path does, and the file is only known
/// once the run starts, so the shell still works that part out itself.
pub fn build_and_run(compiler: &str) -> (String, Vec<String>, HashMap<String, String>) {
    let cache_root = cache_dir().join("run").join(compiler_key(compiler));

    // Read by the shell, name by name, and never as part of the command:
    //   src   the file, made absolute, since a name with no folder in it would
    //         otherwise be read as the folder to build in
    //   dir   the file's own folder under the cache root, the file's whole
    //         name included, so `foo.c` and `foo.cpp` beside each other do not
    //         build over one another
    //   stem  what to call the program: the file's name without its extension,
    //         and the whole name when there is nothing left of it
    let line = format!(
        r#"src="${SOURCE}"; case "$src" in /*) ;; *) src="$PWD/$src" ;; esac; name="${{src##*/}}"; dir="${CACHE_ROOT}${{src%/*}}/$name"; stem="${{name%.*}}"; [ -n "$stem" ] || stem="$name"; mkdir -p "$dir" && {compiler} "$src" -o "$dir/$stem" && "$dir/$stem""#
    );
    let mut env = HashMap::default();
    env.insert(SOURCE.to_string(), VariableName::File.template_value());
    env.insert(
        CACHE_ROOT.to_string(),
        cache_root.to_string_lossy().into_owned(),
    );
    ("sh".to_string(), vec!["-c".to_string(), line], env)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{TaskContext, TaskTemplate, TaskVariables};
    use std::os::unix::fs::PermissionsExt as _;

    fn a_template(compiler: &str) -> TaskTemplate {
        let (command, args, env) = build_and_run(compiler);
        TaskTemplate {
            label: "run it".to_string(),
            command,
            args,
            env,
            ..TaskTemplate::default()
        }
    }

    /// The editor fills in the file, and nothing it fills in may end up inside the
    /// command: that is what keeps a name from becoming a command of its own.
    #[gpui::test]
    async fn the_file_reaches_the_shell_as_a_parameter() {
        let hostile = r#"/projects/thing/my "program" $HOME `id` ; x.c"#;
        let mut variables = TaskVariables::default();
        variables.insert(VariableName::File, hostile.to_string());
        let context = TaskContext {
            cwd: None,
            task_variables: variables,
            project_env: HashMap::default(),
        };

        let resolved = a_template("cc")
            .resolve_task("test", &context)
            .expect("the editor can make a run out of it");
        let line = resolved.resolved.args.join(" ");

        assert!(
            !line.contains("my \"program\""),
            "nothing of the file's name is in the command: {line}"
        );
        assert_eq!(
            resolved.resolved.env.get(SOURCE).map(String::as_str),
            Some(hostile),
            "it is in the environment instead, whole"
        );
        assert!(
            line.contains(CACHE_ROOT),
            "and the cache root is read by name too, never spliced into the command: {line}"
        );
    }

    /// The bug this guards against: a cache root hand-rolled for one OS's
    /// convention, used unconditionally on every OS.
    #[test]
    fn the_cache_root_follows_the_os_it_is_asked_for() {
        let home = Path::new("/home/whoever");

        assert_eq!(
            cache_dir_for("macos", home, None, None),
            home.join("Library/Caches/Zed"),
            "macOS keeps caches under Library/Caches, never under ~/.cache"
        );
        assert_eq!(
            cache_dir_for(
                "windows",
                home,
                None,
                Some(Path::new(r"C:\Users\whoever\AppData\Local"))
            ),
            Path::new(r"C:\Users\whoever\AppData\Local").join("Zed"),
            "Windows keeps caches under %LOCALAPPDATA%, never under ~/.cache"
        );
        assert_eq!(
            cache_dir_for("windows", home, None, None),
            home.join("AppData/Local/Zed"),
            "and falls back to the usual place for it when the variable is unset"
        );
        assert_eq!(
            cache_dir_for("linux", home, Some(Path::new("/custom/cache")), None),
            Path::new("/custom/cache").join("zed"),
            "Linux and the rest honor XDG_CACHE_HOME when it is set"
        );
        assert_eq!(
            cache_dir_for("linux", home, None, None),
            home.join(".cache/zed"),
            "and fall back to ~/.cache otherwise"
        );
    }

    /// Two configurations that build the same source file must not share a
    /// folder, or whichever builds second overwrites the first one's binary.
    #[test]
    fn two_compilers_key_the_cache_apart() {
        assert_ne!(
            compiler_key("cc"),
            compiler_key("g++"),
            "two different compilers get two different folders"
        );
        assert_eq!(
            compiler_key("cc"),
            compiler_key("cc"),
            "the same compiler always gets the same folder"
        );
    }

    /// The same guarantee, checked against what `build_and_run` actually hands
    /// back, not just against `compiler_key` in isolation.
    #[test]
    fn build_and_run_keys_its_cache_root_by_compiler() {
        let (_, _, cc_env) = build_and_run("cc");
        let (_, _, cpp_env) = build_and_run("g++");
        let (_, _, cc_env_again) = build_and_run("cc");

        let cc_root = cc_env.get(CACHE_ROOT).expect("a cache root for cc");
        let cpp_root = cpp_env.get(CACHE_ROOT).expect("a cache root for g++");
        let cc_root_again = cc_env_again
            .get(CACHE_ROOT)
            .expect("a cache root for cc, again");

        assert_ne!(
            cc_root, cpp_root,
            "two configurations differing only in compiler get different roots"
        );
        assert_eq!(
            cc_root, cc_root_again,
            "the same configuration, asked for twice, gets the same root"
        );
    }

    /// The cache root never depends on where a project happens to live, so it
    /// can never end up nested inside one -- unlike a folder derived from the
    /// source file's own path would be.
    #[test]
    fn the_cache_root_is_independent_of_any_project_directory() {
        let (_, _, env) = build_and_run("cc");
        let cache_root = PathBuf::from(env.get(CACHE_ROOT).expect("a cache root"));

        for project in [
            Path::new("/home/whoever/code/widgets"),
            Path::new("/Users/whoever/code/widgets"),
            Path::new(r"C:\Users\whoever\code\widgets"),
        ] {
            assert!(
                !cache_root.starts_with(project),
                "the cache root {cache_root:?} does not depend on a project \
                 directory, so it cannot land inside {project:?}"
            );
        }
    }

    /// The whole line, run for real, with a compiler of our own so nothing has to
    /// be installed: the program has to land in the cache, under the path it was
    /// built from, run from there, and leave the project as it was -- for a file
    /// named everything a shell reads.
    #[gpui::test]
    async fn the_line_builds_into_the_cache_and_runs_from_there() {
        let cache = tempfile::tempdir().expect("a directory to keep the cache in");
        let project = tempfile::tempdir().expect("a directory to be the project");
        let compilers = tempfile::tempdir().expect("a directory for the compiler");
        a_compiler_that_says_it_ran(compilers.path());

        let named = r#"my "program" $HOME `id` ; x.c"#;
        let source = project.path().join(named);
        std::fs::write(&source, "int main(void){return 0;}\n").expect("a source file");

        let (command, args, env) = build_and_run("cc");
        let ran = smol::block_on(
            smol::process::Command::new(&command)
                .args(&args)
                .current_dir(project.path())
                .env(CACHE_ROOT, cache.path())
                .env(SOURCE, &source)
                .env("PATH", with_the_compiler_first(compilers.path()))
                .output(),
        )
        .expect("a machine that runs this editor has a shell");
        assert_eq!(
            env.get(SOURCE).map(String::as_str),
            Some("$ZED_FILE"),
            "the editor is asked for the file, and that is all"
        );

        assert!(
            ran.status.success(),
            "the line did not run: {}",
            String::from_utf8_lossy(&ran.stderr)
        );
        assert_eq!(
            String::from_utf8_lossy(&ran.stdout),
            "ran",
            "the program that was built is the one that ran, and nothing else"
        );

        let left_in_the_project: Vec<String> = std::fs::read_dir(project.path())
            .expect("the project can be read")
            .flatten()
            .map(|entry| entry.file_name().to_string_lossy().to_string())
            .collect();
        assert_eq!(
            left_in_the_project,
            vec![named.to_string()],
            "the project holds the source and nothing else"
        );

        let built = every_file_under(cache.path());
        let expected = cache
            .path()
            .join(
                project
                    .path()
                    .strip_prefix("/")
                    .expect("a directory of its own"),
            )
            .join(named)
            .join(r#"my "program" $HOME `id` ; x"#);
        assert_eq!(
            built,
            vec![expected],
            "the program sits in the cache, under the path it was built from"
        );
    }

    /// A name with no folder in it, and a name that is nothing but an extension:
    /// the first must not be read as the folder to build in, and the second must
    /// leave something to call the program.
    #[gpui::test]
    async fn a_file_named_oddly_still_lands_somewhere_sensible() {
        let cache = tempfile::tempdir().expect("a directory to keep the cache in");
        let project = tempfile::tempdir().expect("a directory to be the project");
        let compilers = tempfile::tempdir().expect("a directory for the compiler");
        let compiler = a_compiler_that_says_it_ran(compilers.path());

        let (command, args, _) = build_and_run("cc");
        for (named, run_from, expected) in [
            // Given no folder, the file is taken to be in the folder the run
            // started from.
            ("plain.c", Some(project.path()), "plain"),
            (".c", None, ".c"),
        ] {
            std::fs::write(project.path().join(named), "int main(void){return 0;}\n")
                .expect("a source file");
            let source = match run_from {
                Some(_) => named.to_string(),
                None => project.path().join(named).to_string_lossy().to_string(),
            };
            let ran = smol::block_on(
                smol::process::Command::new(&command)
                    .args(&args)
                    .current_dir(project.path())
                    .env(CACHE_ROOT, cache.path())
                    .env(SOURCE, &source)
                    .env("PATH", with_the_compiler_first(compilers.path()))
                    .output(),
            )
            .expect("a machine that runs this editor has a shell");
            assert!(
                ran.status.success(),
                "{named} did not run: {}",
                String::from_utf8_lossy(&ran.stderr)
            );
            assert_eq!(String::from_utf8_lossy(&ran.stdout), "ran");

            let built = every_file_under(cache.path());
            let of_this_one: Vec<&PathBuf> = built
                .iter()
                .filter(|path| path.file_name().is_some_and(|name| name == expected))
                .collect();
            assert_eq!(
                of_this_one.len(),
                1,
                "one program called {expected}, in the cache: {built:?}"
            );
            let folder = of_this_one[0].parent().expect("the program is in a folder");
            let expected = cache
                .path()
                .join(
                    project
                        .path()
                        .strip_prefix("/")
                        .expect("a directory of its own"),
                )
                .join(named);
            assert_eq!(
                folder, expected,
                "the folder mirrors where the file is, whatever the file was called"
            );
        }
        assert!(compiler.exists());
    }

    /// Two files of one name are two configurations, and one must not build over
    /// the other.
    #[gpui::test]
    async fn two_files_of_one_name_are_built_apart() {
        let cache = tempfile::tempdir().expect("a directory to keep the cache in");
        let project = tempfile::tempdir().expect("a directory to be the project");
        let compilers = tempfile::tempdir().expect("a directory for the compiler");
        a_compiler_that_says_it_ran(compilers.path());

        let (command, args, _) = build_and_run("cc");
        // Two folders holding a `foo.c`, and beside one of them a `foo.cpp`: three
        // sources, one name between them.
        for at in ["src/foo.c", "tests/foo.c", "src/foo.cpp"] {
            let source = project.path().join(at);
            std::fs::create_dir_all(source.parent().expect("a folder in the project"))
                .expect("the folder is made");
            std::fs::write(&source, "int main(void){return 0;}\n").expect("a source file");
            let ran = smol::block_on(
                smol::process::Command::new(&command)
                    .args(&args)
                    .env(CACHE_ROOT, cache.path())
                    .env(SOURCE, &source)
                    .env("PATH", with_the_compiler_first(compilers.path()))
                    .output(),
            )
            .expect("a machine that runs this editor has a shell");
            assert!(
                ran.status.success(),
                "{at} did not run: {}",
                String::from_utf8_lossy(&ran.stderr)
            );
        }

        let built = every_file_under(cache.path());
        assert_eq!(
            built.len(),
            3,
            "each source file has a program of its own: {built:?}"
        );
    }

    /// A `cc` of our own, so nothing has to be installed: it writes a program
    /// that says it ran, wherever it is told to.
    fn a_compiler_that_says_it_ran(into: &Path) -> PathBuf {
        let compiler = into.join("cc");
        std::fs::write(
            &compiler,
            "#!/bin/sh\nprintf '#!/bin/sh\\nprintf ran\\n' > \"$3\"\nchmod +x \"$3\"\n",
        )
        .expect("the compiler is written");
        std::fs::set_permissions(&compiler, std::fs::Permissions::from_mode(0o755))
            .expect("and can be run");
        compiler
    }

    fn with_the_compiler_first(compilers: &Path) -> String {
        format!(
            "{}:{}",
            compilers.display(),
            std::env::var("PATH").unwrap_or_default()
        )
    }

    /// Every file under `at`, however deep.
    fn every_file_under(at: &Path) -> Vec<PathBuf> {
        let mut found = Vec::new();
        let Ok(entries) = std::fs::read_dir(at) else {
            return found;
        };
        for entry in entries.flatten() {
            match entry.path().is_dir() {
                true => found.extend(every_file_under(&entry.path())),
                false => found.push(entry.path()),
            }
        }
        found
    }
}
