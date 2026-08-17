use api_client::{
    Request, ResolveMode, ResolvedRequest, SystemDynamicVariableSource, VariableContext,
    build_resolved_request, resolve,
};

/// The shapes a request can be copied in: a shell command, the raw exchange, or
/// working code in one of the languages this editor is used for.
///
/// Every one is built from `build_resolved_request`, the same call Send makes, so
/// a snippet cannot drift from what the editor would actually send: the same
/// variables resolved, the same auth, the same headers the reader set.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Snippet {
    Curl,
    HttpText,
    Go,
    Python,
    JavaScript,
    NodeAxios,
    Rust,
    Php,
    CSharp,
    Java,
    Ruby,
    Wget,
}

impl Snippet {
    /// Every shape, in the order the picker offers them.
    pub const ALL: [Snippet; 12] = [
        Snippet::Curl,
        Snippet::HttpText,
        Snippet::Go,
        Snippet::Python,
        Snippet::JavaScript,
        Snippet::NodeAxios,
        Snippet::Rust,
        Snippet::Php,
        Snippet::CSharp,
        Snippet::Java,
        Snippet::Ruby,
        Snippet::Wget,
    ];

    pub fn label(self) -> &'static str {
        match self {
            Snippet::Curl => "cURL",
            Snippet::HttpText => "HTTP",
            Snippet::Go => "Go - net/http",
            Snippet::Python => "Python - requests",
            Snippet::JavaScript => "JavaScript - fetch",
            Snippet::NodeAxios => "Node - axios",
            Snippet::Rust => "Rust - reqwest",
            Snippet::Php => "PHP - cURL",
            Snippet::CSharp => "C# - HttpClient",
            Snippet::Java => "Java - OkHttp",
            Snippet::Ruby => "Ruby - net/http",
            Snippet::Wget => "Shell - wget",
        }
    }
}

/// The request in the shape asked for.
pub fn generate(snippet: Snippet, request: &Request, context: &VariableContext) -> String {
    let resolved = as_it_was_written(request, context);
    let body = resolved
        .body
        .as_ref()
        .map(|body| String::from_utf8_lossy(body).to_string());
    let body = body.as_deref();

    match snippet {
        Snippet::Curl => as_curl(&resolved, body),
        Snippet::HttpText => as_http_text(&resolved, body),
        Snippet::Go => as_go(&resolved, body),
        Snippet::Python => as_python(&resolved, body),
        Snippet::JavaScript => as_javascript(&resolved, body),
        Snippet::NodeAxios => as_node_axios(&resolved, body),
        Snippet::Rust => as_rust(&resolved, body),
        Snippet::Php => as_php(&resolved, body),
        Snippet::CSharp => as_csharp(&resolved, body),
        Snippet::Java => as_java(&resolved, body),
        Snippet::Ruby => as_ruby(&resolved, body),
        Snippet::Wget => as_wget(&resolved, body),
    }
}

/// The request as the reader wrote it, with the variables resolved and the auth
/// applied -- but without the headers this client adds for itself.
///
/// A snippet is for somebody else's tool, and that tool sends its own
/// `User-Agent`, `Accept` and the rest. Carrying ours into it says the request
/// needs headers it does not need, which is how a reader ends up copying
/// `ZedApiClient/1.0` into a bug report. A header the reader set by hand is kept
/// even when it happens to have the same name as one of ours.
fn as_it_was_written(request: &Request, context: &VariableContext) -> ResolvedRequest {
    let dynamic = SystemDynamicVariableSource;
    let resolver = |text: &str| resolve(text, context, &dynamic, ResolveMode::ForSend);
    let mut resolved = build_resolved_request(request, &resolver);
    let written_by_hand: Vec<String> = request
        .headers
        .iter()
        .filter(|header| header.enabled && !header.key.is_empty())
        .map(|header| resolver(&header.key).trim().to_lowercase())
        .collect();
    resolved.headers.retain(|(key, _)| {
        let name = key.trim().to_lowercase();
        written_by_hand.contains(&name)
            || !api_client::AUTO_HEADER_DEFAULTS
                .iter()
                .any(|(automatic, _)| automatic.eq_ignore_ascii_case(key.trim()))
    });
    resolved
}

/// The exchange as it goes over the wire, which is what a reader pastes into a
/// bug report.
fn as_http_text(resolved: &ResolvedRequest, body: Option<&str>) -> String {
    let (host, path) = host_and_path(&resolved.url);
    let mut text = format!("{} {} HTTP/1.1\nHost: {host}\n", resolved.method, path);
    for (key, value) in &resolved.headers {
        text.push_str(&format!("{key}: {value}\n"));
    }
    if let Some(body) = body {
        text.push('\n');
        text.push_str(body);
    }
    text
}

fn as_go(resolved: &ResolvedRequest, body: Option<&str>) -> String {
    let mut code = String::from("package main\n\nimport (\n\t\"fmt\"\n\t\"io\"\n\t\"net/http\"\n");
    if body.is_some() {
        code.push_str("\t\"strings\"\n");
    }
    code.push_str(")\n\nfunc main() {\n");
    match body {
        Some(body) => code.push_str(&format!(
            "\tbody := strings.NewReader({})\n\trequest, err := http.NewRequest({}, {}, body)\n",
            quoted(body),
            quoted(&resolved.method),
            quoted(&resolved.url)
        )),
        None => code.push_str(&format!(
            "\trequest, err := http.NewRequest({}, {}, nil)\n",
            quoted(&resolved.method),
            quoted(&resolved.url)
        )),
    }
    code.push_str("\tif err != nil {\n\t\tpanic(err)\n\t}\n");
    for (key, value) in &resolved.headers {
        code.push_str(&format!(
            "\trequest.Header.Set({}, {})\n",
            quoted(key),
            quoted(value)
        ));
    }
    code.push_str(concat!(
        "\n\tresponse, err := http.DefaultClient.Do(request)\n",
        "\tif err != nil {\n\t\tpanic(err)\n\t}\n",
        "\tdefer response.Body.Close()\n\n",
        "\tanswer, err := io.ReadAll(response.Body)\n",
        "\tif err != nil {\n\t\tpanic(err)\n\t}\n",
        "\tfmt.Println(response.Status)\n\tfmt.Println(string(answer))\n}\n"
    ));
    code
}

fn as_python(resolved: &ResolvedRequest, body: Option<&str>) -> String {
    let mut code = String::from("import requests\n\n");
    code.push_str(&format!("url = {}\n", quoted(&resolved.url)));
    if resolved.headers.is_empty() {
        code.push_str("headers = {}\n");
    } else {
        code.push_str("headers = {\n");
        for (key, value) in &resolved.headers {
            code.push_str(&format!("    {}: {},\n", quoted(key), quoted(value)));
        }
        code.push_str("}\n");
    }
    match body {
        Some(body) => code.push_str(&format!(
            "data = {}\n\nresponse = requests.request({}, url, headers=headers, data=data)\n",
            quoted(body),
            quoted(&resolved.method)
        )),
        None => code.push_str(&format!(
            "\nresponse = requests.request({}, url, headers=headers)\n",
            quoted(&resolved.method)
        )),
    }
    code.push_str("print(response.status_code)\nprint(response.text)\n");
    code
}

fn as_javascript(resolved: &ResolvedRequest, body: Option<&str>) -> String {
    let mut code = format!(
        "const response = await fetch({}, {{\n  method: {},\n",
        quoted(&resolved.url),
        quoted(&resolved.method)
    );
    if !resolved.headers.is_empty() {
        code.push_str("  headers: {\n");
        for (key, value) in &resolved.headers {
            code.push_str(&format!("    {}: {},\n", quoted(key), quoted(value)));
        }
        code.push_str("  },\n");
    }
    if let Some(body) = body {
        code.push_str(&format!("  body: {},\n", quoted(body)));
    }
    code.push_str("});\n\nconsole.log(response.status);\nconsole.log(await response.text());\n");
    code
}

fn as_node_axios(resolved: &ResolvedRequest, body: Option<&str>) -> String {
    let mut code = String::from("import axios from \"axios\";\n\nconst response = await axios({\n");
    code.push_str(&format!("  method: {},\n", quoted(&resolved.method)));
    code.push_str(&format!("  url: {},\n", quoted(&resolved.url)));
    if !resolved.headers.is_empty() {
        code.push_str("  headers: {\n");
        for (key, value) in &resolved.headers {
            code.push_str(&format!("    {}: {},\n", quoted(key), quoted(value)));
        }
        code.push_str("  },\n");
    }
    if let Some(body) = body {
        code.push_str(&format!("  data: {},\n", quoted(body)));
    }
    code.push_str("});\n\nconsole.log(response.status);\nconsole.log(response.data);\n");
    code
}

fn as_rust(resolved: &ResolvedRequest, body: Option<&str>) -> String {
    let mut code = String::from(concat!(
        "// reqwest = { version = \"0.12\" }\n",
        "// tokio = { version = \"1\", features = [\"full\"] }\n\n",
        "#[tokio::main]\nasync fn main() -> Result<(), Box<dyn std::error::Error>> {\n",
        "    let client = reqwest::Client::new();\n"
    ));
    code.push_str(&format!(
        "    let response = client\n        .request(reqwest::Method::from_bytes({}.as_bytes())?, {})\n",
        quoted(&resolved.method),
        quoted(&resolved.url)
    ));
    for (key, value) in &resolved.headers {
        code.push_str(&format!(
            "        .header({}, {})\n",
            quoted(key),
            quoted(value)
        ));
    }
    if let Some(body) = body {
        code.push_str(&format!("        .body({})\n", quoted(body)));
    }
    code.push_str(concat!(
        "        .send()\n        .await?;\n\n",
        "    println!(\"{}\", response.status());\n",
        "    println!(\"{}\", response.text().await?);\n    Ok(())\n}\n"
    ));
    code
}

fn as_php(resolved: &ResolvedRequest, body: Option<&str>) -> String {
    let mut code = String::from("<?php\n\n$curl = curl_init();\n\ncurl_setopt_array($curl, [\n");
    code.push_str(&format!(
        "    CURLOPT_URL => {},\n",
        php_quoted(&resolved.url)
    ));
    code.push_str("    CURLOPT_RETURNTRANSFER => true,\n");
    code.push_str(&format!(
        "    CURLOPT_CUSTOMREQUEST => {},\n",
        php_quoted(&resolved.method)
    ));
    if !resolved.headers.is_empty() {
        code.push_str("    CURLOPT_HTTPHEADER => [\n");
        for (key, value) in &resolved.headers {
            code.push_str(&format!(
                "        {},\n",
                php_quoted(&format!("{key}: {value}"))
            ));
        }
        code.push_str("    ],\n");
    }
    if let Some(body) = body {
        code.push_str(&format!(
            "    CURLOPT_POSTFIELDS => {},\n",
            php_quoted(body)
        ));
    }
    code.push_str("]);\n\n$response = curl_exec($curl);\ncurl_close($curl);\n\necho $response;\n");
    code
}

fn as_csharp(resolved: &ResolvedRequest, body: Option<&str>) -> String {
    let mut code = String::from("using System.Net.Http;\n\nvar client = new HttpClient();\n");
    code.push_str(&format!(
        "var request = new HttpRequestMessage(new HttpMethod({}), {});\n",
        quoted(&resolved.method),
        quoted(&resolved.url)
    ));
    for (key, value) in &resolved.headers {
        code.push_str(&format!(
            "request.Headers.TryAddWithoutValidation({}, {});\n",
            quoted(key),
            quoted(value)
        ));
    }
    if let Some(body) = body {
        code.push_str(&format!(
            "request.Content = new StringContent({});\n",
            quoted(body)
        ));
    }
    code.push_str(concat!(
        "\nvar response = await client.SendAsync(request);\n",
        "Console.WriteLine((int)response.StatusCode);\n",
        "Console.WriteLine(await response.Content.ReadAsStringAsync());\n"
    ));
    code
}

fn as_java(resolved: &ResolvedRequest, body: Option<&str>) -> String {
    let mut code = String::from("OkHttpClient client = new OkHttpClient();\n\n");
    match body {
        Some(body) => code.push_str(&format!(
            "RequestBody body = RequestBody.create({}, null);\n\nRequest request = new Request.Builder()\n  .url({})\n  .method({}, body)\n",
            quoted(body),
            quoted(&resolved.url),
            quoted(&resolved.method)
        )),
        None => code.push_str(&format!(
            "Request request = new Request.Builder()\n  .url({})\n  .method({}, null)\n",
            quoted(&resolved.url),
            quoted(&resolved.method)
        )),
    }
    for (key, value) in &resolved.headers {
        code.push_str(&format!(
            "  .addHeader({}, {})\n",
            quoted(key),
            quoted(value)
        ));
    }
    code.push_str(concat!(
        "  .build();\n\nResponse response = client.newCall(request).execute();\n",
        "System.out.println(response.code());\n",
        "System.out.println(response.body().string());\n"
    ));
    code
}

fn as_ruby(resolved: &ResolvedRequest, body: Option<&str>) -> String {
    let mut code = String::from("require \"net/http\"\nrequire \"uri\"\n\n");
    code.push_str(&format!("uri = URI({})\n", quoted(&resolved.url)));
    code.push_str(&format!(
        "request = Net::HTTPGenericRequest.new({}, true, true, uri)\n",
        quoted(&resolved.method)
    ));
    for (key, value) in &resolved.headers {
        code.push_str(&format!("request[{}] = {}\n", quoted(key), quoted(value)));
    }
    if let Some(body) = body {
        code.push_str(&format!("request.body = {}\n", quoted(body)));
    }
    code.push_str(concat!(
        "\nresponse = Net::HTTP.start(uri.hostname, uri.port, use_ssl: uri.scheme == \"https\") do |http|\n",
        "  http.request(request)\nend\n\nputs response.code\nputs response.body\n"
    ));
    code
}

fn as_wget(resolved: &ResolvedRequest, body: Option<&str>) -> String {
    let mut command = format!(
        "wget --quiet --output-document=- --method={} {}",
        resolved.method,
        shell_quote(&resolved.url)
    );
    for (key, value) in &resolved.headers {
        command.push_str(&format!(
            " \\\n  --header {}",
            shell_quote(&format!("{key}: {value}"))
        ));
    }
    if let Some(body) = body {
        command.push_str(&format!(" \\\n  --body-data {}", shell_quote(body)));
    }
    command.push('\n');
    command
}

/// The host and the path of a URL, for the raw form of a request. Deliberately
/// plain: what is wanted is the two halves of a request line, not a parsed URL.
fn host_and_path(url: &str) -> (String, String) {
    let after_scheme = url.split_once("://").map(|(_, rest)| rest).unwrap_or(url);
    match after_scheme.find(['/', '?', '#']) {
        Some(at) => (
            after_scheme[..at].to_string(),
            after_scheme[at..].to_string(),
        ),
        None => (after_scheme.to_string(), "/".to_string()),
    }
}

/// A double-quoted string, which is how every language here but PHP writes one.
/// The backslash is escaped first: a value holding one would otherwise change
/// whatever follows it.
fn quoted(value: &str) -> String {
    let escaped = value
        .replace('\\', "\\\\")
        .replace('"', "\\\"")
        .replace('\n', "\\n")
        .replace('\r', "\\r")
        .replace('\t', "\\t");
    format!("\"{escaped}\"")
}

/// PHP single quotes take only two escapes, and a `$` inside double quotes would
/// be read as a variable -- so single quotes are the safe form there.
fn php_quoted(value: &str) -> String {
    format!("'{}'", value.replace('\\', "\\\\").replace('\'', "\\'"))
}

/// The request as a `curl` command line: the form every tool writes, so it can be
/// pasted into a shell or a ticket and read by somebody who has never seen this
/// editor.
///
/// `--location` because a redirect is followed when the request is sent here too.
/// `--request` is written only where it is needed: `curl` sends a GET without it,
/// and turns a request with `--data` into a POST on its own, so naming the method
/// in those two cases is noise.
fn as_curl(resolved: &ResolvedRequest, body: Option<&str>) -> String {
    let method = resolved.method.to_uppercase();
    let spelled_out = match (method.as_str(), body.is_some()) {
        ("GET", false) | ("POST", true) => String::new(),
        _ => format!("--request {method} "),
    };
    let mut command = format!(
        "curl --location {spelled_out}{}",
        shell_quote(&resolved.url)
    );
    for (key, value) in &resolved.headers {
        command.push_str(&format!(
            " \\\n  --header {}",
            shell_quote(&format!("{key}: {value}"))
        ));
    }
    if let Some(body) = body {
        command.push_str(&format!(" \\\n  --data {}", shell_quote(body)));
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

    /// Every auto-generated default header disabled. A snippet leaves them out
    /// anyway; this keeps the older assertions honest about what they are
    /// checking, which is the headers the test itself set.
    fn request_with_auto_headers_disabled(name: &str) -> Request {
        let mut request = Request::new(Uuid::new_v4(), name.to_string());
        request.settings.disabled_auto_headers = api_client::AUTO_HEADER_DEFAULTS
            .iter()
            .map(|(key, _)| key.to_string())
            .collect();
        request
    }

    /// Every shape has to carry what makes a request a request: the method, the
    /// address, a header and the body. A snippet missing one of them is code that
    /// does not do what the editor did.
    #[test]
    fn every_shape_carries_the_whole_request() {
        let mut request = request_with_auto_headers_disabled("Balance sheet");
        request.method = HttpMethod::Post;
        request.url = "https://api.example.com/v1/things".to_string();
        request.headers = vec![Header {
            key: "X-Token".to_string(),
            value: "sesame".to_string(),
            enabled: true,
            description: None,
        }];
        request.body = RequestBody::Raw {
            text: "{\"a\":1}".to_string(),
            content_type: RawBodyContentType::Json,
        };
        let global = Environment::global();
        let context = VariableContext {
            environment: None,
            collection: None,
            global: &global,
        };

        for snippet in Snippet::ALL {
            let code = generate(snippet, &request, &context);

            // The raw form writes the host and the path on separate lines, as the
            // wire does; everything else writes the whole address.
            for expected in ["api.example.com", "/v1/things", "X-Token", "sesame"] {
                assert!(
                    code.contains(expected),
                    "{} left out {expected}:\n{code}",
                    snippet.label()
                );
            }
            // `curl` turns a request with `--data` into a POST by itself, so
            // naming the method there would only be noise.
            let says_post = match snippet {
                Snippet::Curl => code.contains("--data"),
                _ => code.contains("POST"),
            };
            assert!(says_post, "{} left out POST:\n{code}", snippet.label());
            assert!(
                code.contains("a\\\":1") || code.contains("a\":1"),
                "{} left out the body:\n{code}",
                snippet.label()
            );
        }
    }

    /// A value with a quote, a backslash or a newline in it must not break out of
    /// the string it is written into: that is code which will not compile, or --
    /// worse -- code that runs and means something else.
    #[test]
    fn a_value_with_quotes_in_it_stays_inside_the_string() {
        let mut request = request_with_auto_headers_disabled("Awkward");
        request.url = "https://api.example.com/v1/things".to_string();
        request.headers = vec![Header {
            key: "X-Awkward".to_string(),
            value: "he said \"hello\" \\ and\nthen left".to_string(),
            enabled: true,
            description: None,
        }];
        let global = Environment::global();
        let context = VariableContext {
            environment: None,
            collection: None,
            global: &global,
        };

        for snippet in Snippet::ALL {
            let code = generate(snippet, &request, &context);

            match snippet {
                // Neither of these is code. A shell's single quotes hold a newline
                // as it is -- that is what makes the command safe to paste -- and
                // the raw form is the request itself, where a newline in a header
                // is the request's own problem.
                Snippet::Curl | Snippet::Wget | Snippet::HttpText => {}
                // PHP writes single quotes, where only the backslash and the quote
                // need escaping; a newline inside them is the value, not a break.
                Snippet::Php => {
                    assert!(
                        code.contains("\\\\ and"),
                        "PHP left a bare backslash in a single-quoted string:\n{code}"
                    );
                }
                _ => {
                    assert!(
                        !code.contains("and\nthen left"),
                        "{} let a newline out of the string:\n{code}",
                        snippet.label()
                    );
                    assert!(
                        code.contains("and\\nthen left"),
                        "{} did not write the newline as an escape:\n{code}",
                        snippet.label()
                    );
                    assert!(
                        code.contains("\\\"hello\\\""),
                        "{} did not escape the quotes in the value:\n{code}",
                        snippet.label()
                    );
                }
            }
        }
    }

    #[test]
    fn the_raw_form_reads_like_the_request_on_the_wire() {
        let mut request = request_with_auto_headers_disabled("Read it");
        request.url = "https://api.example.com/v1/things?page=2".to_string();
        let global = Environment::global();
        let context = VariableContext {
            environment: None,
            collection: None,
            global: &global,
        };

        let text = generate(Snippet::HttpText, &request, &context);

        assert!(
            text.starts_with("GET /v1/things?page=2 HTTP/1.1\nHost: api.example.com"),
            "the request line and the host come first, as they do on the wire:\n{text}"
        );
    }

    #[test]
    fn every_shape_has_a_name_of_its_own() {
        let mut names: Vec<&str> = Snippet::ALL.iter().map(|shape| shape.label()).collect();
        let count = names.len();
        names.sort();
        names.dedup();

        assert_eq!(
            names.len(),
            count,
            "two shapes with the same name in the picker cannot be told apart"
        );
    }

    fn a_request_named(name: &str, url: &str) -> Request {
        let mut request = Request::new(Uuid::new_v4(), name.to_string());
        request.url = url.to_string();
        request
    }

    fn the_global_context(global: &Environment) -> VariableContext<'_> {
        VariableContext {
            environment: None,
            collection: None,
            global,
        }
    }

    /// A snippet is for another tool, which sends its own `User-Agent`, `Accept`
    /// and the rest. Ours have no business in it -- but a header of the same name
    /// that the reader set by hand is theirs, and stays.
    #[test]
    fn the_headers_this_client_adds_for_itself_stay_out_of_a_snippet() {
        let mut request = a_request_named("Balance sheet", "https://api.example.com/v1/things");
        request.headers = vec![Header {
            key: "Accept".to_string(),
            value: "application/json".to_string(),
            enabled: true,
            description: None,
        }];
        let global = Environment::global();
        let context = the_global_context(&global);

        let curl = generate(Snippet::Curl, &request, &context);
        assert_eq!(
            curl,
            "curl --location 'https://api.example.com/v1/things' \\\n  \
             --header 'Accept: application/json'",
            "only the reader's own header, and theirs wins over ours of the same name"
        );

        for snippet in Snippet::ALL {
            let code = generate(snippet, &request, &context);
            assert!(
                !code.contains("ZedApiClient"),
                "{:?} must not tell another tool to call itself this editor:\n{code}",
                snippet
            );
            assert!(
                !code.contains("keep-alive") && !code.contains("no-cache"),
                "{:?} carries a header the reader never set:\n{code}",
                snippet
            );
        }
    }

    /// The method is named only where `curl` would otherwise get it wrong: it
    /// sends a GET by default, and `--data` makes it a POST on its own.
    #[test]
    fn the_method_is_named_only_where_curl_needs_it() {
        let global = Environment::global();
        let context = the_global_context(&global);
        let with_a_body = |mut request: Request| {
            request.body = RequestBody::Raw {
                content_type: RawBodyContentType::Json,
                text: "{\"a\":1}".to_string(),
            };
            request
        };

        let get = a_request_named("Read", "https://api.example.com/things");
        assert_eq!(
            generate(Snippet::Curl, &get, &context),
            "curl --location 'https://api.example.com/things'"
        );

        let mut post = a_request_named("Create", "https://api.example.com/things");
        post.method = HttpMethod::Post;
        let post = with_a_body(post);
        let written = generate(Snippet::Curl, &post, &context);
        assert!(
            written.starts_with("curl --location 'https://api.example.com/things'"),
            "a POST with a body needs no --request: {written}"
        );
        assert!(written.contains("--data"), "{written}");

        let mut put = a_request_named("Replace", "https://api.example.com/things/1");
        put.method = HttpMethod::Put;
        let put = with_a_body(put);
        assert!(
            generate(Snippet::Curl, &put, &context)
                .starts_with("curl --location --request PUT 'https://api.example.com/things/1'"),
            "every other method has to be named"
        );

        let mut delete = a_request_named("Remove", "https://api.example.com/things/1");
        delete.method = HttpMethod::Delete;
        assert!(
            generate(Snippet::Curl, &delete, &context)
                .starts_with("curl --location --request DELETE"),
            "a DELETE without a body still has to be named"
        );

        let read_with_a_body =
            with_a_body(a_request_named("Odd", "https://api.example.com/search"));
        assert!(
            generate(Snippet::Curl, &read_with_a_body, &context)
                .starts_with("curl --location --request GET"),
            "a GET that carries a body has to say so, or curl would send a POST"
        );
    }

    /// What the reader sees for the request in front of them: a plain command, and
    /// nothing that was not asked for.
    #[test]
    fn a_get_with_a_filled_in_path_is_one_plain_line() {
        let mut request = a_request_named(
            "Balance sheet",
            "{{financials-api}}/v1/instruments/:instrument_id/balance-sheet",
        );
        request.params = vec![api_client::QueryParam {
            key: ":instrument_id".to_string(),
            value: "6408".to_string(),
            enabled: true,
            description: None,
        }];
        let mut global = Environment::global();
        global.variables.push(api_client::Variable::new(
            "financials-api".into(),
            "http://financials.example.com".into(),
        ));
        let context = the_global_context(&global);

        assert_eq!(
            generate(Snippet::Curl, &request, &context),
            "curl --location 'http://financials.example.com/v1/instruments/6408/balance-sheet'",
            "nothing else belongs in it: no body, no headers of ours, no --request"
        );
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
        let curl = generate(Snippet::Curl, &request, &ctx);
        assert_eq!(curl, "curl --location 'https://api.example.com/ping'");
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
        let curl = generate(Snippet::Curl, &request, &ctx);
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
        let curl = generate(Snippet::Curl, &request, &ctx);
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
        let curl = generate(Snippet::Curl, &request, &ctx);
        assert!(curl.contains("https://staging.example.com/ping"));
    }
}
