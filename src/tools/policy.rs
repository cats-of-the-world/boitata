// Tool permission policy.
//
// Boitata runs unattended, so there's no human to approve individual tool calls
// the way goose prompts interactively. Instead the policy is configured up front
// and consulted before every tool execution, mirroring the *decision* goose's
// permission layer makes (allow vs. deny) driven by the tool's annotations.
//
// Two, composable controls:
//   - a `mode` that can restrict the agent to read-only tools, and
//   - a denylist of regexes matched against `execute_command` command strings.

use regex::Regex;
use serde::Deserialize;
use serde_json::Value;

use crate::tools::ToolAnnotations;

/// How permissive the policy is about tools that may modify state.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PolicyMode {
    /// Allow every tool (still subject to the command denylist). The default,
    /// preserving the agent's full capability.
    #[default]
    AllowAll,
    /// Allow only tools annotated read-only; deny anything that may mutate. Use
    /// for locked-down, observe-only runs.
    ReadOnly,
}

/// The outcome of consulting the policy for one tool call.
#[derive(Debug, PartialEq, Eq)]
pub enum Decision {
    Allow,
    /// Deny with a human-readable reason (surfaced to the model and the audit log).
    Deny(String),
}

/// Decides whether a given tool call may run.
#[derive(Debug, Default)]
pub struct ToolPolicy {
    mode: PolicyMode,
    /// Regexes; an `execute_command` whose command matches any is denied.
    denied_commands: Vec<Regex>,
}

impl ToolPolicy {
    /// Build a policy from a mode and a list of denylist regex patterns. Returns
    /// an error if any pattern fails to compile.
    pub fn new(mode: PolicyMode, denied_command_patterns: &[String]) -> Result<Self, regex::Error> {
        let denied_commands = denied_command_patterns
            .iter()
            .map(|p| Regex::new(p))
            .collect::<Result<Vec<_>, _>>()?;
        Ok(Self {
            mode,
            denied_commands,
        })
    }

    /// The permissive default: allow every tool, no denylist.
    pub fn allow_all() -> Self {
        Self::default()
    }

    /// Decide whether the named tool may run with the given annotations and
    /// arguments.
    pub fn decide(
        &self,
        tool_name: &str,
        annotations: Option<ToolAnnotations>,
        arguments: &Value,
    ) -> Decision {
        // Read-only mode blocks anything not explicitly annotated read-only.
        // Unknown tools (no annotations) are treated as potentially mutating.
        if self.mode == PolicyMode::ReadOnly {
            let read_only = annotations.map(|a| a.read_only).unwrap_or(false);
            if !read_only {
                return Decision::Deny(format!(
                    "tool `{tool_name}` may modify state, but the tool policy is read-only"
                ));
            }
        }

        // The denylist targets arbitrary shell execution — the one tool that can
        // run anything. Structured tools (cargo_*, git_*, ...) aren't matched.
        if tool_name == "execute_command" {
            if let Some(command) = arguments.get("command").and_then(|v| v.as_str()) {
                if let Some(re) = self.denied_commands.iter().find(|re| re.is_match(command)) {
                    return Decision::Deny(format!(
                        "command blocked by policy (matched `{}`)",
                        re.as_str()
                    ));
                }
            }
        }

        Decision::Allow
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_allow_all_allows_mutating_tool() {
        let policy = ToolPolicy::allow_all();
        let decision = policy.decide(
            "file_write",
            Some(ToolAnnotations::default()),
            &serde_json::json!({}),
        );
        assert_eq!(decision, Decision::Allow);
    }

    #[test]
    fn test_read_only_mode_denies_mutation_allows_reads() {
        let policy = ToolPolicy::new(PolicyMode::ReadOnly, &[]).unwrap();
        // A mutating tool is denied.
        assert!(matches!(
            policy.decide(
                "file_write",
                Some(ToolAnnotations::default()),
                &serde_json::json!({})
            ),
            Decision::Deny(_)
        ));
        // A read-only tool is allowed.
        assert_eq!(
            policy.decide(
                "file_read",
                Some(ToolAnnotations::read_only()),
                &serde_json::json!({})
            ),
            Decision::Allow
        );
        // An unknown tool (no annotations) is treated as mutating and denied.
        assert!(matches!(
            policy.decide("mystery", None, &serde_json::json!({})),
            Decision::Deny(_)
        ));
    }

    #[test]
    fn test_denylist_blocks_matching_command() {
        let policy = ToolPolicy::new(PolicyMode::AllowAll, &[r"rm\s+-rf\s+/".to_string()]).unwrap();
        let denied = policy.decide(
            "execute_command",
            None,
            &serde_json::json!({"command": "rm -rf / --no-preserve-root"}),
        );
        assert!(matches!(denied, Decision::Deny(_)), "{denied:?}");
        // A harmless command is allowed.
        assert_eq!(
            policy.decide(
                "execute_command",
                None,
                &serde_json::json!({"command": "ls -la"})
            ),
            Decision::Allow
        );
    }

    #[test]
    fn test_denylist_only_applies_to_execute_command() {
        let policy = ToolPolicy::new(PolicyMode::AllowAll, &[r"secret".to_string()]).unwrap();
        // The pattern would match, but this isn't execute_command, so it's allowed.
        assert_eq!(
            policy.decide(
                "file_write",
                Some(ToolAnnotations::default()),
                &serde_json::json!({"content": "secret"})
            ),
            Decision::Allow
        );
    }

    #[test]
    fn test_new_rejects_bad_regex() {
        assert!(ToolPolicy::new(PolicyMode::AllowAll, &["(unclosed".to_string()]).is_err());
    }
}
