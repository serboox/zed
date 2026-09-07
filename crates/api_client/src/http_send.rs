use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::time::Instant;

use rand::RngCore;

use crate::request::{ApiKeyPlacement, AuthConfig, FormDataValue, Request, RequestBody};

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

/// Headers most HTTP clients send by default, in the order they're applied.
/// `Content-Length` and `Host` are deliberately not in this list -- both are
/// computed by the HTTP transport itself from the final body/URL, so
/// treating them as regular send-able headers here would risk the
/// transport-computed value and a stale one we sent disagreeing.
pub const AUTO_HEADER_DEFAULTS: &[(&str, &str)] = &[
    ("Cache-Control", "no-cache"),
    ("User-Agent", "ZedApiClient/1.0"),
    ("Accept", "*/*"),
    ("Accept-Encoding", "gzip, deflate, br"),
    ("Connection", "keep-alive"),
];

/// Layers the enabled auto-generated headers (every `AUTO_HEADER_DEFAULTS`
/// entry not named in `disabled`) onto `headers`, skipping (case- and
/// whitespace-insensitively) any header already present under the same
/// name -- a user-defined header always wins over the auto-generated
/// default. Runs before auth signing so signature-based auth (e.g. AWS
/// SigV4) covers these headers too, the same as any other header the user
/// set explicitly.
fn apply_auto_headers(headers: &mut Vec<(String, String)>, disabled: &[String]) {
    for (key, value) in AUTO_HEADER_DEFAULTS {
        let is_disabled = disabled
            .iter()
            .any(|name| name.trim().eq_ignore_ascii_case(key));
        if is_disabled {
            continue;
        }
        let already_present = headers
            .iter()
            .any(|(existing_key, _)| existing_key.trim().eq_ignore_ascii_case(key));
        if !already_present {
            headers.push((key.to_string(), value.to_string()));
        }
    }
}

/// The bytes of every file one body sends, read before the build so that the
/// build itself stays free of I/O -- it runs wherever Send was pressed, up to
/// and including the thread that draws the window, where reading a file of
/// upload size would stop the editor.
#[derive(Debug, Clone, Default)]
pub struct FilesForABody {
    read: HashMap<PathBuf, Vec<u8>>,
    unreadable: Vec<(PathBuf, String)>,
}

impl FilesForABody {
    /// Reads every path [`files_a_body_needs`] named. Awaiting this is safe from
    /// any thread -- the read runs on a blocking pool rather than here -- which
    /// is why it is async rather than a plain call every caller would have to
    /// remember to wrap in a background task.
    pub async fn read_them(paths: Vec<PathBuf>) -> Self {
        let mut files = Self::default();
        for path in paths {
            match smol::fs::read(&path).await {
                Ok(bytes) => {
                    files.read.insert(path, bytes);
                }
                Err(error) => files.unreadable.push((path, error.to_string())),
            }
        }
        files
    }

    pub fn bytes_of(&self, path: &Path) -> Option<&[u8]> {
        self.read.get(path).map(Vec::as_slice)
    }

    /// The paths that could not be read, each with the reason. A body needing
    /// one of them is not built at all, so a caller that can tell the reader
    /// something -- Send can -- has to read this to say what went wrong; the
    /// build itself has nowhere to report it.
    pub fn unreadable(&self) -> &[(PathBuf, String)] {
        &self.unreadable
    }
}

/// Every file this body sends, so they can be read before the build.
///
/// The paths come back as they were written, with no `resolve` applied: this has
/// no variable context to resolve against, and resolving inside the build
/// instead would leave it looking up a file nobody read. A path is chosen from
/// disk rather than typed, so there is nothing in it a variable would fill.
pub fn files_a_body_needs(body: &RequestBody) -> Vec<PathBuf> {
    match body {
        RequestBody::Binary { path } if !path.as_os_str().is_empty() => vec![path.clone()],
        RequestBody::FormData(fields) => fields
            .iter()
            .filter(|field| field.enabled && !field.key.is_empty())
            .filter_map(|field| match &field.value {
                FormDataValue::File(path) if !path.as_os_str().is_empty() => Some(path.clone()),
                FormDataValue::File(_) | FormDataValue::Text(_) => None,
            })
            .collect(),
        RequestBody::None
        | RequestBody::Raw { .. }
        | RequestBody::UrlEncoded(_)
        | RequestBody::GraphQl { .. }
        | RequestBody::Binary { .. } => Vec::new(),
    }
}

/// A content type a body requires, and how firmly.
enum ContentTypeToSend {
    /// A header the reader wrote by hand wins over this one.
    UnlessWrittenByHand(String),
    /// Replaces whatever the reader wrote. Only the build knows the multipart
    /// boundary it has just generated, so a hand-written `multipart/form-data`
    /// header names a different boundary or none at all, and a server reading
    /// that header finds no parts in a body that has them.
    EvenOverWhatWasWritten(String),
}

/// A boundary no body can hold by accident, written the way other clients write
/// theirs so that a reader recognises it in a capture.
fn a_multipart_boundary() -> String {
    let mut bytes = [0u8; 16];
    rand::rng().fill_bytes(&mut bytes);
    let random: String = bytes.iter().map(|byte| format!("{byte:02x}")).collect();
    format!("----ZedApiClientFormBoundary{random}")
}

/// Text put between the quotes of a `Content-Disposition` parameter. A quote or
/// a line ending left in it would end the parameter, or the header itself,
/// early -- which is a part the reader never wrote appearing in the body.
fn between_quotes(text: &str) -> String {
    text.chars()
        .filter(|character| !matches!(character, '\r' | '\n'))
        .map(|character| if character == '"' { '\'' } else { character })
        .collect()
}

/// The bytes to send for one body, and the content type they have to carry.
///
/// The content type comes back rather than being set here because a header the
/// reader wrote by hand usually wins over it, and only the caller holds the
/// headers. `None` means this body says nothing about its type -- a raw body's
/// type is the reader's to state, and always has been.
///
/// The bytes of a file body -- `Binary`, or a `FormData` field holding a file --
/// come from `files`, read before this was called; see [`FilesForABody`] for
/// why they are not read here.
fn body_to_send(
    body: &RequestBody,
    resolve: &impl Fn(&str) -> String,
    files: &FilesForABody,
) -> (Option<Vec<u8>>, Option<ContentTypeToSend>) {
    match body {
        RequestBody::Raw { text, .. } if !text.is_empty() => {
            (Some(resolve(text).into_bytes()), None)
        }
        RequestBody::UrlEncoded(pairs) => {
            let written: Vec<String> = pairs
                .iter()
                .filter(|(key, _)| !key.is_empty())
                .map(|(key, value)| {
                    format!(
                        "{}={}",
                        urlencoding::encode(&resolve(key)),
                        urlencoding::encode(&resolve(value))
                    )
                })
                .collect();
            if written.is_empty() {
                return (None, None);
            }
            (
                Some(written.join("&").into_bytes()),
                Some(ContentTypeToSend::UnlessWrittenByHand(
                    "application/x-www-form-urlencoded".to_string(),
                )),
            )
        }
        RequestBody::GraphQl { query, variables } => {
            if query.trim().is_empty() {
                return (None, None);
            }
            let mut sending = serde_json::Map::new();
            sending.insert(
                "query".to_string(),
                serde_json::Value::String(resolve(query)),
            );
            let written = resolve(variables);
            // Variables are written as JSON by the reader, and are sent as the
            // object they parse to rather than as a string holding one -- a
            // server reading `variables` expects an object there. Text that is
            // not an object at all is left out entirely: sending it as a string
            // would be rejected by every server, and guessing at what was meant
            // is worse than sending the query alone.
            if let Ok(serde_json::Value::Object(parsed)) =
                serde_json::from_str::<serde_json::Value>(&written)
            {
                sending.insert("variables".to_string(), serde_json::Value::Object(parsed));
            }
            let Ok(text) = serde_json::to_vec(&serde_json::Value::Object(sending)) else {
                return (None, None);
            };
            (
                Some(text),
                Some(ContentTypeToSend::UnlessWrittenByHand(
                    "application/json".to_string(),
                )),
            )
        }
        RequestBody::Binary { path } if !path.as_os_str().is_empty() => {
            // A file that could not be read makes this no body at all rather
            // than an empty one: an empty PUT reads to a server as "store
            // nothing here", which is a worse answer than a request that
            // plainly carried nothing. What went wrong is in
            // `FilesForABody::unreadable`, for the caller to show.
            let Some(bytes) = files.bytes_of(path) else {
                return (None, None);
            };
            (
                Some(bytes.to_vec()),
                Some(ContentTypeToSend::UnlessWrittenByHand(
                    "application/octet-stream".to_string(),
                )),
            )
        }
        RequestBody::FormData(fields) => {
            let sending: Vec<&crate::request::FormDataField> = fields
                .iter()
                .filter(|field| field.enabled && !field.key.is_empty())
                .collect();
            if sending.is_empty() {
                return (None, None);
            }
            let boundary = a_multipart_boundary();
            let mut written: Vec<u8> = Vec::new();
            for field in sending {
                let name = between_quotes(&resolve(&field.key));
                written.extend_from_slice(format!("--{boundary}\r\n").as_bytes());
                match &field.value {
                    FormDataValue::Text(text) => {
                        written.extend_from_slice(
                            format!("Content-Disposition: form-data; name=\"{name}\"\r\n\r\n")
                                .as_bytes(),
                        );
                        written.extend_from_slice(resolve(text).as_bytes());
                    }
                    FormDataValue::File(path) => {
                        // One part missing makes the whole form wrong, so none
                        // of it is sent: a server handed a form without its file
                        // either refuses it or stores a record with a hole in it,
                        // and both are harder to trace back to an unreadable
                        // file than a request that carried no body at all. The
                        // reason is in `FilesForABody::unreadable`.
                        let Some(bytes) = files.bytes_of(path) else {
                            return (None, None);
                        };
                        let filename =
                            between_quotes(&path.file_name().unwrap_or_default().to_string_lossy());
                        written.extend_from_slice(
                            format!(
                                "Content-Disposition: form-data; name=\"{name}\"; \
                                 filename=\"{filename}\"\r\n\
                                 Content-Type: application/octet-stream\r\n\r\n"
                            )
                            .as_bytes(),
                        );
                        written.extend_from_slice(bytes);
                    }
                }
                written.extend_from_slice(b"\r\n");
            }
            written.extend_from_slice(format!("--{boundary}--\r\n").as_bytes());
            (
                Some(written),
                Some(ContentTypeToSend::EvenOverWhatWasWritten(format!(
                    "multipart/form-data; boundary={boundary}"
                ))),
            )
        }
        RequestBody::None | RequestBody::Raw { .. } | RequestBody::Binary { .. } => (None, None),
    }
}

/// Applies `resolve` (a `{{token}}` substitution function, typically
/// `variable_resolution::resolve` bound to a `VariableContext`) to the URL,
/// the query parameters, the headers, the body and every credential, merges
/// enabled query params into the URL, and layers auth on top as either a
/// header or a query param.
///
/// For a request whose body holds no file. Anything that can hold one goes
/// through [`build_resolved_request_with_files`], the single place a request is
/// built, so that the file reaches the wire whether Send, the collection runner
/// or a generated snippet asked for it.
pub fn build_resolved_request(
    request: &Request,
    resolve: &impl Fn(&str) -> String,
) -> ResolvedRequest {
    build_resolved_request_with_files(request, resolve, &FilesForABody::default())
}

/// The same, for a body whose files [`FilesForABody::read_them`] has already
/// read.
pub fn build_resolved_request_with_files(
    request: &Request,
    resolve: &impl Fn(&str) -> String,
    files: &FilesForABody,
) -> ResolvedRequest {
    let mut url = resolve(&request.url);

    let enabled_params: Vec<(String, String)> = request
        .params
        .iter()
        .filter(|param| param.enabled && !param.key.is_empty())
        .map(|param| (resolve(&param.key), resolve(&param.value)))
        .collect();
    // A parameter whose name matches a `:name` in the path fills that place in
    // the path rather than being hung on the end as a query: a URL written
    // `/v1/instruments/:instrument_id/balance-sheet` is asking for the value
    // there, not for `?instrument_id=`.
    let left_for_the_query = put_params_in_the_path(&mut url, &enabled_params);
    // A parameter the reader wrote into the address bar is already in the URL --
    // the table below it shows the same one. Appending it again would send it
    // twice.
    let already_there = query_of(&url).map(query_pairs).unwrap_or_default();
    let to_append: Vec<(String, String)> = left_for_the_query
        .into_iter()
        .filter(|pair| !already_there.contains(pair))
        .collect();
    append_query_params(&mut url, &to_append);

    let mut headers: Vec<(String, String)> = request
        .headers
        .iter()
        .filter(|header| header.enabled && !header.key.is_empty())
        .map(|header| (resolve(&header.key), resolve(&header.value)))
        .collect();
    apply_auto_headers(&mut headers, &request.settings.disabled_auto_headers);

    let (body, needs_content_type) = body_to_send(&request.body, resolve, files);
    match needs_content_type {
        Some(ContentTypeToSend::UnlessWrittenByHand(content_type)) => {
            if !headers
                .iter()
                .any(|(key, _)| key.trim().eq_ignore_ascii_case("content-type"))
            {
                headers.push(("Content-Type".to_string(), content_type));
            }
        }
        Some(ContentTypeToSend::EvenOverWhatWasWritten(content_type)) => {
            headers.retain(|(key, _)| !key.trim().eq_ignore_ascii_case("content-type"));
            headers.push(("Content-Type".to_string(), content_type));
        }
        None => {}
    }

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
                    format!("Bearer {}", resolve(&oauth2.access_token)),
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

/// Fills each `:name` in the URL's path with the parameter of that name, and
/// returns the parameters that were not used, which belong on the query string.
///
/// A parameter may be written with the colon (`:instrument_id`) or without
/// (`instrument_id`); both fill `:instrument_id`. A place in the path with no
/// parameter for it is left as it is, so the reader can see what is missing
/// rather than sending a URL with a hole in it.
fn put_params_in_the_path(url: &mut String, params: &[(String, String)]) -> Vec<(String, String)> {
    // Only the path is looked at: a colon appears in `https://` and in a port,
    // and a query string may hold one legitimately.
    let after_scheme = url.find("://").map(|at| at + 3).unwrap_or(0);
    let path_ends = url[after_scheme..]
        .find(['?', '#'])
        .map(|at| at + after_scheme)
        .unwrap_or(url.len());
    let mut path = url[after_scheme..path_ends].to_string();
    let mut left_over = Vec::new();
    let mut filled_any = false;

    for (key, value) in params {
        let name = key.strip_prefix(':').unwrap_or(key);
        if name.is_empty() {
            left_over.push((key.clone(), value.clone()));
            continue;
        }
        let place = format!(":{name}");
        // Whole segments only: `:id` must not be found inside `:instrument_id`.
        let filled: Vec<String> = path
            .split('/')
            .map(|segment| match segment == place {
                true => urlencoding::encode(value).into_owned(),
                false => segment.to_string(),
            })
            .collect();
        let filled = filled.join("/");
        match filled == path {
            true => left_over.push((key.clone(), value.clone())),
            false => {
                path = filled;
                filled_any = true;
            }
        }
    }

    if filled_any {
        url.replace_range(after_scheme..path_ends, &path);
    }
    left_over
}

/// Where the first of `wanted` sits after the scheme, skipping over any
/// `{{variable}}` token: a `?` or a `#` inside a token is part of the token's
/// name, not a delimiter of the URL.
fn delimiter_after_the_scheme(url: &str, wanted: &[char]) -> Option<usize> {
    let mut at = url.find("://").map(|scheme| scheme + 3).unwrap_or(0);
    while at < url.len() {
        let rest = &url[at..];
        if rest.starts_with("{{") {
            // An unclosed token runs to the end of the text, and everything in it
            // is the token's own business.
            let closes = rest.find("}}")?;
            at += closes + 2;
            continue;
        }
        // Stepped a character at a time, never a byte: a URL may hold any letter
        // somebody can type, and a byte step lands inside a multi-byte one -- which
        // is a panic, not a mistake in the answer.
        let character = rest.chars().next()?;
        if wanted.contains(&character) {
            return Some(at);
        }
        at += character.len_utf8();
    }
    None
}

/// The query string of `url`, without the `?`, and without any `#fragment`.
///
/// Plain text handling on purpose: a URL here may still hold `{{variable}}`
/// tokens and `:name` places, so it is not a URL a parser would accept yet.
pub fn query_of(url: &str) -> Option<&str> {
    // The fragment goes first: a `?` after a `#` belongs to the fragment.
    let ends = delimiter_after_the_scheme(url, &['#']).unwrap_or(url.len());
    let before_fragment = &url[..ends];
    let question = delimiter_after_the_scheme(before_fragment, &['?'])?;
    Some(&before_fragment[question + 1..])
}

/// Everything before the query string, and the `#fragment` if there is one.
pub fn url_without_query(url: &str) -> (&str, Option<&str>) {
    let fragment = delimiter_after_the_scheme(url, &['#']).map(|at| &url[at + 1..]);
    let ends = delimiter_after_the_scheme(url, &['?', '#']).unwrap_or(url.len());
    (&url[..ends], fragment)
}

/// The `:name` places the path holds, in the order they appear, each with its
/// colon.
///
/// Whole segments only, the same rule `put_params_in_the_path` fills them by: a
/// colon inside a segment (`v1:beta`) is a colon somebody wrote, not a place. A
/// `{{token}}` is stepped over -- what is inside it is the token's own.
pub fn path_places(url: &str) -> Vec<String> {
    let (path, _) = url_without_query(url);
    let after_scheme = path.find("://").map(|scheme| scheme + 3).unwrap_or(0);
    let mut places = Vec::new();
    for segment in path[after_scheme..].split('/') {
        if segment.starts_with("{{") || !segment.starts_with(':') || segment.len() < 2 {
            continue;
        }
        let place = segment.to_string();
        if !places.contains(&place) {
            places.push(place);
        }
    }
    places
}

/// The `key=value` pairs a query string holds, exactly as written. A pair with no
/// `=` is a key with an empty value, which is how a bare `?flag` reads.
///
/// Nothing is decoded here. What the address bar holds is what the table beside it
/// shows, so a value that no decoder accepts (`%2G`, `%FF`) reads back the way it
/// was written instead of coming home as `%252G`.
pub fn query_pairs(query: &str) -> Vec<(String, String)> {
    query
        .split('&')
        .filter(|pair| !pair.is_empty())
        .map(|pair| match pair.split_once('=') {
            Some((key, value)) => (key.to_string(), value.to_string()),
            None => (pair.to_string(), String::new()),
        })
        .collect()
}

/// `url` with its query string replaced by `pairs`, keeping the fragment. No pairs
/// means no `?` at all.
pub fn url_with_query(url: &str, pairs: &[(String, String)]) -> String {
    let (base, fragment) = url_without_query(url);
    let mut written = base.to_string();
    for (at, (key, value)) in pairs.iter().enumerate() {
        written.push(match at {
            0 => '?',
            _ => '&',
        });
        written.push_str(&kept_from_breaking_the_query(key, true));
        written.push('=');
        written.push_str(&kept_from_breaking_the_query(value, false));
    }
    if let Some(fragment) = fragment {
        written.push('#');
        written.push_str(fragment);
    }
    written
}

/// `text` with only what would break a query string escaped: a space, an `&`, a
/// `#`, and in a key an `=` as well.
///
/// A `%` is left as it stands, so an escape already in the address bar is not
/// escaped a second time. Anything else the transport encodes on its way out.
fn kept_from_breaking_the_query(text: &str, is_key: bool) -> String {
    let mut written = String::with_capacity(text.len());
    for character in text.chars() {
        match character {
            ' ' => written.push_str("%20"),
            '&' => written.push_str("%26"),
            '#' => written.push_str("%23"),
            '=' if is_key => written.push_str("%3D"),
            _ => written.push(character),
        }
    }
    written
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
    pub timings: Timings,
}

/// Where a request's time went, in milliseconds.
///
/// Only what this transport can honestly measure is here. Looking the host up is
/// measured because it is done here, before the request goes out. What follows --
/// opening the connection, the handshake, sending, and the server's own thinking
/// -- arrives as one number, because the client underneath reports nothing
/// between them. Reading the body is measured separately, since that is where a
/// large response spends its time and a reader needs to tell the two apart.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Timings {
    /// Looking the host up. None when there was no host to look up, or when the
    /// lookup failed and the client was left to do it.
    pub resolve_ms: Option<u64>,
    /// From the request going out to its headers arriving.
    pub wait_ms: u64,
    /// Reading the body, once the headers were in.
    pub download_ms: u64,
    /// The whole exchange, which is what the reader sees beside the status.
    pub total_ms: u64,
}

/// Looks up the host of `url`, and says how long it took. None when there is no
/// host to look up or the lookup failed -- then the client does it itself and
/// there is nothing honest to report.
async fn look_the_host_up(url: &str) -> Option<u64> {
    let host = host_of(url)?;
    let started = Instant::now();
    let found = crate::network_runtime::on_network_runtime(async move {
        // Port zero: what is being timed is the name, not a connection.
        let addresses = tokio::net::lookup_host((host.as_str(), 0u16)).await?;
        anyhow::Ok(addresses.count())
    })
    .await;
    match found {
        Ok(count) if count > 0 => Some(started.elapsed().as_millis() as u64),
        _ => None,
    }
}

/// The host part of a URL, without the scheme, the credentials or the port.
fn host_of(url: &str) -> Option<String> {
    let after_scheme = url.split_once("://").map(|(_, rest)| rest).unwrap_or(url);
    let authority = after_scheme
        .split(['/', '?', '#'])
        .next()
        .filter(|authority| !authority.is_empty())?;
    let authority = authority
        .rsplit_once('@')
        .map(|(_, host)| host)
        .unwrap_or(authority);
    // A bracketed address holds colons of its own.
    let host = match authority.starts_with('[') {
        true => authority.split_once(']').map(|(host, _)| &host[1..])?,
        false => authority.split(':').next()?,
    };
    match host.is_empty() {
        true => None,
        false => Some(host.to_string()),
    }
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
    // The host is looked up here, before the request goes out, so the reader can
    // see what the lookup cost. The client would otherwise do it inside the send
    // and report nothing about it.
    let looked_up = look_the_host_up(&resolved.url).await;
    let (status, status_text, headers, body, wait_ms, download_ms) =
        crate::network_runtime::on_network_runtime(async move {
            let sent = Instant::now();
            let response = builder.send().await?;
            let wait_ms = sent.elapsed().as_millis() as u64;
            let status = response.status();
            let status_text = status.canonical_reason().unwrap_or("").to_string();
            let headers = response
                .headers()
                .iter()
                .map(|(name, value)| (name.to_string(), value.to_str().unwrap_or("").to_string()))
                .collect::<Vec<_>>();
            let reading = Instant::now();
            let body = response.bytes().await?.to_vec();
            let download_ms = reading.elapsed().as_millis() as u64;
            anyhow::Ok((
                status.as_u16(),
                status_text,
                headers,
                body,
                wait_ms,
                download_ms,
            ))
        })
        .await?;
    let elapsed_ms = started.elapsed().as_millis() as u64;
    let timings = Timings {
        resolve_ms: looked_up,
        wait_ms,
        download_ms,
        total_ms: elapsed_ms,
    };

    Ok(HttpResponseSummary {
        status,
        status_text,
        headers,
        body,
        elapsed_ms,
        timings,
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

    /// What the reader typed after the `?` is read back exactly, tokens and all.
    #[test]
    fn a_query_string_is_read_pair_by_pair() {
        let url = "{{financials-api}}/v1/instruments/:instrument_id/ratios?hello=world&page=2";
        assert_eq!(query_of(url), Some("hello=world&page=2"));
        assert_eq!(
            query_pairs(query_of(url).expect("there is a query")),
            vec![
                ("hello".to_string(), "world".to_string()),
                ("page".to_string(), "2".to_string()),
            ]
        );
        assert_eq!(
            url_without_query(url).0,
            "{{financials-api}}/v1/instruments/:instrument_id/ratios"
        );
        assert_eq!(query_of("https://example.com/things"), None);
        assert_eq!(
            query_pairs("flag&name=a%20b"),
            vec![
                ("flag".to_string(), String::new()),
                ("name".to_string(), "a%20b".to_string()),
            ],
            "a bare word is a key with nothing after it, and what is written is what is read"
        );
    }

    /// A colon in `https://` or in a port is not a query, and a fragment is kept.
    #[test]
    fn the_query_is_found_where_the_query_is() {
        assert_eq!(query_of("https://example.com:8080/things?a=1"), Some("a=1"));
        assert_eq!(query_of("https://example.com/things#a?b"), None);
        assert_eq!(
            url_with_query(
                "https://example.com/things?old=1#section",
                &[("new".to_string(), "2".to_string())]
            ),
            "https://example.com/things?new=2#section"
        );
        assert_eq!(
            url_with_query("https://example.com/things?old=1", &[]),
            "https://example.com/things",
            "nothing to write means no question mark at all"
        );
        assert_eq!(
            url_with_query(
                "https://example.com/things",
                &[("q".to_string(), "a b&c".to_string())]
            ),
            "https://example.com/things?q=a%20b%26c",
            "what would otherwise split the query is encoded"
        );
    }

    /// A value the address bar holds comes back out of the table the way it went
    /// in, whether or not any decoder would accept it.
    #[test]
    fn a_query_survives_the_trip_through_the_table() {
        for query in [
            "x=%2G",
            "x=%FF",
            "x=%20",
            "name=a%20b&other=%D0%BF%D1%80%D0%B8%D0%B2%D0%B5%D1%82",
            "=1",
            "a=1&a=2",
            "flag=",
        ] {
            let url = format!("https://example.com/things?{query}");
            let pairs = query_pairs(query_of(&url).expect("there is a query"));
            assert_eq!(
                url_with_query(&url, &pairs),
                url,
                "`{query}` has to read back exactly as it was written"
            );
        }
    }

    /// A `?` or a `#` inside a `{{token}}` is part of the token's name. The address
    /// bar holds tokens that have not been resolved yet, so they are stepped over
    /// rather than read as the start of a query.
    #[test]
    fn a_token_is_not_a_delimiter() {
        assert_eq!(query_of("{{base?part}}/v1/things"), None);
        assert_eq!(
            url_without_query("{{base#part}}/v1/things").0,
            "{{base#part}}/v1/things"
        );
        assert_eq!(
            query_of("{{base?part}}/v1/things?real=1"),
            Some("real=1"),
            "the query after the token is still found"
        );
        assert_eq!(
            query_of("{{unclosed?/v1/things?real=1"),
            None,
            "an unclosed token runs to the end, and everything in it is its own"
        );
    }

    /// A `:name` written into the path is a place waiting for a value, and the table
    /// beside the address bar is where that value is written.
    #[test]
    fn the_places_in_a_path_are_found_in_the_order_they_are_written() {
        assert_eq!(
            path_places("{{financials-api}}/v1/instruments/:instrument_id/ratios"),
            vec![":instrument_id".to_string()]
        );
        assert_eq!(
            path_places("https://example.com/:first/things/:second?:third=1"),
            vec![":first".to_string(), ":second".to_string()],
            "the query is not the path, whatever it holds"
        );
        assert_eq!(
            path_places("https://example.com:8080/v1:beta/things"),
            Vec::<String>::new(),
            "a port and a colon inside a segment are not places"
        );
        assert_eq!(
            path_places("https://example.com/:same/x/:same"),
            vec![":same".to_string()],
            "one place, however many times it is written"
        );
        assert_eq!(
            path_places("https://example.com/:/x"),
            Vec::<String>::new(),
            "a bare colon names nothing"
        );
    }

    /// Any letter somebody can type may end up in the address bar. Walking it a
    /// byte at a time lands inside a multi-byte one, which is a panic rather than a
    /// wrong answer.
    #[test]
    fn a_url_of_any_letters_is_read_without_panicking() {
        for url in [
            "https://пример.рф/путь?ключ=значение",
            "{{база}}/v1/инструменты/:идентификатор/ratios",
            "https://example.com/emoji/🙂?q=🙂#🙂",
            "{{unclosed🙂/v1/things?a=1",
            "приветбезсхемы?a=1",
            "",
            "?",
            "#",
            "{{",
            "}}",
        ] {
            let query = query_of(url);
            let (base, fragment) = url_without_query(url);
            let pairs = query.map(query_pairs).unwrap_or_default();
            let written = url_with_query(url, &pairs);
            assert!(
                base.len() <= url.len() && written.starts_with(base),
                "`{url}` has to read back as itself: base `{base}`, fragment {fragment:?}, \
                 written `{written}`"
            );
        }
        assert_eq!(
            query_of("https://пример.рф/путь?ключ=значение"),
            Some("ключ=значение")
        );
        assert_eq!(
            query_of("{{база}}/v1/инструменты/:идентификатор/ratios"),
            None,
            "a token of any letters is still stepped over"
        );
    }

    /// Nothing in the table is dropped for sharing a key with the address bar --
    /// only for being the very same pair, which would otherwise go over the wire
    /// twice.
    #[test]
    fn the_same_key_with_another_value_is_still_sent() {
        let mut request = Request::new(uuid::Uuid::new_v4(), "Ratios".to_string());
        request.url = "https://example.com/v1/ratios?a=1".to_string();
        request.params = vec![QueryParam {
            key: "a".to_string(),
            value: "2".to_string(),
            enabled: true,
            description: None,
        }];

        let resolved = build_resolved_request(&request, &|text: &str| text.to_string());
        assert_eq!(resolved.url, "https://example.com/v1/ratios?a=1&a=2");
    }

    /// The table below the address bar shows the same parameters the address bar
    /// holds, so sending must not put them in twice.
    #[test]
    fn a_parameter_already_in_the_url_is_not_sent_twice() {
        let mut request = Request::new(uuid::Uuid::new_v4(), "Ratios".to_string());
        request.url = "https://example.com/v1/ratios?hello=world".to_string();
        request.params = vec![
            QueryParam {
                key: "hello".to_string(),
                value: "world".to_string(),
                enabled: true,
                description: None,
            },
            QueryParam {
                key: "page".to_string(),
                value: "2".to_string(),
                enabled: true,
                description: None,
            },
        ];

        let resolved = build_resolved_request(&request, &|text: &str| text.to_string());
        assert_eq!(
            resolved.url, "https://example.com/v1/ratios?hello=world&page=2",
            "the one the address bar already carries stays as it is, the other is added"
        );
    }
    use crate::request::{ApiKeyPlacement, Header, HttpMethod, QueryParam};
    use uuid::Uuid;

    fn identity(text: &str) -> String {
        text.to_string()
    }

    /// A request with every auto-generated default header disabled, so
    /// existing assertions about `resolved.headers` don't have to account for
    /// them -- the auto-header behavior itself is covered by the dedicated
    /// `auto_header_*` tests below.
    fn base_request() -> Request {
        let mut request = Request::new(Uuid::new_v4(), "Test".to_string());
        request.settings.disabled_auto_headers = AUTO_HEADER_DEFAULTS
            .iter()
            .map(|(key, _)| key.to_string())
            .collect();
        request
    }

    #[test]
    fn the_host_is_picked_out_of_whatever_shape_the_url_has() {
        for (url, expected) in [
            ("https://api.example.com/v1/things", Some("api.example.com")),
            ("https://api.example.com:8443/v1", Some("api.example.com")),
            (
                "http://user:secret@api.example.com/v1",
                Some("api.example.com"),
            ),
            ("https://[2001:db8::1]:443/v1", Some("2001:db8::1")),
            ("api.example.com/v1", Some("api.example.com")),
            ("https:///v1/things", None),
            ("", None),
        ] {
            assert_eq!(
                host_of(url).as_deref(),
                expected,
                "the host of {url} was read wrongly, and a lookup of the wrong name \
                 is a timing that means nothing"
            );
        }
    }

    /// A parameter named after a place in the path fills that place. The value he
    /// types has to reach the server as part of the path, not as a query he did
    /// not ask for.
    #[test]
    fn a_parameter_named_in_the_path_fills_that_place_in_the_path() {
        let mut request = base_request();
        request.url =
            "https://api.example.com/v1/instruments/:instrument_id/balance-sheet".to_string();
        request.params = vec![
            QueryParam {
                key: ":instrument_id".into(),
                value: "6408".into(),
                enabled: true,
                description: None,
            },
            QueryParam {
                key: "period".into(),
                value: "annual".into(),
                enabled: true,
                description: None,
            },
        ];

        let resolved = build_resolved_request(&request, &identity);

        assert_eq!(
            resolved.url, "https://api.example.com/v1/instruments/6408/balance-sheet?period=annual",
            "the parameter belongs in the path, and only what is left belongs on \
             the query string"
        );
    }

    #[test]
    fn a_place_in_the_path_is_filled_whether_or_not_the_parameter_carries_the_colon() {
        for key in [":instrument_id", "instrument_id"] {
            let mut request = base_request();
            request.url = "https://api.example.com/v1/instruments/:instrument_id".to_string();
            request.params = vec![QueryParam {
                key: key.into(),
                value: "6408".into(),
                enabled: true,
                description: None,
            }];

            let resolved = build_resolved_request(&request, &identity);

            assert_eq!(
                resolved.url, "https://api.example.com/v1/instruments/6408",
                "a parameter written {key} has to fill :instrument_id"
            );
        }
    }

    #[test]
    fn a_place_in_the_path_with_no_parameter_is_left_where_it_can_be_seen() {
        let mut request = base_request();
        request.url =
            "https://api.example.com/v1/instruments/:instrument_id/balance-sheet".to_string();

        let resolved = build_resolved_request(&request, &identity);

        assert_eq!(
            resolved.url, "https://api.example.com/v1/instruments/:instrument_id/balance-sheet",
            "an unfilled place stays visible rather than being quietly dropped"
        );
    }

    #[test]
    fn a_colon_that_is_not_a_place_in_the_path_is_left_alone() {
        let mut request = base_request();
        request.url = "https://api.example.com:8443/v1/things?filter=a:b".to_string();
        request.params = vec![QueryParam {
            key: "port".into(),
            value: "9999".into(),
            enabled: true,
            description: None,
        }];

        let resolved = build_resolved_request(&request, &identity);

        assert_eq!(
            resolved.url, "https://api.example.com:8443/v1/things?filter=a:b&port=9999",
            "a port and a colon inside a query value are not places to fill"
        );
    }

    #[test]
    fn a_value_put_in_the_path_is_encoded_for_a_path() {
        let mut request = base_request();
        request.url = "https://api.example.com/v1/things/:name".to_string();
        request.params = vec![QueryParam {
            key: ":name".into(),
            value: "a b/c".into(),
            enabled: true,
            description: None,
        }];

        let resolved = build_resolved_request(&request, &identity);

        assert_eq!(
            resolved.url, "https://api.example.com/v1/things/a%20b%2Fc",
            "a value with a space or a slash must not change which path is asked for"
        );
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

    #[test]
    fn every_auto_header_default_is_enabled_when_none_are_disabled() {
        let mut request = base_request();
        request.settings.disabled_auto_headers = Vec::new();
        let resolved = build_resolved_request(&request, &identity);
        for (key, value) in AUTO_HEADER_DEFAULTS {
            assert_eq!(
                resolved
                    .headers
                    .iter()
                    .find(|(existing_key, _)| existing_key == key)
                    .map(|(_, existing_value)| existing_value.as_str()),
                Some(*value),
                "{key} should be sent with its default value"
            );
        }
    }

    #[test]
    fn a_disabled_auto_header_is_not_sent() {
        let mut request = base_request();
        request.settings.disabled_auto_headers = vec!["User-Agent".to_string()];
        let resolved = build_resolved_request(&request, &identity);
        assert!(
            !resolved
                .headers
                .iter()
                .any(|(key, _)| key.eq_ignore_ascii_case("User-Agent"))
        );
        assert!(
            resolved
                .headers
                .iter()
                .any(|(key, _)| key.eq_ignore_ascii_case("Accept")),
            "disabling one default must not disable the others"
        );
    }

    #[test]
    fn a_users_own_header_overrides_the_auto_generated_default_of_the_same_name() {
        let mut request = base_request();
        request.settings.disabled_auto_headers = Vec::new();
        request.headers = vec![Header {
            key: " user-agent ".into(),
            value: "MyCustomAgent/2.0".into(),
            enabled: true,
            description: None,
        }];
        let resolved = build_resolved_request(&request, &identity);
        let user_agent_headers: Vec<_> = resolved
            .headers
            .iter()
            .filter(|(key, _)| key.trim().eq_ignore_ascii_case("user-agent"))
            .collect();
        assert_eq!(
            user_agent_headers,
            vec![&(" user-agent ".to_string(), "MyCustomAgent/2.0".to_string())],
            "the user's own header (even with surrounding whitespace) must win, with no duplicate added"
        );
    }

    #[test]
    fn auto_headers_are_included_in_the_aws_sigv4_signed_headers_set() {
        let mut request = base_request();
        request.settings.disabled_auto_headers = Vec::new();
        request.url = "https://example.amazonaws.com/data".to_string();
        request.auth = AuthConfig::AwsSigV4(crate::aws_sigv4::AwsSigV4Config {
            access_key: "AKIDEXAMPLE".to_string(),
            secret_key: "secret".to_string(),
            session_token: String::new(),
            region: "us-east-1".to_string(),
            service: "execute-api".to_string(),
        });
        let resolved = build_resolved_request(&request, &identity);
        let authorization = resolved
            .headers
            .iter()
            .find(|(key, _)| key == "Authorization")
            .map(|(_, value)| value.clone())
            .expect("SigV4 auth should add an Authorization header");
        assert!(
            authorization.contains("accept")
                && authorization.contains("cache-control")
                && authorization.contains("user-agent"),
            "auto-generated headers must be part of the signed headers set \
             (added before signing), not appended afterward unsigned: {authorization}"
        );
    }

    /// A body written as a table of pairs is sent as those pairs, which is
    /// what the content type it now carries says it is.
    #[test]
    fn a_table_of_pairs_is_sent_as_a_form_body_with_the_type_that_names_it() {
        let mut request = base_request();
        request.body = RequestBody::UrlEncoded(vec![
            ("grant_type".to_string(), "password".to_string()),
            ("username".to_string(), "alice".to_string()),
        ]);

        let resolved = build_resolved_request(&request, &identity);

        assert_eq!(
            resolved.body.as_deref().map(String::from_utf8_lossy),
            Some("grant_type=password&username=alice".into())
        );
        assert_eq!(
            resolved.headers,
            vec![(
                "Content-Type".to_string(),
                "application/x-www-form-urlencoded".to_string()
            )]
        );
    }

    /// A variable in a pair is resolved, and what it resolves to is escaped
    /// afterwards rather than before: a value holding a space or an ampersand
    /// would otherwise end the pair early and start another.
    #[test]
    fn a_pair_carrying_a_variable_is_resolved_and_then_escaped() {
        let mut request = base_request();
        request.body =
            RequestBody::UrlEncoded(vec![("q".to_string(), "{{search-words}}".to_string())]);

        let resolved = build_resolved_request(&request, &|text: &str| {
            if text == "{{search-words}}" {
                "one two&three".to_string()
            } else {
                text.to_string()
            }
        });

        assert_eq!(
            resolved.body.as_deref().map(String::from_utf8_lossy),
            Some("q=one%20two%26three".into()),
            "the ampersand inside the value cannot be allowed to separate pairs"
        );
    }

    /// A pair with no name is not a pair, and a table holding nothing else is
    /// not a body -- an empty form body would make a GET carry a content type
    /// for nothing.
    #[test]
    fn a_table_with_nothing_named_in_it_is_not_a_body() {
        let mut request = base_request();
        request.body = RequestBody::UrlEncoded(vec![(String::new(), "1".to_string())]);

        let resolved = build_resolved_request(&request, &identity);

        assert_eq!(resolved.body, None);
        assert!(resolved.headers.is_empty(), "{:?}", resolved.headers);
    }

    /// A GraphQL request is one JSON object holding the query and the
    /// variables, and the variables are the object the reader wrote rather
    /// than a string holding it.
    #[test]
    fn a_graphql_body_is_sent_as_json_with_its_variables_as_an_object() {
        let mut request = base_request();
        request.body = RequestBody::GraphQl {
            query: "query Ratios($id: ID!) { ratios(id: $id) { pe } }".to_string(),
            variables: r#"{"id": "{{instrument-id}}"}"#.to_string(),
        };

        let resolved = build_resolved_request(&request, &|text: &str| {
            text.replace("{{instrument-id}}", "8830")
        });

        let sent: serde_json::Value =
            serde_json::from_slice(resolved.body.as_deref().expect("a graphql body is sent"))
                .expect("what is sent is json");
        assert_eq!(
            sent["query"],
            "query Ratios($id: ID!) { ratios(id: $id) { pe } }"
        );
        assert_eq!(
            sent["variables"],
            serde_json::json!({"id": "8830"}),
            "an object, not the text of one, and with the variable resolved"
        );
        assert_eq!(
            resolved.headers,
            vec![("Content-Type".to_string(), "application/json".to_string())]
        );
    }

    /// Variables that are not an object are left out rather than sent as
    /// text: every server would refuse a string there, and the query alone
    /// still reaches one that can answer it.
    #[test]
    fn graphql_variables_that_are_not_an_object_are_left_out() {
        for written in ["", "not json at all", "[1, 2]", "\"a string\""] {
            let mut request = base_request();
            request.body = RequestBody::GraphQl {
                query: "{ me }".to_string(),
                variables: written.to_string(),
            };

            let resolved = build_resolved_request(&request, &identity);
            let sent: serde_json::Value =
                serde_json::from_slice(resolved.body.as_deref().expect("a graphql body is sent"))
                    .expect("what is sent is json");
            assert_eq!(sent["query"], "{ me }", "{written:?}");
            assert_eq!(sent.get("variables"), None, "{written:?}");
        }
    }

    /// A GraphQL body with no query is nothing to send.
    #[test]
    fn a_graphql_body_with_no_query_is_not_a_body() {
        let mut request = base_request();
        request.body = RequestBody::GraphQl {
            query: "   ".to_string(),
            variables: r#"{"id": 1}"#.to_string(),
        };

        assert_eq!(build_resolved_request(&request, &identity).body, None);
    }

    /// A content type the reader wrote by hand wins: they may be sending a
    /// form body to a server that insists on a vendor type for it.
    #[test]
    fn a_content_type_written_by_hand_is_not_replaced() {
        let mut request = base_request();
        request.headers = vec![Header {
            key: "content-type".to_string(),
            value: "application/vnd.example+x-www-form-urlencoded".to_string(),
            enabled: true,
            description: None,
        }];
        request.body = RequestBody::UrlEncoded(vec![("a".to_string(), "1".to_string())]);

        let resolved = build_resolved_request(&request, &identity);

        assert_eq!(
            resolved.headers,
            vec![(
                "content-type".to_string(),
                "application/vnd.example+x-www-form-urlencoded".to_string()
            )],
            "one content type, and it is theirs"
        );
    }

    /// An access token is as much a place for a variable as any other
    /// credential -- a token kept in the environment is the ordinary way to
    /// keep it out of a saved request.
    #[test]
    fn an_oauth2_access_token_written_as_a_variable_is_resolved() {
        let mut request = base_request();
        request.auth = AuthConfig::OAuth2(crate::oauth2::OAuth2Config {
            access_token: "{{access-token}}".to_string(),
            ..Default::default()
        });

        let resolved = build_resolved_request(&request, &|text: &str| {
            text.replace("{{access-token}}", "ya29.a0")
        });

        assert_eq!(
            resolved.headers,
            vec![("Authorization".to_string(), "Bearer ya29.a0".to_string())],
            "the braces must not reach the wire"
        );
    }

    /// A file body is the file: what is on disk, byte for byte, under the type
    /// a server reads as "bytes I am not to interpret".
    #[test]
    fn a_binary_body_sends_the_file_itself() {
        let holding = tempfile::tempdir().expect("a directory to write into");
        let path = holding.path().join("upload.bin");
        let bytes: Vec<u8> = (0u8..=255).collect();
        std::fs::write(&path, &bytes).expect("the file to be written");

        let mut request = base_request();
        request.body = RequestBody::Binary { path };
        let files = smol::block_on(FilesForABody::read_them(files_a_body_needs(&request.body)));

        let resolved = build_resolved_request_with_files(&request, &identity, &files);

        assert!(files.unreadable().is_empty(), "{:?}", files.unreadable());
        assert_eq!(resolved.body.as_deref(), Some(bytes.as_slice()));
        assert_eq!(
            resolved.headers,
            vec![(
                "Content-Type".to_string(),
                "application/octet-stream".to_string()
            )]
        );
    }

    /// A form body is multipart: every part named, a file part carrying the
    /// file's own name so the server can store it under something better than
    /// the field name, and the boundary the header names separating them.
    #[test]
    fn a_form_body_carries_its_text_and_its_file() {
        let holding = tempfile::tempdir().expect("a directory to write into");
        let path = holding.path().join("portrait.png");
        std::fs::write(&path, b"\x89PNG\r\n\x1a\n").expect("the file to be written");

        let mut request = base_request();
        request.body = RequestBody::FormData(vec![
            crate::request::FormDataField {
                key: "caption".to_string(),
                value: crate::request::FormDataValue::Text("{{who}} at work".to_string()),
                enabled: true,
            },
            crate::request::FormDataField {
                key: "avatar".to_string(),
                value: crate::request::FormDataValue::File(path.clone()),
                enabled: true,
            },
            crate::request::FormDataField {
                key: "unwanted".to_string(),
                value: crate::request::FormDataValue::Text("left out".to_string()),
                enabled: false,
            },
        ]);
        let files = smol::block_on(FilesForABody::read_them(files_a_body_needs(&request.body)));

        let resolved = build_resolved_request_with_files(
            &request,
            &|text: &str| text.replace("{{who}}", "Ada"),
            &files,
        );

        let sent = String::from_utf8_lossy(resolved.body.as_deref().expect("a form body is sent"))
            .into_owned();
        assert!(
            sent.contains(
                "Content-Disposition: form-data; name=\"caption\"\r\n\r\nAda at work\r\n"
            ),
            "{sent}"
        );
        assert!(
            sent.contains(
                "Content-Disposition: form-data; name=\"avatar\"; filename=\"portrait.png\"\r\n\
                 Content-Type: application/octet-stream\r\n\r\n"
            ),
            "{sent}"
        );
        let body = resolved.body.as_deref().expect("a form body is sent");
        let on_disk = std::fs::read(&path).expect("the file to be read back");
        assert!(
            body.windows(on_disk.len())
                .any(|window| window == on_disk.as_slice()),
            "the part carries the file itself, and a file is not text"
        );
        assert!(
            !sent.contains("unwanted"),
            "a field the reader switched off is not a part: {sent}"
        );
    }

    /// The boundary is generated during the build, so the header cannot be
    /// written from anywhere else -- and if the two ever disagree the parts
    /// become body text no server will look at.
    #[test]
    fn the_boundary_in_the_header_is_the_boundary_in_the_body() {
        let mut request = base_request();
        request.body = RequestBody::FormData(vec![crate::request::FormDataField {
            key: "a".to_string(),
            value: crate::request::FormDataValue::Text("1".to_string()),
            enabled: true,
        }]);

        let resolved = build_resolved_request(&request, &identity);

        let (_, content_type) = resolved
            .headers
            .iter()
            .find(|(key, _)| key.eq_ignore_ascii_case("content-type"))
            .expect("a multipart body names its boundary in the header");
        let named = content_type
            .split_once("boundary=")
            .map(|(_, boundary)| boundary.to_string())
            .expect("the header carries a boundary");
        let sent = String::from_utf8_lossy(resolved.body.as_deref().expect("a form body is sent"))
            .into_owned();
        assert!(sent.starts_with(&format!("--{named}\r\n")), "{sent}");
        assert!(sent.ends_with(&format!("--{named}--\r\n")), "{sent}");
    }

    /// Every body kind leaves a hand-written content type alone -- except
    /// multipart, whose header holds the boundary this build has just made up
    /// and nobody else can know.
    #[test]
    fn a_hand_written_content_type_loses_only_to_multipart() {
        let holding = tempfile::tempdir().expect("a directory to write into");
        let path = holding.path().join("upload.bin");
        std::fs::write(&path, b"payload").expect("the file to be written");
        let theirs = "application/vnd.example+what-they-said";

        for body in [
            RequestBody::Raw {
                content_type: crate::request::RawBodyContentType::Json,
                text: "{}".to_string(),
            },
            RequestBody::UrlEncoded(vec![("a".to_string(), "1".to_string())]),
            RequestBody::GraphQl {
                query: "{ me }".to_string(),
                variables: String::new(),
            },
            RequestBody::Binary { path },
        ] {
            let mut request = base_request();
            request.headers = vec![Header {
                key: "Content-Type".to_string(),
                value: theirs.to_string(),
                enabled: true,
                description: None,
            }];
            request.body = body;
            let files = smol::block_on(FilesForABody::read_them(files_a_body_needs(&request.body)));

            let resolved = build_resolved_request_with_files(&request, &identity, &files);

            assert_eq!(
                resolved.headers,
                vec![("Content-Type".to_string(), theirs.to_string())],
                "{:?} has to keep the reader's own type",
                request.body
            );
        }

        let mut request = base_request();
        request.headers = vec![Header {
            key: "Content-Type".to_string(),
            value: "multipart/form-data; boundary=whatever-they-wrote".to_string(),
            enabled: true,
            description: None,
        }];
        request.body = RequestBody::FormData(vec![crate::request::FormDataField {
            key: "a".to_string(),
            value: crate::request::FormDataValue::Text("1".to_string()),
            enabled: true,
        }]);

        let resolved = build_resolved_request(&request, &identity);

        assert_eq!(resolved.headers.len(), 1, "{:?}", resolved.headers);
        let (_, content_type) = &resolved.headers[0];
        assert!(
            !content_type.contains("whatever-they-wrote"),
            "the parts are separated by the boundary the build made, so the header \
             cannot keep naming theirs: {content_type}"
        );
        let named = content_type
            .split_once("boundary=")
            .map(|(_, boundary)| boundary.to_string())
            .expect("the header carries a boundary");
        let sent = String::from_utf8_lossy(resolved.body.as_deref().expect("a form body is sent"))
            .into_owned();
        assert!(sent.starts_with(&format!("--{named}\r\n")), "{sent}");
    }

    /// A file that cannot be read stops the body it belonged to, and says so
    /// through `unreadable` -- a form missing one part, or an empty PUT, would
    /// reach the server as an answer the reader never gave.
    #[test]
    fn a_file_that_cannot_be_read_stops_the_body_and_is_reported() {
        let holding = tempfile::tempdir().expect("a directory to write into");
        let missing = holding.path().join("never-written.bin");
        let there = holding.path().join("caption.txt");
        std::fs::write(&there, b"read me").expect("the file to be written");

        for body in [
            RequestBody::Binary {
                path: missing.clone(),
            },
            RequestBody::FormData(vec![
                crate::request::FormDataField {
                    key: "caption".to_string(),
                    value: crate::request::FormDataValue::Text("a picture".to_string()),
                    enabled: true,
                },
                crate::request::FormDataField {
                    key: "avatar".to_string(),
                    value: crate::request::FormDataValue::File(missing.clone()),
                    enabled: true,
                },
            ]),
        ] {
            let mut request = base_request();
            request.body = body;
            let files = smol::block_on(FilesForABody::read_them(files_a_body_needs(&request.body)));

            let resolved = build_resolved_request_with_files(&request, &identity, &files);

            assert_eq!(resolved.body, None, "{:?}", request.body);
            assert!(resolved.headers.is_empty(), "{:?}", resolved.headers);
            assert_eq!(
                files.unreadable().len(),
                1,
                "the caller has to be able to say which file: {:?}",
                files.unreadable()
            );
            assert_eq!(files.unreadable()[0].0, missing);
        }
    }

    /// A body with no file in it needs nothing read, so Send does not go to disk
    /// for a request that never mentioned one.
    #[test]
    fn a_body_with_no_file_in_it_asks_for_nothing() {
        assert!(files_a_body_needs(&RequestBody::None).is_empty());
        assert!(
            files_a_body_needs(&RequestBody::Binary {
                path: std::path::PathBuf::new()
            })
            .is_empty(),
            "no file chosen yet is not a file to read"
        );
        assert!(
            files_a_body_needs(&RequestBody::FormData(vec![
                crate::request::FormDataField {
                    key: "caption".to_string(),
                    value: crate::request::FormDataValue::Text("text".to_string()),
                    enabled: true,
                },
                crate::request::FormDataField {
                    key: "avatar".to_string(),
                    value: crate::request::FormDataValue::File(std::path::PathBuf::from(
                        "/tmp/off.bin"
                    )),
                    enabled: false,
                },
            ]))
            .is_empty(),
            "a field switched off is not sent, so its file is not read either"
        );
    }
}
