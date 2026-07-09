use std::collections::BTreeMap;

use anyhow::{Context as _, Result, bail};
use boa_engine::{Context, Source};

/// One `pm.test(name, fn)` outcome.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TestResult {
    pub name: String,
    pub passed: bool,
    pub error: Option<String>,
}

/// The subset of a request a script can read -- read-only by design; a
/// script mutates behavior through `pm.environment`/`pm.collectionVariables`
/// instead of poking at the request object directly, mirroring Postman.
#[derive(Debug, Clone, Default)]
pub struct ScriptRequestData {
    pub method: String,
    pub url: String,
    pub headers: Vec<(String, String)>,
    pub body: String,
}

/// The subset of a response a script can read -- only present for the
/// post-response ("Tests") script, never the pre-request script.
#[derive(Debug, Clone, Default)]
pub struct ScriptResponseData {
    pub status: u16,
    pub headers: Vec<(String, String)>,
    pub body: String,
}

/// Everything a script run can produce: updated variable scopes (a script
/// may call `pm.environment.set`/`pm.collectionVariables.set`), any
/// `pm.test()` results, an optional `pm.visualize()` payload, and captured
/// `console.log` output.
#[derive(Debug, Clone, Default)]
pub struct ScriptRunResult {
    pub environment: BTreeMap<String, String>,
    pub collection_variables: BTreeMap<String, String>,
    pub test_results: Vec<TestResult>,
    pub visualize_data: Option<serde_json::Value>,
    pub console_logs: Vec<String>,
}

/// The `pm.*` API surface, defined entirely in JS rather than as native Rust
/// bindings -- every piece of mutable state (`__env`, `__testResults`, ...)
/// is a plain JS global that Rust seeds before running the user script and
/// reads back afterwards, so there is no need for boa's GC-traced native
/// closures just to carry Rust state across calls.
const PM_PRELUDE: &str = r#"
(function () {
    function clone(value) { return value === undefined ? undefined : JSON.parse(JSON.stringify(value)); }

    globalThis.pm = {
        environment: {
            get(key) { return __env[key]; },
            set(key, value) { __env[key] = String(value); },
            unset(key) { delete __env[key]; },
            has(key) { return Object.prototype.hasOwnProperty.call(__env, key); },
        },
        collectionVariables: {
            get(key) { return __collectionVars[key]; },
            set(key, value) { __collectionVars[key] = String(value); },
            unset(key) { delete __collectionVars[key]; },
            has(key) { return Object.prototype.hasOwnProperty.call(__collectionVars, key); },
        },
        variables: {
            get(key) { return Object.prototype.hasOwnProperty.call(__env, key) ? __env[key] : __collectionVars[key]; },
            set(key, value) { __env[key] = String(value); },
        },
        request: __request,
        response: __response ? {
            code: __response.status,
            headers: __response.headers,
            json() { return JSON.parse(__response.body); },
            text() { return __response.body; },
        } : undefined,
        test(name, fn) {
            try {
                fn();
                __testResults.push({ name, passed: true, error: null });
            } catch (error) {
                __testResults.push({ name, passed: false, error: String(error && error.message ? error.message : error) });
            }
        },
        expect(actual) {
            function fail(message) { throw new Error(message); }
            const assertion = {
                to: {
                    equal(expected) {
                        if (actual !== expected) fail(`expected ${JSON.stringify(actual)} to equal ${JSON.stringify(expected)}`);
                        return assertion;
                    },
                    eql(expected) {
                        if (JSON.stringify(actual) !== JSON.stringify(expected)) fail(`expected ${JSON.stringify(actual)} to eql ${JSON.stringify(expected)}`);
                        return assertion;
                    },
                    include(expected) {
                        const ok = Array.isArray(actual) ? actual.includes(expected) : String(actual).includes(expected);
                        if (!ok) fail(`expected ${JSON.stringify(actual)} to include ${JSON.stringify(expected)}`);
                        return assertion;
                    },
                    get be() {
                        return {
                            get true() { if (actual !== true) fail(`expected ${JSON.stringify(actual)} to be true`); return assertion; },
                            get false() { if (actual !== false) fail(`expected ${JSON.stringify(actual)} to be false`); return assertion; },
                        };
                    },
                },
            };
            return assertion;
        },
        visualize(data) { __visualizeData = clone(data); },
    };

    globalThis.console = {
        log(...args) {
            __consoleLogs.push(args.map((value) => (typeof value === "string" ? value : JSON.stringify(value))).join(" "));
        },
    };
})();
"#;

fn map_to_json_object(map: &BTreeMap<String, String>) -> String {
    serde_json::to_string(map).unwrap_or_else(|_| "{}".to_string())
}

/// Runs `script` in a fresh, sandboxed `boa_engine::Context` seeded with
/// `environment`/`collection_variables` and the given request/response
/// snapshots, then returns the script's effects. Each call gets a brand new
/// `Context` -- no state or native bindings persist between runs, so a
/// misbehaving script can't affect anything outside `ScriptRunResult`.
///
/// This has no execution timeout: an infinite loop in a user script will
/// hang whatever thread calls this. Callers must run it off the UI thread
/// (a background executor), never inline on a GPUI foreground task.
pub fn run_script(
    script: &str,
    environment: &BTreeMap<String, String>,
    collection_variables: &BTreeMap<String, String>,
    request: Option<&ScriptRequestData>,
    response: Option<&ScriptResponseData>,
) -> Result<ScriptRunResult> {
    let mut context = Context::default();

    let request_json = match request {
        Some(request) => serde_json::json!({
            "method": request.method,
            "url": request.url,
            "headers": request.headers.iter().cloned().collect::<BTreeMap<_, _>>(),
            "body": request.body,
        })
        .to_string(),
        None => "null".to_string(),
    };
    let response_json = match response {
        Some(response) => serde_json::json!({
            "status": response.status,
            "headers": response.headers.iter().cloned().collect::<BTreeMap<_, _>>(),
            "body": response.body,
        })
        .to_string(),
        None => "null".to_string(),
    };

    let setup = format!(
        "globalThis.__env = {};\nglobalThis.__collectionVars = {};\nglobalThis.__request = {request_json};\nglobalThis.__response = {response_json};\nglobalThis.__testResults = [];\nglobalThis.__visualizeData = null;\nglobalThis.__consoleLogs = [];\n{PM_PRELUDE}",
        map_to_json_object(environment),
        map_to_json_object(collection_variables),
    );
    context
        .eval(Source::from_bytes(&setup))
        .map_err(|error| anyhow::anyhow!("failed to initialize the script sandbox: {error}"))?;

    context
        .eval(Source::from_bytes(script))
        .map_err(|error| anyhow::anyhow!("script error: {error}"))?;

    let read_back = |context: &mut Context, expression: &str| -> Result<String> {
        let value = context
            .eval(Source::from_bytes(expression))
            .map_err(|error| anyhow::anyhow!("failed to read back `{expression}`: {error}"))?;
        value
            .to_string(context)
            .map(|js_string| js_string.to_std_string_escaped())
            .map_err(|error| anyhow::anyhow!("failed to stringify `{expression}`: {error}"))
    };

    let environment_json = read_back(&mut context, "JSON.stringify(__env)")?;
    let collection_variables_json = read_back(&mut context, "JSON.stringify(__collectionVars)")?;
    let test_results_json = read_back(&mut context, "JSON.stringify(__testResults)")?;
    let visualize_json = read_back(&mut context, "JSON.stringify(__visualizeData)")?;
    let console_logs_json = read_back(&mut context, "JSON.stringify(__consoleLogs)")?;

    #[derive(serde::Deserialize)]
    struct RawTestResult {
        name: String,
        passed: bool,
        error: Option<String>,
    }

    let environment: BTreeMap<String, String> = serde_json::from_str(&environment_json)
        .context("script sandbox returned malformed environment JSON")?;
    let collection_variables: BTreeMap<String, String> =
        serde_json::from_str(&collection_variables_json)
            .context("script sandbox returned malformed collection-variables JSON")?;
    let raw_test_results: Vec<RawTestResult> = serde_json::from_str(&test_results_json)
        .context("script sandbox returned malformed test-results JSON")?;
    let visualize_data: serde_json::Value = serde_json::from_str(&visualize_json)
        .context("script sandbox returned malformed visualize JSON")?;
    let console_logs: Vec<String> = serde_json::from_str(&console_logs_json)
        .context("script sandbox returned malformed console-log JSON")?;

    Ok(ScriptRunResult {
        environment,
        collection_variables,
        test_results: raw_test_results
            .into_iter()
            .map(|raw| TestResult {
                name: raw.name,
                passed: raw.passed,
                error: raw.error,
            })
            .collect(),
        visualize_data: if visualize_data.is_null() {
            None
        } else {
            Some(visualize_data)
        },
        console_logs,
    })
}

/// Runs a script with no request/response context -- the shape a
/// pre-request script sees, since the request it is about to modify is not
/// yet "the request" the way the post-response script sees a finished one.
/// The caller is responsible for feeding `pm.request.*` fields it wants
/// visible in the pre-request phase via `run_script` directly if needed.
pub fn run_pre_request_script(
    script: &str,
    environment: &BTreeMap<String, String>,
    collection_variables: &BTreeMap<String, String>,
    request: &ScriptRequestData,
) -> Result<ScriptRunResult> {
    if script.trim().is_empty() {
        bail!("run_pre_request_script called with an empty script");
    }
    run_script(
        script,
        environment,
        collection_variables,
        Some(request),
        None,
    )
}

/// Runs the post-response ("Tests") script with both request and response
/// visible, matching Postman's `pm.request`/`pm.response` availability in
/// that phase.
pub fn run_test_script(
    script: &str,
    environment: &BTreeMap<String, String>,
    collection_variables: &BTreeMap<String, String>,
    request: &ScriptRequestData,
    response: &ScriptResponseData,
) -> Result<ScriptRunResult> {
    if script.trim().is_empty() {
        bail!("run_test_script called with an empty script");
    }
    run_script(
        script,
        environment,
        collection_variables,
        Some(request),
        Some(response),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn empty_maps() -> (BTreeMap<String, String>, BTreeMap<String, String>) {
        (BTreeMap::new(), BTreeMap::new())
    }

    #[test]
    fn a_script_can_read_and_set_an_environment_variable() {
        let (mut environment, collection_variables) = empty_maps();
        environment.insert(
            "base_url".to_string(),
            "https://api.example.com".to_string(),
        );
        let result = run_script(
            "pm.environment.set('token', pm.environment.get('base_url').length.toString());",
            &environment,
            &collection_variables,
            None,
            None,
        )
        .unwrap();
        assert_eq!(result.environment.get("token"), Some(&"23".to_string()));
    }

    #[test]
    fn a_script_can_set_a_collection_variable() {
        let (environment, collection_variables) = empty_maps();
        let result = run_script(
            "pm.collectionVariables.set('seen', 'yes');",
            &environment,
            &collection_variables,
            None,
            None,
        )
        .unwrap();
        assert_eq!(
            result.collection_variables.get("seen"),
            Some(&"yes".to_string())
        );
    }

    #[test]
    fn pm_test_records_a_passing_and_a_failing_assertion() {
        let (environment, collection_variables) = empty_maps();
        let script = r#"
            pm.test("passes", () => { pm.expect(1 + 1).to.equal(2); });
            pm.test("fails", () => { pm.expect(1 + 1).to.equal(3); });
        "#;
        let result = run_script(script, &environment, &collection_variables, None, None).unwrap();
        assert_eq!(result.test_results.len(), 2);
        assert!(result.test_results[0].passed);
        assert!(result.test_results[0].error.is_none());
        assert!(!result.test_results[1].passed);
        assert!(
            result.test_results[1]
                .error
                .as_deref()
                .unwrap()
                .contains("to equal")
        );
    }

    #[test]
    fn expect_to_be_true_and_false_work_as_getters() {
        let (environment, collection_variables) = empty_maps();
        let script = r#"
            pm.test("true check", () => { pm.expect(true).to.be.true; });
            pm.test("false check", () => { pm.expect(false).to.be.false; });
        "#;
        let result = run_script(script, &environment, &collection_variables, None, None).unwrap();
        assert!(result.test_results[0].passed);
        assert!(result.test_results[1].passed);
    }

    #[test]
    fn a_post_response_script_can_read_the_response_status_and_body() {
        let (environment, collection_variables) = empty_maps();
        let request = ScriptRequestData {
            method: "GET".to_string(),
            url: "https://api.example.com/users".to_string(),
            headers: Vec::new(),
            body: String::new(),
        };
        let response = ScriptResponseData {
            status: 201,
            headers: Vec::new(),
            body: r#"{"id":42}"#.to_string(),
        };
        let script = r#"
            pm.test("status is 201", () => { pm.expect(pm.response.code).to.equal(201); });
            pm.test("body has id", () => { pm.expect(pm.response.json().id).to.equal(42); });
        "#;
        let result = run_test_script(
            script,
            &environment,
            &collection_variables,
            &request,
            &response,
        )
        .unwrap();
        assert!(
            result.test_results.iter().all(|test| test.passed),
            "{:?}",
            result.test_results
        );
    }

    #[test]
    fn a_pre_request_script_sees_the_request_but_not_a_response() {
        let (environment, collection_variables) = empty_maps();
        let request = ScriptRequestData {
            method: "POST".to_string(),
            url: "https://api.example.com/login".to_string(),
            headers: Vec::new(),
            body: String::new(),
        };
        let script = r#"
            pm.test("method is POST", () => { pm.expect(pm.request.method).to.equal("POST"); });
            pm.test("response is undefined", () => { pm.expect(pm.response).to.equal(undefined); });
        "#;
        let result =
            run_pre_request_script(script, &environment, &collection_variables, &request).unwrap();
        assert!(
            result.test_results.iter().all(|test| test.passed),
            "{:?}",
            result.test_results
        );
    }

    #[test]
    fn pm_visualize_captures_a_json_serializable_payload() {
        let (environment, collection_variables) = empty_maps();
        let result = run_script(
            "pm.visualize({ total: 3, items: ['a', 'b', 'c'] });",
            &environment,
            &collection_variables,
            None,
            None,
        )
        .unwrap();
        let data = result.visualize_data.unwrap();
        assert_eq!(data["total"], 3);
        assert_eq!(data["items"][1], "b");
    }

    #[test]
    fn console_log_output_is_captured_rather_than_printed() {
        let (environment, collection_variables) = empty_maps();
        let result = run_script(
            r#"console.log("hello", 42);"#,
            &environment,
            &collection_variables,
            None,
            None,
        )
        .unwrap();
        assert_eq!(result.console_logs, vec!["hello 42".to_string()]);
    }

    #[test]
    fn a_thrown_error_outside_pm_test_is_reported_as_a_script_error_not_a_panic() {
        let (environment, collection_variables) = empty_maps();
        let result = run_script(
            "throw new Error('boom');",
            &environment,
            &collection_variables,
            None,
            None,
        );
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("boom"));
    }

    #[test]
    fn an_empty_pre_request_script_is_rejected_rather_than_silently_running() {
        let (environment, collection_variables) = empty_maps();
        let request = ScriptRequestData::default();
        assert!(
            run_pre_request_script("   ", &environment, &collection_variables, &request).is_err()
        );
    }
}
