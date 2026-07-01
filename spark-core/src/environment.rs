//! Environment variable storage and request template substitution.

use serde::{Deserialize, Serialize};
use std::fmt;
use std::path::Path;

/// Opening marker for an environment variable reference.
const VARIABLE_OPEN: &str = "{{";
/// Closing marker for an environment variable reference.
const VARIABLE_CLOSE: &str = "}}";

/// A named set of variables that can be applied to request templates.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct Environment {
    /// Human-readable environment name.
    pub name: String,
    /// Variables available to request URLs, headers, and bodies.
    #[serde(default)]
    pub variables: Vec<(String, String)>,
}

impl Environment {
    /// Returns the variable value for `name`, ignoring surrounding whitespace in references.
    #[must_use]
    pub fn variable(&self, name: &str) -> Option<&str> {
        let name = name.trim();
        self.variables
            .iter()
            .find(|(key, _)| key == name)
            .map(|(_, value)| value.as_str())
    }
}

/// Error returned when a request template references an unavailable variable.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VariableError {
    /// Missing variable names in encounter order.
    missing: Vec<String>,
}

impl VariableError {
    /// Creates a missing-variable error.
    fn new(missing: Vec<String>) -> Self {
        Self { missing }
    }

    /// Returns the missing variable names.
    #[must_use]
    pub fn missing(&self) -> &[String] {
        &self.missing
    }
}

impl fmt::Display for VariableError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "missing environment variable")?;
        if self.missing.len() != 1 {
            write!(f, "s")?;
        }
        write!(f, ": {}", self.missing.join(", "))
    }
}

impl std::error::Error for VariableError {}

/// Reads all environments from a JSON file.
///
/// Returns an empty [`Vec`] if the file does not exist, cannot be read, or
/// cannot be parsed.
#[must_use]
pub fn load_environments(path: &Path) -> Vec<Environment> {
    let Ok(content) = std::fs::read_to_string(path) else {
        return Vec::new();
    };

    serde_json::from_str(&content).unwrap_or_default()
}

/// Resolves `{{variable}}` references in `template` using `environment`.
///
/// Empty references and unterminated references are left as literal text. If no
/// environment is active, any non-empty references are reported as missing.
///
/// # Errors
/// Returns a [`VariableError`] when one or more referenced variables are not
/// present in the active environment.
pub fn resolve_template(
    template: &str,
    environment: Option<&Environment>,
) -> Result<String, VariableError> {
    let mut output = String::with_capacity(template.len());
    let mut missing = Vec::new();
    let mut remaining = template;

    while let Some(open) = remaining.find(VARIABLE_OPEN) {
        output.push_str(&remaining[..open]);
        let after_open = &remaining[open + VARIABLE_OPEN.len()..];
        let Some(close) = after_open.find(VARIABLE_CLOSE) else {
            output.push_str(&remaining[open..]);
            return finish_resolution(output, missing);
        };

        let raw_name = &after_open[..close];
        let name = raw_name.trim();
        if name.is_empty() {
            output.push_str(VARIABLE_OPEN);
            output.push_str(raw_name);
            output.push_str(VARIABLE_CLOSE);
        } else if let Some(value) = environment.and_then(|env| env.variable(name)) {
            output.push_str(value);
        } else {
            push_missing_once(&mut missing, name);
            output.push_str(VARIABLE_OPEN);
            output.push_str(raw_name);
            output.push_str(VARIABLE_CLOSE);
        }

        remaining = &after_open[close + VARIABLE_CLOSE.len()..];
    }

    output.push_str(remaining);
    finish_resolution(output, missing)
}

/// Returns `output` when no variables are missing.
fn finish_resolution(output: String, missing: Vec<String>) -> Result<String, VariableError> {
    if missing.is_empty() {
        Ok(output)
    } else {
        Err(VariableError::new(missing))
    }
}

/// Adds a missing variable name once while preserving encounter order.
fn push_missing_once(missing: &mut Vec<String>, name: &str) {
    if !missing.iter().any(|existing| existing == name) {
        missing.push(name.to_string());
    }
}

#[cfg(test)]
mod tests {
    //! Tests for environment loading and variable substitution.

    use super::*;

    /// Creates an environment with common API variables.
    fn environment() -> Environment {
        Environment {
            name: "Local".to_string(),
            variables: vec![
                ("base_url".to_string(), "http://localhost:8080".to_string()),
                ("token".to_string(), "abc123".to_string()),
            ],
        }
    }

    /// Variables resolve from the active environment.
    #[test]
    fn resolve_template_replaces_variables() {
        let resolved =
            resolve_template("{{base_url}}/users?token={{ token }}", Some(&environment()))
                .expect("template should resolve");

        assert_eq!(resolved, "http://localhost:8080/users?token=abc123");
    }

    /// Missing variables are reported without duplicating names.
    #[test]
    fn resolve_template_reports_missing_variables_once() {
        let error = resolve_template("{{base_url}}/{{id}}/{{id}}", Some(&environment()))
            .expect_err("id should be missing");

        assert_eq!(error.missing(), &["id".to_string()]);
        assert_eq!(error.to_string(), "missing environment variable: id");
    }

    /// Unterminated variables are preserved as literal text.
    #[test]
    fn resolve_template_preserves_unterminated_references() {
        let resolved = resolve_template("{{base_url}}/{{id", Some(&environment()))
            .expect("unterminated reference is literal");

        assert_eq!(resolved, "http://localhost:8080/{{id");
    }

    /// Environment files round-trip through JSON loading.
    #[test]
    fn load_environments_reads_json_array() {
        let path = std::env::temp_dir().join(format!("spark-env-test-{}.json", std::process::id()));
        let json = serde_json::to_string(&vec![environment()]).expect("environment serializes");
        std::fs::write(&path, json).expect("environment file writes");

        let loaded = load_environments(&path);

        assert_eq!(loaded.len(), 1);
        assert_eq!(loaded[0].name, "Local");
        let _ = std::fs::remove_file(path);
    }
}
