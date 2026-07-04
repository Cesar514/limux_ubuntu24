// summary: Provide Codex Teams helpers for CMUX app-server approval parity.
// purpose: Convert Codex app-server approval requests to Feed events and map Feed decisions back.
// inputs: Codex app-server JSON-RPC method names, params, Feed responses, CLI args, and cwd state.
// returns/effects: Returns bounded JSON payloads and validates explicit working directories loudly.

#![allow(dead_code)]

use std::collections::{BTreeMap, BTreeSet};
use std::env;
use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Component, Path, PathBuf};
use std::time::Duration;
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{anyhow, bail, Context, Result};
use futures_util::{SinkExt, StreamExt};
use serde_json::{json, Map, Value};
use tokio::net::TcpStream;
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::{connect_async, MaybeTlsStream, WebSocketStream};

use crate::agent_hooks::codex_update_check_suppression_args;

const PARAM_STRING_LIMIT: usize = 4_096;
const PARAM_COLLECTION_LIMIT: usize = 50;
const PARAM_DEPTH_LIMIT: usize = 5;
const ITEM_CHANGE_LIMIT: usize = 20;
pub const MAX_AUTO_DEPTH: usize = 2;
pub const MANAGED_SUBAGENT_ENV_KEY: &str = "CMUX_AGENT_MANAGED_SUBAGENT";
pub const THREAD_ENV_KEY: &str = "CMUX_CODEX_TEAMS_THREAD_ID";
pub const PARENT_THREAD_ENV_KEY: &str = "CMUX_CODEX_TEAMS_PARENT_THREAD_ID";
pub const DEPTH_ENV_KEY: &str = "CMUX_CODEX_TEAMS_DEPTH";

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Spawn {
    pub parent_thread_id: String,
    pub source_depth: Option<usize>,
    pub agent_nickname: Option<String>,
    pub agent_role: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Thread {
    pub id: String,
    pub cwd: Option<String>,
    pub status_type: Option<String>,
    pub agent_nickname: Option<String>,
    pub agent_role: Option<String>,
    pub spawn: Option<Spawn>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SubagentSplitPlan {
    pub params: Value,
    pub title: String,
    pub command_text: String,
    pub startup_script: PathBuf,
}

/// purpose: Parse the thread object delivered by Codex app-server notifications.
/// inputs: Codex app-server `thread` JSON object.
/// returns/effects: Returns a normalized thread model when the object has a non-empty id.
pub fn thread_from_object(object: &Map<String, Value>) -> Option<Thread> {
    let id = object.get("id")?.as_str()?.trim();
    if id.is_empty() {
        return None;
    }
    Some(Thread {
        id: id.to_string(),
        cwd: string_value(object, &["cwd"]),
        status_type: object
            .get("status")
            .and_then(Value::as_object)
            .and_then(|status| string_value(status, &["type"])),
        agent_nickname: string_value(object, &["agentNickname", "agent_nickname"]),
        agent_role: string_value(object, &["agentRole", "agent_role"]),
        spawn: spawn_from_thread_object(object),
    })
}

/// purpose: Decide whether a Codex app-server thread is ready enough to attach.
/// inputs: Parsed thread status.
/// returns/effects: Returns false for missing status and `not_loaded` variants.
pub fn thread_may_be_attachable(thread: &Thread) -> bool {
    let Some(status_type) = thread.status_type.as_deref() else {
        return false;
    };
    let normalized = status_type.replace('_', "").to_ascii_lowercase();
    !normalized.is_empty() && normalized != "notloaded"
}

/// purpose: Build the command text used inside managed Codex subagent panes.
/// inputs: Codex executable, app-server URL, child/parent thread ids, depth, and optional PATH.
/// returns/effects: Returns a shell-quoted `env ... codex resume --remote ...` command.
pub fn resume_command_text(
    codex_executable: &str,
    app_server_url: &str,
    thread_id: &str,
    parent_thread_id: &str,
    depth: usize,
    launch_path: Option<&str>,
) -> String {
    let mut parts = vec!["env".to_string()];
    if let Some(launch_path) = nonempty_str(launch_path) {
        parts.push(format!("PATH={launch_path}"));
    }
    parts.extend([
        format!("CMUX_CODEX_TEAMS_APP_SERVER_URL={app_server_url}"),
        format!("{MANAGED_SUBAGENT_ENV_KEY}=1"),
        format!("{THREAD_ENV_KEY}={thread_id}"),
        format!("{PARENT_THREAD_ENV_KEY}={parent_thread_id}"),
        format!("{DEPTH_ENV_KEY}={}", depth.max(1)),
        codex_executable.to_string(),
        "resume".to_string(),
        "--remote".to_string(),
        app_server_url.to_string(),
        thread_id.to_string(),
    ]);
    parts.extend(codex_update_check_suppression_args(&[]));
    parts
        .iter()
        .map(|part| shell_quote(part))
        .collect::<Vec<_>>()
        .join(" ")
}

/// purpose: Build the root Codex arguments for a private app-server launch.
/// inputs: App-server URL and user-supplied Codex arguments.
/// returns/effects: Inserts `--remote <url>` after `resume`/`fork` selectors or before normal args.
pub fn root_codex_arguments(app_server_url: &str, command_args: &[String]) -> Vec<String> {
    match command_args.first().map(String::as_str) {
        Some("resume" | "fork") => {
            let mut args = vec![
                command_args[0].clone(),
                "--remote".to_string(),
                app_server_url.to_string(),
            ];
            args.extend(command_args.iter().skip(1).cloned());
            args
        }
        _ => {
            let mut args = vec!["--remote".to_string(), app_server_url.to_string()];
            args.extend(command_args.iter().cloned());
            args
        }
    }
}

/// purpose: Build CMUX-compatible `surface.split` params for a managed Codex subagent.
/// inputs: Workspace/root surface, previous managed surface, Codex thread, spawn data, and launch metadata.
/// returns/effects: Writes a one-shot startup script and returns split params plus tab title.
pub fn subagent_split_plan(
    workspace_id: &str,
    root_surface_id: &str,
    last_agent_surface_id: Option<&str>,
    thread: &Thread,
    spawn: &Spawn,
    launch: &SubagentLaunch<'_>,
) -> Result<SubagentSplitPlan> {
    let command_text = resume_command_text(
        launch.codex_executable,
        launch.app_server_url,
        &thread.id,
        &spawn.parent_thread_id,
        launch.depth,
        launch.launch_path,
    );
    let startup_script = write_startup_script(&command_text, thread.cwd.as_deref())?;
    let target_surface_id = last_agent_surface_id.unwrap_or(root_surface_id);
    let direction = if last_agent_surface_id.is_some() {
        "down"
    } else {
        "right"
    };
    let mut params = Map::new();
    params.insert("workspace_id".to_string(), json!(workspace_id));
    params.insert("surface_id".to_string(), json!(target_surface_id));
    params.insert("direction".to_string(), json!(direction));
    params.insert("focus".to_string(), json!(false));
    params.insert(
        "initial_command".to_string(),
        json!(startup_script.display().to_string()),
    );
    params.insert(
        "tmux_start_command".to_string(),
        json!(command_text.clone()),
    );
    params.insert(
        "startup_environment".to_string(),
        startup_environment(thread, spawn, launch.depth),
    );
    if let Some(cwd) = nonempty_str(thread.cwd.as_deref()) {
        params.insert("working_directory".to_string(), json!(cwd));
    }
    Ok(SubagentSplitPlan {
        params: Value::Object(params),
        title: title(thread, spawn, launch.depth),
        command_text,
        startup_script,
    })
}

#[derive(Debug, Clone, Copy)]
pub struct SubagentLaunch<'a> {
    pub codex_executable: &'a str,
    pub app_server_url: &'a str,
    pub launch_path: Option<&'a str>,
    pub depth: usize,
}

pub struct AppServerConnection {
    stream: WebSocketStream<MaybeTlsStream<TcpStream>>,
    next_request_id: u64,
}

impl AppServerConnection {
    /// purpose: Connect to a Codex app-server websocket endpoint.
    /// inputs: `ws://` or `wss://` app-server URL.
    /// returns/effects: Opens the websocket or fails loudly on invalid URLs/handshake errors.
    pub async fn connect(app_server_url: &str) -> Result<Self> {
        let parsed = url::Url::parse(app_server_url)
            .with_context(|| format!("Invalid Codex app-server URL: {app_server_url}"))?;
        if !matches!(parsed.scheme(), "ws" | "wss") {
            bail!("Codex app-server URL must use ws:// or wss://: {app_server_url}");
        }
        let (stream, _) = connect_async(parsed.as_str())
            .await
            .with_context(|| format!("failed to connect to Codex app-server {app_server_url}"))?;
        Ok(Self {
            stream,
            next_request_id: 1,
        })
    }

    /// purpose: Perform the Codex experimental app-server initialize handshake.
    /// inputs: Client name/version, optional notification opt-out list, and response timeout.
    /// returns/effects: Sends `initialize`, then `initialized`, failing on app-server errors.
    pub async fn initialize(
        &mut self,
        client_name: &str,
        version: &str,
        opt_out_notification_methods: &[&str],
        response_timeout: Duration,
    ) -> Result<Value> {
        let mut capabilities = Map::new();
        capabilities.insert("experimentalApi".to_string(), json!(true));
        if !opt_out_notification_methods.is_empty() {
            capabilities.insert(
                "optOutNotificationMethods".to_string(),
                json!(opt_out_notification_methods),
            );
        }
        let result = self
            .request(
                "initialize",
                Some(json!({
                    "clientInfo": {
                        "name": client_name,
                        "title": "limux Codex Teams",
                        "version": version,
                    },
                    "capabilities": capabilities,
                })),
                |_| Ok(()),
                response_timeout,
            )
            .await?;
        self.send_object(json!({"method": "initialized"}))
            .await
            .context("failed to send Codex app-server initialized notification")?;
        Ok(result)
    }

    /// purpose: Send a Codex app-server JSON-RPC request and wait for its matching response.
    /// inputs: Method, optional params, notification callback, and response timeout.
    /// returns/effects: Invokes callback for interleaved notifications and returns result payload.
    pub async fn request<F>(
        &mut self,
        method: &str,
        params: Option<Value>,
        mut notification_handler: F,
        response_timeout: Duration,
    ) -> Result<Value>
    where
        F: FnMut(Value) -> Result<()>,
    {
        let request_id = self.next_request_id;
        self.next_request_id = self.next_request_id.saturating_add(1);
        let mut request = Map::new();
        request.insert("id".to_string(), json!(request_id));
        request.insert("method".to_string(), json!(method));
        if let Some(params) = params {
            request.insert("params".to_string(), params);
        }
        self.send_object(Value::Object(request)).await?;

        loop {
            let message = tokio::time::timeout(response_timeout, self.receive_object())
                .await
                .map_err(|_| anyhow!("Timed out waiting for Codex app-server response"))??;
            if message.get("method").and_then(Value::as_str).is_some() {
                notification_handler(message)?;
                continue;
            }
            if !message_has_id(&message, request_id) {
                continue;
            }
            if let Some(error) = message.get("error") {
                let text = error
                    .get("message")
                    .and_then(Value::as_str)
                    .unwrap_or("Codex app-server request failed");
                bail!("{text}");
            }
            return Ok(message.get("result").cloned().unwrap_or(Value::Null));
        }
    }

    /// purpose: Send a JSON-RPC response to a Codex app-server request.
    /// inputs: Original request id and result payload.
    /// returns/effects: Writes a websocket text frame.
    pub async fn respond(&mut self, request_id: Value, result: Value) -> Result<()> {
        self.send_object(json!({
            "id": request_id,
            "result": result,
        }))
        .await
    }

    /// purpose: Send a JSON-RPC error response to a Codex app-server request.
    /// inputs: Original request id, JSON-RPC error code, and message.
    /// returns/effects: Writes a websocket text frame.
    pub async fn respond_error(
        &mut self,
        request_id: Value,
        code: i64,
        message: &str,
    ) -> Result<()> {
        self.send_object(json!({
            "id": request_id,
            "error": {
                "code": code,
                "message": message,
            },
        }))
        .await
    }

    /// purpose: Receive the next Codex app-server JSON object from the websocket.
    /// inputs: Current websocket stream.
    /// returns/effects: Fails loudly for closed sockets, invalid JSON, or non-object frames.
    pub async fn receive_object(&mut self) -> Result<Value> {
        loop {
            let Some(message) = self.stream.next().await else {
                bail!("Codex app-server websocket closed");
            };
            match message.context("failed to receive Codex app-server websocket frame")? {
                Message::Text(text) => return decode_object(text.as_str()),
                Message::Binary(bytes) => return decode_object(std::str::from_utf8(&bytes)?),
                Message::Ping(bytes) => {
                    self.stream.send(Message::Pong(bytes)).await?;
                }
                Message::Pong(_) => {}
                Message::Close(_) => bail!("Codex app-server websocket closed"),
                Message::Frame(_) => {}
            }
        }
    }

    async fn send_object(&mut self, object: Value) -> Result<()> {
        if !object.is_object() {
            bail!("Codex app-server JSON-RPC frame must be an object");
        }
        let text =
            serde_json::to_string(&object).context("failed to encode Codex app-server frame")?;
        self.stream
            .send(Message::Text(text.into()))
            .await
            .context("failed to send Codex app-server websocket frame")
    }
}

fn decode_object(text: &str) -> Result<Value> {
    let value: Value = serde_json::from_str(text).context("Codex app-server sent invalid JSON")?;
    if value.is_object() {
        Ok(value)
    } else {
        bail!("Codex app-server sent non-object JSON")
    }
}

fn message_has_id(message: &Value, request_id: u64) -> bool {
    let Some(id) = message.get("id") else {
        return false;
    };
    if id.as_u64() == Some(request_id) {
        return true;
    }
    id.as_str()
        .map(|value| value == request_id.to_string())
        .unwrap_or(false)
}

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

fn spawn_from_thread_object(object: &Map<String, Value>) -> Option<Spawn> {
    let source = object.get("source")?.as_object()?;
    let subagent = first_value(source, &["subAgent", "subagent"])?.as_object()?;
    let spawn = first_value(subagent, &["thread_spawn", "threadSpawn"])?.as_object()?;
    let parent_thread_id = string_value(spawn, &["parent_thread_id", "parentThreadId"])?;
    Some(Spawn {
        parent_thread_id,
        source_depth: spawn.get("depth").and_then(number_to_usize),
        agent_nickname: string_value(spawn, &["agent_nickname", "agentNickname"]),
        agent_role: string_value(spawn, &["agent_role", "agentRole"]),
    })
}

fn number_to_usize(value: &Value) -> Option<usize> {
    if let Some(value) = value.as_u64() {
        usize::try_from(value).ok()
    } else if let Some(value) = value.as_i64() {
        usize::try_from(value).ok()
    } else {
        None
    }
}

fn startup_environment(thread: &Thread, spawn: &Spawn, depth: usize) -> Value {
    let mut env = BTreeMap::new();
    env.insert(MANAGED_SUBAGENT_ENV_KEY.to_string(), "1".to_string());
    env.insert(THREAD_ENV_KEY.to_string(), thread.id.clone());
    env.insert(
        PARENT_THREAD_ENV_KEY.to_string(),
        spawn.parent_thread_id.clone(),
    );
    env.insert(DEPTH_ENV_KEY.to_string(), depth.max(1).to_string());
    json!(env)
}

fn title(thread: &Thread, spawn: &Spawn, depth: usize) -> String {
    let label = [
        spawn.agent_role.as_deref(),
        thread.agent_role.as_deref(),
        spawn.agent_nickname.as_deref(),
        thread.agent_nickname.as_deref(),
    ]
    .into_iter()
    .flatten()
    .map(str::trim)
    .find(|value| !value.is_empty())
    .map(str::to_string)
    .unwrap_or_else(|| thread.id.chars().take(8).collect());
    format!("Codex d{}: {}", depth.max(1), label)
}

fn write_startup_script(command_text: &str, cwd: Option<&str>) -> Result<PathBuf> {
    let path = env::temp_dir().join(format!(
        "limux-codex-teams-{}-{}.sh",
        std::process::id(),
        unique_millis()
    ));
    let mut lines = vec![
        "#!/bin/sh".to_string(),
        "rm -f -- \"$0\" 2>/dev/null || true".to_string(),
    ];
    if let Some(cwd) = nonempty_str(cwd) {
        let quoted = shell_quote(cwd);
        lines.push(format!(
            "{{ cd -- {quoted} 2>/dev/null || [ ! -d {quoted} ]; }} || exit $?"
        ));
    }
    lines.push(format!(
        "exec \"${{SHELL:-/bin/sh}}\" -lc {}",
        shell_quote(command_text)
    ));
    fs::write(&path, format!("{}\n", lines.join("\n")))?;
    let mut permissions = fs::metadata(&path)?.permissions();
    permissions.set_mode(0o700);
    fs::set_permissions(&path, permissions)?;
    Ok(path)
}

fn unique_millis() -> u128 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_millis())
        .unwrap_or(0)
}

fn shell_quote(value: &str) -> String {
    format!("'{}'", value.replace('\'', "'\"'\"'"))
}

fn nonempty_str(value: Option<&str>) -> Option<&str> {
    value.map(str::trim).filter(|value| !value.is_empty())
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

    #[tokio::test]
    async fn codex_app_server_connection_handles_initialize_request_and_respond() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind websocket server");
        let address = listener.local_addr().expect("local addr");
        let server = tokio::spawn(async move {
            let (stream, _) = listener.accept().await.expect("accept websocket");
            let mut websocket = tokio_tungstenite::accept_async(stream)
                .await
                .expect("websocket handshake");

            let initialize = next_json_object(&mut websocket).await;
            assert_eq!(initialize["method"], "initialize");
            assert_eq!(
                initialize["params"]["capabilities"]["experimentalApi"],
                true
            );
            websocket
                .send(Message::Text(
                    json!({"method": "thread/updated", "params": {"ignored": true}})
                        .to_string()
                        .into(),
                ))
                .await
                .expect("send notification");
            websocket
                .send(Message::Text(
                    json!({"id": initialize["id"].clone(), "result": {"ready": true}})
                        .to_string()
                        .into(),
                ))
                .await
                .expect("send initialize response");

            let initialized = next_json_object(&mut websocket).await;
            assert_eq!(initialized["method"], "initialized");

            let request = next_json_object(&mut websocket).await;
            assert_eq!(request["method"], "thread/loaded/list");
            websocket
                .send(Message::Text(
                    json!({"method": "thread/updated", "params": {"thread": {"id": "thread-1"}}})
                        .to_string()
                        .into(),
                ))
                .await
                .expect("send interleaved notification");
            websocket
                .send(Message::Text(
                    json!({"id": request["id"].clone(), "result": {"data": ["thread-1"]}})
                        .to_string()
                        .into(),
                ))
                .await
                .expect("send request response");

            let response = next_json_object(&mut websocket).await;
            assert_eq!(response["id"], "approval-1");
            assert_eq!(response["result"]["decision"], "accept");
        });

        let mut connection = AppServerConnection::connect(&format!("ws://{address}"))
            .await
            .expect("connect client");
        let initialize_result = connection
            .initialize(
                "limux-codex-teams-test",
                "0.1.0",
                &["thread/tokenUsage/updated"],
                Duration::from_secs(2),
            )
            .await
            .expect("initialize");
        assert_eq!(initialize_result["ready"], true);

        let mut notifications = Vec::new();
        let response = connection
            .request(
                "thread/loaded/list",
                Some(json!({"limit": 200})),
                |message| {
                    notifications.push(message);
                    Ok(())
                },
                Duration::from_secs(2),
            )
            .await
            .expect("request");
        assert_eq!(response["data"][0], "thread-1");
        assert_eq!(notifications.len(), 1);

        connection
            .respond(json!("approval-1"), json!({"decision": "accept"}))
            .await
            .expect("respond");
        server.await.expect("server task");
    }

    async fn next_json_object<S>(websocket: &mut S) -> Value
    where
        S: StreamExt<Item = std::result::Result<Message, tokio_tungstenite::tungstenite::Error>>
            + Unpin,
    {
        let message = websocket
            .next()
            .await
            .expect("websocket message")
            .expect("message result");
        match message {
            Message::Text(text) => serde_json::from_str(text.as_str()).expect("json text"),
            other => panic!("unexpected websocket message: {other:?}"),
        }
    }

    #[test]
    fn codex_teams_thread_parser_extracts_subagent_spawn_metadata() {
        let thread_object = object(json!({
            "id": "thread-child",
            "cwd": "/repo",
            "status": {"type": "loaded"},
            "agentNickname": "builder",
            "source": {
                "subAgent": {
                    "thread_spawn": {
                        "parent_thread_id": "thread-parent",
                        "depth": 2,
                        "agent_role": "reviewer"
                    }
                }
            }
        }));

        let thread = thread_from_object(&thread_object).expect("thread");
        assert_eq!(thread.id, "thread-child");
        assert_eq!(thread.cwd.as_deref(), Some("/repo"));
        assert_eq!(thread.status_type.as_deref(), Some("loaded"));
        assert!(thread_may_be_attachable(&thread));
        let spawn = thread.spawn.as_ref().expect("spawn");
        assert_eq!(spawn.parent_thread_id, "thread-parent");
        assert_eq!(spawn.source_depth, Some(2));
        assert_eq!(spawn.agent_role.as_deref(), Some("reviewer"));

        let not_loaded = Thread {
            status_type: Some("not_loaded".to_string()),
            ..thread
        };
        assert!(!thread_may_be_attachable(&not_loaded));
    }

    #[test]
    fn codex_teams_root_arguments_insert_remote_in_cmux_position() {
        assert_eq!(
            root_codex_arguments("ws://127.0.0.1:1234", &["--model".into(), "gpt-5.4".into()]),
            vec!["--remote", "ws://127.0.0.1:1234", "--model", "gpt-5.4"]
        );
        assert_eq!(
            root_codex_arguments(
                "ws://127.0.0.1:1234",
                &[
                    "resume".into(),
                    "--last".into(),
                    "--model".into(),
                    "gpt-5.4".into()
                ]
            ),
            vec![
                "resume",
                "--remote",
                "ws://127.0.0.1:1234",
                "--last",
                "--model",
                "gpt-5.4"
            ]
        );
    }

    #[test]
    fn codex_teams_resume_command_quotes_env_and_remote_thread() {
        let command = resume_command_text(
            "/usr/local/bin/codex",
            "ws://127.0.0.1:2345",
            "thread-child",
            "thread-parent",
            2,
            Some("/tmp/bin:/usr/bin"),
        );

        assert!(command.contains("'env'"));
        assert!(command.contains("'PATH=/tmp/bin:/usr/bin'"));
        assert!(command.contains("'CMUX_CODEX_TEAMS_APP_SERVER_URL=ws://127.0.0.1:2345'"));
        assert!(command.contains("'CMUX_AGENT_MANAGED_SUBAGENT=1'"));
        assert!(command.contains(concat!(
            "'/usr/local/bin/codex' 'resume' '--remote' 'ws://127.0.0.1:2345' ",
            "'thread-child' '-c' 'check_for_update_on_startup=false'"
        )));
    }

    #[test]
    fn codex_teams_subagent_split_plan_matches_cmux_surface_contract() {
        let thread = Thread {
            id: "thread-child".to_string(),
            cwd: Some("/tmp/project".to_string()),
            status_type: Some("loaded".to_string()),
            agent_nickname: Some("builder".to_string()),
            agent_role: None,
            spawn: None,
        };
        let spawn = Spawn {
            parent_thread_id: "thread-parent".to_string(),
            source_depth: Some(1),
            agent_nickname: None,
            agent_role: Some("reviewer".to_string()),
        };

        let plan = subagent_split_plan(
            "workspace-1",
            "surface-root",
            Some("surface-last"),
            &thread,
            &spawn,
            &SubagentLaunch {
                codex_executable: "/usr/local/bin/codex",
                app_server_url: "ws://127.0.0.1:2345",
                launch_path: Some("/tmp/bin"),
                depth: 2,
            },
        )
        .expect("split plan");

        assert_eq!(plan.params["workspace_id"], "workspace-1");
        assert_eq!(plan.params["surface_id"], "surface-last");
        assert_eq!(plan.params["direction"], "down");
        assert_eq!(plan.params["focus"], false);
        assert_eq!(plan.params["working_directory"], "/tmp/project");
        assert_eq!(
            plan.params["startup_environment"][MANAGED_SUBAGENT_ENV_KEY],
            "1"
        );
        assert_eq!(
            plan.params["startup_environment"][THREAD_ENV_KEY],
            "thread-child"
        );
        assert_eq!(
            plan.params["startup_environment"][PARENT_THREAD_ENV_KEY],
            "thread-parent"
        );
        assert_eq!(plan.params["startup_environment"][DEPTH_ENV_KEY], "2");
        assert_eq!(plan.title, "Codex d2: reviewer");

        let script = fs::read_to_string(&plan.startup_script).expect("startup script");
        assert!(script.contains("cd -- '/tmp/project'"));
        assert!(script.contains("exec \"${SHELL:-/bin/sh}\" -lc"));
        assert!(script.contains("codex"));
        fs::remove_file(&plan.startup_script).expect("cleanup startup script");
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
