use serde::{Deserialize, Serialize};
use uuid::Uuid;

pub type EnvironmentId = Uuid;

/// Fixed id of the single Global environment. Global is modeled as an
/// ordinary `Environment` rather than a separate type, so the resolver in
/// `variable_resolution` can treat every scope uniformly -- it is simply the
/// one environment guaranteed to always exist, always has this id, and is
/// never shown in the environment-switcher picker.
pub const GLOBAL_ENVIRONMENT_ID: EnvironmentId = Uuid::nil();

/// A single named value available for `{{key}}` substitution. `initial_value`
/// is what gets persisted/shared; `current_value` is a session-local override
/// that starts equal to `initial_value` and is never written back to
/// `initial_value` automatically -- this mirrors the deliberate split found
/// in comparable API-client tools so a locally-tweaked value never silently
/// becomes the shared default.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Variable {
    pub key: String,
    pub initial_value: String,
    pub current_value: String,
    #[serde(default)]
    pub secret: bool,
    #[serde(default = "default_true")]
    pub enabled: bool,
}

fn default_true() -> bool {
    true
}

impl Variable {
    pub fn new(key: String, value: String) -> Self {
        Self {
            key,
            initial_value: value.clone(),
            current_value: value,
            secret: false,
            enabled: true,
        }
    }

    /// The value to use when actually sending a request.
    pub fn value_for_send(&self) -> &str {
        &self.current_value
    }

    /// The value to use anywhere it might be displayed, logged, or exported
    /// (masked when `secret` is set). Callers must go through this rather
    /// than reading `current_value` directly for any display purpose.
    pub fn value_for_display(&self) -> &str {
        if self.secret {
            "••••••••"
        } else {
            &self.current_value
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Environment {
    pub id: EnvironmentId,
    pub name: String,
    #[serde(default)]
    pub variables: Vec<Variable>,
}

impl Environment {
    pub fn new(name: String) -> Self {
        Self {
            id: Uuid::new_v4(),
            name,
            variables: Vec::new(),
        }
    }

    /// Creates the single, well-known Global environment.
    pub fn global() -> Self {
        Self {
            id: GLOBAL_ENVIRONMENT_ID,
            name: "Global".to_string(),
            variables: Vec::new(),
        }
    }

    pub fn is_global(&self) -> bool {
        self.id == GLOBAL_ENVIRONMENT_ID
    }

    pub fn variable(&self, key: &str) -> Option<&Variable> {
        self.variables
            .iter()
            .find(|variable| variable.enabled && variable.key == key)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn global_environment_has_the_reserved_nil_id() {
        let global = Environment::global();
        assert_eq!(global.id, GLOBAL_ENVIRONMENT_ID);
        assert!(global.is_global());
    }

    #[test]
    fn a_freshly_created_environment_is_not_global() {
        let environment = Environment::new("Staging".to_string());
        assert!(!environment.is_global());
    }

    #[test]
    fn variable_lookup_skips_disabled_entries() {
        let mut environment = Environment::new("Staging".to_string());
        environment.variables.push(Variable::new(
            "base_url".to_string(),
            "https://staging.example.com".to_string(),
        ));
        let mut disabled = Variable::new("api_key".to_string(), "abc123".to_string());
        disabled.enabled = false;
        environment.variables.push(disabled);

        assert!(environment.variable("base_url").is_some());
        assert!(environment.variable("api_key").is_none());
        assert!(environment.variable("does_not_exist").is_none());
    }

    #[test]
    fn secret_variable_display_value_is_masked_but_send_value_is_not() {
        let mut variable = Variable::new("token".to_string(), "super-secret".to_string());
        variable.secret = true;

        assert_eq!(variable.value_for_send(), "super-secret");
        assert_eq!(variable.value_for_display(), "••••••••");
    }

    #[test]
    fn non_secret_variable_display_value_matches_current_value() {
        let variable = Variable::new("base_url".to_string(), "https://example.com".to_string());
        assert_eq!(variable.value_for_display(), "https://example.com");
    }

    #[test]
    fn current_value_can_diverge_from_initial_value_without_mutating_it() {
        let mut variable = Variable::new("base_url".to_string(), "https://example.com".to_string());
        variable.current_value = "https://localhost:8080".to_string();
        assert_eq!(variable.initial_value, "https://example.com");
        assert_eq!(variable.current_value, "https://localhost:8080");
    }
}
