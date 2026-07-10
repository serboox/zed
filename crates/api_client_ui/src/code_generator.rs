use api_client::{
    Request, ResolveMode, SystemDynamicVariableSource, VariableContext, build_resolved_request,
    resolve,
};

/// Generates a `curl` command line equivalent to sending `request`, after
/// resolving `{{variable}}` tokens through `context`. Pure string
/// templating -- no UI dependency -- so it is trivially unit-testable and
/// reusable for a future "Copy as cURL" action anywhere in the UI.
pub fn generate_curl(request: &Request, context: &VariableContext) -> String {
    let dynamic = SystemDynamicVariableSource;
    let resolver = |text: &str| resolve(text, context, &dynamic, ResolveMode::ForSend);
    let resolved = build_resolved_request(request, &resolver);

    let mut command = format!(
        "curl --request {} {}",
        resolved.method,
        shell_quote(&resolved.url)
    );
    for (key, value) in &resolved.headers {
        command.push_str(&format!(
            " \\\n  --header {}",
            shell_quote(&format!("{key}: {value}"))
        ));
    }
    if let Some(body) = &resolved.body {
        let body_text = String::from_utf8_lossy(body);
        command.push_str(&format!(" \\\n  --data {}", shell_quote(&body_text)));
    }
    command
}

/// Wraps `value` in single quotes, escaping any embedded single quote as
/// `'\''` (the standard POSIX-shell trick: close the quote, emit an escaped
/// quote, reopen the quote) -- so the generated command is always safe to
/// paste into a shell verbatim, no matter what characters a header/body/URL
/// contains.
fn shell_quote(value: &str) -> String {
    format!("'{}'", value.replace('\'', r"'\''"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use api_client::{Environment, Header, HttpMethod, RawBodyContentType, RequestBody};
    use uuid::Uuid;

    /// Every auto-generated default header disabled, so a `generate_curl`
    /// assertion only has to reason about the headers the test itself set --
    /// auto-header inclusion is covered by `http_send`'s own tests.
    fn request_with_auto_headers_disabled(name: &str) -> Request {
        let mut request = Request::new(Uuid::new_v4(), name.to_string());
        request.settings.disabled_auto_headers = api_client::AUTO_HEADER_DEFAULTS
            .iter()
            .map(|(key, _)| key.to_string())
            .collect();
        request
    }

    #[test]
    fn a_simple_get_request_becomes_a_one_line_curl_command() {
        let mut request = request_with_auto_headers_disabled("Ping");
        request.url = "https://api.example.com/ping".to_string();
        let global = Environment::global();
        let ctx = VariableContext {
            environment: None,
            collection: None,
            global: &global,
        };
        let curl = generate_curl(&request, &ctx);
        assert_eq!(curl, "curl --request GET 'https://api.example.com/ping'");
    }

    #[test]
    fn headers_are_appended_as_separate_flags() {
        let mut request = request_with_auto_headers_disabled("Ping");
        request.url = "https://api.example.com/ping".to_string();
        request.headers = vec![Header {
            key: "Accept".to_string(),
            value: "application/json".to_string(),
            enabled: true,
            description: None,
        }];
        let global = Environment::global();
        let ctx = VariableContext {
            environment: None,
            collection: None,
            global: &global,
        };
        let curl = generate_curl(&request, &ctx);
        assert!(curl.contains("--header 'Accept: application/json'"));
    }

    #[test]
    fn a_body_with_an_embedded_single_quote_is_escaped_safely() {
        let mut request = Request::new(Uuid::new_v4(), "Create".to_string());
        request.method = HttpMethod::Post;
        request.url = "https://api.example.com/items".to_string();
        request.body = RequestBody::Raw {
            content_type: RawBodyContentType::Json,
            text: r#"{"name":"O'Brien"}"#.to_string(),
        };
        let global = Environment::global();
        let ctx = VariableContext {
            environment: None,
            collection: None,
            global: &global,
        };
        let curl = generate_curl(&request, &ctx);
        assert!(curl.contains(r#"--data '{"name":"O'\''Brien"}'"#));
    }

    #[test]
    fn variable_tokens_are_resolved_before_generating_the_command() {
        let mut request = Request::new(Uuid::new_v4(), "Ping".to_string());
        request.url = "{{base_url}}/ping".to_string();
        let mut global = Environment::global();
        global.variables.push(api_client::Variable::new(
            "base_url".into(),
            "https://staging.example.com".into(),
        ));
        let ctx = VariableContext {
            environment: None,
            collection: None,
            global: &global,
        };
        let curl = generate_curl(&request, &ctx);
        assert!(curl.contains("https://staging.example.com/ping"));
    }
}
