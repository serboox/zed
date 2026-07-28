use uuid::Uuid;

use crate::collection::Collection;
use crate::environment::{Environment, Variable};

/// OpenAPI path templates use single braces (`/users/{id}`); variable
/// substitution here uses double braces (`{{id}}`) -- every `{name}` segment in
/// an OpenAPI path is therefore a path parameter that must be rewritten to the
/// double-brace form before it means anything to [`resolve`].
pub fn rewrite_path_template(path: &str) -> String {
    let mut rewritten = String::with_capacity(path.len());
    let mut chars = path.chars().peekable();
    while let Some(ch) = chars.next() {
        if ch == '{' {
            rewritten.push_str("{{");
            for inner in chars.by_ref() {
                if inner == '}' {
                    break;
                }
                rewritten.push(inner);
            }
            rewritten.push_str("}}");
        } else {
            rewritten.push(ch);
        }
    }
    rewritten
}

/// Every scope a `{{name}}` token can resolve against, ordered
/// narrowest-to-widest. Precedence when the same key exists in more than one
/// scope: **Environment > Collection > Global**. `Local`/data-file scopes
/// from a future collection-runner feature are intentionally out of scope
/// here (Phase 4).
pub struct VariableContext<'a> {
    pub environment: Option<&'a Environment>,
    pub collection: Option<&'a Collection>,
    pub global: &'a Environment,
}

impl<'a> VariableContext<'a> {
    fn lookup(&self, key: &str) -> Option<&'a Variable> {
        if let Some(environment) = self.environment
            && let Some(variable) = environment.variable(key)
        {
            return Some(variable);
        }
        if let Some(collection) = self.collection
            && let Some(variable) = collection
                .variables
                .iter()
                .find(|variable| variable.enabled && variable.key == key)
        {
            return Some(variable);
        }
        self.global.variable(key)
    }
}

/// Supplies the value of a `$name` dynamic variable (Postman calls these
/// "dynamic variables" -- `{{$guid}}`, `{{$timestamp}}`, etc.). Kept as a
/// trait, not a free function calling `std::time`/a random source directly,
/// so tests can inject fixed values and assert exact output instead of only
/// "is non-empty". `resolve_dynamic` returns `None` for any name it doesn't
/// recognize, which falls through to the ordinary stored-variable lookup --
/// this is safe because every recognized dynamic-variable name is reserved
/// (see `DYNAMIC_VARIABLE_NAMES`) and can never collide with a real stored
/// variable's key without the user having typed a `$`-prefixed key
/// themselves, which they cannot do through the environment editor's normal
/// UI.
pub trait DynamicVariableSource {
    fn resolve_dynamic(&self, name: &str) -> Option<String>;
}

/// The production `DynamicVariableSource`, backed by real wall-clock time and
/// randomness. Random values are derived from freshly generated v4 UUIDs
/// (already a workspace dependency) rather than pulling in a dedicated `rand`
/// crate -- Phase 1 has no need for cryptographic-quality randomness here,
/// only visually-varied placeholder values.
pub struct SystemDynamicVariableSource;

const FIRST_NAMES: &[&str] = &[
    "Alex", "Jordan", "Taylor", "Morgan", "Casey", "Riley", "Sam", "Drew",
];
const LAST_NAMES: &[&str] = &[
    "Smith", "Johnson", "Lee", "Brown", "Garcia", "Martin", "Davis", "Clark",
];
const WORDS: &[&str] = &[
    "lorem",
    "ipsum",
    "dolor",
    "sit",
    "amet",
    "consectetur",
    "adipiscing",
    "elit",
    "sed",
    "do",
];

fn random_bytes() -> [u8; 16] {
    *Uuid::new_v4().as_bytes()
}

fn pick<T>(items: &[T], seed: u8) -> &T {
    &items[seed as usize % items.len()]
}

impl DynamicVariableSource for SystemDynamicVariableSource {
    fn resolve_dynamic(&self, name: &str) -> Option<String> {
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default();
        match name {
            "guid" | "randomUUID" => Some(Uuid::new_v4().to_string()),
            "timestamp" => Some(now.as_secs().to_string()),
            "isoTimestamp" => Some(format_unix_seconds_as_iso8601(now.as_secs())),
            "randomInt" => {
                let bytes = random_bytes();
                Some((u16::from_le_bytes([bytes[0], bytes[1]]) % 1000).to_string())
            }
            "randomEmail" => {
                let bytes = random_bytes();
                Some(format!(
                    "{}.{}@example.com",
                    pick(FIRST_NAMES, bytes[0]).to_lowercase(),
                    pick(LAST_NAMES, bytes[1]).to_lowercase()
                ))
            }
            "randomFirstName" => Some(pick(FIRST_NAMES, random_bytes()[0]).to_string()),
            "randomLastName" => Some(pick(LAST_NAMES, random_bytes()[0]).to_string()),
            "randomFullName" => {
                let bytes = random_bytes();
                Some(format!(
                    "{} {}",
                    pick(FIRST_NAMES, bytes[0]),
                    pick(LAST_NAMES, bytes[1])
                ))
            }
            "randomWord" => Some(pick(WORDS, random_bytes()[0]).to_string()),
            "randomWords" => {
                let bytes = random_bytes();
                Some(format!(
                    "{} {} {}",
                    pick(WORDS, bytes[0]),
                    pick(WORDS, bytes[1]),
                    pick(WORDS, bytes[2])
                ))
            }
            "randomIP" => {
                let bytes = random_bytes();
                Some(format!(
                    "{}.{}.{}.{}",
                    bytes[0], bytes[1], bytes[2], bytes[3]
                ))
            }
            _ => None,
        }
    }
}

/// Formats a Unix timestamp (seconds) as `YYYY-MM-DDTHH:MM:SSZ` without
/// pulling in a date/time crate -- Phase 1 only needs this for the
/// `{{$isoTimestamp}}` dynamic variable, not general date arithmetic.
fn format_unix_seconds_as_iso8601(total_seconds: u64) -> String {
    const SECONDS_PER_DAY: u64 = 86_400;
    let days_since_epoch = total_seconds / SECONDS_PER_DAY;
    let seconds_of_day = total_seconds % SECONDS_PER_DAY;
    let (hour, minute, second) = (
        seconds_of_day / 3600,
        (seconds_of_day / 60) % 60,
        seconds_of_day % 60,
    );

    // Civil-from-days algorithm (Howard Hinnant's public-domain date algorithms).
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

    format!("{year:04}-{month:02}-{day:02}T{hour:02}:{minute:02}:{second:02}Z")
}

/// The set of every `$name` `SystemDynamicVariableSource` recognizes,
/// exposed so `variable_highlighting` (Phase 1 item 10) can flag a
/// `{{$name}}` token as always-resolved without needing an
/// `Environment`/`Collection` in scope to check against.
pub const DYNAMIC_VARIABLE_NAMES: &[&str] = &[
    "guid",
    "timestamp",
    "isoTimestamp",
    "randomInt",
    "randomUUID",
    "randomEmail",
    "randomFirstName",
    "randomLastName",
    "randomFullName",
    "randomWord",
    "randomWords",
    "randomIP",
];

fn find_token(text: &str, start: usize) -> Option<(usize, usize, &str)> {
    let open = text[start..].find("{{")? + start;
    let close = text[open..].find("}}")? + open;
    let name = text[open + 2..close].trim();
    Some((open, close + 2, name))
}

/// Resolves every `{{name}}` token in `text`. Dynamic variables (`$name`) are
/// checked before the stored-variable lookup. A token whose name resolves to
/// nothing (neither dynamic nor found in any scope) is left in the output
/// verbatim, unchanged -- this matches the reference tool's behavior and
/// means a typo never silently turns into an empty string.
///
/// `for_display` selects between the send-time value (`Variable::value_for_send`)
/// and the display-safe, secret-masking value (`Variable::value_for_display`).
/// Kept as one function with an explicit enum parameter -- rather than two
/// near-identical public functions or one function with a bare `bool` -- so
/// call sites read unambiguously (`resolve(.., ResolveMode::ForDisplay)`
/// rather than a bare `true`/`false` a reader has to look up the meaning of).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ResolveMode {
    ForSend,
    ForDisplay,
}

pub fn resolve(
    text: &str,
    context: &VariableContext,
    dynamic: &dyn DynamicVariableSource,
    mode: ResolveMode,
) -> String {
    let mut result = String::with_capacity(text.len());
    let mut cursor = 0;
    while let Some((open, close, name)) = find_token(text, cursor) {
        result.push_str(&text[cursor..open]);
        if let Some(value) = dynamic.resolve_dynamic(name.strip_prefix('$').unwrap_or(name)) {
            if name.starts_with('$') {
                result.push_str(&value);
            } else {
                // A literal `{{name}}` (no `$`) that happens to match a
                // dynamic variable's bare name is not a dynamic-variable
                // reference -- fall through to the stored-variable lookup.
                push_resolved_or_literal(&mut result, context, name, mode, open, close, text);
            }
        } else {
            push_resolved_or_literal(&mut result, context, name, mode, open, close, text);
        }
        cursor = close;
    }
    result.push_str(&text[cursor..]);
    result
}

fn push_resolved_or_literal(
    result: &mut String,
    context: &VariableContext,
    name: &str,
    mode: ResolveMode,
    open: usize,
    close: usize,
    text: &str,
) {
    match context.lookup(name) {
        Some(variable) => match mode {
            ResolveMode::ForSend => result.push_str(variable.value_for_send()),
            ResolveMode::ForDisplay => result.push_str(variable.value_for_display()),
        },
        None => result.push_str(&text[open..close]),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::environment::Variable;

    struct NoDynamicVariables;
    impl DynamicVariableSource for NoDynamicVariables {
        fn resolve_dynamic(&self, _name: &str) -> Option<String> {
            None
        }
    }

    struct FixedDynamicVariables;
    impl DynamicVariableSource for FixedDynamicVariables {
        fn resolve_dynamic(&self, name: &str) -> Option<String> {
            match name {
                "guid" => Some("11111111-1111-1111-1111-111111111111".to_string()),
                "timestamp" => Some("1700000000".to_string()),
                _ => None,
            }
        }
    }

    fn environment_with(key: &str, value: &str) -> Environment {
        let mut environment = Environment::new("Test".to_string());
        environment
            .variables
            .push(Variable::new(key.to_string(), value.to_string()));
        environment
    }

    fn collection_with(key: &str, value: &str) -> Collection {
        let mut collection = Collection::new("Test collection".to_string());
        collection
            .variables
            .push(Variable::new(key.to_string(), value.to_string()));
        collection
    }

    #[test]
    fn a_variable_present_in_both_environment_and_global_resolves_to_the_environment_value() {
        let environment = environment_with("base_url", "https://env.example.com");
        let mut global = Environment::global();
        global.variables.push(Variable::new(
            "base_url".to_string(),
            "https://global.example.com".to_string(),
        ));
        let context = VariableContext {
            environment: Some(&environment),
            collection: None,
            global: &global,
        };
        let resolved = resolve(
            "{{base_url}}/users",
            &context,
            &NoDynamicVariables,
            ResolveMode::ForSend,
        );
        assert_eq!(resolved, "https://env.example.com/users");
    }

    #[test]
    fn a_variable_present_in_both_collection_and_global_resolves_to_the_collection_value_when_no_environment_has_it()
     {
        let environment = environment_with("unrelated", "x");
        let collection = collection_with("base_url", "https://collection.example.com");
        let mut global = Environment::global();
        global.variables.push(Variable::new(
            "base_url".to_string(),
            "https://global.example.com".to_string(),
        ));
        let context = VariableContext {
            environment: Some(&environment),
            collection: Some(&collection),
            global: &global,
        };
        let resolved = resolve(
            "{{base_url}}",
            &context,
            &NoDynamicVariables,
            ResolveMode::ForSend,
        );
        assert_eq!(resolved, "https://collection.example.com");
    }

    #[test]
    fn a_variable_only_in_global_still_resolves() {
        let mut global = Environment::global();
        global
            .variables
            .push(Variable::new("api_version".to_string(), "v2".to_string()));
        let context = VariableContext {
            environment: None,
            collection: None,
            global: &global,
        };
        let resolved = resolve(
            "/api/{{api_version}}/users",
            &context,
            &NoDynamicVariables,
            ResolveMode::ForSend,
        );
        assert_eq!(resolved, "/api/v2/users");
    }

    #[test]
    fn a_missing_variable_is_left_as_the_literal_token_rather_than_erroring_or_becoming_empty() {
        let global = Environment::global();
        let context = VariableContext {
            environment: None,
            collection: None,
            global: &global,
        };
        let resolved = resolve(
            "{{does_not_exist}}/users",
            &context,
            &NoDynamicVariables,
            ResolveMode::ForSend,
        );
        assert_eq!(resolved, "{{does_not_exist}}/users");
    }

    #[test]
    fn a_disabled_variable_is_treated_as_missing() {
        let mut global = Environment::global();
        let mut disabled = Variable::new("base_url".to_string(), "https://example.com".to_string());
        disabled.enabled = false;
        global.variables.push(disabled);
        let context = VariableContext {
            environment: None,
            collection: None,
            global: &global,
        };
        let resolved = resolve(
            "{{base_url}}",
            &context,
            &NoDynamicVariables,
            ResolveMode::ForSend,
        );
        assert_eq!(resolved, "{{base_url}}");
    }

    #[test]
    fn for_send_mode_reveals_a_secret_variables_real_value() {
        let mut global = Environment::global();
        let mut secret = Variable::new("api_key".to_string(), "sk-live-123".to_string());
        secret.secret = true;
        global.variables.push(secret);
        let context = VariableContext {
            environment: None,
            collection: None,
            global: &global,
        };
        let resolved = resolve(
            "{{api_key}}",
            &context,
            &NoDynamicVariables,
            ResolveMode::ForSend,
        );
        assert_eq!(resolved, "sk-live-123");
    }

    #[test]
    fn for_display_mode_never_reveals_a_secret_variables_real_value() {
        let mut global = Environment::global();
        let mut secret = Variable::new("api_key".to_string(), "sk-live-123".to_string());
        secret.secret = true;
        global.variables.push(secret);
        let context = VariableContext {
            environment: None,
            collection: None,
            global: &global,
        };
        let resolved = resolve(
            "Authorization: {{api_key}}",
            &context,
            &NoDynamicVariables,
            ResolveMode::ForDisplay,
        );
        assert!(!resolved.contains("sk-live-123"));
        assert!(resolved.contains("••••••••"));
    }

    #[test]
    fn multiple_tokens_in_one_string_all_resolve_independently() {
        let mut global = Environment::global();
        global.variables.push(Variable::new(
            "host".to_string(),
            "api.example.com".to_string(),
        ));
        global
            .variables
            .push(Variable::new("version".to_string(), "v1".to_string()));
        let context = VariableContext {
            environment: None,
            collection: None,
            global: &global,
        };
        let resolved = resolve(
            "https://{{host}}/{{version}}/users",
            &context,
            &NoDynamicVariables,
            ResolveMode::ForSend,
        );
        assert_eq!(resolved, "https://api.example.com/v1/users");
    }

    #[test]
    fn a_dynamic_variable_resolves_before_any_stored_variable_lookup() {
        let global = Environment::global();
        let context = VariableContext {
            environment: None,
            collection: None,
            global: &global,
        };
        let resolved = resolve(
            "id={{$guid}}&at={{$timestamp}}",
            &context,
            &FixedDynamicVariables,
            ResolveMode::ForSend,
        );
        assert_eq!(
            resolved,
            "id=11111111-1111-1111-1111-111111111111&at=1700000000"
        );
    }

    #[test]
    fn a_literal_token_without_a_dollar_sign_never_triggers_dynamic_resolution_even_if_the_name_matches()
     {
        let mut global = Environment::global();
        global.variables.push(Variable::new(
            "guid".to_string(),
            "not-a-real-guid".to_string(),
        ));
        let context = VariableContext {
            environment: None,
            collection: None,
            global: &global,
        };
        let resolved = resolve(
            "{{guid}}",
            &context,
            &FixedDynamicVariables,
            ResolveMode::ForSend,
        );
        assert_eq!(resolved, "not-a-real-guid");
    }

    #[test]
    fn an_unrecognized_dynamic_variable_name_falls_through_to_the_literal_token() {
        let global = Environment::global();
        let context = VariableContext {
            environment: None,
            collection: None,
            global: &global,
        };
        let resolved = resolve(
            "{{$notARealDynamicVariable}}",
            &context,
            &FixedDynamicVariables,
            ResolveMode::ForSend,
        );
        assert_eq!(resolved, "{{$notARealDynamicVariable}}");
    }

    #[test]
    fn system_dynamic_variable_source_produces_every_documented_name() {
        let source = SystemDynamicVariableSource;
        for name in DYNAMIC_VARIABLE_NAMES {
            assert!(
                source.resolve_dynamic(name).is_some(),
                "expected SystemDynamicVariableSource to resolve ${name}"
            );
        }
        assert!(source.resolve_dynamic("notARealDynamicVariable").is_none());
    }

    #[test]
    fn iso8601_formatting_matches_a_known_reference_timestamp() {
        // 2023-11-14T22:13:20Z, a widely-cited "nice round number" epoch value.
        assert_eq!(
            format_unix_seconds_as_iso8601(1_700_000_000),
            "2023-11-14T22:13:20Z"
        );
        // The epoch itself.
        assert_eq!(format_unix_seconds_as_iso8601(0), "1970-01-01T00:00:00Z");
    }

    #[test]
    fn text_with_no_tokens_at_all_is_returned_unchanged() {
        let global = Environment::global();
        let context = VariableContext {
            environment: None,
            collection: None,
            global: &global,
        };
        let resolved = resolve(
            "https://example.com/health",
            &context,
            &NoDynamicVariables,
            ResolveMode::ForSend,
        );
        assert_eq!(resolved, "https://example.com/health");
    }

    #[test]
    fn an_unterminated_token_is_left_untouched_rather_than_panicking() {
        let global = Environment::global();
        let context = VariableContext {
            environment: None,
            collection: None,
            global: &global,
        };
        let resolved = resolve(
            "https://example.com/{{unterminated",
            &context,
            &NoDynamicVariables,
            ResolveMode::ForSend,
        );
        assert_eq!(resolved, "https://example.com/{{unterminated");
    }
}
