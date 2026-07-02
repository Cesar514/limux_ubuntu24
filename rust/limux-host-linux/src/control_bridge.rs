// summary: Bridge the Limux control socket onto GTK host state.
// purpose: Parse socket RPC requests, authorize clients, and dispatch live workspace/pane/surface commands.
// inputs: Unix socket frames, v1/v2 protocol requests, peer credentials, and GTK command dispatch callbacks.
// returns/effects: Sends JSON responses, mutates live host state through ControlCommand, and exits loudly on invalid requests.

use std::collections::BTreeMap;
use std::io::{self, Write};
use std::os::unix::net::UnixStream;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::mpsc;
use std::sync::Arc;
use std::time::Duration;

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
    "auth.login",
    "auth.status",
    "reload_config",
    "config.reload",
    "settings.open",
    "events.stream",
    "feed.push",
    "feed.list",
    "feed.permission.reply",
    "feed.question.reply",
    "feed.exit_plan.reply",
    "workspace.current",
    "workspace.list",
    "workspace.env",
    "workspace.create",
    "workspace.create_many",
    "workspace.select",
    "workspace.next",
    "workspace.previous",
    "workspace.last",
    "workspace.rename",
    "workspace.close",
    "workspace.remote.reconnect",
    "workspace.remote.disconnect",
    "workspace.group.list",
    "workspace.group.create",
    "workspace.group.ungroup",
    "workspace.group.delete",
    "workspace.group.rename",
    "workspace.group.collapse",
    "workspace.group.expand",
    "workspace.group.pin",
    "workspace.group.unpin",
    "workspace.group.add",
    "workspace.group.remove",
    "workspace.group.set_anchor",
    "workspace.group.new_workspace",
    "workspace.group.set_color",
    "workspace.group.set_icon",
    "workspace.group.move",
    "workspace.group.focus",
    "pane.list",
    "pane.surfaces",
    "pane.create",
    "pane.create_many",
    "pane.focus",
    "pane.last",
    "pane.resize",
    "pane.swap",
    "pane.join",
    "pane.break",
    "browser.open_split",
    "browser.navigate",
    "browser.url.get",
    "browser.back",
    "browser.forward",
    "browser.reload",
    "browser.click",
    "browser.dblclick",
    "browser.fill",
    "browser.type",
    "browser.select",
    "browser.hover",
    "browser.focus",
    "browser.check",
    "browser.uncheck",
    "browser.press",
    "browser.keydown",
    "browser.keyup",
    "browser.scroll",
    "browser.scroll_into_view",
    "browser.focus_webview",
    "browser.is_webview_focused",
    "browser.eval",
    "browser.get.title",
    "browser.get.text",
    "browser.get.value",
    "browser.get.attr",
    "browser.get.count",
    "browser.get.box",
    "browser.get.html",
    "browser.get.styles",
    "browser.is.checked",
    "browser.is.enabled",
    "browser.is.visible",
    "browser.screenshot",
    "browser.find.role",
    "browser.find.text",
    "browser.find.label",
    "browser.find.placeholder",
    "browser.find.alt",
    "browser.find.title",
    "browser.find.testid",
    "browser.find.first",
    "browser.find.last",
    "browser.find.nth",
    "browser.frame.main",
    "browser.frame.select",
    "browser.dialog.accept",
    "browser.dialog.dismiss",
    "browser.download.wait",
    "browser.snapshot",
    "browser.wait",
    "browser.addscript",
    "browser.addinitscript",
    "browser.addstyle",
    "browser.console.list",
    "browser.console.clear",
    "browser.errors.list",
    "browser.errors.clear",
    "browser.highlight",
    "browser.cookies.get",
    "browser.cookies.set",
    "browser.cookies.clear",
    "browser.storage.get",
    "browser.storage.set",
    "browser.storage.clear",
    "browser.state.save",
    "browser.state.load",
    "browser.tab.list",
    "browser.tab.new",
    "browser.tab.switch",
    "browser.tab.close",
    "surface.split",
    "surface.create",
    "surface.create_many",
    "surface.list",
    "surface.focus",
    "surface.close",
    "surface.move",
    "surface.reorder",
    "surface.drag_to_split",
    "surface.refresh",
    "surface.clear_history",
    "surface.respawn",
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
    "sidebar.status.set",
    "sidebar.status.clear",
    "sidebar.status.list",
    "sidebar.progress.set",
    "sidebar.progress.clear",
    "sidebar.log.append",
    "sidebar.log.clear",
    "sidebar.log.list",
    "sidebar.state",
    "set_status",
    "clear_status",
    "list_status",
    "set_progress",
    "clear_progress",
    "log",
    "clear_log",
    "list_log",
    "sidebar_state",
    "right_sidebar",
];

const PARSE_ERROR_CODE: i64 = -32700;
const INVALID_PARAMS_CODE: i64 = -32602;
const UNKNOWN_METHOD_CODE: i64 = -32601;
const INTERNAL_ERROR_CODE: i64 = -32603;
const NOT_SUPPORTED_CODE: i64 = -32001;
const UNAUTHORIZED_CODE: i64 = -32002;
const NOT_FOUND_CODE: i64 = -32004;
const CONFLICT_CODE: i64 = -32009;

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
    Navigate {
        url: String,
    },
    GetUrl,
    Back,
    Forward,
    Reload,
    Click {
        selector: String,
    },
    DblClick {
        selector: String,
    },
    Fill {
        selector: String,
        text: String,
    },
    Type {
        selector: String,
        text: String,
    },
    Select {
        selector: String,
        value: String,
    },
    Hover {
        selector: String,
    },
    FocusElement {
        selector: String,
    },
    Check {
        selector: String,
    },
    Uncheck {
        selector: String,
    },
    Press {
        key: String,
    },
    KeyDown {
        key: String,
    },
    KeyUp {
        key: String,
    },
    Scroll {
        selector: Option<String>,
        dx: i64,
        dy: i64,
    },
    ScrollIntoView {
        selector: String,
    },
    Focus,
    IsFocused,
    Eval {
        script: String,
    },
    GetTitle,
    GetText {
        selector: Option<String>,
    },
    GetValue {
        selector: String,
    },
    GetAttr {
        selector: String,
        name: String,
    },
    GetCount {
        selector: String,
    },
    GetBox {
        selector: String,
    },
    GetHtml {
        selector: Option<String>,
    },
    GetStyles {
        selector: String,
        property: Option<String>,
    },
    IsChecked {
        selector: String,
    },
    IsEnabled {
        selector: String,
    },
    IsVisible {
        selector: String,
    },
    Screenshot {
        path: Option<String>,
        full_page: bool,
    },
    Find {
        locator: String,
        selector: Option<String>,
        query: Option<String>,
        role: Option<String>,
        name: Option<String>,
        index: Option<usize>,
    },
    FrameMain,
    FrameSelect {
        selector: String,
    },
    DialogAccept {
        text: Option<String>,
    },
    DialogDismiss,
    DownloadWait {
        path: Option<String>,
        timeout_ms: u64,
    },
    Snapshot {
        interactive: bool,
        compact: bool,
        max_depth: Option<usize>,
    },
    Wait {
        selector: Option<String>,
        text: Option<String>,
        url_contains: Option<String>,
        load_state: Option<String>,
        function: Option<String>,
        timeout_ms: u64,
    },
    AddScript {
        script: String,
    },
    AddInitScript {
        script: String,
    },
    AddStyle {
        css: String,
    },
    ConsoleList,
    ConsoleClear,
    ErrorsList,
    ErrorsClear,
    Highlight {
        selector: String,
    },
    CookiesGet {
        name: Option<String>,
    },
    CookiesSet {
        name: String,
        value: String,
    },
    CookiesClear {
        name: Option<String>,
    },
    StorageGet {
        storage_type: String,
        key: String,
    },
    StorageSet {
        storage_type: String,
        key: String,
        value: String,
    },
    StorageClear {
        storage_type: String,
        key: Option<String>,
    },
    StateSave {
        path: String,
    },
    StateLoad {
        path: String,
    },
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum BrowserTabAction {
    List,
    New { url: Option<String> },
    Switch { target_surface_hint: String },
    Close { target_surface_hint: Option<String> },
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

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum WorkspaceGroupAction {
    Create {
        name: Option<String>,
        cwd: Option<String>,
        from_workspace_ids: Vec<String>,
    },
    Ungroup {
        group_id: String,
    },
    Delete {
        group_id: String,
    },
    Rename {
        group_id: String,
        name: String,
    },
    Collapse {
        group_id: String,
    },
    Expand {
        group_id: String,
    },
    Pin {
        group_id: String,
    },
    Unpin {
        group_id: String,
    },
    Add {
        group_id: String,
        workspace_id: String,
    },
    Remove {
        workspace_id: String,
    },
    SetAnchor {
        group_id: String,
        workspace_id: String,
    },
    NewWorkspace {
        group_id: String,
        placement: Option<String>,
    },
    SetColor {
        group_id: String,
        color: Option<String>,
    },
    SetIcon {
        group_id: String,
        symbol: Option<String>,
    },
    Move {
        group_id: String,
        index: usize,
    },
    Focus {
        group_id: String,
    },
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum WorkspaceNavigation {
    Next,
    Previous,
    Last,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum RightSidebarMode {
    Files,
    Find,
    Vault,
    Sessions,
    Feed,
    Dock,
}

impl RightSidebarMode {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Files => "files",
            Self::Find => "find",
            Self::Vault => "vault",
            Self::Sessions => "sessions",
            Self::Feed => "feed",
            Self::Dock => "dock",
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum RightSidebarAction {
    Toggle,
    Show,
    Hide,
    Focus,
    SetMode { mode: RightSidebarMode, focus: bool },
    GetState,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RightSidebarTarget {
    pub workspace_id: Option<String>,
    pub window_id: Option<String>,
}

#[derive(Clone, Debug, PartialEq)]
pub enum SidebarAction {
    SetStatus {
        key: String,
        value: String,
        icon: Option<String>,
        color: Option<String>,
        url: Option<String>,
        priority: i64,
    },
    ClearStatus {
        key: String,
    },
    ListStatus,
    SetProgress {
        value: f64,
        label: Option<String>,
    },
    ClearProgress,
    AppendLog {
        level: String,
        source: Option<String>,
        message: String,
    },
    ClearLog,
    ListLog {
        limit: Option<usize>,
    },
    State,
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
    ReloadConfig {
        reply: mpsc::Sender<BridgeResult>,
    },
    OpenSettings {
        target: Option<String>,
        activate: bool,
        reply: mpsc::Sender<BridgeResult>,
    },
    CurrentWorkspace {
        reply: mpsc::Sender<BridgeResult>,
    },
    ListWorkspaces {
        reply: mpsc::Sender<BridgeResult>,
    },
    ListWorkspaceGroups {
        reply: mpsc::Sender<BridgeResult>,
    },
    WorkspaceGroupAction {
        action: WorkspaceGroupAction,
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
    LastPane {
        target: WorkspaceTarget,
        reply: mpsc::Sender<BridgeResult>,
    },
    ResizePane {
        target: WorkspaceTarget,
        pane_id: String,
        direction: String,
        amount: u64,
        reply: mpsc::Sender<BridgeResult>,
    },
    SwapPane {
        target: WorkspaceTarget,
        pane_id: String,
        target_pane_id: String,
        reply: mpsc::Sender<BridgeResult>,
    },
    JoinPane {
        target: WorkspaceTarget,
        source_pane_id: Option<String>,
        source_surface_id: Option<String>,
        target_pane_id: String,
        reply: mpsc::Sender<BridgeResult>,
    },
    BreakPane {
        target: WorkspaceTarget,
        pane_id: Option<String>,
        surface_hint: Option<String>,
        reply: mpsc::Sender<BridgeResult>,
    },
    BrowserAction {
        target: WorkspaceTarget,
        surface_hint: String,
        action: BrowserAction,
        reply: mpsc::Sender<BridgeResult>,
    },
    BrowserTabAction {
        target: WorkspaceTarget,
        surface_hint: String,
        action: BrowserTabAction,
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
    CloseSurface {
        target: WorkspaceTarget,
        surface_hint: Option<String>,
        reply: mpsc::Sender<BridgeResult>,
    },
    MoveSurface {
        target: WorkspaceTarget,
        surface_hint: String,
        target_pane_id: String,
        index: Option<usize>,
        reply: mpsc::Sender<BridgeResult>,
    },
    ReorderSurface {
        target: WorkspaceTarget,
        surface_hint: String,
        index: Option<usize>,
        before_surface_hint: Option<String>,
        after_surface_hint: Option<String>,
        reply: mpsc::Sender<BridgeResult>,
    },
    DragSurfaceToSplit {
        target: WorkspaceTarget,
        surface_hint: String,
        direction: PaneCreateDirection,
        reply: mpsc::Sender<BridgeResult>,
    },
    RefreshSurfaces {
        target: WorkspaceTarget,
        surface_hint: Option<String>,
        reply: mpsc::Sender<BridgeResult>,
    },
    ClearSurfaceHistory {
        target: WorkspaceTarget,
        surface_hint: Option<String>,
        reply: mpsc::Sender<BridgeResult>,
    },
    RespawnSurface {
        target: WorkspaceTarget,
        surface_hint: Option<String>,
        command: String,
        tmux_start_command: Option<String>,
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
        lines: Option<u64>,
        scrollback: bool,
        reply: mpsc::Sender<BridgeResult>,
    },
    CreateWorkspace {
        name: Option<String>,
        cwd: Option<String>,
        command: Option<String>,
        environment: BTreeMap<String, String>,
        reply: mpsc::Sender<BridgeResult>,
    },
    WorkspaceEnv {
        target: WorkspaceTarget,
        mask: bool,
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
    NavigateWorkspace {
        action: WorkspaceNavigation,
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
        surface_hint: Option<String>,
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
    RightSidebar {
        action: RightSidebarAction,
        target: RightSidebarTarget,
        reply: mpsc::Sender<BridgeResult>,
    },
    Sidebar {
        action: SidebarAction,
        target: WorkspaceTarget,
        reply: mpsc::Sender<BridgeResult>,
    },
}

impl ControlCommand {
    pub fn respond(self, result: BridgeResult) {
        match self {
            Self::Identify { reply, .. }
            | Self::Memory { reply, .. }
            | Self::ReloadConfig { reply }
            | Self::OpenSettings { reply, .. }
            | Self::CurrentWorkspace { reply }
            | Self::ListWorkspaces { reply }
            | Self::ListWorkspaceGroups { reply }
            | Self::WorkspaceGroupAction { reply, .. }
            | Self::ListPanes { reply, .. }
            | Self::ListPaneSurfaces { reply, .. }
            | Self::CreatePane { reply, .. }
            | Self::CreatePanes { reply, .. }
            | Self::FocusPane { reply, .. }
            | Self::LastPane { reply, .. }
            | Self::ResizePane { reply, .. }
            | Self::SwapPane { reply, .. }
            | Self::JoinPane { reply, .. }
            | Self::BreakPane { reply, .. }
            | Self::BrowserAction { reply, .. }
            | Self::BrowserTabAction { reply, .. }
            | Self::CreateSurface { reply, .. }
            | Self::CreateSurfaces { reply, .. }
            | Self::ListSurfaces { reply, .. }
            | Self::FocusSurface { reply, .. }
            | Self::CloseSurface { reply, .. }
            | Self::MoveSurface { reply, .. }
            | Self::ReorderSurface { reply, .. }
            | Self::DragSurfaceToSplit { reply, .. }
            | Self::RefreshSurfaces { reply, .. }
            | Self::ClearSurfaceHistory { reply, .. }
            | Self::RespawnSurface { reply, .. }
            | Self::SurfaceHealth { reply, .. }
            | Self::ReadSurfaceText { reply, .. }
            | Self::CreateWorkspace { reply, .. }
            | Self::WorkspaceEnv { reply, .. }
            | Self::CreateWorkspaces { reply, .. }
            | Self::SelectWorkspace { reply, .. }
            | Self::NavigateWorkspace { reply, .. }
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
            | Self::ClearNotifications { reply, .. }
            | Self::RightSidebar { reply, .. }
            | Self::Sidebar { reply, .. } => {
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

    fn not_supported(method: &str) -> Self {
        Self::new(NOT_SUPPORTED_CODE, format!("not_supported: {method}"))
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

fn required_browser_selector(
    method: &str,
    params: &Map<String, Value>,
) -> Result<String, BridgeError> {
    optional_string(params, &["selector"])
        .ok_or_else(|| BridgeError::invalid_params(format!("{method} requires selector")))
}

fn required_browser_key(method: &str, params: &Map<String, Value>) -> Result<String, BridgeError> {
    optional_string(params, &["key"])
        .ok_or_else(|| BridgeError::invalid_params(format!("{method} requires key")))
}

// purpose: Read and validate the CMUX browser storage namespace, defaulting to local storage.
// inputs: Browser RPC parameter map with optional `type`.
// returns/effects: Returns local/session or an invalid_params error for unknown storage types.
fn browser_storage_type(params: &Map<String, Value>) -> Result<String, BridgeError> {
    let storage_type = optional_string(params, &["type"]).unwrap_or_else(|| "local".to_string());
    match storage_type.as_str() {
        "local" | "session" => Ok(storage_type),
        other => Err(BridgeError::invalid_params(format!(
            "unsupported browser storage type: {other}"
        ))),
    }
}

// purpose: Parse CMUX browser locator methods into one live bridge action.
// inputs: Fully-qualified browser.find.* method and its JSON parameter map.
// returns/effects: Returns a validated Find action or invalid_params for incomplete locators.
fn parse_browser_find_action(
    method: &str,
    params: &Map<String, Value>,
) -> Result<BrowserAction, BridgeError> {
    let locator = method
        .strip_prefix("browser.find.")
        .ok_or_else(|| BridgeError::invalid_params("invalid browser find method"))?
        .to_string();
    match locator.as_str() {
        "role" => browser_find_role_action(method, params, locator),
        "first" | "last" => browser_find_selector_action(method, params, locator, None),
        "nth" => browser_find_nth_action(method, params, locator),
        "text" | "label" | "placeholder" | "alt" | "title" | "testid" => {
            browser_find_query_action(method, params, locator)
        }
        _ => Err(BridgeError::invalid_params(format!(
            "unsupported browser find locator: {locator}"
        ))),
    }
}

// purpose: Build a role-based CMUX browser finder action.
// inputs: Browser find method, params, and parsed locator name.
// returns/effects: Returns a validated role finder action.
fn browser_find_role_action(
    method: &str,
    params: &Map<String, Value>,
    locator: String,
) -> Result<BrowserAction, BridgeError> {
    Ok(BrowserAction::Find {
        locator,
        selector: None,
        query: None,
        role: Some(required_browser_field(method, params, &["role"])?),
        name: optional_string(params, &["name"]),
        index: None,
    })
}

// purpose: Build a selector-based CMUX browser finder action.
// inputs: Browser find method, params, parsed locator name, and optional index.
// returns/effects: Returns a validated first/last/nth finder action.
fn browser_find_selector_action(
    method: &str,
    params: &Map<String, Value>,
    locator: String,
    index: Option<usize>,
) -> Result<BrowserAction, BridgeError> {
    Ok(BrowserAction::Find {
        locator,
        selector: Some(required_browser_field(method, params, &["selector"])?),
        query: None,
        role: None,
        name: None,
        index,
    })
}

// purpose: Build a selector-and-index CMUX browser finder action.
// inputs: Browser find method, params, and parsed locator name.
// returns/effects: Returns a validated nth finder action.
fn browser_find_nth_action(
    method: &str,
    params: &Map<String, Value>,
    locator: String,
) -> Result<BrowserAction, BridgeError> {
    let index = optional_usize(params, &["index"])?
        .ok_or_else(|| BridgeError::invalid_params("browser.find.nth requires index"))?;
    browser_find_selector_action(method, params, locator, Some(index))
}

// purpose: Build a query-based CMUX browser finder action.
// inputs: Browser find method, params, and parsed locator name.
// returns/effects: Returns a validated text/label/attribute finder action.
fn browser_find_query_action(
    method: &str,
    params: &Map<String, Value>,
    locator: String,
) -> Result<BrowserAction, BridgeError> {
    let query = required_browser_field(method, params, &[locator.as_str(), "query"])?;
    Ok(BrowserAction::Find {
        locator,
        selector: None,
        query: Some(query),
        role: None,
        name: None,
        index: None,
    })
}

// purpose: Read a required non-empty browser locator field from alternate parameter names.
// inputs: Method name, JSON parameter map, and accepted field keys.
// returns/effects: Returns the field or an invalid_params error naming the missing field.
fn required_browser_field(
    method: &str,
    params: &Map<String, Value>,
    keys: &[&str],
) -> Result<String, BridgeError> {
    optional_string(params, keys).ok_or_else(|| {
        BridgeError::invalid_params(format!("{method} requires {}", keys.join(" or ")))
    })
}

// purpose: Identify CMUX browser APIs documented as unsupported by WKWebView.
// inputs: Fully-qualified browser RPC method.
// returns/effects: Returns true for explicit not_supported responses, without dispatching.
fn is_unsupported_browser_method(method: &str) -> bool {
    matches!(
        method,
        "browser.viewport.set"
            | "browser.geolocation.set"
            | "browser.offline.set"
            | "browser.trace.start"
            | "browser.trace.stop"
            | "browser.network.route"
            | "browser.network.unroute"
            | "browser.network.requests"
            | "browser.screencast.start"
            | "browser.screencast.stop"
            | "browser.input_mouse"
            | "browser.input_keyboard"
            | "browser.input_touch"
    )
}

// purpose: Identify CMUX remote workspace APIs not yet backed by a Limux remote daemon.
// inputs: Requested control method name.
// returns/effects: Returns true for explicit not_supported responses, without dispatching.
fn is_unsupported_remote_workspace_method(method: &str) -> bool {
    matches!(
        method,
        "workspace.remote.reconnect" | "workspace.remote.disconnect"
    )
}

/// purpose: Parse a CMUX right-sidebar mode from socket params.
/// inputs: Raw mode string from CLI or API.
/// returns/effects: Returns a mode enum or invalid_params for unknown modes.
fn parse_right_sidebar_mode(raw: &str) -> Result<RightSidebarMode, BridgeError> {
    match raw.trim().to_ascii_lowercase().as_str() {
        "files" => Ok(RightSidebarMode::Files),
        "find" => Ok(RightSidebarMode::Find),
        "vault" => Ok(RightSidebarMode::Vault),
        "sessions" => Ok(RightSidebarMode::Sessions),
        "feed" => Ok(RightSidebarMode::Feed),
        "dock" => Ok(RightSidebarMode::Dock),
        _ => Err(BridgeError::invalid_params(format!(
            "Unknown right-sidebar mode '{raw}'"
        ))),
    }
}

/// purpose: Parse one CMUX right-sidebar command request.
/// inputs: JSON parameter map with action, optional mode, targets, and focus.
/// returns/effects: Returns a live host command action/target or invalid_params.
fn parse_right_sidebar_request(
    params: &Map<String, Value>,
) -> Result<(RightSidebarAction, RightSidebarTarget), BridgeError> {
    let action = optional_string(params, &["action", "command"])
        .ok_or_else(|| BridgeError::invalid_params("right_sidebar requires action"))?;
    let target = RightSidebarTarget {
        workspace_id: optional_ref_handle(
            params,
            &["workspace_id", "workspace", "tab_id", "tab"],
            "workspace:",
        )?,
        window_id: optional_ref_handle(params, &["window_id", "window"], "window:")?,
    };
    let action = match action.as_str() {
        "toggle" => RightSidebarAction::Toggle,
        "show" => RightSidebarAction::Show,
        "hide" => RightSidebarAction::Hide,
        "focus" => RightSidebarAction::Focus,
        "mode" | "state" => RightSidebarAction::GetState,
        "set" => {
            let mode = optional_string(params, &["mode"]).ok_or_else(|| {
                BridgeError::invalid_params(
                    "right_sidebar set requires mode files|find|vault|sessions|feed|dock",
                )
            })?;
            let focus = optional_bool(params, "focus")?.unwrap_or(true);
            RightSidebarAction::SetMode {
                mode: parse_right_sidebar_mode(&mode)?,
                focus,
            }
        }
        _ => {
            let mode = parse_right_sidebar_mode(&action)?;
            RightSidebarAction::SetMode { mode, focus: true }
        }
    };
    Ok((action, target))
}

/// purpose: Parse a CMUX sidebar metadata/log method into a live host action.
/// inputs: Socket method name and JSON parameter map.
/// returns/effects: Returns a normalized action or invalid_params for malformed requests.
fn parse_sidebar_action(
    method: &str,
    params: &Map<String, Value>,
) -> Result<SidebarAction, BridgeError> {
    match method {
        "sidebar.status.set" | "set_status" => parse_sidebar_status_set(params),
        "sidebar.status.clear" | "clear_status" => Ok(SidebarAction::ClearStatus {
            key: required_sidebar_string(params, &["key"], method)?,
        }),
        "sidebar.status.list" | "list_status" => Ok(SidebarAction::ListStatus),
        "sidebar.progress.set" | "set_progress" => parse_sidebar_progress_set(params),
        "sidebar.progress.clear" | "clear_progress" => Ok(SidebarAction::ClearProgress),
        "sidebar.log.append" | "log" => parse_sidebar_log_append(params),
        "sidebar.log.clear" | "clear_log" => Ok(SidebarAction::ClearLog),
        "sidebar.log.list" | "list_log" => Ok(SidebarAction::ListLog {
            limit: optional_usize(params, &["limit"])?,
        }),
        "sidebar.state" | "sidebar_state" => Ok(SidebarAction::State),
        _ => Err(BridgeError::invalid_params(format!(
            "unsupported sidebar method: {method}"
        ))),
    }
}

/// purpose: Parse a CMUX set-status request.
/// inputs: JSON parameter map containing key, value, and optional presentation fields.
/// returns/effects: Returns a SetStatus action with validated priority.
fn parse_sidebar_status_set(params: &Map<String, Value>) -> Result<SidebarAction, BridgeError> {
    Ok(SidebarAction::SetStatus {
        key: required_sidebar_string(params, &["key"], "sidebar.status.set")?,
        value: required_sidebar_string(params, &["value"], "sidebar.status.set")?,
        icon: optional_string(params, &["icon"]),
        color: optional_string(params, &["color"]),
        url: optional_string(params, &["url"]),
        priority: optional_i64(params, &["priority"])?.unwrap_or(0),
    })
}

/// purpose: Parse a CMUX set-progress request.
/// inputs: JSON parameter map containing progress value and optional label.
/// returns/effects: Returns a SetProgress action after clamping validation.
fn parse_sidebar_progress_set(params: &Map<String, Value>) -> Result<SidebarAction, BridgeError> {
    let value = required_f64(params, "value", "sidebar.progress.set")?;
    if !(0.0..=1.0).contains(&value) {
        return Err(BridgeError::invalid_params(
            "sidebar.progress.set value must be between 0.0 and 1.0",
        ));
    }
    Ok(SidebarAction::SetProgress {
        value,
        label: optional_string(params, &["label"]),
    })
}

/// purpose: Parse a CMUX sidebar log append request.
/// inputs: JSON parameter map with message plus optional level/source.
/// returns/effects: Returns an AppendLog action with a known log level.
fn parse_sidebar_log_append(params: &Map<String, Value>) -> Result<SidebarAction, BridgeError> {
    let level = optional_string(params, &["level"]).unwrap_or_else(|| "info".to_string());
    if !matches!(
        level.as_str(),
        "info" | "progress" | "success" | "warning" | "error"
    ) {
        return Err(BridgeError::invalid_params(format!(
            "unsupported sidebar log level: {level}"
        )));
    }
    Ok(SidebarAction::AppendLog {
        level,
        source: optional_string(params, &["source"]),
        message: required_sidebar_string(params, &["message"], "sidebar.log.append")?,
    })
}

/// purpose: Read a required non-empty sidebar string parameter.
/// inputs: Parameter map, accepted keys, and method name for diagnostics.
/// returns/effects: Returns the trimmed value or invalid_params.
fn required_sidebar_string(
    params: &Map<String, Value>,
    keys: &[&str],
    method: &str,
) -> Result<String, BridgeError> {
    optional_string(params, keys).ok_or_else(|| {
        BridgeError::invalid_params(format!("{method} requires {}", keys.join(" or ")))
    })
}

/// purpose: Read a required floating-point sidebar parameter.
/// inputs: Parameter map, field name, and method name for diagnostics.
/// returns/effects: Returns a finite f64 or invalid_params.
fn required_f64(params: &Map<String, Value>, key: &str, method: &str) -> Result<f64, BridgeError> {
    let value = params
        .get(key)
        .ok_or_else(|| BridgeError::invalid_params(format!("{method} requires {key}")))?;
    let parsed = value.as_f64().or_else(|| {
        value
            .as_str()
            .and_then(|raw| raw.trim().parse::<f64>().ok())
    });
    match parsed {
        Some(number) if number.is_finite() => Ok(number),
        _ => Err(BridgeError::invalid_params(format!(
            "{key} must be a number"
        ))),
    }
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

const SETTINGS_TARGETS: &[&str] = &[
    "account",
    "app",
    "terminal",
    "textBox",
    "sleepyMode",
    "mobile",
    "sidebarAppearance",
    "customSidebars",
    "betaFeatures",
    "automation",
    "browser",
    "browserImport",
    "globalHotkey",
    "keyboardShortcuts",
    "workspaceColors",
    "settingsJSON",
    "reset",
];

// purpose: Validate the optional CMUX `settings.open` target parameter.
// inputs: JSON-RPC params for settings.open.
// returns/effects: Returns a canonical CMUX target or invalid_params for unknown targets.
fn parse_settings_target(params: &Map<String, Value>) -> Result<Option<String>, BridgeError> {
    let Some(value) = params.get("target") else {
        return Ok(None);
    };
    let Some(target) = value
        .as_str()
        .map(str::trim)
        .filter(|target| !target.is_empty())
    else {
        return Err(BridgeError::invalid_params(
            "settings.open target must be a string",
        ));
    };
    if SETTINGS_TARGETS.contains(&target) {
        return Ok(Some(target.to_string()));
    }
    Err(BridgeError::invalid_params("Unknown settings target"))
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

fn optional_i64(params: &Map<String, Value>, keys: &[&str]) -> Result<Option<i64>, BridgeError> {
    for key in keys {
        let Some(value) = params.get(*key) else {
            continue;
        };
        if let Some(number) = value.as_i64() {
            return Ok(Some(number));
        }
        if let Some(raw) = value.as_str() {
            return raw
                .trim()
                .parse::<i64>()
                .map(Some)
                .map_err(|_| BridgeError::invalid_params(format!("{key} must be an integer")));
        }
        return Err(BridgeError::invalid_params(format!(
            "{key} must be an integer"
        )));
    }
    Ok(None)
}

fn optional_usize(
    params: &Map<String, Value>,
    keys: &[&str],
) -> Result<Option<usize>, BridgeError> {
    for key in keys {
        let Some(value) = params.get(*key) else {
            continue;
        };
        if let Some(number) = value.as_u64() {
            return Ok(Some(number as usize));
        }
        if let Some(raw) = value.as_str() {
            return raw.trim().parse::<usize>().map(Some).map_err(|_| {
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

// purpose: Parse a live pane resize direction.
// inputs: Raw direction string from `pane.resize` or tmux-compatible `resize-pane`.
// returns/effects: Returns the normalized direction or invalid_params for unknown values.
fn parse_pane_resize_direction(raw: &str) -> Result<String, BridgeError> {
    match raw {
        "left" | "right" | "up" | "down" => Ok(raw.to_string()),
        _ => Err(BridgeError::invalid_params(
            "pane.resize direction must be one of left|right|up|down",
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

fn required_group_id(params: &Map<String, Value>, method: &str) -> Result<String, BridgeError> {
    optional_handle(params, &["group_id", "group", "id"])?.ok_or_else(|| {
        BridgeError::invalid_params(format!("{method} requires group_id/id or --group"))
    })
}

// purpose: Validate user-defined workspace environment variable names.
// inputs: Candidate environment key from CLI or RPC.
// returns/effects: Rejects empty, malformed, and managed CMUX/LIMUX keys.
fn validate_workspace_env_key(key: &str) -> Result<(), BridgeError> {
    let mut chars = key.chars();
    let Some(first) = chars.next() else {
        return Err(BridgeError::invalid_params(
            "workspace_env keys must not be empty",
        ));
    };
    if !(first == '_' || first.is_ascii_alphabetic()) {
        return Err(BridgeError::invalid_params(format!(
            "invalid workspace_env key `{key}`"
        )));
    }
    if !chars.all(|ch| ch == '_' || ch.is_ascii_alphanumeric()) {
        return Err(BridgeError::invalid_params(format!(
            "invalid workspace_env key `{key}`"
        )));
    }
    if key.starts_with("CMUX_") || key.starts_with("LIMUX_") {
        return Err(BridgeError::invalid_params(format!(
            "workspace_env cannot override managed key `{key}`"
        )));
    }
    Ok(())
}

// purpose: Parse CMUX workspace.create environment object.
// inputs: Request params that may contain workspace_env, workspaceEnv, or env.
// returns/effects: Returns sorted key/value pairs or a loud validation error.
fn parse_workspace_environment(
    params: &Map<String, Value>,
) -> Result<BTreeMap<String, String>, BridgeError> {
    let Some(value) = params
        .get("workspace_env")
        .or_else(|| params.get("workspaceEnv"))
        .or_else(|| params.get("env"))
    else {
        return Ok(BTreeMap::new());
    };
    let object = value
        .as_object()
        .ok_or_else(|| BridgeError::invalid_params("workspace_env must be an object"))?;
    let mut environment = BTreeMap::new();
    for (key, value) in object {
        validate_workspace_env_key(key)?;
        let Some(value) = value.as_str() else {
            return Err(BridgeError::invalid_params(format!(
                "workspace_env value for `{key}` must be a string"
            )));
        };
        environment.insert(key.clone(), value.to_string());
    }
    Ok(environment)
}

fn required_workspace_id(params: &Map<String, Value>, method: &str) -> Result<String, BridgeError> {
    optional_handle(params, &["workspace_id", "workspace"])?.ok_or_else(|| {
        BridgeError::invalid_params(format!("{method} requires workspace_id or --workspace"))
    })
}

fn optional_workspace_id_list(
    params: &Map<String, Value>,
    keys: &[&str],
) -> Result<Vec<String>, BridgeError> {
    let Some(value) = keys.iter().find_map(|key| params.get(*key)) else {
        return Ok(Vec::new());
    };
    match value {
        Value::Array(values) => values
            .iter()
            .map(|item| {
                item.as_str()
                    .map(str::trim)
                    .filter(|raw| !raw.is_empty())
                    .map(ToOwned::to_owned)
                    .ok_or_else(|| {
                        BridgeError::invalid_params("workspace id lists must contain strings")
                    })
            })
            .collect(),
        Value::String(raw) => Ok(raw
            .split(',')
            .map(str::trim)
            .filter(|item| !item.is_empty())
            .map(ToOwned::to_owned)
            .collect()),
        _ => Err(BridgeError::invalid_params(
            "workspace id lists must be a string or string array",
        )),
    }
}

fn parse_workspace_group_action(
    method: &str,
    params: &Map<String, Value>,
) -> Result<WorkspaceGroupAction, BridgeError> {
    let action = match method {
        "workspace.group.create" => WorkspaceGroupAction::Create {
            name: optional_string(params, &["name"]),
            cwd: optional_string(params, &["cwd"]),
            from_workspace_ids: optional_workspace_id_list(params, &["from", "workspace_ids"])?,
        },
        "workspace.group.ungroup" => WorkspaceGroupAction::Ungroup {
            group_id: required_group_id(params, method)?,
        },
        "workspace.group.delete" => WorkspaceGroupAction::Delete {
            group_id: required_group_id(params, method)?,
        },
        "workspace.group.rename" => WorkspaceGroupAction::Rename {
            group_id: required_group_id(params, method)?,
            name: optional_string(params, &["name", "title"]).ok_or_else(|| {
                BridgeError::invalid_params("workspace.group.rename requires name")
            })?,
        },
        "workspace.group.collapse" => WorkspaceGroupAction::Collapse {
            group_id: required_group_id(params, method)?,
        },
        "workspace.group.expand" => WorkspaceGroupAction::Expand {
            group_id: required_group_id(params, method)?,
        },
        "workspace.group.pin" => WorkspaceGroupAction::Pin {
            group_id: required_group_id(params, method)?,
        },
        "workspace.group.unpin" => WorkspaceGroupAction::Unpin {
            group_id: required_group_id(params, method)?,
        },
        "workspace.group.add" => WorkspaceGroupAction::Add {
            group_id: required_group_id(params, method)?,
            workspace_id: required_workspace_id(params, method)?,
        },
        "workspace.group.remove" => WorkspaceGroupAction::Remove {
            workspace_id: required_workspace_id(params, method)?,
        },
        "workspace.group.set_anchor" => WorkspaceGroupAction::SetAnchor {
            group_id: required_group_id(params, method)?,
            workspace_id: required_workspace_id(params, method)?,
        },
        "workspace.group.new_workspace" => WorkspaceGroupAction::NewWorkspace {
            group_id: required_group_id(params, method)?,
            placement: optional_string(params, &["placement"]),
        },
        "workspace.group.set_color" => WorkspaceGroupAction::SetColor {
            group_id: required_group_id(params, method)?,
            color: optional_string(params, &["hex", "color", "customColor"]),
        },
        "workspace.group.set_icon" => WorkspaceGroupAction::SetIcon {
            group_id: required_group_id(params, method)?,
            symbol: optional_string(params, &["symbol", "icon", "iconSymbol"]),
        },
        "workspace.group.move" => WorkspaceGroupAction::Move {
            group_id: required_group_id(params, method)?,
            index: optional_index(params, "index")?.ok_or_else(|| {
                BridgeError::invalid_params("workspace.group.move requires index")
            })?,
        },
        "workspace.group.focus" => WorkspaceGroupAction::Focus {
            group_id: required_group_id(params, method)?,
        },
        _ => {
            return Err(BridgeError::invalid_params(
                "unsupported workspace group action",
            ))
        }
    };
    Ok(action)
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
    if is_unsupported_browser_method(method) {
        return error_response(id, BridgeError::not_supported(method));
    }
    if is_unsupported_remote_workspace_method(method) {
        return error_response(id, BridgeError::not_supported(method));
    }

    let queued = match method {
        "system.ping" | "ping" => return V2Response::success(id, json!({ "pong": true })),
        "auth.status" => {
            return V2Response::success(id, json!({ "authenticated": true, "mode": "password" }));
        }
        "system.capabilities" => {
            return V2Response::success(id, json!({ "commands": METHODS, "methods": METHODS }));
        }
        "feed.push" => {
            return match crate::feed::coordinator().push(params) {
                Ok(result) => V2Response::success(id, result),
                Err(error) => error_response(id, error),
            };
        }
        "feed.list" => {
            return match crate::feed::coordinator().list(params) {
                Ok(result) => V2Response::success(id, result),
                Err(error) => error_response(id, error),
            };
        }
        "feed.permission.reply" => {
            return match crate::feed::coordinator().permission_reply(params) {
                Ok(result) => V2Response::success(id, result),
                Err(error) => error_response(id, error),
            };
        }
        "feed.question.reply" => {
            return match crate::feed::coordinator().question_reply(params) {
                Ok(result) => V2Response::success(id, result),
                Err(error) => error_response(id, error),
            };
        }
        "feed.exit_plan.reply" => {
            return match crate::feed::coordinator().exit_plan_reply(params) {
                Ok(result) => V2Response::success(id, result),
                Err(error) => error_response(id, error),
            };
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
        "right_sidebar" => {
            let (action, target) = match parse_right_sidebar_request(params) {
                Ok(parsed) => parsed,
                Err(error) => return error_response(id, error),
            };
            let (reply, rx) = mpsc::channel();
            (
                ControlCommand::RightSidebar {
                    action,
                    target,
                    reply,
                },
                rx,
            )
        }
        "sidebar.status.set"
        | "sidebar.status.clear"
        | "sidebar.status.list"
        | "sidebar.progress.set"
        | "sidebar.progress.clear"
        | "sidebar.log.append"
        | "sidebar.log.clear"
        | "sidebar.log.list"
        | "sidebar.state"
        | "set_status"
        | "clear_status"
        | "list_status"
        | "set_progress"
        | "clear_progress"
        | "log"
        | "clear_log"
        | "list_log"
        | "sidebar_state" => {
            let action = match parse_sidebar_action(method, params) {
                Ok(action) => action,
                Err(error) => return error_response(id, error),
            };
            let target = match parse_optional_workspace_target(params, true) {
                Ok(target) => target,
                Err(error) => return error_response(id, error),
            };
            let (reply, rx) = mpsc::channel();
            (
                ControlCommand::Sidebar {
                    action,
                    target,
                    reply,
                },
                rx,
            )
        }
        "reload_config" | "config.reload" => {
            let (reply, rx) = mpsc::channel();
            (ControlCommand::ReloadConfig { reply }, rx)
        }
        "settings.open" => {
            let target = match parse_settings_target(params) {
                Ok(target) => target,
                Err(error) => return error_response(id, error),
            };
            let activate = match optional_bool(params, "activate") {
                Ok(activate) => activate.unwrap_or(true),
                Err(error) => return error_response(id, error),
            };
            let (reply, rx) = mpsc::channel();
            (
                ControlCommand::OpenSettings {
                    target,
                    activate,
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
        "workspace.env" => {
            let target = match parse_optional_workspace_target(params, true) {
                Ok(target) => target,
                Err(error) => return error_response(id, error),
            };
            let mask = match optional_bool(params, "mask") {
                Ok(value) => value.unwrap_or(false),
                Err(error) => return error_response(id, error),
            };
            let (reply, rx) = mpsc::channel();
            (
                ControlCommand::WorkspaceEnv {
                    target,
                    mask,
                    reply,
                },
                rx,
            )
        }
        "workspace.group.list" | "list-workspace-groups" => {
            let (reply, rx) = mpsc::channel();
            (ControlCommand::ListWorkspaceGroups { reply }, rx)
        }
        "workspace.group.create"
        | "workspace.group.ungroup"
        | "workspace.group.delete"
        | "workspace.group.rename"
        | "workspace.group.collapse"
        | "workspace.group.expand"
        | "workspace.group.pin"
        | "workspace.group.unpin"
        | "workspace.group.add"
        | "workspace.group.remove"
        | "workspace.group.set_anchor"
        | "workspace.group.new_workspace"
        | "workspace.group.set_color"
        | "workspace.group.set_icon"
        | "workspace.group.move"
        | "workspace.group.focus" => {
            let action = match parse_workspace_group_action(method, params) {
                Ok(action) => action,
                Err(error) => return error_response(id, error),
            };
            let (reply, rx) = mpsc::channel();
            (ControlCommand::WorkspaceGroupAction { action, reply }, rx)
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
        | "browser.click"
        | "browser.dblclick"
        | "browser.fill"
        | "browser.type"
        | "browser.select"
        | "browser.hover"
        | "browser.focus"
        | "browser.check"
        | "browser.uncheck"
        | "browser.press"
        | "browser.keydown"
        | "browser.keyup"
        | "browser.scroll"
        | "browser.scroll_into_view"
        | "browser.focus_webview"
        | "browser.is_webview_focused"
        | "browser.eval"
        | "browser.get.title"
        | "browser.get.text"
        | "browser.get.value"
        | "browser.get.attr"
        | "browser.get.count"
        | "browser.get.box"
        | "browser.get.html"
        | "browser.get.styles"
        | "browser.is.checked"
        | "browser.is.enabled"
        | "browser.is.visible"
        | "browser.screenshot"
        | "browser.find.role"
        | "browser.find.text"
        | "browser.find.label"
        | "browser.find.placeholder"
        | "browser.find.alt"
        | "browser.find.title"
        | "browser.find.testid"
        | "browser.find.first"
        | "browser.find.last"
        | "browser.find.nth"
        | "browser.frame.main"
        | "browser.frame.select"
        | "browser.dialog.accept"
        | "browser.dialog.dismiss"
        | "browser.download.wait"
        | "browser.snapshot"
        | "browser.wait"
        | "browser.addscript"
        | "browser.addinitscript"
        | "browser.addstyle"
        | "browser.console.list"
        | "browser.console.clear"
        | "browser.errors.list"
        | "browser.errors.clear"
        | "browser.highlight"
        | "browser.cookies.get"
        | "browser.cookies.set"
        | "browser.cookies.clear"
        | "browser.storage.get"
        | "browser.storage.set"
        | "browser.storage.clear"
        | "browser.state.save"
        | "browser.state.load" => {
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
                "browser.click" => BrowserAction::Click {
                    selector: match required_browser_selector(method, params) {
                        Ok(value) => value,
                        Err(error) => return error_response(id, error),
                    },
                },
                "browser.dblclick" => BrowserAction::DblClick {
                    selector: match required_browser_selector(method, params) {
                        Ok(value) => value,
                        Err(error) => return error_response(id, error),
                    },
                },
                "browser.fill" => BrowserAction::Fill {
                    selector: match required_browser_selector(method, params) {
                        Ok(value) => value,
                        Err(error) => return error_response(id, error),
                    },
                    text: optional_string(params, &["text"]).unwrap_or_default(),
                },
                "browser.type" => {
                    let Some(text) = optional_string(params, &["text"]) else {
                        return error_response(
                            id,
                            BridgeError::invalid_params("browser.type requires text"),
                        );
                    };
                    BrowserAction::Type {
                        selector: match required_browser_selector(method, params) {
                            Ok(value) => value,
                            Err(error) => return error_response(id, error),
                        },
                        text,
                    }
                }
                "browser.select" => {
                    let Some(value) = optional_string(params, &["value"]) else {
                        return error_response(
                            id,
                            BridgeError::invalid_params("browser.select requires value"),
                        );
                    };
                    BrowserAction::Select {
                        selector: match required_browser_selector(method, params) {
                            Ok(value) => value,
                            Err(error) => return error_response(id, error),
                        },
                        value,
                    }
                }
                "browser.hover" => BrowserAction::Hover {
                    selector: match required_browser_selector(method, params) {
                        Ok(value) => value,
                        Err(error) => return error_response(id, error),
                    },
                },
                "browser.focus" => BrowserAction::FocusElement {
                    selector: match required_browser_selector(method, params) {
                        Ok(value) => value,
                        Err(error) => return error_response(id, error),
                    },
                },
                "browser.check" => BrowserAction::Check {
                    selector: match required_browser_selector(method, params) {
                        Ok(value) => value,
                        Err(error) => return error_response(id, error),
                    },
                },
                "browser.uncheck" => BrowserAction::Uncheck {
                    selector: match required_browser_selector(method, params) {
                        Ok(value) => value,
                        Err(error) => return error_response(id, error),
                    },
                },
                "browser.press" => BrowserAction::Press {
                    key: match required_browser_key(method, params) {
                        Ok(value) => value,
                        Err(error) => return error_response(id, error),
                    },
                },
                "browser.keydown" => BrowserAction::KeyDown {
                    key: match required_browser_key(method, params) {
                        Ok(value) => value,
                        Err(error) => return error_response(id, error),
                    },
                },
                "browser.keyup" => BrowserAction::KeyUp {
                    key: match required_browser_key(method, params) {
                        Ok(value) => value,
                        Err(error) => return error_response(id, error),
                    },
                },
                "browser.scroll" => BrowserAction::Scroll {
                    selector: optional_string(params, &["selector"]),
                    dx: match optional_i64(params, &["dx"]) {
                        Ok(value) => value.unwrap_or(0),
                        Err(error) => return error_response(id, error),
                    },
                    dy: match optional_i64(params, &["dy"]) {
                        Ok(value) => value.unwrap_or(0),
                        Err(error) => return error_response(id, error),
                    },
                },
                "browser.scroll_into_view" => BrowserAction::ScrollIntoView {
                    selector: match required_browser_selector(method, params) {
                        Ok(value) => value,
                        Err(error) => return error_response(id, error),
                    },
                },
                "browser.focus_webview" => BrowserAction::Focus,
                "browser.is_webview_focused" => BrowserAction::IsFocused,
                "browser.eval" => {
                    let Some(script) = optional_string(params, &["script"]) else {
                        return error_response(
                            id,
                            BridgeError::invalid_params("browser.eval requires script"),
                        );
                    };
                    BrowserAction::Eval { script }
                }
                "browser.get.title" => BrowserAction::GetTitle,
                "browser.get.text" => BrowserAction::GetText {
                    selector: optional_string(params, &["selector"]),
                },
                "browser.get.value" => {
                    let Some(selector) = optional_string(params, &["selector"]) else {
                        return error_response(
                            id,
                            BridgeError::invalid_params("browser.get.value requires selector"),
                        );
                    };
                    BrowserAction::GetValue { selector }
                }
                "browser.get.attr" => {
                    let Some(selector) = optional_string(params, &["selector"]) else {
                        return error_response(
                            id,
                            BridgeError::invalid_params("browser.get.attr requires selector"),
                        );
                    };
                    let Some(name) = optional_string(params, &["name", "attr"]) else {
                        return error_response(
                            id,
                            BridgeError::invalid_params("browser.get.attr requires name or attr"),
                        );
                    };
                    BrowserAction::GetAttr { selector, name }
                }
                "browser.get.count" => {
                    let Some(selector) = optional_string(params, &["selector"]) else {
                        return error_response(
                            id,
                            BridgeError::invalid_params("browser.get.count requires selector"),
                        );
                    };
                    BrowserAction::GetCount { selector }
                }
                "browser.get.box" => {
                    let Some(selector) = optional_string(params, &["selector"]) else {
                        return error_response(
                            id,
                            BridgeError::invalid_params("browser.get.box requires selector"),
                        );
                    };
                    BrowserAction::GetBox { selector }
                }
                "browser.get.html" => BrowserAction::GetHtml {
                    selector: optional_string(params, &["selector"]),
                },
                "browser.get.styles" => {
                    let Some(selector) = optional_string(params, &["selector"]) else {
                        return error_response(
                            id,
                            BridgeError::invalid_params("browser.get.styles requires selector"),
                        );
                    };
                    BrowserAction::GetStyles {
                        selector,
                        property: optional_string(params, &["property", "name"]),
                    }
                }
                "browser.is.checked" => BrowserAction::IsChecked {
                    selector: match required_browser_selector(method, params) {
                        Ok(value) => value,
                        Err(error) => return error_response(id, error),
                    },
                },
                "browser.is.enabled" => BrowserAction::IsEnabled {
                    selector: match required_browser_selector(method, params) {
                        Ok(value) => value,
                        Err(error) => return error_response(id, error),
                    },
                },
                "browser.is.visible" => BrowserAction::IsVisible {
                    selector: match required_browser_selector(method, params) {
                        Ok(value) => value,
                        Err(error) => return error_response(id, error),
                    },
                },
                "browser.screenshot" => BrowserAction::Screenshot {
                    path: optional_string(params, &["path", "out"]),
                    full_page: match optional_bool(params, "full_page") {
                        Ok(Some(value)) => value,
                        Ok(None) => match optional_bool(params, "fullPage") {
                            Ok(value) => value.unwrap_or(false),
                            Err(error) => return error_response(id, error),
                        },
                        Err(error) => return error_response(id, error),
                    },
                },
                "browser.find.role"
                | "browser.find.text"
                | "browser.find.label"
                | "browser.find.placeholder"
                | "browser.find.alt"
                | "browser.find.title"
                | "browser.find.testid"
                | "browser.find.first"
                | "browser.find.last"
                | "browser.find.nth" => match parse_browser_find_action(method, params) {
                    Ok(action) => action,
                    Err(error) => return error_response(id, error),
                },
                "browser.frame.main" => BrowserAction::FrameMain,
                "browser.frame.select" => {
                    let Some(selector) = optional_string(params, &["selector", "frame_id"]) else {
                        return error_response(
                            id,
                            BridgeError::invalid_params(
                                "browser.frame.select requires selector or frame_id",
                            ),
                        );
                    };
                    BrowserAction::FrameSelect { selector }
                }
                "browser.dialog.accept" => BrowserAction::DialogAccept {
                    text: optional_string(params, &["text", "value"]),
                },
                "browser.dialog.dismiss" => BrowserAction::DialogDismiss,
                "browser.download.wait" => BrowserAction::DownloadWait {
                    path: optional_string(params, &["path"]),
                    timeout_ms: match optional_u64(params, &["timeout_ms", "timeoutMs"]) {
                        Ok(value) => value.unwrap_or(10_000).min(120_000),
                        Err(error) => return error_response(id, error),
                    },
                },
                "browser.snapshot" => BrowserAction::Snapshot {
                    interactive: match optional_bool(params, "interactive") {
                        Ok(value) => value.unwrap_or(false),
                        Err(error) => return error_response(id, error),
                    },
                    compact: match optional_bool(params, "compact") {
                        Ok(value) => value.unwrap_or(false),
                        Err(error) => return error_response(id, error),
                    },
                    max_depth: match optional_usize(params, &["max_depth", "maxDepth"]) {
                        Ok(value) => value,
                        Err(error) => return error_response(id, error),
                    },
                },
                "browser.wait" => {
                    let timeout_ms = match optional_u64(params, &["timeout_ms", "timeoutMs"]) {
                        Ok(value) => value.unwrap_or(5_000),
                        Err(error) => return error_response(id, error),
                    };
                    let action = BrowserAction::Wait {
                        selector: optional_string(params, &["selector"]),
                        text: optional_string(params, &["text"]),
                        url_contains: optional_string(params, &["url_contains", "urlContains"]),
                        load_state: optional_string(params, &["load_state", "loadState"]),
                        function: optional_string(params, &["function", "script"]),
                        timeout_ms,
                    };
                    if let BrowserAction::Wait {
                        selector,
                        text,
                        url_contains,
                        load_state,
                        function,
                        ..
                    } = &action
                    {
                        if selector.is_none()
                            && text.is_none()
                            && url_contains.is_none()
                            && load_state.is_none()
                            && function.is_none()
                        {
                            return error_response(
                                id,
                                BridgeError::invalid_params(
                                    "browser.wait requires selector, text, url_contains, load_state, or function",
                                ),
                            );
                        }
                    }
                    action
                }
                "browser.addscript" => {
                    let Some(script) = optional_string(params, &["script"]) else {
                        return error_response(
                            id,
                            BridgeError::invalid_params("browser.addscript requires script"),
                        );
                    };
                    BrowserAction::AddScript { script }
                }
                "browser.addinitscript" => {
                    let Some(script) = optional_string(params, &["script"]) else {
                        return error_response(
                            id,
                            BridgeError::invalid_params("browser.addinitscript requires script"),
                        );
                    };
                    BrowserAction::AddInitScript { script }
                }
                "browser.addstyle" => {
                    let Some(css) = optional_string(params, &["css", "style"]) else {
                        return error_response(
                            id,
                            BridgeError::invalid_params("browser.addstyle requires css or style"),
                        );
                    };
                    BrowserAction::AddStyle { css }
                }
                "browser.console.list" => BrowserAction::ConsoleList,
                "browser.console.clear" => BrowserAction::ConsoleClear,
                "browser.errors.list" => BrowserAction::ErrorsList,
                "browser.errors.clear" => BrowserAction::ErrorsClear,
                "browser.highlight" => BrowserAction::Highlight {
                    selector: match required_browser_selector(method, params) {
                        Ok(value) => value,
                        Err(error) => return error_response(id, error),
                    },
                },
                "browser.cookies.get" => BrowserAction::CookiesGet {
                    name: optional_string(params, &["name"]),
                },
                "browser.cookies.set" => {
                    let Some(name) = optional_string(params, &["name"]) else {
                        return error_response(
                            id,
                            BridgeError::invalid_params("browser.cookies.set requires name"),
                        );
                    };
                    let Some(value) = optional_string(params, &["value"]) else {
                        return error_response(
                            id,
                            BridgeError::invalid_params("browser.cookies.set requires value"),
                        );
                    };
                    BrowserAction::CookiesSet { name, value }
                }
                "browser.cookies.clear" => BrowserAction::CookiesClear {
                    name: optional_string(params, &["name"]),
                },
                "browser.storage.get" => {
                    let Some(key) = optional_string(params, &["key"]) else {
                        return error_response(
                            id,
                            BridgeError::invalid_params("browser.storage.get requires key"),
                        );
                    };
                    BrowserAction::StorageGet {
                        storage_type: match browser_storage_type(params) {
                            Ok(value) => value,
                            Err(error) => return error_response(id, error),
                        },
                        key,
                    }
                }
                "browser.storage.set" => {
                    let Some(key) = optional_string(params, &["key"]) else {
                        return error_response(
                            id,
                            BridgeError::invalid_params("browser.storage.set requires key"),
                        );
                    };
                    let Some(value) = optional_string(params, &["value"]) else {
                        return error_response(
                            id,
                            BridgeError::invalid_params("browser.storage.set requires value"),
                        );
                    };
                    BrowserAction::StorageSet {
                        storage_type: match browser_storage_type(params) {
                            Ok(value) => value,
                            Err(error) => return error_response(id, error),
                        },
                        key,
                        value,
                    }
                }
                "browser.storage.clear" => BrowserAction::StorageClear {
                    storage_type: match browser_storage_type(params) {
                        Ok(value) => value,
                        Err(error) => return error_response(id, error),
                    },
                    key: optional_string(params, &["key"]),
                },
                "browser.state.save" | "browser.state.load" => {
                    let Some(path) = optional_string(params, &["path"]) else {
                        return error_response(
                            id,
                            BridgeError::invalid_params(format!("{method} requires path")),
                        );
                    };
                    if path.trim().is_empty() {
                        return error_response(
                            id,
                            BridgeError::invalid_params(format!(
                                "{method} requires non-empty path"
                            )),
                        );
                    }
                    if method == "browser.state.save" {
                        BrowserAction::StateSave { path }
                    } else {
                        BrowserAction::StateLoad { path }
                    }
                }
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
        "browser.tab.list" | "browser.tab.new" | "browser.tab.switch" | "browser.tab.close" => {
            let surface_hint =
                match optional_ref_handle(params, &["surface_id", "surface", "id"], "surface:") {
                    Ok(Some(value)) if !value.trim().is_empty() => value,
                    Ok(_) => {
                        return error_response(
                            id,
                            BridgeError::invalid_params(format!("{method} requires surface_id")),
                        );
                    }
                    Err(error) => return error_response(id, error),
                };
            let action = match method {
                "browser.tab.list" => BrowserTabAction::List,
                "browser.tab.new" => BrowserTabAction::New {
                    url: optional_string(params, &["url"]),
                },
                "browser.tab.switch" => {
                    let target_surface_hint = match optional_ref_handle(
                        params,
                        &["target_surface_id", "tab_id", "target_id"],
                        "surface:",
                    ) {
                        Ok(Some(value)) if !value.trim().is_empty() => value,
                        Ok(_) => {
                            return error_response(
                                id,
                                BridgeError::invalid_params(
                                    "browser.tab.switch requires target_surface_id",
                                ),
                            );
                        }
                        Err(error) => return error_response(id, error),
                    };
                    BrowserTabAction::Switch {
                        target_surface_hint,
                    }
                }
                "browser.tab.close" => BrowserTabAction::Close {
                    target_surface_hint: match optional_ref_handle(
                        params,
                        &["target_surface_id", "tab_id", "target_id"],
                        "surface:",
                    ) {
                        Ok(value) => value,
                        Err(error) => return error_response(id, error),
                    },
                },
                _ => unreachable!("browser tab method matched above"),
            };
            let target = match parse_optional_workspace_target(params, true) {
                Ok(target) => target,
                Err(error) => return error_response(id, error),
            };
            let (reply, rx) = mpsc::channel();
            (
                ControlCommand::BrowserTabAction {
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
        "pane.last" | "last-pane" => {
            let target = match parse_optional_workspace_target(params, true) {
                Ok(target) => target,
                Err(error) => return error_response(id, error),
            };
            let (reply, rx) = mpsc::channel();
            (ControlCommand::LastPane { target, reply }, rx)
        }
        "pane.resize" | "resize-pane" => {
            let target = match parse_optional_workspace_target(params, true) {
                Ok(target) => target,
                Err(error) => return error_response(id, error),
            };
            let pane_id = match optional_ref_handle(params, &["pane_id", "id"], "pane:") {
                Ok(Some(value)) if !value.trim().is_empty() => value,
                Ok(_) => {
                    return error_response(
                        id,
                        BridgeError::invalid_params("pane.resize requires pane_id"),
                    );
                }
                Err(error) => return error_response(id, error),
            };
            let direction = match optional_string(params, &["direction"]) {
                Some(raw) => match parse_pane_resize_direction(raw.as_str()) {
                    Ok(direction) => direction,
                    Err(error) => return error_response(id, error),
                },
                None => "right".to_string(),
            };
            let amount = match optional_u64(params, &["amount"]) {
                Ok(amount) => amount.unwrap_or(1),
                Err(error) => return error_response(id, error),
            };
            let (reply, rx) = mpsc::channel();
            (
                ControlCommand::ResizePane {
                    target,
                    pane_id,
                    direction,
                    amount,
                    reply,
                },
                rx,
            )
        }
        "pane.swap" | "swap-pane" => {
            let target = match parse_optional_workspace_target(params, true) {
                Ok(target) => target,
                Err(error) => return error_response(id, error),
            };
            let pane_id =
                match optional_ref_handle(params, &["pane_id", "first_pane_id", "id"], "pane:") {
                    Ok(Some(value)) if !value.trim().is_empty() => value,
                    Ok(_) => {
                        return error_response(
                            id,
                            BridgeError::invalid_params("pane.swap requires pane_id"),
                        );
                    }
                    Err(error) => return error_response(id, error),
                };
            let target_pane_id = match optional_ref_handle(
                params,
                &["target_pane_id", "second_pane_id", "target_pane"],
                "pane:",
            ) {
                Ok(Some(value)) if !value.trim().is_empty() => value,
                Ok(_) => {
                    return error_response(
                        id,
                        BridgeError::invalid_params("pane.swap requires target_pane_id"),
                    );
                }
                Err(error) => return error_response(id, error),
            };
            let (reply, rx) = mpsc::channel();
            (
                ControlCommand::SwapPane {
                    target,
                    pane_id,
                    target_pane_id,
                    reply,
                },
                rx,
            )
        }
        "pane.join" | "join-pane" => {
            let target = match parse_optional_workspace_target(params, true) {
                Ok(target) => target,
                Err(error) => return error_response(id, error),
            };
            let target_pane_id =
                match optional_ref_handle(params, &["target_pane_id", "target_pane"], "pane:") {
                    Ok(Some(value)) if !value.trim().is_empty() => value,
                    Ok(_) => {
                        return error_response(
                            id,
                            BridgeError::invalid_params("pane.join requires target_pane_id"),
                        );
                    }
                    Err(error) => return error_response(id, error),
                };
            let source_pane_id =
                match optional_ref_handle(params, &["source_pane_id", "pane_id", "id"], "pane:") {
                    Ok(value) => value,
                    Err(error) => return error_response(id, error),
                };
            let source_surface_id = match optional_ref_handle(
                params,
                &["source_surface_id", "surface_id", "panel_id"],
                "surface:",
            ) {
                Ok(value) => value,
                Err(error) => return error_response(id, error),
            };
            let (reply, rx) = mpsc::channel();
            (
                ControlCommand::JoinPane {
                    target,
                    source_pane_id,
                    source_surface_id,
                    target_pane_id,
                    reply,
                },
                rx,
            )
        }
        "pane.break" | "break-pane" => {
            let target = match parse_optional_workspace_target(params, true) {
                Ok(target) => target,
                Err(error) => return error_response(id, error),
            };
            let pane_id = match optional_ref_handle(params, &["pane_id", "id"], "pane:") {
                Ok(value) => value,
                Err(error) => return error_response(id, error),
            };
            let surface_hint =
                match optional_ref_handle(params, &["surface_id", "panel_id"], "surface:") {
                    Ok(value) => value,
                    Err(error) => return error_response(id, error),
                };
            let (reply, rx) = mpsc::channel();
            (
                ControlCommand::BreakPane {
                    target,
                    pane_id,
                    surface_hint,
                    reply,
                },
                rx,
            )
        }
        "surface.split" | "new-split" => {
            let target = match parse_optional_workspace_target(params, true) {
                Ok(target) => target,
                Err(error) => return error_response(id, error),
            };
            let direction = match parse_pane_create_direction(
                optional_string(params, &["direction"])
                    .unwrap_or_else(|| "right".to_string())
                    .as_str(),
            ) {
                Ok(direction) => direction,
                Err(error) => return error_response(id, error),
            };
            let source_surface_id =
                match optional_ref_handle(params, &["surface_id", "panel_id", "id"], "surface:") {
                    Ok(value) => value,
                    Err(error) => return error_response(id, error),
                };
            let (reply, rx) = mpsc::channel();
            (
                ControlCommand::CreatePane {
                    request: CreatePaneRequest {
                        target,
                        source_pane_id: None,
                        source_surface_id,
                        direction,
                        pane_type: PaneCreateType::Terminal,
                        command: optional_string(params, &["command"]),
                        url: None,
                    },
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
        "surface.close" | "close-surface" => {
            let target = match parse_optional_workspace_target(params, true) {
                Ok(target) => target,
                Err(error) => return error_response(id, error),
            };
            let surface_hint =
                match optional_ref_handle(params, &["surface_id", "panel_id", "id"], "surface:") {
                    Ok(value) => value,
                    Err(error) => return error_response(id, error),
                };
            let (reply, rx) = mpsc::channel();
            (
                ControlCommand::CloseSurface {
                    target,
                    surface_hint,
                    reply,
                },
                rx,
            )
        }
        "surface.move" | "move-surface" => {
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
                            BridgeError::invalid_params("surface.move requires surface_id"),
                        );
                    }
                    Err(error) => return error_response(id, error),
                };
            let target_pane_id =
                match optional_ref_handle(params, &["target_pane_id", "pane_id"], "pane:") {
                    Ok(Some(value)) if !value.trim().is_empty() => value,
                    Ok(_) => {
                        return error_response(
                            id,
                            BridgeError::invalid_params("surface.move requires target_pane_id"),
                        );
                    }
                    Err(error) => return error_response(id, error),
                };
            let index = match optional_index(params, "index") {
                Ok(index) => index,
                Err(error) => return error_response(id, error),
            };
            let (reply, rx) = mpsc::channel();
            (
                ControlCommand::MoveSurface {
                    target,
                    surface_hint,
                    target_pane_id,
                    index,
                    reply,
                },
                rx,
            )
        }
        "surface.reorder" | "reorder-surface" => {
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
                            BridgeError::invalid_params("surface.reorder requires surface_id"),
                        );
                    }
                    Err(error) => return error_response(id, error),
                };
            let index = match optional_index(params, "index") {
                Ok(index) => index,
                Err(error) => return error_response(id, error),
            };
            let before_surface_hint =
                match optional_ref_handle(params, &["before_surface_id"], "surface:") {
                    Ok(value) => value,
                    Err(error) => return error_response(id, error),
                };
            let after_surface_hint =
                match optional_ref_handle(params, &["after_surface_id"], "surface:") {
                    Ok(value) => value,
                    Err(error) => return error_response(id, error),
                };
            let target_count = usize::from(index.is_some())
                + usize::from(before_surface_hint.is_some())
                + usize::from(after_surface_hint.is_some());
            if target_count != 1 {
                return error_response(
                    id,
                    BridgeError::invalid_params(
                        "surface.reorder requires exactly one target: index|before_surface_id|after_surface_id",
                    ),
                );
            }
            let (reply, rx) = mpsc::channel();
            (
                ControlCommand::ReorderSurface {
                    target,
                    surface_hint,
                    index,
                    before_surface_hint,
                    after_surface_hint,
                    reply,
                },
                rx,
            )
        }
        "surface.drag_to_split" | "drag-surface-to-split" | "split-off" => {
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
                            BridgeError::invalid_params(
                                "surface.drag_to_split requires surface_id",
                            ),
                        );
                    }
                    Err(error) => return error_response(id, error),
                };
            let direction = match parse_pane_create_direction(
                optional_string(params, &["direction"])
                    .unwrap_or_else(|| "right".to_string())
                    .as_str(),
            ) {
                Ok(direction) => direction,
                Err(error) => return error_response(id, error),
            };
            let (reply, rx) = mpsc::channel();
            (
                ControlCommand::DragSurfaceToSplit {
                    target,
                    surface_hint,
                    direction,
                    reply,
                },
                rx,
            )
        }
        "surface.refresh" | "refresh-surfaces" => {
            let target = match parse_optional_workspace_target(params, true) {
                Ok(target) => target,
                Err(error) => return error_response(id, error),
            };
            let surface_hint =
                match optional_ref_handle(params, &["surface_id", "panel_id", "id"], "surface:") {
                    Ok(value) => value,
                    Err(error) => return error_response(id, error),
                };
            let (reply, rx) = mpsc::channel();
            (
                ControlCommand::RefreshSurfaces {
                    target,
                    surface_hint,
                    reply,
                },
                rx,
            )
        }
        "surface.clear_history" | "clear-history" => {
            let target = match parse_optional_workspace_target(params, true) {
                Ok(target) => target,
                Err(error) => return error_response(id, error),
            };
            let surface_hint =
                match optional_ref_handle(params, &["surface_id", "panel_id", "id"], "surface:") {
                    Ok(value) => value,
                    Err(error) => return error_response(id, error),
                };
            let (reply, rx) = mpsc::channel();
            (
                ControlCommand::ClearSurfaceHistory {
                    target,
                    surface_hint,
                    reply,
                },
                rx,
            )
        }
        "surface.respawn" | "respawn-pane" => {
            let Some(command) = optional_string(params, &["command"]) else {
                return error_response(
                    id,
                    BridgeError::invalid_params("surface.respawn requires command"),
                );
            };
            let command = command.trim().to_string();
            if command.is_empty() {
                return error_response(
                    id,
                    BridgeError::invalid_params("surface.respawn requires non-empty command"),
                );
            }
            let target = match parse_optional_workspace_target(params, true) {
                Ok(target) => target,
                Err(error) => return error_response(id, error),
            };
            let (reply, rx) = mpsc::channel();
            (
                ControlCommand::RespawnSurface {
                    target,
                    surface_hint: match optional_ref_handle(
                        params,
                        &["surface_id", "panel_id", "id"],
                        "surface:",
                    ) {
                        Ok(value) => value,
                        Err(error) => return error_response(id, error),
                    },
                    command,
                    tmux_start_command: optional_string(params, &["tmux_start_command"]),
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
            let lines = match optional_u64(params, &["lines"]) {
                Ok(lines) => lines,
                Err(error) => return error_response(id, error),
            };
            if lines == Some(0) {
                return error_response(
                    id,
                    BridgeError::invalid_params("lines must be greater than 0"),
                );
            }
            let scrollback = match optional_bool(params, "scrollback") {
                Ok(scrollback) => scrollback.unwrap_or(lines.is_some()),
                Err(error) => return error_response(id, error),
            };
            let (reply, rx) = mpsc::channel();
            (
                ControlCommand::ReadSurfaceText {
                    target,
                    surface_hint,
                    lines,
                    scrollback,
                    reply,
                },
                rx,
            )
        }
        "workspace.create" | "new-workspace" => {
            let environment = match parse_workspace_environment(params) {
                Ok(environment) => environment,
                Err(error) => return error_response(id, error),
            };
            let (reply, rx) = mpsc::channel();
            (
                ControlCommand::CreateWorkspace {
                    name: optional_string(params, &["name", "title"]),
                    cwd: optional_string(params, &["cwd"]),
                    command: optional_string(params, &["command"]),
                    environment,
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
        "workspace.next" | "next-window" => {
            let (reply, rx) = mpsc::channel();
            (
                ControlCommand::NavigateWorkspace {
                    action: WorkspaceNavigation::Next,
                    reply,
                },
                rx,
            )
        }
        "workspace.previous" | "previous-window" => {
            let (reply, rx) = mpsc::channel();
            (
                ControlCommand::NavigateWorkspace {
                    action: WorkspaceNavigation::Previous,
                    reply,
                },
                rx,
            )
        }
        "workspace.last" | "last-window" => {
            let (reply, rx) = mpsc::channel();
            (
                ControlCommand::NavigateWorkspace {
                    action: WorkspaceNavigation::Last,
                    reply,
                },
                rx,
            )
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
            let surface_hint = optional_string(params, &["surface_id", "surface", "tab_id", "tab"]);
            // allow_name = true: lets agent hooks target a peer by name.
            let target = match parse_optional_workspace_target(params, true) {
                Ok(target) => target,
                Err(error) => return error_response(id, error),
            };
            let (reply, rx) = mpsc::channel();
            (
                ControlCommand::CreateNotification {
                    target,
                    surface_hint,
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

fn auth_login_response(id: Option<Value>, params: &Value, authenticated: &mut bool) -> V2Response {
    let Some(password) = params.get("password").and_then(Value::as_str) else {
        return V2Response::error(
            id,
            INVALID_PARAMS_CODE,
            "auth.login requires password",
            None,
        );
    };
    match auth::configured_socket_password() {
        Ok(expected) if auth::password_matches(password, &expected) => {
            *authenticated = true;
            V2Response::success(id, json!({ "ok": true, "authenticated": true }))
        }
        Ok(_) => V2Response::error(id, UNAUTHORIZED_CODE, "unauthorized: bad password", None),
        Err(error) => V2Response::error(
            id,
            INTERNAL_ERROR_CODE,
            format!("socket password is not configured: {error}"),
            None,
        ),
    }
}

fn legacy_auth_response(input: &str, authenticated: &mut bool) -> Option<String> {
    let password = input.strip_prefix("auth ")?;
    match auth::configured_socket_password() {
        Ok(expected) if auth::password_matches(password, &expected) => {
            *authenticated = true;
            Some("OK\n".to_string())
        }
        Ok(_) => Some("ERROR: unauthorized: bad password\n".to_string()),
        Err(error) => Some(format!(
            "ERROR: socket password is not configured: {error}\n"
        )),
    }
}

#[cfg(test)]
fn dispatch_request(input: &str, dispatch: &dyn Fn(ControlCommand)) -> V2Response {
    let mut authenticated = true;
    dispatch_request_with_auth(
        input,
        dispatch,
        SocketControlMode::AllowAll,
        &mut authenticated,
    )
}

fn dispatch_request_with_auth(
    input: &str,
    dispatch: &dyn Fn(ControlCommand),
    control_mode: SocketControlMode,
    authenticated: &mut bool,
) -> V2Response {
    match parse_request(input) {
        Ok(request) => {
            if request.method == "auth.login" {
                return auth_login_response(request.id, &request.params, authenticated);
            }
            if control_mode.requires_password() && !*authenticated {
                return V2Response::error(
                    request.id,
                    UNAUTHORIZED_CODE,
                    "unauthorized: authentication required",
                    None,
                );
            }
            handle_method(request.id, &request.method, request.params, dispatch)
        }
        Err(error) => error_response(None, error),
    }
}

/// purpose: Handle CMUX event stream takeover requests outside JSON-RPC response framing.
/// inputs: Raw request line and the socket writer.
/// returns/effects: Streams retained/live JSONL frames and returns true when the stream was handled.
fn try_handle_event_stream(input: &str, writer: &mut UnixStream) -> io::Result<bool> {
    let request = match parse_request(input) {
        Ok(request) => request,
        Err(_) => return Ok(false),
    };
    if request.method != "events.stream" {
        return Ok(false);
    }

    crate::event_bus::bus().stream(&request.params, writer)?;
    Ok(true)
}

fn handle_client(
    stream: UnixStream,
    control_mode: SocketControlMode,
    dispatch: &(dyn Fn(ControlCommand) + Send + Sync + 'static),
) -> io::Result<()> {
    stream.set_read_timeout(Some(request_io::CLIENT_IDLE_TIMEOUT))?;
    let reader_stream = stream.try_clone()?;
    reader_stream.set_read_timeout(Some(request_io::CLIENT_IDLE_TIMEOUT))?;
    let mut reader = io::BufReader::new(reader_stream);
    let mut writer = stream;
    let mut line_buf = Vec::with_capacity(4096);
    let mut authenticated = !control_mode.requires_password();

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

        if let Some(response) = legacy_auth_response(input, &mut authenticated) {
            writer.write_all(response.as_bytes())?;
            writer.flush()?;
            continue;
        }

        if control_mode.requires_password() && !authenticated {
            let response =
                dispatch_request_with_auth(input, dispatch, control_mode, &mut authenticated);
            let mut payload = serde_json::to_string(&response)
                .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error.to_string()))?;
            payload.push('\n');
            writer.write_all(payload.as_bytes())?;
            writer.flush()?;
            continue;
        }

        if try_handle_event_stream(input, &mut writer)? {
            return Ok(());
        }

        let response =
            dispatch_request_with_auth(input, dispatch, control_mode, &mut authenticated);
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
            let control_mode = match auth::socket_control_mode_from_env() {
                Ok(mode) => mode,
                Err(error) => {
                    eprintln!("limux: invalid control socket mode: {error}");
                    return;
                }
            };
            if control_mode.requires_password() {
                match auth::configured_socket_password() {
                    Ok(_) => {}
                    Err(error) => {
                        eprintln!("limux: password control mode is not configured: {error}");
                        return;
                    }
                }
            }
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
                                if let Err(error) =
                                    handle_client(stream, control_mode, dispatch.as_ref())
                                {
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
    use std::io::{BufRead, BufReader};
    use std::sync::{Mutex, OnceLock};
    use std::thread;

    fn feed_test_guard() -> std::sync::MutexGuard<'static, ()> {
        static FEED_TEST_LOCK: OnceLock<Mutex<()>> = OnceLock::new();
        FEED_TEST_LOCK
            .get_or_init(|| Mutex::new(()))
            .lock()
            .expect("feed test lock")
    }

    fn auth_test_guard() -> std::sync::MutexGuard<'static, ()> {
        static AUTH_TEST_LOCK: OnceLock<Mutex<()>> = OnceLock::new();
        AUTH_TEST_LOCK
            .get_or_init(|| Mutex::new(()))
            .lock()
            .expect("auth test lock")
    }

    struct EnvGuard {
        key: &'static str,
        old: Option<std::ffi::OsString>,
    }

    impl EnvGuard {
        fn set(key: &'static str, value: Option<&str>) -> Self {
            let old = std::env::var_os(key);
            match value {
                Some(value) => unsafe { std::env::set_var(key, value) },
                None => unsafe { std::env::remove_var(key) },
            }
            Self { key, old }
        }
    }

    impl Drop for EnvGuard {
        fn drop(&mut self) {
            match &self.old {
                Some(value) => unsafe { std::env::set_var(self.key, value) },
                None => unsafe { std::env::remove_var(self.key) },
            }
        }
    }

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
    fn capabilities_include_config_reload_methods() {
        assert!(METHODS.contains(&"reload_config"));
        assert!(METHODS.contains(&"config.reload"));
    }

    #[test]
    fn capabilities_include_socket_auth_methods() {
        assert!(METHODS.contains(&"auth.login"));
        assert!(METHODS.contains(&"auth.status"));
    }

    #[test]
    fn password_mode_rejects_request_until_auth_login_succeeds() {
        let _lock = auth_test_guard();
        let _password = EnvGuard::set("CMUX_SOCKET_PASSWORD", Some("secret"));
        let mut authenticated = false;

        let rejected = dispatch_request_with_auth(
            r#"{"id":"1","method":"system.ping","params":{}}"#,
            &|_| panic!("unauthenticated command must not dispatch"),
            SocketControlMode::Password,
            &mut authenticated,
        );
        assert!(!rejected.ok);
        assert_eq!(
            rejected.error.as_ref().map(|error| error.code),
            Some(UNAUTHORIZED_CODE)
        );

        let login = dispatch_request_with_auth(
            r#"{"id":"2","method":"auth.login","params":{"password":"secret"}}"#,
            &|_| panic!("auth.login must not dispatch through host command queue"),
            SocketControlMode::Password,
            &mut authenticated,
        );
        assert!(login.ok);
        assert!(authenticated);

        let accepted = dispatch_request_with_auth(
            r#"{"id":"3","method":"system.ping","params":{}}"#,
            &|_| panic!("system.ping is handled without queue dispatch"),
            SocketControlMode::Password,
            &mut authenticated,
        );
        assert!(accepted.ok);
        assert_eq!(
            accepted.result.as_ref().and_then(|value| value.get("pong")),
            Some(&json!(true))
        );
    }

    #[test]
    fn legacy_cmux_auth_command_returns_text_status() {
        let _lock = auth_test_guard();
        let _password = EnvGuard::set("CMUX_SOCKET_PASSWORD", Some("secret"));
        let mut authenticated = false;

        let ok = legacy_auth_response("auth secret", &mut authenticated).expect("legacy auth");
        assert_eq!(ok, "OK\n");
        assert!(authenticated);

        let mut rejected_auth = false;
        let error = legacy_auth_response("auth wrong", &mut rejected_auth).expect("legacy auth");
        assert_eq!(error, "ERROR: unauthorized: bad password\n");
        assert!(!rejected_auth);
    }

    #[test]
    fn capabilities_include_settings_open_method() {
        assert!(METHODS.contains(&"settings.open"));
    }

    #[test]
    fn capabilities_include_feed_methods() {
        assert!(METHODS.contains(&"feed.push"));
        assert!(METHODS.contains(&"feed.permission.reply"));
        assert!(METHODS.contains(&"feed.question.reply"));
        assert!(METHODS.contains(&"feed.exit_plan.reply"));
    }

    #[test]
    fn capabilities_include_workspace_group_list() {
        assert!(METHODS.contains(&"workspace.group.list"));
    }

    #[test]
    fn capabilities_include_remote_workspace_methods() {
        assert!(METHODS.contains(&"workspace.remote.reconnect"));
        assert!(METHODS.contains(&"workspace.remote.disconnect"));
    }

    #[test]
    fn capabilities_include_right_sidebar_method() {
        assert!(METHODS.contains(&"right_sidebar"));
    }

    #[test]
    fn capabilities_include_sidebar_metadata_methods() {
        assert!(METHODS.contains(&"sidebar.status.set"));
        assert!(METHODS.contains(&"sidebar.progress.set"));
        assert!(METHODS.contains(&"sidebar.log.append"));
        assert!(METHODS.contains(&"sidebar.state"));
        assert!(METHODS.contains(&"set_status"));
    }

    #[test]
    fn right_sidebar_route_accepts_cmux_modes_and_targets() {
        let request = r#"{"id":1,"method":"right_sidebar","params":{"action":"set","mode":"dock","focus":false,"workspace_id":"workspace:2","window_id":"window:7"}}"#;
        let response = dispatch_request(request, &|command| match command {
            ControlCommand::RightSidebar {
                action,
                target,
                reply,
            } => {
                assert_eq!(
                    action,
                    RightSidebarAction::SetMode {
                        mode: RightSidebarMode::Dock,
                        focus: false
                    }
                );
                assert_eq!(target.workspace_id.as_deref(), Some("2"));
                assert_eq!(target.window_id.as_deref(), Some("7"));
                reply
                    .send(Ok(
                        json!({"visible": true, "mode": "dock", "focused": false}),
                    ))
                    .expect("reply sends");
            }
            other => panic!("unexpected command: {other:?}"),
        });

        assert_eq!(response.error, None);
        assert_eq!(response.result.expect("result")["mode"], "dock");
    }

    #[test]
    fn right_sidebar_route_rejects_unknown_modes() {
        let request =
            r#"{"id":1,"method":"right_sidebar","params":{"action":"set","mode":"bogus"}}"#;
        let response = dispatch_request(request, &|command| {
            panic!("invalid mode should not dispatch: {command:?}");
        });

        assert_eq!(response.result, None);
        let error = response.error.expect("error");
        assert_eq!(error.code, INVALID_PARAMS_CODE);
        assert!(error.message.contains("Unknown right-sidebar mode"));
    }

    #[test]
    fn sidebar_status_route_accepts_cmux_aliases_and_targets() {
        let request = r##"{"id":1,"method":"set_status","params":{"workspace_id":"workspace:2","key":"build","value":"running","color":"#ff9500","priority":"80"}}"##;
        let response = dispatch_request(request, &|command| match command {
            ControlCommand::Sidebar {
                action,
                target,
                reply,
            } => {
                assert_eq!(target, WorkspaceTarget::Handle("workspace:2".to_string()));
                assert_eq!(
                    action,
                    SidebarAction::SetStatus {
                        key: "build".to_string(),
                        value: "running".to_string(),
                        icon: None,
                        color: Some("#ff9500".to_string()),
                        url: None,
                        priority: 80,
                    }
                );
                reply.send(Ok(json!({"ok": true}))).expect("reply sends");
            }
            other => panic!("unexpected command: {other:?}"),
        });

        assert_eq!(response.error, None);
    }

    #[test]
    fn sidebar_progress_and_log_routes_validate_params() {
        let progress = dispatch_request(
            r#"{"id":1,"method":"sidebar.progress.set","params":{"value":0.5,"label":"Building"}}"#,
            &|command| match command {
                ControlCommand::Sidebar {
                    action,
                    target,
                    reply,
                } => {
                    assert_eq!(target, WorkspaceTarget::Active);
                    assert_eq!(
                        action,
                        SidebarAction::SetProgress {
                            value: 0.5,
                            label: Some("Building".to_string())
                        }
                    );
                    reply.send(Ok(json!({"ok": true}))).expect("reply sends");
                }
                other => panic!("unexpected command: {other:?}"),
            },
        );
        assert_eq!(progress.error, None);

        let invalid_progress = dispatch_request(
            r#"{"id":1,"method":"sidebar.progress.set","params":{"value":1.5}}"#,
            &|command| panic!("invalid progress should not dispatch: {command:?}"),
        );
        assert_eq!(
            invalid_progress.error.as_ref().map(|error| error.code),
            Some(INVALID_PARAMS_CODE)
        );

        let invalid_log = dispatch_request(
            r#"{"id":1,"method":"sidebar.log.append","params":{"level":"debug","message":"x"}}"#,
            &|command| panic!("invalid log should not dispatch: {command:?}"),
        );
        assert_eq!(
            invalid_log.error.as_ref().map(|error| error.code),
            Some(INVALID_PARAMS_CODE)
        );
    }

    #[test]
    fn remote_workspace_methods_fail_before_dispatch() {
        for method in ["workspace.remote.reconnect", "workspace.remote.disconnect"] {
            let request = json!({
                "id": 1,
                "method": method,
                "params": {},
            })
            .to_string();
            let response = dispatch_request(&request, &|command| {
                panic!("unsupported remote method should not dispatch: {command:?}")
            });
            let error = response.error.expect("unsupported error");
            assert_eq!(error.code, NOT_SUPPORTED_CODE);
            assert!(error.message.contains("not_supported"));
            assert!(error.message.contains(method));
        }
    }

    #[test]
    fn config_reload_routes_queue_live_command() {
        for method in ["reload_config", "config.reload"] {
            let request = json!({
                "id": 1,
                "method": method,
                "params": {},
            })
            .to_string();
            let response = dispatch_request(&request, &|command| match command {
                ControlCommand::ReloadConfig { reply } => {
                    let _ = reply.send(Ok(json!({ "ok": true, "reloaded": true })));
                }
                other => panic!("unexpected command: {other:?}"),
            });
            assert_eq!(response.error, None);
            assert_eq!(response.result.expect("reload result")["reloaded"], true);
        }
    }

    #[test]
    fn settings_open_routes_queue_live_command() {
        let response = dispatch_request(
            r#"{"id":1,"method":"settings.open","params":{"target":"keyboardShortcuts","activate":false}}"#,
            &|command| match command {
                ControlCommand::OpenSettings {
                    target,
                    activate,
                    reply,
                } => {
                    assert_eq!(target.as_deref(), Some("keyboardShortcuts"));
                    assert!(!activate);
                    let _ = reply.send(Ok(json!({
                        "ok": true,
                        "opened": true,
                        "target": target,
                    })));
                }
                other => panic!("unexpected command: {other:?}"),
            },
        );

        assert_eq!(response.error, None);
        assert_eq!(response.result.expect("settings result")["opened"], true);
    }

    #[test]
    fn settings_open_rejects_unknown_target() {
        let response = dispatch_request(
            r#"{"id":1,"method":"settings.open","params":{"target":"general"}}"#,
            &|command| panic!("unexpected command: {command:?}"),
        );

        let error = response.error.expect("settings target error");
        assert_eq!(error.code, INVALID_PARAMS_CODE);
        assert!(error.message.contains("Unknown settings target"));
    }

    #[test]
    fn feed_push_acknowledges_and_lists_nonblocking_items() {
        let _guard = feed_test_guard();
        crate::feed::coordinator().reset_for_tests();
        let request = json!({
            "id": 1,
            "method": "feed.push",
            "params": {
                "event": {
                    "session_id": "s1",
                    "hook_event_name": "PreToolUse",
                    "_source": "claude",
                    "tool_name": "Bash",
                },
                "wait_timeout_seconds": 0,
            },
        })
        .to_string();
        let response = dispatch_request(&request, &|command| {
            panic!("feed.push should not queue command: {command:?}")
        });
        assert_eq!(response.error, None);
        let result = response.result.expect("feed.push result");
        assert_eq!(result["status"], "acknowledged");
        assert_eq!(result["item_id"], "feed-1");

        let listed = dispatch_request(r#"{"id":2,"method":"feed.list","params":{}}"#, &|command| {
            panic!("feed.list should not queue command: {command:?}")
        });
        let items = listed.result.expect("feed.list result")["items"]
            .as_array()
            .expect("items array")
            .clone();
        assert_eq!(items.len(), 1);
        assert_eq!(items[0]["kind"], "PreToolUse");
        assert_eq!(items[0]["status"], "telemetry");
    }

    #[test]
    fn feed_push_blocks_until_permission_reply_resolves() {
        let _guard = feed_test_guard();
        crate::feed::coordinator().reset_for_tests();
        let request = json!({
            "id": 1,
            "method": "feed.push",
            "params": {
                "event": {
                    "session_id": "s1",
                    "hook_event_name": "PermissionRequest",
                    "_source": "claude",
                    "_opencode_request_id": "req-feed-perm",
                    "tool_name": "Bash",
                },
                "wait_timeout_seconds": 1,
            },
        })
        .to_string();
        let handle = std::thread::spawn(move || {
            dispatch_request(&request, &|command| {
                panic!("feed.push should not queue command: {command:?}")
            })
        });
        std::thread::sleep(Duration::from_millis(25));
        let reply = dispatch_request(
            r#"{"id":2,"method":"feed.permission.reply","params":{"request_id":"req-feed-perm","mode":"once"}}"#,
            &|command| panic!("feed.permission.reply should not queue command: {command:?}"),
        );
        assert_eq!(reply.error, None);
        assert_eq!(
            reply.result.expect("permission reply result")["delivered"],
            true
        );

        let pushed = handle.join().expect("feed push thread");
        assert_eq!(pushed.error, None);
        let result = pushed.result.expect("feed.push resolved result");
        assert_eq!(result["status"], "resolved");
        assert_eq!(
            result["decision"],
            json!({ "kind": "permission", "mode": "once" })
        );
    }

    #[test]
    fn feed_push_times_out_and_invalid_modes_fail_loudly() {
        let _guard = feed_test_guard();
        crate::feed::coordinator().reset_for_tests();
        let request = json!({
            "id": 1,
            "method": "feed.push",
            "params": {
                "event": {
                    "session_id": "s1",
                    "hook_event_name": "PermissionRequest",
                    "_source": "claude",
                    "_opencode_request_id": "req-feed-timeout",
                },
                "wait_timeout_seconds": 0.01,
            },
        })
        .to_string();
        let timed_out = dispatch_request(&request, &|command| {
            panic!("feed.push should not queue command: {command:?}")
        });
        assert_eq!(timed_out.error, None);
        assert_eq!(
            timed_out.result.expect("feed timeout result")["status"],
            "timed_out"
        );

        let invalid = dispatch_request(
            r#"{"id":2,"method":"feed.permission.reply","params":{"request_id":"req-feed-timeout","mode":"maybe"}}"#,
            &|command| panic!("invalid feed reply should not queue command: {command:?}"),
        );
        assert_eq!(
            invalid.error.as_ref().map(|error| error.code),
            Some(INVALID_PARAMS_CODE)
        );

        let missing = dispatch_request(
            r#"{"id":3,"method":"feed.question.reply","params":{"request_id":"missing","selections":[]}}"#,
            &|command| panic!("missing feed reply should not queue command: {command:?}"),
        );
        assert_eq!(
            missing.error.as_ref().map(|error| error.code),
            Some(NOT_FOUND_CODE)
        );
    }

    #[test]
    fn workspace_group_list_route_queues_group_command() {
        let response = dispatch_request(
            r#"{"id":1,"method":"workspace.group.list","params":{}}"#,
            &|command| match command {
                ControlCommand::ListWorkspaceGroups { reply } => {
                    let _ = reply.send(Ok(json!({ "groups": [] })));
                }
                other => panic!("unexpected command: {other:?}"),
            },
        );

        assert_eq!(response.error, None);
        assert_eq!(
            response.result.expect("workspace.group.list result")["groups"],
            json!([])
        );
    }

    #[test]
    fn workspace_group_mutation_routes_parse_cmux_params() {
        let response = dispatch_request(
            r#"{"id":1,"method":"workspace.group.rename","params":{"group_id":"workspace_group:1","name":"Agents"}}"#,
            &|command| match command {
                ControlCommand::WorkspaceGroupAction { action, reply } => {
                    assert_eq!(
                        action,
                        WorkspaceGroupAction::Rename {
                            group_id: "workspace_group:1".to_string(),
                            name: "Agents".to_string(),
                        }
                    );
                    let _ = reply.send(Ok(json!({ "ok": true })));
                }
                other => panic!("unexpected command: {other:?}"),
            },
        );

        assert_eq!(response.error, None);

        let response = dispatch_request(
            r#"{"id":2,"method":"workspace.group.add","params":{"group":"group-1","workspace":"workspace:abc"}}"#,
            &|command| match command {
                ControlCommand::WorkspaceGroupAction { action, reply } => {
                    assert_eq!(
                        action,
                        WorkspaceGroupAction::Add {
                            group_id: "group-1".to_string(),
                            workspace_id: "workspace:abc".to_string(),
                        }
                    );
                    let _ = reply.send(Ok(json!({ "ok": true })));
                }
                other => panic!("unexpected command: {other:?}"),
            },
        );

        assert_eq!(response.error, None);
    }

    #[test]
    fn workspace_group_create_parses_from_workspace_list() {
        let response = dispatch_request(
            r#"{"id":1,"method":"workspace.group.create","params":{"name":"Agents","from":["workspace:1","workspace:2"],"cwd":"/tmp"}}"#,
            &|command| match command {
                ControlCommand::WorkspaceGroupAction { action, reply } => {
                    assert_eq!(
                        action,
                        WorkspaceGroupAction::Create {
                            name: Some("Agents".to_string()),
                            cwd: Some("/tmp".to_string()),
                            from_workspace_ids: vec![
                                "workspace:1".to_string(),
                                "workspace:2".to_string(),
                            ],
                        }
                    );
                    let _ = reply.send(Ok(json!({ "ok": true })));
                }
                other => panic!("unexpected command: {other:?}"),
            },
        );

        assert_eq!(response.error, None);
    }

    #[test]
    fn event_stream_takeover_writes_jsonl_ack_and_replay() {
        let category = "notification.test.event_stream_takeover";
        crate::event_bus::bus().publish(crate::event_bus::EventPublish {
            name: "notification.created",
            category,
            source: "test",
            workspace_id: Some(Value::String("workspace-a".to_string())),
            surface_id: None,
            pane_id: None,
            payload: json!({ "notification_id": 1 }),
        });
        let (mut writer, reader) = UnixStream::pair().expect("socket pair");
        let handle = thread::spawn(move || {
            try_handle_event_stream(
                r#"{"id":"events-1","method":"events.stream","params":{"category":"notification.test.event_stream_takeover","include_heartbeats":false}}"#,
                &mut writer,
            )
        });

        let mut reader = BufReader::new(reader);
        let mut ack = String::new();
        reader.read_line(&mut ack).expect("read ack");
        let frame: Value = serde_json::from_str(ack.trim()).expect("ack json");
        assert_eq!(frame["type"], "ack");
        assert_eq!(frame["replay_count"], 1);
        assert_eq!(
            frame["filters"]["categories"],
            json!(["notification.test.event_stream_takeover"])
        );

        let mut event = String::new();
        reader.read_line(&mut event).expect("read event");
        let frame: Value = serde_json::from_str(event.trim()).expect("event json");
        assert_eq!(frame["type"], "event");
        assert_eq!(frame["name"], "notification.created");

        drop(reader);
        crate::event_bus::bus().publish(crate::event_bus::EventPublish {
            name: "notification.created",
            category,
            source: "test",
            workspace_id: Some(Value::String("workspace-b".to_string())),
            surface_id: None,
            pane_id: None,
            payload: json!({ "notification_id": 2 }),
        });
        let _ = handle.join().expect("event stream thread");
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
    fn workspace_env_and_create_routes_validate_environment() {
        let created = dispatch_request(
            r#"{"id":1,"method":"workspace.create","params":{"workspace_env":{"FOO":"bar"}}}"#,
            &|command| match command {
                ControlCommand::CreateWorkspace {
                    environment, reply, ..
                } => {
                    assert_eq!(environment.get("FOO").map(String::as_str), Some("bar"));
                    let _ = reply.send(Ok(json!({ "workspace_id": "workspace-a" })));
                }
                other => panic!("unexpected command: {other:?}"),
            },
        );
        assert_eq!(created.error, None);

        let env = dispatch_request(
            r#"{"id":2,"method":"workspace.env","params":{"workspace_id":"workspace-a","mask":true}}"#,
            &|command| match command {
                ControlCommand::WorkspaceEnv {
                    target,
                    mask,
                    reply,
                } => {
                    assert_eq!(target, WorkspaceTarget::Name("workspace-a".to_string()));
                    assert!(mask);
                    let _ = reply.send(Ok(json!({ "environment": { "FOO": "********" } })));
                }
                other => panic!("unexpected command: {other:?}"),
            },
        );
        assert_eq!(env.error, None);

        let invalid = dispatch_request(
            r#"{"id":3,"method":"workspace.create","params":{"workspace_env":{"CMUX_SOCKET":"/tmp/socket"}}}"#,
            &|command| panic!("invalid workspace.create should not dispatch: {command:?}"),
        );
        assert_eq!(
            invalid.error.as_ref().map(|error| error.code),
            Some(INVALID_PARAMS_CODE)
        );
    }

    #[test]
    fn workspace_navigation_aliases_route_to_live_commands() {
        for (method, expected) in [
            ("next-window", WorkspaceNavigation::Next),
            ("previous-window", WorkspaceNavigation::Previous),
            ("last-window", WorkspaceNavigation::Last),
        ] {
            let response = dispatch_request(
                format!(r#"{{"id":1,"method":"{}","params":{{}}}}"#, method).as_str(),
                &|command| match command {
                    ControlCommand::NavigateWorkspace { action, reply } => {
                        assert_eq!(action, expected);
                        let _ = reply.send(Ok(json!({ "ok": true })));
                    }
                    other => panic!("unexpected command: {other:?}"),
                },
            );
            assert_eq!(response.error, None);
            assert_eq!(
                response.result.expect("workspace navigation result")["ok"],
                true
            );
        }
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

        let eval = dispatch_request(
            r#"{"id":1,"method":"browser.eval","params":{"surface_id":"surface:9:browser","script":"document.title"}}"#,
            &|command| match command {
                ControlCommand::BrowserAction {
                    surface_hint,
                    action,
                    reply,
                    ..
                } => {
                    assert_eq!(surface_hint, "9:browser");
                    assert_eq!(
                        action,
                        BrowserAction::Eval {
                            script: "document.title".to_string()
                        }
                    );
                    let _ = reply.send(Ok(json!({ "value": "Example" })));
                }
                other => panic!("unexpected command: {other:?}"),
            },
        );
        assert_eq!(eval.error, None);

        let focused = dispatch_request(
            r#"{"id":1,"method":"browser.is_webview_focused","params":{"surface_id":"surface:9:browser"}}"#,
            &|command| match command {
                ControlCommand::BrowserAction { action, reply, .. } => {
                    assert_eq!(action, BrowserAction::IsFocused);
                    let _ = reply.send(Ok(json!({ "focused": true })));
                }
                other => panic!("unexpected command: {other:?}"),
            },
        );
        assert_eq!(focused.error, None);

        let title = dispatch_request(
            r#"{"id":1,"method":"browser.get.title","params":{"surface_id":"surface:9:browser"}}"#,
            &|command| match command {
                ControlCommand::BrowserAction { action, reply, .. } => {
                    assert_eq!(action, BrowserAction::GetTitle);
                    let _ = reply.send(Ok(json!({ "title": "Example" })));
                }
                other => panic!("unexpected command: {other:?}"),
            },
        );
        assert_eq!(title.error, None);

        let text = dispatch_request(
            r##"{"id":1,"method":"browser.get.text","params":{"surface_id":"surface:9:browser","selector":"#copy"}}"##,
            &|command| match command {
                ControlCommand::BrowserAction { action, reply, .. } => {
                    assert_eq!(
                        action,
                        BrowserAction::GetText {
                            selector: Some("#copy".to_string())
                        }
                    );
                    let _ = reply.send(Ok(json!({ "text": "Copy" })));
                }
                other => panic!("unexpected command: {other:?}"),
            },
        );
        assert_eq!(text.error, None);

        let value = dispatch_request(
            r##"{"id":1,"method":"browser.get.value","params":{"surface_id":"surface:9:browser","selector":"#email"}}"##,
            &|command| match command {
                ControlCommand::BrowserAction { action, reply, .. } => {
                    assert_eq!(
                        action,
                        BrowserAction::GetValue {
                            selector: "#email".to_string()
                        }
                    );
                    let _ = reply.send(Ok(json!({ "value": "a@example.com" })));
                }
                other => panic!("unexpected command: {other:?}"),
            },
        );
        assert_eq!(value.error, None);

        let attr = dispatch_request(
            r##"{"id":1,"method":"browser.get.attr","params":{"surface_id":"surface:9:browser","selector":"#email","attr":"aria-label"}}"##,
            &|command| match command {
                ControlCommand::BrowserAction { action, reply, .. } => {
                    assert_eq!(
                        action,
                        BrowserAction::GetAttr {
                            selector: "#email".to_string(),
                            name: "aria-label".to_string()
                        }
                    );
                    let _ = reply.send(Ok(json!({ "value": "Email" })));
                }
                other => panic!("unexpected command: {other:?}"),
            },
        );
        assert_eq!(attr.error, None);

        let count = dispatch_request(
            r##"{"id":1,"method":"browser.get.count","params":{"surface_id":"surface:9:browser","selector":".item"}}"##,
            &|command| match command {
                ControlCommand::BrowserAction { action, reply, .. } => {
                    assert_eq!(
                        action,
                        BrowserAction::GetCount {
                            selector: ".item".to_string()
                        }
                    );
                    let _ = reply.send(Ok(json!({ "count": 3 })));
                }
                other => panic!("unexpected command: {other:?}"),
            },
        );
        assert_eq!(count.error, None);

        let box_result = dispatch_request(
            r##"{"id":1,"method":"browser.get.box","params":{"surface_id":"surface:9:browser","selector":"main"}}"##,
            &|command| match command {
                ControlCommand::BrowserAction { action, reply, .. } => {
                    assert_eq!(
                        action,
                        BrowserAction::GetBox {
                            selector: "main".to_string()
                        }
                    );
                    let _ = reply.send(Ok(json!({ "value": { "x": 1, "y": 2 } })));
                }
                other => panic!("unexpected command: {other:?}"),
            },
        );
        assert_eq!(box_result.error, None);

        let html = dispatch_request(
            r##"{"id":1,"method":"browser.get.html","params":{"surface_id":"surface:9:browser","selector":"main"}}"##,
            &|command| match command {
                ControlCommand::BrowserAction { action, reply, .. } => {
                    assert_eq!(
                        action,
                        BrowserAction::GetHtml {
                            selector: Some("main".to_string())
                        }
                    );
                    let _ = reply.send(Ok(json!({ "html": "<main></main>" })));
                }
                other => panic!("unexpected command: {other:?}"),
            },
        );
        assert_eq!(html.error, None);

        let styles = dispatch_request(
            r##"{"id":1,"method":"browser.get.styles","params":{"surface_id":"surface:9:browser","selector":"main","property":"display"}}"##,
            &|command| match command {
                ControlCommand::BrowserAction { action, reply, .. } => {
                    assert_eq!(
                        action,
                        BrowserAction::GetStyles {
                            selector: "main".to_string(),
                            property: Some("display".to_string())
                        }
                    );
                    let _ = reply.send(Ok(json!({ "value": "block" })));
                }
                other => panic!("unexpected command: {other:?}"),
            },
        );
        assert_eq!(styles.error, None);

        let visible = dispatch_request(
            r##"{"id":1,"method":"browser.is.visible","params":{"surface_id":"surface:9:browser","selector":"main"}}"##,
            &|command| match command {
                ControlCommand::BrowserAction { action, reply, .. } => {
                    assert_eq!(
                        action,
                        BrowserAction::IsVisible {
                            selector: "main".to_string()
                        }
                    );
                    let _ = reply.send(Ok(json!({ "visible": true, "value": true })));
                }
                other => panic!("unexpected command: {other:?}"),
            },
        );
        assert_eq!(visible.error, None);

        let enabled = dispatch_request(
            r##"{"id":1,"method":"browser.is.enabled","params":{"surface_id":"surface:9:browser","selector":"button"}}"##,
            &|command| match command {
                ControlCommand::BrowserAction { action, reply, .. } => {
                    assert_eq!(
                        action,
                        BrowserAction::IsEnabled {
                            selector: "button".to_string()
                        }
                    );
                    let _ = reply.send(Ok(json!({ "enabled": true, "value": true })));
                }
                other => panic!("unexpected command: {other:?}"),
            },
        );
        assert_eq!(enabled.error, None);

        let checked = dispatch_request(
            r##"{"id":1,"method":"browser.is.checked","params":{"surface_id":"surface:9:browser","selector":"input"}}"##,
            &|command| match command {
                ControlCommand::BrowserAction { action, reply, .. } => {
                    assert_eq!(
                        action,
                        BrowserAction::IsChecked {
                            selector: "input".to_string()
                        }
                    );
                    let _ = reply.send(Ok(json!({ "checked": false, "value": false })));
                }
                other => panic!("unexpected command: {other:?}"),
            },
        );
        assert_eq!(checked.error, None);

        let click = dispatch_request(
            r##"{"id":1,"method":"browser.click","params":{"surface_id":"surface:9:browser","selector":"#submit"}}"##,
            &|command| match command {
                ControlCommand::BrowserAction { action, reply, .. } => {
                    assert_eq!(
                        action,
                        BrowserAction::Click {
                            selector: "#submit".to_string()
                        }
                    );
                    let _ = reply.send(Ok(json!({ "action": { "ok": true } })));
                }
                other => panic!("unexpected command: {other:?}"),
            },
        );
        assert_eq!(click.error, None);

        let fill = dispatch_request(
            r##"{"id":1,"method":"browser.fill","params":{"surface_id":"surface:9:browser","selector":"#email","text":"a@example.com"}}"##,
            &|command| match command {
                ControlCommand::BrowserAction { action, reply, .. } => {
                    assert_eq!(
                        action,
                        BrowserAction::Fill {
                            selector: "#email".to_string(),
                            text: "a@example.com".to_string()
                        }
                    );
                    let _ = reply.send(Ok(json!({ "action": { "ok": true } })));
                }
                other => panic!("unexpected command: {other:?}"),
            },
        );
        assert_eq!(fill.error, None);

        let select = dispatch_request(
            r##"{"id":1,"method":"browser.select","params":{"surface_id":"surface:9:browser","selector":"select","value":"pro"}}"##,
            &|command| match command {
                ControlCommand::BrowserAction { action, reply, .. } => {
                    assert_eq!(
                        action,
                        BrowserAction::Select {
                            selector: "select".to_string(),
                            value: "pro".to_string()
                        }
                    );
                    let _ = reply.send(Ok(json!({ "action": { "ok": true } })));
                }
                other => panic!("unexpected command: {other:?}"),
            },
        );
        assert_eq!(select.error, None);

        let keydown = dispatch_request(
            r#"{"id":1,"method":"browser.keydown","params":{"surface_id":"surface:9:browser","key":"Enter"}}"#,
            &|command| match command {
                ControlCommand::BrowserAction { action, reply, .. } => {
                    assert_eq!(
                        action,
                        BrowserAction::KeyDown {
                            key: "Enter".to_string()
                        }
                    );
                    let _ = reply.send(Ok(json!({ "action": { "ok": true } })));
                }
                other => panic!("unexpected command: {other:?}"),
            },
        );
        assert_eq!(keydown.error, None);

        let scroll = dispatch_request(
            r#"{"id":1,"method":"browser.scroll","params":{"surface_id":"surface:9:browser","dx":10,"dy":-20}}"#,
            &|command| match command {
                ControlCommand::BrowserAction { action, reply, .. } => {
                    assert_eq!(
                        action,
                        BrowserAction::Scroll {
                            selector: None,
                            dx: 10,
                            dy: -20
                        }
                    );
                    let _ = reply.send(Ok(json!({ "action": { "ok": true } })));
                }
                other => panic!("unexpected command: {other:?}"),
            },
        );
        assert_eq!(scroll.error, None);

        let snapshot = dispatch_request(
            r#"{"id":1,"method":"browser.snapshot","params":{"surface_id":"surface:9:browser","interactive":true,"compact":true,"max_depth":3}}"#,
            &|command| match command {
                ControlCommand::BrowserAction {
                    surface_hint,
                    action,
                    reply,
                    ..
                } => {
                    assert_eq!(surface_hint, "9:browser");
                    assert_eq!(
                        action,
                        BrowserAction::Snapshot {
                            interactive: true,
                            compact: true,
                            max_depth: Some(3)
                        }
                    );
                    let _ = reply.send(Ok(json!({ "snapshot": { "text": "body" } })));
                }
                other => panic!("unexpected command: {other:?}"),
            },
        );
        assert_eq!(snapshot.error, None);

        let screenshot = dispatch_request(
            r#"{"id":1,"method":"browser.screenshot","params":{"surface_id":"surface:9:browser","path":"/tmp/shot.png","fullPage":true}}"#,
            &|command| match command {
                ControlCommand::BrowserAction {
                    surface_hint,
                    action,
                    reply,
                    ..
                } => {
                    assert_eq!(surface_hint, "9:browser");
                    assert_eq!(
                        action,
                        BrowserAction::Screenshot {
                            path: Some("/tmp/shot.png".to_string()),
                            full_page: true
                        }
                    );
                    let _ = reply.send(Ok(json!({ "path": "/tmp/shot.png" })));
                }
                other => panic!("unexpected command: {other:?}"),
            },
        );
        assert_eq!(screenshot.error, None);

        let wait = dispatch_request(
            r##"{
                "id":1,
                "method":"browser.wait",
                "params":{
                    "surface_id":"surface:9:browser",
                    "selector":"#ready",
                    "text":"Ready",
                    "url_contains":"example",
                    "load_state":"complete",
                    "function":"() => true",
                    "timeout_ms":250
                }
            }"##,
            &|command| match command {
                ControlCommand::BrowserAction {
                    surface_hint,
                    action,
                    reply,
                    ..
                } => {
                    assert_eq!(surface_hint, "9:browser");
                    assert_eq!(
                        action,
                        BrowserAction::Wait {
                            selector: Some("#ready".to_string()),
                            text: Some("Ready".to_string()),
                            url_contains: Some("example".to_string()),
                            load_state: Some("complete".to_string()),
                            function: Some("() => true".to_string()),
                            timeout_ms: 250,
                        }
                    );
                    let _ = reply.send(Ok(json!({ "matched": true })));
                }
                other => panic!("unexpected command: {other:?}"),
            },
        );
        assert_eq!(wait.error, None);

        let frame_select = dispatch_request(
            r##"{"id":1,"method":"browser.frame.select","params":{"surface_id":"surface:9:browser","selector":"iframe.docs"}}"##,
            &|command| match command {
                ControlCommand::BrowserAction { action, reply, .. } => {
                    assert_eq!(
                        action,
                        BrowserAction::FrameSelect {
                            selector: "iframe.docs".to_string()
                        }
                    );
                    let _ = reply.send(Ok(json!({ "frame_id": "iframe.docs" })));
                }
                other => panic!("unexpected command: {other:?}"),
            },
        );
        assert_eq!(frame_select.error, None);

        let frame_main = dispatch_request(
            r#"{"id":1,"method":"browser.frame.main","params":{"surface_id":"surface:9:browser"}}"#,
            &|command| match command {
                ControlCommand::BrowserAction { action, reply, .. } => {
                    assert_eq!(action, BrowserAction::FrameMain);
                    let _ = reply.send(Ok(json!({ "frame_id": "main" })));
                }
                other => panic!("unexpected command: {other:?}"),
            },
        );
        assert_eq!(frame_main.error, None);

        let dialog_accept = dispatch_request(
            r#"{"id":1,"method":"browser.dialog.accept","params":{"surface_id":"surface:9:browser","text":"yes"}}"#,
            &|command| match command {
                ControlCommand::BrowserAction { action, reply, .. } => {
                    assert_eq!(
                        action,
                        BrowserAction::DialogAccept {
                            text: Some("yes".to_string())
                        }
                    );
                    let _ = reply.send(Ok(json!({ "accepted": true })));
                }
                other => panic!("unexpected command: {other:?}"),
            },
        );
        assert_eq!(dialog_accept.error, None);

        let dialog_dismiss = dispatch_request(
            r#"{"id":1,"method":"browser.dialog.dismiss","params":{"surface_id":"surface:9:browser"}}"#,
            &|command| match command {
                ControlCommand::BrowserAction { action, reply, .. } => {
                    assert_eq!(action, BrowserAction::DialogDismiss);
                    let _ = reply.send(Ok(json!({ "accepted": false })));
                }
                other => panic!("unexpected command: {other:?}"),
            },
        );
        assert_eq!(dialog_dismiss.error, None);

        let download_wait = dispatch_request(
            r#"{"id":1,"method":"browser.download.wait","params":{"surface_id":"surface:9:browser","path":"/tmp/file.bin","timeout_ms":25}}"#,
            &|command| match command {
                ControlCommand::BrowserAction { action, reply, .. } => {
                    assert_eq!(
                        action,
                        BrowserAction::DownloadWait {
                            path: Some("/tmp/file.bin".to_string()),
                            timeout_ms: 25
                        }
                    );
                    let _ = reply.send(Ok(json!({ "downloaded": true, "path": "/tmp/file.bin" })));
                }
                other => panic!("unexpected command: {other:?}"),
            },
        );
        assert_eq!(download_wait.error, None);

        let invalid_wait = dispatch_request(
            r#"{"id":1,"method":"browser.wait","params":{"surface_id":"surface:9:browser"}}"#,
            &|command| panic!("invalid browser.wait should not dispatch: {command:?}"),
        );
        assert_eq!(
            invalid_wait.error.as_ref().map(|error| error.code),
            Some(INVALID_PARAMS_CODE)
        );

        let invalid_getter = dispatch_request(
            r#"{"id":1,"method":"browser.get.value","params":{"surface_id":"surface:9:browser"}}"#,
            &|command| panic!("invalid browser.get.value should not dispatch: {command:?}"),
        );
        assert_eq!(
            invalid_getter.error.as_ref().map(|error| error.code),
            Some(INVALID_PARAMS_CODE)
        );

        let invalid_attr = dispatch_request(
            r##"{"id":1,"method":"browser.get.attr","params":{"surface_id":"surface:9:browser","selector":"#email"}}"##,
            &|command| panic!("invalid browser.get.attr should not dispatch: {command:?}"),
        );
        assert_eq!(
            invalid_attr.error.as_ref().map(|error| error.code),
            Some(INVALID_PARAMS_CODE)
        );

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
    fn browser_unsupported_wkwebview_gap_methods_fail_before_dispatch() {
        for method in [
            "browser.viewport.set",
            "browser.geolocation.set",
            "browser.offline.set",
            "browser.trace.start",
            "browser.trace.stop",
            "browser.network.route",
            "browser.network.unroute",
            "browser.network.requests",
            "browser.screencast.start",
            "browser.screencast.stop",
            "browser.input_mouse",
            "browser.input_keyboard",
            "browser.input_touch",
        ] {
            let response = dispatch_request(
                &json!({
                    "id": 1,
                    "method": method,
                    "params": { "surface_id": "surface:9:browser" }
                })
                .to_string(),
                &|command| panic!("unsupported browser method should not dispatch: {command:?}"),
            );

            let error = response.error.expect("unsupported error");
            assert_eq!(error.code, NOT_SUPPORTED_CODE);
            assert!(error.message.contains("not_supported"));
            assert!(error.message.contains(method));
        }
    }

    // purpose: Assert a browser RPC request queues the expected live bridge action.
    // inputs: Raw JSON-RPC request and expected BrowserAction payload.
    // returns/effects: Panics when parsing dispatches the wrong command or returns an error.
    fn assert_browser_action_route(request: &str, expected_action: BrowserAction) {
        let response = dispatch_request(request, &|command| match command {
            ControlCommand::BrowserAction {
                surface_hint,
                action,
                reply,
                ..
            } => {
                assert_eq!(surface_hint, "9:browser");
                assert_eq!(action, expected_action);
                let _ = reply.send(Ok(json!({ "ok": true })));
            }
            other => panic!("unexpected command: {other:?}"),
        });

        assert_eq!(response.error, None);
    }

    // purpose: Verify CMUX browser script and style injection methods reach the live bridge.
    // inputs: JSON-RPC requests for addscript, addinitscript, and addstyle.
    // returns/effects: Panics when any request fails validation or dispatches the wrong action.
    #[test]
    fn browser_injection_routes_queue_browser_actions() {
        let cases = [
            (
                r#"{"id":1,"method":"browser.addscript","params":{"surface_id":"surface:9:browser","script":"1 + 2"}}"#,
                BrowserAction::AddScript {
                    script: "1 + 2".to_string(),
                },
            ),
            (
                r#"{"id":1,"method":"browser.addinitscript","params":{"surface_id":"surface:9:browser","script":"window.ready = true"}}"#,
                BrowserAction::AddInitScript {
                    script: "window.ready = true".to_string(),
                },
            ),
            (
                r#"{"id":1,"method":"browser.addstyle","params":{"surface_id":"surface:9:browser","css":"body { color: red; }"}}"#,
                BrowserAction::AddStyle {
                    css: "body { color: red; }".to_string(),
                },
            ),
        ];

        for (request, expected_action) in cases {
            assert_browser_action_route(request, expected_action);
        }
    }

    // purpose: Verify CMUX browser console and error diagnostic methods reach the live bridge.
    // inputs: JSON-RPC requests for console and error list/clear methods.
    // returns/effects: Panics when any request fails validation or dispatches the wrong action.
    #[test]
    fn browser_diagnostics_routes_queue_browser_actions() {
        let cases = [
            (
                r#"{"id":1,"method":"browser.console.list","params":{"surface_id":"surface:9:browser"}}"#,
                BrowserAction::ConsoleList,
            ),
            (
                r#"{"id":1,"method":"browser.console.clear","params":{"surface_id":"surface:9:browser"}}"#,
                BrowserAction::ConsoleClear,
            ),
            (
                r#"{"id":1,"method":"browser.errors.list","params":{"surface_id":"surface:9:browser"}}"#,
                BrowserAction::ErrorsList,
            ),
            (
                r#"{"id":1,"method":"browser.errors.clear","params":{"surface_id":"surface:9:browser"}}"#,
                BrowserAction::ErrorsClear,
            ),
        ];

        for (request, expected_action) in cases {
            assert_browser_action_route(request, expected_action);
        }
    }

    #[test]
    // purpose: Verify CMUX browser highlight and cookie methods reach the live bridge.
    // inputs: JSON-RPC requests for highlight and cookie get/set/clear actions.
    // returns/effects: Panics when any request fails validation or dispatches the wrong action.
    fn browser_highlight_and_cookie_routes_queue_browser_actions() {
        let cases = [
            (
                r##"{"id":1,"method":"browser.highlight","params":{"surface_id":"surface:9:browser","selector":"#submit"}}"##,
                BrowserAction::Highlight {
                    selector: "#submit".to_string(),
                },
            ),
            (
                r#"{"id":1,"method":"browser.cookies.get","params":{"surface_id":"surface:9:browser","name":"sid"}}"#,
                BrowserAction::CookiesGet {
                    name: Some("sid".to_string()),
                },
            ),
            (
                r#"{"id":1,"method":"browser.cookies.set","params":{"surface_id":"surface:9:browser","name":"sid","value":"abc"}}"#,
                BrowserAction::CookiesSet {
                    name: "sid".to_string(),
                    value: "abc".to_string(),
                },
            ),
            (
                r#"{"id":1,"method":"browser.cookies.clear","params":{"surface_id":"surface:9:browser"}}"#,
                BrowserAction::CookiesClear { name: None },
            ),
        ];

        for (request, expected_action) in cases {
            assert_browser_action_route(request, expected_action);
        }
    }

    #[test]
    // purpose: Verify CMUX browser Web Storage methods reach the live bridge.
    // inputs: JSON-RPC requests for local/session storage get/set/clear actions.
    // returns/effects: Panics when any request fails validation or dispatches the wrong action.
    fn browser_storage_routes_queue_browser_actions() {
        let cases = [
            (
                r#"{"id":1,"method":"browser.storage.get","params":{"surface_id":"surface:9:browser","type":"session","key":"mode"}}"#,
                BrowserAction::StorageGet {
                    storage_type: "session".to_string(),
                    key: "mode".to_string(),
                },
            ),
            (
                r#"{"id":1,"method":"browser.storage.set","params":{"surface_id":"surface:9:browser","key":"mode","value":"dark"}}"#,
                BrowserAction::StorageSet {
                    storage_type: "local".to_string(),
                    key: "mode".to_string(),
                    value: "dark".to_string(),
                },
            ),
            (
                r#"{"id":1,"method":"browser.storage.clear","params":{"surface_id":"surface:9:browser","type":"local","key":"mode"}}"#,
                BrowserAction::StorageClear {
                    storage_type: "local".to_string(),
                    key: Some("mode".to_string()),
                },
            ),
        ];

        for (request, expected_action) in cases {
            assert_browser_action_route(request, expected_action);
        }
    }

    // purpose: Verify CMUX browser state save/load methods reach the live bridge.
    // inputs: JSON-RPC requests for state save and load with explicit paths.
    // returns/effects: Panics when either request fails validation or dispatches the wrong action.
    #[test]
    fn browser_state_routes_queue_browser_actions() {
        let cases = [
            (
                r#"{"id":1,"method":"browser.state.save","params":{"surface_id":"surface:9:browser","path":"/tmp/state.json"}}"#,
                BrowserAction::StateSave {
                    path: "/tmp/state.json".to_string(),
                },
            ),
            (
                r#"{"id":1,"method":"browser.state.load","params":{"surface_id":"surface:9:browser","path":"/tmp/state.json"}}"#,
                BrowserAction::StateLoad {
                    path: "/tmp/state.json".to_string(),
                },
            ),
        ];

        for (request, expected_action) in cases {
            assert_browser_action_route(request, expected_action);
        }
    }

    // purpose: Verify CMUX browser tab methods reach the live bridge with validated targets.
    // inputs: JSON-RPC requests for tab list, new, switch, and close.
    // returns/effects: Panics when any request fails validation or dispatches the wrong command.
    #[test]
    fn browser_tab_routes_queue_browser_tab_actions() {
        let cases = [
            (
                r#"{"id":1,"method":"browser.tab.list","params":{"surface_id":"surface:9:browser"}}"#,
                BrowserTabAction::List,
            ),
            (
                r#"{"id":1,"method":"browser.tab.new","params":{"surface_id":"surface:9:browser","url":"https://example.com"}}"#,
                BrowserTabAction::New {
                    url: Some("https://example.com".to_string()),
                },
            ),
            (
                r#"{"id":1,"method":"browser.tab.switch","params":{"surface_id":"surface:9:browser","target_surface_id":"surface:9:other"}}"#,
                BrowserTabAction::Switch {
                    target_surface_hint: "9:other".to_string(),
                },
            ),
            (
                r#"{"id":1,"method":"browser.tab.close","params":{"surface_id":"surface:9:browser","tab_id":"other"}}"#,
                BrowserTabAction::Close {
                    target_surface_hint: Some("other".to_string()),
                },
            ),
        ];

        for (request, expected_action) in cases {
            let response = dispatch_request(request, &|command| match command {
                ControlCommand::BrowserTabAction {
                    surface_hint,
                    action,
                    reply,
                    ..
                } => {
                    assert_eq!(surface_hint, "9:browser");
                    assert_eq!(action, expected_action);
                    let _ = reply.send(Ok(json!({ "ok": true })));
                }
                other => panic!("unexpected command: {other:?}"),
            });

            assert_eq!(response.error, None);
        }
    }

    // purpose: Verify tab switching fails before dispatch when no target tab is supplied.
    // inputs: Malformed browser.tab.switch request missing target_surface_id/tab_id.
    // returns/effects: Panics when invalid params are not reported.
    #[test]
    fn browser_tab_switch_rejects_missing_target() {
        let response = dispatch_request(
            r#"{"id":1,"method":"browser.tab.switch","params":{"surface_id":"surface:9:browser"}}"#,
            &|command| panic!("invalid browser.tab.switch should not dispatch: {command:?}"),
        );

        assert_eq!(
            response.error.as_ref().map(|error| error.code),
            Some(INVALID_PARAMS_CODE)
        );
    }

    #[test]
    // purpose: Verify CMUX semantic browser locator methods reach the live bridge.
    // inputs: JSON-RPC requests for role and text finders.
    // returns/effects: Panics when any request fails validation or dispatches the wrong action.
    fn browser_find_semantic_routes_queue_browser_actions() {
        let cases = [
            (
                r#"{"id":1,"method":"browser.find.role","params":{"surface_id":"surface:9:browser","role":"button","name":"Submit"}}"#,
                BrowserAction::Find {
                    locator: "role".to_string(),
                    selector: None,
                    query: None,
                    role: Some("button".to_string()),
                    name: Some("Submit".to_string()),
                    index: None,
                },
            ),
            (
                r#"{"id":1,"method":"browser.find.text","params":{"surface_id":"surface:9:browser","text":"Done"}}"#,
                BrowserAction::Find {
                    locator: "text".to_string(),
                    selector: None,
                    query: Some("Done".to_string()),
                    role: None,
                    name: None,
                    index: None,
                },
            ),
        ];

        for (request, expected_action) in cases {
            assert_browser_action_route(request, expected_action);
        }
    }

    #[test]
    // purpose: Verify CMUX positional browser locator methods reach the live bridge.
    // inputs: JSON-RPC requests for first and nth finders.
    // returns/effects: Panics when any request fails validation or dispatches the wrong action.
    fn browser_find_positional_routes_queue_browser_actions() {
        let cases = [
            (
                r##"{"id":1,"method":"browser.find.first","params":{"surface_id":"surface:9:browser","selector":".row"}}"##,
                BrowserAction::Find {
                    locator: "first".to_string(),
                    selector: Some(".row".to_string()),
                    query: None,
                    role: None,
                    name: None,
                    index: None,
                },
            ),
            (
                r##"{"id":1,"method":"browser.find.nth","params":{"surface_id":"surface:9:browser","selector":".row","index":2}}"##,
                BrowserAction::Find {
                    locator: "nth".to_string(),
                    selector: Some(".row".to_string()),
                    query: None,
                    role: None,
                    name: None,
                    index: Some(2),
                },
            ),
        ];

        for (request, expected_action) in cases {
            assert_browser_action_route(request, expected_action);
        }
    }

    // purpose: Verify injection routes reject requests missing required script or CSS payloads.
    // inputs: Malformed addscript and addstyle JSON-RPC requests.
    // returns/effects: Panics when invalid requests dispatch instead of returning invalid_params.
    #[test]
    fn browser_injection_routes_reject_missing_payloads() {
        let missing_script = dispatch_request(
            r#"{"id":1,"method":"browser.addscript","params":{"surface_id":"surface:9:browser"}}"#,
            &|command| panic!("invalid addscript should not dispatch: {command:?}"),
        );
        assert_eq!(
            missing_script.error.as_ref().map(|error| error.code),
            Some(INVALID_PARAMS_CODE)
        );

        let missing_css = dispatch_request(
            r#"{"id":1,"method":"browser.addstyle","params":{"surface_id":"surface:9:browser"}}"#,
            &|command| panic!("invalid addstyle should not dispatch: {command:?}"),
        );
        assert_eq!(
            missing_css.error.as_ref().map(|error| error.code),
            Some(INVALID_PARAMS_CODE)
        );
    }

    // purpose: Verify stateful page routes reject missing or invalid parameters before dispatch.
    // inputs: Malformed highlight, cookie, and storage JSON-RPC requests.
    // returns/effects: Panics when invalid requests dispatch instead of returning invalid_params.
    #[test]
    fn browser_stateful_page_routes_reject_invalid_params() {
        for request in [
            r#"{"id":1,"method":"browser.highlight","params":{"surface_id":"surface:9:browser"}}"#,
            r#"{"id":1,"method":"browser.cookies.set","params":{"surface_id":"surface:9:browser","name":"sid"}}"#,
            r#"{"id":1,"method":"browser.storage.get","params":{"surface_id":"surface:9:browser"}}"#,
            r#"{"id":1,"method":"browser.storage.set","params":{"surface_id":"surface:9:browser","key":"mode"}}"#,
            r#"{"id":1,"method":"browser.storage.clear","params":{"surface_id":"surface:9:browser","type":"global"}}"#,
            r#"{"id":1,"method":"browser.state.save","params":{"surface_id":"surface:9:browser"}}"#,
            r#"{"id":1,"method":"browser.state.load","params":{"surface_id":"surface:9:browser","path":""}}"#,
            r#"{"id":1,"method":"browser.is.visible","params":{"surface_id":"surface:9:browser"}}"#,
        ] {
            let response = dispatch_request(request, &|command| {
                panic!("invalid stateful browser action should not dispatch: {command:?}")
            });
            assert_eq!(
                response.error.as_ref().map(|error| error.code),
                Some(INVALID_PARAMS_CODE)
            );
        }
    }

    // purpose: Verify browser locator methods reject missing required locator fields.
    // inputs: Malformed role, text, first, and nth finder requests.
    // returns/effects: Panics when invalid requests dispatch instead of returning invalid_params.
    #[test]
    fn browser_find_routes_reject_invalid_params() {
        for request in [
            r#"{"id":1,"method":"browser.find.role","params":{"surface_id":"surface:9:browser"}}"#,
            r#"{"id":1,"method":"browser.find.text","params":{"surface_id":"surface:9:browser"}}"#,
            r#"{"id":1,"method":"browser.find.first","params":{"surface_id":"surface:9:browser"}}"#,
            r##"{"id":1,"method":"browser.find.nth","params":{"surface_id":"surface:9:browser","selector":".row"}}"##,
        ] {
            let response = dispatch_request(request, &|command| {
                panic!("invalid browser finder should not dispatch: {command:?}")
            });
            assert_eq!(
                response.error.as_ref().map(|error| error.code),
                Some(INVALID_PARAMS_CODE)
            );
        }
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
    fn pane_last_route_accepts_cmux_alias() {
        let response = dispatch_request(
            r#"{"id":1,"method":"last-pane","params":{"workspace_id":"codex"}}"#,
            &|command| match command {
                ControlCommand::LastPane { target, reply } => {
                    assert_eq!(target, WorkspaceTarget::Name("codex".to_string()));
                    let _ = reply.send(Ok(json!({ "pane_ref": "pane:10" })));
                }
                other => panic!("unexpected command: {other:?}"),
            },
        );

        assert_eq!(response.error, None);
        assert_eq!(
            response.result.expect("pane.last result")["pane_ref"],
            "pane:10"
        );
    }

    #[test]
    fn pane_resize_route_accepts_cmux_alias_and_pane_refs() {
        let response = dispatch_request(
            concat!(
                r#"{"id":1,"method":"resize-pane","params":{"workspace_id":"codex","#,
                r#""pane_id":"pane:11","direction":"left","amount":3}}"#
            ),
            &|command| match command {
                ControlCommand::ResizePane {
                    target,
                    pane_id,
                    direction,
                    amount,
                    reply,
                } => {
                    assert_eq!(target, WorkspaceTarget::Name("codex".to_string()));
                    assert_eq!(pane_id, "11");
                    assert_eq!(direction, "left");
                    assert_eq!(amount, 3);
                    let _ = reply.send(Ok(json!({ "pane_ref": "pane:11", "ratio": 0.44 })));
                }
                other => panic!("unexpected command: {other:?}"),
            },
        );

        assert_eq!(response.error, None);
        assert_eq!(
            response.result.expect("pane.resize result")["pane_ref"],
            "pane:11"
        );
    }

    #[test]
    fn pane_resize_route_rejects_missing_pane_and_invalid_direction() {
        let missing = dispatch_request(
            r#"{"id":1,"method":"pane.resize","params":{"direction":"right"}}"#,
            &|command| panic!("invalid pane.resize should not dispatch: {command:?}"),
        );
        assert_eq!(
            missing.error.as_ref().map(|error| error.code),
            Some(INVALID_PARAMS_CODE)
        );

        let invalid_direction = dispatch_request(
            r#"{"id":1,"method":"pane.resize","params":{"pane_id":"pane:11","direction":"wide"}}"#,
            &|command| panic!("invalid pane.resize should not dispatch: {command:?}"),
        );
        assert_eq!(
            invalid_direction.error.as_ref().map(|error| error.code),
            Some(INVALID_PARAMS_CODE)
        );
    }

    #[test]
    fn pane_swap_route_accepts_cmux_alias_and_refs() {
        let response = dispatch_request(
            concat!(
                r#"{"id":1,"method":"swap-pane","params":{"workspace_id":"codex","#,
                r#""pane_id":"pane:11","target_pane_id":"pane:12"}}"#
            ),
            &|command| match command {
                ControlCommand::SwapPane {
                    target,
                    pane_id,
                    target_pane_id,
                    reply,
                } => {
                    assert_eq!(target, WorkspaceTarget::Name("codex".to_string()));
                    assert_eq!(pane_id, "11");
                    assert_eq!(target_pane_id, "12");
                    let _ = reply.send(Ok(json!({ "ok": true })));
                }
                other => panic!("unexpected command: {other:?}"),
            },
        );

        assert_eq!(response.error, None);
        assert_eq!(response.result.expect("pane.swap result")["ok"], true);
    }

    #[test]
    fn pane_swap_route_rejects_missing_targets() {
        let response = dispatch_request(
            r#"{"id":1,"method":"pane.swap","params":{"pane_id":"pane:11"}}"#,
            &|command| panic!("invalid pane.swap should not dispatch: {command:?}"),
        );
        assert_eq!(
            response.error.as_ref().map(|error| error.code),
            Some(INVALID_PARAMS_CODE)
        );
    }

    #[test]
    fn pane_join_route_accepts_cmux_alias_and_refs() {
        let response = dispatch_request(
            concat!(
                r#"{"id":1,"method":"join-pane","params":{"workspace_id":"codex","#,
                r#""pane_id":"pane:11","target_pane_id":"pane:12","surface_id":"surface:11:tab"}}"#
            ),
            &|command| match command {
                ControlCommand::JoinPane {
                    target,
                    source_pane_id,
                    source_surface_id,
                    target_pane_id,
                    reply,
                } => {
                    assert_eq!(target, WorkspaceTarget::Name("codex".to_string()));
                    assert_eq!(source_pane_id, Some("11".to_string()));
                    assert_eq!(source_surface_id, Some("11:tab".to_string()));
                    assert_eq!(target_pane_id, "12");
                    let _ = reply.send(Ok(json!({ "pane_ref": "pane:12" })));
                }
                other => panic!("unexpected command: {other:?}"),
            },
        );

        assert_eq!(response.error, None);
        assert_eq!(
            response.result.expect("pane.join result")["pane_ref"],
            "pane:12"
        );
    }

    #[test]
    fn pane_join_route_rejects_missing_target() {
        let response = dispatch_request(
            r#"{"id":1,"method":"pane.join","params":{"pane_id":"pane:11"}}"#,
            &|command| panic!("invalid pane.join should not dispatch: {command:?}"),
        );
        assert_eq!(
            response.error.as_ref().map(|error| error.code),
            Some(INVALID_PARAMS_CODE)
        );
    }

    #[test]
    fn pane_break_route_accepts_cmux_alias_and_refs() {
        let response = dispatch_request(
            concat!(
                r#"{"id":1,"method":"break-pane","params":{"workspace_id":"codex","#,
                r#""pane_id":"pane:11","surface_id":"surface:11:tab"}}"#
            ),
            &|command| match command {
                ControlCommand::BreakPane {
                    target,
                    pane_id,
                    surface_hint,
                    reply,
                } => {
                    assert_eq!(target, WorkspaceTarget::Name("codex".to_string()));
                    assert_eq!(pane_id, Some("11".to_string()));
                    assert_eq!(surface_hint, Some("11:tab".to_string()));
                    let _ = reply.send(Ok(json!({ "workspace_ref": "workspace:new" })));
                }
                other => panic!("unexpected command: {other:?}"),
            },
        );

        assert_eq!(response.error, None);
        assert_eq!(
            response.result.expect("pane.break result")["workspace_ref"],
            "workspace:new"
        );
    }

    #[test]
    fn surface_split_route_queues_terminal_pane_create() {
        let response = dispatch_request(
            concat!(
                r#"{"id":1,"method":"new-split","params":{"workspace_id":"codex","#,
                r#""surface_id":"surface:4:tab","direction":"down","command":"top"}}"#
            ),
            &|command| match command {
                ControlCommand::CreatePane { request, reply } => {
                    assert_eq!(request.target, WorkspaceTarget::Name("codex".to_string()));
                    assert_eq!(request.source_surface_id, Some("4:tab".to_string()));
                    assert_eq!(request.source_pane_id, None);
                    assert_eq!(request.direction, PaneCreateDirection::Down);
                    assert_eq!(request.pane_type, PaneCreateType::Terminal);
                    assert_eq!(request.command, Some("top".to_string()));
                    assert_eq!(request.url, None);
                    let _ = reply.send(Ok(json!({
                        "pane_ref": "pane:12",
                        "surface_ref": "surface:12:tab"
                    })));
                }
                other => panic!("unexpected command: {other:?}"),
            },
        );

        assert_eq!(response.error, None);
        assert_eq!(
            response.result.expect("surface.split result")["surface_ref"],
            "surface:12:tab"
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
    fn surface_close_route_accepts_surface_refs_and_context_defaults() {
        let explicit = dispatch_request(
            r#"{"id":1,"method":"close-surface","params":{"workspace_id":"codex","surface_id":"surface:4:tab"}}"#,
            &|command| match command {
                ControlCommand::CloseSurface {
                    target,
                    surface_hint,
                    reply,
                } => {
                    assert_eq!(target, WorkspaceTarget::Name("codex".to_string()));
                    assert_eq!(surface_hint, Some("4:tab".to_string()));
                    let _ = reply.send(Ok(json!({ "surface_ref": "surface:4:tab" })));
                }
                other => panic!("unexpected command: {other:?}"),
            },
        );
        assert_eq!(explicit.error, None);
        assert_eq!(
            explicit.result.expect("surface.close result")["surface_ref"],
            "surface:4:tab"
        );

        let contextual = dispatch_request(
            r#"{"id":1,"method":"surface.close","params":{"workspace_id":"codex"}}"#,
            &|command| match command {
                ControlCommand::CloseSurface {
                    target,
                    surface_hint,
                    reply,
                } => {
                    assert_eq!(target, WorkspaceTarget::Name("codex".to_string()));
                    assert_eq!(surface_hint, None);
                    let _ = reply.send(Ok(json!({ "closed": true })));
                }
                other => panic!("unexpected command: {other:?}"),
            },
        );
        assert_eq!(contextual.error, None);
    }

    #[test]
    fn surface_move_route_accepts_target_pane_and_index() {
        let response = dispatch_request(
            r#"{"id":1,"method":"move-surface","params":{"workspace_id":"codex","surface_id":"surface:4:tab","target_pane_id":"pane:9","index":2}}"#,
            &|command| match command {
                ControlCommand::MoveSurface {
                    target,
                    surface_hint,
                    target_pane_id,
                    index,
                    reply,
                } => {
                    assert_eq!(target, WorkspaceTarget::Name("codex".to_string()));
                    assert_eq!(surface_hint, "4:tab");
                    assert_eq!(target_pane_id, "9");
                    assert_eq!(index, Some(2));
                    let _ = reply.send(Ok(json!({ "surface_ref": "surface:9:tab" })));
                }
                other => panic!("unexpected command: {other:?}"),
            },
        );

        assert_eq!(response.error, None);
        assert_eq!(
            response.result.expect("surface.move result")["surface_ref"],
            "surface:9:tab"
        );

        let invalid = dispatch_request(
            r#"{"id":1,"method":"surface.move","params":{"surface_id":"surface:4:tab"}}"#,
            &|command| panic!("invalid surface.move should not dispatch: {command:?}"),
        );
        assert_eq!(
            invalid.error.as_ref().map(|error| error.code),
            Some(INVALID_PARAMS_CODE)
        );
    }

    #[test]
    fn surface_reorder_route_accepts_index_and_relative_targets() {
        let response = dispatch_request(
            concat!(
                r#"{"id":1,"method":"reorder-surface","params":{"workspace_id":"codex","#,
                r#""surface_id":"surface:4:tab","index":2}}"#
            ),
            &|command| match command {
                ControlCommand::ReorderSurface {
                    target,
                    surface_hint,
                    index,
                    before_surface_hint,
                    after_surface_hint,
                    reply,
                } => {
                    assert_eq!(target, WorkspaceTarget::Name("codex".to_string()));
                    assert_eq!(surface_hint, "4:tab");
                    assert_eq!(index, Some(2));
                    assert_eq!(before_surface_hint, None);
                    assert_eq!(after_surface_hint, None);
                    let _ = reply.send(Ok(json!({ "surface_ref": "surface:4:tab" })));
                }
                other => panic!("unexpected command: {other:?}"),
            },
        );

        assert_eq!(response.error, None);
        assert_eq!(
            response.result.expect("surface.reorder result")["surface_ref"],
            "surface:4:tab"
        );

        let before = dispatch_request(
            concat!(
                r#"{"id":1,"method":"surface.reorder","params":{"surface_id":"surface:4:tab","#,
                r#""before_surface_id":"surface:4:other"}}"#
            ),
            &|command| match command {
                ControlCommand::ReorderSurface {
                    before_surface_hint,
                    after_surface_hint,
                    reply,
                    ..
                } => {
                    assert_eq!(before_surface_hint, Some("4:other".to_string()));
                    assert_eq!(after_surface_hint, None);
                    let _ = reply.send(Ok(json!({ "surface_ref": "surface:4:tab" })));
                }
                other => panic!("unexpected command: {other:?}"),
            },
        );
        assert_eq!(before.error, None);

        let after = dispatch_request(
            concat!(
                r#"{"id":1,"method":"surface.reorder","params":{"surface_id":"surface:4:tab","#,
                r#""after_surface_id":"surface:4:other"}}"#
            ),
            &|command| match command {
                ControlCommand::ReorderSurface {
                    before_surface_hint,
                    after_surface_hint,
                    reply,
                    ..
                } => {
                    assert_eq!(before_surface_hint, None);
                    assert_eq!(after_surface_hint, Some("4:other".to_string()));
                    let _ = reply.send(Ok(json!({ "surface_ref": "surface:4:tab" })));
                }
                other => panic!("unexpected command: {other:?}"),
            },
        );
        assert_eq!(after.error, None);
    }

    #[test]
    fn surface_reorder_route_rejects_missing_or_multiple_targets() {
        let missing = dispatch_request(
            r#"{"id":1,"method":"surface.reorder","params":{"surface_id":"surface:4:tab"}}"#,
            &|command| panic!("invalid surface.reorder should not dispatch: {command:?}"),
        );
        assert_eq!(
            missing.error.as_ref().map(|error| error.code),
            Some(INVALID_PARAMS_CODE)
        );

        let multiple = dispatch_request(
            concat!(
                r#"{"id":1,"method":"surface.reorder","params":{"surface_id":"surface:4:tab","#,
                r#""index":1,"after_surface_id":"surface:4:other"}}"#
            ),
            &|command| panic!("invalid surface.reorder should not dispatch: {command:?}"),
        );
        assert_eq!(
            multiple.error.as_ref().map(|error| error.code),
            Some(INVALID_PARAMS_CODE)
        );
    }

    #[test]
    fn surface_refresh_route_accepts_surface_refs_and_context_defaults() {
        let explicit = dispatch_request(
            concat!(
                r#"{"id":1,"method":"refresh-surfaces","params":{"workspace_id":"codex","#,
                r#""panel_id":"surface:4:tab"}}"#
            ),
            &|command| match command {
                ControlCommand::RefreshSurfaces {
                    target,
                    surface_hint,
                    reply,
                } => {
                    assert_eq!(target, WorkspaceTarget::Name("codex".to_string()));
                    assert_eq!(surface_hint, Some("4:tab".to_string()));
                    let _ = reply.send(Ok(json!({ "refreshed": 1 })));
                }
                other => panic!("unexpected command: {other:?}"),
            },
        );
        assert_eq!(explicit.error, None);
        assert_eq!(
            explicit.result.expect("surface.refresh result")["refreshed"],
            1
        );

        let all = dispatch_request(
            r#"{"id":1,"method":"surface.refresh","params":{"workspace_id":"codex"}}"#,
            &|command| match command {
                ControlCommand::RefreshSurfaces {
                    target,
                    surface_hint,
                    reply,
                } => {
                    assert_eq!(target, WorkspaceTarget::Name("codex".to_string()));
                    assert_eq!(surface_hint, None);
                    let _ = reply.send(Ok(json!({ "refreshed": 2 })));
                }
                other => panic!("unexpected command: {other:?}"),
            },
        );
        assert_eq!(all.error, None);
        assert_eq!(all.result.expect("surface.refresh result")["refreshed"], 2);
    }

    #[test]
    fn surface_clear_history_route_accepts_surface_refs_and_context_defaults() {
        let explicit = dispatch_request(
            concat!(
                r#"{"id":1,"method":"clear-history","params":{"workspace_id":"codex","#,
                r#""panel_id":"surface:4:tab"}}"#
            ),
            &|command| match command {
                ControlCommand::ClearSurfaceHistory {
                    target,
                    surface_hint,
                    reply,
                } => {
                    assert_eq!(target, WorkspaceTarget::Name("codex".to_string()));
                    assert_eq!(surface_hint, Some("4:tab".to_string()));
                    let _ = reply.send(Ok(json!({ "cleared": true })));
                }
                other => panic!("unexpected command: {other:?}"),
            },
        );
        assert_eq!(explicit.error, None);
        assert_eq!(
            explicit.result.expect("surface.clear_history result")["cleared"],
            true
        );

        let active = dispatch_request(
            r#"{"id":1,"method":"surface.clear_history","params":{"workspace_id":"codex"}}"#,
            &|command| match command {
                ControlCommand::ClearSurfaceHistory {
                    target,
                    surface_hint,
                    reply,
                } => {
                    assert_eq!(target, WorkspaceTarget::Name("codex".to_string()));
                    assert_eq!(surface_hint, None);
                    let _ = reply.send(Ok(json!({ "cleared": true })));
                }
                other => panic!("unexpected command: {other:?}"),
            },
        );
        assert_eq!(active.error, None);
        assert_eq!(
            active.result.expect("surface.clear_history result")["cleared"],
            true
        );
    }

    #[test]
    fn surface_respawn_route_requires_command_and_preserves_metadata() {
        let response = dispatch_request(
            concat!(
                r#"{"id":1,"method":"respawn-pane","params":{"workspace_id":"codex","#,
                r#""surface_id":"surface:4:tab","command":"echo ready","#,
                r#""tmux_start_command":"echo ready"}}"#
            ),
            &|command| match command {
                ControlCommand::RespawnSurface {
                    target,
                    surface_hint,
                    command,
                    tmux_start_command,
                    reply,
                } => {
                    assert_eq!(target, WorkspaceTarget::Name("codex".to_string()));
                    assert_eq!(surface_hint, Some("4:tab".to_string()));
                    assert_eq!(command, "echo ready");
                    assert_eq!(tmux_start_command.as_deref(), Some("echo ready"));
                    let _ = reply.send(Ok(json!({ "respawned": true })));
                }
                other => panic!("unexpected command: {other:?}"),
            },
        );

        assert_eq!(response.error, None);
        assert_eq!(
            response.result.expect("surface.respawn result")["respawned"],
            true
        );

        let missing = dispatch_request(
            r#"{"id":1,"method":"surface.respawn","params":{"surface_id":"surface:4:tab"}}"#,
            &|command| panic!("unexpected command: {command:?}"),
        );
        assert_eq!(
            missing.error.expect("missing command error").code,
            INVALID_PARAMS_CODE
        );
    }

    #[test]
    fn surface_drag_to_split_route_accepts_cmux_aliases() {
        let explicit = dispatch_request(
            concat!(
                r#"{"id":1,"method":"drag-surface-to-split","params":{"workspace_id":"codex","#,
                r#""surface_id":"surface:4:tab","direction":"left"}}"#
            ),
            &|command| match command {
                ControlCommand::DragSurfaceToSplit {
                    target,
                    surface_hint,
                    direction,
                    reply,
                } => {
                    assert_eq!(target, WorkspaceTarget::Name("codex".to_string()));
                    assert_eq!(surface_hint, "4:tab");
                    assert_eq!(direction, PaneCreateDirection::Left);
                    let _ = reply.send(Ok(json!({ "surface_ref": "surface:12:tab" })));
                }
                other => panic!("unexpected command: {other:?}"),
            },
        );
        assert_eq!(explicit.error, None);
        assert_eq!(
            explicit.result.expect("surface.drag_to_split result")["surface_ref"],
            "surface:12:tab"
        );

        let split_off = dispatch_request(
            r#"{"id":1,"method":"split-off","params":{"surface_id":"surface:4:tab"}}"#,
            &|command| match command {
                ControlCommand::DragSurfaceToSplit {
                    surface_hint,
                    direction,
                    reply,
                    ..
                } => {
                    assert_eq!(surface_hint, "4:tab");
                    assert_eq!(direction, PaneCreateDirection::Right);
                    let _ = reply.send(Ok(json!({ "surface_ref": "surface:12:tab" })));
                }
                other => panic!("unexpected command: {other:?}"),
            },
        );
        assert_eq!(split_off.error, None);
    }

    #[test]
    fn notification_list_and_clear_routes_validate_params() {
        let created = dispatch_request(
            r#"{"id":1,"method":"notification.create","params":{"title":"Done","surface_id":"2:tab-a"}}"#,
            &|command| match command {
                ControlCommand::CreateNotification {
                    surface_hint,
                    title,
                    reply,
                    ..
                } => {
                    assert_eq!(surface_hint.as_deref(), Some("2:tab-a"));
                    assert_eq!(title, "Done");
                    let _ = reply.send(Ok(json!({ "notification_id": 1 })));
                }
                other => panic!("unexpected command: {other:?}"),
            },
        );
        assert_eq!(created.error, None);

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
            r#"{
                "id":1,
                "method":"workspace.create_many",
                "params":{
                    "count":12,
                    "name_prefix":"triple",
                    "cwd":"/tmp",
                    "panes_per_workspace":4,
                    "terminals_per_workspace":10
                }
            }"#,
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
                    lines,
                    scrollback,
                    reply,
                } => {
                    assert_eq!(target, WorkspaceTarget::Active);
                    assert_eq!(surface_hint, Some("9:tab".to_string()));
                    assert_eq!(lines, None);
                    assert!(!scrollback);
                    let _ = reply.send(Ok(json!({ "text": "ready" })));
                }
                other => panic!("unexpected command: {other:?}"),
            },
        );

        assert_eq!(response.error, None);
        assert_eq!(response.result.expect("result")["text"], "ready");
    }

    #[test]
    fn read_text_route_accepts_lines_and_scrollback_flags() {
        let response = dispatch_request(
            r#"{"id":1,"method":"read-screen","params":{"surface_id":"surface:9:tab","lines":5,"scrollback":true}}"#,
            &|command| match command {
                ControlCommand::ReadSurfaceText {
                    target,
                    surface_hint,
                    lines,
                    scrollback,
                    reply,
                } => {
                    assert_eq!(target, WorkspaceTarget::Active);
                    assert_eq!(surface_hint, Some("9:tab".to_string()));
                    assert_eq!(lines, Some(5));
                    assert!(scrollback);
                    let _ = reply.send(Ok(json!({ "text": "ready" })));
                }
                other => panic!("unexpected command: {other:?}"),
            },
        );

        assert_eq!(response.error, None);
        assert_eq!(response.result.expect("result")["text"], "ready");
    }
}
