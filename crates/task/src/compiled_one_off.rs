use collections::HashMap;

use crate::VariableName;

/// The name the shell reads the source file's path from.
///
/// Not one of the editor's own names. The editor fills its variables in as plain
/// text, so a path with a quote, a `$` or a backtick in it would end the command
/// and start one nobody asked for; a name the editor does not know is left alone,
/// reaches the shell as a parameter, and a parameter's value is never read as
/// part of the command.
pub const SOURCE: &str = "RUN_CONFIGURATION_SOURCE";

/// How a language that has to be compiled first is built and run in one go:
/// the command, its arguments, and the one variable the shell needs.
///
/// The program is built into the editor's own cache, under the path of the file
/// it was built from. Not inside the project: a build directory there shows up in
/// the file tree, in search and in go-to-file, for a file nobody asked to see.
/// The cache is outside every worktree, so none of that happens and no
/// `.gitignore` is needed either.
///
/// The shell works the whole path out when the run starts, so what is written
/// into a project's file is the same on every machine.
pub fn build_and_run(compiler: &str) -> (String, Vec<String>, HashMap<String, String>) {
    // Read by the shell, name by name, and never as part of the command:
    //   src   the file, made absolute, since a name with no folder in it would
    //         otherwise be read as the folder to build in
    //   dir   the file's own folder under the cache, the file's whole name
    //         included, so `foo.c` and `foo.cpp` beside each other do not build
    //         over one another
    //   stem  what to call the program: the file's name without its extension,
    //         and the whole name when there is nothing left of it
    let line = format!(
        r#"src="${SOURCE}"; case "$src" in /*) ;; *) src="$PWD/$src" ;; esac; name="${{src##*/}}"; dir="${{XDG_CACHE_HOME-$HOME/.cache}}/zed/run${{src%/*}}/$name"; stem="${{name%.*}}"; [ -n "$stem" ] || stem="$name"; mkdir -p "$dir" && {compiler} "$src" -o "$dir/$stem" && "$dir/$stem""#
    );
    let mut env = HashMap::default();
    env.insert(SOURCE.to_string(), VariableName::File.template_value());
    ("sh".to_string(), vec!["-c".to_string(), line], env)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{TaskContext, TaskTemplate, TaskVariables};
    use std::os::unix::fs::PermissionsExt as _;
    use std::path::{Path, PathBuf};

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
            line.contains("${XDG_CACHE_HOME-$HOME/.cache}"),
            "and the shell's own work is left to the shell: {line}"
        );
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
                .env("XDG_CACHE_HOME", cache.path())
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
            .join("zed/run")
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
                    .env("XDG_CACHE_HOME", cache.path())
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
                .join("zed/run")
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
                    .env("XDG_CACHE_HOME", cache.path())
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
