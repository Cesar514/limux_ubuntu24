// summary: Provide Codex Teams helpers for CMUX app-server approval parity.
// purpose: Convert Codex app-server approval requests to Feed events and map Feed decisions back.
// inputs: Codex app-server JSON-RPC method names, params, Feed responses, CLI args, and cwd state.
// returns/effects: Returns bounded JSON payloads and validates explicit working directories loudly.

#![allow(dead_code)]

use std::collections::BTreeSet;
use std::env;
use std::path::{Component, Path, PathBuf};

use anyhow::{bail, Result};
use serde_json::{json, Map, Value};

const PARAM_STRING_LIMIT: usize = 4_096;
const PARAM_COLLECTION_LIMIT: usize = 50;
const PARAM_DEPTH_LIMIT: usize = 5;
const ITEM_CHANGE_LIMIT: usize = 20;

/// purpose: Build the CMUX-shaped Feed event for a Codex app-server approval request.
/// inputs: Codex app-server method, JSON-RPC request id, params, workspace id, and optional related item.
/// returns/effects: Returns an actionable `PermissionRequest` Feed event with bounded tool input.
pub fn feed_event(
    method: &str,
    request_id: &Value,
    params: &Map<String, Value>,
    workspace_id: &str,
    related_item: Option<&Map<String, Value>>,
) -> Value {
    let request_id_text = request_id_string(request_id);
    let thread_id = string_value(params, &["threadId", "thread_id", "threadID"])
        .unwrap_or_else(|| "unknown".to_string());
    let turn_id = string_value(params, &["turnId", "turn_id"]);
    let item_id = string_value(params, &["approvalId", "approval_id", "itemId", "item_id"])
        .unwrap_or_else(|| request_id_text.clone());
    let cwd = string_value(params, &["cwd"]);
    let reason = string_value(params, &["reason"]);
    let command = string_value(params, &["command"]);
    let mut tool_input = Map::new();

    tool_input.insert("app_server_method".to_string(), json!(method));
    tool_input.insert("request_id".to_string(), json!(request_id_text));
    tool_input.insert("item_id".to_string(), json!(item_id));
    tool_input.insert(
        "approval_params".to_string(),
        approval_params_snapshot(params),
    );
    insert_optional_string(&mut tool_input, "turn_id", turn_id.as_deref());
    insert_optional_string(&mut tool_input, "reason", reason.as_deref());
    insert_optional_string(&mut tool_input, "command", command.as_deref());
    insert_optional_string(&mut tool_input, "cwd", cwd.as_deref());
    if let Some(value) = string_value(params, &["approvalId", "approval_id"]) {
        tool_input.insert("approval_id".to_string(), json!(value));
    }
    set_bounded_tool_input(
        &mut tool_input,
        "grant_root",
        first_value(params, &["grantRoot", "grant_root"]),
    );
    if let Some(value) = first_value(params, &["availableDecisions", "available_decisions"]) {
        tool_input.insert(
            "available_decisions".to_string(),
            decision_names_value(value),
        );
    }
    set_bounded_tool_input(&mut tool_input, "permissions", params.get("permissions"));
    set_bounded_tool_input(
        &mut tool_input,
        "network_approval_context",
        first_value(
            params,
            &["networkApprovalContext", "network_approval_context"],
        ),
    );
    set_bounded_tool_input(
        &mut tool_input,
        "additional_permissions",
        first_value(params, &["additionalPermissions", "additional_permissions"]),
    );
    set_bounded_tool_input(
        &mut tool_input,
        "command_actions",
        first_value(params, &["commandActions", "command_actions"]),
    );
    set_bounded_tool_input(
        &mut tool_input,
        "proposed_execpolicy_amendment",
        first_value(
            params,
            &[
                "proposedExecpolicyAmendment",
                "proposed_execpolicy_amendment",
            ],
        ),
    );
    set_bounded_tool_input(
        &mut tool_input,
        "proposed_network_policy_amendments",
        first_value(
            params,
            &[
                "proposedNetworkPolicyAmendments",
                "proposed_network_policy_amendments",
            ],
        ),
    );
    merge_related_item(
        &mut tool_input,
        related_item,
        command.as_deref(),
        cwd.as_deref(),
    );

    let mut context = Map::new();
    context.insert("permissionMode".to_string(), json!("codex app-server"));
    insert_optional_string(&mut context, "assistantPreamble", reason.as_deref());
    insert_optional_string(&mut context, "toolSummary", command.as_deref());

    let mut event = Map::new();
    event.insert(
        "session_id".to_string(),
        json!(format!("codex-{thread_id}")),
    );
    event.insert("hook_event_name".to_string(), json!("PermissionRequest"));
    event.insert("_source".to_string(), json!("codex"));
    event.insert("workspace_id".to_string(), json!(workspace_id));
    event.insert("tool_name".to_string(), json!(tool_name_for_method(method)));
    event.insert("tool_input".to_string(), Value::Object(tool_input));
    event.insert("context".to_string(), Value::Object(context));
    event.insert(
        "_opencode_request_id".to_string(),
        json!(format!("codex-app-server-{item_id}")),
    );
    insert_optional_string(&mut event, "cwd", cwd.as_deref());
    Value::Object(event)
}

/// purpose: Extract the normalized permission mode from a blocking Feed response.
/// inputs: Feed `feed.push` response JSON.
/// returns/effects: Returns a lower-case mode only for resolved permission decisions.
pub fn permission_mode_from_feed_push_response(response: &Value) -> Option<String> {
    let decision = response.get("decision")?.as_object()?;
    if response.get("status")?.as_str()? != "resolved"
        || decision.get("kind")?.as_str()? != "permission"
    {
        return None;
    }
    let mode = decision.get("mode")?.as_str()?.trim().to_ascii_lowercase();
    if mode.is_empty() {
        None
    } else {
        Some(mode)
    }
}

/// purpose: Convert a Feed permission mode into the Codex app-server approval response shape.
/// inputs: Codex app-server method, original approval params, and normalized Feed mode.
/// returns/effects: Returns `None` for non-approval methods; denies unknown modes fail closed.
pub fn app_server_approval_response(
    method: &str,
    params: &Map<String, Value>,
    mode: &str,
) -> Option<Value> {
    match method {
        "item/commandExecution/requestApproval" => Some(json!({
            "decision": command_approval_decision(params, mode),
        })),
        "item/fileChange/requestApproval" => Some(json!({
            "decision": file_change_approval_decision(params, mode),
        })),
        "item/permissions/requestApproval" => Some(permissions_approval_response(params, mode)),
        _ => None,
    }
}

/// purpose: Snapshot a Codex app-server item without carrying unbounded output or patches.
/// inputs: A Codex app-server item object.
/// returns/effects: Returns only known scalar fields and the first bounded 20 change entries.
pub fn approval_item_snapshot(item: &Map<String, Value>) -> Value {
    let mut snapshot = Map::new();
    for key in [
        "id",
        "type",
        "threadId",
        "thread_id",
        "turnId",
        "turn_id",
        "command",
        "cwd",
        "path",
        "status",
    ] {
        if let Some(value) = item.get(key).and_then(bounded_item_value) {
            snapshot.insert(key.to_string(), value);
        }
    }
    if let Some(changes) = item.get("changes").and_then(Value::as_array) {
        let values = changes
            .iter()
            .filter_map(Value::as_object)
            .take(ITEM_CHANGE_LIMIT)
            .map(snapshot_change)
            .collect();
        snapshot.insert("changes".to_string(), Value::Array(values));
    }
    Value::Object(snapshot)
}

/// purpose: Resolve Codex `-C`, `--cd`, and `--cwd` arguments against a base directory.
/// inputs: Raw Codex args and the process base directory.
/// returns/effects: Returns a normalized absolute path when an explicit cwd option is present.
pub fn resolved_working_directory(
    command_args: &[String],
    base_directory: &Path,
) -> Option<PathBuf> {
    let mut index = 0usize;
    let mut requested = None;
    while index < command_args.len() {
        let arg = &command_args[index];
        if arg == "--" {
            break;
        }
        if matches!(arg.as_str(), "-C" | "--cd" | "--cwd") && index + 1 < command_args.len() {
            requested = Some(command_args[index + 1].clone());
            index += 2;
            continue;
        }
        for prefix in ["-C=", "--cd=", "--cwd="] {
            if let Some(value) = arg.strip_prefix(prefix) {
                requested = Some(value.to_string());
            }
        }
        index += 1;
    }
    let requested = requested?.trim().to_string();
    if requested.is_empty() {
        return None;
    }
    let expanded = expand_tilde(&requested);
    let path = PathBuf::from(expanded);
    if path.is_absolute() {
        Some(normalize_path(&path))
    } else {
        Some(normalize_path(&base_directory.join(path)))
    }
}

/// purpose: Validate that an explicit Codex Teams cwd exists before process launch.
/// inputs: Raw Codex args and the process base directory.
/// returns/effects: Fails loudly if the explicit cwd is missing or not a directory.
pub fn validate_working_directory(command_args: &[String], base_directory: &Path) -> Result<()> {
    let Some(cwd) = resolved_working_directory(command_args, base_directory) else {
        return Ok(());
    };
    if cwd.is_dir() {
        Ok(())
    } else {
        bail!("cmux codex-teams cwd does not exist: {}", cwd.display())
    }
}

/// purpose: Convert JSON-RPC ids into CMUX-compatible request-id strings.
/// inputs: A JSON id value.
/// returns/effects: Returns strings and numbers directly; other JSON values are compact encoded.
pub fn request_id_string(request_id: &Value) -> String {
    match request_id {
        Value::String(value) => value.clone(),
        Value::Number(value) => value.to_string(),
        other => other.to_string(),
    }
}

/// purpose: Read the first non-empty string or number-like value from an object.
/// inputs: JSON object and ordered key aliases.
/// returns/effects: Returns trimmed strings or number text.
pub fn string_value(object: &Map<String, Value>, keys: &[&str]) -> Option<String> {
    keys.iter().find_map(|key| {
        let value = object.get(*key)?;
        match value {
            Value::String(text) => {
                let trimmed = text.trim();
                if trimmed.is_empty() {
                    None
                } else {
                    Some(trimmed.to_string())
                }
            }
            Value::Number(number) => Some(number.to_string()),
            _ => None,
        }
    })
}

fn approval_params_snapshot(params: &Map<String, Value>) -> Value {
    let mut snapshot = Map::new();
    for key in [
        "threadId",
        "thread_id",
        "turnId",
        "turn_id",
        "itemId",
        "item_id",
        "approvalId",
        "approval_id",
        "environmentId",
        "environment_id",
        "cwd",
        "reason",
        "command",
        "grantRoot",
        "grant_root",
        "availableDecisions",
        "available_decisions",
        "permissions",
        "networkApprovalContext",
        "network_approval_context",
        "additionalPermissions",
        "additional_permissions",
        "commandActions",
        "command_actions",
        "proposedExecpolicyAmendment",
        "proposed_execpolicy_amendment",
        "proposedNetworkPolicyAmendments",
        "proposed_network_policy_amendments",
    ] {
        if let Some(value) = params
            .get(key)
            .and_then(|value| bounded_param_value(value, 0))
        {
            snapshot.insert(key.to_string(), value);
        }
    }
    Value::Object(snapshot)
}

fn set_bounded_tool_input(tool_input: &mut Map<String, Value>, key: &str, value: Option<&Value>) {
    if let Some(value) = value.and_then(|value| bounded_param_value(value, 0)) {
        tool_input.insert(key.to_string(), value);
    }
}

fn bounded_param_value(value: &Value, depth: usize) -> Option<Value> {
    if depth > PARAM_DEPTH_LIMIT {
        return None;
    }
    match value {
        Value::String(text) => Some(Value::String(limit_string(text, PARAM_STRING_LIMIT))),
        Value::Number(_) | Value::Bool(_) | Value::Null => Some(value.clone()),
        Value::Array(values) => Some(Value::Array(
            values
                .iter()
                .take(PARAM_COLLECTION_LIMIT)
                .filter_map(|value| bounded_param_value(value, depth + 1))
                .collect(),
        )),
        Value::Object(object) => {
            let mut snapshot = Map::new();
            for key in object.keys().take(PARAM_COLLECTION_LIMIT) {
                if let Some(value) = object
                    .get(key)
                    .and_then(|value| bounded_param_value(value, depth + 1))
                {
                    snapshot.insert(key.clone(), value);
                }
            }
            Some(Value::Object(snapshot))
        }
    }
}

fn bounded_item_value(value: &Value) -> Option<Value> {
    match value {
        Value::String(text) => Some(Value::String(limit_string(text, PARAM_STRING_LIMIT))),
        Value::Number(_) | Value::Bool(_) | Value::Null => Some(value.clone()),
        _ => None,
    }
}

fn command_approval_decision(params: &Map<String, Value>, mode: &str) -> Value {
    if mode == "deny" {
        return json!(reject_approval_decision(params));
    }
    if mode_requests_persistent_approval(mode) && mode == "all" {
        if let Some(decision) = command_approval_amendment_decision(params) {
            return decision;
        }
    }
    if mode == "bypass" {
        if let Some(decision) = command_approval_amendment_decision(params) {
            return decision;
        }
        return accept_or_reject(params);
    }
    if mode_requests_persistent_approval(mode)
        && decision_available_or_unspecified("acceptForSession", params)
    {
        return json!("acceptForSession");
    }
    if mode_requests_persistent_approval(mode) {
        if let Some(decision) = command_approval_amendment_decision(params) {
            return decision;
        }
        return accept_or_reject(params);
    }
    if mode == "once" && decision_available_or_unspecified("accept", params) {
        return json!("accept");
    }
    json!(reject_approval_decision(params))
}

fn command_approval_amendment_decision(params: &Map<String, Value>) -> Option<Value> {
    if decision_available_or_unspecified("acceptWithExecpolicyAmendment", params) {
        if let Some(amendment) = first_value(
            params,
            &[
                "proposedExecpolicyAmendment",
                "proposed_execpolicy_amendment",
            ],
        ) {
            return Some(json!({
                "acceptWithExecpolicyAmendment": {
                    "execpolicy_amendment": amendment,
                },
            }));
        }
    }
    if decision_available_or_unspecified("applyNetworkPolicyAmendment", params) {
        let amendment = first_value(
            params,
            &[
                "proposedNetworkPolicyAmendments",
                "proposed_network_policy_amendments",
            ],
        )
        .and_then(Value::as_array)
        .and_then(|values| values.first())?;
        return Some(json!({
            "applyNetworkPolicyAmendment": {
                "network_policy_amendment": amendment,
            },
        }));
    }
    None
}

fn file_change_approval_decision(params: &Map<String, Value>, mode: &str) -> Value {
    if mode == "deny" {
        return json!(reject_approval_decision(params));
    }
    if mode_requests_persistent_approval(mode)
        && decision_available_or_unspecified("acceptForSession", params)
    {
        return json!("acceptForSession");
    }
    if mode_requests_persistent_approval(mode) {
        return accept_or_reject(params);
    }
    if mode == "once" && decision_available_or_unspecified("accept", params) {
        return json!("accept");
    }
    json!(reject_approval_decision(params))
}

fn permissions_approval_response(params: &Map<String, Value>, mode: &str) -> Value {
    if mode == "deny" || !(mode == "once" || mode_requests_persistent_approval(mode)) {
        return json!({
            "permissions": {},
            "scope": "turn",
        });
    }
    json!({
        "permissions": params.get("permissions").cloned().unwrap_or_else(|| json!({})),
        "scope": if mode_requests_persistent_approval(mode) { "session" } else { "turn" },
    })
}

fn available_decisions(params: &Map<String, Value>) -> BTreeSet<String> {
    first_value(params, &["availableDecisions", "available_decisions"])
        .map(decision_names)
        .unwrap_or_default()
        .into_iter()
        .collect()
}

fn decision_available_or_unspecified(decision: &str, params: &Map<String, Value>) -> bool {
    if first_value(params, &["availableDecisions", "available_decisions"]).is_none() {
        return true;
    }
    available_decisions(params).contains(decision)
}

fn decision_names(raw: &Value) -> Vec<String> {
    raw.as_array()
        .map(|values| {
            values
                .iter()
                .filter_map(|value| match value {
                    Value::String(text) => Some(text.clone()),
                    Value::Object(object) => object.keys().next().cloned(),
                    _ => None,
                })
                .collect()
        })
        .unwrap_or_default()
}

fn decision_names_value(raw: &Value) -> Value {
    Value::Array(decision_names(raw).into_iter().map(Value::String).collect())
}

fn reject_approval_decision(params: &Map<String, Value>) -> &'static str {
    let decisions = available_decisions(params);
    if decisions.contains("decline") || decisions.is_empty() {
        "decline"
    } else if decisions.contains("cancel") {
        "cancel"
    } else {
        "decline"
    }
}

fn mode_requests_persistent_approval(mode: &str) -> bool {
    matches!(mode, "always" | "all" | "bypass")
}

fn accept_or_reject(params: &Map<String, Value>) -> Value {
    if decision_available_or_unspecified("accept", params) {
        json!("accept")
    } else {
        json!(reject_approval_decision(params))
    }
}

fn first_value<'a>(object: &'a Map<String, Value>, keys: &[&str]) -> Option<&'a Value> {
    keys.iter().find_map(|key| object.get(*key))
}

fn insert_optional_string(object: &mut Map<String, Value>, key: &str, value: Option<&str>) {
    if let Some(value) = value {
        object.insert(key.to_string(), json!(value));
    }
}

fn merge_related_item(
    tool_input: &mut Map<String, Value>,
    related_item: Option<&Map<String, Value>>,
    command: Option<&str>,
    cwd: Option<&str>,
) {
    let Some(related_item) = related_item else {
        return;
    };
    tool_input.insert(
        "related_item".to_string(),
        Value::Object(related_item.clone()),
    );
    if command.is_none() {
        if let Some(value) = string_value(related_item, &["command"]) {
            tool_input.insert("command".to_string(), json!(value));
        }
    }
    if cwd.is_none() {
        if let Some(value) = string_value(related_item, &["cwd"]) {
            tool_input.insert("cwd".to_string(), json!(value));
        }
    }
}

fn snapshot_change(change: &Map<String, Value>) -> Value {
    let mut snapshot = Map::new();
    for key in ["path", "kind", "type", "status", "diff", "summary"] {
        if let Some(value) = change.get(key).and_then(bounded_item_value) {
            snapshot.insert(key.to_string(), value);
        }
    }
    Value::Object(snapshot)
}

fn tool_name_for_method(method: &str) -> &'static str {
    match method {
        "item/fileChange/requestApproval" => "Write",
        "item/permissions/requestApproval" => "request_permissions",
        _ => "Bash",
    }
}

fn limit_string(text: &str, limit: usize) -> String {
    text.chars().take(limit).collect()
}

fn expand_tilde(path: &str) -> String {
    if path == "~" {
        return env::var("HOME").unwrap_or_else(|_| path.to_string());
    }
    if let Some(rest) = path.strip_prefix("~/") {
        if let Ok(home) = env::var("HOME") {
            return format!("{home}/{rest}");
        }
    }
    path.to_string()
}

fn normalize_path(path: &Path) -> PathBuf {
    let mut normalized = PathBuf::new();
    for component in path.components() {
        match component {
            Component::Prefix(prefix) => normalized.push(prefix.as_os_str()),
            Component::RootDir => normalized.push(Path::new("/")),
            Component::CurDir => {}
            Component::ParentDir => {
                normalized.pop();
            }
            Component::Normal(value) => normalized.push(value),
        }
    }
    normalized
}

#[cfg(test)]
mod tests {
    use super::*;

    fn object(value: Value) -> Map<String, Value> {
        value.as_object().cloned().expect("object")
    }

    #[test]
    fn codex_teams_resolves_and_validates_explicit_working_directory_flags() {
        let base = Path::new("/tmp/cmux-base");
        assert_eq!(
            resolved_working_directory(&["-C".into(), "child".into(), "prompt".into()], base)
                .expect("cwd"),
            PathBuf::from("/tmp/cmux-base/child")
        );
        assert_eq!(
            resolved_working_directory(
                &[
                    "--cwd=/tmp/cmux-review".into(),
                    "--cd".into(),
                    "/tmp/cmux-final".into(),
                ],
                base,
            )
            .expect("cwd"),
            PathBuf::from("/tmp/cmux-final")
        );
        assert!(resolved_working_directory(
            &["--".into(), "-C".into(), "/tmp/inside-prompt".into()],
            base,
        )
        .is_none());

        let dir = tempfile::tempdir().expect("tempdir");
        validate_working_directory(
            &["-C".into(), dir.path().display().to_string()],
            Path::new("/tmp"),
        )
        .expect("existing cwd validates");
        let missing = dir.path().join("missing");
        let error = validate_working_directory(
            &["-C".into(), missing.display().to_string()],
            Path::new("/tmp"),
        )
        .expect_err("missing cwd fails");
        assert!(error
            .to_string()
            .contains("cmux codex-teams cwd does not exist"));
    }

    #[test]
    fn codex_app_server_approval_builds_actionable_feed_event() {
        let params = object(json!({
            "threadId": "thread-1",
            "turnId": "turn-1",
            "itemId": "call-1",
            "approvalId": "approval-1",
            "command": "touch /tmp/cmux-security-review",
            "cwd": "/tmp/project",
            "reason": "requires approval",
            "unboundedRawPatch": "x".repeat(8_000),
            "additionalPermissions": {"fileSystem": {"write": ["/tmp/project"]}},
            "networkApprovalContext": {"host": "example.com"},
            "commandActions": [{"type": "write", "path": "/tmp/cmux-security-review", "diff": "d".repeat(8_000)}],
            "proposedExecpolicyAmendment": [{"kind": "prefix", "value": "touch"}],
            "availableDecisions": ["accept", "acceptForSession", "decline"]
        }));
        let related = object(json!({
            "type": "commandExecution",
            "id": "call-1",
            "command": "touch /tmp/cmux-security-review",
            "cwd": "/tmp/project"
        }));

        let event = feed_event(
            "item/commandExecution/requestApproval",
            &json!(41),
            &params,
            "workspace-1",
            Some(&related),
        );

        assert_eq!(event["session_id"], "codex-thread-1");
        assert_eq!(event["hook_event_name"], "PermissionRequest");
        assert_eq!(event["_source"], "codex");
        assert_eq!(event["workspace_id"], "workspace-1");
        assert_eq!(event["_opencode_request_id"], "codex-app-server-approval-1");
        assert_eq!(event["tool_name"], "Bash");
        assert_eq!(event["cwd"], "/tmp/project");
        let tool_input = event["tool_input"].as_object().expect("tool input");
        assert_eq!(
            tool_input["app_server_method"],
            "item/commandExecution/requestApproval"
        );
        assert_eq!(tool_input["request_id"], "41");
        assert_eq!(tool_input["item_id"], "approval-1");
        assert!(tool_input["approval_params"]["unboundedRawPatch"].is_null());
        assert!(tool_input["additional_permissions"].is_object());
        assert_eq!(
            tool_input["command_actions"][0]["diff"]
                .as_str()
                .expect("diff")
                .len(),
            4_096
        );
        assert_eq!(event["context"]["permissionMode"], "codex app-server");
        assert_eq!(event["context"]["assistantPreamble"], "requires approval");
    }

    #[test]
    fn codex_app_server_permissions_approval_builds_feed_event_and_response() {
        let permissions = json!({
            "network": {"enabled": true},
            "fileSystem": {"read": ["/tmp/read"], "write": ["/tmp/write"]}
        });
        let params = object(json!({
            "threadId": "thread-1",
            "turnId": "turn-1",
            "itemId": "permissions-call",
            "environmentId": "local",
            "cwd": "/tmp/project",
            "reason": "Need broader access",
            "permissions": permissions
        }));
        let event = feed_event(
            "item/permissions/requestApproval",
            &json!("permissions-request"),
            &params,
            "workspace-1",
            None,
        );

        assert_eq!(event["tool_name"], "request_permissions");
        assert_eq!(
            event["_opencode_request_id"],
            "codex-app-server-permissions-call"
        );
        assert!(event["tool_input"]["approval_params"].is_object());
        assert!(event["tool_input"]["permissions"].is_object());

        let once =
            app_server_approval_response("item/permissions/requestApproval", &params, "once")
                .expect("once");
        assert_eq!(once["scope"], "turn");
        assert!(once["permissions"].is_object());
        let always =
            app_server_approval_response("item/permissions/requestApproval", &params, "always")
                .expect("always");
        assert_eq!(always["scope"], "session");
        let deny =
            app_server_approval_response("item/permissions/requestApproval", &params, "deny")
                .expect("deny");
        assert_eq!(deny["scope"], "turn");
        assert_eq!(
            deny["permissions"].as_object().expect("permissions").len(),
            0
        );
    }

    #[test]
    fn codex_app_server_approval_response_follows_feed_decision() {
        let params =
            object(json!({"availableDecisions": ["accept", "acceptForSession", "decline"]}));
        assert_eq!(
            permission_mode_from_feed_push_response(&json!({
                "status": "resolved",
                "decision": {"kind": "permission", "mode": "always"}
            }))
            .as_deref(),
            Some("always")
        );
        assert_eq!(
            app_server_approval_response(
                "item/commandExecution/requestApproval",
                &params,
                "always"
            )
            .expect("response")["decision"],
            "acceptForSession"
        );
        let amendment = object(json!({
            "availableDecisions": [{"acceptWithExecpolicyAmendment": {}}],
            "proposedExecpolicyAmendment": [{"kind": "prefix", "value": "npm test"}]
        }));
        assert!(app_server_approval_response(
            "item/commandExecution/requestApproval",
            &amendment,
            "always"
        )
        .expect("response")["decision"]["acceptWithExecpolicyAmendment"]
            .is_object());
        let network_once = object(json!({
            "availableDecisions": [{"applyNetworkPolicyAmendment": {}}],
            "proposedNetworkPolicyAmendments": [{"host": "example.com"}]
        }));
        assert_eq!(
            app_server_approval_response(
                "item/commandExecution/requestApproval",
                &network_once,
                "once"
            )
            .expect("response")["decision"],
            "decline"
        );
        assert_eq!(
            app_server_approval_response("item/fileChange/requestApproval", &Map::new(), "always")
                .expect("response")["decision"],
            "acceptForSession"
        );
        assert!(permission_mode_from_feed_push_response(&json!({"status": "timed_out"})).is_none());
    }

    #[test]
    fn codex_approval_item_snapshot_strips_large_payloads() {
        let item = object(json!({
            "id": "call-1",
            "type": "commandExecution",
            "command": "x".repeat(5_000),
            "cwd": "/tmp/project",
            "output": "y".repeat(100_000),
            "changes": [{"path": "/tmp/file.txt", "diff": "z".repeat(100_000), "summary": "file summary"}]
        }));
        let snapshot = approval_item_snapshot(&item);

        assert_eq!(snapshot["id"], "call-1");
        assert_eq!(snapshot["cwd"], "/tmp/project");
        assert_eq!(snapshot["command"].as_str().expect("command").len(), 4_096);
        assert!(snapshot["output"].is_null());
        assert_eq!(snapshot["changes"][0]["path"], "/tmp/file.txt");
        assert_eq!(
            snapshot["changes"][0]["diff"].as_str().expect("diff").len(),
            4_096
        );
    }
}
