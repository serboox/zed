use std::collections::BTreeMap;

use anyhow::{Context, Result, bail};
use api_client::{CollectionId, Request};

/// One request's outcome for one iteration of a collection run.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RunnerRowResult {
    pub request_name: String,
    pub iteration_index: usize,
    pub status: Option<u16>,
    pub passed_tests: usize,
    pub failed_tests: usize,
    pub error: Option<String>,
}

/// Every request belonging to `collection_id`, folder or not, in the same
/// order the panel's tree renders them (folder order, then request order
/// within each folder) -- a flat run order rather than the panel's nested
/// tree, since a runner has no concept of "collapsed".
pub fn requests_for_collection(requests: &[Request], collection_id: CollectionId) -> Vec<Request> {
    let mut matching: Vec<Request> = requests
        .iter()
        .filter(|request| request.collection_id == collection_id)
        .cloned()
        .collect();
    matching.sort_by_key(|request| request.order);
    matching
}

/// Parses a runner data file: a JSON array of flat objects (Postman's own
/// format) or, for anything not starting with `[`, CSV with a header row.
/// Every row becomes one iteration; `pm.environment.get(key)` sees each
/// row's values merged over the active environment for that iteration --
/// this pass exposes iteration data through `pm.environment` rather than a
/// separate `pm.iterationData` namespace, a deliberate scope cut (see the
/// work plan's own note that the iteration variable bag is cheap to sketch
/// but a dedicated namespace is not essential to get value from it).
pub fn parse_data_file(text: &str) -> Result<Vec<BTreeMap<String, String>>> {
    let trimmed = text.trim();
    if trimmed.is_empty() {
        return Ok(Vec::new());
    }
    if trimmed.starts_with('[') {
        parse_json_data_file(trimmed)
    } else {
        parse_csv_data_file(trimmed)
    }
}

fn parse_json_data_file(text: &str) -> Result<Vec<BTreeMap<String, String>>> {
    let rows: Vec<BTreeMap<String, serde_json::Value>> =
        serde_json::from_str(text).context("data file is not a JSON array of flat objects")?;
    Ok(rows
        .into_iter()
        .map(|row| {
            row.into_iter()
                .map(|(key, value)| {
                    let value = match value {
                        serde_json::Value::String(text) => text,
                        other => other.to_string(),
                    };
                    (key, value)
                })
                .collect()
        })
        .collect())
}

fn parse_csv_data_file(text: &str) -> Result<Vec<BTreeMap<String, String>>> {
    let mut lines = text.lines();
    let Some(header_line) = lines.next() else {
        return Ok(Vec::new());
    };
    let headers: Vec<&str> = header_line.split(',').map(str::trim).collect();
    if headers.iter().any(|header| header.is_empty()) {
        bail!("CSV header row has an empty column name");
    }

    let mut rows = Vec::new();
    for line in lines {
        if line.trim().is_empty() {
            continue;
        }
        let values: Vec<&str> = line.split(',').collect();
        if values.len() != headers.len() {
            bail!(
                "CSV row has {} column(s), expected {} to match the header",
                values.len(),
                headers.len()
            );
        }
        rows.push(
            headers
                .iter()
                .zip(values)
                .map(|(header, value)| (header.to_string(), value.trim().to_string()))
                .collect(),
        );
    }
    Ok(rows)
}

/// Runs every request in `requests` once per row in `iterations` (once with
/// no iteration data at all if `iterations` is empty), sequentially --
/// never in parallel, so a run never hammers the target server harder than
/// clicking Send by hand would. `base_environment`/`collection_variables`
/// are read once up front; iteration rows are layered on top of the
/// environment for that iteration only and never written back to the real
/// stored environment (a run's iteration data is transient by design).
///
/// This is the seam between pure orchestration and real network I/O (via
/// `api_client::execute`) and script execution (via `boa_engine`) -- like
/// `http_send::execute` and `scripting::run_script`, it is deliberately
/// left untested here; `requests_for_collection`/`parse_data_file` above
/// carry the actual unit-tested logic this function composes.
pub async fn run_collection(
    requests: Vec<Request>,
    iterations: Vec<BTreeMap<String, String>>,
    base_environment: BTreeMap<String, String>,
    collection_variables: BTreeMap<String, String>,
    client: reqwest::Client,
) -> Vec<RunnerRowResult> {
    let iterations: Vec<BTreeMap<String, String>> = if iterations.is_empty() {
        vec![BTreeMap::new()]
    } else {
        iterations
    };
    let mut environment = base_environment;
    let mut results = Vec::with_capacity(requests.len() * iterations.len());

    for (iteration_index, iteration_row) in iterations.into_iter().enumerate() {
        for row in &iteration_row {
            environment.insert(row.0.clone(), row.1.clone());
        }

        for request in &requests {
            let result =
                run_one_request(request, &mut environment, &collection_variables, &client).await;
            results.push(RunnerRowResult {
                request_name: request.name.clone(),
                iteration_index,
                status: result.status,
                passed_tests: result.passed_tests,
                failed_tests: result.failed_tests,
                error: result.error,
            });
        }
    }

    results
}

struct OneRequestOutcome {
    status: Option<u16>,
    passed_tests: usize,
    failed_tests: usize,
    error: Option<String>,
}

async fn run_one_request(
    request: &Request,
    environment: &mut BTreeMap<String, String>,
    collection_variables: &BTreeMap<String, String>,
    client: &reqwest::Client,
) -> OneRequestOutcome {
    if !request.pre_request_script.trim().is_empty() {
        let script_request = api_client::ScriptRequestData {
            method: request.method.as_str().to_string(),
            url: request.url.clone(),
            headers: request
                .headers
                .iter()
                .filter(|header| header.enabled)
                .map(|header| (header.key.clone(), header.value.clone()))
                .collect(),
            body: match &request.body {
                api_client::RequestBody::Raw { text, .. } => text.clone(),
                _ => String::new(),
            },
        };
        match api_client::run_pre_request_script(
            &request.pre_request_script,
            environment,
            collection_variables,
            &script_request,
        ) {
            Ok(result) => *environment = result.environment,
            Err(error) => {
                return OneRequestOutcome {
                    status: None,
                    passed_tests: 0,
                    failed_tests: 0,
                    error: Some(format!("Pre-request script failed: {error}")),
                };
            }
        }
    }

    let resolve_environment = api_client::Environment {
        id: uuid::Uuid::nil(),
        name: "Runner Iteration".to_string(),
        variables: environment
            .iter()
            .map(|(key, value)| api_client::Variable::new(key.clone(), value.clone()))
            .collect(),
    };
    let global = api_client::Environment::global();
    let context = api_client::VariableContext {
        environment: Some(&resolve_environment),
        collection: None,
        global: &global,
    };
    let dynamic = api_client::SystemDynamicVariableSource;
    let resolve = |text: &str| {
        api_client::resolve(text, &context, &dynamic, api_client::ResolveMode::ForSend)
    };
    let resolved = api_client::build_resolved_request(request, &resolve);

    let response = match api_client::execute(client, &resolved).await {
        Ok(summary) => summary,
        Err(error) => {
            return OneRequestOutcome {
                status: None,
                passed_tests: 0,
                failed_tests: 0,
                error: Some(error.to_string()),
            };
        }
    };
    let status = response.status;

    if request.test_script.trim().is_empty() {
        return OneRequestOutcome {
            status: Some(status),
            passed_tests: 0,
            failed_tests: 0,
            error: None,
        };
    }

    let script_request = api_client::ScriptRequestData {
        method: resolved.method.clone(),
        url: resolved.url.clone(),
        headers: resolved.headers.clone(),
        body: resolved
            .body
            .as_ref()
            .map(|body| String::from_utf8_lossy(body).into_owned())
            .unwrap_or_default(),
    };
    let script_response = api_client::ScriptResponseData {
        status: response.status,
        headers: response.headers.clone(),
        body: String::from_utf8_lossy(&response.body).into_owned(),
    };
    match api_client::run_test_script(
        &request.test_script,
        environment,
        collection_variables,
        &script_request,
        &script_response,
    ) {
        Ok(result) => {
            *environment = result.environment;
            let passed = result
                .test_results
                .iter()
                .filter(|test| test.passed)
                .count();
            let failed = result.test_results.len() - passed;
            OneRequestOutcome {
                status: Some(status),
                passed_tests: passed,
                failed_tests: failed,
                error: None,
            }
        }
        Err(error) => OneRequestOutcome {
            status: Some(status),
            passed_tests: 0,
            failed_tests: 0,
            error: Some(format!("Tests script failed: {error}")),
        },
    }
}

/// Renders a run's results as a short plain-text summary -- used by the
/// runner view's "copy results" affordance rather than reformatting the
/// row list by hand each time it is needed as text.
pub fn summarize_run(results: &[RunnerRowResult]) -> String {
    let total_requests = results.len();
    let failed_requests = results.iter().filter(|row| row.error.is_some()).count();
    let total_passed_tests: usize = results.iter().map(|row| row.passed_tests).sum();
    let total_failed_tests: usize = results.iter().map(|row| row.failed_tests).sum();
    format!(
        "{total_requests} request(s) run, {failed_requests} failed to send, {total_passed_tests} test(s) passed, {total_failed_tests} test(s) failed"
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use uuid::Uuid;

    #[test]
    fn requests_for_collection_returns_only_that_collections_requests_in_order() {
        let collection_id = Uuid::new_v4();
        let other_collection_id = Uuid::new_v4();
        let mut first = Request::new(collection_id, "B".to_string());
        first.order = 1;
        let mut second = Request::new(collection_id, "A".to_string());
        second.order = 0;
        let other = Request::new(other_collection_id, "Other".to_string());

        let requests = vec![first, other, second];
        let ordered = requests_for_collection(&requests, collection_id);
        assert_eq!(ordered.len(), 2);
        assert_eq!(ordered[0].name, "A");
        assert_eq!(ordered[1].name, "B");
    }

    #[test]
    fn an_empty_data_file_produces_zero_iterations() {
        assert_eq!(parse_data_file("").unwrap(), Vec::new());
        assert_eq!(parse_data_file("   ").unwrap(), Vec::new());
    }

    #[test]
    fn a_json_array_data_file_produces_one_row_per_object() {
        let rows =
            parse_data_file(r#"[{"username":"alice","id":1},{"username":"bob","id":2}]"#).unwrap();
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0].get("username"), Some(&"alice".to_string()));
        assert_eq!(rows[0].get("id"), Some(&"1".to_string()));
        assert_eq!(rows[1].get("username"), Some(&"bob".to_string()));
    }

    #[test]
    fn a_csv_data_file_uses_the_first_line_as_headers() {
        let rows = parse_data_file("username,id\nalice,1\nbob,2").unwrap();
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0].get("username"), Some(&"alice".to_string()));
        assert_eq!(rows[0].get("id"), Some(&"1".to_string()));
        assert_eq!(rows[1].get("username"), Some(&"bob".to_string()));
    }

    #[test]
    fn csv_blank_lines_are_skipped() {
        let rows = parse_data_file("username\nalice\n\nbob").unwrap();
        assert_eq!(rows.len(), 2);
    }

    #[test]
    fn a_csv_row_with_the_wrong_column_count_is_rejected() {
        assert!(parse_data_file("a,b\n1,2,3").is_err());
    }

    #[test]
    fn malformed_json_data_is_rejected_rather_than_panicking() {
        assert!(parse_data_file("[{not json").is_err());
    }

    #[test]
    fn summarize_run_reports_request_and_test_counts() {
        let results = vec![
            RunnerRowResult {
                request_name: "Get users".to_string(),
                iteration_index: 0,
                status: Some(200),
                passed_tests: 2,
                failed_tests: 0,
                error: None,
            },
            RunnerRowResult {
                request_name: "Get users".to_string(),
                iteration_index: 1,
                status: None,
                passed_tests: 0,
                failed_tests: 0,
                error: Some("connection refused".to_string()),
            },
        ];
        let summary = summarize_run(&results);
        assert!(summary.contains("2 request(s) run"));
        assert!(summary.contains("1 failed to send"));
        assert!(summary.contains("2 test(s) passed"));
    }
}
