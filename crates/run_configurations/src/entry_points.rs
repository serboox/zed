use std::path::{Path, PathBuf};

use collections::HashMap;
use futures::StreamExt as _;
use gpui::{App, AppContext as _, Entity, Task};
use project::Project;

use crate::templates::HowToRun;

/// One way a project can be started, as the reader picks it out of a list.
///
/// Found by reading the project rather than asked for: a field a reader has to
/// type a package path into is a form, and the point of the list is that there
/// is nothing to type.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct EntryPoint {
    /// What the reader reads -- `./cmd/api`, `server`, `npm run dev`.
    pub name: String,
    /// The language or tool it belongs to, so a list of them can be grouped.
    pub family: Family,
    /// What running it means.
    pub how: HowToRun,
    /// The debugger it can be debugged with, when one applies.
    pub debugger: Option<&'static str>,
}

/// One way of running, and the file that has to exist for it to be real.
///
/// The condition travels with the way rather than being checked where the way
/// is read, so reading a file stays a pure function of its text -- which is
/// what makes the readers testable without a filesystem. [`look_through`]
/// resolves it, beside the reads it is already doing.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Offered {
    pub point: EntryPoint,
    /// Relative to the project's root. `None` means the way is real on the
    /// strength of the file it was read from alone.
    pub only_if: Option<PathBuf>,
}

impl EntryPoint {
    /// This way of running, real on the strength of the file it was read from.
    fn on_its_own(self) -> Offered {
        Offered {
            point: self,
            only_if: None,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum Family {
    Go,
    Rust,
    Node,
    Python,
    Docker,
    Compose,
}

impl Family {
    pub fn shown(self) -> &'static str {
        match self {
            Family::Go => "Go",
            Family::Rust => "Rust",
            Family::Node => "Node.js",
            Family::Python => "Python",
            Family::Docker => "Docker",
            Family::Compose => "Docker Compose",
        }
    }
}

/// How many files the search will open. A project can hold tens of thousands of
/// them, and past this the list is long enough that the reader is filtering it
/// rather than reading it -- so the cost of reading more buys nothing.
const AT_MOST: usize = 400;

/// Everything the project says about how it is started: the ways of running it,
/// and the files of variables those runs can be given.
///
/// Read from the project rather than asked of the reader: a field to type a
/// package path into is a form, and what the editor can find it should find.
pub fn look_through(project: &Entity<Project>, cx: &App) -> Task<(Vec<EntryPoint>, Vec<PathBuf>)> {
    let project = project.read(cx);
    let fs = project.fs().clone();
    let mut roots = Vec::new();
    let mut to_read: Vec<(PathBuf, PathBuf)> = Vec::new();
    for worktree in project.visible_worktrees(cx) {
        let worktree = worktree.read(cx);
        let root = worktree.abs_path().to_path_buf();
        roots.push(root.clone());
        // Ignored files are left out on purpose, which also means a project kept
        // somewhere the reader's own global ignore rules cover has nothing here
        // to find -- as it has nothing for the file finder either.
        for entry in worktree.entries(false, 0) {
            if to_read.len() >= AT_MOST {
                break;
            }
            if !entry.is_file() {
                continue;
            }
            let relative = entry.path.as_std_path();
            if worth_reading(relative) {
                to_read.push((relative.to_path_buf(), root.clone()));
            }
        }
    }
    cx.background_spawn(async move {
        let mut found = Vec::new();
        for (relative, root) in to_read {
            let absolute = root.join(&relative);
            let contents = match fs.load(&absolute).await {
                Ok(contents) => contents,
                // A file that cannot be read is one way fewer to offer, not a
                // reason to offer none: it may have been deleted between the
                // worktree scan and this read.
                Err(error) => {
                    log::debug!("{}: {error}", absolute.display());
                    continue;
                }
            };
            for offered in ways_to_run(&relative, &contents) {
                let real = match &offered.only_if {
                    None => true,
                    Some(needed) => fs.is_file(&root.join(needed)).await,
                };
                if real {
                    found.push(offered.point);
                }
            }
        }
        found.sort_by(|one, other| {
            one.family
                .cmp(&other.family)
                .then_with(|| one.name.cmp(&other.name))
        });
        found.dedup();

        // The files of variables are listed off the disk rather than taken from
        // the worktree: every one of them begins with a dot, and a hidden entry
        // is not scanned into the tree until somebody expands the directory
        // holding it, so a project's `.env` is never there to be found.
        let mut env = Vec::new();
        for root in roots {
            let Ok(mut listed) = fs.read_dir(&root).await else {
                continue;
            };
            while let Some(Ok(path)) = listed.next().await {
                if is_env_file(&path)
                    && let Ok(relative) = path.strip_prefix(&root)
                {
                    env.push(relative.to_path_buf());
                }
            }
        }
        env.sort();
        env.dedup();
        log::debug!(
            "run configurations: {} ways to run, {} environment files",
            found.len(),
            env.len()
        );
        (found, env)
    })
}

/// Files the search opens. Everything else in a project is skipped on its name
/// alone, because the search runs over the whole worktree and reading it all
/// would cost more than the answer is worth.
pub fn worth_reading(path: &Path) -> bool {
    let Some(name) = path.file_name().and_then(|name| name.to_str()) else {
        return false;
    };
    if is_compose_file(name) || is_dockerfile(name) {
        return true;
    }
    match name {
        "Cargo.toml" | "package.json" | "__main__.py" => return true,
        _ => {}
    }
    matches!(
        path.extension().and_then(|extension| extension.to_str()),
        Some("go") | Some("py")
    )
}

/// Suffixes that make a `Dockerfile.`-prefixed name a file *about* a Dockerfile
/// rather than one. `Dockerfile.dockerignore` is a list of paths.
const NOT_A_DOCKERFILE: [&str; 3] = ["dockerignore", "md", "txt"];

fn is_dockerfile(name: &str) -> bool {
    if name == "Dockerfile" {
        return true;
    }
    let Some(rest) = name.strip_prefix("Dockerfile.") else {
        return false;
    };
    let last = rest.rsplit('.').next().unwrap_or(rest);
    !last.is_empty() && !NOT_A_DOCKERFILE.contains(&last)
}

fn is_compose_file(name: &str) -> bool {
    let Some(stem) = name
        .strip_suffix(".yaml")
        .or_else(|| name.strip_suffix(".yml"))
    else {
        return false;
    };
    stem == "compose" || stem == "docker-compose" || stem.starts_with("docker-compose.")
}

/// Whether a file holds the project's environment, so it can be offered for the
/// environment field. `.env.example` is deliberately included: it is often the
/// only one a fresh checkout has, and a reader who picks it finds out at once
/// that the values are placeholders.
pub fn is_env_file(path: &Path) -> bool {
    let Some(name) = path.file_name().and_then(|name| name.to_str()) else {
        return false;
    };
    name == ".env" || name.starts_with(".env.") || name.ends_with(".env")
}

/// The ways to run that this one file describes. `path` is relative to the
/// project's root, which is also what the commands are written against.
pub fn ways_to_run(path: &Path, contents: &str) -> Vec<Offered> {
    let Some(name) = path.file_name().and_then(|name| name.to_str()) else {
        return Vec::new();
    };
    if is_compose_file(name) {
        return compose_services(path, contents);
    }
    if is_dockerfile(name) {
        return docker_image(path, contents).into_iter().collect();
    }
    match name {
        "Cargo.toml" => return cargo_binaries(path, contents),
        "package.json" => return node_scripts(path, contents),
        _ => {}
    }
    match path.extension().and_then(|extension| extension.to_str()) {
        Some("go") => go_package(path, contents).into_iter().collect(),
        Some("py") => python_module(path, name, contents).into_iter().collect(),
        _ => Vec::new(),
    }
}

/// The directory a file sits in, written the way a command names a package:
/// `./cmd/api`, or `.` at the root.
fn package_of(path: &Path) -> String {
    match path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
    {
        Some(parent) => format!("./{}", parent.to_string_lossy().replace('\\', "/")),
        None => ".".to_string(),
    }
}

fn plain(command: &str, args: Vec<String>) -> HowToRun {
    HowToRun {
        command: command.to_string(),
        args,
        cwd: Some("$ZED_WORKTREE_ROOT".to_string()),
        env: HashMap::default(),
    }
}

fn go_package(path: &Path, contents: &str) -> Option<Offered> {
    let is_a_program = contents
        .lines()
        .any(|line| line.trim_start().starts_with("package main"))
        && contents.contains("func main(");
    if !is_a_program {
        return None;
    }
    let package = package_of(path);
    Some(
        EntryPoint {
            name: package.clone(),
            family: Family::Go,
            how: plain("go", vec!["run".into(), package]),
            debugger: Some("Delve"),
        }
        .on_its_own(),
    )
}

/// One binary a manifest declares, as its lines are read.
#[derive(Default)]
struct Binary {
    name: String,
    /// The features cargo will not build it without.
    features: Vec<String>,
}

/// The binaries a Cargo manifest names. Read line by line rather than parsed:
/// only three keys matter, the list is a suggestion the reader can edit, and a
/// manifest this cannot read leaves the list shorter rather than wrong.
///
/// A manifest with no `[[bin]]` builds one binary named after the package only
/// when it has a `src/main.rs`, so that way is offered on the condition that
/// the file is there. Without the condition every library in a workspace is
/// offered as something to run and `cargo run --bin <library>` fails outright:
/// on this one that was most of the list.
fn cargo_binaries(path: &Path, contents: &str) -> Vec<Offered> {
    let at = path.parent().map(Path::to_path_buf).unwrap_or_default();
    let mut declared: Vec<Binary> = Vec::new();
    let mut section = String::new();
    let mut package_name = None;
    for line in contents.lines() {
        let line = line.trim();
        if line.starts_with('[') && line.ends_with(']') {
            section = line.to_string();
            if section == "[[bin]]" {
                declared.push(Binary::default());
            }
            continue;
        }
        let Some((key, value)) = line.split_once('=') else {
            continue;
        };
        let value = value.trim();
        match (section.as_str(), key.trim()) {
            ("[package]", "name") => {
                let name = value.trim_matches('"');
                if !name.is_empty() {
                    package_name = Some(name.to_string());
                }
            }
            ("[[bin]]", "name") => {
                let name = value.trim_matches('"');
                if let Some(binary) = declared.last_mut()
                    && !name.is_empty()
                {
                    binary.name = name.to_string();
                }
            }
            ("[[bin]]", "required-features") => {
                if let Some(binary) = declared.last_mut() {
                    binary.features = features_named(value);
                }
            }
            _ => {}
        }
    }
    // A `[[bin]]` with only a path is one cargo names after that path; there is
    // nothing here to name it by, so it is left out rather than guessed at.
    declared.retain(|binary| !binary.name.is_empty());

    let named_after_the_package =
        declared
            .is_empty()
            .then_some(package_name)
            .flatten()
            .map(|name| Offered {
                point: cargo_run(&name, &[], &at),
                only_if: Some(at.join("src").join("main.rs")),
            });
    declared
        .into_iter()
        .map(|binary| Offered {
            point: cargo_run(&binary.name, &binary.features, &at),
            only_if: None,
        })
        .chain(named_after_the_package)
        .collect()
}

/// Running one binary of a crate, from that crate's own directory.
fn cargo_run(binary: &str, features: &[String], at: &Path) -> EntryPoint {
    let mut args = vec!["run".to_string(), "--bin".to_string(), binary.to_string()];
    // A binary cargo only builds under some features cannot be run without
    // them, and the manifest is the only place that says which.
    if !features.is_empty() {
        args.push("--features".to_string());
        args.push(features.join(","));
    }
    EntryPoint {
        name: binary.to_string(),
        family: Family::Rust,
        how: HowToRun {
            command: "cargo".to_string(),
            args,
            cwd: Some(cwd_for(&at.to_path_buf())),
            env: HashMap::default(),
        },
        debugger: Some("CodeLLDB"),
    }
}

/// The features a `required-features` line names, as written on one line. A
/// list spread over several lines reads as none, which offers the plain command
/// rather than a wrong one.
fn features_named(value: &str) -> Vec<String> {
    value
        .trim_start_matches('[')
        .trim_end_matches(']')
        .split(',')
        .map(|feature| feature.trim().trim_matches('"').to_string())
        .filter(|feature| !feature.is_empty())
        .collect()
}

fn node_scripts(path: &Path, contents: &str) -> Vec<Offered> {
    let Ok(manifest) = serde_json::from_str::<serde_json::Value>(contents) else {
        return Vec::new();
    };
    let Some(scripts) = manifest
        .get("scripts")
        .and_then(|scripts| scripts.as_object())
    else {
        return Vec::new();
    };
    let at = path.parent().map(Path::to_path_buf).unwrap_or_default();
    scripts
        .keys()
        .map(|script| {
            EntryPoint {
                name: format!("npm run {script}"),
                family: Family::Node,
                how: HowToRun {
                    command: "npm".to_string(),
                    args: vec!["run".into(), script.clone()],
                    cwd: Some(cwd_for(&at)),
                    env: HashMap::default(),
                },
                debugger: Some("JavaScript"),
            }
            .on_its_own()
        })
        .collect()
}

fn python_module(path: &Path, name: &str, contents: &str) -> Option<Offered> {
    // The guard itself, not the word: `__main__` turns up in docstrings and in
    // imports of modules that are not programs.
    let is_a_program = name == "__main__.py" || has_a_main_guard(contents);
    // A test file is started by its runner, not by `python3 <file>`.
    let is_a_test = name == "conftest.py"
        || name.starts_with("test_")
        || path.components().any(|part| part.as_os_str() == "tests");
    if !is_a_program || is_a_test {
        return None;
    }
    let shown = path.to_string_lossy().replace('\\', "/");
    Some(
        EntryPoint {
            name: shown.clone(),
            family: Family::Python,
            how: plain("python3", vec![shown]),
            debugger: Some("Debugpy"),
        }
        .on_its_own(),
    )
}

/// Whether a Python file guards a program on being the module that was run.
fn has_a_main_guard(contents: &str) -> bool {
    contents.lines().any(|line| {
        let line = line.trim_start();
        line.starts_with("if __name__") && line.contains("==") && line.contains("__main__")
    })
}

/// The image a Dockerfile builds, when running it starts anything. A file with
/// no `CMD` and no `ENTRYPOINT` builds an image that has nothing to run -- a
/// builder stage, or a base image for CI -- and `docker run` on it fails.
fn docker_image(path: &Path, contents: &str) -> Option<Offered> {
    let starts_something = contents.lines().any(|line| {
        let line = line.trim_start().to_ascii_uppercase();
        line.starts_with("CMD ")
            || line.starts_with("CMD[")
            || line.starts_with("ENTRYPOINT ")
            || line.starts_with("ENTRYPOINT[")
    });
    if !starts_something {
        return None;
    }
    let file = path.to_string_lossy().replace('\\', "/");
    // Built and run in one press, tagged after the file so a second press
    // replaces the image rather than leaving a heap of untagged ones behind.
    let tag = format!("zed-run/{}", file.replace(['/', '.'], "-").to_lowercase());
    Some(
        EntryPoint {
            name: file.clone(),
            family: Family::Docker,
            how: HowToRun {
                command: "sh".to_string(),
                args: vec![
                    "-c".into(),
                    format!("docker build -f {file} -t {tag} . && docker run --rm -it {tag}"),
                ],
                cwd: Some("$ZED_WORKTREE_ROOT".to_string()),
                env: HashMap::default(),
            },
            debugger: None,
        }
        .on_its_own(),
    )
}

/// The services a compose file names. Only the keys one level under `services:`
/// count, which is what the format says a service is; anything deeper belongs to
/// a service rather than being one.
fn compose_services(path: &Path, contents: &str) -> Vec<Offered> {
    let file = path.to_string_lossy().replace('\\', "/");
    let mut services = Vec::new();
    let mut inside = false;
    let mut indent_of_a_service = None;
    for line in contents.lines() {
        if line.trim_start().starts_with('#') || line.trim().is_empty() {
            continue;
        }
        let indent = line.len() - line.trim_start().len();
        if indent == 0 {
            inside = line.trim_start().starts_with("services:");
            indent_of_a_service = None;
            continue;
        }
        if !inside {
            continue;
        }
        let depth = *indent_of_a_service.get_or_insert(indent);
        if indent != depth {
            continue;
        }
        let Some(name) = line.trim().strip_suffix(':') else {
            continue;
        };
        if !name.is_empty() {
            services.push(name.to_string());
        }
    }
    services
        .into_iter()
        .map(|service| {
            EntryPoint {
                name: format!("{file} · {service}"),
                family: Family::Compose,
                how: HowToRun {
                    command: "docker".to_string(),
                    args: vec![
                        "compose".into(),
                        "-f".into(),
                        file.clone(),
                        "up".into(),
                        "--build".into(),
                        service,
                    ],
                    cwd: Some("$ZED_WORKTREE_ROOT".to_string()),
                    env: HashMap::default(),
                },
                debugger: None,
            }
            .on_its_own()
        })
        .collect()
}

fn cwd_for(at: &PathBuf) -> String {
    match at.as_os_str().is_empty() {
        true => "$ZED_WORKTREE_ROOT".to_string(),
        false => format!(
            "$ZED_WORKTREE_ROOT/{}",
            at.to_string_lossy().replace('\\', "/")
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ways(path: &str, contents: &str) -> Vec<EntryPoint> {
        offered(path, contents)
            .into_iter()
            .map(|offered| offered.point)
            .collect()
    }

    /// The same, keeping what each way needs to be real -- which is the whole
    /// question for a Cargo manifest with no `[[bin]]`.
    fn offered(path: &str, contents: &str) -> Vec<Offered> {
        ways_to_run(Path::new(path), contents)
    }

    #[test]
    fn only_files_that_could_say_something_are_opened() {
        for path in [
            "cmd/api/main.go",
            "Cargo.toml",
            "web/package.json",
            "tool/__main__.py",
            "script.py",
            "Dockerfile",
            "Dockerfile.dev",
            "compose.yaml",
            "docker-compose.override.yml",
        ] {
            assert!(worth_reading(Path::new(path)), "{path} has to be read");
        }
        for path in [
            "README.md",
            "target/debug/thing",
            "src/lib.rs",
            "go.sum",
            "compose.json",
            "notdocker-compose.yml",
        ] {
            assert!(
                !worth_reading(Path::new(path)),
                "{path} must not be opened for nothing"
            );
        }
    }

    #[test]
    fn a_go_program_names_its_own_package() {
        let found = ways("cmd/api/main.go", "package main\n\nfunc main() {}\n");
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].name, "./cmd/api");
        assert_eq!(found[0].how.command, "go");
        assert_eq!(found[0].how.args, vec!["run", "./cmd/api"]);
        assert_eq!(found[0].debugger, Some("Delve"));
    }

    #[test]
    fn a_go_file_that_is_not_a_program_is_no_way_to_run_anything() {
        assert!(ways("internal/store/store.go", "package store\n").is_empty());
        assert!(
            ways("cmd/api/helper.go", "package main\n\nfunc helper() {}\n").is_empty(),
            "a file of the main package without an entry point starts nothing"
        );
    }

    #[test]
    fn a_go_program_at_the_root_names_the_root() {
        let found = ways("main.go", "package main\nfunc main() {}\n");
        assert_eq!(found[0].name, ".");
        assert_eq!(found[0].how.args, vec!["run", "."]);
    }

    #[test]
    fn a_manifest_names_every_binary_it_declares() {
        let found = ways(
            "Cargo.toml",
            "[package]\nname = \"thing\"\n\n[[bin]]\nname = \"server\"\n\n[[bin]]\nname = \"cli\"\n",
        );
        let named: Vec<&str> = found.iter().map(|entry| entry.name.as_str()).collect();
        assert_eq!(named, vec!["server", "cli"]);
        assert_eq!(found[0].how.args, vec!["run", "--bin", "server"]);
    }

    #[test]
    /// A crate with no `[[bin]]` is a way of running only if it has a
    /// `src/main.rs`. Offering it unconditionally is what filled this
    /// workspace's list with library crates that `cargo run --bin` refuses.
    fn a_manifest_with_no_binaries_is_a_way_to_run_only_with_a_main() {
        let found = offered(
            "crates/thing/Cargo.toml",
            "[package]\nname = \"thing\"\nversion = \"0\"\n",
        );
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].point.name, "thing");
        assert_eq!(
            found[0].only_if.as_deref(),
            Some(Path::new("crates/thing/src/main.rs")),
            "the package name is a binary only where there is a program to build"
        );
    }

    /// A binary cargo will not build without a feature cannot be run without
    /// it either, and the manifest is the only place that says which.
    #[test]
    fn a_binary_behind_a_feature_is_offered_with_it() {
        let found = offered(
            "crates/zed/Cargo.toml",
            "[package]\nname = \"zed\"\n\n[[bin]]\nname = \"zed\"\n\n\
             [[bin]]\nname = \"runner\"\nrequired-features = [\"visual-tests\"]\n",
        );
        assert_eq!(found.len(), 2);
        assert_eq!(found[0].point.how.args, vec!["run", "--bin", "zed"]);
        assert_eq!(
            found[1].point.how.args,
            vec!["run", "--bin", "runner", "--features", "visual-tests"]
        );
        assert!(
            found.iter().all(|offered| offered.only_if.is_none()),
            "a declared binary is real on the manifest's word alone"
        );
    }

    /// A test file is started by its runner, not by `python3 <file>`.
    #[test]
    fn a_test_file_is_no_way_to_run_the_project() {
        let program = "if __name__ == \"__main__\":\n    run()\n";
        assert!(ways("pkg/tests/test_report.py", program).is_empty());
        assert!(ways("pkg/conftest.py", program).is_empty());
        assert!(
            !ways("script/triage.py", program).is_empty(),
            "a program that is not a test is still a way of running"
        );
        assert!(
            ways("tool/helpers.py", "# see __main__ for the entry point\n").is_empty(),
            "the word in a comment is not the guard"
        );
    }

    #[test]
    fn a_manifest_in_a_member_runs_from_that_member() {
        let found = ways("crates/server/Cargo.toml", "[package]\nname = \"server\"\n");
        assert_eq!(
            found[0].how.cwd.as_deref(),
            Some("$ZED_WORKTREE_ROOT/crates/server")
        );
    }

    #[test]
    fn every_script_of_a_package_is_a_way_to_run_it() {
        let found = ways(
            "web/package.json",
            r#"{ "name": "web", "scripts": { "dev": "vite", "build": "vite build" } }"#,
        );
        let named: Vec<&str> = found.iter().map(|entry| entry.name.as_str()).collect();
        assert!(named.contains(&"npm run dev") && named.contains(&"npm run build"));
        assert_eq!(found[0].how.cwd.as_deref(), Some("$ZED_WORKTREE_ROOT/web"));
    }

    #[test]
    fn a_package_that_is_not_json_says_nothing_rather_than_guessing() {
        assert!(ways("package.json", "{ this is not json").is_empty());
    }

    #[test]
    fn a_compose_file_names_its_services_and_not_their_settings() {
        let found = ways(
            "compose.yaml",
            "services:\n  api:\n    build: .\n    ports:\n      - 8080:8080\n  db:\n    image: postgres\nvolumes:\n  data:\n",
        );
        let named: Vec<&str> = found.iter().map(|entry| entry.name.as_str()).collect();
        assert_eq!(named, vec!["compose.yaml · api", "compose.yaml · db"]);
        assert_eq!(
            found[0].how.args,
            vec!["compose", "-f", "compose.yaml", "up", "--build", "api"]
        );
        assert!(
            !named.iter().any(|name| name.contains("data")),
            "a volume is not a service"
        );
    }

    #[test]
    fn a_dockerfile_builds_and_runs_in_one_press() {
        let found = ways("Dockerfile.dev", "FROM alpine\nCMD [\"/app\"]\n");
        assert_eq!(found.len(), 1);
        let command = found[0].how.args.join(" ");
        assert!(
            command.contains("docker build -f Dockerfile.dev")
                && command.contains("docker run --rm -it"),
            "{command}"
        );
        assert_eq!(found[0].debugger, None);
    }

    /// An image with nothing to start is a builder stage or a base image, and
    /// `docker run` on it fails; a `.dockerignore` is not an image at all.
    #[test]
    fn a_dockerfile_that_starts_nothing_is_no_way_to_run_anything() {
        assert!(
            ways("Dockerfile", "FROM rust:1.95 AS builder\nRUN cargo build\n").is_empty(),
            "a builder stage has nothing to run"
        );
        assert!(
            ways(
                "crates/eval_cli/Dockerfile.dockerignore",
                ".git\n**/target\n"
            )
            .is_empty(),
            "a .dockerignore is a file about a Dockerfile, not one"
        );
    }

    #[test]
    fn a_python_file_with_no_entry_point_starts_nothing() {
        assert!(ways("tool/helpers.py", "def helper():\n    pass\n").is_empty());
        let found = ways(
            "tool/run.py",
            "if __name__ == \"__main__\":\n    print(1)\n",
        );
        assert_eq!(found[0].how.args, vec!["tool/run.py"]);
    }

    #[test]
    fn the_files_that_hold_an_environment_are_recognised() {
        for path in [".env", ".env.local", "deploy/staging.env"] {
            assert!(is_env_file(Path::new(path)), "{path} holds an environment");
        }
        for path in ["environment.md", "src/env.rs", "envs"] {
            assert!(!is_env_file(Path::new(path)), "{path} does not");
        }
    }
}
