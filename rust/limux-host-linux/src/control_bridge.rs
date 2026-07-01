// summary: Bridge the Limux control socket onto GTK host state.
// purpose: Parse socket RPC requests, authorize clients, and dispatch live workspace/pane/surface commands.
// inputs: Unix socket frames, v1/v2 protocol requests, peer credentials, and GTK command dispatch callbacks.
// returns/effects: Sends JSON responses, mutates live host state through ControlCommand, and exits loudly on invalid requests.

use std::io::{self, Write};
use std::os::unix::net::UnixStream;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::mpsc;
use std::sync::{Arc, OnceLock};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use gtk::glib;
use gtk4 as gtk;
use limux_control::auth::{self, SocketControlMode};
use limux_control::request_io::{self, read_request_frame};
use limux_control::socket_path::{bind_listener, resolve_socket_path, SocketMode};
use limux_protocol::{parse_v1_command_envelope, V2Request, V2Response};
use serde_json::{json, Map, Value};

const METHODS: &[&str] = &[
    "system.ping",
    "system.identify",
    "system.capabilities",
    "system.memory",
    "events.stream",
    "workspace.current",
    "workspace.list",
    "workspace.create",
    "workspace.create_many",
    "workspace.select",
    "workspace.rename",
    "workspace.close",
    "pane.list",
    "pane.surfaces",
    "pane.create",
    "pane.create_many",
    "pane.focus",
    "browser.open_split",
    "browser.navigate",
    "browser.url.get",
    "browser.back",
    "browser.forward",
    "browser.reload",
    "browser.focus_webview",
    "surface.create",
    "surface.create_many",
    "surface.list",
    "surface.focus",
    "surface.health",
    "surface.read_text",
    "surface.send_text",
    "surface.send_key",
    "notification.create",
    "notification.list",
    "notification.dismiss",
    "notification.mark_read",
    "notification.open",
    "notification.jump_to_unread",
    "notification.clear",
];

const PARSE_ERROR_CODE: i64 = -32700;
const INVALID_PARAMS_CODE: i64 = -32602;
const UNKNOWN_METHOD_CODE: i64 = -32601;
const INTERNAL_ERROR_CODE: i64 = -32603;
const NOT_FOUND_CODE: i64 = -32004;
const CONFLICT_CODE: i64 = -32009;

static EVENT_BOOT_ID: OnceLock<String> = OnceLock::new();

type BridgeResult = Result<Value, BridgeError>;

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum WorkspaceTarget {
    Active,
    Handle(String),
    Name(String),
    Index(usize),
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum PaneCreateDirection {
    Left,
    Right,
    Up,
    Down,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum PaneCreateType {
    Terminal,
    Browser,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum BrowserAction {
    Navigate { url: String },
    GetUrl,
    Back,
    Forward,
    Reload,
    Focus,
}

/// Parser-level contract for the live-GTK `pane.create` route.
///
/// Request fields accepted by the bridge:
/// - `workspace_id`/`id`, `name`, or `index` target the workspace. Raw
///   handles and `workspace:<id>` refs are accepted and preserved for the GTK
///   layer to resolve.
/// - `surface_id` and `pane_id` identify the source pane. Raw handles and
///   `surface:<id>`/`pane:<id>` refs are accepted. Later GTK work resolves
///   precedence as explicit surface, explicit pane, then safe workspace-local
///   fallback.
/// - `direction` is one of `left|right|up|down`, defaulting to `right`.
/// - `type` is one of `terminal|browser`, defaulting to `terminal`.
/// - `command` is a terminal-only host extension: the host injects it into the
///   newly-created surface after creation. The standalone core dispatcher may
///   accept the field for compatibility but does not launch a process.
///
/// Browser pane support uses the existing WebKit pane state path. Responses
/// must keep the existing core/CLI field names: `pane_id`, `pane_ref`,
/// `surface_id`, and `surface_ref`.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CreatePaneRequest {
    pub target: WorkspaceTarget,
    pub source_pane_id: Option<String>,
    pub source_surface_id: Option<String>,
    pub direction: PaneCreateDirection,
    pub pane_type: PaneCreateType,
    pub command: Option<String>,
    pub url: Option<String>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CreatePanesRequest {
    pub target: WorkspaceTarget,
    pub source_pane_id: Option<String>,
    pub source_surface_id: Option<String>,
    pub count: usize,
    pub directions: Vec<PaneCreateDirection>,
}

#[derive(Debug)]
pub enum ControlCommand {
    Identify {
        caller: Option<Value>,
        reply: mpsc::Sender<BridgeResult>,
    },
    Memory {
        top_group_limit: usize,
        reply: mpsc::Sender<BridgeResult>,
    },
    CurrentWorkspace {
        reply: mpsc::Sender<BridgeResult>,
    },
    ListWorkspaces {
        reply: mpsc::Sender<BridgeResult>,
    },
    ListPanes {
        target: WorkspaceTarget,
        reply: mpsc::Sender<BridgeResult>,
    },
    ListPaneSurfaces {
        target: WorkspaceTarget,
        pane_id: Option<String>,
        reply: mpsc::Sender<BridgeResult>,
    },
    CreatePane {
        request: CreatePaneRequest,
        reply: mpsc::Sender<BridgeResult>,
    },
    CreatePanes {
        request: CreatePanesRequest,
        reply: mpsc::Sender<BridgeResult>,
    },
    FocusPane {
        target: WorkspaceTarget,
        pane_id: String,
        reply: mpsc::Sender<BridgeResult>,
    },
    BrowserAction {
        target: WorkspaceTarget,
        surface_hint: String,
        action: BrowserAction,
        reply: mpsc::Sender<BridgeResult>,
    },
    CreateSurface {
        target: WorkspaceTarget,
        command: Option<String>,
        reply: mpsc::Sender<BridgeResult>,
    },
    CreateSurfaces {
        target: WorkspaceTarget,
        pane_id: Option<String>,
        count: usize,
        command_template: Option<String>,
        reply: mpsc::Sender<BridgeResult>,
    },
    ListSurfaces {
        target: WorkspaceTarget,
        reply: mpsc::Sender<BridgeResult>,
    },
    FocusSurface {
        target: WorkspaceTarget,
        surface_hint: String,
        reply: mpsc::Sender<BridgeResult>,
    },
    SurfaceHealth {
        target: WorkspaceTarget,
        surface_hint: Option<String>,
        reply: mpsc::Sender<BridgeResult>,
    },
    ReadSurfaceText {
        target: WorkspaceTarget,
        surface_hint: Option<String>,
        reply: mpsc::Sender<BridgeResult>,
    },
    CreateWorkspace {
        name: Option<String>,
        cwd: Option<String>,
        command: Option<String>,
        reply: mpsc::Sender<BridgeResult>,
    },
    CreateWorkspaces {
        count: usize,
        name_prefix: String,
        cwd: Option<String>,
        panes_per_workspace: usize,
        terminals_per_workspace: usize,
        reply: mpsc::Sender<BridgeResult>,
    },
    SelectWorkspace {
        target: WorkspaceTarget,
        reply: mpsc::Sender<BridgeResult>,
    },
    RenameWorkspace {
        target: WorkspaceTarget,
        title: String,
        reply: mpsc::Sender<BridgeResult>,
    },
    CloseWorkspace {
        target: WorkspaceTarget,
        reply: mpsc::Sender<BridgeResult>,
    },
    SendText {
        target: WorkspaceTarget,
        surface_hint: Option<String>,
        text: String,
        reply: mpsc::Sender<BridgeResult>,
    },
    SendKey {
        target: WorkspaceTarget,
        surface_hint: Option<String>,
        key: String,
        reply: mpsc::Sender<BridgeResult>,
    },
    /// Post a desktop-style notification into the sidebar + toast overlay.
    /// `target` chooses the workspace to flag as unread; if not provided,
    /// the currently-active workspace is used.
    CreateNotification {
        target: WorkspaceTarget,
        title: String,
        subtitle: String,
        body: String,
        reply: mpsc::Sender<BridgeResult>,
    },
    ListNotifications {
        unread_only: bool,
        reply: mpsc::Sender<BridgeResult>,
    },
    DismissNotification {
        notification_id: Option<u64>,
        all_read: bool,
        reply: mpsc::Sender<BridgeResult>,
    },
    MarkNotificationRead {
        notification_id: Option<u64>,
        target: Option<WorkspaceTarget>,
        all: bool,
        reply: mpsc::Sender<BridgeResult>,
    },
    OpenNotification {
        notification_id: u64,
        reply: mpsc::Sender<BridgeResult>,
    },
    JumpToUnreadNotification {
        reply: mpsc::Sender<BridgeResult>,
    },
    ClearNotifications {
        notification_id: Option<u64>,
        reply: mpsc::Sender<BridgeResult>,
    },
}

impl ControlCommand {
    pub fn respond(self, result: BridgeResult) {
        match self {
            Self::Identify { reply, .. }
            | Self::Memory { reply, .. }
            | Self::CurrentWorkspace { reply }
            | Self::ListWorkspaces { reply }
            | Self::ListPanes { reply, .. }
            | Self::ListPaneSurfaces { reply, .. }
            | Self::CreatePane { reply, .. }
            | Self::CreatePanes { reply, .. }
            | Self::FocusPane { reply, .. }
            | Self::BrowserAction { reply, .. }
            | Self::CreateSurface { reply, .. }
            | Self::CreateSurfaces { reply, .. }
            | Self::ListSurfaces { reply, .. }
            | Self::FocusSurface { reply, .. }
            | Self::SurfaceHealth { reply, .. }
            | Self::ReadSurfaceText { reply, .. }
            | Self::CreateWorkspace { reply, .. }
            | Self::CreateWorkspaces { reply, .. }
            | Self::SelectWorkspace { reply, .. }
            | Self::RenameWorkspace { reply, .. }
            | Self::CloseWorkspace { reply, .. }
            | Self::SendText { reply, .. }
            | Self::SendKey { reply, .. }
            | Self::CreateNotification { reply, .. }
            | Self::ListNotifications { reply, .. }
            | Self::DismissNotification { reply, .. }
            | Self::MarkNotificationRead { reply, .. }
            | Self::OpenNotification { reply, .. }
            | Self::JumpToUnreadNotification { reply }
            | Self::ClearNotifications { reply, .. } => {
                let _ = reply.send(result);
            }
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct BridgeError {
    code: i64,
    message: String,
    data: Option<Value>,
}

impl BridgeError {
    fn new(code: i64, message: impl Into<String>) -> Self {
        Self {
            code,
            message: message.into(),
            data: None,
        }
    }

    fn with_data(mut self, data: Value) -> Self {
        self.data = Some(data);
        self
    }

    pub fn invalid_params(message: impl Into<String>) -> Self {
        Self::new(INVALID_PARAMS_CODE, message)
    }

    pub fn not_found(message: impl Into<String>) -> Self {
        Self::new(NOT_FOUND_CODE, message)
    }

    pub fn conflict(message: impl Into<String>) -> Self {
        Self::new(CONFLICT_CODE, message)
    }

    pub fn internal(message: impl Into<String>) -> Self {
        Self::new(INTERNAL_ERROR_CODE, message)
    }
}

fn parse_request(input: &str) -> Result<V2Request, BridgeError> {
    if let Ok(request) = serde_json::from_str::<V2Request>(input) {
        return Ok(request);
    }

    match parse_v1_command_envelope(input) {
        Ok(v1) => Ok(v1.into_v2_request(None)),
        Err(error) => Err(BridgeError::new(
            PARSE_ERROR_CODE,
            format!("invalid request payload: {error}"),
        )
        .with_data(json!({ "raw": input }))),
    }
}

fn params_object(params: &Value) -> Result<&Map<String, Value>, BridgeError> {
    params
        .as_object()
        .ok_or_else(|| BridgeError::invalid_params("params must be a JSON object"))
}

fn optional_string(params: &Map<String, Value>, keys: &[&str]) -> Option<String> {
    keys.iter().find_map(|key| {
        params
            .get(*key)
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .map(ToOwned::to_owned)
    })
}

fn optional_handle(
    params: &Map<String, Value>,
    keys: &[&str],
) -> Result<Option<String>, BridgeError> {
    for key in keys {
        let Some(value) = params.get(*key) else {
            continue;
        };
        match value {
            Value::Null => {}
            Value::String(raw) => {
                let handle = raw.trim();
                if !handle.is_empty() {
                    return Ok(Some(handle.to_string()));
                }
            }
            Value::Number(number) => {
                let id = number.as_u64().ok_or_else(|| {
                    BridgeError::invalid_params(format!(
                        "{key} must be a non-negative integer or ref handle"
                    ))
                })?;
                return Ok(Some(id.to_string()));
            }
            _ => {
                return Err(BridgeError::invalid_params(format!(
                    "{key} must be a non-negative integer or ref handle"
                )));
            }
        }
    }
    Ok(None)
}

fn optional_ref_handle(
    params: &Map<String, Value>,
    keys: &[&str],
    prefix: &str,
) -> Result<Option<String>, BridgeError> {
    optional_handle(params, keys).map(|handle| {
        handle.map(|handle| {
            handle
                .strip_prefix(prefix)
                .unwrap_or(handle.as_str())
                .to_string()
        })
    })
}

fn optional_index(params: &Map<String, Value>, key: &str) -> Result<Option<usize>, BridgeError> {
    let Some(value) = params.get(key) else {
        return Ok(None);
    };

    if let Some(index) = value.as_u64() {
        return Ok(Some(index as usize));
    }

    Err(BridgeError::invalid_params(format!(
        "{key} must be a non-negative integer"
    )))
}

fn optional_bool(params: &Map<String, Value>, key: &str) -> Result<Option<bool>, BridgeError> {
    let Some(value) = params.get(key) else {
        return Ok(None);
    };
    value
        .as_bool()
        .map(Some)
        .ok_or_else(|| BridgeError::invalid_params(format!("{key} must be a boolean")))
}

fn optional_u64(params: &Map<String, Value>, keys: &[&str]) -> Result<Option<u64>, BridgeError> {
    for key in keys {
        let Some(value) = params.get(*key) else {
            continue;
        };
        if let Some(number) = value.as_u64() {
            return Ok(Some(number));
        }
        if let Some(raw) = value.as_str() {
            return raw.trim().parse::<u64>().map(Some).map_err(|_| {
                BridgeError::invalid_params(format!("{key} must be a non-negative integer"))
            });
        }
        return Err(BridgeError::invalid_params(format!(
            "{key} must be a non-negative integer"
        )));
    }
    Ok(None)
}

fn has_workspace_selector(params: &Map<String, Value>) -> bool {
    [
        "workspace_id",
        "workspace",
        "tab_id",
        "tab",
        "id",
        "name",
        "index",
    ]
    .iter()
    .any(|key| params.contains_key(*key))
}

fn looks_like_workspace_handle(raw: &str) -> bool {
    let raw = raw.trim();
    if raw.starts_with("workspace:") {
        return true;
    }
    let value = raw;
    uuid::Uuid::parse_str(value).is_ok() || value.chars().all(|ch| ch.is_ascii_digit())
}

fn parse_optional_workspace_target(
    params: &Map<String, Value>,
    allow_name: bool,
) -> Result<WorkspaceTarget, BridgeError> {
    if let Some(handle) = optional_handle(params, &["workspace_id", "id"])? {
        if allow_name && !looks_like_workspace_handle(&handle) {
            return Ok(WorkspaceTarget::Name(handle));
        }
        return Ok(WorkspaceTarget::Handle(handle));
    }
    if allow_name {
        if let Some(name) = optional_string(params, &["name"]) {
            return Ok(WorkspaceTarget::Name(name));
        }
    }
    if let Some(index) = optional_index(params, "index")? {
        return Ok(WorkspaceTarget::Index(index));
    }
    Ok(WorkspaceTarget::Active)
}

#[cfg_attr(not(test), allow(dead_code))]
fn parse_pane_create_direction(raw: &str) -> Result<PaneCreateDirection, BridgeError> {
    match raw {
        "left" => Ok(PaneCreateDirection::Left),
        "right" => Ok(PaneCreateDirection::Right),
        "up" => Ok(PaneCreateDirection::Up),
        "down" => Ok(PaneCreateDirection::Down),
        _ => Err(BridgeError::invalid_params(
            "pane.create direction must be one of left|right|up|down",
        )),
    }
}

fn parse_create_pane_request(
    params: &Map<String, Value>,
) -> Result<CreatePaneRequest, BridgeError> {
    let direction = parse_pane_create_direction(
        optional_string(params, &["direction"])
            .unwrap_or_else(|| "right".to_string())
            .as_str(),
    )?;

    let pane_type = match optional_string(params, &["type"])
        .unwrap_or_else(|| "terminal".to_string())
        .as_str()
    {
        "terminal" => PaneCreateType::Terminal,
        "browser" => PaneCreateType::Browser,
        _ => {
            return Err(BridgeError::invalid_params(
                "pane.create type must be one of terminal|browser",
            ));
        }
    };

    let url = optional_string(params, &["url"]);
    let command = optional_string(params, &["command"]);
    if matches!(pane_type, PaneCreateType::Terminal) && url.is_some() {
        return Err(BridgeError::invalid_params(
            "pane.create url is only supported for browser panes",
        ));
    }
    if matches!(pane_type, PaneCreateType::Browser) && command.is_some() {
        return Err(BridgeError::invalid_params(
            "pane.create command is only supported for terminal panes",
        ));
    }

    Ok(CreatePaneRequest {
        target: parse_optional_workspace_target(params, true)?,
        source_pane_id: optional_ref_handle(params, &["pane_id"], "pane:")?,
        source_surface_id: optional_ref_handle(params, &["surface_id"], "surface:")?,
        direction,
        pane_type,
        command,
        url,
    })
}

fn parse_pane_create_many_directions(
    params: &Map<String, Value>,
) -> Result<Vec<PaneCreateDirection>, BridgeError> {
    let Some(value) = params.get("directions") else {
        return Ok(vec![PaneCreateDirection::Right, PaneCreateDirection::Down]);
    };
    let Some(values) = value.as_array() else {
        return Err(BridgeError::invalid_params(
            "pane.create_many directions must be an array",
        ));
    };
    if values.is_empty() {
        return Err(BridgeError::invalid_params(
            "pane.create_many directions must not be empty",
        ));
    }
    values
        .iter()
        .map(|value| {
            value
                .as_str()
                .ok_or_else(|| {
                    BridgeError::invalid_params(
                        "pane.create_many directions must contain only strings",
                    )
                })
                .and_then(parse_pane_create_direction)
        })
        .collect()
}

fn parse_create_panes_request(
    params: &Map<String, Value>,
) -> Result<CreatePanesRequest, BridgeError> {
    let count = match optional_index(params, "count")? {
        Some(1) => 1,
        _ => {
            return Err(BridgeError::invalid_params(
                "pane.create_many currently supports count=1 only",
            ));
        }
    };
    if optional_string(params, &["command_template", "command"]).is_some() {
        return Err(BridgeError::invalid_params(
            "pane.create_many does not support command templates",
        ));
    }

    Ok(CreatePanesRequest {
        target: parse_optional_workspace_target(params, true)?,
        source_pane_id: optional_ref_handle(params, &["pane_id"], "pane:")?,
        source_surface_id: optional_ref_handle(params, &["surface_id"], "surface:")?,
        count,
        directions: parse_pane_create_many_directions(params)?,
    })
}

fn parse_required_workspace_target(
    params: &Map<String, Value>,
    allow_name: bool,
    method: &str,
) -> Result<WorkspaceTarget, BridgeError> {
    let target = parse_optional_workspace_target(params, allow_name)?;
    if matches!(target, WorkspaceTarget::Active) {
        Err(BridgeError::invalid_params(format!(
            "{method} requires workspace_id/id, name, or index"
        )))
    } else {
        Ok(target)
    }
}

fn handle_method(
    id: Option<Value>,
    method: &str,
    params: Value,
    dispatch: &dyn Fn(ControlCommand),
) -> V2Response {
    let params = match params_object(&params) {
        Ok(params) => params,
        Err(error) => return error_response(id, error),
    };

    let queued = match method {
        "system.ping" | "ping" => return V2Response::success(id, json!({ "pong": true })),
        "system.capabilities" => {
            return V2Response::success(id, json!({ "commands": METHODS, "methods": METHODS }));
        }
        "system.identify" => {
            let (reply, rx) = mpsc::channel();
            (
                ControlCommand::Identify {
                    caller: params.get("caller").cloned(),
                    reply,
                },
                rx,
            )
        }
        "system.memory" | "memory" => {
            let top_group_limit = match optional_index(params, "top_group_limit") {
                Ok(Some(limit)) if (1..=100).contains(&limit) => limit,
                Ok(Some(_)) => {
                    return error_response(
                        id,
                        BridgeError::invalid_params(
                            "system.memory top_group_limit must be 1..=100",
                        ),
                    );
                }
                Ok(None) => 12,
                Err(error) => return error_response(id, error),
            };
            let (reply, rx) = mpsc::channel();
            (
                ControlCommand::Memory {
                    top_group_limit,
                    reply,
                },
                rx,
            )
        }
        "workspace.current" => {
            let (reply, rx) = mpsc::channel();
            (ControlCommand::CurrentWorkspace { reply }, rx)
        }
        "workspace.list" | "list-workspaces" => {
            let (reply, rx) = mpsc::channel();
            (ControlCommand::ListWorkspaces { reply }, rx)
        }
        "pane.list" | "list-panes" => {
            let target = match parse_optional_workspace_target(params, true) {
                Ok(target) => target,
                Err(error) => return error_response(id, error),
            };
            let (reply, rx) = mpsc::channel();
            (ControlCommand::ListPanes { target, reply }, rx)
        }
        "pane.surfaces" => {
            let target = match parse_optional_workspace_target(params, true) {
                Ok(target) => target,
                Err(error) => return error_response(id, error),
            };
            let (reply, rx) = mpsc::channel();
            (
                ControlCommand::ListPaneSurfaces {
                    target,
                    pane_id: optional_string(params, &["pane_id", "id"]),
                    reply,
                },
                rx,
            )
        }
        "pane.create" | "new-pane" => {
            let request = match parse_create_pane_request(params) {
                Ok(request) => request,
                Err(error) => return error_response(id, error),
            };
            let (reply, rx) = mpsc::channel();
            (ControlCommand::CreatePane { request, reply }, rx)
        }
        "browser.open_split" => {
            let mut create_params = params.clone();
            create_params.insert("type".to_string(), Value::String("browser".to_string()));
            if !create_params.contains_key("direction") {
                create_params.insert("direction".to_string(), Value::String("right".to_string()));
            }
            let request = match parse_create_pane_request(&create_params) {
                Ok(request) => request,
                Err(error) => return error_response(id, error),
            };
            let (reply, rx) = mpsc::channel();
            (ControlCommand::CreatePane { request, reply }, rx)
        }
        "browser.navigate"
        | "browser.url.get"
        | "browser.back"
        | "browser.forward"
        | "browser.reload"
        | "browser.focus_webview" => {
            let surface_hint =
                match optional_ref_handle(params, &["surface_id", "surface", "id"], "surface:") {
                    Ok(Some(value)) => value,
                    Ok(None) => {
                        return error_response(
                            id,
                            BridgeError::invalid_params(format!("{method} requires surface_id")),
                        )
                    }
                    Err(error) => return error_response(id, error),
                };
            let action = match method {
                "browser.navigate" => {
                    let Some(url) = optional_string(params, &["url"]) else {
                        return error_response(
                            id,
                            BridgeError::invalid_params("browser.navigate requires url"),
                        );
                    };
                    BrowserAction::Navigate { url }
                }
                "browser.url.get" => BrowserAction::GetUrl,
                "browser.back" => BrowserAction::Back,
                "browser.forward" => BrowserAction::Forward,
                "browser.reload" => BrowserAction::Reload,
                "browser.focus_webview" => BrowserAction::Focus,
                _ => unreachable!("browser method matched above"),
            };
            let target = match parse_optional_workspace_target(params, true) {
                Ok(target) => target,
                Err(error) => return error_response(id, error),
            };
            let (reply, rx) = mpsc::channel();
            (
                ControlCommand::BrowserAction {
                    target,
                    surface_hint,
                    action,
                    reply,
                },
                rx,
            )
        }
        "pane.create_many" => {
            let request = match parse_create_panes_request(params) {
                Ok(request) => request,
                Err(error) => return error_response(id, error),
            };
            let (reply, rx) = mpsc::channel();
            (ControlCommand::CreatePanes { request, reply }, rx)
        }
        "pane.focus" => {
            let target = match parse_optional_workspace_target(params, true) {
                Ok(target) => target,
                Err(error) => return error_response(id, error),
            };
            let pane_id = match optional_ref_handle(params, &["pane_id", "id"], "pane:") {
                Ok(Some(value)) if !value.trim().is_empty() => value,
                Ok(_) => {
                    return error_response(
                        id,
                        BridgeError::invalid_params("pane.focus requires pane_id"),
                    );
                }
                Err(error) => return error_response(id, error),
            };
            let (reply, rx) = mpsc::channel();
            (
                ControlCommand::FocusPane {
                    target,
                    pane_id,
                    reply,
                },
                rx,
            )
        }
        "surface.create" | "new-surface" => {
            let target = match parse_optional_workspace_target(params, true) {
                Ok(target) => target,
                Err(error) => return error_response(id, error),
            };
            let (reply, rx) = mpsc::channel();
            (
                ControlCommand::CreateSurface {
                    target,
                    command: optional_string(params, &["command"]),
                    reply,
                },
                rx,
            )
        }
        "surface.create_many" => {
            let target = match parse_optional_workspace_target(params, true) {
                Ok(target) => target,
                Err(error) => return error_response(id, error),
            };
            let pane_id = match optional_ref_handle(params, &["pane_id"], "pane:") {
                Ok(pane_id) => pane_id,
                Err(error) => return error_response(id, error),
            };
            let count = match optional_index(params, "count") {
                Ok(Some(count)) if (1..=200).contains(&count) => count,
                Ok(_) => {
                    return error_response(
                        id,
                        BridgeError::invalid_params("surface.create_many count must be 1..=200"),
                    );
                }
                Err(error) => return error_response(id, error),
            };
            let (reply, rx) = mpsc::channel();
            (
                ControlCommand::CreateSurfaces {
                    target,
                    pane_id,
                    count,
                    command_template: optional_string(params, &["command_template", "command"]),
                    reply,
                },
                rx,
            )
        }
        "surface.list" | "list-panels" => {
            let target = match parse_optional_workspace_target(params, true) {
                Ok(target) => target,
                Err(error) => return error_response(id, error),
            };
            let (reply, rx) = mpsc::channel();
            (ControlCommand::ListSurfaces { target, reply }, rx)
        }
        "surface.focus" | "focus-panel" => {
            let target = match parse_optional_workspace_target(params, true) {
                Ok(target) => target,
                Err(error) => return error_response(id, error),
            };
            let surface_hint =
                match optional_ref_handle(params, &["surface_id", "panel_id", "id"], "surface:") {
                    Ok(Some(value)) if !value.trim().is_empty() => value,
                    Ok(_) => {
                        return error_response(
                            id,
                            BridgeError::invalid_params("surface.focus requires surface_id"),
                        );
                    }
                    Err(error) => return error_response(id, error),
                };
            let (reply, rx) = mpsc::channel();
            (
                ControlCommand::FocusSurface {
                    target,
                    surface_hint,
                    reply,
                },
                rx,
            )
        }
        "surface.health" | "surface-health" => {
            let target = match parse_optional_workspace_target(params, true) {
                Ok(target) => target,
                Err(error) => return error_response(id, error),
            };
            let surface_hint = match optional_ref_handle(params, &["surface_id", "id"], "surface:")
            {
                Ok(surface_hint) => surface_hint,
                Err(error) => return error_response(id, error),
            };
            let (reply, rx) = mpsc::channel();
            (
                ControlCommand::SurfaceHealth {
                    target,
                    surface_hint,
                    reply,
                },
                rx,
            )
        }
        "surface.read_text" | "read-screen" | "capture-pane" => {
            let target = match parse_optional_workspace_target(params, true) {
                Ok(target) => target,
                Err(error) => return error_response(id, error),
            };
            let surface_hint = match optional_ref_handle(params, &["surface_id", "id"], "surface:")
            {
                Ok(surface_hint) => surface_hint,
                Err(error) => return error_response(id, error),
            };
            let (reply, rx) = mpsc::channel();
            (
                ControlCommand::ReadSurfaceText {
                    target,
                    surface_hint,
                    reply,
                },
                rx,
            )
        }
        "workspace.create" | "new-workspace" => {
            let (reply, rx) = mpsc::channel();
            (
                ControlCommand::CreateWorkspace {
                    name: optional_string(params, &["name", "title"]),
                    cwd: optional_string(params, &["cwd"]),
                    command: optional_string(params, &["command"]),
                    reply,
                },
                rx,
            )
        }
        "workspace.create_many" => {
            let count = match optional_index(params, "count") {
                Ok(Some(count)) if (1..=64).contains(&count) => count,
                Ok(_) => {
                    return error_response(
                        id,
                        BridgeError::invalid_params("workspace.create_many count must be 1..=64"),
                    );
                }
                Err(error) => return error_response(id, error),
            };
            let panes_per_workspace = match optional_index(params, "panes_per_workspace") {
                Ok(Some(count)) if (1..=16).contains(&count) => count,
                Ok(_) => {
                    return error_response(
                        id,
                        BridgeError::invalid_params(
                            "workspace.create_many panes_per_workspace must be 1..=16",
                        ),
                    );
                }
                Err(error) => return error_response(id, error),
            };
            let terminals_per_workspace = match optional_index(params, "terminals_per_workspace") {
                Ok(Some(count)) if count >= panes_per_workspace && (1..=200).contains(&count) => {
                    count
                }
                Ok(_) => {
                    return error_response(
                        id,
                        BridgeError::invalid_params(
                            "workspace.create_many terminals_per_workspace must be >= panes_per_workspace and <= 200",
                        ),
                    );
                }
                Err(error) => return error_response(id, error),
            };
            let (reply, rx) = mpsc::channel();
            (
                ControlCommand::CreateWorkspaces {
                    count,
                    name_prefix: optional_string(params, &["name_prefix"])
                        .unwrap_or_else(|| "mixed".to_string()),
                    cwd: optional_string(params, &["cwd"]),
                    panes_per_workspace,
                    terminals_per_workspace,
                    reply,
                },
                rx,
            )
        }
        "workspace.select" | "workspace.activate" | "activate-workspace" => {
            let target = match parse_required_workspace_target(params, true, method) {
                Ok(target) => target,
                Err(error) => return error_response(id, error),
            };
            let (reply, rx) = mpsc::channel();
            (ControlCommand::SelectWorkspace { target, reply }, rx)
        }
        "workspace.rename" | "rename-workspace" => {
            let Some(title) = optional_string(params, &["title", "name"]) else {
                return error_response(
                    id,
                    BridgeError::invalid_params("workspace.rename requires title/name"),
                );
            };
            let target = match parse_optional_workspace_target(params, false) {
                Ok(target) => target,
                Err(error) => return error_response(id, error),
            };
            let (reply, rx) = mpsc::channel();
            (
                ControlCommand::RenameWorkspace {
                    target,
                    title,
                    reply,
                },
                rx,
            )
        }
        "workspace.close" | "close-workspace" => {
            let target = match parse_optional_workspace_target(params, false) {
                Ok(target) => target,
                Err(error) => return error_response(id, error),
            };
            let (reply, rx) = mpsc::channel();
            (ControlCommand::CloseWorkspace { target, reply }, rx)
        }
        "surface.send_text" | "send-text" | "send" => {
            let Some(text) = optional_string(params, &["text"]) else {
                return error_response(
                    id,
                    BridgeError::invalid_params("surface.send_text requires text"),
                );
            };
            // allow_name = true: lets agent-team peers address each other by
            // workspace name (e.g. `--workspace codex`) instead of UUID.
            let target = match parse_optional_workspace_target(params, true) {
                Ok(target) => target,
                Err(error) => return error_response(id, error),
            };
            let (reply, rx) = mpsc::channel();
            (
                ControlCommand::SendText {
                    target,
                    surface_hint: optional_string(params, &["surface_id"]),
                    text,
                    reply,
                },
                rx,
            )
        }
        "surface.send_key" | "send-key" => {
            let Some(key) = optional_string(params, &["key"]) else {
                return error_response(
                    id,
                    BridgeError::invalid_params("surface.send_key requires key"),
                );
            };
            let target = match parse_optional_workspace_target(params, true) {
                Ok(target) => target,
                Err(error) => return error_response(id, error),
            };
            let (reply, rx) = mpsc::channel();
            (
                ControlCommand::SendKey {
                    target,
                    surface_hint: optional_string(params, &["surface_id"]),
                    key,
                    reply,
                },
                rx,
            )
        }
        "notification.create" | "notify" => {
            // Title is required; subtitle and body are optional. This mirrors
            // cmux notify's shape (title/subtitle/body) and maps onto the
            // existing sidebar unread pipeline.
            let Some(title) = optional_string(params, &["title"]) else {
                return error_response(
                    id,
                    BridgeError::invalid_params("notification.create requires title"),
                );
            };
            let subtitle = optional_string(params, &["subtitle"]).unwrap_or_default();
            let body = optional_string(params, &["body", "message"]).unwrap_or_default();
            // allow_name = true: lets agent hooks target a peer by name.
            let target = match parse_optional_workspace_target(params, true) {
                Ok(target) => target,
                Err(error) => return error_response(id, error),
            };
            let (reply, rx) = mpsc::channel();
            (
                ControlCommand::CreateNotification {
                    target,
                    title,
                    subtitle,
                    body,
                    reply,
                },
                rx,
            )
        }
        "notification.list" => {
            let unread_only = match optional_bool(params, "unread_only") {
                Ok(value) => value.unwrap_or(false),
                Err(error) => return error_response(id, error),
            };
            let (reply, rx) = mpsc::channel();
            (ControlCommand::ListNotifications { unread_only, reply }, rx)
        }
        "notification.dismiss" => {
            let notification_id = match optional_u64(params, &["notification_id", "id"]) {
                Ok(value) => value,
                Err(error) => return error_response(id, error),
            };
            let all_read = match optional_bool(params, "all_read") {
                Ok(value) => value.unwrap_or(false),
                Err(error) => return error_response(id, error),
            };
            if notification_id.is_some() == all_read {
                return error_response(
                    id,
                    BridgeError::invalid_params(
                        "notification.dismiss requires exactly one of id or all_read",
                    ),
                );
            }
            let (reply, rx) = mpsc::channel();
            (
                ControlCommand::DismissNotification {
                    notification_id,
                    all_read,
                    reply,
                },
                rx,
            )
        }
        "notification.mark_read" => {
            let notification_id = match optional_u64(params, &["notification_id", "id"]) {
                Ok(value) => value,
                Err(error) => return error_response(id, error),
            };
            let all = match optional_bool(params, "all") {
                Ok(value) => value.unwrap_or(false),
                Err(error) => return error_response(id, error),
            };
            let has_workspace = notification_id.is_none() && has_workspace_selector(params);
            let target = if has_workspace {
                match parse_required_workspace_target(params, true, "notification.mark_read") {
                    Ok(target) => Some(target),
                    Err(error) => return error_response(id, error),
                }
            } else {
                None
            };
            let selector_count = usize::from(notification_id.is_some())
                + usize::from(target.is_some())
                + usize::from(all);
            if selector_count != 1 {
                return error_response(
                    id,
                    BridgeError::invalid_params(
                        "notification.mark_read requires exactly one selector: id, workspace, or all",
                    ),
                );
            }
            let (reply, rx) = mpsc::channel();
            (
                ControlCommand::MarkNotificationRead {
                    notification_id,
                    target,
                    all,
                    reply,
                },
                rx,
            )
        }
        "notification.open" => {
            let notification_id = match optional_u64(params, &["notification_id", "id"]) {
                Ok(Some(value)) => value,
                Ok(None) => {
                    return error_response(
                        id,
                        BridgeError::invalid_params("notification.open requires id"),
                    )
                }
                Err(error) => return error_response(id, error),
            };
            let (reply, rx) = mpsc::channel();
            (
                ControlCommand::OpenNotification {
                    notification_id,
                    reply,
                },
                rx,
            )
        }
        "notification.jump_to_unread" => {
            let (reply, rx) = mpsc::channel();
            (ControlCommand::JumpToUnreadNotification { reply }, rx)
        }
        "notification.clear" => {
            let notification_id = match optional_u64(params, &["notification_id", "id"]) {
                Ok(value) => value,
                Err(error) => return error_response(id, error),
            };
            let (reply, rx) = mpsc::channel();
            (
                ControlCommand::ClearNotifications {
                    notification_id,
                    reply,
                },
                rx,
            )
        }
        _ => {
            return error_response(
                id,
                BridgeError::new(UNKNOWN_METHOD_CODE, format!("unknown method: {method}")),
            );
        }
    };

    let (command, reply_rx) = queued;

    dispatch(command);

    match reply_rx.recv_timeout(Duration::from_secs(5)) {
        Ok(Ok(result)) => V2Response::success(id, result),
        Ok(Err(error)) => error_response(id, error),
        Err(_) => error_response(id, BridgeError::internal("control command timed out")),
    }
}

fn error_response(id: Option<Value>, error: BridgeError) -> V2Response {
    V2Response::error(id, error.code, error.message, error.data)
}

fn dispatch_request(input: &str, dispatch: &dyn Fn(ControlCommand)) -> V2Response {
    match parse_request(input) {
        Ok(request) => handle_method(request.id, &request.method, request.params, dispatch),
        Err(error) => error_response(None, error),
    }
}

/// purpose: Return a process-stable CMUX-compatible event boot identifier.
/// inputs: Process id and current system time on first use.
/// returns/effects: Initializes a stable id for this host process.
fn event_boot_id() -> &'static str {
    EVENT_BOOT_ID.get_or_init(|| {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|duration| duration.as_nanos())
            .unwrap_or(0);
        format!("limux-{}-{nanos}", std::process::id())
    })
}

/// purpose: Normalize CMUX event stream filter fields from a v2 request.
/// inputs: Request params that may contain singular or array filter aliases.
/// returns/effects: Returns string arrays for ack metadata; invalid shapes are ignored by design.
fn event_filter_strings(params: &Value, singular: &str, plural: &str) -> Vec<String> {
    fn push_value(out: &mut Vec<String>, value: &Value) {
        match value {
            Value::String(text) if !text.is_empty() => out.push(text.clone()),
            Value::Array(items) => {
                for item in items {
                    push_value(out, item);
                }
            }
            _ => {}
        }
    }

    let mut values = Vec::new();
    if let Some(value) = params.get(plural) {
        push_value(&mut values, value);
    }
    if let Some(value) = params.get(singular) {
        push_value(&mut values, value);
    }
    values
}

/// purpose: Build the initial CMUX event stream ack frame for the current process.
/// inputs: Event stream request params.
/// returns/effects: Returns a JSON frame; no replay or live events are emitted yet.
fn event_stream_ack(params: &Value) -> Value {
    let requested_after_seq = params
        .get("after_seq")
        .or_else(|| params.get("after"))
        .and_then(Value::as_u64)
        .unwrap_or(0);
    json!({
        "type": "ack",
        "protocol": "cmux-events",
        "version": 1,
        "boot_id": event_boot_id(),
        "subscription_id": format!("{}-sub-0", event_boot_id()),
        "heartbeat_interval_seconds": 15,
        "replay_count": 0,
        "resume": {
            "after_seq": requested_after_seq,
            "requested_after_seq": requested_after_seq,
            "oldest_seq": 0,
            "latest_seq": 0,
            "next_seq": 1,
            "gap": requested_after_seq > 0
        },
        "filters": {
            "names": event_filter_strings(params, "name", "names"),
            "categories": event_filter_strings(params, "category", "categories")
        },
        "limux_status": "event_ack_only"
    })
}

/// purpose: Handle CMUX event stream takeover requests outside JSON-RPC response framing.
/// inputs: Raw request line and the socket writer.
/// returns/effects: Writes one ack JSONL frame and returns true when the stream was handled.
fn try_handle_event_stream(input: &str, writer: &mut UnixStream) -> io::Result<bool> {
    let request = match parse_request(input) {
        Ok(request) => request,
        Err(_) => return Ok(false),
    };
    if request.method != "events.stream" {
        return Ok(false);
    }

    let mut payload = serde_json::to_string(&event_stream_ack(&request.params))
        .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error.to_string()))?;
    payload.push('\n');
    writer.write_all(payload.as_bytes())?;
    writer.flush()?;
    Ok(true)
}

fn handle_client(
    stream: UnixStream,
    dispatch: &(dyn Fn(ControlCommand) + Send + Sync + 'static),
) -> io::Result<()> {
    stream.set_read_timeout(Some(request_io::CLIENT_IDLE_TIMEOUT))?;
    let reader_stream = stream.try_clone()?;
    reader_stream.set_read_timeout(Some(request_io::CLIENT_IDLE_TIMEOUT))?;
    let mut reader = io::BufReader::new(reader_stream);
    let mut writer = stream;
    let mut line_buf = Vec::with_capacity(4096);

    loop {
        if !read_request_frame(&mut reader, &mut line_buf)? {
            return Ok(());
        }

        let input = std::str::from_utf8(&line_buf)
            .map(|line| line.trim_end_matches(['\n', '\r']))
            .unwrap_or("");
        if input.is_empty() {
            continue;
        }

        if try_handle_event_stream(input, &mut writer)? {
            return Ok(());
        }

        let response = dispatch_request(input, dispatch);
        let mut payload = serde_json::to_string(&response)
            .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error.to_string()))?;
        payload.push('\n');
        writer.write_all(payload.as_bytes())?;
        writer.flush()?;
    }
}

struct ConnectionSlot {
    active_connections: Arc<AtomicUsize>,
}

impl ConnectionSlot {
    fn try_acquire(active_connections: Arc<AtomicUsize>) -> Option<Self> {
        active_connections
            .fetch_update(Ordering::AcqRel, Ordering::Acquire, |current| {
                (current < request_io::MAX_CONNECTIONS).then_some(current + 1)
            })
            .ok()?;
        Some(Self { active_connections })
    }
}

impl Drop for ConnectionSlot {
    fn drop(&mut self) {
        self.active_connections.fetch_sub(1, Ordering::AcqRel);
    }
}

/// Start the control socket server in a background thread and dispatch each
/// command onto the GTK main context.
pub fn start(dispatch: fn(ControlCommand)) {
    let context = glib::MainContext::default();
    let dispatch = std::sync::Arc::new(move |command: ControlCommand| {
        context.invoke(move || dispatch(command));
    });

    std::thread::Builder::new()
        .name("limux-control".into())
        .spawn(move || {
            let path = resolve_socket_path(None, SocketMode::Runtime);
            let control_mode = match SocketControlMode::from_env() {
                Ok(mode) => mode,
                Err(error) => {
                    eprintln!("limux: invalid control socket mode: {error}");
                    return;
                }
            };
            let listener = match bind_listener(
                &path,
                SocketMode::Runtime,
                control_mode.requires_owner_only_socket(),
            ) {
                Ok(listener) => listener,
                Err(error) => {
                    eprintln!(
                        "limux: control socket bind failed ({}): {error}",
                        path.display()
                    );
                    return;
                }
            };

            eprintln!("limux: control socket at {}", path.display());
            let active_connections = Arc::new(AtomicUsize::new(0));

            for stream in listener.incoming() {
                match stream {
                    Ok(stream) => {
                        let Some(slot) = ConnectionSlot::try_acquire(active_connections.clone()) else {
                            eprintln!("limux: rejecting control client, too many active connections");
                            continue;
                        };
                        let peer = match auth::authorize_peer(&stream, control_mode) {
                            Ok(peer) => peer,
                            Err(error) => {
                                eprintln!("limux: rejected control client: {error}");
                                continue;
                            }
                        };
                        let dispatch = dispatch.clone();
                        std::thread::Builder::new()
                            .name("limux-ctrl-conn".into())
                            .spawn(move || {
                                let _slot = slot;
                                if let Err(error) = handle_client(stream, dispatch.as_ref()) {
                                    eprintln!(
                                        "limux: control connection error for pid={} uid={}: {error}",
                                        peer.pid, peer.uid
                                    );
                                }
                            })
                            .ok();
                    }
                    Err(error) => {
                        eprintln!("limux: control accept error: {error}");
                    }
                }
            }
        })
        .expect("failed to spawn control server thread");
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Read;

    #[test]
    fn parses_v2_request_directly() {
        let request = parse_request(r#"{"id":"1","method":"system.ping","params":{}}"#)
            .expect("v2 request should parse");
        assert_eq!(request.id, Some(Value::String("1".to_string())));
        assert_eq!(request.method, "system.ping");
    }

    #[test]
    fn parses_v1_request_envelope() {
        let request = parse_request(r#"{"command":"workspace.create","args":{"cwd":"/tmp"}}"#)
            .expect("v1 request should parse");
        assert_eq!(request.method, "workspace.create");
        assert_eq!(request.params["cwd"], "/tmp");
    }

    #[test]
    fn capabilities_include_events_stream() {
        assert!(METHODS.contains(&"events.stream"));
    }

    #[test]
    fn event_stream_ack_preserves_filters_and_resume_gap() {
        let ack = event_stream_ack(&json!({
            "after_seq": 7,
            "names": ["notification.created"],
            "category": "notification"
        }));

        assert_eq!(ack["type"], "ack");
        assert_eq!(ack["protocol"], "cmux-events");
        assert_eq!(ack["version"], 1);
        assert_eq!(ack["resume"]["after_seq"], 7);
        assert_eq!(ack["resume"]["requested_after_seq"], 7);
        assert_eq!(ack["resume"]["gap"], true);
        assert_eq!(ack["filters"]["names"], json!(["notification.created"]));
        assert_eq!(ack["filters"]["categories"], json!(["notification"]));
    }

    #[test]
    fn event_stream_takeover_writes_jsonl_ack() {
        let (mut writer, mut reader) = UnixStream::pair().expect("socket pair");
        let handled = try_handle_event_stream(
            r#"{"id":"events-1","method":"events.stream","params":{"category":"notification"}}"#,
            &mut writer,
        )
        .expect("event stream handled");
        writer
            .shutdown(std::net::Shutdown::Write)
            .expect("shutdown");

        let mut raw = String::new();
        reader.read_to_string(&mut raw).expect("read ack");
        let frame: Value = serde_json::from_str(raw.trim()).expect("ack json");

        assert!(handled);
        assert_eq!(frame["type"], "ack");
        assert!(frame.get("ok").is_none());
        assert_eq!(frame["filters"]["categories"], json!(["notification"]));
    }

    #[test]
    fn workspace_target_prefers_handle_over_index() {
        let params = json!({
            "workspace_id": "workspace:abc",
            "index": 2
        });
        let target =
            parse_optional_workspace_target(params.as_object().expect("object params"), true)
                .expect("target should parse");
        assert_eq!(target, WorkspaceTarget::Handle("workspace:abc".to_string()));
    }

    #[test]
    fn workspace_target_treats_cli_workspace_id_as_name_when_allowed() {
        let params = json!({
            "workspace_id": "claude"
        });
        let target =
            parse_optional_workspace_target(params.as_object().expect("object params"), true)
                .expect("target should parse");
        assert_eq!(target, WorkspaceTarget::Name("claude".to_string()));
    }

    #[test]
    fn workspace_target_preserves_raw_uuid_workspace_ids_when_names_are_allowed() {
        let workspace_id = "2b8b5ca4-0200-4433-9f7c-d5c9f725be50";
        let params = json!({
            "workspace_id": workspace_id
        });
        let target =
            parse_optional_workspace_target(params.as_object().expect("object params"), true)
                .expect("target should parse");
        assert_eq!(target, WorkspaceTarget::Handle(workspace_id.to_string()));
    }

    #[test]
    fn workspace_select_requires_explicit_target() {
        let params = Map::new();
        let error = parse_required_workspace_target(&params, true, "workspace.select")
            .expect_err("workspace.select should require a target");
        assert_eq!(error.code, INVALID_PARAMS_CODE);
    }

    #[test]
    fn pane_create_contract_accepts_raw_and_ref_targets() {
        let params = json!({
            "workspace_id": 7,
            "surface_id": "surface:11",
            "pane_id": "pane:12",
            "direction": "left",
            "type": "terminal",
            "command": "claude"
        });
        let request = parse_create_pane_request(params.as_object().expect("object params"))
            .expect("pane.create request should parse");

        assert_eq!(request.target, WorkspaceTarget::Handle("7".to_string()));
        assert_eq!(request.source_surface_id, Some("11".to_string()));
        assert_eq!(request.source_pane_id, Some("12".to_string()));
        assert_eq!(request.direction, PaneCreateDirection::Left);
        assert_eq!(request.pane_type, PaneCreateType::Terminal);
        assert_eq!(request.command, Some("claude".to_string()));
    }

    #[test]
    fn pane_create_contract_rejects_invalid_direction_and_type() {
        let bad_direction = json!({ "direction": "diagonal" });
        let error = parse_create_pane_request(bad_direction.as_object().expect("object params"))
            .expect_err("invalid direction should fail");
        assert_eq!(error.code, INVALID_PARAMS_CODE);

        let bad_type = json!({ "type": "webview" });
        let error = parse_create_pane_request(bad_type.as_object().expect("object params"))
            .expect_err("invalid type should fail");
        assert_eq!(error.code, INVALID_PARAMS_CODE);
    }

    #[test]
    fn pane_create_contract_accepts_browser_fields() {
        let browser = json!({ "type": "browser" });
        let request = parse_create_pane_request(browser.as_object().expect("object params"))
            .expect("browser panes parse");
        assert_eq!(request.pane_type, PaneCreateType::Browser);

        let url = json!({ "url": "https://example.com" });
        let error = parse_create_pane_request(url.as_object().expect("object params"))
            .expect_err("url is browser-only");
        assert_eq!(error.code, INVALID_PARAMS_CODE);

        let browser_with_url = json!({ "type": "browser", "url": "https://example.com" });
        let request =
            parse_create_pane_request(browser_with_url.as_object().expect("object params"))
                .expect("browser url parses");
        assert_eq!(request.url.as_deref(), Some("https://example.com"));
    }

    #[test]
    fn pane_create_route_queues_create_pane_command() {
        let response = dispatch_request(
            r#"{"id":1,"method":"pane.create","params":{"name":"claude","surface_id":"surface:4:tab","direction":"down","command":"codex"}}"#,
            &|command| match command {
                ControlCommand::CreatePane { request, reply } => {
                    assert_eq!(request.target, WorkspaceTarget::Name("claude".to_string()));
                    assert_eq!(request.source_surface_id, Some("4:tab".to_string()));
                    assert_eq!(request.direction, PaneCreateDirection::Down);
                    assert_eq!(request.command, Some("codex".to_string()));
                    let _ = reply.send(Ok(json!({
                        "pane_id": "9",
                        "pane_ref": "pane:9",
                        "surface_id": "9:tab",
                        "surface_ref": "surface:9:tab"
                    })));
                }
                other => panic!("unexpected command: {other:?}"),
            },
        );

        assert_eq!(response.error, None);
        let result = response.result.expect("pane.create should return a result");
        assert_eq!(result["pane_ref"], "pane:9");
        assert_eq!(result["surface_ref"], "surface:9:tab");
    }

    #[test]
    fn browser_open_split_route_queues_browser_pane() {
        let response = dispatch_request(
            r#"{"id":1,"method":"browser.open_split","params":{"workspace_id":"codex","url":"https://example.com"}}"#,
            &|command| match command {
                ControlCommand::CreatePane { request, reply } => {
                    assert_eq!(request.target, WorkspaceTarget::Name("codex".to_string()));
                    assert_eq!(request.pane_type, PaneCreateType::Browser);
                    assert_eq!(request.direction, PaneCreateDirection::Right);
                    assert_eq!(request.url.as_deref(), Some("https://example.com"));
                    let _ = reply.send(Ok(json!({
                        "pane_id": "9",
                        "pane_ref": "pane:9",
                        "surface_id": "9:browser",
                        "surface_ref": "surface:9:browser"
                    })));
                }
                other => panic!("unexpected command: {other:?}"),
            },
        );

        assert_eq!(response.error, None);
        let result = response
            .result
            .expect("browser.open_split should return a result");
        assert_eq!(result["surface_ref"], "surface:9:browser");
    }

    #[test]
    fn browser_action_routes_validate_surface_and_url() {
        let navigated = dispatch_request(
            r#"{"id":1,"method":"browser.navigate","params":{"workspace_id":"codex","surface_id":"surface:9:browser","url":"example.com"}}"#,
            &|command| match command {
                ControlCommand::BrowserAction {
                    target,
                    surface_hint,
                    action,
                    reply,
                } => {
                    assert_eq!(target, WorkspaceTarget::Name("codex".to_string()));
                    assert_eq!(surface_hint, "9:browser");
                    assert_eq!(
                        action,
                        BrowserAction::Navigate {
                            url: "example.com".to_string()
                        }
                    );
                    let _ = reply.send(Ok(json!({ "url": "https://example.com" })));
                }
                other => panic!("unexpected command: {other:?}"),
            },
        );
        assert_eq!(navigated.error, None);

        let url = dispatch_request(
            r#"{"id":1,"method":"browser.url.get","params":{"surface_id":"surface:9:browser"}}"#,
            &|command| match command {
                ControlCommand::BrowserAction {
                    surface_hint,
                    action,
                    reply,
                    ..
                } => {
                    assert_eq!(surface_hint, "9:browser");
                    assert_eq!(action, BrowserAction::GetUrl);
                    let _ = reply.send(Ok(json!({ "url": "https://example.com" })));
                }
                other => panic!("unexpected command: {other:?}"),
            },
        );
        assert_eq!(url.error, None);

        let invalid = dispatch_request(
            r#"{"id":1,"method":"browser.navigate","params":{"surface_id":"surface:9:browser"}}"#,
            &|command| panic!("invalid browser.navigate should not dispatch: {command:?}"),
        );
        assert_eq!(
            invalid.error.as_ref().map(|error| error.code),
            Some(INVALID_PARAMS_CODE)
        );
    }

    #[test]
    fn pane_create_route_rejects_invalid_params_before_dispatch() {
        let response = dispatch_request(
            r#"{"id":1,"method":"new-pane","params":{"direction":"diagonal"}}"#,
            &|command| panic!("invalid pane.create should not dispatch: {command:?}"),
        );

        assert_eq!(response.result, None);
        assert_eq!(
            response.error.as_ref().map(|error| error.code),
            Some(INVALID_PARAMS_CODE)
        );
    }

    #[test]
    fn pane_focus_route_accepts_pane_refs() {
        let response = dispatch_request(
            r#"{"id":1,"method":"pane.focus","params":{"workspace_id":"codex","pane_id":"pane:11"}}"#,
            &|command| match command {
                ControlCommand::FocusPane {
                    target,
                    pane_id,
                    reply,
                } => {
                    assert_eq!(target, WorkspaceTarget::Name("codex".to_string()));
                    assert_eq!(pane_id, "11");
                    let _ = reply.send(Ok(json!({ "pane_ref": "pane:11" })));
                }
                other => panic!("unexpected command: {other:?}"),
            },
        );

        assert_eq!(response.error, None);
        assert_eq!(
            response.result.expect("pane.focus result")["pane_ref"],
            "pane:11"
        );
    }

    #[test]
    fn surface_create_route_queues_command_backed_terminal_surface() {
        let response = dispatch_request(
            r#"{"id":1,"method":"surface.create","params":{"workspace_id":"codex","command":"bash"}}"#,
            &|command| match command {
                ControlCommand::CreateSurface {
                    target,
                    command,
                    reply,
                } => {
                    assert_eq!(target, WorkspaceTarget::Name("codex".to_string()));
                    assert_eq!(command, Some("bash".to_string()));
                    let _ = reply.send(Ok(json!({
                        "pane_ref": "pane:1",
                        "surface_ref": "surface:1:tab"
                    })));
                }
                other => panic!("unexpected command: {other:?}"),
            },
        );

        assert_eq!(response.error, None);
        assert_eq!(
            response.result.expect("surface.create result")["surface_ref"],
            "surface:1:tab"
        );
    }

    #[test]
    fn surface_focus_route_accepts_cmux_panel_alias() {
        let response = dispatch_request(
            r#"{"id":1,"method":"focus-panel","params":{"workspace_id":"codex","panel_id":"surface:4:tab"}}"#,
            &|command| match command {
                ControlCommand::FocusSurface {
                    target,
                    surface_hint,
                    reply,
                } => {
                    assert_eq!(target, WorkspaceTarget::Name("codex".to_string()));
                    assert_eq!(surface_hint, "4:tab");
                    let _ = reply.send(Ok(json!({ "surface_ref": "surface:4:tab" })));
                }
                other => panic!("unexpected command: {other:?}"),
            },
        );

        assert_eq!(response.error, None);
        assert_eq!(
            response.result.expect("surface.focus result")["surface_ref"],
            "surface:4:tab"
        );
    }

    #[test]
    fn notification_list_and_clear_routes_validate_params() {
        let listed = dispatch_request(
            r#"{"id":1,"method":"notification.list","params":{"unread_only":true}}"#,
            &|command| match command {
                ControlCommand::ListNotifications { unread_only, reply } => {
                    assert!(unread_only);
                    let _ = reply.send(Ok(json!({ "notifications": [] })));
                }
                other => panic!("unexpected command: {other:?}"),
            },
        );
        assert_eq!(listed.error, None);
        assert_eq!(
            listed.result.expect("notification.list result")["notifications"],
            json!([])
        );

        let cleared = dispatch_request(
            r#"{"id":1,"method":"notification.clear","params":{"notification_id":7}}"#,
            &|command| match command {
                ControlCommand::ClearNotifications {
                    notification_id,
                    reply,
                } => {
                    assert_eq!(notification_id, Some(7));
                    let _ = reply.send(Ok(json!({ "notifications": [] })));
                }
                other => panic!("unexpected command: {other:?}"),
            },
        );
        assert_eq!(cleared.error, None);

        let dismissed = dispatch_request(
            r#"{"id":1,"method":"notification.dismiss","params":{"id":"8"}}"#,
            &|command| match command {
                ControlCommand::DismissNotification {
                    notification_id,
                    all_read,
                    reply,
                } => {
                    assert_eq!(notification_id, Some(8));
                    assert!(!all_read);
                    let _ = reply.send(Ok(json!({ "notifications": [] })));
                }
                other => panic!("unexpected command: {other:?}"),
            },
        );
        assert_eq!(dismissed.error, None);

        let marked = dispatch_request(
            r#"{"id":1,"method":"notification.mark_read","params":{"workspace_id":"codex"}}"#,
            &|command| match command {
                ControlCommand::MarkNotificationRead {
                    notification_id,
                    target,
                    all,
                    reply,
                } => {
                    assert_eq!(notification_id, None);
                    assert_eq!(target, Some(WorkspaceTarget::Name("codex".to_string())));
                    assert!(!all);
                    let _ = reply.send(Ok(json!({ "notifications": [] })));
                }
                other => panic!("unexpected command: {other:?}"),
            },
        );
        assert_eq!(marked.error, None);

        let opened = dispatch_request(
            r#"{"id":1,"method":"notification.open","params":{"id":9}}"#,
            &|command| match command {
                ControlCommand::OpenNotification {
                    notification_id,
                    reply,
                } => {
                    assert_eq!(notification_id, 9);
                    let _ = reply.send(Ok(json!({ "notification": { "id": 9 } })));
                }
                other => panic!("unexpected command: {other:?}"),
            },
        );
        assert_eq!(opened.error, None);

        let jumped = dispatch_request(
            r#"{"id":1,"method":"notification.jump_to_unread","params":{}}"#,
            &|command| match command {
                ControlCommand::JumpToUnreadNotification { reply } => {
                    let _ = reply.send(Ok(json!({ "notification": { "id": 10 } })));
                }
                other => panic!("unexpected command: {other:?}"),
            },
        );
        assert_eq!(jumped.error, None);

        let invalid = dispatch_request(
            r#"{"id":1,"method":"notification.list","params":{"unread_only":"yes"}}"#,
            &|command| panic!("invalid notification.list should not dispatch: {command:?}"),
        );
        assert_eq!(
            invalid.error.as_ref().map(|error| error.code),
            Some(INVALID_PARAMS_CODE)
        );
    }

    #[test]
    fn system_memory_route_validates_group_limit() {
        let response = dispatch_request(
            r#"{"id":1,"method":"system.memory","params":{"top_group_limit":5}}"#,
            &|command| match command {
                ControlCommand::Memory {
                    top_group_limit,
                    reply,
                } => {
                    assert_eq!(top_group_limit, 5);
                    let _ = reply.send(Ok(json!({ "memory_diagnostic": { "ok": true } })));
                }
                other => panic!("unexpected command: {other:?}"),
            },
        );

        assert_eq!(response.error, None);
        assert_eq!(
            response.result.expect("system.memory result")["memory_diagnostic"]["ok"],
            true
        );

        let invalid = dispatch_request(
            r#"{"id":1,"method":"system.memory","params":{"top_group_limit":101}}"#,
            &|command| panic!("invalid system.memory should not dispatch: {command:?}"),
        );
        assert_eq!(
            invalid.error.as_ref().map(|error| error.code),
            Some(INVALID_PARAMS_CODE)
        );
    }

    #[test]
    fn workspace_create_many_route_validates_shape() {
        let response = dispatch_request(
            r#"{"id":1,"method":"workspace.create_many","params":{"count":12,"name_prefix":"triple","cwd":"/tmp","panes_per_workspace":4,"terminals_per_workspace":10}}"#,
            &|command| match command {
                ControlCommand::CreateWorkspaces {
                    count,
                    name_prefix,
                    cwd,
                    panes_per_workspace,
                    terminals_per_workspace,
                    reply,
                } => {
                    assert_eq!(count, 12);
                    assert_eq!(name_prefix, "triple");
                    assert_eq!(cwd, Some("/tmp".to_string()));
                    assert_eq!(panes_per_workspace, 4);
                    assert_eq!(terminals_per_workspace, 10);
                    let _ = reply.send(Ok(json!({ "count": count, "workspaces": [] })));
                }
                other => panic!("unexpected command: {other:?}"),
            },
        );

        assert_eq!(response.error, None);
        assert_eq!(
            response.result.expect("workspace.create_many result")["count"],
            12
        );

        let invalid = dispatch_request(
            r#"{"id":1,"method":"workspace.create_many","params":{"count":1,"panes_per_workspace":4,"terminals_per_workspace":3}}"#,
            &|command| panic!("invalid workspace.create_many should not dispatch: {command:?}"),
        );
        assert_eq!(
            invalid.error.as_ref().map(|error| error.code),
            Some(INVALID_PARAMS_CODE)
        );
    }

    #[test]
    fn pane_create_many_route_validates_count_directions_and_rejects_template() {
        let response = dispatch_request(
            r#"{"id":1,"method":"pane.create_many","params":{"workspace_id":"codex","count":1,"directions":["right","down"]}}"#,
            &|command| match command {
                ControlCommand::CreatePanes { request, reply } => {
                    assert_eq!(request.target, WorkspaceTarget::Name("codex".to_string()));
                    assert_eq!(request.count, 1);
                    assert_eq!(
                        request.directions,
                        vec![PaneCreateDirection::Right, PaneCreateDirection::Down]
                    );
                    let _ = reply.send(Ok(json!({ "count": request.count, "panes": [] })));
                }
                other => panic!("unexpected command: {other:?}"),
            },
        );

        assert_eq!(response.error, None);
        assert_eq!(
            response.result.expect("pane.create_many result")["count"],
            1
        );

        let invalid_count = dispatch_request(
            r#"{"id":1,"method":"pane.create_many","params":{"count":0}}"#,
            &|command| panic!("invalid pane.create_many should not dispatch: {command:?}"),
        );
        assert_eq!(
            invalid_count.error.as_ref().map(|error| error.code),
            Some(INVALID_PARAMS_CODE)
        );

        let invalid_direction = dispatch_request(
            r#"{"id":1,"method":"pane.create_many","params":{"count":1,"directions":["diagonal"]}}"#,
            &|command| panic!("invalid pane.create_many should not dispatch: {command:?}"),
        );
        assert_eq!(
            invalid_direction.error.as_ref().map(|error| error.code),
            Some(INVALID_PARAMS_CODE)
        );

        let invalid_template = dispatch_request(
            r#"{"id":1,"method":"pane.create_many","params":{"count":1,"command_template":"echo {i}"}}"#,
            &|command| panic!("invalid pane.create_many should not dispatch: {command:?}"),
        );
        assert_eq!(
            invalid_template.error.as_ref().map(|error| error.code),
            Some(INVALID_PARAMS_CODE)
        );
    }

    #[test]
    fn surface_create_many_route_validates_count_and_template() {
        let response = dispatch_request(
            r#"{"id":1,"method":"surface.create_many","params":{"workspace_id":"codex","count":40,"command_template":"echo {i}"}}"#,
            &|command| match command {
                ControlCommand::CreateSurfaces {
                    target,
                    pane_id,
                    count,
                    command_template,
                    reply,
                } => {
                    assert_eq!(target, WorkspaceTarget::Name("codex".to_string()));
                    assert_eq!(pane_id, None);
                    assert_eq!(count, 40);
                    assert_eq!(command_template, Some("echo {i}".to_string()));
                    let _ = reply.send(Ok(json!({ "count": count, "surfaces": [] })));
                }
                other => panic!("unexpected command: {other:?}"),
            },
        );

        assert_eq!(response.error, None);
        assert_eq!(
            response.result.expect("surface.create_many result")["count"],
            40
        );

        let invalid = dispatch_request(
            r#"{"id":1,"method":"surface.create_many","params":{"count":0}}"#,
            &|command| panic!("invalid surface.create_many should not dispatch: {command:?}"),
        );
        assert_eq!(
            invalid.error.as_ref().map(|error| error.code),
            Some(INVALID_PARAMS_CODE)
        );
    }

    #[test]
    fn surface_health_route_accepts_surface_refs() {
        let response = dispatch_request(
            r#"{"id":1,"method":"surface.health","params":{"workspace_id":"codex","surface_id":"surface:4:tab"}}"#,
            &|command| match command {
                ControlCommand::SurfaceHealth {
                    target,
                    surface_hint,
                    reply,
                } => {
                    assert_eq!(target, WorkspaceTarget::Name("codex".to_string()));
                    assert_eq!(surface_hint, Some("4:tab".to_string()));
                    let _ = reply.send(Ok(json!({ "surfaces": [] })));
                }
                other => panic!("unexpected command: {other:?}"),
            },
        );

        assert_eq!(response.error, None);
        assert!(response.result.is_some());
    }

    #[test]
    fn read_text_route_accepts_capture_alias_and_surface_refs() {
        let response = dispatch_request(
            r#"{"id":1,"method":"capture-pane","params":{"surface_id":"surface:9:tab"}}"#,
            &|command| match command {
                ControlCommand::ReadSurfaceText {
                    target,
                    surface_hint,
                    reply,
                } => {
                    assert_eq!(target, WorkspaceTarget::Active);
                    assert_eq!(surface_hint, Some("9:tab".to_string()));
                    let _ = reply.send(Ok(json!({ "text": "ready" })));
                }
                other => panic!("unexpected command: {other:?}"),
            },
        );

        assert_eq!(response.error, None);
        assert_eq!(response.result.expect("result")["text"], "ready");
    }
}
