use hmac::{Hmac, Mac};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

type HmacSha256 = Hmac<Sha256>;

/// AWS Signature Version 4 credentials. `secret_key`/`session_token` are
/// plaintext only in memory, exactly like every other `AuthConfig` secret
/// field -- the UI-side store redacts them before persisting and routes
/// them through `CredentialsProvider` instead.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct AwsSigV4Config {
    #[serde(default)]
    pub access_key: String,
    #[serde(default)]
    pub secret_key: String,
    #[serde(default)]
    pub region: String,
    #[serde(default)]
    pub service: String,
    /// Set for temporary (STS-issued) credentials; empty for long-lived
    /// IAM user keys, in which case no `X-Amz-Security-Token` header is added.
    #[serde(default)]
    pub session_token: String,
}

fn hex_sha256(bytes: &[u8]) -> String {
    hex_encode(&Sha256::digest(bytes))
}

fn hex_encode(bytes: &[u8]) -> String {
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}

fn hmac_sha256(key: &[u8], data: &[u8]) -> Vec<u8> {
    let mut mac = HmacSha256::new_from_slice(key).expect("HMAC accepts a key of any length");
    mac.update(data);
    mac.finalize().into_bytes().to_vec()
}

/// URI-encodes a single path segment/query value per AWS's specific
/// percent-encoding rules (RFC 3986 unreserved characters left alone,
/// everything else percent-encoded -- notably `/` IS encoded in query
/// values but must NOT be encoded in the canonical URI path itself, which
/// is why this takes an `encode_slash` flag instead of being one shared
/// helper for both positions).
fn aws_uri_encode(value: &str, encode_slash: bool) -> String {
    let mut encoded = String::with_capacity(value.len());
    for byte in value.bytes() {
        let c = byte as char;
        if c.is_ascii_alphanumeric() || matches!(c, '-' | '.' | '_' | '~') {
            encoded.push(c);
        } else if c == '/' && !encode_slash {
            encoded.push(c);
        } else {
            encoded.push_str(&format!("%{byte:02X}"));
        }
    }
    encoded
}

fn canonical_uri(path: &str) -> String {
    if path.is_empty() {
        return "/".to_string();
    }
    path.split('/')
        .map(|segment| aws_uri_encode(segment, true))
        .collect::<Vec<_>>()
        .join("/")
}

/// Canonicalizes a query string: sorts params by key (AWS's tie-break is
/// then by value, which `sort` on the encoded pair strings gives for free
/// since it compares key first, then value).
fn canonical_query_string(params: &[(String, String)]) -> String {
    let mut pairs: Vec<String> = params
        .iter()
        .map(|(key, value)| {
            format!(
                "{}={}",
                aws_uri_encode(key, true),
                aws_uri_encode(value, true)
            )
        })
        .collect();
    pairs.sort();
    pairs.join("&")
}

fn canonical_headers_and_signed_headers(headers: &[(String, String)]) -> (String, String) {
    let mut normalized: Vec<(String, String)> = headers
        .iter()
        .map(|(key, value)| (key.to_lowercase(), value.trim().to_string()))
        .collect();
    normalized.sort_by(|a, b| a.0.cmp(&b.0));

    let canonical_headers: String = normalized
        .iter()
        .map(|(key, value)| format!("{key}:{value}\n"))
        .collect();
    let signed_headers = normalized
        .iter()
        .map(|(key, _)| key.as_str())
        .collect::<Vec<_>>()
        .join(";");
    (canonical_headers, signed_headers)
}

/// The full SigV4-signed `Authorization` header value, plus the extra
/// headers (`X-Amz-Date`, and `X-Amz-Security-Token` for temporary
/// credentials) that must accompany it on the actual HTTP request --
/// signing does not modify `headers`/`params` in place, the caller is
/// responsible for merging these back in before sending.
pub struct SignedAuthorization {
    pub authorization_header: String,
    pub amz_date: String,
    pub security_token_header: Option<String>,
}

/// Signs a request per AWS Signature Version 4. `headers` must already
/// include every header that will actually be sent except `Authorization`
/// and `X-Amz-Date` themselves (most importantly `Host`, which SigV4
/// requires as a signed header) -- `amz_date` is generated internally so
/// signing and sending always agree on the timestamp.
pub fn sign_request(
    config: &AwsSigV4Config,
    method: &str,
    path: &str,
    query_params: &[(String, String)],
    headers: &[(String, String)],
    body: &[u8],
    amz_date: &str,
) -> SignedAuthorization {
    let date_stamp = &amz_date[0..8];

    let mut all_headers = headers.to_vec();
    all_headers.push(("x-amz-date".to_string(), amz_date.to_string()));
    if !config.session_token.is_empty() {
        all_headers.push((
            "x-amz-security-token".to_string(),
            config.session_token.clone(),
        ));
    }
    let (canonical_headers, signed_headers) = canonical_headers_and_signed_headers(&all_headers);

    let canonical_request = format!(
        "{method}\n{uri}\n{query}\n{headers}\n{signed_headers}\n{payload_hash}",
        method = method.to_uppercase(),
        uri = canonical_uri(path),
        query = canonical_query_string(query_params),
        headers = canonical_headers,
        payload_hash = hex_sha256(body),
    );

    let credential_scope = format!(
        "{date_stamp}/{}/{}/aws4_request",
        config.region, config.service
    );
    let string_to_sign = format!(
        "AWS4-HMAC-SHA256\n{amz_date}\n{credential_scope}\n{}",
        hex_sha256(canonical_request.as_bytes())
    );

    let key_date = hmac_sha256(
        format!("AWS4{}", config.secret_key).as_bytes(),
        date_stamp.as_bytes(),
    );
    let key_region = hmac_sha256(&key_date, config.region.as_bytes());
    let key_service = hmac_sha256(&key_region, config.service.as_bytes());
    let signing_key = hmac_sha256(&key_service, b"aws4_request");
    let signature = hex_encode(&hmac_sha256(&signing_key, string_to_sign.as_bytes()));

    let authorization_header = format!(
        "AWS4-HMAC-SHA256 Credential={}/{credential_scope}, SignedHeaders={signed_headers}, Signature={signature}",
        config.access_key,
    );

    SignedAuthorization {
        authorization_header,
        amz_date: amz_date.to_string(),
        security_token_header: (!config.session_token.is_empty())
            .then(|| config.session_token.clone()),
    }
}

/// Formats the current instant as SigV4's required `YYYYMMDDTHHMMSSZ`
/// timestamp. Takes `unix_seconds` rather than calling `SystemTime::now()`
/// itself, so signing stays a pure, deterministically-testable function.
pub fn format_amz_date(unix_seconds: u64) -> String {
    const SECONDS_PER_DAY: u64 = 86_400;
    let days_since_epoch = unix_seconds / SECONDS_PER_DAY;
    let seconds_of_day = unix_seconds % SECONDS_PER_DAY;
    let (hour, minute, second) = (
        seconds_of_day / 3600,
        (seconds_of_day / 60) % 60,
        seconds_of_day % 60,
    );

    let z = days_since_epoch as i64 + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = (z - era * 146_097) as u64;
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146_096) / 365;
    let y = yoe as i64 + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = if month <= 2 { y + 1 } else { y };

    format!("{year:04}{month:02}{day:02}T{hour:02}{minute:02}{second:02}Z")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn example_config() -> AwsSigV4Config {
        AwsSigV4Config {
            access_key: "AKIDEXAMPLE".to_string(),
            secret_key: "wJalrXUtnFEMI/K7MDENG/bPxRfiCYEXAMPLEKEY".to_string(),
            region: "us-east-1".to_string(),
            service: "service".to_string(),
            session_token: String::new(),
        }
    }

    #[test]
    fn signing_is_deterministic_for_the_same_inputs() {
        let config = example_config();
        let headers = vec![("host".to_string(), "example.amazonaws.com".to_string())];
        let a = sign_request(&config, "GET", "/", &[], &headers, b"", "20150830T123600Z");
        let b = sign_request(&config, "GET", "/", &[], &headers, b"", "20150830T123600Z");
        assert_eq!(a.authorization_header, b.authorization_header);
    }

    #[test]
    fn changing_the_secret_key_changes_the_signature() {
        let headers = vec![("host".to_string(), "example.amazonaws.com".to_string())];
        let mut config_a = example_config();
        let signature_a = sign_request(
            &config_a,
            "GET",
            "/",
            &[],
            &headers,
            b"",
            "20150830T123600Z",
        );
        config_a.secret_key = "different-secret-key".to_string();
        let signature_b = sign_request(
            &config_a,
            "GET",
            "/",
            &[],
            &headers,
            b"",
            "20150830T123600Z",
        );
        assert_ne!(
            signature_a.authorization_header,
            signature_b.authorization_header
        );
    }

    #[test]
    fn changing_the_body_changes_the_signature() {
        let config = example_config();
        let headers = vec![("host".to_string(), "example.amazonaws.com".to_string())];
        let a = sign_request(
            &config,
            "POST",
            "/",
            &[],
            &headers,
            b"{}",
            "20150830T123600Z",
        );
        let b = sign_request(
            &config,
            "POST",
            "/",
            &[],
            &headers,
            b"{\"changed\":true}",
            "20150830T123600Z",
        );
        assert_ne!(a.authorization_header, b.authorization_header);
    }

    #[test]
    fn the_authorization_header_carries_credential_scope_and_signed_headers() {
        let config = example_config();
        let headers = vec![
            ("host".to_string(), "example.amazonaws.com".to_string()),
            ("content-type".to_string(), "application/json".to_string()),
        ];
        let signed = sign_request(&config, "GET", "/", &[], &headers, b"", "20150830T123600Z");
        assert!(signed.authorization_header.starts_with(
            "AWS4-HMAC-SHA256 Credential=AKIDEXAMPLE/20150830/us-east-1/service/aws4_request"
        ));
        assert!(
            signed
                .authorization_header
                .contains("SignedHeaders=content-type;host;x-amz-date")
        );
        assert!(signed.authorization_header.contains("Signature="));
    }

    #[test]
    fn a_session_token_is_reflected_in_the_signed_headers_list() {
        let mut config = example_config();
        config.session_token = "the-session-token".to_string();
        let headers = vec![("host".to_string(), "example.amazonaws.com".to_string())];
        let signed = sign_request(&config, "GET", "/", &[], &headers, b"", "20150830T123600Z");
        assert!(signed.authorization_header.contains("x-amz-security-token"));
        assert_eq!(
            signed.security_token_header,
            Some("the-session-token".to_string())
        );
    }

    #[test]
    fn query_parameters_are_sorted_into_the_canonical_query_string() {
        let params = vec![
            ("b".to_string(), "2".to_string()),
            ("a".to_string(), "1".to_string()),
        ];
        assert_eq!(canonical_query_string(&params), "a=1&b=2");
    }

    #[test]
    fn a_forward_slash_in_a_query_value_is_percent_encoded() {
        let params = vec![("key".to_string(), "a/b".to_string())];
        assert_eq!(canonical_query_string(&params), "key=a%2Fb");
    }

    #[test]
    fn a_forward_slash_in_the_path_is_left_unencoded() {
        assert_eq!(canonical_uri("/a/b"), "/a/b");
    }

    #[test]
    fn an_empty_path_becomes_a_single_slash() {
        assert_eq!(canonical_uri(""), "/");
    }

    #[test]
    fn canonical_headers_are_lowercased_trimmed_and_sorted() {
        let headers = vec![
            ("Host".to_string(), " example.amazonaws.com ".to_string()),
            ("X-Amz-Date".to_string(), "20150830T123600Z".to_string()),
        ];
        let (canonical, signed) = canonical_headers_and_signed_headers(&headers);
        assert_eq!(
            canonical,
            "host:example.amazonaws.com\nx-amz-date:20150830T123600Z\n"
        );
        assert_eq!(signed, "host;x-amz-date");
    }

    #[test]
    fn format_amz_date_matches_a_known_reference_timestamp() {
        // 2015-08-30T12:36:00Z, the timestamp used throughout AWS's own
        // published SigV4 signing examples.
        assert_eq!(format_amz_date(1_440_938_160), "20150830T123600Z");
    }
}
