use anyhow::{Context as _, Result, anyhow};
use api_client::{
    Environment, EnvironmentId, HistoryEntry, HttpResponseSummary, Request, RequestId, ResolveMode,
    SystemDynamicVariableSource, TestResult, Variable, VariableContext,
};
use gpui::{AsyncApp, Entity};

use std::collections::BTreeMap;
use std::time::Duration;

use crate::request_view::{apply_script_variable_changes, script_request_data, variable_maps_for};
use crate::response_view::ResponseData;
use crate::store::{ApiClientStore, HistoryExchangeDetail, HistoryExchangeOutcome};

/// A saved request to send without a window, the way `zedcli api send` does.
pub struct HeadlessSend {
    pub request: RequestId,
    /// The environment to resolve against instead of the request's own choice.
    pub environment: Option<EnvironmentId>,
    /// Values that win over every environment, collection and global variable
    /// for this one send, and are never written back.
    pub variables: Vec<(String, String)>,
    /// How long the server has to answer before the send is given up.
    pub timeout: Duration,
}

pub struct HeadlessOutcome {
    pub method: String,
    /// The URL that went out, with every secret variable's value and any
    /// password in it masked.
    pub url: String,
    pub environment_name: Option<String>,
    pub response: HttpResponseSummary,
    /// What the request's test script reported; empty when it has none.
    pub tests: Vec<TestResult>,
}

/// Sends a saved request exactly as the request view's Send does: its
/// pre-request script first, its variables resolved, the response through its
/// test script, and the exchange recorded in the history.
pub async fn send(
    store: &Entity<ApiClientStore>,
    send: HeadlessSend,
    cx: &mut AsyncApp,
) -> Result<HeadlessOutcome> {
    let request = store
        .read_with(cx, |store, _| {
            store
                .requests
                .iter()
                .find(|request| request.id == send.request)
                .cloned()
        })
        .ok_or_else(|| anyhow!("the request no longer exists"))?;

    if !request.pre_request_script.trim().is_empty() {
        let (before_environment, before_collection) =
            store.read_with(cx, |store, _| script_maps(store, &request, &send));
        let script_request = script_request_data(&request, None);
        let script = request.pre_request_script.clone();
        let environment = before_environment.clone();
        let collection = before_collection.clone();
        let result = cx
            .background_executor()
            .spawn(async move {
                api_client::run_pre_request_script(
                    &script,
                    &environment,
                    &collection,
                    &script_request,
                )
            })
            .await
            .context("the pre-request script failed")?;
        store.update(cx, |store, cx| {
            write_back(
                store,
                &request,
                &send,
                ScriptChanges {
                    before_environment: &before_environment,
                    after_environment: &result.environment,
                    before_collection: &before_collection,
                    after_collection: &result.collection_variables,
                },
                cx,
            );
        });
    }

    let files =
        api_client::FilesForABody::read_them(api_client::files_a_body_needs(&request.body)).await;
    if let Some((path, why)) = files.unreadable().first() {
        return Err(anyhow!("could not read {}: {why}", path.display()));
    }

    let (resolved, environment_name, client, secrets) = store.read_with(cx, |store, _| {
        let secrets = secret_values(store, &request, &send);
        let environment = environment_for(store, &request, &send);
        let context = VariableContext {
            environment: Some(&environment),
            collection: store
                .collections
                .iter()
                .find(|collection| collection.id == request.collection_id),
            global: &store.global_environment,
        };
        let dynamic = SystemDynamicVariableSource;
        let resolve =
            |text: &str| api_client::resolve(text, &context, &dynamic, ResolveMode::ForSend);
        let resolved = api_client::build_resolved_request_with_files(&request, &resolve, &files);
        let environment_name = (!environment.name.is_empty()).then(|| environment.name.clone());
        (
            resolved,
            environment_name,
            store.http_client.clone(),
            secrets,
        )
    });
    let shown_url = masked(&resolved.url, &secrets);

    let sent_at_unix_ms = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|duration| duration.as_millis() as u64)
        .unwrap_or(0);
    let timer = cx.background_executor().timer(send.timeout);
    let result = smol::future::or(
        async { Some(api_client::execute(&client, &resolved).await) },
        async {
            timer.await;
            None
        },
    )
    .await
    .unwrap_or_else(|| {
        Err(anyhow!(
            "no answer within {}s",
            send.timeout.as_secs().max(1)
        ))
    });
    let summary = match result {
        Ok(summary) => summary,
        Err(error) => {
            let message = format!("{error:#}");
            store.update(cx, |store, cx| {
                let entry = HistoryEntry::new(
                    send.request,
                    resolved.method.clone(),
                    resolved.url.clone(),
                    None,
                    sent_at_unix_ms,
                );
                store.record_history_detail(
                    entry.id,
                    HistoryExchangeDetail {
                        request: resolved,
                        outcome: HistoryExchangeOutcome::Error(message.clone()),
                        environment_name,
                    },
                );
                store.record_history_entry(entry, cx);
            });
            return Err(anyhow!(message));
        }
    };

    let mut tests = Vec::new();
    if !request.test_script.trim().is_empty() {
        let (before_environment, before_collection) =
            store.read_with(cx, |store, _| script_maps(store, &request, &send));
        let script_request = script_request_data(&request, Some(&resolved));
        let script_response = api_client::ScriptResponseData {
            status: summary.status,
            headers: summary.headers.clone(),
            body: String::from_utf8_lossy(&summary.body).into_owned(),
        };
        let script = request.test_script.clone();
        let environment = before_environment.clone();
        let collection = before_collection.clone();
        let result = cx
            .background_executor()
            .spawn(async move {
                api_client::run_test_script(
                    &script,
                    &environment,
                    &collection,
                    &script_request,
                    &script_response,
                )
            })
            .await;
        match result {
            Ok(result) => {
                store.update(cx, |store, cx| {
                    write_back(
                        store,
                        &request,
                        &send,
                        ScriptChanges {
                            before_environment: &before_environment,
                            after_environment: &result.environment,
                            before_collection: &before_collection,
                            after_collection: &result.collection_variables,
                        },
                        cx,
                    );
                });
                tests = result.test_results;
            }
            Err(error) => tests.push(TestResult {
                name: "Tests script".to_string(),
                passed: false,
                error: Some(error.to_string()),
            }),
        }
    }

    let outcome = HeadlessOutcome {
        method: resolved.method.clone(),
        url: shown_url,
        environment_name: environment_name.clone(),
        response: summary.clone(),
        tests,
    };
    store.update(cx, |store, cx| {
        let entry = HistoryEntry::new(
            send.request,
            resolved.method.clone(),
            resolved.url.clone(),
            Some(summary.status),
            sent_at_unix_ms,
        );
        store.record_history_detail(
            entry.id,
            HistoryExchangeDetail {
                request: resolved,
                outcome: HistoryExchangeOutcome::Success(ResponseData::from_summary(summary)),
                environment_name,
            },
        );
        store.record_history_entry(entry, cx);
    });
    Ok(outcome)
}

/// What a script sees: the environment this send resolves against, one-off
/// values included, and the request's collection variables.
fn script_maps(
    store: &ApiClientStore,
    request: &Request,
    send: &HeadlessSend,
) -> (BTreeMap<String, String>, BTreeMap<String, String>) {
    let environment = environment_for(store, request, send)
        .variables
        .iter()
        .filter(|variable| variable.enabled)
        .map(|variable| (variable.key.clone(), variable.value_for_send().to_string()))
        .collect();
    let (_, collection) = variable_maps_for(store, request);
    (environment, collection)
}

struct ScriptChanges<'a> {
    before_environment: &'a BTreeMap<String, String>,
    after_environment: &'a BTreeMap<String, String>,
    before_collection: &'a BTreeMap<String, String>,
    after_collection: &'a BTreeMap<String, String>,
}

/// Keeps what a script changed, in the environment this send resolved against
/// rather than whichever one the request would pick by itself. The one-off
/// `--var` values are the caller's, not the project's, and are never kept.
fn write_back(
    store: &mut ApiClientStore,
    request: &Request,
    send: &HeadlessSend,
    changes: ScriptChanges,
    cx: &mut gpui::Context<ApiClientStore>,
) {
    let one_off = |key: &str| send.variables.iter().any(|(one, _)| one == key);
    let changed: Vec<(String, String)> = changes
        .after_environment
        .iter()
        .filter(|(key, value)| {
            !one_off(key) && changes.before_environment.get(*key) != Some(*value)
        })
        .map(|(key, value)| (key.clone(), value.clone()))
        .collect();
    let target = send.environment.or_else(|| {
        store
            .effective_environment_for(request)
            .map(|environment| environment.id)
    });
    if let Some(target) = target
        && !changed.is_empty()
    {
        store.update_environment(Some(target), cx, |environment| {
            for (key, value) in &changed {
                match environment
                    .variables
                    .iter_mut()
                    .find(|variable| &variable.key == key)
                {
                    Some(variable) => variable.current_value = value.clone(),
                    None => environment
                        .variables
                        .push(Variable::new(key.clone(), value.clone())),
                }
            }
        });
    }
    // The collection's part is the request view's own; the environment is
    // passed unchanged so that part is left alone.
    apply_script_variable_changes(
        store,
        request,
        changes.before_environment,
        changes.before_environment,
        changes.before_collection,
        changes.after_collection,
        cx,
    );
}

/// The values of every secret variable this send could resolve, longest
/// first, so a secret that contains another is masked whole.
fn secret_values(store: &ApiClientStore, request: &Request, send: &HeadlessSend) -> Vec<String> {
    let base = match send.environment {
        Some(id) => store.environment_by_id(id),
        None => store.effective_environment_for(request),
    };
    let collection = store
        .collections
        .iter()
        .find(|collection| collection.id == request.collection_id);
    let mut secrets: Vec<String> = base
        .into_iter()
        .flat_map(|environment| environment.variables.iter())
        .chain(
            collection
                .into_iter()
                .flat_map(|collection| collection.variables.iter()),
        )
        .chain(store.global_environment.variables.iter())
        .filter(|variable| variable.secret)
        .map(|variable| variable.value_for_send().to_string())
        .filter(|value| !value.is_empty())
        .collect();
    secrets.sort_by_key(|value| std::cmp::Reverse(value.len()));
    secrets
}

const MASK: &str = "••••";

/// `url` as it may be shown: every secret value replaced, and the password of
/// any `user:password@` in it.
fn masked(url: &str, secrets: &[String]) -> String {
    let mut shown = url.to_string();
    for secret in secrets {
        shown = shown.replace(secret.as_str(), MASK);
    }
    if let Some(scheme_end) = shown.find("://") {
        let authority_start = scheme_end + 3;
        let authority_end = shown[authority_start..]
            .find(['/', '?', '#'])
            .map_or(shown.len(), |at| authority_start + at);
        if let Some(at) = shown[authority_start..authority_end].rfind('@') {
            let userinfo = &shown[authority_start..authority_start + at];
            if let Some(colon) = userinfo.find(':') {
                let password_start = authority_start + colon + 1;
                let password_end = authority_start + at;
                shown.replace_range(password_start..password_end, MASK);
            }
        }
    }
    shown
}

/// The environment this send resolves against: the one asked for, or the
/// request's own, with the one-off values laid over it. A request with no
/// environment at all gets an unnamed one holding just those values.
fn environment_for(store: &ApiClientStore, request: &Request, send: &HeadlessSend) -> Environment {
    let base = match send.environment {
        Some(id) => store.environment_by_id(id),
        None => store.effective_environment_for(request),
    };
    let mut environment = base.cloned().unwrap_or_else(|| Environment {
        id: EnvironmentId::nil(),
        name: String::new(),
        variables: Vec::new(),
    });
    for (key, value) in &send.variables {
        match environment
            .variables
            .iter_mut()
            .find(|variable| &variable.key == key)
        {
            Some(variable) => {
                variable.current_value = value.clone();
                variable.enabled = true;
            }
            None => environment.variables.push(Variable {
                key: key.clone(),
                initial_value: value.clone(),
                current_value: value.clone(),
                secret: false,
                enabled: true,
            }),
        }
    }
    environment
}

#[cfg(test)]
mod tests {
    use super::*;
    use gpui::{AppContext as _, TestAppContext};

    #[test]
    fn a_url_is_shown_without_its_secrets_or_password() {
        let secrets = vec!["s3cr3t-token".to_string(), "k3y".to_string()];
        assert_eq!(
            masked(
                "https://alice:hunter2@api.example.com/v1?key=k3y&token=s3cr3t-token",
                &secrets
            ),
            "https://alice:••••@api.example.com/v1?key=••••&token=••••"
        );
        assert_eq!(
            masked("https://api.example.com/users/a@b.c", &[]),
            "https://api.example.com/users/a@b.c",
            "an @ in the path is not a password"
        );
        assert_eq!(
            masked("https://bob@api.example.com/", &[]),
            "https://bob@api.example.com/",
            "a user without a password is left as it is"
        );
    }

    #[gpui::test]
    fn a_one_off_value_wins_over_the_environment_and_is_not_kept(cx: &mut TestAppContext) {
        let store = cx.new(ApiClientStore::new);
        store.update(cx, |store, cx| {
            let environment = store.create_environment("staging".into(), cx);
            if let Some(stored) = store
                .environments
                .iter_mut()
                .find(|stored| stored.id == environment)
            {
                stored.variables.push(Variable {
                    key: "host".into(),
                    initial_value: "staging.example.com".into(),
                    current_value: "staging.example.com".into(),
                    secret: false,
                    enabled: true,
                });
            }
            let collection = store.create_collection("Shop".into(), cx);
            let request = store.create_request(collection, "Orders".into(), None, cx);
            let request = store
                .requests
                .iter()
                .find(|candidate| candidate.id == request)
                .cloned()
                .expect("the request exists");
            let send = HeadlessSend {
                request: request.id,
                environment: Some(environment),
                variables: vec![
                    ("host".into(), "localhost".into()),
                    ("token".into(), "abc".into()),
                ],
                timeout: Duration::from_secs(30),
            };
            let resolved = environment_for(store, &request, &send);
            let value = |key: &str| {
                resolved
                    .variables
                    .iter()
                    .find(|variable| variable.key == key)
                    .map(|variable| variable.current_value.clone())
            };
            assert_eq!(value("host").as_deref(), Some("localhost"));
            assert_eq!(value("token").as_deref(), Some("abc"));
            assert_eq!(resolved.name, "staging");
            let kept = store
                .environment_by_id(environment)
                .and_then(|stored| stored.variables.first())
                .map(|variable| variable.current_value.clone());
            assert_eq!(
                kept.as_deref(),
                Some("staging.example.com"),
                "the stored environment is left as it was"
            );
        });
    }

    /// A script's changes land in the environment the send was asked to use,
    /// and the caller's one-off values are never saved into it.
    #[gpui::test]
    fn a_script_writes_into_the_chosen_environment_and_not_the_one_off_values(
        cx: &mut TestAppContext,
    ) {
        let store = cx.new(ApiClientStore::new);
        store.update(cx, |store, cx| {
            let pinned = store.create_environment("production".into(), cx);
            let chosen = store.create_environment("staging".into(), cx);
            let collection = store.create_collection("Shop".into(), cx);
            let id = store.create_request(collection, "Orders".into(), None, cx);
            store.choose_request_environment(id, Some(pinned), cx);
            let request = store
                .requests
                .iter()
                .find(|candidate| candidate.id == id)
                .cloned()
                .expect("the request exists");
            let send = HeadlessSend {
                request: id,
                environment: Some(chosen),
                variables: vec![("id".into(), "42".into())],
                timeout: Duration::from_secs(30),
            };
            let (before, collection_before) = script_maps(store, &request, &send);
            assert_eq!(
                before.get("id").map(String::as_str),
                Some("42"),
                "the script sees the one-off value"
            );
            let mut after = before.clone();
            after.insert("id".into(), "43".into());
            after.insert("session".into(), "abc".into());
            write_back(
                store,
                &request,
                &send,
                ScriptChanges {
                    before_environment: &before,
                    after_environment: &after,
                    before_collection: &collection_before,
                    after_collection: &collection_before,
                },
                cx,
            );
            let keys_of = |id| {
                store
                    .environment_by_id(id)
                    .map(|environment| {
                        environment
                            .variables
                            .iter()
                            .map(|variable| variable.key.clone())
                            .collect::<Vec<_>>()
                    })
                    .unwrap_or_default()
            };
            assert_eq!(keys_of(chosen), vec!["session".to_string()]);
            assert!(
                keys_of(pinned).is_empty(),
                "the pinned environment is untouched"
            );
        });
    }
}
