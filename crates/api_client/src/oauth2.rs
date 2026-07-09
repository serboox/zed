use anyhow::{Result, bail};
use base64::Engine as _;
use rand::RngCore;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub enum OAuth2GrantType {
    /// Needs a user-facing browser round trip, so it's the one grant type
    /// that needs a redirect-capture mechanism (see `redirect_capture.rs`).
    AuthorizationCodePkce,
    /// A pure machine-to-machine POST, no browser or redirect involved --
    /// picked as the default since it's the simpler of the two to configure.
    #[default]
    ClientCredentials,
}

/// An OAuth 2.0 auth configuration. `client_secret`/`access_token`/
/// `refresh_token` are plaintext only in memory, exactly like
/// `AuthConfig::Basic`'s `password` -- the UI-side store redacts all three
/// before the collection tree is written to disk and routes them through
/// `CredentialsProvider` instead (see `api_client_ui::store`).
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct OAuth2Config {
    #[serde(default)]
    pub grant_type: OAuth2GrantType,
    #[serde(default)]
    pub auth_url: String,
    #[serde(default)]
    pub token_url: String,
    #[serde(default)]
    pub client_id: String,
    #[serde(default)]
    pub client_secret: String,
    #[serde(default)]
    pub scope: String,
    #[serde(default)]
    pub access_token: String,
    #[serde(default)]
    pub refresh_token: String,
}

/// A pending Authorization Code + PKCE exchange's request-bound values, kept
/// separate from `OAuth2Config` since they are single-use and never
/// persisted (unlike the config, which is saved with the request).
pub struct PkcePendingAuthorization {
    pub verifier: String,
    pub state: String,
}

/// Generates a PKCE code verifier: a random string of `length` unreserved
/// URL-safe characters, within RFC 7636's required 43-128 range. Also doubles
/// as the random `state` value generator (same shape, different purpose).
fn random_url_safe_string(length: usize) -> String {
    const ALPHABET: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-._~";
    let mut bytes = vec![0u8; length];
    rand::rng().fill_bytes(&mut bytes);
    bytes
        .iter()
        .map(|byte| ALPHABET[*byte as usize % ALPHABET.len()] as char)
        .collect()
}

pub fn begin_pkce_authorization() -> PkcePendingAuthorization {
    PkcePendingAuthorization {
        verifier: random_url_safe_string(64),
        state: random_url_safe_string(32),
    }
}

/// RFC 7636 `S256` code challenge: base64url-no-pad of SHA-256(verifier).
pub fn pkce_challenge_s256(verifier: &str) -> String {
    let digest = Sha256::digest(verifier.as_bytes());
    base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(digest)
}

/// Builds the browser-facing authorization URL for the Authorization Code +
/// PKCE flow. `config.auth_url` is used as-is (query params appended with
/// `?`/`&` as appropriate, so a URL that already carries its own query
/// string still works).
pub fn build_authorization_url(
    config: &OAuth2Config,
    redirect_uri: &str,
    pending: &PkcePendingAuthorization,
) -> String {
    let separator = if config.auth_url.contains('?') {
        '&'
    } else {
        '?'
    };
    let challenge = pkce_challenge_s256(&pending.verifier);
    format!(
        "{url}{sep}response_type=code&client_id={client_id}&redirect_uri={redirect_uri}&state={state}&code_challenge={challenge}&code_challenge_method=S256{scope}",
        url = config.auth_url,
        sep = separator,
        client_id = urlencoding::encode(&config.client_id),
        redirect_uri = urlencoding::encode(redirect_uri),
        state = urlencoding::encode(&pending.state),
        challenge = urlencoding::encode(&challenge),
        scope = if config.scope.is_empty() {
            String::new()
        } else {
            format!("&scope={}", urlencoding::encode(&config.scope))
        },
    )
}

/// Extracts the `code` query parameter from the raw request line/query
/// string a loopback redirect listener receives (e.g.
/// `GET /callback?code=abc&state=xyz HTTP/1.1` or just `code=abc&state=xyz`),
/// and verifies `state` matches `expected_state` -- rejecting a mismatched
/// state is the whole point of using one, so this is not optional.
pub fn parse_authorization_redirect(raw: &str, expected_state: &str) -> Result<String> {
    let query_start = raw.find('?').map(|index| index + 1).unwrap_or(0);
    let query_end = raw[query_start..]
        .find(|c: char| c.is_whitespace())
        .map(|offset| query_start + offset)
        .unwrap_or(raw.len());
    let query = &raw[query_start..query_end];

    let mut code = None;
    let mut state = None;
    for pair in query.split('&') {
        let Some((key, value)) = pair.split_once('=') else {
            continue;
        };
        match key {
            "code" => code = Some(value.to_string()),
            "state" => state = Some(value.to_string()),
            "error" => bail!("authorization server returned an error: {value}"),
            _ => {}
        }
    }

    let Some(state) = state else {
        bail!("redirect had no `state` parameter");
    };
    if state != expected_state {
        bail!("redirect `state` did not match the value sent in the authorization request");
    }
    code.ok_or_else(|| anyhow::anyhow!("redirect had no `code` parameter"))
}

/// A token-endpoint POST request, fully built and ready to send -- the
/// caller (which has network access) is responsible for actually executing
/// it as `application/x-www-form-urlencoded`.
pub struct TokenRequest {
    pub url: String,
    pub params: Vec<(String, String)>,
}

pub fn client_credentials_token_request(config: &OAuth2Config) -> TokenRequest {
    let mut params = vec![
        ("grant_type".to_string(), "client_credentials".to_string()),
        ("client_id".to_string(), config.client_id.clone()),
        ("client_secret".to_string(), config.client_secret.clone()),
    ];
    if !config.scope.is_empty() {
        params.push(("scope".to_string(), config.scope.clone()));
    }
    TokenRequest {
        url: config.token_url.clone(),
        params,
    }
}

pub fn authorization_code_token_request(
    config: &OAuth2Config,
    code: &str,
    verifier: &str,
    redirect_uri: &str,
) -> TokenRequest {
    let params = vec![
        ("grant_type".to_string(), "authorization_code".to_string()),
        ("code".to_string(), code.to_string()),
        ("redirect_uri".to_string(), redirect_uri.to_string()),
        ("client_id".to_string(), config.client_id.clone()),
        ("client_secret".to_string(), config.client_secret.clone()),
        ("code_verifier".to_string(), verifier.to_string()),
    ];
    TokenRequest {
        url: config.token_url.clone(),
        params,
    }
}

pub fn refresh_token_request(config: &OAuth2Config) -> TokenRequest {
    let params = vec![
        ("grant_type".to_string(), "refresh_token".to_string()),
        ("refresh_token".to_string(), config.refresh_token.clone()),
        ("client_id".to_string(), config.client_id.clone()),
        ("client_secret".to_string(), config.client_secret.clone()),
    ];
    TokenRequest {
        url: config.token_url.clone(),
        params,
    }
}

#[derive(Debug, Clone, Deserialize)]
pub struct TokenResponse {
    pub access_token: String,
    #[serde(default)]
    pub refresh_token: Option<String>,
    #[serde(default)]
    pub expires_in: Option<u64>,
}

pub fn parse_token_response(json: &str) -> Result<TokenResponse> {
    serde_json::from_str(json)
        .map_err(|error| anyhow::anyhow!("could not parse token response: {error}"))
}

/// POSTs `request` as `application/x-www-form-urlencoded` (the OAuth 2.0
/// token endpoint's required content type) and parses the JSON response.
/// Runs on the shared network Tokio runtime -- see `network_runtime` for
/// why that's necessary.
pub async fn exchange_token(
    client: &reqwest::Client,
    request: &TokenRequest,
) -> Result<TokenResponse> {
    let builder = client.post(&request.url).form(&request.params);
    let (status, body) = crate::network_runtime::on_network_runtime(async move {
        let response = builder.send().await?;
        let status = response.status();
        let body = response.text().await?;
        anyhow::Ok((status, body))
    })
    .await?;
    if !status.is_success() {
        bail!("token endpoint returned {status}: {body}");
    }
    parse_token_response(&body)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_pkce_verifier_is_within_the_rfc_7636_length_range() {
        let pending = begin_pkce_authorization();
        assert!(pending.verifier.len() >= 43 && pending.verifier.len() <= 128);
        assert!(
            pending
                .verifier
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || "-._~".contains(c))
        );
    }

    #[test]
    fn two_generated_verifiers_are_not_the_same() {
        let a = begin_pkce_authorization();
        let b = begin_pkce_authorization();
        assert_ne!(a.verifier, b.verifier);
        assert_ne!(a.state, b.state);
    }

    #[test]
    fn the_s256_challenge_matches_a_known_reference_value() {
        // From RFC 7636 appendix B.
        let verifier = "dBjftJeZ4CVP-mB92K27uhbUJU1p1r_wW1gFWFOEjXk";
        let challenge = pkce_challenge_s256(verifier);
        assert_eq!(challenge, "E9Melhoa2OwvFrEMTJguCHaoeK1t8URWbuGJSstw-cM");
    }

    #[test]
    fn the_authorization_url_includes_client_id_redirect_and_pkce_challenge() {
        let config = OAuth2Config {
            auth_url: "https://auth.example.com/authorize".to_string(),
            client_id: "abc123".to_string(),
            scope: "read write".to_string(),
            ..Default::default()
        };
        let pending = PkcePendingAuthorization {
            verifier: "verifier-value".to_string(),
            state: "state-value".to_string(),
        };
        let url = build_authorization_url(&config, "http://127.0.0.1:9999/callback", &pending);
        assert!(url.starts_with("https://auth.example.com/authorize?"));
        assert!(url.contains("client_id=abc123"));
        assert!(url.contains("redirect_uri=http%3A%2F%2F127.0.0.1%3A9999%2Fcallback"));
        assert!(url.contains("state=state-value"));
        assert!(url.contains(&format!(
            "code_challenge={}",
            pkce_challenge_s256("verifier-value")
        )));
        assert!(url.contains("scope=read%20write"));
    }

    #[test]
    fn the_authorization_url_appends_with_ampersand_when_the_auth_url_already_has_a_query_string() {
        let config = OAuth2Config {
            auth_url: "https://auth.example.com/authorize?tenant=acme".to_string(),
            ..Default::default()
        };
        let pending = PkcePendingAuthorization {
            verifier: "v".repeat(43),
            state: "s".to_string(),
        };
        let url = build_authorization_url(&config, "http://127.0.0.1/callback", &pending);
        assert!(url.starts_with("https://auth.example.com/authorize?tenant=acme&"));
    }

    #[test]
    fn a_valid_redirect_request_line_yields_the_code() {
        let raw = "GET /callback?code=abc123&state=xyz HTTP/1.1\r\n";
        let code = parse_authorization_redirect(raw, "xyz").unwrap();
        assert_eq!(code, "abc123");
    }

    #[test]
    fn a_bare_query_string_without_a_request_line_also_works() {
        let code = parse_authorization_redirect("code=abc123&state=xyz", "xyz").unwrap();
        assert_eq!(code, "abc123");
    }

    #[test]
    fn a_mismatched_state_is_rejected() {
        let raw = "GET /callback?code=abc123&state=wrong HTTP/1.1\r\n";
        assert!(parse_authorization_redirect(raw, "expected").is_err());
    }

    #[test]
    fn an_error_query_parameter_is_surfaced_as_an_error_not_a_missing_code() {
        let raw = "GET /callback?error=access_denied&state=xyz HTTP/1.1\r\n";
        let error = parse_authorization_redirect(raw, "xyz").unwrap_err();
        assert!(error.to_string().contains("access_denied"));
    }

    #[test]
    fn a_missing_code_is_an_error() {
        let raw = "GET /callback?state=xyz HTTP/1.1\r\n";
        assert!(parse_authorization_redirect(raw, "xyz").is_err());
    }

    #[test]
    fn client_credentials_request_includes_grant_type_and_credentials() {
        let config = OAuth2Config {
            token_url: "https://auth.example.com/token".to_string(),
            client_id: "id".to_string(),
            client_secret: "secret".to_string(),
            ..Default::default()
        };
        let request = client_credentials_token_request(&config);
        assert_eq!(request.url, "https://auth.example.com/token");
        assert!(
            request
                .params
                .contains(&("grant_type".to_string(), "client_credentials".to_string()))
        );
        assert!(
            request
                .params
                .contains(&("client_secret".to_string(), "secret".to_string()))
        );
    }

    #[test]
    fn authorization_code_request_includes_the_code_verifier_and_redirect_uri() {
        let config = OAuth2Config {
            token_url: "https://auth.example.com/token".to_string(),
            ..Default::default()
        };
        let request = authorization_code_token_request(
            &config,
            "the-code",
            "the-verifier",
            "http://127.0.0.1/callback",
        );
        assert!(
            request
                .params
                .contains(&("code".to_string(), "the-code".to_string()))
        );
        assert!(
            request
                .params
                .contains(&("code_verifier".to_string(), "the-verifier".to_string()))
        );
        assert!(request.params.contains(&(
            "redirect_uri".to_string(),
            "http://127.0.0.1/callback".to_string()
        )));
    }

    #[test]
    fn refresh_token_request_uses_the_refresh_token_grant_type() {
        let config = OAuth2Config {
            refresh_token: "the-refresh-token".to_string(),
            ..Default::default()
        };
        let request = refresh_token_request(&config);
        assert!(
            request
                .params
                .contains(&("grant_type".to_string(), "refresh_token".to_string()))
        );
        assert!(
            request
                .params
                .contains(&("refresh_token".to_string(), "the-refresh-token".to_string()))
        );
    }

    #[test]
    fn a_token_response_with_a_refresh_token_and_expiry_parses_correctly() {
        let json = r#"{"access_token":"abc","refresh_token":"def","expires_in":3600,"token_type":"Bearer"}"#;
        let response = parse_token_response(json).unwrap();
        assert_eq!(response.access_token, "abc");
        assert_eq!(response.refresh_token, Some("def".to_string()));
        assert_eq!(response.expires_in, Some(3600));
    }

    #[test]
    fn a_token_response_without_a_refresh_token_leaves_it_none() {
        let json = r#"{"access_token":"abc"}"#;
        let response = parse_token_response(json).unwrap();
        assert_eq!(response.refresh_token, None);
    }

    #[test]
    fn a_malformed_token_response_is_rejected_rather_than_panicking() {
        assert!(parse_token_response("not json").is_err());
    }
}
