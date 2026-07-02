// summary: Decide which CMUX Feed action buttons are valid for pending rows.
// purpose: Share Codex app-server permission action policy between notifications and the right sidebar.
// inputs: Feed row source metadata plus hook or app-server tool_input JSON.
// returns/effects: Returns deterministic button specs without mutating Feed state.

use serde_json::Value;

const DEFAULT_PERMISSION_ACTIONS: &[(&str, &str)] = &[
    ("Once", "once"),
    ("Always", "always"),
    ("Bypass", "bypass"),
    ("Deny", "deny"),
];

// purpose: Select valid permission action buttons for a Feed row.
// inputs: Feed source name and the original or listed Feed event row.
// returns/effects: Returns CMUX-compatible modes, filtering Codex app-server rows by advertised decisions.
pub(crate) fn permission_action_specs(
    source: &str,
    event: &Value,
) -> Vec<(&'static str, &'static str)> {
    let Some(tool_input) = event.get("tool_input").or_else(|| event.get("toolInput")) else {
        return DEFAULT_PERMISSION_ACTIONS.to_vec();
    };
    if source != "codex" || app_server_method(tool_input).is_none() {
        return DEFAULT_PERMISSION_ACTIONS.to_vec();
    }

    let capabilities = codex_app_server_capabilities(tool_input);
    let mut actions = Vec::new();
    if capabilities.once {
        actions.push(("Once", "once"));
    }
    if capabilities.always {
        actions.push(("Always", "always"));
    }
    if capabilities.all {
        actions.push(("All", "all"));
    }
    actions.push(("Deny", "deny"));
    actions
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct CodexAppServerCapabilities {
    once: bool,
    always: bool,
    all: bool,
}

// purpose: Match CMUX's Codex app-server permission-mode capability policy.
// inputs: Tool input JSON emitted by the Codex app-server approval bridge.
// returns/effects: Returns which Feed permission modes can map back to app-server decisions.
fn codex_app_server_capabilities(tool_input: &Value) -> CodexAppServerCapabilities {
    let decisions = available_decisions(tool_input);
    let accepts_once = decision_available_or_unspecified(&decisions, "accept");
    let accepts_session = decision_available_or_unspecified(&decisions, "acceptForSession");
    match app_server_method(tool_input) {
        Some("item/permissions/requestApproval") => CodexAppServerCapabilities {
            once: true,
            always: true,
            all: true,
        },
        Some("item/commandExecution/requestApproval") => CodexAppServerCapabilities {
            once: accepts_once,
            always: accepts_session,
            all: codex_supports_amendment_decision(tool_input, decisions.as_deref()),
        },
        Some("item/fileChange/requestApproval") => CodexAppServerCapabilities {
            once: accepts_once,
            always: accepts_session,
            all: false,
        },
        _ => CodexAppServerCapabilities {
            once: accepts_once,
            always: accepts_session,
            all: false,
        },
    }
}

// purpose: Read the app-server approval method from a Feed tool_input object.
// inputs: Tool input JSON.
// returns/effects: Returns a non-empty method string when present.
fn app_server_method(tool_input: &Value) -> Option<&str> {
    let method = tool_input.get("app_server_method")?.as_str()?.trim();
    (!method.is_empty()).then_some(method)
}

// purpose: Extract app-server decision names from either direct or snapshot fields.
// inputs: Tool input JSON with optional available_decisions or approval_params.
// returns/effects: Returns None when the app-server did not advertise a bounded decision set.
fn available_decisions(tool_input: &Value) -> Option<Vec<String>> {
    let raw = tool_input
        .get("available_decisions")
        .or_else(|| tool_input.get("availableDecisions"))
        .or_else(|| {
            tool_input
                .get("approval_params")
                .and_then(|params| params.get("availableDecisions"))
        })
        .or_else(|| {
            tool_input
                .get("approval_params")
                .and_then(|params| params.get("available_decisions"))
        })?;
    Some(decision_names(raw))
}

// purpose: Normalize Codex app-server decision descriptors into string names.
// inputs: JSON array containing names or one-key decision objects.
// returns/effects: Returns names in source order and ignores malformed entries.
fn decision_names(raw: &Value) -> Vec<String> {
    raw.as_array()
        .map(|values| {
            values
                .iter()
                .filter_map(|value| {
                    value.as_str().map(str::to_string).or_else(|| {
                        value
                            .as_object()
                            .and_then(|object| object.keys().next().map(String::from))
                    })
                })
                .collect()
        })
        .unwrap_or_default()
}

// purpose: Check whether a decision is available when the app-server provided a bounded set.
// inputs: Optional decision list and one desired app-server decision name.
// returns/effects: Returns true when the list is absent or includes the requested decision.
fn decision_available_or_unspecified(decisions: &Option<Vec<String>>, decision: &str) -> bool {
    decisions
        .as_ref()
        .map(|values| values.iter().any(|value| value == decision))
        .unwrap_or(true)
}

// purpose: Decide whether the Feed `all` action can map to an app-server amendment decision.
// inputs: Tool input JSON plus optional normalized decision names.
// returns/effects: Returns true only when an advertised amendment has matching payload data.
fn codex_supports_amendment_decision(tool_input: &Value, decisions: Option<&[String]>) -> bool {
    if has_non_null(tool_input.get("proposed_execpolicy_amendment"))
        && codex_decision_available_or_unspecified(decisions, "acceptWithExecpolicyAmendment")
    {
        return true;
    }
    has_non_empty_array(tool_input.get("proposed_network_policy_amendments"))
        && codex_decision_available_or_unspecified(decisions, "applyNetworkPolicyAmendment")
}

// purpose: Check amendment decision availability while treating missing lists as unbounded.
// inputs: Optional decision names and one amendment decision name.
// returns/effects: Returns true when the decision list is absent or contains the name.
fn codex_decision_available_or_unspecified(decisions: Option<&[String]>, decision: &str) -> bool {
    decisions
        .map(|values| values.iter().any(|value| value == decision))
        .unwrap_or(true)
}

// purpose: Test whether an optional JSON value is materially present.
// inputs: Optional JSON value.
// returns/effects: Returns false for absent or null values.
fn has_non_null(value: Option<&Value>) -> bool {
    value.is_some_and(|value| !value.is_null())
}

// purpose: Test whether an optional JSON value is a non-empty array.
// inputs: Optional JSON value.
// returns/effects: Returns false for absent, null, non-array, or empty values.
fn has_non_empty_array(value: Option<&Value>) -> bool {
    value
        .and_then(Value::as_array)
        .is_some_and(|values| !values.is_empty())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    // purpose: Verify app-server command approvals expose only actions Codex can accept.
    // inputs: Codex command approval tool_input with accept and decline decisions.
    // returns/effects: Asserts `always` and `all` are hidden while `once` and `deny` remain.
    #[test]
    fn codex_app_server_command_actions_follow_available_decisions() {
        let event = json!({
            "tool_input": {
                "app_server_method": "item/commandExecution/requestApproval",
                "available_decisions": ["accept", "decline"]
            }
        });

        assert_eq!(
            permission_action_specs("codex", &event),
            vec![("Once", "once"), ("Deny", "deny")]
        );
    }

    // purpose: Verify the Feed `all` action appears for supported Codex amendment approvals.
    // inputs: Codex command approval tool_input with an execpolicy amendment decision.
    // returns/effects: Asserts only `all` and `deny` are exposed for the row.
    #[test]
    fn codex_app_server_all_action_requires_supported_amendment() {
        let event = json!({
            "tool_input": {
                "app_server_method": "item/commandExecution/requestApproval",
                "available_decisions": [{"acceptWithExecpolicyAmendment": {}}],
                "proposed_execpolicy_amendment": [{"kind": "prefix", "value": "npm test"}]
            }
        });

        assert_eq!(
            permission_action_specs("codex", &event),
            vec![("All", "all"), ("Deny", "deny")]
        );
    }

    // purpose: Verify broad permissions approvals support all persistent Feed modes.
    // inputs: Codex permissions approval tool_input.
    // returns/effects: Asserts `once`, `always`, `all`, and `deny` actions are exposed.
    #[test]
    fn codex_app_server_permissions_support_all_permission_modes() {
        let event = json!({
            "tool_input": {
                "app_server_method": "item/permissions/requestApproval",
                "permissions": {"network": {"enabled": true}}
            }
        });

        assert_eq!(
            permission_action_specs("codex", &event),
            vec![
                ("Once", "once"),
                ("Always", "always"),
                ("All", "all"),
                ("Deny", "deny")
            ]
        );
    }

    // purpose: Verify legacy hook Feed rows retain the existing action set.
    // inputs: Non-app-server Codex Feed tool_input.
    // returns/effects: Asserts no app-server capability filtering is applied.
    #[test]
    fn non_app_server_rows_keep_existing_permission_actions() {
        let event = json!({"tool_input": {"command": "cargo test"}});

        assert_eq!(
            permission_action_specs("codex", &event),
            DEFAULT_PERMISSION_ACTIONS.to_vec()
        );
    }
}
