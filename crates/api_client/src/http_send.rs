use std::time::Instant;

use crate::request::{ApiKeyPlacement, AuthConfig, Request, RequestBody};

/// The concrete HTTP request that will be sent, after variable resolution,
/// query-param merging, and auth have all been applied. Pure and
/// network-free so it can be unit-tested without a live server -- only
/// `execute` below touches the network.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResolvedRequest {
    pub method: String,
    pub url: String,
    pub headers: Vec<(String, String)>,
    pub body: Option<Vec<u8>>,
}

/// Applies `resolve` (a `{{token}}` substitution function, typically
/// `variable_resolution::resolve` bound to a `VariableContext`) to every
/// user-facing string on `request`, merges enabled query params into the
/// URL, and layers auth on top as either a header or a query param.
pub fn build_resolved_request(
    request: &Request,
    resolve: &impl Fn(&str) -> String,
) -> ResolvedRequest {
    let mut url = resolve(&request.url);

    let enabled_params: Vec<(String, String)> = request
        .params
        .iter()
        .filter(|param| param.enabled && !param.key.is_empty())
        .map(|param| (resolve(&param.key), resolve(&param.value)))
        .collect();
    append_query_params(&mut url, &enabled_params);

    let mut headers: Vec<(String, String)> = request
        .headers
        .iter()
        .filter(|header| header.enabled && !header.key.is_empty())
        .map(|header| (resolve(&header.key), resolve(&header.value)))
        .collect();

    let body = match &request.body {
        RequestBody::Raw { text, .. } if !text.is_empty() => Some(resolve(text).into_bytes()),
        _ => None,
    };

    match &request.auth {
        AuthConfig::Basic { username, password } => {
            let credentials = format!("{}:{}", resolve(username), resolve(password));
            let encoded = base64::Engine::encode(
                &base64::engine::general_purpose::STANDARD,
                credentials.as_bytes(),
            );
            headers.push(("Authorization".to_string(), format!("Basic {encoded}")));
        }
        AuthConfig::Bearer { token } => {
            headers.push((
                "Authorization".to_string(),
                format!("Bearer {}", resolve(token)),
            ));
        }
        AuthConfig::ApiKey {
            key,
            value,
            placement,
        } => {
            let resolved_key = resolve(key);
            let resolved_value = resolve(value);
            match placement {
                ApiKeyPlacement::Header => headers.push((resolved_key, resolved_value)),
                ApiKeyPlacement::Query => {
                    append_query_params(&mut url, &[(resolved_key, resolved_value)])
                }
            }
        }
        AuthConfig::OAuth2(oauth2) => {
            if !oauth2.access_token.is_empty() {
                headers.push((
                    "Authorization".to_string(),
                    format!("Bearer {}", oauth2.access_token),
                ));
            }
        }
        AuthConfig::AwsSigV4(config) => {
            sign_with_aws_sigv4(
                &mut headers,
                &url,
                request.method.as_str(),
                body.as_deref().unwrap_or(&[]),
                config,
            );
        }
        AuthConfig::Jwt(config) => {
            if let Some(token) = crate::jwt::sign_jwt(config) {
                if config.add_to_query_param {
                    let key = if config.query_param_key.is_empty() {
                        "token".to_string()
                    } else {
                        resolve(&config.query_param_key)
                    };
                    append_query_params(&mut url, &[(key, token)]);
                } else {
                    let prefix = if config.header_prefix.is_empty() {
                        "Bearer".to_string()
                    } else {
                        config.header_prefix.clone()
                    };
                    headers.push(("Authorization".to_string(), format!("{prefix} {token}")));
                }
            }
        }
        AuthConfig::Inherit | AuthConfig::None => {}
    }

    ResolvedRequest {
        method: request.method.as_str().to_string(),
        url,
        headers,
        body,
    }
}

/// Adds a `Host` header (required for SigV4 signing) if not already present,
/// signs the request, and appends the `Authorization`/`X-Amz-Date`/
/// `X-Amz-Security-Token` headers SigV4 requires alongside it. Parse
/// failures on `url` are treated as "nothing to sign" rather than a panic --
/// an unparsable URL will already fail to send once `execute` tries it.
fn sign_with_aws_sigv4(
    headers: &mut Vec<(String, String)>,
    url: &str,
    method: &str,
    body: &[u8],
    config: &crate::aws_sigv4::AwsSigV4Config,
) {
    let Ok(parsed_url) = reqwest::Url::parse(url) else {
        return;
    };
    let Some(host) = parsed_url.host_str() else {
        return;
    };

    if !headers
        .iter()
        .any(|(key, _)| key.eq_ignore_ascii_case("host"))
    {
        headers.push(("Host".to_string(), host.to_string()));
    }

    let query_params: Vec<(String, String)> = parsed_url
        .query_pairs()
        .map(|(key, value)| (key.into_owned(), value.into_owned()))
        .collect();

    let unix_seconds = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|duration| duration.as_secs())
        .unwrap_or(0);
    let amz_date = crate::aws_sigv4::format_amz_date(unix_seconds);

    let signed = crate::aws_sigv4::sign_request(
        config,
        method,
        parsed_url.path(),
        &query_params,
        headers,
        body,
        &amz_date,
    );

    headers.push(("Authorization".to_string(), signed.authorization_header));
    headers.push(("X-Amz-Date".to_string(), signed.amz_date));
    if let Some(security_token) = signed.security_token_header {
        headers.push(("X-Amz-Security-Token".to_string(), security_token));
    }
}

fn append_query_params(url: &mut String, params: &[(String, String)]) {
    for (key, value) in params {
        let separator = if url.contains('?') { '&' } else { '?' };
        url.push(separator);
        url.push_str(&urlencoding::encode(key));
        url.push('=');
        url.push_str(&urlencoding::encode(value));
    }
}

/// The status/headers/body/timing of a completed HTTP exchange, independent
/// of `reqwest`'s own response type so the UI layer and tests never need to
/// hold a live `reqwest::Response` (which cannot be constructed by hand).
#[derive(Debug, Clone)]
pub struct HttpResponseSummary {
    pub status: u16,
    pub status_text: String,
    pub headers: Vec<(String, String)>,
    pub body: Vec<u8>,
    pub elapsed_ms: u64,
}

/// Sends `resolved` and awaits the full response. Runs on the shared
/// network Tokio runtime -- see `network_runtime` for why that's necessary
/// (`reqwest`'s DNS resolver panics with "there is no reactor running" when
/// driven directly from GPUI's own executor).
pub async fn execute(
    client: &reqwest::Client,
    resolved: &ResolvedRequest,
) -> anyhow::Result<HttpResponseSummary> {
    let method = reqwest::Method::from_bytes(resolved.method.as_bytes())?;
    let mut builder = client.request(method, &resolved.url);
    for (key, value) in &resolved.headers {
        builder = builder.header(key, value);
    }
    if let Some(body) = resolved.body.clone() {
        builder = builder.body(body);
    }

    let started = Instant::now();
    let (status, status_text, headers, body) =
        crate::network_runtime::on_network_runtime(async move {
            let response = builder.send().await?;
            let status = response.status();
            let status_text = status.canonical_reason().unwrap_or("").to_string();
            let headers = response
                .headers()
                .iter()
                .map(|(name, value)| (name.to_string(), value.to_str().unwrap_or("").to_string()))
                .collect::<Vec<_>>();
            let body = response.bytes().await?.to_vec();
            anyhow::Ok((status.as_u16(), status_text, headers, body))
        })
        .await?;
    let elapsed_ms = started.elapsed().as_millis() as u64;

    Ok(HttpResponseSummary {
        status,
        status_text,
        headers,
        body,
        elapsed_ms,
    })
}

/// One `Set-Cookie` response header, parsed into its name/value plus the
/// remaining `key=value; ...` attribute string verbatim (Path, Domain,
/// Expires, etc.) -- Phase 1 only needs to display these, not enforce
/// cookie-jar semantics across requests.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParsedCookie {
    pub name: String,
    pub value: String,
    pub attributes: String,
}

/// Parses every `Set-Cookie` header in `headers` (header names are matched
/// case-insensitively, per RFC 7230). A malformed cookie (no `=`) is skipped
/// rather than erroring, since a broken cookie must never take down the
/// whole response view.
pub fn parse_set_cookie_headers(headers: &[(String, String)]) -> Vec<ParsedCookie> {
    headers
        .iter()
        .filter(|(name, _)| name.eq_ignore_ascii_case("set-cookie"))
        .filter_map(|(_, value)| {
            let mut parts = value.split(';');
            let name_value = parts.next()?.trim();
            let (name, cookie_value) = name_value.split_once('=')?;
            let attributes = parts.collect::<Vec<_>>().join(";").trim().to_string();
            Some(ParsedCookie {
                name: name.trim().to_string(),
                value: cookie_value.trim().to_string(),
                attributes,
            })
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::request::{ApiKeyPlacement, Header, HttpMethod, QueryParam};
    use uuid::Uuid;

    fn identity(text: &str) -> String {
        text.to_string()
    }

    fn base_request() -> Request {
        Request::new(Uuid::new_v4(), "Test".to_string())
    }

    #[test]
    fn enabled_query_params_are_appended_and_url_encoded() {
        let mut request = base_request();
        request.url = "https://api.example.com/users".to_string();
        request.params = vec![
            QueryParam {
                key: "q".into(),
                value: "a b".into(),
                enabled: true,
                description: None,
            },
            QueryParam {
                key: "disabled".into(),
                value: "x".into(),
                enabled: false,
                description: None,
            },
        ];
        let resolved = build_resolved_request(&request, &identity);
        assert_eq!(resolved.url, "https://api.example.com/users?q=a%20b");
    }

    #[test]
    fn a_variable_token_in_the_url_is_resolved_before_sending() {
        let mut request = base_request();
        request.url = "{{base_url}}/users".to_string();
        let resolve = |text: &str| text.replace("{{base_url}}", "https://staging.example.com");
        let resolved = build_resolved_request(&request, &resolve);
        assert_eq!(resolved.url, "https://staging.example.com/users");
    }

    #[test]
    fn enabled_headers_are_included_and_disabled_ones_are_not() {
        let mut request = base_request();
        request.headers = vec![
            Header {
                key: "Accept".into(),
                value: "application/json".into(),
                enabled: true,
                description: None,
            },
            Header {
                key: "X-Skip".into(),
                value: "nope".into(),
                enabled: false,
                description: None,
            },
        ];
        let resolved = build_resolved_request(&request, &identity);
        assert_eq!(
            resolved.headers,
            vec![("Accept".to_string(), "application/json".to_string())]
        );
    }

    #[test]
    fn basic_auth_adds_a_base64_authorization_header() {
        let mut request = base_request();
        request.auth = AuthConfig::Basic {
            username: "alice".into(),
            password: "secret".into(),
        };
        let resolved = build_resolved_request(&request, &identity);
        assert_eq!(
            resolved.headers,
            vec![(
                "Authorization".to_string(),
                "Basic YWxpY2U6c2VjcmV0".to_string()
            )]
        );
    }

    #[test]
    fn bearer_auth_adds_a_bearer_authorization_header() {
        let mut request = base_request();
        request.auth = AuthConfig::Bearer {
            token: "tok123".into(),
        };
        let resolved = build_resolved_request(&request, &identity);
        assert_eq!(
            resolved.headers,
            vec![("Authorization".to_string(), "Bearer tok123".to_string())]
        );
    }

    #[test]
    fn api_key_in_query_placement_appends_to_the_url_instead_of_headers() {
        let mut request = base_request();
        request.url = "https://api.example.com/data".to_string();
        request.auth = AuthConfig::ApiKey {
            key: "api_key".into(),
            value: "xyz".into(),
            placement: ApiKeyPlacement::Query,
        };
        let resolved = build_resolved_request(&request, &identity);
        assert!(resolved.headers.is_empty());
        assert_eq!(resolved.url, "https://api.example.com/data?api_key=xyz");
    }

    #[test]
    fn inherit_and_none_auth_add_no_headers() {
        let request = base_request();
        let resolved = build_resolved_request(&request, &identity);
        assert!(resolved.headers.is_empty());
    }

    #[test]
    fn raw_body_text_is_resolved_and_carried_as_bytes() {
        let mut request = base_request();
        request.method = HttpMethod::Post;
        request.body = RequestBody::Raw {
            content_type: crate::request::RawBodyContentType::Json,
            text: r#"{"name":"{{name}}"}"#.to_string(),
        };
        let resolve = |text: &str| text.replace("{{name}}", "Alice");
        let resolved = build_resolved_request(&request, &resolve);
        assert_eq!(resolved.body, Some(br#"{"name":"Alice"}"#.to_vec()));
        assert_eq!(resolved.method, "POST");
    }

    #[test]
    fn no_body_produces_no_body_bytes() {
        let request = base_request();
        let resolved = build_resolved_request(&request, &identity);
        assert_eq!(resolved.body, None);
    }

    #[test]
    fn set_cookie_headers_are_parsed_into_name_value_and_attributes() {
        let headers = vec![
            ("Content-Type".to_string(), "application/json".to_string()),
            (
                "Set-Cookie".to_string(),
                "session_id=abc123; Path=/; HttpOnly; Secure".to_string(),
            ),
            ("set-cookie".to_string(), "theme=dark".to_string()),
        ];
        let cookies = parse_set_cookie_headers(&headers);
        assert_eq!(cookies.len(), 2);
        assert_eq!(cookies[0].name, "session_id");
        assert_eq!(cookies[0].value, "abc123");
        assert_eq!(cookies[0].attributes, "Path=/; HttpOnly; Secure");
        assert_eq!(cookies[1].name, "theme");
        assert_eq!(cookies[1].value, "dark");
        assert_eq!(cookies[1].attributes, "");
    }

    #[test]
    fn a_malformed_cookie_without_an_equals_sign_is_skipped() {
        let headers = vec![("Set-Cookie".to_string(), "not-a-valid-cookie".to_string())];
        assert!(parse_set_cookie_headers(&headers).is_empty());
    }
}
