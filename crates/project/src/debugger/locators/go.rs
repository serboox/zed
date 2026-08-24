use std::{
    env::consts::EXE_SUFFIX,
    hash::{DefaultHasher, Hash, Hasher},
    path::{Path, PathBuf},
};

use anyhow::Result;
use async_trait::async_trait;
use collections::HashMap;
use dap::{DapLocator, DebugRequest, adapters::DebugAdapterName};
use gpui::{BackgroundExecutor, SharedString};
use serde::{Deserialize, Serialize};
use task::{DebugScenario, SpawnInTerminal, TaskTemplate};

pub struct GoLocator;

/// Delve builds the debuggee itself when `mode` is `debug` or `test`, and without an explicit
/// `output` it writes the binary into the working directory (e.g. `./debug`), leaving a stray
/// untracked executable in the project tree. This picks a path under Zed's cache directory
/// instead, keyed by the working directory and the resolved configuration so repeated debug runs
/// of the same configuration reuse one path while different configurations don't collide.
fn debug_binary_output_path(build_config: &TaskTemplate, resolved_label: &str) -> PathBuf {
    let mut hasher = DefaultHasher::new();
    build_config
        .cwd
        .as_deref()
        .unwrap_or_default()
        .hash(&mut hasher);
    resolved_label.hash(&mut hasher);
    build_config.args.hash(&mut hasher);

    paths::temp_dir()
        .join("go-debug-builds")
        .join(format!("{:016x}{EXE_SUFFIX}", hasher.finish()))
}

/// Ensures the parent directory of a go debug build output path exists, so Delve's build step
/// doesn't fail trying to write into a missing cache directory.
fn ensure_output_dir_exists(output_path: &Path) {
    if let Some(parent) = output_path.parent() {
        if let Err(error) = std::fs::create_dir_all(parent) {
            log::warn!("Failed to create go debug build directory {parent:?}: {error}");
        }
    }
}

#[derive(Serialize, Deserialize, Debug, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct DelveLaunchRequest {
    pub request: String,
    pub mode: String,
    pub program: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cwd: Option<String>,
    pub args: Vec<String>,
    pub build_flags: Vec<String>,
    pub env: HashMap<String, String>,
}

fn is_debug_flag(arg: &str) -> Option<bool> {
    let mut part = if let Some(suffix) = arg.strip_prefix("test.") {
        suffix
    } else {
        arg
    };
    let mut might_have_arg = true;
    if let Some(idx) = part.find('=') {
        might_have_arg = false;
        part = &part[..idx];
    }
    match part {
        "benchmem" | "failfast" | "fullpath" | "fuzzworker" | "json" | "short" | "v"
        | "paniconexit0" => Some(false),
        "bench"
        | "benchtime"
        | "blockprofile"
        | "blockprofilerate"
        | "count"
        | "coverprofile"
        | "cpu"
        | "cpuprofile"
        | "fuzz"
        | "fuzzcachedir"
        | "fuzzminimizetime"
        | "fuzztime"
        | "gocoverdir"
        | "list"
        | "memprofile"
        | "memprofilerate"
        | "mutexprofile"
        | "mutexprofilefraction"
        | "outputdir"
        | "parallel"
        | "run"
        | "shuffle"
        | "skip"
        | "testlogfile"
        | "timeout"
        | "trace" => Some(might_have_arg),
        _ if arg.starts_with("test.") => Some(false),
        _ => None,
    }
}

fn is_build_flag(mut arg: &str) -> Option<bool> {
    let mut might_have_arg = true;
    if let Some(idx) = arg.find('=') {
        might_have_arg = false;
        arg = &arg[..idx];
    }
    match arg {
        "a" | "n" | "race" | "msan" | "asan" | "cover" | "work" | "x" | "v" | "buildvcs"
        | "json" | "linkshared" | "modcacherw" | "trimpath" => Some(false),

        "p" | "covermode" | "coverpkg" | "asmflags" | "buildmode" | "compiler" | "gccgoflags"
        | "gcflags" | "installsuffix" | "ldflags" | "mod" | "modfile" | "overlay" | "pgo"
        | "pkgdir" | "tags" | "toolexec" => Some(might_have_arg),
        _ => None,
    }
}

#[async_trait]
impl DapLocator for GoLocator {
    fn name(&self) -> SharedString {
        SharedString::new_static("go-debug-locator")
    }

    async fn create_scenario(
        &self,
        build_config: &TaskTemplate,
        resolved_label: &str,
        adapter: &DebugAdapterName,
    ) -> Option<DebugScenario> {
        if build_config.command != "go" {
            return None;
        }
        let go_action = build_config.args.first()?;

        match go_action.as_str() {
            "test" => {
                let mut program = ".".to_string();
                let mut args = Vec::default();
                let mut build_flags = Vec::default();

                let mut all_args_are_test = false;
                let mut next_arg_is_test = false;
                let mut next_arg_is_build = false;
                let mut seen_pkg = false;
                let mut seen_v = false;

                for arg in build_config.args.iter().skip(1) {
                    if all_args_are_test || next_arg_is_test {
                        // HACK: tasks assume that they are run in a shell context,
                        // so the -run regex has escaped specials. Delve correctly
                        // handles escaping, so we undo that here.
                        if let Some((left, right)) = arg.split_once("/")
                            && left.starts_with("\\^")
                            && left.ends_with("\\$")
                            && right.starts_with("\\^")
                            && right.ends_with("\\$")
                        {
                            let mut left = left[1..left.len() - 2].to_string();
                            left.push('$');

                            let mut right = right[1..right.len() - 2].to_string();
                            right.push('$');

                            args.push(format!("{left}/{right}"));
                        } else if arg.starts_with("\\^") && arg.ends_with("\\$") {
                            let mut arg = arg[1..arg.len() - 2].to_string();
                            arg.push('$');
                            args.push(arg);
                        } else {
                            args.push(arg.clone());
                        }
                        next_arg_is_test = false;
                    } else if next_arg_is_build {
                        build_flags.push(arg.clone());
                        next_arg_is_build = false;
                    } else if arg.starts_with('-') {
                        let flag = arg.trim_start_matches('-');
                        if flag == "args" {
                            all_args_are_test = true;
                        } else if let Some(has_arg) = is_debug_flag(flag) {
                            if flag == "v" || flag == "test.v" {
                                seen_v = true;
                            }
                            if flag.starts_with("test.") {
                                args.push(arg.clone());
                            } else {
                                args.push(format!("-test.{flag}"))
                            }
                            next_arg_is_test = has_arg;
                        } else if let Some(has_arg) = is_build_flag(flag) {
                            build_flags.push(arg.clone());
                            next_arg_is_build = has_arg;
                        }
                    } else if !seen_pkg {
                        program = arg.clone();
                        seen_pkg = true;
                    } else {
                        args.push(arg.clone());
                    }
                }
                if !seen_v {
                    args.push("-test.v".to_string());
                }

                let mut config: serde_json::Value = serde_json::to_value(DelveLaunchRequest {
                    request: "launch".to_string(),
                    mode: "test".to_string(),
                    program,
                    args,
                    build_flags,
                    cwd: build_config.cwd.clone(),
                    env: build_config.env.clone(),
                })
                .unwrap();

                let output_path = debug_binary_output_path(build_config, resolved_label);
                ensure_output_dir_exists(&output_path);
                if let Some(config_object) = config.as_object_mut() {
                    config_object.insert(
                        "output".to_string(),
                        output_path.to_string_lossy().into_owned().into(),
                    );
                }

                Some(DebugScenario {
                    label: resolved_label.to_string().into(),
                    adapter: adapter.0.clone(),
                    build: None,
                    config,
                    tcp_connection: None,
                })
            }
            "run" => {
                let mut next_arg_is_build = false;
                let mut seen_pkg = false;

                let mut program = ".".to_string();
                let mut args = Vec::default();
                let mut build_flags = Vec::default();

                for arg in build_config.args.iter().skip(1) {
                    if seen_pkg {
                        args.push(arg.clone())
                    } else if next_arg_is_build {
                        build_flags.push(arg.clone());
                        next_arg_is_build = false;
                    } else if arg.starts_with("-") {
                        if let Some(has_arg) = is_build_flag(arg.trim_start_matches("-")) {
                            next_arg_is_build = has_arg;
                        }
                        build_flags.push(arg.clone())
                    } else {
                        program = arg.to_string();
                        seen_pkg = true;
                    }
                }

                let mut config: serde_json::Value = serde_json::to_value(DelveLaunchRequest {
                    cwd: build_config.cwd.clone(),
                    env: build_config.env.clone(),
                    request: "launch".to_string(),
                    mode: "debug".to_string(),
                    program,
                    args,
                    build_flags,
                })
                .unwrap();

                let output_path = debug_binary_output_path(build_config, resolved_label);
                ensure_output_dir_exists(&output_path);
                if let Some(config_object) = config.as_object_mut() {
                    config_object.insert(
                        "output".to_string(),
                        output_path.to_string_lossy().into_owned().into(),
                    );
                }

                Some(DebugScenario {
                    label: resolved_label.to_string().into(),
                    adapter: adapter.0.clone(),
                    build: None,
                    config,
                    tcp_connection: None,
                })
            }
            _ => None,
        }
    }

    async fn run(
        &self,
        _build_config: SpawnInTerminal,
        _executor: BackgroundExecutor,
    ) -> Result<DebugRequest> {
        unreachable!()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use gpui::TestAppContext;

    fn go_task(args: &[&str], cwd: Option<&str>) -> TaskTemplate {
        TaskTemplate {
            label: "go task".into(),
            command: "go".into(),
            args: args.iter().map(|arg| arg.to_string()).collect(),
            cwd: cwd.map(str::to_string),
            ..Default::default()
        }
    }

    fn output_path_of(config: &serde_json::Value) -> PathBuf {
        PathBuf::from(
            config
                .get("output")
                .and_then(serde_json::Value::as_str)
                .expect("Delve launch config should carry an explicit `output` path"),
        )
    }

    #[gpui::test]
    async fn go_run_builds_into_cache_dir_outside_project(_: &mut TestAppContext) {
        let locator = GoLocator;
        let delve = DebugAdapterName("Delve".into());
        let task = go_task(&["run", "."], Some("/home/example/project"));

        let scenario = locator
            .create_scenario(&task, "Run main", &delve)
            .await
            .unwrap();

        let output_path = output_path_of(&scenario.config);
        assert!(
            output_path.starts_with(paths::temp_dir()),
            "expected {output_path:?} to be under the Zed cache dir {:?}",
            paths::temp_dir()
        );
        assert!(
            !output_path.starts_with("/home/example/project"),
            "debug build output must not land inside the project directory, got {output_path:?}"
        );
    }

    #[gpui::test]
    async fn go_test_also_builds_into_cache_dir(_: &mut TestAppContext) {
        let locator = GoLocator;
        let delve = DebugAdapterName("Delve".into());
        let task = go_task(&["test", "./..."], Some("/home/example/project"));

        let scenario = locator
            .create_scenario(&task, "Run tests", &delve)
            .await
            .unwrap();

        let output_path = output_path_of(&scenario.config);
        assert!(output_path.starts_with(paths::temp_dir()));
        assert!(!output_path.starts_with("/home/example/project"));
    }

    #[gpui::test]
    async fn same_configuration_reuses_the_same_output_path(_: &mut TestAppContext) {
        let locator = GoLocator;
        let delve = DebugAdapterName("Delve".into());
        let task = go_task(&["run", "."], Some("/home/example/project"));

        let first = locator
            .create_scenario(&task, "Run main", &delve)
            .await
            .unwrap();
        let second = locator
            .create_scenario(&task, "Run main", &delve)
            .await
            .unwrap();

        assert_eq!(
            output_path_of(&first.config),
            output_path_of(&second.config)
        );
    }

    #[gpui::test]
    async fn different_configurations_get_different_output_paths(_: &mut TestAppContext) {
        let locator = GoLocator;
        let delve = DebugAdapterName("Delve".into());
        let task_a = go_task(&["run", "."], Some("/home/example/project"));
        let task_b = go_task(&["run", "./cmd/worker"], Some("/home/example/project"));

        let scenario_a = locator
            .create_scenario(&task_a, "Run main", &delve)
            .await
            .unwrap();
        let scenario_b = locator
            .create_scenario(&task_b, "Run worker", &delve)
            .await
            .unwrap();

        assert_ne!(
            output_path_of(&scenario_a.config),
            output_path_of(&scenario_b.config)
        );
    }

    #[gpui::test]
    async fn non_go_command_is_rejected(_: &mut TestAppContext) {
        let locator = GoLocator;
        let delve = DebugAdapterName("Delve".into());
        let task = TaskTemplate {
            label: "cargo build".into(),
            command: "cargo".into(),
            args: vec!["build".into()],
            ..Default::default()
        };

        assert!(
            locator
                .create_scenario(&task, "cargo build", &delve)
                .await
                .is_none()
        );
    }

    #[gpui::test]
    async fn unsupported_go_action_is_rejected(_: &mut TestAppContext) {
        let locator = GoLocator;
        let delve = DebugAdapterName("Delve".into());
        let task = go_task(&["clean"], None);

        assert!(
            locator
                .create_scenario(&task, "go clean", &delve)
                .await
                .is_none()
        );
    }

    #[gpui::test]
    async fn go_build_is_still_unhandled(_: &mut TestAppContext) {
        let locator = GoLocator;
        let delve = DebugAdapterName("Delve".into());
        let task = go_task(&["build", "."], None);

        assert!(
            locator
                .create_scenario(&task, "go build", &delve)
                .await
                .is_none()
        );
    }
}
