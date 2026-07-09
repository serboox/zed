use hmac::{Hmac, Mac};
use sha2::{Sha256, Sha384, Sha512};

use crate::request::{JwtAlgorithm, JwtAuthConfig};

fn base64_url_encode(bytes: &[u8]) -> String {
    base64::Engine::encode(&base64::engine::general_purpose::URL_SAFE_NO_PAD, bytes)
}

fn hmac_sha256(secret: &[u8], signing_input: &[u8]) -> Option<Vec<u8>> {
    let mut mac = Hmac::<Sha256>::new_from_slice(secret).ok()?;
    mac.update(signing_input);
    Some(mac.finalize().into_bytes().to_vec())
}

fn hmac_sha384(secret: &[u8], signing_input: &[u8]) -> Option<Vec<u8>> {
    let mut mac = Hmac::<Sha384>::new_from_slice(secret).ok()?;
    mac.update(signing_input);
    Some(mac.finalize().into_bytes().to_vec())
}

fn hmac_sha512(secret: &[u8], signing_input: &[u8]) -> Option<Vec<u8>> {
    let mut mac = Hmac::<Sha512>::new_from_slice(secret).ok()?;
    mac.update(signing_input);
    Some(mac.finalize().into_bytes().to_vec())
}

/// Signs `config`'s `payload` (JSON claims) into a compact JWT, matching
/// Postman's "jwt bearer" auth type. Only the `HS*` algorithms are supported
/// -- `RS*` needs an RSA private key, which Postman's own config never
/// carries either (it signs client-side with a library, not by config alone),
/// so `RS*` requests are sent unsigned rather than pretending to sign them.
pub fn sign_jwt(config: &JwtAuthConfig) -> Option<String> {
    let secret_bytes = if config.is_secret_base64_encoded {
        base64::Engine::decode(&base64::engine::general_purpose::STANDARD, &config.secret).ok()?
    } else {
        config.secret.as_bytes().to_vec()
    };

    let header = serde_json::json!({ "alg": config.algorithm.as_str(), "typ": "JWT" });
    let header_b64 = base64_url_encode(&serde_json::to_vec(&header).ok()?);
    let payload_value: serde_json::Value = serde_json::from_str(&config.payload).ok()?;
    let payload_b64 = base64_url_encode(&serde_json::to_vec(&payload_value).ok()?);
    let signing_input = format!("{header_b64}.{payload_b64}");

    let signature = match config.algorithm {
        JwtAlgorithm::HS256 => hmac_sha256(&secret_bytes, signing_input.as_bytes())?,
        JwtAlgorithm::HS384 => hmac_sha384(&secret_bytes, signing_input.as_bytes())?,
        JwtAlgorithm::HS512 => hmac_sha512(&secret_bytes, signing_input.as_bytes())?,
        JwtAlgorithm::RS256 | JwtAlgorithm::RS384 | JwtAlgorithm::RS512 => return None,
    };
    Some(format!("{signing_input}.{}", base64_url_encode(&signature)))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn signs_a_hs256_jwt_with_the_expected_three_dot_separated_shape() {
        let config = JwtAuthConfig {
            algorithm: JwtAlgorithm::HS256,
            secret: "test-secret".to_string(),
            is_secret_base64_encoded: false,
            payload: r#"{"sub":"user-1","role":"admin"}"#.to_string(),
            header_prefix: "Bearer".to_string(),
            add_to_query_param: false,
            query_param_key: String::new(),
        };
        let token = sign_jwt(&config).expect("HS256 signing must succeed");
        let parts: Vec<&str> = token.split('.').collect();
        assert_eq!(parts.len(), 3);
        assert!(!parts[0].is_empty());
        assert!(!parts[1].is_empty());
        assert!(!parts[2].is_empty());
    }

    #[test]
    fn signing_is_deterministic_for_the_same_secret_and_payload() {
        let config = JwtAuthConfig {
            algorithm: JwtAlgorithm::HS256,
            secret: "test-secret".to_string(),
            payload: r#"{"sub":"user-1"}"#.to_string(),
            ..Default::default()
        };
        assert_eq!(sign_jwt(&config), sign_jwt(&config));
    }

    #[test]
    fn a_base64_encoded_secret_is_decoded_before_signing() {
        let raw_secret = b"test-secret";
        let encoded_secret =
            base64::Engine::encode(&base64::engine::general_purpose::STANDARD, raw_secret);
        let plain = JwtAuthConfig {
            algorithm: JwtAlgorithm::HS256,
            secret: "test-secret".to_string(),
            payload: r#"{"sub":"user-1"}"#.to_string(),
            ..Default::default()
        };
        let base64_encoded = JwtAuthConfig {
            algorithm: JwtAlgorithm::HS256,
            secret: encoded_secret,
            is_secret_base64_encoded: true,
            payload: r#"{"sub":"user-1"}"#.to_string(),
            ..Default::default()
        };
        assert_eq!(sign_jwt(&plain), sign_jwt(&base64_encoded));
    }

    #[test]
    fn an_rs_algorithm_is_left_unsigned() {
        let config = JwtAuthConfig {
            algorithm: JwtAlgorithm::RS256,
            secret: "test-secret".to_string(),
            payload: r#"{"sub":"user-1"}"#.to_string(),
            ..Default::default()
        };
        assert!(sign_jwt(&config).is_none());
    }

    #[test]
    fn invalid_json_payload_fails_to_sign_rather_than_panicking() {
        let config = JwtAuthConfig {
            algorithm: JwtAlgorithm::HS256,
            secret: "test-secret".to_string(),
            payload: "not json".to_string(),
            ..Default::default()
        };
        assert!(sign_jwt(&config).is_none());
    }
}
