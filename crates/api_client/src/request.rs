use std::path::PathBuf;

use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::collection::CollectionId;
use crate::environment::EnvironmentId;
use crate::folder::FolderId;

pub type RequestId = Uuid;

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub enum HttpMethod {
    #[default]
    Get,
    Post,
    Put,
    Patch,
    Delete,
    Head,
    Options,
    Custom(String),
}

impl HttpMethod {
    pub fn as_str(&self) -> &str {
        match self {
            HttpMethod::Get => "GET",
            HttpMethod::Post => "POST",
            HttpMethod::Put => "PUT",
            HttpMethod::Patch => "PATCH",
            HttpMethod::Delete => "DELETE",
            HttpMethod::Head => "HEAD",
            HttpMethod::Options => "OPTIONS",
            HttpMethod::Custom(method) => method.as_str(),
        }
    }
}

/// A single query-parameter row. Kept bidirectionally in sync with the
/// request's `url` query string by the UI layer -- this type only stores the
/// parsed shape, it does not itself own the sync logic.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct QueryParam {
    pub key: String,
    pub value: String,
    #[serde(default = "default_true")]
    pub enabled: bool,
    #[serde(default)]
    pub description: Option<String>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Header {
    pub key: String,
    pub value: String,
    #[serde(default = "default_true")]
    pub enabled: bool,
    #[serde(default)]
    pub description: Option<String>,
}

fn default_true() -> bool {
    true
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum RawBodyContentType {
    Text,
    Json,
    Xml,
    Html,
    JavaScript,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum FormDataValue {
    Text(String),
    File(PathBuf),
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FormDataField {
    pub key: String,
    pub value: FormDataValue,
    #[serde(default = "default_true")]
    pub enabled: bool,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub enum RequestBody {
    #[default]
    None,
    Raw {
        content_type: RawBodyContentType,
        text: String,
    },
    FormData(Vec<FormDataField>),
    UrlEncoded(Vec<(String, String)>),
    Binary {
        path: PathBuf,
    },
    GraphQl {
        query: String,
        #[serde(default)]
        variables: String,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ApiKeyPlacement {
    Header,
    Query,
}

#[derive(Debug, Clone, Copy, Default, Eq, PartialEq, Serialize, Deserialize)]
pub enum JwtAlgorithm {
    #[default]
    HS256,
    HS384,
    HS512,
    RS256,
    RS384,
    RS512,
}

impl JwtAlgorithm {
    pub fn as_str(&self) -> &'static str {
        match self {
            JwtAlgorithm::HS256 => "HS256",
            JwtAlgorithm::HS384 => "HS384",
            JwtAlgorithm::HS512 => "HS512",
            JwtAlgorithm::RS256 => "RS256",
            JwtAlgorithm::RS384 => "RS384",
            JwtAlgorithm::RS512 => "RS512",
        }
    }

    pub fn from_str(value: &str) -> Self {
        match value {
            "HS384" => JwtAlgorithm::HS384,
            "HS512" => JwtAlgorithm::HS512,
            "RS256" => JwtAlgorithm::RS256,
            "RS384" => JwtAlgorithm::RS384,
            "RS512" => JwtAlgorithm::RS512,
            _ => JwtAlgorithm::HS256,
        }
    }
}

/// Postman's "jwt bearer" auth type: a JWT is signed from `payload` with
/// `secret` and attached either as a bearer token header (with `header_prefix`,
/// e.g. `"Bearer"`) or as a query parameter named `query_param_key`. `secret`
/// is plaintext only in memory, exactly like `AuthConfig::Basic`'s `password`.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct JwtAuthConfig {
    #[serde(default)]
    pub algorithm: JwtAlgorithm,
    #[serde(default)]
    pub secret: String,
    #[serde(default)]
    pub is_secret_base64_encoded: bool,
    /// The JWT claims, as JSON text (Postman calls this the "payload").
    #[serde(default)]
    pub payload: String,
    #[serde(default)]
    pub header_prefix: String,
    #[serde(default)]
    pub add_to_query_param: bool,
    #[serde(default)]
    pub query_param_key: String,
}

/// How a request authenticates. Secret fields (`password`, `token`, `value`)
/// are plaintext only in memory -- the UI-side store must redact them to
/// empty before the collection tree is written to disk and route the real
/// secret through `CredentialsProvider` (the OS keychain), exactly like
/// `db_client::ConnectionConfig::password`/`ssh_password` already do. This
/// crate has no GPUI dependency and therefore no access to
/// `CredentialsProvider` itself (it takes a `gpui::AsyncApp`) -- that wiring
/// belongs in `api_client_ui`'s store, not here.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub enum AuthConfig {
    /// Use whatever auth the containing folder/collection resolves to.
    #[default]
    Inherit,
    None,
    Basic {
        username: String,
        #[serde(default)]
        password: String,
    },
    Bearer {
        #[serde(default)]
        token: String,
    },
    ApiKey {
        key: String,
        #[serde(default)]
        value: String,
        placement: ApiKeyPlacement,
    },
    OAuth2(crate::oauth2::OAuth2Config),
    AwsSigV4(crate::aws_sigv4::AwsSigV4Config),
    Jwt(JwtAuthConfig),
}

pub type ExampleId = Uuid;

/// A named, persisted request/response snapshot attached to a `Request` --
/// Postman calls these "saved examples". Distinct from `HistoryEntry`
/// (`crates/api_client/src/history.rs`), which is an ephemeral, unnamed send
/// log entry with no response body; a `SavedExample` is a deliberate,
/// user-named save that keeps the full request and response bodies and is
/// meant to be shared/exported with the collection.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SavedExample {
    pub id: ExampleId,
    pub name: String,
    pub request_method: HttpMethod,
    pub request_url: String,
    #[serde(default)]
    pub request_headers: Vec<Header>,
    #[serde(default)]
    pub request_body_text: String,
    pub response_status: u16,
    #[serde(default)]
    pub response_headers: Vec<(String, String)>,
    #[serde(default)]
    pub response_body: String,
}

impl SavedExample {
    pub fn new(
        name: String,
        request_method: HttpMethod,
        request_url: String,
        request_headers: Vec<Header>,
        request_body_text: String,
        response_status: u16,
        response_headers: Vec<(String, String)>,
        response_body: String,
    ) -> Self {
        Self {
            id: Uuid::new_v4(),
            name,
            request_method,
            request_url,
            request_headers,
            request_body_text,
            response_status,
            response_headers,
            response_body,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RequestSettings {
    #[serde(default = "default_true")]
    pub follow_redirects: bool,
    #[serde(default = "default_true")]
    pub verify_ssl: bool,
    /// Timeout in milliseconds; `None` means no explicit timeout.
    #[serde(default)]
    pub timeout_ms: Option<u64>,
    /// Names (matched case-insensitively) of the default auto-generated
    /// headers -- see `http_send::AUTO_HEADER_DEFAULTS` -- that this request
    /// has opted out of sending. Empty means every default is sent.
    #[serde(default)]
    pub disabled_auto_headers: Vec<String>,
}

impl Default for RequestSettings {
    fn default() -> Self {
        Self {
            follow_redirects: true,
            verify_ssl: true,
            timeout_ms: None,
            disabled_auto_headers: Vec::new(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Request {
    pub id: RequestId,
    pub collection_id: CollectionId,
    #[serde(default)]
    pub folder_id: Option<FolderId>,
    #[serde(default)]
    pub order: i64,
    pub name: String,
    #[serde(default)]
    pub description: Option<String>,
    #[serde(default)]
    pub method: HttpMethod,
    #[serde(default)]
    pub url: String,
    #[serde(default)]
    pub params: Vec<QueryParam>,
    #[serde(default)]
    pub headers: Vec<Header>,
    #[serde(default)]
    pub body: RequestBody,
    #[serde(default)]
    pub auth: AuthConfig,
    #[serde(default)]
    pub settings: RequestSettings,
    /// Runs before the request is sent; can read/write environment and
    /// collection variables (`pm.environment`/`pm.collectionVariables`) but
    /// has no `pm.response` yet -- mirrors Postman's pre-request script.
    #[serde(default)]
    pub pre_request_script: String,
    /// Runs after a response is received; adds `pm.response` and is where
    /// `pm.test()`/`pm.visualize()` are meant to be called -- mirrors
    /// Postman's "Tests" script.
    #[serde(default)]
    pub test_script: String,
    #[serde(default)]
    pub examples: Vec<SavedExample>,
    /// Overrides the store's globally active environment for this request
    /// alone, for a request that is always meant to run against one
    /// particular environment regardless of what's currently active.
    #[serde(default)]
    pub pinned_environment_id: Option<EnvironmentId>,
}

impl Request {
    pub fn new(collection_id: CollectionId, name: String) -> Self {
        Self {
            id: Uuid::new_v4(),
            collection_id,
            folder_id: None,
            order: 0,
            name,
            description: None,
            method: HttpMethod::default(),
            url: String::new(),
            params: Vec::new(),
            headers: Vec::new(),
            body: RequestBody::default(),
            auth: AuthConfig::default(),
            settings: RequestSettings::default(),
            pre_request_script: String::new(),
            test_script: String::new(),
            examples: Vec::new(),
            pinned_environment_id: None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn new_request_defaults_to_get_with_no_body_and_inherited_auth() {
        let request = Request::new(Uuid::new_v4(), "List users".to_string());
        assert_eq!(request.method, HttpMethod::Get);
        assert!(matches!(request.body, RequestBody::None));
        assert!(matches!(request.auth, AuthConfig::Inherit));
        assert!(request.settings.follow_redirects);
        assert!(request.settings.verify_ssl);
    }

    #[test]
    fn custom_http_method_round_trips_through_serde() {
        let method = HttpMethod::Custom("PROPFIND".to_string());
        let json = serde_json::to_string(&method).unwrap();
        let restored: HttpMethod = serde_json::from_str(&json).unwrap();
        assert_eq!(restored.as_str(), "PROPFIND");
    }

    #[test]
    fn every_standard_method_as_str_matches_its_wire_name() {
        let cases = [
            (HttpMethod::Get, "GET"),
            (HttpMethod::Post, "POST"),
            (HttpMethod::Put, "PUT"),
            (HttpMethod::Patch, "PATCH"),
            (HttpMethod::Delete, "DELETE"),
            (HttpMethod::Head, "HEAD"),
            (HttpMethod::Options, "OPTIONS"),
        ];
        for (method, expected) in cases {
            assert_eq!(method.as_str(), expected);
        }
    }

    #[test]
    fn a_request_missing_optional_fields_in_json_falls_back_to_sane_defaults() {
        let json = format!(
            r#"{{"id":"{}","collection_id":"{}","name":"Legacy"}}"#,
            Uuid::new_v4(),
            Uuid::new_v4()
        );
        let request: Request = serde_json::from_str(&json).unwrap();
        assert_eq!(request.method, HttpMethod::Get);
        assert!(request.url.is_empty());
        assert!(request.params.is_empty());
        assert!(request.headers.is_empty());
        assert!(matches!(request.body, RequestBody::None));
        assert!(matches!(request.auth, AuthConfig::Inherit));
        assert!(request.examples.is_empty());
    }

    #[test]
    fn a_new_request_has_no_saved_examples() {
        let request = Request::new(Uuid::new_v4(), "List users".to_string());
        assert!(request.examples.is_empty());
    }

    #[test]
    fn a_saved_example_round_trips_through_serde() {
        let example = SavedExample::new(
            "200 OK".to_string(),
            HttpMethod::Get,
            "https://api.example.com/users".to_string(),
            vec![Header {
                key: "Accept".to_string(),
                value: "application/json".to_string(),
                enabled: true,
                description: None,
            }],
            String::new(),
            200,
            vec![("Content-Type".to_string(), "application/json".to_string())],
            r#"{"id":1}"#.to_string(),
        );
        let json = serde_json::to_string(&example).unwrap();
        let restored: SavedExample = serde_json::from_str(&json).unwrap();
        assert_eq!(restored.name, "200 OK");
        assert_eq!(restored.request_method, HttpMethod::Get);
        assert_eq!(restored.response_status, 200);
        assert_eq!(restored.response_body, r#"{"id":1}"#);
        assert_eq!(restored.request_headers.len(), 1);
    }

    #[test]
    fn form_data_and_url_encoded_bodies_round_trip_through_serde() {
        let form = RequestBody::FormData(vec![FormDataField {
            key: "avatar".to_string(),
            value: FormDataValue::File(PathBuf::from("/tmp/avatar.png")),
            enabled: true,
        }]);
        let json = serde_json::to_string(&form).unwrap();
        let restored: RequestBody = serde_json::from_str(&json).unwrap();
        match restored {
            RequestBody::FormData(fields) => {
                assert_eq!(fields.len(), 1);
                assert_eq!(fields[0].key, "avatar");
            }
            other => panic!("expected FormData, got {other:?}"),
        }

        let url_encoded = RequestBody::UrlEncoded(vec![(
            "grant_type".to_string(),
            "client_credentials".to_string(),
        )]);
        let json = serde_json::to_string(&url_encoded).unwrap();
        let restored: RequestBody = serde_json::from_str(&json).unwrap();
        assert!(matches!(restored, RequestBody::UrlEncoded(pairs) if pairs.len() == 1));
    }

    #[test]
    fn api_key_auth_carries_its_placement() {
        let auth = AuthConfig::ApiKey {
            key: "X-Api-Key".to_string(),
            value: "secret".to_string(),
            placement: ApiKeyPlacement::Header,
        };
        let json = serde_json::to_string(&auth).unwrap();
        let restored: AuthConfig = serde_json::from_str(&json).unwrap();
        assert!(matches!(
            restored,
            AuthConfig::ApiKey {
                placement: ApiKeyPlacement::Header,
                ..
            }
        ));
    }
}
