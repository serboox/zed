use std::path::{Path, PathBuf};

use collections::HashMap;
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

/// Every way of starting this project that its own files describe.
///
/// Read from the worktree rather than asked of the reader: a field to type a
/// package path into is a form, and what the editor can find it should find.
pub fn look_through(project: &Entity<Project>, cx: &App) -> Task<Vec<EntryPoint>> {
    let project = project.read(cx);
    let fs = project.fs().clone();
    let mut to_read: Vec<(PathBuf, PathBuf)> = Vec::new();
    for worktree in project.visible_worktrees(cx) {
        let worktree = worktree.read(cx);
        let root = worktree.abs_path().to_path_buf();
        for entry in worktree.entries(false, 0) {
            if to_read.len() >= AT_MOST {
                break;
            }
            if !entry.is_file() {
                continue;
            }
            let relative = entry.path.as_std_path();
            if worth_reading(relative) {
                to_read.push((relative.to_path_buf(), root.join(relative)));
            }
        }
    }
    cx.background_spawn(async move {
        let mut found = Vec::new();
        for (relative, absolute) in to_read {
            match fs.load(&absolute).await {
                Ok(contents) => found.extend(ways_to_run(&relative, &contents)),
                // A file that cannot be read is one way fewer to offer, not a
                // reason to offer none: it may have been deleted between the
                // worktree scan and this read.
                Err(error) => log::debug!("{}: {error}", absolute.display()),
            }
        }
        found.sort_by(|one, other| {
            one.family
                .cmp(&other.family)
                .then_with(|| one.name.cmp(&other.name))
        });
        found.dedup();
        found
    })
}

/// The environment files of the project, for the field that names one. Their
/// paths alone are the answer, so nothing is read.
pub fn env_files(project: &Entity<Project>, cx: &App) -> Vec<PathBuf> {
    let project = project.read(cx);
    let mut found = Vec::new();
    for worktree in project.visible_worktrees(cx) {
        let worktree = worktree.read(cx);
        for entry in worktree.entries(false, 0) {
            if found.len() >= AT_MOST {
                break;
            }
            let relative = entry.path.as_std_path();
            if entry.is_file() && is_env_file(relative) {
                found.push(relative.to_path_buf());
            }
        }
    }
    found.sort();
    found
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

fn is_dockerfile(name: &str) -> bool {
    name == "Dockerfile" || name.starts_with("Dockerfile.")
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
pub fn ways_to_run(path: &Path, contents: &str) -> Vec<EntryPoint> {
    let Some(name) = path.file_name().and_then(|name| name.to_str()) else {
        return Vec::new();
    };
    if is_compose_file(name) {
        return compose_services(path, contents);
    }
    if is_dockerfile(name) {
        return vec![docker_image(path)];
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

fn go_package(path: &Path, contents: &str) -> Option<EntryPoint> {
    let is_a_program = contents
        .lines()
        .any(|line| line.trim_start().starts_with("package main"))
        && contents.contains("func main(");
    if !is_a_program {
        return None;
    }
    let package = package_of(path);
    Some(EntryPoint {
        name: package.clone(),
        family: Family::Go,
        how: plain("go", vec!["run".into(), package]),
        debugger: Some("Delve"),
    })
}

/// The binaries a Cargo manifest names. Read line by line rather than parsed:
/// only two keys matter, the list is a suggestion the reader can edit, and a
/// manifest this cannot read leaves the list shorter rather than wrong.
fn cargo_binaries(path: &Path, contents: &str) -> Vec<EntryPoint> {
    let mut found = Vec::new();
    let mut section = String::new();
    let mut package_name = None;
    for line in contents.lines() {
        let line = line.trim();
        if line.starts_with('[') && line.ends_with(']') {
            section = line.to_string();
            continue;
        }
        let Some(value) = line.strip_prefix("name") else {
            continue;
        };
        let Some(value) = value.trim_start().strip_prefix('=') else {
            continue;
        };
        let name = value.trim().trim_matches('"').to_string();
        if name.is_empty() {
            continue;
        }
        match section.as_str() {
            "[package]" => package_name = Some(name),
            "[[bin]]" => found.push(name),
            _ => {}
        }
    }
    // A manifest with no `[[bin]]` still builds one binary, named after the
    // package, whenever it has a `src/main.rs` -- which is the usual case and
    // the one a reader is looking for.
    if found.is_empty()
        && let Some(package_name) = package_name
    {
        found.push(package_name);
    }
    let at = path.parent().map(Path::to_path_buf).unwrap_or_default();
    found
        .into_iter()
        .map(|binary| EntryPoint {
            name: binary.clone(),
            family: Family::Rust,
            how: HowToRun {
                command: "cargo".to_string(),
                args: vec!["run".into(), "--bin".into(), binary],
                cwd: Some(cwd_for(&at)),
                env: HashMap::default(),
            },
            debugger: Some("CodeLLDB"),
        })
        .collect()
}

fn node_scripts(path: &Path, contents: &str) -> Vec<EntryPoint> {
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
        .map(|script| EntryPoint {
            name: format!("npm run {script}"),
            family: Family::Node,
            how: HowToRun {
                command: "npm".to_string(),
                args: vec!["run".into(), script.clone()],
                cwd: Some(cwd_for(&at)),
                env: HashMap::default(),
            },
            debugger: Some("JavaScript"),
        })
        .collect()
}

fn python_module(path: &Path, name: &str, contents: &str) -> Option<EntryPoint> {
    let is_a_program = name == "__main__.py" || contents.contains("__main__");
    if !is_a_program {
        return None;
    }
    let shown = path.to_string_lossy().replace('\\', "/");
    Some(EntryPoint {
        name: shown.clone(),
        family: Family::Python,
        how: plain("python3", vec![shown]),
        debugger: Some("Debugpy"),
    })
}

fn docker_image(path: &Path) -> EntryPoint {
    let file = path.to_string_lossy().replace('\\', "/");
    // Built and run in one press, tagged after the file so a second press
    // replaces the image rather than leaving a heap of untagged ones behind.
    let tag = format!("zed-run/{}", file.replace(['/', '.'], "-").to_lowercase());
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
}

/// The services a compose file names. Only the keys one level under `services:`
/// count, which is what the format says a service is; anything deeper belongs to
/// a service rather than being one.
fn compose_services(path: &Path, contents: &str) -> Vec<EntryPoint> {
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
        .map(|service| EntryPoint {
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
    fn a_manifest_with_no_binaries_falls_back_to_the_package() {
        let found = ways(
            "Cargo.toml",
            "[package]\nname = \"thing\"\nversion = \"0\"\n",
        );
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].name, "thing");
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
        let found = ways("Dockerfile.dev", "FROM alpine\n");
        assert_eq!(found.len(), 1);
        let command = found[0].how.args.join(" ");
        assert!(
            command.contains("docker build -f Dockerfile.dev")
                && command.contains("docker run --rm -it"),
            "{command}"
        );
        assert_eq!(found[0].debugger, None);
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
