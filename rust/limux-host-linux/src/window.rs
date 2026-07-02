// summary: Build and coordinate the GTK application window for Limux.
// purpose: Manage workspaces, panes, shortcuts, persistence, styling, and control dispatch.
// inputs: GTK application activation, user session files, settings, and control socket commands.
// returns/effects: Presents the main window and persists workspace/session changes.

use std::cell::{Cell, RefCell};
use std::collections::{BTreeMap, HashMap};
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::rc::Rc;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use adw::prelude::*;
use gtk::gdk::prelude::ToplevelExt;
use gtk::gio;
use gtk::glib;
use gtk::glib::variant::ToVariant;
use gtk4 as gtk;
use libadwaita as adw;
use serde_json::json;

use crate::app_config;
use crate::control_bridge::{
    BridgeError, BrowserAction, BrowserTabAction, ControlCommand,
    PaneCreateDirection as BridgePaneCreateDirection, PaneCreateType, RightSidebarAction,
    RightSidebarMode, RightSidebarTarget, SidebarAction, WorkspaceGroupAction, WorkspaceNavigation,
    WorkspaceTarget,
};
use crate::keybind_editor;
use crate::layout_state::{
    self, AppSessionState, LayoutNodeState, LoadedSession, PaneState, SplitOrientation, SplitState,
    TabState, WorkspaceGroupState, WorkspaceState,
};
use crate::pane::{self, PaneCallbacks};
use crate::shortcut_config::{
    self, EditableCapturePolicy, ResolvedShortcutConfig, ShortcutCommand, ShortcutId,
};
use crate::split_tree::{self, SplitTreeContainer};

const PANE_CREATE_COMMAND_READY_INTERVAL_MS: u64 = 50;
const PANE_CREATE_COMMAND_READY_ATTEMPTS: u32 = 40;
const BROWSER_WAIT_POLL_INTERVAL_MS: u64 = 50;
const MAX_HOST_NOTIFICATIONS: usize = 200;
const MAX_SIDEBAR_LOG_ENTRIES: usize = 500;

// ---------------------------------------------------------------------------
// State
// ---------------------------------------------------------------------------

struct Workspace {
    id: String,
    name: String,
    description: Option<String>,
    /// The root widget in the content stack for this workspace.
    root: gtk::Widget,
    /// Manages the split tree data model and async widget rebuild.
    split_container: Rc<SplitTreeContainer>,
    /// The sidebar row widget.
    sidebar_row: gtk::ListBoxRow,
    /// Name label in sidebar row.
    name_label: gtk::Label,
    /// Favorite star button in sidebar row.
    favorite_button: gtk::Button,
    /// Notification dot in the sidebar row.
    notify_dot: gtk::Label,
    /// Notification message label in the sidebar row.
    notify_label: gtk::Label,
    /// Workspace description label shown in sidebar details.
    description_label: gtk::Label,
    /// Whether this workspace has unread notifications.
    unread: bool,
    /// Whether this workspace is favorited/pinned to top.
    favorite: bool,
    /// Previous pane focused through the live control bridge, for tmux `last-pane`.
    last_pane_id: Option<u32>,
    /// CMUX-compatible workspace-group id when this workspace belongs to a group.
    group_id: Option<String>,
    /// User-defined workspace environment inherited by new terminal surfaces.
    environment: BTreeMap<String, String>,
    /// Last known working directory from the terminal (via OSC 7).
    cwd: Rc<RefCell<Option<String>>>,
    /// The folder path this workspace was opened with.
    folder_path: Option<String>,
    /// Path label shown below workspace name in sidebar.
    #[allow(dead_code)]
    path_label: gtk::Label,
    /// CMUX-compatible sidebar status entries keyed by agent/tool id.
    sidebar_status: BTreeMap<String, SidebarStatusEntry>,
    /// CMUX-compatible workspace progress metadata.
    sidebar_progress: Option<SidebarProgress>,
    /// Bounded CMUX-compatible sidebar log stream for this workspace.
    sidebar_log: Vec<SidebarLogEntry>,
}

#[derive(Clone, Debug, PartialEq)]
struct SidebarStatusEntry {
    key: String,
    value: String,
    icon: Option<String>,
    color: Option<String>,
    url: Option<String>,
    priority: i64,
}

#[derive(Clone, Debug, PartialEq)]
struct SidebarProgress {
    value: f64,
    label: Option<String>,
}

#[derive(Clone, Debug, PartialEq)]
struct SidebarLogEntry {
    id: u64,
    created_at: String,
    level: String,
    source: Option<String>,
    message: String,
}

struct WorkspaceGroupFolderTarget {
    group_id: String,
    reference_workspace_id: String,
    placement: app_config::WorkspaceGroupNewPlacement,
}

struct WorkspaceFolderTarget {
    group: Option<WorkspaceGroupFolderTarget>,
    reference_workspace_id: Option<String>,
    placement: app_config::WorkspaceGroupNewPlacement,
}

#[derive(Clone)]
struct HostNotification {
    id: u64,
    workspace_id: String,
    surface_id: Option<String>,
    pane_id: Option<u32>,
    tab_title: Option<String>,
    created_at: String,
    title: String,
    subtitle: String,
    body: String,
    message: String,
    unread: bool,
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct WorkspaceEventSnapshot {
    workspace_id: String,
    workspace_ref: String,
    title: String,
    description: Option<String>,
    index: usize,
    selected: bool,
    favorite: bool,
    group_id: Option<String>,
    tab_count: usize,
}

pub(crate) struct AppState {
    app: adw::Application,
    window: adw::ApplicationWindow,
    top_bar: Option<adw::HeaderBar>,
    top_bar_visible: bool,
    config: Rc<RefCell<app_config::AppConfig>>,
    system_prefers_dark: Rc<Cell<Option<bool>>>,
    workspaces: Vec<Workspace>,
    workspace_groups: Vec<WorkspaceGroupState>,
    active_idx: usize,
    previous_workspace_id: Option<String>,
    shortcuts: Rc<ResolvedShortcutConfig>,
    stack: gtk::Stack,
    sidebar_list: gtk::ListBox,
    sidebar_shell: gtk::Box,
    sidebar_handle: gtk::Box,
    right_sidebar_shell: gtk::Box,
    right_sidebar_title_label: gtk::Label,
    right_sidebar_body: gtk::Box,
    new_ws_btn: gtk::Button,
    sidebar_animation: Option<adw::TimedAnimation>,
    sidebar_animation_epoch: u64,
    sidebar_expanded_width: i32,
    right_sidebar_visible: bool,
    right_sidebar_mode: RightSidebarMode,
    right_sidebar_focused: bool,
    persistence_suspended: bool,
    save_queued: bool,
    workspace_dragging: Option<String>,
    next_notification_id: u64,
    next_sidebar_log_id: u64,
    notifications: Vec<HostNotification>,
    desktop_notification_routes: HashMap<u32, DesktopNotificationRoute>,
    _theme_portal_signal: Option<gio::SignalSubscription>,
    _theme_gnome_settings: Option<gio::Settings>,
    _theme_gnome_signal: Option<glib::SignalHandlerId>,
    _desktop_notification_token_signal: Option<gio::SignalSubscription>,
    _desktop_notification_action_signal: Option<gio::SignalSubscription>,
    _desktop_notification_closed_signal: Option<gio::SignalSubscription>,
}

impl AppState {
    fn active_workspace(&self) -> Option<&Workspace> {
        self.workspaces.get(self.active_idx)
    }

    fn workspace_for_widget(&self, widget: &gtk::Widget) -> Option<&Workspace> {
        self.workspaces
            .iter()
            .find(|workspace| widget.is_ancestor(&workspace.root))
    }
}

fn workspace_ref(id: &str) -> String {
    format!("workspace:{id}")
}

fn pane_ref(id: u32) -> String {
    format!("pane:{id}")
}

fn surface_ref(id: &str) -> String {
    format!("surface:{id}")
}

fn pane_create_response_payload(
    workspace_id: &str,
    workspace_name: &str,
    surface: pane::SurfaceSummary,
) -> serde_json::Value {
    let surface_id = surface.surface_id;
    serde_json::json!({
        "workspace_id": workspace_id,
        "workspace_ref": workspace_ref(workspace_id),
        "workspace": {
            "id": workspace_id,
            "ref": workspace_ref(workspace_id),
            "workspace_id": workspace_id,
            "workspace_ref": workspace_ref(workspace_id),
            "title": workspace_name,
            "name": workspace_name,
        },
        "title": workspace_name,
        "name": workspace_name,
        "pane_id": surface.pane_id.to_string(),
        "pane_ref": pane_ref(surface.pane_id),
        "surface_id": surface_id.clone(),
        "surface_ref": surface_ref(&surface_id),
        "surface_title": surface.title,
        "surface_type": surface.kind,
        "ok": true,
    })
}

fn browser_action_response_payload(
    workspace_id: &str,
    workspace_name: &str,
    browser: &pane::BrowserSurfaceTarget,
) -> serde_json::Value {
    let surface_id = browser.surface.surface_id.clone();
    serde_json::json!({
        "ok": true,
        "workspace_id": workspace_id,
        "workspace_ref": workspace_ref(workspace_id),
        "workspace_name": workspace_name,
        "surface_id": surface_id,
        "surface_ref": surface_ref(&browser.surface.surface_id),
        "pane_id": browser.surface.pane_id.to_string(),
        "pane_ref": pane_ref(browser.surface.pane_id),
    })
}

#[derive(Clone, Debug)]
struct BrowserEvent {
    name: &'static str,
    workspace_id: String,
    surface_id: String,
    pane_id: u32,
    payload: serde_json::Value,
}

// purpose: Build a CMUX browser event payload from a live browser surface.
// inputs: Workspace id, browser target, command name, and non-sensitive metadata.
// returns/effects: Returns JSON without publishing or exposing typed values/scripts.
fn browser_event_payload(
    workspace_id: &str,
    browser: &pane::BrowserSurfaceTarget,
    command: &'static str,
    extra: serde_json::Value,
) -> serde_json::Value {
    let mut payload = serde_json::json!({
        "workspace_id": workspace_id,
        "workspace_ref": workspace_ref(workspace_id),
        "surface_id": browser.surface.surface_id,
        "surface_ref": surface_ref(&browser.surface.surface_id),
        "pane_id": browser.surface.pane_id.to_string(),
        "pane_ref": pane_ref(browser.surface.pane_id),
        "surface_title": browser.surface.title,
        "surface_type": browser.surface.kind,
        "command": command,
    });
    if let Some(uri) = browser.surface.uri.as_ref().filter(|uri| !uri.is_empty()) {
        payload["url"] = serde_json::Value::String(uri.clone());
        payload["uri"] = serde_json::Value::String(uri.clone());
    }
    if let (Some(payload), Some(extra)) = (payload.as_object_mut(), extra.as_object()) {
        for (key, value) in extra {
            payload.insert(key.clone(), value.clone());
        }
    }
    payload
}

// purpose: Prepare a retained CMUX browser event for later publication.
// inputs: Event name, workspace id, browser target, command, and safe metadata.
// returns/effects: Returns an event object that can be emitted after success.
fn browser_event(
    name: &'static str,
    workspace_id: &str,
    browser: &pane::BrowserSurfaceTarget,
    command: &'static str,
    extra: serde_json::Value,
) -> BrowserEvent {
    BrowserEvent {
        name,
        workspace_id: workspace_id.to_string(),
        surface_id: browser.surface.surface_id.clone(),
        pane_id: browser.surface.pane_id,
        payload: browser_event_payload(workspace_id, browser, command, extra),
    }
}

// purpose: Publish a CMUX browser action event after a command succeeds.
// inputs: Prepared browser event with redacted payload metadata.
// returns/effects: Appends a retained browser event to the host event bus.
fn publish_browser_event(event: BrowserEvent) -> u64 {
    crate::event_bus::bus().publish(crate::event_bus::EventPublish {
        name: event.name,
        category: "browser",
        source: "browser.action",
        workspace_id: Some(serde_json::Value::String(event.workspace_id)),
        surface_id: Some(serde_json::Value::String(event.surface_id)),
        pane_id: Some(serde_json::Value::String(event.pane_id.to_string())),
        payload: event.payload,
    })
}

fn send_browser_eval_response(
    browser: pane::BrowserSurfaceTarget,
    script: String,
    mut payload: serde_json::Value,
    output_key: &'static str,
    reply: std::sync::mpsc::Sender<Result<serde_json::Value, BridgeError>>,
) {
    browser.evaluate_javascript(&script, move |result| match result {
        Ok(value) => {
            payload[output_key] = value.clone();
            if output_key != "value" {
                payload["value"] = value;
            }
            let _ = reply.send(Ok(payload));
        }
        Err(error) => {
            let _ = reply.send(Err(BridgeError::internal(format!(
                "browser JavaScript evaluation failed: {error}"
            ))));
        }
    });
}

// purpose: Evaluate JavaScript and publish a browser event only after success.
// inputs: Browser target, script, response payload, output key, optional event, and reply channel.
// returns/effects: Sends the socket response and emits no event when evaluation fails.
fn send_browser_eval_response_with_event(
    browser: pane::BrowserSurfaceTarget,
    script: String,
    mut payload: serde_json::Value,
    output_key: &'static str,
    event: BrowserEvent,
    reply: std::sync::mpsc::Sender<Result<serde_json::Value, BridgeError>>,
) {
    browser.evaluate_javascript(&script, move |result| match result {
        Ok(value) => {
            payload[output_key] = value.clone();
            if output_key != "value" {
                payload["value"] = value;
            }
            publish_browser_event(event);
            let _ = reply.send(Ok(payload));
        }
        Err(error) => {
            let _ = reply.send(Err(BridgeError::internal(format!(
                "browser JavaScript evaluation failed: {error}"
            ))));
        }
    });
}

// purpose: Evaluate JavaScript that returns an object and merge it into the bridge response.
// inputs: Browser target, JavaScript source, base payload, and socket reply channel.
// returns/effects: Sends merged object fields or a fatal JavaScript evaluation error.
fn send_browser_object_response(
    browser: pane::BrowserSurfaceTarget,
    script: String,
    mut payload: serde_json::Value,
    reply: std::sync::mpsc::Sender<Result<serde_json::Value, BridgeError>>,
) {
    browser.evaluate_javascript(&script, move |result| match result {
        Ok(value) => {
            let Some(object) = value.as_object() else {
                let _ = reply.send(Err(BridgeError::internal(
                    "browser JavaScript object response was not an object",
                )));
                return;
            };
            for (key, value) in object {
                payload[key] = value.clone();
            }
            let _ = reply.send(Ok(payload));
        }
        Err(error) => {
            let _ = reply.send(Err(BridgeError::internal(format!(
                "browser JavaScript evaluation failed: {error}"
            ))));
        }
    });
}

// purpose: Save evaluated browser state to disk and reply with the written payload.
// inputs: Browser target, destination path, base response payload, and socket reply channel.
// returns/effects: Writes JSON state to the requested path or sends a fatal bridge error.
fn send_browser_state_save_response(
    browser: pane::BrowserSurfaceTarget,
    path: String,
    mut payload: serde_json::Value,
    reply: std::sync::mpsc::Sender<Result<serde_json::Value, BridgeError>>,
) {
    browser.evaluate_javascript(browser_state_save_script(), move |result| match result {
        Ok(state_json) => {
            let encoded = match serde_json::to_vec_pretty(&state_json) {
                Ok(encoded) => encoded,
                Err(error) => {
                    let _ = reply.send(Err(BridgeError::internal(format!(
                        "browser state encoding failed: {error}"
                    ))));
                    return;
                }
            };
            if let Err(error) = std::fs::write(&path, encoded) {
                let _ = reply.send(Err(BridgeError::internal(format!(
                    "browser state write failed: {error}"
                ))));
                return;
            }
            payload["path"] = serde_json::Value::String(path);
            payload["state"] = state_json;
            let _ = reply.send(Ok(payload));
        }
        Err(error) => {
            let _ = reply.send(Err(BridgeError::internal(format!(
                "browser JavaScript evaluation failed: {error}"
            ))));
        }
    });
}

// purpose: Resolve the destination used by live browser screenshot capture.
// inputs: Optional user-supplied output path from browser.screenshot params.
// returns/effects: Returns the explicit path or a unique temp-file destination.
fn browser_screenshot_path(path: &Option<String>) -> PathBuf {
    if let Some(path) = path {
        return PathBuf::from(path);
    }
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("system time should be after UNIX_EPOCH")
        .as_nanos();
    std::env::temp_dir().join(format!(
        "limux-browser-shot-{}-{nanos}.png",
        std::process::id()
    ))
}

// purpose: Capture a WebKit browser surface to PNG and reply with CMUX-compatible metadata.
// inputs: Browser target, output path option, full-page flag, base payload, and reply channel.
// returns/effects: Writes the screenshot file or sends a fatal bridge error.
fn send_browser_screenshot_response(
    browser: pane::BrowserSurfaceTarget,
    path: Option<String>,
    full_page: bool,
    mut payload: serde_json::Value,
    reply: std::sync::mpsc::Sender<Result<serde_json::Value, BridgeError>>,
) {
    let path = browser_screenshot_path(&path);
    let unavailable_reply = reply.clone();
    if !browser.save_screenshot(path, full_page, move |result| match result {
        Ok(screenshot) => {
            payload["ok"] = serde_json::Value::Bool(true);
            payload["format"] = serde_json::Value::String("png".to_string());
            payload["path"] = serde_json::Value::String(screenshot.path.clone());
            payload["url"] = serde_json::Value::String(format!("file://{}", screenshot.path));
            payload["width"] = serde_json::json!(screenshot.width);
            payload["height"] = serde_json::json!(screenshot.height);
            payload["bytes"] = serde_json::json!(screenshot.bytes);
            let _ = reply.send(Ok(payload));
        }
        Err(error) => {
            let _ = reply.send(Err(BridgeError::internal(format!(
                "browser screenshot capture failed: {error}"
            ))));
        }
    }) {
        let _ = unavailable_reply.send(Err(BridgeError::internal(
            "browser screenshot capture is unavailable",
        )));
    }
}

const BROWSER_ELEMENT_REF_SCRIPT: &str = r#"
function limuxElementRefStore() {
  if (!(window.__limuxElementRefs instanceof Map)) {
    window.__limuxElementRefs = new Map();
  }
  if (!Number.isInteger(window.__limuxNextElementRef) || window.__limuxNextElementRef < 1) {
    window.__limuxNextElementRef = 1;
  }
  return window.__limuxElementRefs;
}
function limuxResetElementRefs() {
  window.__limuxElementRefs = new Map();
  window.__limuxNextElementRef = 1;
}
function limuxNormalizeElementRef(value) {
  if (typeof value !== 'string') return null;
  const raw = value.trim();
  const match = raw.match(/^@?e([0-9]+)$/);
  return match ? '@e' + match[1] : null;
}
function limuxStoreElementRef(node) {
  const store = limuxElementRefStore();
  const ref = '@e' + window.__limuxNextElementRef++;
  store.set(ref, node);
  return ref;
}
function limuxResolveElement(target) {
  const ref = limuxNormalizeElementRef(target);
  if (ref !== null) {
    const node = limuxElementRefStore().get(ref);
    if (!node || !node.isConnected) {
      throw new Error('stale element ref: ' + ref);
    }
    return { node, selector: ref, element_ref: ref };
  }
  const node = document.querySelector(target);
  if (!node) {
    throw new Error('selector not found: ' + target);
  }
  return { node, selector: target, element_ref: null };
}
"#;

// purpose: Build JavaScript that targets one required DOM element and fails loudly when it cannot be resolved.
// inputs: CSS selector or CMUX element ref plus JavaScript expression that receives `node`.
// returns/effects: Returns an immediately-invoked JavaScript snippet for WebKit evaluation.
fn browser_required_element_script(selector: &str, expression: &str) -> String {
    let selector = serde_json::to_string(selector).expect("json selector");
    format!(
        r#"(function() {{
{ref_script}
const target = {selector};
const resolved = limuxResolveElement(target);
const node = resolved.node;
const selector = resolved.selector;
const element_ref = resolved.element_ref;
return {expression};
}})()"#,
        ref_script = BROWSER_ELEMENT_REF_SCRIPT,
    )
}

// purpose: Build JavaScript for text/html getters that can address either the whole page or one selected element.
// inputs: Optional CSS selector and JavaScript expressions for page-wide and element-specific reads.
// returns/effects: Returns an immediately-invoked JavaScript snippet for WebKit evaluation.
fn browser_optional_element_script(
    selector: Option<&str>,
    page_expression: &str,
    element_expression: &str,
) -> String {
    match selector {
        Some(selector) => browser_required_element_script(selector, element_expression),
        None => format!("(function() {{ return {page_expression}; }})()"),
    }
}

// purpose: Build JavaScript that counts all DOM elements matching a required CSS selector.
// inputs: CSS selector or CMUX element ref.
// returns/effects: Returns a JavaScript snippet that fails on invalid selectors and returns a number.
fn browser_count_script(selector: &str) -> String {
    let selector = serde_json::to_string(selector).expect("json selector");
    format!(
        r#"(function() {{
{ref_script}
const target = {selector};
if (limuxNormalizeElementRef(target) !== null) {{
  limuxResolveElement(target);
  return 1;
}}
return document.querySelectorAll(target).length;
}})()"#,
        ref_script = BROWSER_ELEMENT_REF_SCRIPT,
    )
}

// purpose: Build JavaScript that injects a CSS style block into the active document.
// inputs: Raw CSS text supplied by the browser addstyle command.
// returns/effects: Returns an immediately-invoked script for WebKit evaluation.
fn browser_add_style_script(css: &str) -> String {
    let css = serde_json::to_string(css).expect("json css");
    format!(
        r#"(function() {{
const style = document.createElement('style');
style.setAttribute('data-limux-injected-style', 'true');
style.textContent = {css};
(document.head || document.documentElement).appendChild(style);
return {{ action: 'addstyle', ok: true }};
}})()"#,
    )
}

// purpose: Build JavaScript that visually marks a selected element without changing app state.
// inputs: CSS selector supplied to browser.highlight.
// returns/effects: Returns a script that outlines the element or fails when it is missing.
fn browser_highlight_script(selector: &str) -> String {
    browser_element_action_script(
        selector,
        r#"
node.setAttribute('data-limux-highlighted', 'true');
node.style.outline = '3px solid #ffcc00';
return { action: 'highlight', selector, ok: true };
"#,
    )
}

const BROWSER_FIND_SCRIPT_BODY: &str = r#"
function textOf(node) {
  return (node.innerText || node.textContent || node.value || '').replace(/\s+/g, ' ').trim();
}
function attr(node, name) {
  return node.getAttribute ? (node.getAttribute(name) || '') : '';
}
function inferredRole(node) {
  const tag = (node.tagName || '').toLowerCase();
  if (tag === 'button') return 'button';
  if (tag === 'a' && attr(node, 'href')) return 'link';
  if (['input', 'textarea', 'select'].includes(tag)) return 'textbox';
  return attr(node, 'role');
}
function selectorFor(node) {
  if (node.id) return '#' + node.id;
  const tag = (node.tagName || '').toLowerCase() || '*';
  const testid = attr(node, 'data-testid');
  if (testid) return '[data-testid="' + testid.replace(/"/g, '\\"') + '"]';
  return tag;
}
let nodes = [];
if (locator === 'first' || locator === 'last' || locator === 'nth') {
  nodes = Array.from(document.querySelectorAll(selector));
  if (locator === 'first') {
    nodes = nodes.slice(0, 1);
  } else if (locator === 'last') {
    nodes = nodes.slice(-1);
  } else {
    nodes = nodes.slice(index, index + 1);
  }
} else if (locator === 'role') {
  nodes = Array.from(document.querySelectorAll('*')).filter((node) => {
    const roleMatches = inferredRole(node) === role;
    const nameText = [attr(node, 'aria-label'), attr(node, 'name'), textOf(node)].join(' ');
    return roleMatches && (name === null || nameText.toLowerCase().includes(name.toLowerCase()));
  });
} else if (locator === 'text') {
  nodes = Array.from(document.querySelectorAll('body *')).filter((node) => {
    return textOf(node).includes(query);
  });
} else if (locator === 'label') {
  nodes = Array.from(document.querySelectorAll('label,[aria-label]')).filter((node) => {
    return textOf(node).includes(query) || attr(node, 'aria-label').includes(query);
  });
} else if (locator === 'placeholder') {
  nodes = Array.from(document.querySelectorAll('[placeholder]')).filter((node) => {
    return attr(node, 'placeholder').includes(query);
  });
} else if (locator === 'alt') {
  nodes = Array.from(document.querySelectorAll('[alt]')).filter((node) => {
    return attr(node, 'alt').includes(query);
  });
} else if (locator === 'title') {
  nodes = Array.from(document.querySelectorAll('[title]')).filter((node) => {
    return attr(node, 'title').includes(query);
  });
} else if (locator === 'testid') {
  nodes = Array.from(document.querySelectorAll('[data-testid]')).filter((node) => {
    return attr(node, 'data-testid') === query;
  });
}
const node = nodes[0];
if (!node) throw new Error('locator not found: ' + locator);
const elementRef = limuxStoreElementRef(node);
const resolvedSelector = selectorFor(node);
return {
  element_ref: elementRef,
  selector: resolvedSelector,
  matches: nodes.slice(0, 25).map((node) => ({
    selector: selectorFor(node),
    tag: (node.tagName || '').toLowerCase(),
    text: textOf(node).slice(0, 120)
  }))
};
"#;

// purpose: Build JavaScript for CMUX browser.find.* locator methods.
// inputs: Parsed BrowserAction::Find locator fields.
// returns/effects: Returns element_ref and match metadata, or throws when no element matches.
fn browser_find_script(action: &BrowserAction) -> String {
    let BrowserAction::Find {
        locator,
        selector,
        query,
        role,
        name,
        index,
    } = action
    else {
        unreachable!("browser find script requires BrowserAction::Find");
    };
    let locator = serde_json::to_string(locator).expect("json locator");
    let selector = serde_json::to_string(selector).expect("json selector");
    let query = serde_json::to_string(query).expect("json query");
    let role = serde_json::to_string(role).expect("json role");
    let name = serde_json::to_string(name).expect("json name");
    let index = serde_json::to_string(index).expect("json index");
    format!(
        r#"(function() {{
const locator = {locator};
const selector = {selector};
const query = {query};
const role = {role};
const name = {name};
const index = {index};
{ref_script}
{body}
}})()"#,
        ref_script = BROWSER_ELEMENT_REF_SCRIPT,
        body = BROWSER_FIND_SCRIPT_BODY,
    )
}

// purpose: Build JavaScript that reads document cookies in CMUX-compatible row form.
// inputs: Optional cookie name filter.
// returns/effects: Returns an array of {name, value} cookie rows.
fn browser_cookies_get_script(name: Option<&str>) -> String {
    let name = serde_json::to_string(&name).expect("json cookie name");
    format!(
        r#"(function() {{
const requestedName = {name};
const rows = document.cookie.split(';').map((raw) => raw.trim()).filter(Boolean).map((raw) => {{
  const index = raw.indexOf('=');
  const name = decodeURIComponent(index >= 0 ? raw.slice(0, index) : raw);
  const value = decodeURIComponent(index >= 0 ? raw.slice(index + 1) : '');
  return {{ name, value }};
}});
return requestedName === null ? rows : rows.filter((row) => row.name === requestedName);
}})()"#,
    )
}

// purpose: Build JavaScript that writes one cookie in the current document.
// inputs: Cookie name and value.
// returns/effects: Returns action metadata after setting the cookie path to root.
fn browser_cookie_set_script(name: &str, value: &str) -> String {
    let name = serde_json::to_string(name).expect("json cookie name");
    let value = serde_json::to_string(value).expect("json cookie value");
    format!(
        r#"(function() {{
const name = {name};
const value = {value};
const separator = String.fromCharCode(59);
document.cookie = encodeURIComponent(name) + '=' + encodeURIComponent(value) + separator + ' path=/';
return {{ action: 'cookies.set', ok: true, name, value }};
}})()"#,
    )
}

// purpose: Build JavaScript that clears one or all document cookies.
// inputs: Optional cookie name filter.
// returns/effects: Expires matching cookies and returns the count selected for clearing.
fn browser_cookies_clear_script(name: Option<&str>) -> String {
    let name = serde_json::to_string(&name).expect("json cookie name");
    format!(
        r#"(function() {{
const requestedName = {name};
const names = document.cookie.split(';').map((raw) => raw.trim()).filter(Boolean).map((raw) => {{
  const index = raw.indexOf('=');
  return decodeURIComponent(index >= 0 ? raw.slice(0, index) : raw);
}}).filter((name) => requestedName === null || name === requestedName);
for (const name of names) {{
  const separator = String.fromCharCode(59);
  const expires = ' expires=Thu, 01 Jan 1970 00:00:00 GMT';
  document.cookie = encodeURIComponent(name) + '=' + separator + expires + separator + ' path=/';
}}
return {{ action: 'cookies.clear', ok: true, cleared: names.length }};
}})()"#,
    )
}

// purpose: Build JavaScript that reads localStorage or sessionStorage.
// inputs: Storage type and key.
// returns/effects: Returns the stored string value or null when the key is absent.
fn browser_storage_get_script(storage_type: &str, key: &str) -> String {
    let storage_type = serde_json::to_string(storage_type).expect("json storage type");
    let key = serde_json::to_string(key).expect("json storage key");
    format!(
        r#"(function() {{
const storage = {storage_type} === 'session' ? window.sessionStorage : window.localStorage;
return storage.getItem({key});
}})()"#,
    )
}

// purpose: Build JavaScript that writes localStorage or sessionStorage.
// inputs: Storage type, key, and value.
// returns/effects: Stores the string value and returns action metadata.
fn browser_storage_set_script(storage_type: &str, key: &str, value: &str) -> String {
    let storage_type = serde_json::to_string(storage_type).expect("json storage type");
    let key = serde_json::to_string(key).expect("json storage key");
    let value = serde_json::to_string(value).expect("json storage value");
    format!(
        r#"(function() {{
const storageType = {storage_type};
const storage = storageType === 'session' ? window.sessionStorage : window.localStorage;
const key = {key};
const value = {value};
storage.setItem(key, value);
return {{ action: 'storage.set', ok: true, type: storageType, key, value }};
}})()"#,
    )
}

// purpose: Build JavaScript that clears localStorage or sessionStorage.
// inputs: Storage type and optional key.
// returns/effects: Removes one key or clears the namespace and returns action metadata.
fn browser_storage_clear_script(storage_type: &str, key: Option<&str>) -> String {
    let storage_type = serde_json::to_string(storage_type).expect("json storage type");
    let key = serde_json::to_string(&key).expect("json storage key");
    format!(
        r#"(function() {{
const storageType = {storage_type};
const key = {key};
const storage = storageType === 'session' ? window.sessionStorage : window.localStorage;
if (key === null) {{
  storage.clear();
}} else {{
  storage.removeItem(key);
}}
return {{ action: 'storage.clear', ok: true, type: storageType, key }};
}})()"#,
    )
}

// purpose: Build JavaScript that snapshots browser session state from the current page.
// inputs: None; the script reads URL, title, localStorage, and sessionStorage.
// returns/effects: Returns JSON-serializable browser state without mutating the page.
fn browser_state_save_script() -> &'static str {
    r#"(() => {
  const dumpStorage = (storage) => {
    const out = {};
    for (let index = 0; index < storage.length; index += 1) {
      const key = storage.key(index);
      if (key !== null) out[key] = storage.getItem(key);
    }
    return out;
  };
  return {
    url: location.href,
    title: document.title,
    local_storage: dumpStorage(localStorage),
    session_storage: dumpStorage(sessionStorage)
  };
})()"#
}

// purpose: Build JavaScript that restores browser session state into the current page.
// inputs: Parsed state JSON with optional URL, local_storage, and session_storage fields.
// returns/effects: Clears and rewrites Web Storage, then navigates to the saved URL when present.
fn browser_state_load_script(state_json: &serde_json::Value) -> Result<String, serde_json::Error> {
    let encoded = serde_json::to_string(state_json)?;
    Ok(format!(
        r#"(() => {{
  const state = {encoded};
  const applyStorage = (storage, values) => {{
    storage.clear();
    for (const [key, value] of Object.entries(values || {{}})) {{
      storage.setItem(key, value === null || value === undefined ? "" : String(value));
    }}
  }};
  applyStorage(localStorage, state.local_storage);
  applyStorage(sessionStorage, state.session_storage);
  if (state.url && location.href !== state.url) {{
    location.href = state.url;
  }}
  return {{ ok: true, url: state.url || location.href, title: state.title || document.title }};
}})()"#
    ))
}

// purpose: Build JavaScript that reads a selected element's computed styles.
// inputs: CSS selector and optional style property name.
// returns/effects: Returns a JavaScript snippet that fails on missing elements and returns a string or style map.
fn browser_styles_script(selector: &str, property: Option<&str>) -> String {
    let selector = serde_json::to_string(selector).expect("json selector");
    let property = serde_json::to_string(&property).expect("json property");
    format!(
        r#"(function() {{
{ref_script}
const target = {selector};
const property = {property};
const resolved = limuxResolveElement(target);
const node = resolved.node;
const styles = window.getComputedStyle(node);
if (property !== null) {{
  return styles.getPropertyValue(property);
}}
const result = {{}};
for (const name of styles) {{
  result[name] = styles.getPropertyValue(name);
}}
return result;
}})()"#,
        ref_script = BROWSER_ELEMENT_REF_SCRIPT,
    )
}

// purpose: Build JavaScript that reads one boolean DOM state from a selected element.
// inputs: CSS selector and one of checked, enabled, or visible.
// returns/effects: Returns the state value while failing loudly for missing selectors.
fn browser_is_script(selector: &str, state_name: &str) -> String {
    let expression = match state_name {
        "checked" => {
            r#"(() => {
  if ('checked' in node) return Boolean(node.checked);
  return node.getAttribute('aria-checked') === 'true';
})()"#
        }
        "enabled" => {
            r#"(() => {
  if (node.disabled === true) return false;
  return node.getAttribute('aria-disabled') !== 'true';
})()"#
        }
        "visible" => {
            r#"(() => {
  const style = window.getComputedStyle(node);
  const hasBox = Boolean(node.offsetWidth || node.offsetHeight || node.getClientRects().length);
  return hasBox && style.visibility !== 'hidden' && style.display !== 'none';
})()"#
        }
        _ => unreachable!("browser is state should be checked, enabled, or visible"),
    };
    browser_required_element_script(selector, expression)
}

// purpose: Build JavaScript for a DOM action against one required CSS selector.
// inputs: CSS selector and JavaScript body that operates on `node`.
// returns/effects: Returns a script that throws when the element cannot be resolved.
fn browser_element_action_script(selector: &str, body: &str) -> String {
    let selector = serde_json::to_string(selector).expect("json selector");
    format!(
        r#"(function() {{
{ref_script}
const target = {selector};
const resolved = limuxResolveElement(target);
const node = resolved.node;
const selector = resolved.selector;
const element_ref = resolved.element_ref;
{body}
}})()"#,
        ref_script = BROWSER_ELEMENT_REF_SCRIPT,
    )
}

// purpose: Build JavaScript for keyboard event browser actions.
// inputs: Key value and event name.
// returns/effects: Dispatches a bubbling keyboard event on the focused element or document body.
fn browser_key_action_script(key: &str, event_name: &str) -> String {
    let key = serde_json::to_string(key).expect("json key");
    let event_name = serde_json::to_string(event_name).expect("json event");
    format!(
        r#"(function() {{
const target = document.activeElement || document.body || document.documentElement;
if (!target) throw new Error('no keyboard target available');
target.dispatchEvent(new KeyboardEvent({event_name}, {{ key: {key}, bubbles: true, cancelable: true }}));
return {{ action: {event_name}, key: {key}, ok: true }};
}})()"#,
    )
}

// purpose: Build JavaScript for browser scroll actions.
// inputs: Optional CSS selector and x/y deltas.
// returns/effects: Scrolls the selected element or window and returns action metadata.
fn browser_scroll_script(selector: Option<&str>, dx: i64, dy: i64) -> String {
    let selector = serde_json::to_string(&selector).expect("json selector");
    format!(
        r#"(function() {{
{ref_script}
const selector = {selector};
if (selector !== null) {{
  const resolved = limuxResolveElement(selector);
  const node = resolved.node;
  node.scrollBy({{ left: {dx}, top: {dy}, behavior: 'instant' }});
  return {{ action: 'scroll', selector: resolved.selector, dx: {dx}, dy: {dy}, ok: true }};
}}
window.scrollBy({{ left: {dx}, top: {dy}, behavior: 'instant' }});
return {{ action: 'scroll', selector: null, dx: {dx}, dy: {dy}, ok: true }};
}})()"#,
        ref_script = BROWSER_ELEMENT_REF_SCRIPT,
    )
}

fn browser_dblclick_body() -> &'static str {
    r#"node.dispatchEvent(new MouseEvent('dblclick', {
  bubbles: true,
  cancelable: true,
  view: window
}));
return { action: 'dblclick', selector, ok: true };"#
}

fn browser_fill_body(text: &str) -> String {
    let text = serde_json::to_string(text).expect("json text");
    format!(
        r#"const value = {text};
if (!('value' in node)) throw new Error('element is not fillable: ' + selector);
node.focus();
node.value = value;
node.dispatchEvent(new Event('input', {{ bubbles: true }}));
node.dispatchEvent(new Event('change', {{ bubbles: true }}));
return {{ action: 'fill', selector, ok: true }};"#
    )
}

fn browser_type_body(text: &str) -> String {
    let text = serde_json::to_string(text).expect("json text");
    format!(
        r#"const value = {text};
if (!('value' in node)) throw new Error('element is not typeable: ' + selector);
node.focus();
node.value = String(node.value || '') + value;
node.dispatchEvent(new Event('input', {{ bubbles: true }}));
node.dispatchEvent(new Event('change', {{ bubbles: true }}));
return {{ action: 'type', selector, ok: true }};"#
    )
}

fn browser_select_body(value: &str) -> String {
    let value = serde_json::to_string(value).expect("json value");
    format!(
        r#"const value = {value};
if (node.tagName !== 'SELECT') throw new Error('element is not a select: ' + selector);
const option = Array.from(node.options).find((item) => {{
  return item.value === value || item.text === value;
}});
if (!option) throw new Error('select option not found: ' + value);
node.value = option.value;
node.dispatchEvent(new Event('input', {{ bubbles: true }}));
node.dispatchEvent(new Event('change', {{ bubbles: true }}));
return {{ action: 'select', selector, value: option.value, ok: true }};"#
    )
}

fn browser_hover_body() -> &'static str {
    r#"node.dispatchEvent(new MouseEvent('mouseover', {
  bubbles: true,
  cancelable: true,
  view: window
}));
node.dispatchEvent(new MouseEvent('mouseenter', {
  bubbles: true,
  cancelable: true,
  view: window
}));
return { action: 'hover', selector, ok: true };"#
}

fn browser_focus_body() -> &'static str {
    r#"if (typeof node.focus !== 'function') {
  throw new Error('element is not focusable: ' + selector);
}
node.focus();
return { action: 'focus', selector, ok: true };"#
}

fn browser_check_body(checked: bool) -> String {
    let action = if checked { "check" } else { "uncheck" };
    format!(
        r#"if (node.type !== 'checkbox' && node.type !== 'radio') {{
  throw new Error('element is not checkable: ' + selector);
}}
node.checked = {checked};
node.dispatchEvent(new Event('input', {{ bubbles: true }}));
node.dispatchEvent(new Event('change', {{ bubbles: true }}));
return {{ action: '{action}', selector, checked: {checked}, ok: true }};"#
    )
}

fn browser_snapshot_script(interactive: bool, compact: bool, max_depth: Option<usize>) -> String {
    let max_depth = max_depth.unwrap_or(4).min(12);
    format!(
        r#"(function() {{
{ref_script}
const maxDepth = {max_depth};
const interactiveOnly = {interactive};
const compact = {compact};
const refs = {{}};
limuxResetElementRefs();
function labelFor(node) {{
  const tag = (node.tagName || '').toLowerCase();
  const id = node.id ? '#' + node.id : '';
  const cls = node.className && typeof node.className === 'string'
    ? '.' + node.className.trim().split(/\s+/).filter(Boolean).slice(0, 3).join('.')
    : '';
  const role = node.getAttribute ? (node.getAttribute('role') || '') : '';
  const name = node.getAttribute ? (node.getAttribute('aria-label') || node.getAttribute('name') || '') : '';
  const text = (node.innerText || node.textContent || '').replace(/\s+/g, ' ').trim().slice(0, compact ? 60 : 120);
  return [tag + id + cls, role && 'role=' + role, name && 'name=' + name, text && JSON.stringify(text)]
    .filter(Boolean)
    .join(' ');
}}
function isInteractive(node) {{
  const tag = (node.tagName || '').toLowerCase();
  return ['a','button','input','select','textarea','summary'].includes(tag)
    || (node.getAttribute && (node.getAttribute('role') || node.getAttribute('tabindex') !== null))
    || !!node.onclick;
}}
function walk(node, depth, lines) {{
  if (!node || depth > maxDepth || lines.length >= 250) return;
  if (node.nodeType !== Node.ELEMENT_NODE) return;
  const include = !interactiveOnly || isInteractive(node);
  if (include) {{
    const ref = isInteractive(node) ? limuxStoreElementRef(node) : null;
    if (ref) refs[ref] = {{ selector: node.id ? '#' + node.id : null, tag: (node.tagName || '').toLowerCase() }};
    lines.push('  '.repeat(depth) + (ref ? '[' + ref + '] ' : '') + labelFor(node));
  }}
  for (const child of Array.from(node.children || [])) {{
    walk(child, depth + 1, lines);
  }}
}}
const lines = [];
walk(document.body || document.documentElement, 0, lines);
return {{
  url: location.href,
  title: document.title || '',
  text: lines.join('\n'),
  refs,
  interactive
}};
}})()"#,
        ref_script = BROWSER_ELEMENT_REF_SCRIPT,
    )
}

fn browser_wait_script(action: &BrowserAction, current_uri: Option<&str>) -> String {
    let BrowserAction::Wait {
        selector,
        text,
        url_contains,
        load_state,
        function,
        timeout_ms,
    } = action
    else {
        return "({ matched: false, condition: 'invalid' })".to_string();
    };
    let uri = serde_json::to_string(&current_uri.unwrap_or_default()).expect("json string");
    let selector = serde_json::to_string(&selector.as_deref()).expect("json selector");
    let text = serde_json::to_string(&text.as_deref()).expect("json text");
    let url_contains = serde_json::to_string(&url_contains.as_deref()).expect("json url");
    let load_state = serde_json::to_string(&load_state.as_deref()).expect("json load state");
    let function = serde_json::to_string(&function.as_deref()).expect("json function");
    format!(
        r#"(function() {{
const selector = {selector};
const text = {text};
const urlContains = {url_contains};
const loadState = {load_state};
const fnSource = {function};
const currentUri = {uri};
let matched = false;
let condition = 'unknown';
try {{
  if (selector !== null) {{
    condition = 'selector';
    matched = !!document.querySelector(selector);
  }} else if (text !== null) {{
    condition = 'text';
    matched = ((document.body && document.body.innerText) || document.documentElement.innerText || '').includes(text);
  }} else if (urlContains !== null) {{
    condition = 'url_contains';
    matched = currentUri.includes(urlContains) || location.href.includes(urlContains);
  }} else if (loadState !== null) {{
    condition = 'load_state';
    matched = document.readyState === loadState;
  }} else if (fnSource !== null) {{
    condition = 'function';
    matched = !!Function('return (' + fnSource + ')')();
  }}
  return {{ matched, condition, readyState: document.readyState, url: location.href, timeout_ms: {timeout_ms} }};
}} catch (error) {{
  return {{ matched: false, condition, error: String(error), readyState: document.readyState, url: location.href, timeout_ms: {timeout_ms} }};
}}
}})()"#,
    )
}

struct BrowserWaitPollState {
    started: std::time::Instant,
    in_flight: bool,
    completed: bool,
    last_condition: String,
}

// purpose: Poll a browser wait predicate without blocking GTK while preserving loud failures.
// inputs: Browser target, JavaScript predicate, base response payload, timeout, and socket reply.
// returns/effects: Sends exactly one bridge reply on match, timeout, or JavaScript failure.
fn send_browser_wait_response(
    browser: pane::BrowserSurfaceTarget,
    script: String,
    payload: serde_json::Value,
    reply: std::sync::mpsc::Sender<Result<serde_json::Value, BridgeError>>,
    timeout_ms: u64,
) {
    let poll_state = Rc::new(RefCell::new(BrowserWaitPollState {
        started: std::time::Instant::now(),
        in_flight: false,
        completed: false,
        last_condition: "unknown".to_string(),
    }));
    let reply = Rc::new(RefCell::new(Some(reply)));
    let payload = Rc::new(RefCell::new(payload));
    let deadline = std::time::Duration::from_millis(timeout_ms);

    glib::timeout_add_local(
        std::time::Duration::from_millis(BROWSER_WAIT_POLL_INTERVAL_MS),
        move || {
            {
                let mut state = poll_state.borrow_mut();
                if state.completed {
                    return glib::ControlFlow::Break;
                }
                if state.started.elapsed() >= deadline {
                    state.completed = true;
                    if let Some(reply) = reply.borrow_mut().take() {
                        let _ = reply.send(Err(BridgeError::not_found(format!(
                            "browser.wait timed out after {timeout_ms}ms waiting for {}",
                            state.last_condition
                        ))));
                    }
                    return glib::ControlFlow::Break;
                }
                if state.in_flight {
                    return glib::ControlFlow::Continue;
                }
                state.in_flight = true;
            }

            let browser = browser.clone();
            let script = script.clone();
            let poll_state = Rc::clone(&poll_state);
            let reply = Rc::clone(&reply);
            let payload = Rc::clone(&payload);
            browser.evaluate_javascript(&script, move |result| {
                let mut state = poll_state.borrow_mut();
                if state.completed {
                    return;
                }
                state.in_flight = false;
                match result {
                    Ok(value) => {
                        state.last_condition = value
                            .get("condition")
                            .and_then(serde_json::Value::as_str)
                            .unwrap_or("unknown")
                            .to_string();
                        let matched = value
                            .get("matched")
                            .and_then(serde_json::Value::as_bool)
                            .unwrap_or(false);
                        if matched {
                            state.completed = true;
                            let mut response = payload.borrow().clone();
                            response["wait"] = value;
                            response["matched"] = serde_json::Value::Bool(true);
                            if let Some(reply) = reply.borrow_mut().take() {
                                let _ = reply.send(Ok(response));
                            }
                        }
                    }
                    Err(error) => {
                        state.completed = true;
                        if let Some(reply) = reply.borrow_mut().take() {
                            let _ = reply.send(Err(BridgeError::internal(format!(
                                "browser wait evaluation failed: {error}"
                            ))));
                        }
                    }
                }
            });

            glib::ControlFlow::Continue
        },
    );
}

fn send_pane_create_response_after_command(
    pane_widget: gtk::Widget,
    surface_id: String,
    command: String,
    response: serde_json::Value,
    reply: std::sync::mpsc::Sender<Result<serde_json::Value, BridgeError>>,
) {
    let mut attempts = 0;
    let mut reply = Some(reply);
    let command = format!("{command}\n");

    glib::timeout_add_local(
        std::time::Duration::from_millis(PANE_CREATE_COMMAND_READY_INTERVAL_MS),
        move || {
            attempts += 1;

            if let Some((matched_surface_id, handle)) =
                pane::exact_terminal_handle_for_surface(&pane_widget, &surface_id)
            {
                if matched_surface_id == surface_id && handle.send_text(&command) {
                    if let Some(reply) = reply.take() {
                        let _ = reply.send(Ok(response.clone()));
                    }
                    return glib::ControlFlow::Break;
                }
            }

            if attempts >= PANE_CREATE_COMMAND_READY_ATTEMPTS {
                if let Some(reply) = reply.take() {
                    let _ = reply.send(Err(BridgeError::internal(format!(
                        "pane.create command target surface {surface_id} never became writable"
                    ))));
                }
                glib::ControlFlow::Break
            } else {
                glib::ControlFlow::Continue
            }
        },
    );
}

fn normalize_workspace_handle(raw: &str) -> &str {
    raw.trim()
        .strip_prefix("workspace:")
        .unwrap_or_else(|| raw.trim())
}

fn normalize_pane_handle(raw: &str) -> &str {
    raw.trim()
        .strip_prefix("pane:")
        .unwrap_or_else(|| raw.trim())
}

fn parse_pane_handle(raw: &str) -> Option<u32> {
    normalize_pane_handle(raw).parse::<u32>().ok()
}

fn workspace_index_for_target(state: &AppState, target: &WorkspaceTarget) -> Option<usize> {
    match target {
        WorkspaceTarget::Active => (!state.workspaces.is_empty()).then_some(state.active_idx),
        WorkspaceTarget::Handle(handle) => {
            let normalized = normalize_workspace_handle(handle);
            state
                .workspaces
                .iter()
                .position(|workspace| workspace.id == normalized)
        }
        WorkspaceTarget::Name(name) => state
            .workspaces
            .iter()
            .position(|workspace| workspace.name == *name),
        WorkspaceTarget::Index(index) => (*index < state.workspaces.len()).then_some(*index),
    }
}

fn workspace_row(index: usize, selected_idx: usize, workspace: &Workspace) -> serde_json::Value {
    let cwd = workspace.cwd.borrow().clone().unwrap_or_default();
    serde_json::json!({
        "index": index,
        "id": workspace.id.as_str(),
        "ref": workspace_ref(&workspace.id),
        "workspace_id": workspace.id.as_str(),
        "workspace_ref": workspace_ref(&workspace.id),
        "title": workspace.name.as_str(),
        "name": workspace.name.as_str(),
        "description": workspace.description.as_deref(),
        "selected": index == selected_idx,
        "focused": index == selected_idx,
        "cwd": cwd,
        "group_id": workspace.group_id.as_deref(),
    })
}

// purpose: Render CMUX-compatible workspace environment for one workspace.
// inputs: Current app state, workspace selector, and masking preference.
// returns/effects: Returns environment JSON without mutating state.
fn workspace_env_payload(
    state: &AppState,
    target: &WorkspaceTarget,
    mask: bool,
) -> Result<serde_json::Value, BridgeError> {
    let Some(index) = workspace_index_for_target(state, target) else {
        return Err(BridgeError::not_found("workspace not found"));
    };
    let workspace = &state.workspaces[index];
    let environment = workspace
        .environment
        .iter()
        .map(|(key, value)| {
            let value = if mask { "********" } else { value };
            (key.clone(), serde_json::Value::String(value.to_string()))
        })
        .collect::<serde_json::Map<_, _>>();
    Ok(serde_json::json!({
        "workspace_id": workspace.id.as_str(),
        "workspace_ref": workspace_ref(&workspace.id),
        "environment": environment,
    }))
}

// purpose: Render one CMUX-compatible workspace-group row for control clients.
// inputs: A persisted workspace group.
// returns/effects: Returns JSON only; does not mutate GTK state.
fn workspace_group_row(group: &WorkspaceGroupState) -> serde_json::Value {
    serde_json::json!({
        "id": group.id.as_str(),
        "group_id": group.id.as_str(),
        "ref": workspace_group_ref(&group.id),
        "group_ref": workspace_group_ref(&group.id),
        "name": group.name.as_str(),
        "title": group.name.as_str(),
        "isCollapsed": group.is_collapsed,
        "isPinned": group.is_pinned,
        "anchorWorkspaceId": group.anchor_workspace_id.as_deref(),
        "customColor": group.custom_color.as_deref(),
        "iconSymbol": group.icon_symbol.as_deref(),
    })
}

fn workspace_group_ref(id: &str) -> String {
    format!("workspace_group:{id}")
}

fn normalize_workspace_group_handle(raw: &str) -> &str {
    raw.trim()
        .strip_prefix("workspace_group:")
        .or_else(|| raw.trim().strip_prefix("workspace-group:"))
        .unwrap_or_else(|| raw.trim())
}

fn workspace_group_index(state: &AppState, raw: &str) -> Option<usize> {
    let id = normalize_workspace_group_handle(raw);
    state
        .workspace_groups
        .iter()
        .position(|group| group.id == id)
}

fn workspace_group_payload(group: &WorkspaceGroupState) -> serde_json::Value {
    serde_json::json!({
        "group_id": group.id.as_str(),
        "group_ref": workspace_group_ref(&group.id),
        "group": workspace_group_row(group),
    })
}

fn workspace_payload(state: &AppState, index: usize) -> Option<serde_json::Value> {
    let workspace = state.workspaces.get(index)?;
    Some(serde_json::json!({
        "workspace_id": workspace.id.as_str(),
        "workspace_ref": workspace_ref(&workspace.id),
        "workspace": workspace_row(index, state.active_idx, workspace),
        "title": workspace.name.as_str(),
        "name": workspace.name.as_str(),
    }))
}

// purpose: Snapshot workspace metadata needed for CMUX lifecycle events.
// inputs: Current app state and a workspace index.
// returns/effects: Returns an owned snapshot without mutating GTK state.
fn workspace_event_snapshot(state: &AppState, index: usize) -> Option<WorkspaceEventSnapshot> {
    let workspace = state.workspaces.get(index)?;
    let tab_count = pane::surface_summaries_for_root(&workspace.root).len();
    Some(WorkspaceEventSnapshot {
        workspace_id: workspace.id.clone(),
        workspace_ref: workspace_ref(&workspace.id),
        title: workspace.name.clone(),
        description: workspace.description.clone(),
        index,
        selected: index == state.active_idx,
        favorite: workspace.favorite,
        group_id: workspace.group_id.clone(),
        tab_count,
    })
}

// purpose: Build a CMUX-compatible workspace lifecycle event payload.
// inputs: Workspace snapshot and optional event-specific fields.
// returns/effects: Returns JSON without publishing or mutating state.
fn workspace_lifecycle_payload(
    snapshot: &WorkspaceEventSnapshot,
    previous_workspace_id: Option<&str>,
    extra: serde_json::Value,
) -> serde_json::Value {
    let mut payload = serde_json::json!({
        "workspace_id": snapshot.workspace_id,
        "workspace_ref": snapshot.workspace_ref,
        "title": snapshot.title,
        "description": snapshot.description,
        "index": snapshot.index,
        "selected": snapshot.selected,
        "favorite": snapshot.favorite,
        "group_id": snapshot.group_id,
        "tab_count": snapshot.tab_count,
        "previous_workspace_id": previous_workspace_id,
        "previous_workspace_ref": previous_workspace_id.map(workspace_ref),
    });
    if let (Some(object), Some(extra_object)) = (payload.as_object_mut(), extra.as_object()) {
        for (key, value) in extra_object {
            object.insert(key.clone(), value.clone());
        }
    }
    payload
}

// purpose: Publish one CMUX workspace lifecycle event.
// inputs: Event name, workspace snapshot, previous workspace id, and extra payload fields.
// returns/effects: Appends a retained workspace event to the host event bus.
fn publish_workspace_lifecycle_event(
    name: &'static str,
    snapshot: &WorkspaceEventSnapshot,
    previous_workspace_id: Option<&str>,
    extra: serde_json::Value,
) -> u64 {
    crate::event_bus::bus().publish(crate::event_bus::EventPublish {
        name,
        category: "workspace",
        source: "workspace.lifecycle",
        workspace_id: Some(serde_json::Value::String(snapshot.workspace_id.clone())),
        surface_id: None,
        pane_id: None,
        payload: workspace_lifecycle_payload(snapshot, previous_workspace_id, extra),
    })
}

// purpose: Build a CMUX-compatible workspace.reordered payload.
// inputs: Ordered workspace ids, moved ids, pinned ids, and active selection metadata.
// returns/effects: Returns JSON without publishing or mutating state.
fn workspace_reordered_payload(
    ordered_workspace_ids: Vec<String>,
    moved_workspace_ids: Vec<String>,
    pinned_workspace_ids: Vec<String>,
    selected_index: usize,
) -> serde_json::Value {
    serde_json::json!({
        "workspace_ids": ordered_workspace_ids,
        "moved_workspace_ids": moved_workspace_ids,
        "pinned_workspace_ids": pinned_workspace_ids,
        "selected_workspace_index": selected_index,
        "count": ordered_workspace_ids.len(),
    })
}

// purpose: Publish a CMUX workspace.reordered event for changed sidebar order.
// inputs: Ordered workspace ids, moved workspace ids, pinned ids, and active index.
// returns/effects: Appends a retained workspace event to the host event bus.
fn publish_workspace_reordered_event(
    ordered_workspace_ids: Vec<String>,
    moved_workspace_ids: Vec<String>,
    pinned_workspace_ids: Vec<String>,
    selected_workspace_id: Option<String>,
    selected_index: usize,
) {
    crate::event_bus::bus().publish(crate::event_bus::EventPublish {
        name: "workspace.reordered",
        category: "workspace",
        source: "workspace.lifecycle",
        workspace_id: selected_workspace_id.map(serde_json::Value::String),
        surface_id: None,
        pane_id: None,
        payload: workspace_reordered_payload(
            ordered_workspace_ids,
            moved_workspace_ids,
            pinned_workspace_ids,
            selected_index,
        ),
    });
}

// purpose: Build a redacted CMUX surface input event payload.
// inputs: Workspace/surface ids, optional pane id, and input metadata.
// returns/effects: Returns JSON without including raw terminal text.
fn surface_input_event_payload(
    workspace_id: &str,
    surface_id: &str,
    pane_id: Option<u32>,
    text_length: usize,
) -> serde_json::Value {
    serde_json::json!({
        "workspace_id": workspace_id,
        "workspace_ref": workspace_ref(workspace_id),
        "surface_id": surface_id,
        "surface_ref": surface_ref(surface_id),
        "pane_id": pane_id.map(|pane_id| pane_id.to_string()),
        "pane_ref": pane_id.map(pane_ref),
        "text_length": text_length,
        "redacted_fields": ["text"],
    })
}

// purpose: Build a CMUX surface key event payload.
// inputs: Workspace/surface ids, optional pane id, and the sent key name.
// returns/effects: Returns JSON describing the key command without mutating state.
fn surface_key_event_payload(
    workspace_id: &str,
    surface_id: &str,
    pane_id: Option<u32>,
    key: &str,
) -> serde_json::Value {
    serde_json::json!({
        "workspace_id": workspace_id,
        "workspace_ref": workspace_ref(workspace_id),
        "surface_id": surface_id,
        "surface_ref": surface_ref(surface_id),
        "pane_id": pane_id.map(|pane_id| pane_id.to_string()),
        "pane_ref": pane_id.map(pane_ref),
        "key": key,
    })
}

// purpose: Derive the live pane id embedded in Limux surface ids.
// inputs: Surface id in the host `pane_id:tab_id` shape.
// returns/effects: Returns None when the id is not in the host pane-prefixed shape.
fn pane_id_from_surface_id(surface_id: &str) -> Option<u32> {
    surface_id
        .split_once(':')
        .and_then(|(pane_id, _)| pane_id.parse::<u32>().ok())
}

// purpose: Publish a CMUX surface input event after socket text injection succeeds.
// inputs: Workspace id, surface id, and sent text length.
// returns/effects: Appends a retained redacted surface.input_sent event.
fn publish_surface_input_sent_event(
    workspace_id: &str,
    surface_id: &str,
    text_length: usize,
) -> u64 {
    let pane_id = pane_id_from_surface_id(surface_id);
    crate::event_bus::bus().publish(crate::event_bus::EventPublish {
        name: "surface.input_sent",
        category: "surface",
        source: "surface.io",
        workspace_id: Some(serde_json::Value::String(workspace_id.to_string())),
        surface_id: Some(serde_json::Value::String(surface_id.to_string())),
        pane_id: pane_id.map(|pane_id| serde_json::Value::String(pane_id.to_string())),
        payload: surface_input_event_payload(workspace_id, surface_id, pane_id, text_length),
    })
}

// purpose: Publish a CMUX surface key event after socket key injection succeeds.
// inputs: Workspace id, surface id, and sent key.
// returns/effects: Appends a retained surface.key_sent event.
fn publish_surface_key_sent_event(workspace_id: &str, surface_id: &str, key: &str) -> u64 {
    let pane_id = pane_id_from_surface_id(surface_id);
    crate::event_bus::bus().publish(crate::event_bus::EventPublish {
        name: "surface.key_sent",
        category: "surface",
        source: "surface.io",
        workspace_id: Some(serde_json::Value::String(workspace_id.to_string())),
        surface_id: Some(serde_json::Value::String(surface_id.to_string())),
        pane_id: pane_id.map(|pane_id| serde_json::Value::String(pane_id.to_string())),
        payload: surface_key_event_payload(workspace_id, surface_id, pane_id, key),
    })
}

// purpose: Return only the last requested text lines while preserving line endings.
// inputs: Plain terminal text and optional positive line limit.
// returns/effects: Returns the original text when no limit or a non-truncating limit is supplied.
fn limit_text_to_last_lines(text: String, lines: Option<u64>) -> String {
    let Some(lines) = lines else {
        return text;
    };
    let max_lines = usize::try_from(lines).unwrap_or(usize::MAX);
    let chunks = text.split_inclusive('\n').collect::<Vec<_>>();
    if chunks.len() <= max_lines {
        return text;
    }
    chunks[chunks.len().saturating_sub(max_lines)..].concat()
}

// purpose: Build a CMUX surface lifecycle event payload from a live surface summary.
// inputs: Workspace id, surface summary, and event-specific metadata.
// returns/effects: Returns JSON without mutating host state.
fn surface_lifecycle_event_payload(
    workspace_id: &str,
    surface: &pane::SurfaceSummary,
    extra: serde_json::Value,
) -> serde_json::Value {
    let mut payload = serde_json::json!({
        "workspace_id": workspace_id,
        "workspace_ref": workspace_ref(workspace_id),
        "surface_id": surface.surface_id,
        "surface_ref": surface_ref(&surface.surface_id),
        "pane_id": surface.pane_id.to_string(),
        "pane_ref": pane_ref(surface.pane_id),
        "surface_title": surface.title,
        "surface_type": surface.kind,
        "selected": surface.selected,
        "cwd": surface.cwd,
        "uri": surface.uri,
    });
    if let (Some(payload), Some(extra)) = (payload.as_object_mut(), extra.as_object()) {
        for (key, value) in extra {
            payload.insert(key.clone(), value.clone());
        }
    }
    payload
}

// purpose: Publish a CMUX surface lifecycle event after a live surface mutation succeeds.
// inputs: Event name, workspace id, surface summary, and event-specific metadata.
// returns/effects: Appends a retained surface lifecycle event to the host event bus.
fn publish_surface_lifecycle_event(
    name: &'static str,
    workspace_id: &str,
    surface: &pane::SurfaceSummary,
    extra: serde_json::Value,
) -> u64 {
    crate::event_bus::bus().publish(crate::event_bus::EventPublish {
        name,
        category: "surface",
        source: "surface.lifecycle",
        workspace_id: Some(serde_json::Value::String(workspace_id.to_string())),
        surface_id: Some(serde_json::Value::String(surface.surface_id.clone())),
        pane_id: Some(serde_json::Value::String(surface.pane_id.to_string())),
        payload: surface_lifecycle_event_payload(workspace_id, surface, extra),
    })
}

// purpose: Select a live workspace through the same GTK stack/sidebar path as UI navigation.
// inputs: Shared app state and target workspace index.
// returns/effects: Changes active workspace when needed and returns the CMUX-shaped payload.
fn select_workspace_for_control(
    state: &State,
    index: usize,
) -> Result<serde_json::Value, BridgeError> {
    let row = {
        let app_state = state.borrow();
        app_state
            .workspaces
            .get(index)
            .map(|workspace| workspace.sidebar_row.clone())
            .ok_or_else(|| BridgeError::not_found("workspace not found"))?
    };
    let sidebar_list = state.borrow().sidebar_list.clone();
    switch_workspace(state, index);
    sidebar_list.select_row(Some(&row));

    let app_state = state.borrow();
    workspace_payload(&app_state, index)
        .ok_or_else(|| BridgeError::not_found("workspace not found"))
}

fn focused_surface_payload(state: &State) -> Option<serde_json::Value> {
    let (workspace_id, workspace_name, pane_widget) = {
        let app_state = state.borrow();
        let workspace = app_state.active_workspace()?;
        let pane_widget = find_focused_pane(state).map(|(_, pane_widget)| pane_widget)?;
        (workspace.id.clone(), workspace.name.clone(), pane_widget)
    };
    let surface = pane::active_surface_summary(&pane_widget)?;
    let mut payload = serde_json::Map::new();
    payload.insert(
        "workspace_id".to_string(),
        serde_json::Value::String(workspace_id.clone()),
    );
    payload.insert(
        "workspace_ref".to_string(),
        serde_json::Value::String(workspace_ref(&workspace_id)),
    );
    payload.insert(
        "title".to_string(),
        serde_json::Value::String(workspace_name.clone()),
    );
    payload.insert(
        "name".to_string(),
        serde_json::Value::String(workspace_name),
    );
    payload.insert(
        "pane_id".to_string(),
        serde_json::Value::String(surface.pane_id.to_string()),
    );
    payload.insert(
        "pane_ref".to_string(),
        serde_json::Value::String(pane_ref(surface.pane_id)),
    );
    payload.insert(
        "surface_id".to_string(),
        serde_json::Value::String(surface.surface_id.clone()),
    );
    payload.insert(
        "surface_ref".to_string(),
        serde_json::Value::String(surface_ref(&surface.surface_id)),
    );
    if !surface.title.is_empty() {
        payload.insert(
            "surface_title".to_string(),
            serde_json::Value::String(surface.title),
        );
    }
    payload.insert(
        "surface_type".to_string(),
        serde_json::Value::String(surface.kind),
    );
    if let Some(cwd) = surface.cwd.filter(|cwd| !cwd.is_empty()) {
        payload.insert("cwd".to_string(), serde_json::Value::String(cwd));
    }
    if let Some(uri) = surface.uri.filter(|uri| !uri.is_empty()) {
        payload.insert("uri".to_string(), serde_json::Value::String(uri));
    }
    Some(serde_json::Value::Object(payload))
}

fn focused_ids_for_workspace(state: &State, workspace_id: &str) -> (Option<u32>, Option<String>) {
    let is_active = {
        let app_state = state.borrow();
        app_state
            .active_workspace()
            .map(|workspace| workspace.id == workspace_id)
            .unwrap_or(false)
    };
    if !is_active {
        return (None, None);
    }

    let Some((_focused_workspace_id, pane_widget)) = find_focused_pane(state) else {
        return (None, None);
    };
    let Some(surface) = pane::active_surface_summary(&pane_widget) else {
        return (None, None);
    };
    (Some(surface.pane_id), Some(surface.surface_id))
}

// purpose: Preserve tmux-compatible last-pane history after a successful focus change.
// inputs: App state, workspace id, newly focused pane id, and previously focused pane id.
// returns/effects: Updates workspace last_pane_id only when focus moved to a different pane.
fn record_previous_pane_if_changed(
    state: &State,
    workspace_id: &str,
    pane_id: u32,
    previous_pane_id: Option<u32>,
) {
    if previous_pane_id.is_none_or(|previous| previous == pane_id) {
        return;
    }
    if let Some(workspace) = state
        .borrow_mut()
        .workspaces
        .iter_mut()
        .find(|workspace| workspace.id == workspace_id)
    {
        workspace.last_pane_id = previous_pane_id;
    }
}

// purpose: Focus a live pane and record previous focus for tmux-compatible last-pane.
// inputs: App state, workspace index, and pane id.
// returns/effects: Focuses the pane's active tab and updates workspace last_pane_id.
fn focus_pane_for_control(
    state: &State,
    workspace_index: usize,
    pane_id: u32,
) -> Result<serde_json::Value, BridgeError> {
    let workspace_id = {
        let app_state = state.borrow();
        let workspace = app_state
            .workspaces
            .get(workspace_index)
            .ok_or_else(|| BridgeError::not_found("workspace not found"))?;
        workspace.id.clone()
    };
    let previous_pane_id = focused_ids_for_workspace(state, &workspace_id).0;

    let result = {
        let app_state = state.borrow();
        let workspace = app_state
            .workspaces
            .get(workspace_index)
            .ok_or_else(|| BridgeError::not_found("workspace not found"))?;
        pane::focus_pane_for_root(&workspace.root, pane_id).map(|surface| {
            publish_surface_lifecycle_event(
                "surface.focused",
                &workspace.id,
                &surface,
                serde_json::json!({ "origin": "pane.focus" }),
            );
            pane_create_response_payload(&workspace.id, &workspace.name, surface)
        })
    }
    .ok_or_else(|| BridgeError::not_found("pane not found"))?;

    record_previous_pane_if_changed(state, &workspace_id, pane_id, previous_pane_id);

    Ok(result)
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[allow(dead_code)]
pub(crate) enum PaneCreateDirection {
    Left,
    Right,
    Up,
    Down,
}

impl PaneCreateDirection {
    #[allow(dead_code)]
    pub(crate) fn from_str(raw: &str) -> Option<Self> {
        match raw {
            "left" => Some(Self::Left),
            "right" => Some(Self::Right),
            "up" => Some(Self::Up),
            "down" => Some(Self::Down),
            _ => None,
        }
    }
}

impl From<BridgePaneCreateDirection> for PaneCreateDirection {
    fn from(direction: BridgePaneCreateDirection) -> Self {
        match direction {
            BridgePaneCreateDirection::Left => Self::Left,
            BridgePaneCreateDirection::Right => Self::Right,
            BridgePaneCreateDirection::Up => Self::Up,
            BridgePaneCreateDirection::Down => Self::Down,
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct PaneCreateSplitPlacement {
    pub(crate) orientation: gtk::Orientation,
    pub(crate) new_pane_first: bool,
}

#[derive(Clone, Debug, Eq, PartialEq)]
#[allow(dead_code)]
pub(crate) enum PaneCreateTargetError {
    WorkspaceNotFound,
    InvalidSurfaceId(String),
    InvalidPaneId(u32),
    NoPanes,
}

#[allow(dead_code)]
pub(crate) struct ResolvedPaneCreateTarget {
    pub(crate) workspace_id: String,
    pub(crate) pane_id: u32,
    pub(crate) pane_widget: gtk::Widget,
    pub(crate) placement: PaneCreateSplitPlacement,
}

fn pane_create_split_placement(direction: PaneCreateDirection) -> PaneCreateSplitPlacement {
    match direction {
        PaneCreateDirection::Left => PaneCreateSplitPlacement {
            orientation: gtk::Orientation::Horizontal,
            new_pane_first: true,
        },
        PaneCreateDirection::Right => PaneCreateSplitPlacement {
            orientation: gtk::Orientation::Horizontal,
            new_pane_first: false,
        },
        PaneCreateDirection::Up => PaneCreateSplitPlacement {
            orientation: gtk::Orientation::Vertical,
            new_pane_first: true,
        },
        PaneCreateDirection::Down => PaneCreateSplitPlacement {
            orientation: gtk::Orientation::Vertical,
            new_pane_first: false,
        },
    }
}

fn normalize_surface_handle(raw: &str) -> &str {
    let trimmed = raw.trim();
    trimmed
        .strip_prefix("surface:")
        .or_else(|| trimmed.strip_prefix("tab:"))
        .unwrap_or(trimmed)
}

fn surface_hint_matches(surface_id: &str, hint: &str) -> bool {
    let normalized = normalize_surface_handle(hint);
    surface_id == normalized
        || surface_id
            .rsplit_once(':')
            .is_some_and(|(_, tab_id)| tab_id == normalized)
}

fn resolve_pane_create_source_id(
    surface_id: Option<&str>,
    pane_id: Option<u32>,
    focused_pane_id: Option<u32>,
    target_workspace_is_active: bool,
    pane_ids: &[u32],
    surface_to_pane: &[(&str, u32)],
) -> Result<u32, PaneCreateTargetError> {
    if pane_ids.is_empty() {
        return Err(PaneCreateTargetError::NoPanes);
    }

    if let Some(surface_id) = surface_id {
        let requested = normalize_surface_handle(surface_id);
        return surface_to_pane
            .iter()
            .find(|(known_surface_id, _)| *known_surface_id == requested)
            .map(|(_, pane_id)| *pane_id)
            .ok_or_else(|| PaneCreateTargetError::InvalidSurfaceId(surface_id.to_string()));
    }

    if let Some(pane_id) = pane_id {
        if pane_ids.contains(&pane_id) {
            return Ok(pane_id);
        }
        return Err(PaneCreateTargetError::InvalidPaneId(pane_id));
    }

    if target_workspace_is_active {
        if let Some(focused_pane_id) = focused_pane_id {
            if pane_ids.contains(&focused_pane_id) {
                return Ok(focused_pane_id);
            }
        }
    }

    pane_ids
        .first()
        .copied()
        .ok_or(PaneCreateTargetError::NoPanes)
}

fn pane_create_target_error(error: PaneCreateTargetError) -> BridgeError {
    match error {
        PaneCreateTargetError::WorkspaceNotFound => BridgeError::not_found("workspace not found"),
        PaneCreateTargetError::InvalidSurfaceId(_) => BridgeError::not_found("surface not found"),
        PaneCreateTargetError::InvalidPaneId(_) => BridgeError::not_found("pane not found"),
        PaneCreateTargetError::NoPanes => BridgeError::not_found("pane not found"),
    }
}

#[allow(dead_code)]
pub(crate) fn resolve_pane_create_target(
    state: &State,
    target: &WorkspaceTarget,
    surface_id: Option<&str>,
    pane_id: Option<u32>,
    direction: PaneCreateDirection,
) -> Result<ResolvedPaneCreateTarget, PaneCreateTargetError> {
    let (workspace_id, workspace_root, target_workspace_is_active) = {
        let app_state = state.borrow();
        let workspace_index = workspace_index_for_target(&app_state, target)
            .ok_or(PaneCreateTargetError::WorkspaceNotFound)?;
        let workspace = &app_state.workspaces[workspace_index];
        (
            workspace.id.clone(),
            workspace.root.clone(),
            workspace_index == app_state.active_idx,
        )
    };

    let pane_summaries = pane::pane_summaries_for_root(&workspace_root);
    let pane_ids = pane_summaries
        .iter()
        .map(|summary| summary.pane_id)
        .collect::<Vec<_>>();
    let surface_summaries = pane::surface_summaries_for_root(&workspace_root);
    let surface_to_pane = surface_summaries
        .iter()
        .map(|surface| (surface.surface_id.as_str(), surface.pane_id))
        .collect::<Vec<_>>();
    let focused_pane_id = target_workspace_is_active
        .then(|| focused_ids_for_workspace(state, &workspace_id).0)
        .flatten();

    let pane_id = resolve_pane_create_source_id(
        surface_id,
        pane_id,
        focused_pane_id,
        target_workspace_is_active,
        &pane_ids,
        &surface_to_pane,
    )?;
    let pane_widget = pane::pane_widget_for_root(&workspace_root, pane_id)
        .ok_or(PaneCreateTargetError::InvalidPaneId(pane_id))?;

    Ok(ResolvedPaneCreateTarget {
        workspace_id,
        pane_id,
        pane_widget,
        placement: pane_create_split_placement(direction),
    })
}

fn pane_list_payload(state: &State, workspace: &Workspace) -> serde_json::Value {
    let (focused_pane_id, _) = focused_ids_for_workspace(state, &workspace.id);
    let panes = pane::pane_summaries_for_root(&workspace.root)
        .into_iter()
        .enumerate()
        .map(|(index, pane)| {
            let mut row = serde_json::Map::new();
            row.insert(
                "pane_id".to_string(),
                serde_json::Value::String(pane.pane_id.to_string()),
            );
            row.insert(
                "pane_ref".to_string(),
                serde_json::Value::String(pane_ref(pane.pane_id)),
            );
            row.insert("index".to_string(), serde_json::json!(index));
            row.insert(
                "surface_count".to_string(),
                serde_json::json!(pane.surface_count),
            );
            let focused = focused_pane_id == Some(pane.pane_id);
            row.insert("focused".to_string(), serde_json::Value::Bool(focused));
            row.insert("selected".to_string(), serde_json::Value::Bool(focused));
            if let Some(health) = pane.active_terminal_health {
                row.insert("columns".to_string(), serde_json::json!(health.columns));
                row.insert("rows".to_string(), serde_json::json!(health.rows));
                row.insert("width_px".to_string(), serde_json::json!(health.width_px));
                row.insert("height_px".to_string(), serde_json::json!(health.height_px));
                row.insert(
                    "cell_width_px".to_string(),
                    serde_json::json!(health.cell_width_px),
                );
                row.insert(
                    "cell_height_px".to_string(),
                    serde_json::json!(health.cell_height_px),
                );
            }
            if let Some(surface_id) = pane.active_surface_id {
                row.insert(
                    "surface_id".to_string(),
                    serde_json::Value::String(surface_id.clone()),
                );
                row.insert(
                    "surface_ref".to_string(),
                    serde_json::Value::String(surface_ref(&surface_id)),
                );
            }
            serde_json::Value::Object(row)
        })
        .collect::<Vec<_>>();
    serde_json::json!({ "panes": panes })
}

// purpose: Report the current GTK host window in the CMUX window.list shape.
// inputs: Live app state.
// returns/effects: Returns one focused window row for the single GTK host window.
fn window_list_payload(app_state: &AppState) -> serde_json::Value {
    let pane_count = app_state
        .workspaces
        .iter()
        .map(|workspace| pane::pane_summaries_for_root(&workspace.root).len())
        .sum::<usize>();
    serde_json::json!({
        "windows": [{
            "index": 1,
            "id": "window:1",
            "ref": "window:1",
            "window_id": "window:1",
            "window_ref": "window:1",
            "title": "Limux",
            "focused": true,
            "workspace_count": app_state.workspaces.len(),
            "pane_count": pane_count,
        }]
    })
}

// purpose: Validate a system.tree window scope against the current single GTK host window.
// inputs: Optional CMUX window id/ref/index string.
// returns/effects: Returns not_found for unsupported live host windows.
fn validate_system_tree_window(window_id: Option<&str>) -> Result<(), BridgeError> {
    let Some(window_id) = window_id else {
        return Ok(());
    };
    match window_id.strip_prefix("window:").unwrap_or(window_id) {
        "1" => Ok(()),
        _ => Err(BridgeError::not_found("window not found")),
    }
}

// purpose: Build one workspace node for system.tree from live host state.
// inputs: Shared app state and a workspace.
// returns/effects: Attaches pane and surface child nodes under the workspace row.
fn system_tree_workspace_node(
    state: &State,
    app_state: &AppState,
    index: usize,
    workspace: &Workspace,
) -> serde_json::Value {
    let mut workspace_node = workspace_row(index, app_state.active_idx, workspace);
    let surfaces = surface_list_payload(state, workspace, None)
        .get("surfaces")
        .and_then(serde_json::Value::as_array)
        .cloned()
        .unwrap_or_default();
    let pane_nodes = pane_list_payload(state, workspace)
        .get("panes")
        .and_then(serde_json::Value::as_array)
        .cloned()
        .unwrap_or_default()
        .into_iter()
        .map(|pane| system_tree_pane_node(pane, &surfaces))
        .collect::<Vec<_>>();
    if let Some(map) = workspace_node.as_object_mut() {
        map.insert("panes".to_string(), serde_json::Value::Array(pane_nodes));
    }
    workspace_node
}

// purpose: Attach matching surface child nodes to one system.tree pane row.
// inputs: Pane row and all surface rows for the containing workspace.
// returns/effects: Returns the pane row with a surfaces array.
fn system_tree_pane_node(
    mut pane: serde_json::Value,
    surfaces: &[serde_json::Value],
) -> serde_json::Value {
    let pane_handle = pane
        .get("pane_id")
        .or_else(|| pane.get("pane_ref"))
        .and_then(serde_json::Value::as_str)
        .unwrap_or_default()
        .to_string();
    let child_surfaces = surfaces
        .iter()
        .filter(|surface| {
            surface
                .get("pane_id")
                .or_else(|| surface.get("pane_ref"))
                .and_then(serde_json::Value::as_str)
                .is_some_and(|value| value == pane_handle)
        })
        .cloned()
        .collect::<Vec<_>>();
    if let Some(map) = pane.as_object_mut() {
        map.insert(
            "surfaces".to_string(),
            serde_json::Value::Array(child_surfaces),
        );
    }
    pane
}

// purpose: Build native CMUX system.tree output from live GTK host state.
// inputs: Optional workspace and window scopes plus all-window flag.
// returns/effects: Returns a single-window topology snapshot or a loud scope error.
fn system_tree_payload(
    state: &State,
    workspace_target: Option<&WorkspaceTarget>,
    window_id: Option<&str>,
    _include_all: bool,
) -> Result<serde_json::Value, BridgeError> {
    validate_system_tree_window(window_id)?;
    let app_state = state.borrow();
    let workspace_indexes = if let Some(target) = workspace_target {
        vec![workspace_index_for_target(&app_state, target)
            .ok_or_else(|| BridgeError::not_found("workspace not found"))?]
    } else {
        (0..app_state.workspaces.len()).collect::<Vec<_>>()
    };
    let workspaces = workspace_indexes
        .into_iter()
        .map(|index| {
            system_tree_workspace_node(state, &app_state, index, &app_state.workspaces[index])
        })
        .collect::<Vec<_>>();
    let mut window = window_list_payload(&app_state)["windows"][0].clone();
    if let Some(map) = window.as_object_mut() {
        map.insert(
            "workspaces".to_string(),
            serde_json::Value::Array(workspaces),
        );
    }
    Ok(serde_json::json!({
        "source": "limux_live_system_tree",
        "windows": [window],
    }))
}

struct SystemTopPayloadRequest<'a> {
    top_group_limit: usize,
    sample_ms: u64,
    workspace_target: Option<&'a WorkspaceTarget>,
    window_id: Option<&'a str>,
    include_all: bool,
}

// purpose: Build native CMUX system.top output from live process diagnostics.
// inputs: State and scoped top request fields.
// returns/effects: Returns scoped process diagnostics or a loud scope error.
fn system_top_payload(
    state: &State,
    request: SystemTopPayloadRequest<'_>,
) -> Result<serde_json::Value, BridgeError> {
    validate_system_tree_window(request.window_id)?;
    let workspace_id = if let Some(target) = request.workspace_target {
        let app_state = state.borrow();
        let index = workspace_index_for_target(&app_state, target)
            .ok_or_else(|| BridgeError::not_found("workspace not found"))?;
        Some(app_state.workspaces[index].id.clone())
    } else {
        None
    };
    let mut payload = crate::memory_diagnostics::sampled_top_diagnostic_payload(
        request.top_group_limit,
        workspace_id.as_deref(),
        request.sample_ms,
    )
    .map_err(BridgeError::internal)?;
    if let Some(top) = payload.get("top_diagnostic").cloned() {
        payload["memory_diagnostic"] = top;
    }
    payload["all"] = serde_json::json!(request.include_all);
    payload["source"] = serde_json::Value::String("limux_system_top".to_string());
    Ok(payload)
}

fn surface_list_payload(
    state: &State,
    workspace: &Workspace,
    pane_filter: Option<u32>,
) -> serde_json::Value {
    let (_, focused_surface_id) = focused_ids_for_workspace(state, &workspace.id);
    let surfaces = pane::surface_summaries_for_root(&workspace.root)
        .into_iter()
        .filter(|surface| pane_filter.is_none_or(|pane_id| surface.pane_id == pane_id))
        .enumerate()
        .map(|(index, surface)| {
            let mut row = serde_json::Map::new();
            row.insert(
                "surface_id".to_string(),
                serde_json::Value::String(surface.surface_id.clone()),
            );
            row.insert(
                "surface_ref".to_string(),
                serde_json::Value::String(surface_ref(&surface.surface_id)),
            );
            row.insert(
                "pane_id".to_string(),
                serde_json::Value::String(surface.pane_id.to_string()),
            );
            row.insert(
                "pane_ref".to_string(),
                serde_json::Value::String(pane_ref(surface.pane_id)),
            );
            row.insert("index".to_string(), serde_json::json!(index));
            row.insert(
                "title".to_string(),
                serde_json::Value::String(surface.title.clone()),
            );
            row.insert(
                "type".to_string(),
                serde_json::Value::String(surface.kind.clone()),
            );
            row.insert(
                "selected".to_string(),
                serde_json::Value::Bool(surface.selected),
            );
            row.insert(
                "focused".to_string(),
                serde_json::Value::Bool(
                    focused_surface_id.as_deref() == Some(surface.surface_id.as_str()),
                ),
            );
            if let Some(cwd) = surface.cwd.filter(|cwd| !cwd.is_empty()) {
                row.insert("cwd".to_string(), serde_json::Value::String(cwd));
            }
            if let Some(uri) = surface.uri.filter(|uri| !uri.is_empty()) {
                row.insert("uri".to_string(), serde_json::Value::String(uri));
            }
            serde_json::Value::Object(row)
        })
        .collect::<Vec<_>>();
    serde_json::json!({ "surfaces": surfaces })
}

// purpose: Render CMUX-style browser tab rows from live Limux browser surfaces.
// inputs: Focused surface id and browser-only surface summaries from one pane.
// returns/effects: Returns JSON rows without mutating GTK state.
fn browser_tab_list_payload(
    focused_surface_id: Option<String>,
    tabs: Vec<pane::SurfaceSummary>,
) -> serde_json::Value {
    let current_surface_id = tabs
        .iter()
        .find(|tab| tab.selected)
        .or_else(|| tabs.first())
        .map(|tab| tab.surface_id.clone());
    let rows = tabs
        .into_iter()
        .enumerate()
        .map(|(index, tab)| {
            let focused = focused_surface_id.as_deref() == Some(tab.surface_id.as_str());
            let surface_id = tab.surface_id;
            let uri = tab.uri.unwrap_or_default();
            serde_json::json!({
                "id": surface_id.clone(),
                "ref": surface_ref(&surface_id),
                "surface_id": surface_id.clone(),
                "surface_ref": surface_ref(&surface_id),
                "pane_id": tab.pane_id.to_string(),
                "pane_ref": pane_ref(tab.pane_id),
                "index": index,
                "title": tab.title,
                "url": uri.clone(),
                "uri": uri,
                "selected": tab.selected,
                "focused": focused,
            })
        })
        .collect::<Vec<_>>();

    let mut payload = serde_json::json!({ "tabs": rows });
    if let Some(surface_id) = current_surface_id {
        payload["current_surface_id"] = serde_json::Value::String(surface_id.clone());
        payload["current_surface_ref"] = serde_json::Value::String(surface_ref(&surface_id));
    }
    payload
}

fn surface_health_row(
    state: &State,
    workspace: &Workspace,
    index: usize,
    surface: pane::SurfaceSummary,
) -> serde_json::Value {
    let (_, focused_surface_id) = focused_ids_for_workspace(state, &workspace.id);
    let mut row = serde_json::Map::new();
    row.insert("index".to_string(), serde_json::json!(index));
    row.insert(
        "id".to_string(),
        serde_json::Value::String(surface.surface_id.clone()),
    );
    row.insert(
        "ref".to_string(),
        serde_json::Value::String(surface_ref(&surface.surface_id)),
    );
    row.insert(
        "surface_id".to_string(),
        serde_json::Value::String(surface.surface_id.clone()),
    );
    row.insert(
        "surface_ref".to_string(),
        serde_json::Value::String(surface_ref(&surface.surface_id)),
    );
    row.insert(
        "pane_id".to_string(),
        serde_json::Value::String(surface.pane_id.to_string()),
    );
    row.insert(
        "pane_ref".to_string(),
        serde_json::Value::String(pane_ref(surface.pane_id)),
    );
    row.insert(
        "type".to_string(),
        serde_json::Value::String(surface.kind.clone()),
    );
    let focused = focused_surface_id.as_deref() == Some(surface.surface_id.as_str());
    row.insert("focused".to_string(), serde_json::Value::Bool(focused));
    row.insert(
        "selected".to_string(),
        serde_json::Value::Bool(surface.selected),
    );
    row.insert("in_window".to_string(), serde_json::Value::Bool(true));
    row.insert("hidden".to_string(), serde_json::Value::Bool(false));

    if surface.kind == "terminal" {
        if let Some((_surface_id, handle)) =
            pane::terminal_handle_for_root(&workspace.root, Some(&surface.surface_id))
        {
            let health = handle.health();
            row.insert(
                "healthy".to_string(),
                serde_json::Value::Bool(health.realized && !health.process_exited),
            );
            row.insert(
                "realized".to_string(),
                serde_json::Value::Bool(health.realized),
            );
            row.insert(
                "process_exited".to_string(),
                serde_json::Value::Bool(health.process_exited),
            );
            row.insert("columns".to_string(), serde_json::json!(health.columns));
            row.insert("rows".to_string(), serde_json::json!(health.rows));
            row.insert("width_px".to_string(), serde_json::json!(health.width_px));
            row.insert("height_px".to_string(), serde_json::json!(health.height_px));
            row.insert(
                "cell_width_px".to_string(),
                serde_json::json!(health.cell_width_px),
            );
            row.insert(
                "cell_height_px".to_string(),
                serde_json::json!(health.cell_height_px),
            );
        } else {
            row.insert("healthy".to_string(), serde_json::Value::Bool(false));
            row.insert("realized".to_string(), serde_json::Value::Bool(false));
            row.insert("process_exited".to_string(), serde_json::Value::Bool(false));
        }
    } else {
        row.insert("healthy".to_string(), serde_json::Value::Bool(true));
        row.insert("realized".to_string(), serde_json::Value::Bool(true));
        row.insert("process_exited".to_string(), serde_json::Value::Bool(false));
    }

    serde_json::Value::Object(row)
}

fn surface_health_payload(
    state: &State,
    workspace: &Workspace,
    surface_hint: Option<&str>,
) -> Result<serde_json::Value, BridgeError> {
    let requested = surface_hint.map(normalize_surface_handle);
    let surfaces = pane::surface_summaries_for_root(&workspace.root)
        .into_iter()
        .filter(|surface| requested.is_none_or(|requested| surface.surface_id == requested))
        .enumerate()
        .map(|(index, surface)| surface_health_row(state, workspace, index, surface))
        .collect::<Vec<_>>();

    if surface_hint.is_some() && surfaces.is_empty() {
        return Err(BridgeError::not_found("surface not found"));
    }

    Ok(serde_json::json!({ "surfaces": surfaces }))
}

#[derive(Clone)]
struct WorkspaceSeedSource {
    workspace_cwd: Option<String>,
    workspace_folder_path: Option<String>,
}

#[derive(Clone)]
struct TabDragWorkspaceSeed {
    name: String,
    cwd: Option<String>,
    folder_path: Option<String>,
}

pub(crate) type State = Rc<RefCell<AppState>>;
type SettingsConfigChangedHandler = dyn Fn(&app_config::AppConfig, &app_config::AppConfig);
thread_local! {
    static CONTROL_STATE: RefCell<Option<State>> = const { RefCell::new(None) };
}
const SPLIT_RATIO_STATE_KEY: &str = "limux-split-ratio-state";
const PORTAL_DESKTOP_SERVICE: &str = "org.freedesktop.portal.Desktop";
const PORTAL_DESKTOP_PATH: &str = "/org/freedesktop/portal/desktop";
const PORTAL_SETTINGS_INTERFACE: &str = "org.freedesktop.portal.Settings";
const PORTAL_APPEARANCE_NAMESPACE: &str = "org.freedesktop.appearance";
const PORTAL_COLOR_SCHEME_KEY: &str = "color-scheme";
const FREEDESKTOP_NOTIFICATIONS_SERVICE: &str = "org.freedesktop.Notifications";
const FREEDESKTOP_NOTIFICATIONS_PATH: &str = "/org/freedesktop/Notifications";
const FREEDESKTOP_NOTIFICATIONS_INTERFACE: &str = "org.freedesktop.Notifications";
const GNOME_INTERFACE_SCHEMA: &str = "org.gnome.desktop.interface";
const GNOME_COLOR_SCHEME_KEY: &str = "color-scheme";
const DESKTOP_NOTIFICATION_DBUS_TIMEOUT_MS: i32 = 1_000;
const DESKTOP_NOTIFICATION_EXPIRE_TIMEOUT_MS: i32 = 10_000;
const PORTAL_THEME_READ_TIMEOUT_MS: i32 = 500;

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
enum PortalColorSchemePreference {
    #[default]
    Unknown,
    Default,
    Dark,
    Light,
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct DesktopNotificationTarget {
    workspace_id: String,
    pane_id: Option<u32>,
    tab_id: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct DesktopNotificationRoute {
    target: DesktopNotificationTarget,
    activation_token: Option<String>,
    feed_actions: HashMap<String, crate::feed::FeedNotificationDecision>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct DesktopNotificationRequest {
    summary: String,
    body: String,
    sound: app_config::NotificationSound,
    custom_sound_file_path: String,
    target: DesktopNotificationTarget,
    feed_actions: Vec<crate::feed::FeedNotificationAction>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct NotificationPolicyEffects {
    record: bool,
    mark_unread: bool,
    desktop: bool,
    sound: bool,
    command: bool,
    pane_flash: bool,
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct NotificationPolicyContext {
    workspace_id: String,
    surface_id: Option<String>,
    cwd: Option<String>,
    title: String,
    subtitle: String,
    body: String,
    app_focused: bool,
    focused_panel: bool,
}

impl Default for NotificationPolicyEffects {
    fn default() -> Self {
        Self {
            record: true,
            mark_unread: true,
            desktop: true,
            sound: true,
            command: true,
            pane_flash: true,
        }
    }
}

impl PortalColorSchemePreference {
    fn from_raw(raw: u32) -> Option<Self> {
        match raw {
            0 => Some(Self::Default),
            1 => Some(Self::Dark),
            2 => Some(Self::Light),
            _ => None,
        }
    }

    fn resolved(self, gnome_prefers_dark: Option<bool>) -> Option<bool> {
        match self {
            Self::Dark => Some(true),
            Self::Light => Some(false),
            Self::Default | Self::Unknown => gnome_prefers_dark,
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum SessionSaveRequest {
    Ignore,
    RetryOnIdle,
    FlushOnIdle,
}

trait SessionSaveAccess {
    fn persistence_suspended(&self) -> bool;
    fn save_queued(&self) -> bool;
    fn set_save_queued(&mut self, queued: bool);
}

impl SessionSaveAccess for AppState {
    fn persistence_suspended(&self) -> bool {
        self.persistence_suspended
    }

    fn save_queued(&self) -> bool {
        self.save_queued
    }

    fn set_save_queued(&mut self, queued: bool) {
        self.save_queued = queued;
    }
}

fn queue_session_save_request<T: SessionSaveAccess>(state: &Rc<RefCell<T>>) -> SessionSaveRequest {
    let Ok(mut s) = state.try_borrow_mut() else {
        return SessionSaveRequest::RetryOnIdle;
    };

    if s.persistence_suspended() || s.save_queued() {
        SessionSaveRequest::Ignore
    } else {
        s.set_save_queued(true);
        SessionSaveRequest::FlushOnIdle
    }
}

fn request_session_save(state: &State) {
    match queue_session_save_request(state) {
        SessionSaveRequest::Ignore => {}
        SessionSaveRequest::RetryOnIdle => {
            let state = state.clone();
            glib::idle_add_local_once(move || {
                request_session_save(&state);
            });
        }
        SessionSaveRequest::FlushOnIdle => {
            let state = state.clone();
            glib::idle_add_local_once(move || {
                let should_save = {
                    let mut s = state.borrow_mut();
                    let should_save = s.save_queued && !s.persistence_suspended;
                    s.save_queued = false;
                    should_save
                };
                if should_save {
                    save_session_now(&state);
                }
            });
        }
    }
}

fn save_session_now(state: &State) {
    let session = snapshot_session_state(state);
    if let Err(err) = layout_state::save_session_atomic(&session) {
        eprintln!("limux: failed to save session state: {err}");
    }
}

fn suspend_persistence(state: &State, suspended: bool) {
    state.borrow_mut().persistence_suspended = suspended;
}

fn apply_loaded_session(state: &State, mut loaded: LoadedSession) {
    suspend_persistence(state, true);

    apply_top_bar_state_immediately(state, loaded.state.top_bar_visible);

    let restored_any = !loaded.state.workspaces.is_empty();
    {
        state.borrow_mut().workspace_groups = loaded.state.workspace_groups.clone();
    }
    if restored_any {
        let restorable_agents = layout_state::RestorableAgentIndex::load();
        let auto_resume_agent_sessions = state
            .borrow()
            .config
            .borrow()
            .terminal
            .auto_resume_agent_sessions;
        for workspace in &mut loaded.state.workspaces {
            layout_state::attach_restorable_agents_to_layout(
                &mut workspace.layout,
                workspace.id.as_deref().unwrap_or(""),
                &restorable_agents,
                auto_resume_agent_sessions,
            );
        }
        for workspace in &loaded.state.workspaces {
            add_workspace_from_state(state, workspace);
        }
        restore_active_workspace(state, loaded.state.active_workspace_index);
        apply_sidebar_state_immediately(state, &loaded.state.sidebar);
    }

    suspend_persistence(state, false);

    if restored_any || matches!(loaded.source, layout_state::SessionLoadSource::Legacy) {
        save_session_now(state);
    }
}

fn restore_active_workspace(state: &State, index: usize) {
    let maybe_row = {
        let s = state.borrow();
        if s.workspaces.is_empty() {
            None
        } else {
            let clamped = index.min(s.workspaces.len() - 1);
            Some((
                clamped,
                s.workspaces[clamped].sidebar_row.clone(),
                s.sidebar_list.clone(),
            ))
        }
    };

    if let Some((index, row, sidebar_list)) = maybe_row {
        switch_workspace(state, index);
        sidebar_list.select_row(Some(&row));
    }
}

fn apply_sidebar_state_immediately(state: &State, sidebar_state: &layout_state::SidebarState) {
    let (sidebar_shell, sidebar_handle, width) = {
        let mut s = state.borrow_mut();
        s.sidebar_expanded_width = sidebar_state.width.max(SIDEBAR_WIDTH);
        (
            s.sidebar_shell.clone(),
            s.sidebar_handle.clone(),
            s.sidebar_expanded_width,
        )
    };

    // Apply restored sidebar visibility directly; using the animated toggle path during
    // startup would create flicker and extra persistence churn while restore is suspended.
    set_sidebar_state_widgets(
        &sidebar_shell,
        &sidebar_handle,
        if sidebar_state.visible { width } else { 0 },
        sidebar_state.visible,
    );
}

fn apply_top_bar_state_immediately(state: &State, visible: bool) {
    state.borrow_mut().top_bar_visible = visible;
    sync_top_bar_visibility(state);
}

fn snapshot_session_state(state: &State) -> AppSessionState {
    let s = state.borrow();
    let restorable_agents = layout_state::RestorableAgentIndex::load();
    let sidebar_visible = sidebar_is_visible(&s);
    let sidebar_width = if sidebar_visible {
        sidebar_width(&s.sidebar_shell)
    } else {
        s.sidebar_expanded_width
    }
    .max(SIDEBAR_WIDTH);

    let workspaces = s
        .workspaces
        .iter()
        .map(|workspace| {
            let cwd = workspace.cwd.borrow().clone();
            let folder_path = workspace.folder_path.clone();
            let working_directory = folder_path.clone().or(cwd.clone());
            let mut layout = workspace
                .split_container
                .tree()
                .snapshot(working_directory.as_deref());
            layout_state::attach_restorable_agents_to_layout(
                &mut layout,
                &workspace.id,
                &restorable_agents,
                s.config.borrow().terminal.auto_resume_agent_sessions,
            );
            WorkspaceState {
                id: Some(workspace.id.clone()),
                name: workspace.name.clone(),
                description: workspace.description.clone(),
                favorite: workspace.favorite,
                cwd,
                folder_path,
                group_id: workspace.group_id.clone(),
                environment: workspace.environment.clone(),
                layout,
            }
        })
        .collect();

    layout_state::normalize_session(AppSessionState {
        version: layout_state::SESSION_VERSION,
        active_workspace_index: s.active_idx,
        top_bar_visible: s.top_bar_visible,
        sidebar: layout_state::SidebarState {
            visible: sidebar_visible,
            width: sidebar_width,
        },
        workspaces,
        workspace_groups: s.workspace_groups.clone(),
    })
}

fn sidebar_is_visible(state: &AppState) -> bool {
    state.sidebar_shell.is_visible() && sidebar_width(&state.sidebar_shell) > 10
}

/// purpose: Resolve a right-sidebar workspace target against live host workspaces.
/// inputs: Host state and optional CMUX right-sidebar workspace/window selectors.
/// returns/effects: Confirms the target exists or returns not_found for explicit misses.
fn validate_right_sidebar_target(
    state: &AppState,
    target: &RightSidebarTarget,
) -> Result<Option<String>, BridgeError> {
    if target.window_id.is_some() {
        // Limux currently has one GTK host window; full multi-window routing remains separate.
    }
    let Some(workspace_id) = target.workspace_id.as_deref() else {
        return Ok(state
            .active_workspace()
            .map(|workspace| workspace.id.clone()));
    };
    let matched = state.workspaces.iter().find(|workspace| {
        workspace.id == workspace_id || workspace_ref(&workspace.id) == workspace_id
    });
    matched
        .map(|workspace| Some(workspace.id.clone()))
        .ok_or_else(|| BridgeError::not_found("right sidebar workspace target not found"))
}

/// purpose: Render the current CMUX right-sidebar state as JSON.
/// inputs: Host state and resolved workspace id, if known.
/// returns/effects: Returns visible/mode/focused metadata for CLI/API reads.
fn right_sidebar_state_payload(
    state: &AppState,
    workspace_id: Option<String>,
) -> serde_json::Value {
    serde_json::json!({
        "visible": state.right_sidebar_visible,
        "mode": state.right_sidebar_mode.as_str(),
        "focused": state.right_sidebar_focused,
        "workspace_id": workspace_id,
        "supported_modes": ["files", "find", "vault", "sessions", "feed", "dock"],
    })
}

/// purpose: Apply one CMUX right-sidebar visibility, focus, or mode action.
/// inputs: Live host state plus parsed CMUX right-sidebar action and target.
/// returns/effects: Mutates host-owned state and returns a CMUX-shaped payload.
fn apply_right_sidebar_action(
    state: &State,
    action: RightSidebarAction,
    target: RightSidebarTarget,
) -> Result<serde_json::Value, BridgeError> {
    let mut app_state = state.borrow_mut();
    let workspace_id = validate_right_sidebar_target(&app_state, &target)?;
    match action {
        RightSidebarAction::Toggle => {
            app_state.right_sidebar_visible = !app_state.right_sidebar_visible;
            if !app_state.right_sidebar_visible {
                app_state.right_sidebar_focused = false;
            }
        }
        RightSidebarAction::Show => {
            app_state.right_sidebar_visible = true;
        }
        RightSidebarAction::Hide => {
            app_state.right_sidebar_visible = false;
            app_state.right_sidebar_focused = false;
        }
        RightSidebarAction::Focus => {
            app_state.right_sidebar_visible = true;
            app_state.right_sidebar_focused = true;
        }
        RightSidebarAction::SetMode { mode, focus } => {
            app_state.right_sidebar_visible = true;
            app_state.right_sidebar_mode = mode;
            app_state.right_sidebar_focused = focus;
        }
        RightSidebarAction::GetState => {}
    }
    sync_right_sidebar_panel(&mut app_state);
    Ok(right_sidebar_state_payload(&app_state, workspace_id))
}

/// purpose: Convert right-sidebar mode to the visible panel heading.
/// inputs: CMUX right-sidebar mode.
/// returns/effects: Returns a human-readable label without mutating state.
fn right_sidebar_mode_title(mode: &RightSidebarMode) -> &'static str {
    match mode {
        RightSidebarMode::Files => "Files",
        RightSidebarMode::Find => "Find",
        RightSidebarMode::Vault => "Vault",
        RightSidebarMode::Sessions => "Sessions",
        RightSidebarMode::Feed => "Feed",
        RightSidebarMode::Dock => "Dock",
    }
}

/// purpose: Describe what the current right-sidebar panel can render.
/// inputs: CMUX right-sidebar mode.
/// returns/effects: Returns mode-specific visible text.
fn right_sidebar_mode_description(mode: &RightSidebarMode) -> &'static str {
    match mode {
        RightSidebarMode::Files => "Workspace files",
        RightSidebarMode::Find => "Workspace find",
        RightSidebarMode::Vault => "Vault",
        RightSidebarMode::Sessions => "Session diagnostics",
        RightSidebarMode::Feed => "Feed",
        RightSidebarMode::Dock => "Dock",
    }
}

/// purpose: Synchronize the visible right sidebar with host-owned CMUX metadata.
/// inputs: Mutable app state containing widgets and active workspace metadata.
/// returns/effects: Rebuilds the right-sidebar body and toggles shell visibility.
fn sync_right_sidebar_panel(state: &mut AppState) {
    state
        .right_sidebar_shell
        .set_visible(state.right_sidebar_visible);
    state
        .right_sidebar_title_label
        .set_label(&right_sidebar_panel_title(state));
    clear_right_sidebar_body(&state.right_sidebar_body);
    let Some(workspace) = state.active_workspace() else {
        append_right_sidebar_muted(&state.right_sidebar_body, "No workspace");
        return;
    };
    append_right_sidebar_section(&state.right_sidebar_body, "Mode");
    append_right_sidebar_row(
        &state.right_sidebar_body,
        &format!(
            "{} - {}",
            right_sidebar_mode_title(&state.right_sidebar_mode),
            right_sidebar_mode_description(&state.right_sidebar_mode)
        ),
    );
    append_right_sidebar_section(&state.right_sidebar_body, "Workspace");
    append_right_sidebar_row(&state.right_sidebar_body, &workspace.name);
    sync_right_sidebar_mode_rows(
        &state.right_sidebar_body,
        workspace,
        &state.right_sidebar_mode,
    );
    sync_right_sidebar_status_rows(&state.right_sidebar_body, workspace);
    sync_right_sidebar_progress_row(&state.right_sidebar_body, workspace);
    sync_right_sidebar_log_rows(&state.right_sidebar_body, workspace);
}

/// purpose: Render mode-specific right-sidebar content from live workspace state.
/// inputs: Body widget, active workspace, and requested CMUX right-sidebar mode.
/// returns/effects: Adds mode-specific preview rows to the right sidebar.
fn sync_right_sidebar_mode_rows(body: &gtk::Box, workspace: &Workspace, mode: &RightSidebarMode) {
    match mode {
        RightSidebarMode::Files => sync_right_sidebar_files_rows(body, workspace),
        RightSidebarMode::Find => sync_right_sidebar_find_rows(body, workspace),
        RightSidebarMode::Vault => sync_right_sidebar_vault_rows(body, workspace),
        RightSidebarMode::Sessions => sync_right_sidebar_sessions_rows(body, workspace),
        RightSidebarMode::Feed => sync_right_sidebar_feed_rows(body),
        RightSidebarMode::Dock => sync_right_sidebar_dock_rows(body, workspace),
    }
}

/// purpose: Build the right-sidebar title from current mode/focus state.
/// inputs: App state with right-sidebar mode and focus metadata.
/// returns/effects: Returns a title string without mutating widgets.
fn right_sidebar_panel_title(state: &AppState) -> String {
    let suffix = if state.right_sidebar_focused {
        " focused"
    } else {
        ""
    };
    format!(
        "{}{}",
        right_sidebar_mode_title(&state.right_sidebar_mode),
        suffix
    )
}

/// purpose: Remove all rendered rows from the right-sidebar body.
/// inputs: GTK box used as the right-sidebar body.
/// returns/effects: Mutates child widgets only.
fn clear_right_sidebar_body(body: &gtk::Box) {
    while let Some(child) = body.first_child() {
        body.remove(&child);
    }
}

/// purpose: Append a right-sidebar section header.
/// inputs: Body widget and section text.
/// returns/effects: Adds one label widget.
fn append_right_sidebar_section(body: &gtk::Box, text: &str) {
    append_right_sidebar_label(body, text, "limux-right-sidebar-section");
}

/// purpose: Append a normal right-sidebar row.
/// inputs: Body widget and row text.
/// returns/effects: Adds one wrapping label widget.
fn append_right_sidebar_row(body: &gtk::Box, text: &str) {
    append_right_sidebar_label(body, text, "limux-right-sidebar-row");
}

/// purpose: Append a muted right-sidebar row.
/// inputs: Body widget and row text.
/// returns/effects: Adds one wrapping label widget.
fn append_right_sidebar_muted(body: &gtk::Box, text: &str) {
    append_right_sidebar_label(body, text, "limux-right-sidebar-muted");
}

/// purpose: Append one styled right-sidebar label.
/// inputs: Body widget, label text, and CSS class.
/// returns/effects: Adds one wrapping label widget.
fn append_right_sidebar_label(body: &gtk::Box, text: &str, css_class: &str) {
    let label = gtk::Label::builder()
        .label(text)
        .xalign(0.0)
        .wrap(true)
        .selectable(true)
        .build();
    label.add_css_class(css_class);
    body.append(&label);
}

/// purpose: Render a bounded file listing for the active workspace folder.
/// inputs: Body widget and active workspace metadata.
/// returns/effects: Reads at most the preview cap plus one directory entry and appends rows.
fn sync_right_sidebar_files_rows(body: &gtk::Box, workspace: &Workspace) {
    append_right_sidebar_section(body, "Files");
    let Some(path) = right_sidebar_workspace_path(workspace) else {
        append_right_sidebar_muted(body, "No workspace folder");
        return;
    };
    append_right_sidebar_row(body, &path);
    append_right_sidebar_result_rows(
        body,
        sidebar_file_preview_lines(Path::new(&path), RIGHT_SIDEBAR_FILE_PREVIEW_LIMIT),
    );
}

/// purpose: Render current search roots and live surfaces for the Find mode panel.
/// inputs: Body widget and active workspace.
/// returns/effects: Adds search-root and surface context rows.
fn sync_right_sidebar_find_rows(body: &gtk::Box, workspace: &Workspace) {
    append_right_sidebar_section(body, "Search root");
    match right_sidebar_workspace_path(workspace) {
        Some(path) => append_right_sidebar_row(body, &path),
        None => append_right_sidebar_muted(body, "No workspace folder"),
    }
    append_right_sidebar_surface_rows(body, workspace, "Open surfaces", false);
}

/// purpose: Render vault-adjacent workspace metadata without exposing secrets.
/// inputs: Body widget and active workspace.
/// returns/effects: Adds status/progress counts used by agent metadata commands.
fn sync_right_sidebar_vault_rows(body: &gtk::Box, workspace: &Workspace) {
    append_right_sidebar_section(body, "Vault metadata");
    append_right_sidebar_row(
        body,
        &format!("status keys: {}", workspace.sidebar_status.len()),
    );
    append_right_sidebar_row(
        body,
        &format!("progress: {}", workspace.sidebar_progress.is_some()),
    );
}

/// purpose: Render pane/session summaries for the active workspace.
/// inputs: Body widget and active workspace.
/// returns/effects: Adds pane rows and a bounded surface list.
fn sync_right_sidebar_sessions_rows(body: &gtk::Box, workspace: &Workspace) {
    append_right_sidebar_section(body, "Panes");
    let panes = pane::pane_summaries_for_root(&workspace.root);
    if panes.is_empty() {
        append_right_sidebar_muted(body, "No panes");
    }
    for pane in panes {
        append_right_sidebar_row(
            body,
            &format!("pane {} - {} surfaces", pane.pane_id, pane.surface_count),
        );
    }
    append_right_sidebar_surface_rows(body, workspace, "Surfaces", false);
}

/// purpose: Render retained Feed items in the right-sidebar Feed mode.
/// inputs: Body widget.
/// returns/effects: Reads the bounded Feed coordinator state and appends rows.
fn sync_right_sidebar_feed_rows(body: &gtk::Box) {
    append_right_sidebar_section(body, "Feed");
    let params = serde_json::Map::new();
    match crate::feed::coordinator().list(&params) {
        Ok(payload) => append_right_sidebar_feed_items(body, &payload),
        Err(error) => append_right_sidebar_muted(body, &format!("Feed unavailable: {error:?}")),
    }
}

/// purpose: Render Feed preview rows plus direct decision controls for pending permissions.
/// inputs: Body widget and Feed list payload.
/// returns/effects: Adds compact rows and GTK buttons that resolve permission requests.
fn append_right_sidebar_feed_items(body: &gtk::Box, payload: &serde_json::Value) {
    let items = sidebar_feed_visible_items(payload, RIGHT_SIDEBAR_FEED_PREVIEW_LIMIT);
    if items.is_empty() {
        append_right_sidebar_muted(body, "No feed items");
        return;
    }
    for item in items {
        append_right_sidebar_row(body, &sidebar_feed_preview_line(item));
        append_feed_decision_actions(body, item);
    }
}

/// purpose: Add direct decision controls for pending Feed rows.
/// inputs: Body widget and one Feed item row.
/// returns/effects: Appends buttons that call the matching feed.*.reply method.
fn append_feed_decision_actions(body: &gtk::Box, item: &serde_json::Value) {
    if let Some(request_id) = pending_permission_request_id(item) {
        append_feed_permission_actions(body, request_id, item);
    } else if let Some(request_id) = pending_exit_plan_request_id(item) {
        append_feed_exit_plan_actions(body, request_id);
    } else if let Some(request_id) = pending_question_request_id(item) {
        append_feed_question_actions(body, request_id, item);
    }
}

/// purpose: Add permission decision buttons for a pending Feed row.
/// inputs: Body widget, request id, and Feed row.
/// returns/effects: Appends buttons that call feed.permission.reply for the request id.
fn append_feed_permission_actions(body: &gtk::Box, request_id: String, item: &serde_json::Value) {
    let source = json_string_field(item, "source").unwrap_or("");
    append_feed_action_buttons(
        body,
        &request_id,
        &crate::feed_actions::permission_action_specs(source, item),
        reply_to_feed_permission_request,
    );
}

/// purpose: Add exit-plan decision buttons for a pending Feed row.
/// inputs: Body widget and request id.
/// returns/effects: Appends buttons that call feed.exit_plan.reply for the request id.
fn append_feed_exit_plan_actions(body: &gtk::Box, request_id: String) {
    append_feed_action_buttons(
        body,
        &request_id,
        feed_exit_plan_action_specs(),
        reply_to_feed_exit_plan_request,
    );
}

/// purpose: Add question decision buttons for a pending Feed row.
/// inputs: Body widget, request id, and question payload.
/// returns/effects: Appends buttons that call feed.question.reply for the request id.
fn append_feed_question_actions(body: &gtk::Box, request_id: String, item: &serde_json::Value) {
    let options = feed_question_action_specs(item);
    if options.is_empty() {
        return;
    }
    let action_row = gtk::Box::builder()
        .orientation(gtk::Orientation::Horizontal)
        .spacing(6)
        .build();
    action_row.add_css_class("limux-right-sidebar-actions");
    for (label, selections) in options {
        let button = gtk::Button::with_label(&label);
        button.add_css_class("flat");
        button.set_tooltip_text(Some(&format!("Answer {request_id}: {label}")));
        let request_id = request_id.clone();
        button.connect_clicked(move |clicked| {
            match reply_to_feed_question_request(&request_id, selections.clone()) {
                Ok(()) => {
                    clicked.set_label("Sent");
                    clicked.set_sensitive(false);
                }
                Err(error) => {
                    clicked.set_tooltip_text(Some(&format!("Feed reply failed: {error:?}")));
                }
            }
        });
        action_row.append(&button);
    }
    body.append(&action_row);
}

/// purpose: Add a row of one-click Feed decision buttons.
/// inputs: Body widget, request id, action specs, and reply function.
/// returns/effects: Appends GTK buttons that call the supplied reply function.
fn append_feed_action_buttons<F>(
    body: &gtk::Box,
    request_id: &str,
    specs: &[(&'static str, &'static str)],
    reply: F,
) where
    F: Fn(&str, &str) -> Result<(), BridgeError> + Copy + 'static,
{
    let action_row = gtk::Box::builder()
        .orientation(gtk::Orientation::Horizontal)
        .spacing(6)
        .build();
    action_row.add_css_class("limux-right-sidebar-actions");
    for (label, mode) in specs {
        let button = gtk::Button::with_label(label);
        button.add_css_class("flat");
        button.set_tooltip_text(Some(&format!("Reply {mode} to {request_id}")));
        let request_id = request_id.to_string();
        let mode = *mode;
        button.connect_clicked(move |clicked| match reply(&request_id, mode) {
            Ok(()) => {
                clicked.set_label("Sent");
                clicked.set_sensitive(false);
            }
            Err(error) => {
                clicked.set_tooltip_text(Some(&format!("Feed reply failed: {error:?}")));
            }
        });
        action_row.append(&button);
    }
    body.append(&action_row);
}

/// purpose: Resolve one Feed permission request through the shared coordinator.
/// inputs: Request id and CMUX permission mode.
/// returns/effects: Mutates Feed state and wakes any blocked feed.push caller.
pub(crate) fn reply_to_feed_permission_request(
    request_id: &str,
    mode: &str,
) -> Result<(), BridgeError> {
    let params = feed_reply_params(request_id, &[("mode", mode)]);
    crate::feed::coordinator()
        .permission_reply(&params)
        .map(|_| ())
}

/// purpose: Resolve one Feed exit-plan request through the shared coordinator.
/// inputs: Request id and CMUX exit-plan mode.
/// returns/effects: Mutates Feed state and wakes any blocked feed.push caller.
pub(crate) fn reply_to_feed_exit_plan_request(
    request_id: &str,
    mode: &str,
) -> Result<(), BridgeError> {
    let params = feed_reply_params(request_id, &[("mode", mode)]);
    crate::feed::coordinator()
        .exit_plan_reply(&params)
        .map(|_| ())
}

/// purpose: Resolve one Feed question request through the shared coordinator.
/// inputs: Request id and selected answers.
/// returns/effects: Mutates Feed state and wakes any blocked feed.push caller.
pub(crate) fn reply_to_feed_question_request(
    request_id: &str,
    selections: Vec<String>,
) -> Result<(), BridgeError> {
    let mut params = serde_json::Map::new();
    params.insert(
        "request_id".to_string(),
        serde_json::Value::String(request_id.to_string()),
    );
    params.insert(
        "selections".to_string(),
        serde_json::Value::Array(
            selections
                .into_iter()
                .map(serde_json::Value::String)
                .collect(),
        ),
    );
    crate::feed::coordinator()
        .question_reply(&params)
        .map(|_| ())
}

/// purpose: Build common Feed reply params with a request id and string fields.
/// inputs: Request id plus additional key/value fields.
/// returns/effects: Returns JSON params without mutating state.
fn feed_reply_params(
    request_id: &str,
    fields: &[(&str, &str)],
) -> serde_json::Map<String, serde_json::Value> {
    let mut params = serde_json::Map::new();
    params.insert(
        "request_id".to_string(),
        serde_json::Value::String(request_id.to_string()),
    );
    for (key, value) in fields {
        params.insert(
            (*key).to_string(),
            serde_json::Value::String((*value).to_string()),
        );
    }
    params
}

/// purpose: Define direct CMUX exit-plan decisions exposed in the Feed sidebar.
/// inputs: None.
/// returns/effects: Returns stable button label and mode pairs.
fn feed_exit_plan_action_specs() -> &'static [(&'static str, &'static str)] {
    &[
        ("Manual", "manual"),
        ("Auto", "autoAccept"),
        ("Bypass", "bypassPermissions"),
        ("Ultraplan", "ultraplan"),
        ("Deny", "deny"),
    ]
}

/// purpose: Render selected and open surfaces for Dock mode.
/// inputs: Body widget and active workspace.
/// returns/effects: Adds selected surface first, then a bounded surface list.
fn sync_right_sidebar_dock_rows(body: &gtk::Box, workspace: &Workspace) {
    append_right_sidebar_surface_rows(body, workspace, "Dock surfaces", true);
}

/// purpose: Append rows from a fallible preview builder.
/// inputs: Body widget and rows/error result.
/// returns/effects: Appends preview rows or an explicit error row.
fn append_right_sidebar_result_rows(body: &gtk::Box, rows: Result<Vec<String>, String>) {
    match rows {
        Ok(rows) => append_right_sidebar_lines(body, rows, "none"),
        Err(message) => append_right_sidebar_muted(body, &message),
    }
}

/// purpose: Append normal rows or one muted empty-state row.
/// inputs: Body widget, preview rows, and empty-state text.
/// returns/effects: Adds rows to the right-sidebar body.
fn append_right_sidebar_lines(body: &gtk::Box, rows: Vec<String>, empty_text: &str) {
    if rows.is_empty() {
        append_right_sidebar_muted(body, empty_text);
        return;
    }
    for row in rows {
        append_right_sidebar_row(body, &row);
    }
}

/// purpose: Render bounded surface summaries for a workspace.
/// inputs: Body widget, workspace, section title, and selected-first flag.
/// returns/effects: Adds section rows for live pane surfaces.
fn append_right_sidebar_surface_rows(
    body: &gtk::Box,
    workspace: &Workspace,
    title: &str,
    selected_first: bool,
) {
    append_right_sidebar_section(body, title);
    let rows = sidebar_surface_preview_lines(workspace, selected_first);
    append_right_sidebar_lines(body, rows, "No live surfaces");
}

/// purpose: Render sidebar status rows in the visible panel.
/// inputs: Body widget and active workspace metadata.
/// returns/effects: Adds status section rows to the right sidebar.
fn sync_right_sidebar_status_rows(body: &gtk::Box, workspace: &Workspace) {
    append_right_sidebar_section(body, "Status");
    let rows = sidebar_status_preview_lines(workspace);
    if rows.is_empty() {
        append_right_sidebar_muted(body, "none");
        return;
    }
    for row in rows {
        append_right_sidebar_row(body, &row);
    }
}

/// purpose: Render sidebar progress state in the visible panel.
/// inputs: Body widget and active workspace metadata.
/// returns/effects: Adds one progress row to the right sidebar.
fn sync_right_sidebar_progress_row(body: &gtk::Box, workspace: &Workspace) {
    append_right_sidebar_section(body, "Progress");
    match workspace.sidebar_progress.as_ref() {
        Some(progress) => append_right_sidebar_row(body, &sidebar_progress_preview_line(progress)),
        None => append_right_sidebar_muted(body, "none"),
    }
}

/// purpose: Render recent sidebar log entries in the visible panel.
/// inputs: Body widget and active workspace metadata.
/// returns/effects: Adds newest retained log rows up to the preview cap.
fn sync_right_sidebar_log_rows(body: &gtk::Box, workspace: &Workspace) {
    append_right_sidebar_section(body, "Log");
    let rows = sidebar_log_preview_lines(workspace, RIGHT_SIDEBAR_LOG_PREVIEW_LIMIT);
    if rows.is_empty() {
        append_right_sidebar_muted(body, "none");
        return;
    }
    for row in rows {
        append_right_sidebar_row(body, &row);
    }
}

/// purpose: Format status rows for the right-sidebar preview.
/// inputs: Active workspace metadata.
/// returns/effects: Returns rows sorted by priority then key.
fn sidebar_status_preview_lines(workspace: &Workspace) -> Vec<String> {
    sidebar_status_preview_lines_from_entries(workspace.sidebar_status.values())
}

/// purpose: Resolve the workspace path displayed by Files and Find right-sidebar panels.
/// inputs: Active workspace metadata.
/// returns/effects: Returns folder_path or current terminal cwd without mutating state.
fn right_sidebar_workspace_path(workspace: &Workspace) -> Option<String> {
    workspace
        .folder_path
        .clone()
        .or_else(|| workspace.cwd.borrow().clone())
        .map(|path| path.trim().to_string())
        .filter(|path| !path.is_empty())
}

/// purpose: Format a bounded non-recursive directory preview for Files mode.
/// inputs: Directory path and maximum number of visible entries.
/// returns/effects: Reads at most limit plus one entries and returns display rows or error text.
fn sidebar_file_preview_lines(path: &Path, limit: usize) -> Result<Vec<String>, String> {
    let metadata = fs::metadata(path).map_err(|error| format!("Cannot read path: {error}"))?;
    if !metadata.is_dir() {
        return Err("Workspace path is not a directory".to_string());
    }
    let mut rows = fs::read_dir(path)
        .map_err(|error| format!("Cannot list folder: {error}"))?
        .take(limit.saturating_add(1))
        .map(sidebar_file_entry_preview_line)
        .collect::<Result<Vec<_>, _>>()?;
    let truncated = rows.len() > limit;
    rows.truncate(limit);
    rows.sort();
    if truncated {
        rows.push("... more entries".to_string());
    }
    Ok(rows)
}

/// purpose: Format one filesystem entry for the Files right-sidebar panel.
/// inputs: A directory entry result from std::fs::read_dir.
/// returns/effects: Returns one display row or explicit error text.
fn sidebar_file_entry_preview_line(entry: std::io::Result<fs::DirEntry>) -> Result<String, String> {
    let entry = entry.map_err(|error| format!("Cannot read folder entry: {error}"))?;
    let name = entry.file_name().to_string_lossy().to_string();
    let kind = entry
        .file_type()
        .map_err(|error| format!("Cannot read entry type: {error}"))?;
    if kind.is_dir() {
        return Ok(format!("dir  {name}/"));
    }
    if kind.is_symlink() {
        return Ok(format!("link {name}"));
    }
    Ok(format!("file {name}"))
}

/// purpose: Format live workspace surfaces for right-sidebar mode panels.
/// inputs: Workspace and whether selected surfaces should be listed first.
/// returns/effects: Returns bounded rows without mutating GTK state.
fn sidebar_surface_preview_lines(workspace: &Workspace, selected_first: bool) -> Vec<String> {
    let mut surfaces = pane::surface_summaries_for_root(&workspace.root);
    if selected_first {
        surfaces.sort_by(|left, right| right.selected.cmp(&left.selected));
    }
    surfaces
        .into_iter()
        .take(RIGHT_SIDEBAR_SURFACE_PREVIEW_LIMIT)
        .map(|surface| {
            let marker = if surface.selected { "*" } else { "-" };
            format!(
                "{marker} pane {} {} {}",
                surface.pane_id, surface.kind, surface.title
            )
        })
        .collect()
}

/// purpose: Format status preview rows from arbitrary retained entries.
/// inputs: Sidebar status entries.
/// returns/effects: Returns rows sorted by priority then key.
fn sidebar_status_preview_lines_from_entries<'a>(
    entries: impl Iterator<Item = &'a SidebarStatusEntry>,
) -> Vec<String> {
    let mut entries = entries.collect::<Vec<_>>();
    entries.sort_by(|left, right| {
        right
            .priority
            .cmp(&left.priority)
            .then_with(|| left.key.cmp(&right.key))
    });
    entries
        .into_iter()
        .map(|entry| format!("{} = {} ({})", entry.key, entry.value, entry.priority))
        .collect()
}

/// purpose: Format progress state for the right-sidebar preview.
/// inputs: Retained progress metadata.
/// returns/effects: Returns one visible row string.
fn sidebar_progress_preview_line(progress: &SidebarProgress) -> String {
    let percent = (progress.value * 100.0).round() as i64;
    match progress.label.as_deref() {
        Some(label) if !label.is_empty() => format!("{percent}% - {label}"),
        _ => format!("{percent}%"),
    }
}

/// purpose: Format recent log entries for the right-sidebar preview.
/// inputs: Active workspace metadata and maximum number of rows.
/// returns/effects: Returns oldest-to-newest rows within the retained tail.
fn sidebar_log_preview_lines(workspace: &Workspace, limit: usize) -> Vec<String> {
    sidebar_log_preview_lines_from_entries(&workspace.sidebar_log, limit)
}

/// purpose: Format recent log preview rows from retained entries.
/// inputs: Sidebar log entries and maximum number of rows.
/// returns/effects: Returns oldest-to-newest rows within the retained tail.
fn sidebar_log_preview_lines_from_entries(
    entries: &[SidebarLogEntry],
    limit: usize,
) -> Vec<String> {
    let start = entries.len().saturating_sub(limit);
    entries[start..]
        .iter()
        .map(|entry| match entry.source.as_deref() {
            Some(source) if !source.is_empty() => {
                format!("[{}] {}: {}", entry.level, source, entry.message)
            }
            _ => format!("[{}] {}", entry.level, entry.message),
        })
        .collect()
}

/// purpose: Format CMUX Feed list payload rows for the right-sidebar Feed panel.
/// inputs: Feed list response payload and maximum rows.
/// returns/effects: Returns newest retained rows first without mutating Feed state.
#[cfg(test)]
fn sidebar_feed_preview_lines_from_value(payload: &serde_json::Value, limit: usize) -> Vec<String> {
    sidebar_feed_visible_items(payload, limit)
        .into_iter()
        .map(sidebar_feed_preview_line)
        .collect()
}

/// purpose: Select the newest bounded Feed items visible in the right sidebar.
/// inputs: Feed list response payload and maximum rows.
/// returns/effects: Returns borrowed item rows newest-first without mutating Feed state.
fn sidebar_feed_visible_items(
    payload: &serde_json::Value,
    limit: usize,
) -> Vec<&serde_json::Value> {
    let Some(items) = payload.get("items").and_then(serde_json::Value::as_array) else {
        return Vec::new();
    };
    items.iter().rev().take(limit).collect()
}

/// purpose: Format one CMUX Feed row for compact right-sidebar display.
/// inputs: One Feed JSON row.
/// returns/effects: Returns a compact source/kind/status string.
fn sidebar_feed_preview_line(item: &serde_json::Value) -> String {
    let status = json_string_field(item, "status").unwrap_or("unknown");
    let source = json_string_field(item, "source").unwrap_or("unknown");
    let kind = json_string_field(item, "kind").unwrap_or("event");
    match json_string_field(item, "tool_name") {
        Some(tool) => format!("[{status}] {source} {kind}: {tool}"),
        None => format!("[{status}] {source} {kind}"),
    }
}

/// purpose: Extract action-eligible pending permission request ids from Feed rows.
/// inputs: One Feed item row.
/// returns/effects: Returns request_id only for pending permission requests.
fn pending_permission_request_id(item: &serde_json::Value) -> Option<String> {
    pending_request_id_for_kind(item, &["permissionRequest", "PermissionRequest"])
}

/// purpose: Extract action-eligible pending exit-plan request ids from Feed rows.
/// inputs: One Feed item row.
/// returns/effects: Returns request_id only for pending exit-plan requests.
fn pending_exit_plan_request_id(item: &serde_json::Value) -> Option<String> {
    pending_request_id_for_kind(item, &["exitPlan", "ExitPlanMode"])
}

/// purpose: Extract action-eligible pending question request ids from Feed rows.
/// inputs: One Feed item row.
/// returns/effects: Returns request_id only for pending question requests.
fn pending_question_request_id(item: &serde_json::Value) -> Option<String> {
    pending_request_id_for_kind(item, &["question", "AskUserQuestion"])
}

/// purpose: Extract pending request ids for a set of CMUX Feed kinds.
/// inputs: One Feed item row and accepted kind values.
/// returns/effects: Returns request_id only for matching pending rows.
fn pending_request_id_for_kind(item: &serde_json::Value, kinds: &[&str]) -> Option<String> {
    let status = json_string_field(item, "status")?;
    if status != "pending" {
        return None;
    }
    let kind = json_string_field(item, "kind")?;
    if !kinds.contains(&kind) {
        return None;
    }
    json_string_field(item, "request_id").map(ToOwned::to_owned)
}

/// purpose: Build direct question reply options from a Feed question payload.
/// inputs: One Feed item row with optional `tool_input` question data.
/// returns/effects: Returns visible button labels plus selection payloads.
fn feed_question_action_specs(item: &serde_json::Value) -> Vec<(String, Vec<String>)> {
    let questions = feed_question_prompts(item);
    if questions.len() > 1 {
        let defaults = questions
            .iter()
            .map(|question| question.first().cloned().unwrap_or_default())
            .collect::<Vec<_>>();
        return vec![("Default".to_string(), defaults)];
    }
    questions
        .first()
        .into_iter()
        .flat_map(|options| options.iter())
        .take(6)
        .filter(|option| !option.trim().is_empty())
        .map(|option| (option.clone(), vec![option.clone()]))
        .collect()
}

/// purpose: Parse CMUX/Claude question prompt option labels from Feed rows.
/// inputs: One Feed item row.
/// returns/effects: Returns each prompt's available option labels in order.
fn feed_question_prompts(item: &serde_json::Value) -> Vec<Vec<String>> {
    let Some(input) = item.get("tool_input") else {
        return feed_question_options(item);
    };
    if let Some(questions) = input.get("questions").and_then(serde_json::Value::as_array) {
        return questions
            .iter()
            .map(|question| {
                question
                    .get("options")
                    .and_then(serde_json::Value::as_array)
                    .map(|options| feed_option_labels(options))
                    .unwrap_or_default()
            })
            .collect();
    }
    feed_question_options(input)
}

/// purpose: Parse flat question option labels from a Feed row or tool input.
/// inputs: JSON object that may contain question option arrays.
/// returns/effects: Returns one prompt worth of option labels or an empty set.
fn feed_question_options(value: &serde_json::Value) -> Vec<Vec<String>> {
    for key in ["question_options", "options"] {
        if let Some(options) = value.get(key).and_then(serde_json::Value::as_array) {
            return vec![feed_option_labels(options)];
        }
    }
    Vec::new()
}

/// purpose: Convert CMUX question option values to labels.
/// inputs: JSON option values, either strings or objects with label/title/id fields.
/// returns/effects: Returns non-empty labels in input order.
fn feed_option_labels(options: &[serde_json::Value]) -> Vec<String> {
    options
        .iter()
        .filter_map(|option| {
            option.as_str().map(ToOwned::to_owned).or_else(|| {
                ["label", "title", "id"]
                    .iter()
                    .find_map(|key| json_string_field(option, key).map(ToOwned::to_owned))
            })
        })
        .filter(|label| !label.trim().is_empty())
        .collect()
}

/// purpose: Extract one non-empty string field from a JSON object.
/// inputs: JSON value and field name.
/// returns/effects: Returns the field string when present and non-empty.
fn json_string_field<'a>(value: &'a serde_json::Value, key: &str) -> Option<&'a str> {
    value
        .get(key)
        .and_then(serde_json::Value::as_str)
        .map(str::trim)
        .filter(|text| !text.is_empty())
}

/// purpose: Resolve a CMUX sidebar metadata workspace selector.
/// inputs: Host state and a workspace target from the control bridge.
/// returns/effects: Returns the live workspace index or a not_found error.
fn sidebar_workspace_index(
    state: &AppState,
    target: &WorkspaceTarget,
) -> Result<usize, BridgeError> {
    workspace_index_for_target(state, target)
        .ok_or_else(|| BridgeError::not_found("sidebar workspace target not found"))
}

/// purpose: Render one sidebar status entry in the public control API shape.
/// inputs: Retained sidebar status entry plus owning workspace id.
/// returns/effects: Returns JSON without mutating state.
fn sidebar_status_row(workspace_id: &str, entry: &SidebarStatusEntry) -> serde_json::Value {
    serde_json::json!({
        "workspace_id": workspace_id,
        "workspace_ref": workspace_ref(workspace_id),
        "key": entry.key,
        "value": entry.value,
        "icon": entry.icon,
        "color": entry.color,
        "url": entry.url,
        "priority": entry.priority,
    })
}

/// purpose: Render all status entries for a workspace in CMUX priority order.
/// inputs: Workspace with retained sidebar status state.
/// returns/effects: Returns sorted JSON rows without mutating state.
fn sidebar_status_rows(workspace: &Workspace) -> Vec<serde_json::Value> {
    let mut entries = workspace.sidebar_status.values().collect::<Vec<_>>();
    entries.sort_by(|left, right| {
        right
            .priority
            .cmp(&left.priority)
            .then_with(|| left.key.cmp(&right.key))
    });
    entries
        .into_iter()
        .map(|entry| sidebar_status_row(&workspace.id, entry))
        .collect()
}

/// purpose: Render the optional sidebar progress state for a workspace.
/// inputs: Workspace with retained progress metadata.
/// returns/effects: Returns JSON null when progress is absent.
fn sidebar_progress_row(workspace: &Workspace) -> serde_json::Value {
    match workspace.sidebar_progress.as_ref() {
        Some(progress) => serde_json::json!({
            "workspace_id": workspace.id,
            "workspace_ref": workspace_ref(&workspace.id),
            "value": progress.value,
            "label": progress.label,
        }),
        None => serde_json::Value::Null,
    }
}

/// purpose: Render one sidebar log entry in the public control API shape.
/// inputs: Retained sidebar log entry plus owning workspace id.
/// returns/effects: Returns JSON without mutating state.
fn sidebar_log_row(workspace_id: &str, entry: &SidebarLogEntry) -> serde_json::Value {
    serde_json::json!({
        "id": entry.id,
        "workspace_id": workspace_id,
        "workspace_ref": workspace_ref(workspace_id),
        "created_at": entry.created_at,
        "level": entry.level,
        "source": entry.source,
        "message": entry.message,
    })
}

/// purpose: Render sidebar log rows, optionally limited to the newest entries.
/// inputs: Workspace with bounded log state and optional limit.
/// returns/effects: Returns JSON rows without mutating state.
fn sidebar_log_rows(workspace: &Workspace, limit: Option<usize>) -> Vec<serde_json::Value> {
    let start = limit
        .map(|limit| workspace.sidebar_log.len().saturating_sub(limit))
        .unwrap_or(0);
    workspace.sidebar_log[start..]
        .iter()
        .map(|entry| sidebar_log_row(&workspace.id, entry))
        .collect()
}

/// purpose: Publish a CMUX-compatible sidebar metadata event.
/// inputs: Event name, workspace id, and redaction-safe payload.
/// returns/effects: Appends the event to the retained/live event stream.
fn publish_sidebar_event(name: &str, workspace_id: &str, payload: serde_json::Value) {
    crate::event_bus::bus().publish(crate::event_bus::EventPublish {
        name,
        category: "sidebar",
        source: "sidebar.metadata",
        workspace_id: Some(serde_json::Value::String(workspace_id.to_string())),
        surface_id: None,
        pane_id: None,
        payload,
    });
}

/// purpose: Read the current git branch for sidebar-state metadata.
/// inputs: Optional workspace cwd/folder path.
/// returns/effects: Runs `git rev-parse` when a cwd exists; returns "none" on non-git dirs.
fn sidebar_git_branch(cwd: Option<&str>) -> String {
    let Some(cwd) = cwd.filter(|value| !value.is_empty()) else {
        return "none".to_string();
    };
    match Command::new("git")
        .arg("-C")
        .arg(cwd)
        .arg("rev-parse")
        .arg("--abbrev-ref")
        .arg("HEAD")
        .output()
    {
        Ok(output) if output.status.success() => {
            String::from_utf8_lossy(&output.stdout).trim().to_string()
        }
        _ => "none".to_string(),
    }
}

/// purpose: Render all retained CMUX sidebar metadata for one workspace.
/// inputs: Workspace with status/progress/log state.
/// returns/effects: Returns aggregate JSON without mutating state.
fn sidebar_state_payload(workspace: &Workspace) -> serde_json::Value {
    let cwd = workspace
        .folder_path
        .clone()
        .or_else(|| workspace.cwd.borrow().clone())
        .unwrap_or_else(|| "none".to_string());
    let git_branch = if cwd == "none" {
        "none".to_string()
    } else {
        sidebar_git_branch(Some(&cwd))
    };
    serde_json::json!({
        "workspace": workspace.id,
        "workspace_id": workspace.id,
        "workspace_ref": workspace_ref(&workspace.id),
        "cwd": cwd,
        "git_branch": git_branch,
        "ports": [],
        "status": sidebar_status_rows(workspace),
        "progress": sidebar_progress_row(workspace),
        "log": sidebar_log_rows(workspace, None),
    })
}

/// purpose: Apply one CMUX sidebar metadata/status/progress/log action.
/// inputs: Live host state, parsed sidebar action, and workspace target.
/// returns/effects: Mutates bounded per-workspace sidebar state and returns JSON.
fn apply_sidebar_action(
    state: &State,
    action: SidebarAction,
    target: WorkspaceTarget,
) -> Result<serde_json::Value, BridgeError> {
    let mut app_state = state.borrow_mut();
    let index = sidebar_workspace_index(&app_state, &target)?;
    let workspace_id = app_state.workspaces[index].id.clone();
    let result = match action {
        SidebarAction::SetStatus {
            key,
            value,
            icon,
            color,
            url,
            priority,
        } => apply_sidebar_status_set(
            &mut app_state.workspaces[index],
            key,
            value,
            icon,
            color,
            url,
            priority,
        ),
        SidebarAction::ClearStatus { key } => {
            apply_sidebar_status_clear(&mut app_state.workspaces[index], &key)
        }
        SidebarAction::ListStatus => Ok(serde_json::json!({
            "workspace_id": workspace_id,
            "status": sidebar_status_rows(&app_state.workspaces[index]),
        })),
        SidebarAction::SetProgress { value, label } => {
            apply_sidebar_progress_set(&mut app_state.workspaces[index], value, label)
        }
        SidebarAction::ClearProgress => {
            apply_sidebar_progress_clear(&mut app_state.workspaces[index])
        }
        SidebarAction::AppendLog {
            level,
            source,
            message,
        } => apply_sidebar_log_append(&mut app_state, index, level, source, message),
        SidebarAction::ClearLog => apply_sidebar_log_clear(&mut app_state.workspaces[index]),
        SidebarAction::ListLog { limit } => Ok(serde_json::json!({
            "workspace_id": workspace_id,
            "log": sidebar_log_rows(&app_state.workspaces[index], limit),
        })),
        SidebarAction::State => Ok(sidebar_state_payload(&app_state.workspaces[index])),
    };
    if result.is_ok() {
        sync_right_sidebar_panel(&mut app_state);
    }
    result
}

/// purpose: Store or replace one sidebar status entry.
/// inputs: Workspace, key/value, and presentation metadata.
/// returns/effects: Mutates status state, publishes an update event, and returns JSON.
fn apply_sidebar_status_set(
    workspace: &mut Workspace,
    key: String,
    value: String,
    icon: Option<String>,
    color: Option<String>,
    url: Option<String>,
    priority: i64,
) -> Result<serde_json::Value, BridgeError> {
    let entry = SidebarStatusEntry {
        key: key.clone(),
        value,
        icon,
        color,
        url,
        priority,
    };
    workspace.sidebar_status.insert(key, entry.clone());
    publish_sidebar_event(
        "sidebar.metadata.updated",
        &workspace.id,
        serde_json::json!({"key": entry.key, "value_length": entry.value.len()}),
    );
    Ok(serde_json::json!({"ok": true, "status": sidebar_status_row(&workspace.id, &entry)}))
}

/// purpose: Remove one sidebar status entry by key.
/// inputs: Workspace and status key.
/// returns/effects: Mutates status state or fails when the key is absent.
fn apply_sidebar_status_clear(
    workspace: &mut Workspace,
    key: &str,
) -> Result<serde_json::Value, BridgeError> {
    let removed = workspace.sidebar_status.remove(key);
    if removed.is_none() {
        return Err(BridgeError::not_found("sidebar status key not found"));
    }
    publish_sidebar_event(
        "sidebar.metadata.cleared",
        &workspace.id,
        serde_json::json!({"key": key}),
    );
    Ok(serde_json::json!({"ok": true, "key": key, "workspace_id": workspace.id}))
}

/// purpose: Store sidebar progress metadata for a workspace.
/// inputs: Workspace, progress value, and optional label.
/// returns/effects: Mutates progress state, publishes an event, and returns JSON.
fn apply_sidebar_progress_set(
    workspace: &mut Workspace,
    value: f64,
    label: Option<String>,
) -> Result<serde_json::Value, BridgeError> {
    workspace.sidebar_progress = Some(SidebarProgress { value, label });
    publish_sidebar_event(
        "sidebar.progress.updated",
        &workspace.id,
        serde_json::json!({"value": value}),
    );
    Ok(serde_json::json!({"ok": true, "progress": sidebar_progress_row(workspace)}))
}

/// purpose: Clear sidebar progress metadata for a workspace.
/// inputs: Workspace whose progress may be set.
/// returns/effects: Mutates progress state, publishes an event, and returns JSON.
fn apply_sidebar_progress_clear(
    workspace: &mut Workspace,
) -> Result<serde_json::Value, BridgeError> {
    workspace.sidebar_progress = None;
    publish_sidebar_event(
        "sidebar.progress.cleared",
        &workspace.id,
        serde_json::json!({}),
    );
    Ok(serde_json::json!({"ok": true, "workspace_id": workspace.id}))
}

/// purpose: Append one bounded sidebar log entry for a workspace.
/// inputs: Host state, workspace index, level/source, and message.
/// returns/effects: Mutates log state, evicts oldest rows past cap, and publishes an event.
fn apply_sidebar_log_append(
    app_state: &mut AppState,
    index: usize,
    level: String,
    source: Option<String>,
    message: String,
) -> Result<serde_json::Value, BridgeError> {
    let id = app_state.next_sidebar_log_id;
    app_state.next_sidebar_log_id = app_state.next_sidebar_log_id.saturating_add(1);
    let entry = SidebarLogEntry {
        id,
        created_at: notification_created_at(),
        level,
        source,
        message,
    };
    let workspace = &mut app_state.workspaces[index];
    workspace.sidebar_log.push(entry.clone());
    if workspace.sidebar_log.len() > MAX_SIDEBAR_LOG_ENTRIES {
        workspace.sidebar_log.remove(0);
    }
    publish_sidebar_event(
        "sidebar.log.appended",
        &workspace.id,
        serde_json::json!({"id": id, "level": entry.level, "message_length": entry.message.len()}),
    );
    Ok(serde_json::json!({"ok": true, "entry": sidebar_log_row(&workspace.id, &entry)}))
}

/// purpose: Clear all retained sidebar log entries for a workspace.
/// inputs: Workspace with bounded sidebar log state.
/// returns/effects: Mutates log state, publishes an event, and returns removed count.
fn apply_sidebar_log_clear(workspace: &mut Workspace) -> Result<serde_json::Value, BridgeError> {
    let count = workspace.sidebar_log.len();
    workspace.sidebar_log.clear();
    publish_sidebar_event(
        "sidebar.log.cleared",
        &workspace.id,
        serde_json::json!({"count": count}),
    );
    Ok(serde_json::json!({"ok": true, "count": count, "workspace_id": workspace.id}))
}

fn begin_window_move_from_widget(
    widget: &impl IsA<gtk::Widget>,
    window: &adw::ApplicationWindow,
    device: &gtk::gdk::Device,
    button: i32,
    x: f64,
    y: f64,
    timestamp: u32,
) {
    let Some((surface_x, surface_y)) = widget.translate_coordinates(window, x, y) else {
        return;
    };
    let Some(surface) = window.surface() else {
        return;
    };
    let Ok(toplevel) = surface.dynamic_cast::<gtk::gdk::Toplevel>() else {
        return;
    };
    toplevel.begin_move(device, button, surface_x, surface_y, timestamp);
}

fn split_ratio_state(paned: &gtk::Paned) -> Option<Rc<RefCell<f64>>> {
    unsafe {
        paned
            .data::<Rc<RefCell<f64>>>(SPLIT_RATIO_STATE_KEY)
            .map(|ptr| ptr.as_ref().clone())
    }
}

pub(crate) fn update_split_ratio_state(paned: &gtk::Paned, ratio: f64) {
    let ratio = layout_state::clamp_split_ratio(ratio);
    if let Some(stored_ratio) = split_ratio_state(paned) {
        *stored_ratio.borrow_mut() = ratio;
    } else {
        unsafe {
            paned.set_data(SPLIT_RATIO_STATE_KEY, Rc::new(RefCell::new(ratio)));
        }
    }
}

fn build_workspace_root(
    state: &State,
    shortcuts: &Rc<ResolvedShortcutConfig>,
    ws_id: &str,
    working_directory: Option<&str>,
    layout: &LayoutNodeState,
) -> (gtk::Widget, Rc<SplitTreeContainer>) {
    let tree_node = split_tree::build_split_node_from_layout(
        state,
        shortcuts,
        ws_id,
        working_directory,
        layout,
    );
    let container = SplitTreeContainer::new_from_tree(state, tree_node);
    let root = container.widget().clone().upcast::<gtk::Widget>();
    (root, container)
}

fn apply_ratio_value(
    paned: &gtk::Paned,
    orientation: gtk::Orientation,
    ratio: f64,
    applying: &Rc<Cell<bool>>,
) -> bool {
    let ratio = layout_state::clamp_split_ratio(ratio);
    let allocation = paned.allocation();
    let size = if orientation == gtk::Orientation::Horizontal {
        allocation.width()
    } else {
        allocation.height()
    };
    if size <= 0 {
        return false;
    }
    applying.set(true);
    paned.set_position(layout_state::split_position_from_ratio(ratio, size));
    update_split_ratio_state(paned, ratio);
    applying.set(false);
    true
}

pub(crate) fn apply_split_ratio_after_layout(
    paned: &gtk::Paned,
    orientation: gtk::Orientation,
    ratio_cell: Rc<RefCell<f64>>,
    applying: Rc<Cell<bool>>,
) {
    // Capture the ratio by value for the initial idle callback so that early
    // position_notify events (which may corrupt the cell) don't affect it.
    let initial_ratio = *ratio_cell.borrow();

    let paned_for_idle = paned.clone();
    let applying_for_idle = applying.clone();
    glib::idle_add_local_once(move || {
        apply_ratio_value(
            &paned_for_idle,
            orientation,
            initial_ratio,
            &applying_for_idle,
        );
    });

    let paned_for_map = paned.clone();
    // Re-apply the current data model ratio on every map event (workspace switches).
    // Reads from the cell so drag-adjusted ratios are restored correctly.
    paned.connect_map(move |_| {
        let ratio = *ratio_cell.borrow();
        apply_ratio_value(&paned_for_map, orientation, ratio, &applying);
    });
}

pub(crate) fn attach_split_position_persistence(state: &State, paned: &gtk::Paned) {
    update_split_ratio_state(paned, layout_state::DEFAULT_SPLIT_RATIO);
    let state = state.clone();
    paned.connect_position_notify(move |paned| {
        let allocation = paned.allocation();
        let size = if paned.orientation() == gtk::Orientation::Horizontal {
            allocation.width()
        } else {
            allocation.height()
        };
        let ratio = layout_state::snapshot_split_ratio(
            paned.position(),
            size,
            split_ratio_state(paned).map(|ratio| *ratio.borrow()),
        );
        update_split_ratio_state(paned, ratio);
        request_session_save(&state);
    });
}

// ---------------------------------------------------------------------------
// CSS
// ---------------------------------------------------------------------------

const HOST_ENTRY_CSS_CLASS: &str = "limux-host-entry";
const WORKSPACE_RENAME_ENTRY_CSS_CLASS: &str = "limux-ws-rename-entry";
const WORKSPACE_RENAME_ENTRY_CSS_CLASSES: [&str; 2] =
    [HOST_ENTRY_CSS_CLASS, WORKSPACE_RENAME_ENTRY_CSS_CLASS];
const SIDEBAR_HANDLE_CSS_CLASS: &str = "limux-sidebar-handle";
const SIDEBAR_HANDLE_CURSOR_NAME: &str = "col-resize";
const SIDEBAR_RESIZE_HANDLE_WIDTH_PX: i32 = 3;
const RIGHT_SIDEBAR_WIDTH: i32 = 280;
const RIGHT_SIDEBAR_LOG_PREVIEW_LIMIT: usize = 8;
const RIGHT_SIDEBAR_FILE_PREVIEW_LIMIT: usize = 12;
const RIGHT_SIDEBAR_FEED_PREVIEW_LIMIT: usize = 8;
const RIGHT_SIDEBAR_SURFACE_PREVIEW_LIMIT: usize = 12;

const BASE_CSS: &str = r#"
.limux-host-entry {
    background-color: alpha(@window_bg_color, 0.98);
    color: @window_fg_color;
    border: 1px solid alpha(@window_fg_color, 0.16);
    border-radius: 6px;
    caret-color: currentColor;
}
.limux-host-entry:focus-within {
    border-color: alpha(@accent_bg_color, 0.76);
}
.limux-host-entry text {
    background-color: transparent;
    color: @window_fg_color;
}
.limux-host-entry text placeholder {
    color: alpha(@window_fg_color, 0.5);
}
.limux-host-entry image {
    color: alpha(@window_fg_color, 0.5);
}
.limux-sidebar {
    background-color: @window_bg_color;
    color: @window_fg_color;
    border-right: 1px solid alpha(@window_fg_color, 0.08);
}
.limux-right-sidebar {
    background-color: @window_bg_color;
    color: @window_fg_color;
    border-left: 1px solid alpha(@window_fg_color, 0.1);
    padding: 10px;
}
.limux-right-sidebar-title {
    color: @window_fg_color;
    font-size: 15px;
    font-weight: 700;
}
.limux-right-sidebar-section {
    color: alpha(@window_fg_color, 0.64);
    font-size: 11px;
    font-weight: 700;
    margin-top: 12px;
}
.limux-right-sidebar-row {
    color: alpha(@window_fg_color, 0.82);
    font-size: 12px;
    margin-top: 4px;
}
.limux-right-sidebar-muted {
    color: alpha(@window_fg_color, 0.52);
    font-size: 12px;
    margin-top: 4px;
}
.limux-sidebar-row-box {
    padding: 8px 6px 8px 3px;
    border-radius: 6px;
    margin: 2px 3px 2px 1px;
}
.limux-ws-name {
    color: alpha(@window_fg_color, 0.72);
    font-size: 15px;
}
row:selected .limux-ws-name {
    color: @window_fg_color;
}
.limux-ws-star-btn {
    color: alpha(@window_fg_color, 0.45);
    border: none;
    min-height: 0;
    min-width: 0;
    padding: 0 4px;
    font-size: 22px;
}
.limux-ws-star-btn:hover {
    color: alpha(@window_fg_color, 0.9);
}
row:selected .limux-ws-star-btn {
    color: alpha(@window_fg_color, 0.85);
}
.limux-ws-star-btn-active {
    color: @accent_bg_color;
}
.limux-ws-rename-entry {
    min-height: 0;
    padding: 0 4px;
    margin: 0;
}
.limux-notify-dot {
    color: @accent_bg_color;
    font-size: 10px;
    margin-right: 6px;
}
.limux-notify-dot-hidden {
    color: transparent;
    font-size: 10px;
    margin-right: 6px;
}
.limux-notify-msg {
    color: alpha(@window_fg_color, 0.35);
    font-size: 11px;
}
.limux-notify-msg-unread {
    color: alpha(@accent_bg_color, 0.9);
    font-size: 11px;
}
.limux-sidebar-row-unread {
    background-color: alpha(@accent_bg_color, 0.16);
    border-left: 3px solid @accent_bg_color;
    border-radius: 6px;
    margin-left: 0;
    margin-right: 0;
}
.limux-sidebar-row-unread .limux-ws-name {
    color: @window_fg_color;
    font-weight: 700;
}
.limux-drop-above .limux-sidebar-row-box {
    border-radius: 0;
    box-shadow: 0 -2px 0 0 @accent_bg_color;
}
.limux-drop-below .limux-sidebar-row-box {
    border-radius: 0;
    box-shadow: 0 2px 0 0 @accent_bg_color;
}
.limux-tab-drop-target {
    background-color: alpha(@accent_bg_color, 0.18);
    border-radius: 8px;
}
.limux-sidebar row:drop(active) {
    box-shadow: none;
}
.limux-sidebar-title {
    color: alpha(@window_fg_color, 0.55);
    font-size: 11px;
    font-weight: 600;
    letter-spacing: 1px;
}
.limux-sidebar-btn {
    background: alpha(@window_fg_color, 0.08);
    color: alpha(@window_fg_color, 0.7);
    border: 1px solid transparent;
    border-radius: 6px;
    padding: 6px 12px;
    min-height: 0;
    transition: all 200ms ease;
}
.limux-sidebar-btn:hover {
    background: alpha(@window_fg_color, 0.14);
    color: @window_fg_color;
}
.limux-sidebar-btn-trash {
    background: alpha(@error_color, 0.16);
    color: @error_color;
    border: 1px solid alpha(@error_color, 0.4);
}
.limux-sidebar-btn-trash-hover {
    background: alpha(@error_color, 0.26);
    color: @error_color;
    border: 1px solid alpha(@error_color, 0.7);
}
.limux-tab-drag-active {
    background-color: alpha(@accent_bg_color, 0.12);
    border-width: 1px;
    border-style: dashed;
    border-color: alpha(@accent_bg_color, 0.6);
    border-radius: 8px;
}
.limux-sidebar-btn.limux-tab-drop-target {
    background-color: alpha(@accent_bg_color, 0.28);
    border-color: alpha(@accent_bg_color, 0.9);
}
.limux-ws-path {
    color: alpha(@window_fg_color, 0.3);
    font-size: 12px;
}
row:selected .limux-ws-path {
    color: alpha(@window_fg_color, 0.5);
}
.limux-content {
    background-color: @window_bg_color;
}
.limux-sidebar-handle {
    min-width: 3px;
    background-color: alpha(@window_fg_color, 0.08);
}
.limux-sidebar-handle:hover {
    background-color: alpha(@accent_bg_color, 0.45);
}
"#;

const CONTENT_BACKGROUND_RGB: (u8, u8, u8) = (23, 23, 23);

// ---------------------------------------------------------------------------
// Window construction
// ---------------------------------------------------------------------------

pub fn build_window(app: &adw::Application) {
    let display = gtk::gdk::Display::default().expect("display");
    let gnome_interface_settings = gnome_interface_settings();
    let portal_color_scheme_preference = Rc::new(Cell::new(PortalColorSchemePreference::Unknown));
    let system_prefers_dark = Rc::new(Cell::new(resolve_system_prefers_dark(
        portal_color_scheme_preference.get(),
        gnome_interface_settings.as_ref(),
    )));
    let loaded_config = app_config::load();
    for warning in &loaded_config.warnings {
        eprintln!("limux: {warning}");
    }
    let config = Rc::new(RefCell::new(loaded_config.config));
    let background_opacity =
        sanitize_background_opacity(crate::terminal::ghostty_background_opacity());

    let shortcuts = Rc::new(shortcut_config::load_shortcuts_for_display(&display));
    for warning in &shortcuts.warnings {
        eprintln!("limux: {warning}");
    }

    // Load CSS
    let provider = gtk::CssProvider::new();
    let all_css = format!(
        "{}\n{}\n{}\n{}",
        build_window_css(background_opacity),
        pane::PANE_CSS,
        keybind_editor::KEYBIND_EDITOR_CSS,
        crate::settings_editor::SETTINGS_CSS,
    );
    provider.load_from_data(&all_css);
    gtk::style_context_add_provider_for_display(
        &display,
        &provider,
        gtk::STYLE_PROVIDER_PRIORITY_APPLICATION,
    );

    let style_manager = adw::StyleManager::default();
    apply_appearance(
        &style_manager,
        system_prefers_dark.get(),
        &config.borrow().appearance,
    );

    // Register custom icons — look for icons dir relative to the executable
    let icon_theme = gtk::IconTheme::for_display(&display);
    let exe_dir = std::env::current_exe()
        .ok()
        .and_then(|p| p.parent().map(|d| d.to_path_buf()));
    // Try several possible icon locations
    for path in [
        exe_dir
            .as_ref()
            .map(|d| d.join("../../rust/limux-host-linux/icons")),
        exe_dir.as_ref().map(|d| d.join("../icons")),
        Some(std::path::PathBuf::from(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/icons"
        ))),
    ]
    .iter()
    .flatten()
    {
        if path.exists() {
            icon_theme.add_search_path(path);
        }
    }

    let title = format!("Limux v{}", crate::VERSION);
    let window = adw::ApplicationWindow::builder()
        .application(app)
        .title(&title)
        .default_width(1400)
        .default_height(900)
        .build();
    apply_window_background_class(&window, background_opacity);

    // On Wayland compositors with xdg-decoration support, the compositor
    // already provides the window chrome, so keep Limux from rendering a
    // duplicate header bar. X11 continues to use the in-app header.
    let provides_decorations = display
        .clone()
        .downcast::<gdk4_wayland::WaylandDisplay>()
        .ok()
        .map(|display| display.query_registry("zxdg_decoration_manager_v1"))
        .unwrap_or(false);

    let header = if provides_decorations {
        None
    } else {
        let bar = adw::HeaderBar::new();
        bar.set_title_widget(Some(&gtk::Label::builder().label(&title).build()));
        Some(bar)
    };

    let stack = gtk::Stack::new();
    stack.set_transition_type(gtk::StackTransitionType::None);
    stack.set_hexpand(true);
    stack.set_vexpand(true);
    stack.add_css_class("limux-content");

    let sidebar_list = gtk::ListBox::new();
    sidebar_list.set_selection_mode(gtk::SelectionMode::Single);
    sidebar_list.add_css_class("navigation-sidebar");

    let sidebar_scroll = gtk::ScrolledWindow::builder()
        .hscrollbar_policy(gtk::PolicyType::Never)
        .vscrollbar_policy(gtk::PolicyType::Automatic)
        .vexpand(true)
        .child(&sidebar_list)
        .build();

    let sidebar_title_label = gtk::Label::builder()
        .label("WORKSPACES")
        .xalign(0.0)
        .hexpand(true)
        .margin_start(12)
        .build();
    sidebar_title_label.add_css_class("limux-sidebar-title");

    let sidebar_title = gtk::Box::builder()
        .orientation(gtk::Orientation::Horizontal)
        .margin_top(8)
        .margin_bottom(4)
        .margin_end(6)
        .build();
    sidebar_title.append(&sidebar_title_label);

    {
        let window = window.clone();
        let drag_title = sidebar_title.clone();
        let drag = gtk::GestureClick::new();
        drag.set_button(1);
        drag.connect_pressed(move |gesture, _, x, y| {
            let Some(device) = gesture.current_event_device() else {
                return;
            };
            let button = gesture.current_button() as i32;
            let timestamp = gesture.current_event_time();
            begin_window_move_from_widget(&drag_title, &window, &device, button, x, y, timestamp);
            gesture.set_state(gtk::EventSequenceState::Claimed);
        });
        sidebar_title.add_controller(drag);
    }

    let new_ws_btn = gtk::Button::builder()
        .label("New Workspace")
        .hexpand(true)
        .margin_start(6)
        .margin_end(6)
        .margin_bottom(6)
        .build();
    new_ws_btn.add_css_class("limux-sidebar-btn");

    // Drop target on the button: workspace drags delete, tab drags create a new workspace.
    let btn_drop = gtk::DropTarget::new(glib::Type::STRING, gtk::gdk::DragAction::MOVE);
    btn_drop.set_preload(true);
    {
        let btn = new_ws_btn.clone();
        btn_drop.connect_motion(move |_, _, _| {
            if pane::is_tab_dragging() {
                btn.add_css_class("limux-tab-drop-target");
            } else {
                btn.add_css_class("limux-sidebar-btn-trash-hover");
            }
            gtk::gdk::DragAction::MOVE
        });
    }
    {
        let btn = new_ws_btn.clone();
        btn_drop.connect_leave(move |_| {
            btn.remove_css_class("limux-sidebar-btn-trash-hover");
            btn.remove_css_class("limux-tab-drop-target");
        });
    }
    new_ws_btn.add_controller(btn_drop.clone());

    let sidebar = gtk::Box::builder()
        .orientation(gtk::Orientation::Vertical)
        .spacing(4)
        .build();
    sidebar.add_css_class("limux-sidebar");
    sidebar.append(&sidebar_title);
    sidebar.append(&sidebar_scroll);
    sidebar.append(&new_ws_btn);

    let (right_sidebar_shell, right_sidebar_title_label, right_sidebar_body) =
        build_right_sidebar_panel();
    let (main_split, sidebar_shell, sidebar_handle) =
        build_sidebar_split(&sidebar, &stack, &right_sidebar_shell);

    let vbox = gtk::Box::new(gtk::Orientation::Vertical, 0);
    if let Some(ref header) = header {
        vbox.append(header);
    }
    vbox.append(&main_split);
    window.set_content(Some(&vbox));

    let state: State = Rc::new(RefCell::new(AppState {
        app: app.clone(),
        window: window.clone(),
        top_bar: header.clone(),
        top_bar_visible: true,
        config,
        system_prefers_dark: system_prefers_dark.clone(),
        workspaces: Vec::new(),
        workspace_groups: Vec::new(),
        active_idx: 0,
        previous_workspace_id: None,
        shortcuts,
        stack: stack.clone(),
        sidebar_list: sidebar_list.clone(),
        sidebar_shell: sidebar_shell.clone(),
        sidebar_handle: sidebar_handle.clone(),
        right_sidebar_shell: right_sidebar_shell.clone(),
        right_sidebar_title_label: right_sidebar_title_label.clone(),
        right_sidebar_body: right_sidebar_body.clone(),
        new_ws_btn: new_ws_btn.clone(),
        sidebar_animation: None,
        sidebar_animation_epoch: 0,
        sidebar_expanded_width: SIDEBAR_WIDTH,
        right_sidebar_visible: false,
        right_sidebar_mode: RightSidebarMode::Files,
        right_sidebar_focused: false,
        persistence_suspended: false,
        save_queued: false,
        workspace_dragging: None,
        next_notification_id: 1,
        next_sidebar_log_id: 1,
        notifications: Vec::new(),
        desktop_notification_routes: HashMap::new(),
        _theme_portal_signal: None,
        _theme_gnome_settings: None,
        _theme_gnome_signal: None,
        _desktop_notification_token_signal: None,
        _desktop_notification_action_signal: None,
        _desktop_notification_closed_signal: None,
    }));
    {
        let mut app_state = state.borrow_mut();
        sync_right_sidebar_panel(&mut app_state);
    }
    CONTROL_STATE.with(|slot| {
        *slot.borrow_mut() = Some(state.clone());
    });

    install_sidebar_resize(&state, &main_split, &sidebar, &sidebar_shell);

    {
        let state = state.clone();
        let system_prefers_dark = system_prefers_dark.clone();
        style_manager.connect_dark_notify(move |style_manager| {
            sync_ghostty_color_scheme_for_config(
                style_manager,
                system_prefers_dark.get(),
                &state.borrow().config.borrow().appearance,
            );
        });
    }

    let theme_gnome_signal = gnome_interface_settings.as_ref().map(|settings| {
        connect_gnome_appearance_watch(
            settings,
            state.clone(),
            style_manager.clone(),
            system_prefers_dark.clone(),
            portal_color_scheme_preference.clone(),
        )
    });
    {
        let mut s = state.borrow_mut();
        s._theme_gnome_settings = gnome_interface_settings.clone();
        s._theme_gnome_signal = theme_gnome_signal;
    }
    connect_portal_appearance_watch_async(
        gnome_interface_settings.clone(),
        state.clone(),
        style_manager.clone(),
        system_prefers_dark.clone(),
        portal_color_scheme_preference.clone(),
    );
    connect_desktop_notification_watch_async(state.clone());

    apply_shortcuts_to_application(app, &state.borrow().shortcuts);

    {
        let state = state.clone();
        window.connect_fullscreened_notify(move |_| {
            sync_top_bar_visibility(&state);
        });
    }

    register_app_actions(app, &state);
    register_window_actions(&window, &state);
    install_key_capture(&window, &state);

    // Any click anywhere in the window commits an active sidebar rename,
    // UNLESS the click is inside the rename Entry itself.
    {
        let sl = sidebar_list.clone();
        let win = window.clone();
        let click_anywhere = gtk::GestureClick::new();
        click_anywhere.set_propagation_phase(gtk::PropagationPhase::Capture);
        click_anywhere.connect_pressed(move |_, _, x, y| {
            if let Some(entry) = find_active_rename_entry(&sl) {
                // Translate click coords from window to the entry's coordinate space
                if let Some((ex, ey)) = win.translate_coordinates(&entry, x, y) {
                    let alloc = entry.allocation();
                    if ex >= 0.0
                        && ey >= 0.0
                        && ex <= alloc.width() as f64
                        && ey <= alloc.height() as f64
                    {
                        return; // click is inside the entry
                    }
                }
                commit_any_active_rename(&sl);
            }
        });
        window.add_controller(click_anywhere);
    }

    {
        let state = state.clone();
        sidebar_list.connect_row_selected(move |_, row| {
            if let Some(row) = row {
                let idx = row.index() as usize;
                switch_workspace(&state, idx);
            }
        });
    }

    {
        let state = state.clone();
        new_ws_btn.connect_clicked(move |_| {
            add_workspace(&state, None);
        });
    }

    {
        let btn = new_ws_btn.clone();
        pane::on_tab_drag_change(move |dragging| {
            if dragging {
                btn.add_css_class("limux-tab-drag-active");
            } else {
                btn.remove_css_class("limux-tab-drag-active");
                btn.remove_css_class("limux-tab-drop-target");
            }
        });
    }

    {
        let state = state.clone();
        let btn = new_ws_btn.clone();
        btn_drop.connect_drop(move |_, value, _, _| {
            btn.set_label("New Workspace");
            btn.remove_css_class("limux-sidebar-btn-trash");
            btn.remove_css_class("limux-sidebar-btn-trash-hover");
            btn.remove_css_class("limux-tab-drop-target");
            if let Ok(payload) = value.get::<String>() {
                if payload.contains(':') {
                    return create_workspace_for_tab(&state, &payload);
                }
                close_workspace_by_id(&state, &payload);
                return true;
            }
            false
        });
    }

    // Save the full session on window close.
    {
        let state = state.clone();
        window.connect_close_request(move |_| {
            save_session_now(&state);
            CONTROL_STATE.with(|slot| {
                slot.borrow_mut().take();
            });
            glib::Propagation::Proceed
        });
    }

    apply_loaded_session(&state, layout_state::load_session());

    crate::control_bridge::start(dispatch_control_command);

    window.present();
}

fn build_window_css(background_opacity: f64) -> String {
    let background_opacity = sanitize_background_opacity(background_opacity);
    let (r, g, b) = CONTENT_BACKGROUND_RGB;
    format!(
        "{BASE_CSS}\n.limux-content {{\n    background-color: rgba({r}, {g}, {b}, {background_opacity:.3});\n}}\n"
    )
}

/// purpose: Build the CMUX-compatible right sidebar shell and retained-metadata body.
/// inputs: None.
/// returns/effects: Returns shell/title/body widgets for host state ownership.
fn build_right_sidebar_panel() -> (gtk::Box, gtk::Label, gtk::Box) {
    let title = gtk::Label::builder()
        .label("RIGHT SIDEBAR")
        .xalign(0.0)
        .wrap(true)
        .build();
    title.add_css_class("limux-right-sidebar-title");

    let body = gtk::Box::builder()
        .orientation(gtk::Orientation::Vertical)
        .spacing(2)
        .vexpand(true)
        .build();

    let scroll = gtk::ScrolledWindow::builder()
        .hscrollbar_policy(gtk::PolicyType::Never)
        .vscrollbar_policy(gtk::PolicyType::Automatic)
        .vexpand(true)
        .child(&body)
        .build();

    let shell = gtk::Box::builder()
        .orientation(gtk::Orientation::Vertical)
        .width_request(RIGHT_SIDEBAR_WIDTH)
        .hexpand(false)
        .vexpand(true)
        .build();
    shell.add_css_class("limux-right-sidebar");
    shell.append(&title);
    shell.append(&scroll);
    shell.set_visible(false);
    (shell, title, body)
}

fn build_sidebar_split(
    sidebar: &gtk::Box,
    stack: &gtk::Stack,
    right_sidebar: &gtk::Box,
) -> (gtk::Box, gtk::Box, gtk::Box) {
    let sidebar_shell = gtk::Box::builder()
        .orientation(gtk::Orientation::Vertical)
        .hexpand(false)
        .vexpand(true)
        .build();
    sidebar_shell.append(sidebar);
    set_sidebar_width(&sidebar_shell, SIDEBAR_WIDTH);

    let sidebar_handle = gtk::Box::builder()
        .orientation(gtk::Orientation::Vertical)
        .width_request(SIDEBAR_RESIZE_HANDLE_WIDTH_PX)
        .hexpand(false)
        .vexpand(true)
        .build();
    sidebar_handle.add_css_class(SIDEBAR_HANDLE_CSS_CLASS);
    sidebar_handle.set_cursor_from_name(Some(SIDEBAR_HANDLE_CURSOR_NAME));

    let main_split = gtk::Box::builder()
        .orientation(gtk::Orientation::Horizontal)
        .hexpand(true)
        .vexpand(true)
        .build();
    main_split.append(&sidebar_shell);
    main_split.append(&sidebar_handle);
    main_split.append(stack);
    main_split.append(right_sidebar);

    (main_split, sidebar_shell, sidebar_handle)
}

fn install_sidebar_resize(
    state: &State,
    main_split: &gtk::Box,
    sidebar: &gtk::Box,
    sidebar_shell: &gtk::Box,
) {
    let resizing_sidebar = Rc::new(Cell::new(false));
    let drag_origin = Rc::new(Cell::new(SIDEBAR_WIDTH));
    let drag = gtk::GestureDrag::new();

    {
        let drag_origin = drag_origin.clone();
        let sidebar = sidebar.clone();
        let sidebar_shell = sidebar_shell.clone();
        let resizing_sidebar = resizing_sidebar.clone();
        drag.connect_drag_begin(move |gesture, x, _| {
            let current_width = sidebar_width(&sidebar_shell);
            let handle_start = current_width as f64;
            let handle_end = handle_start + SIDEBAR_RESIZE_HANDLE_WIDTH_PX as f64;
            if x < handle_start || x > handle_end {
                gesture.set_state(gtk::EventSequenceState::Denied);
                return;
            }
            resizing_sidebar.set(true);
            drag_origin.set(current_width.max(sidebar_min_width(&sidebar)));
            gesture.set_state(gtk::EventSequenceState::Claimed);
        });
    }

    {
        let drag_origin = drag_origin.clone();
        let sidebar = sidebar.clone();
        let sidebar_shell = sidebar_shell.clone();
        let resizing_sidebar = resizing_sidebar.clone();
        let state = state.clone();
        drag.connect_drag_update(move |_, offset_x, _| {
            if !resizing_sidebar.get() {
                return;
            }
            let min_width = sidebar_min_width(&sidebar);
            let width = (drag_origin.get() as f64 + offset_x).round() as i32;
            let width = width.max(min_width);
            set_sidebar_width(&sidebar_shell, width);
            state.borrow_mut().sidebar_expanded_width = width;
        });
    }

    {
        let sidebar_shell = sidebar_shell.clone();
        let resizing_sidebar = resizing_sidebar.clone();
        let state = state.clone();
        drag.connect_drag_end(move |_, _, _| {
            resizing_sidebar.set(false);
            state.borrow_mut().sidebar_expanded_width = sidebar_width(&sidebar_shell);
            request_session_save(&state);
        });
    }

    main_split.add_controller(drag);
}

fn set_sidebar_width(sidebar_shell: &gtk::Box, width: i32) {
    sidebar_shell.set_width_request(width.max(0));
}

fn set_sidebar_state_widgets(
    sidebar_shell: &gtk::Box,
    sidebar_handle: &gtk::Box,
    width: i32,
    visible: bool,
) {
    set_sidebar_width(sidebar_shell, width);
    sidebar_shell.set_visible(visible);
    sidebar_handle.set_visible(visible);
}

fn sidebar_width(sidebar_shell: &gtk::Box) -> i32 {
    sidebar_shell.width_request().max(0)
}

fn sidebar_min_width(sidebar: &gtk::Box) -> i32 {
    let (minimum, _, _, _) = sidebar.measure(gtk::Orientation::Horizontal, -1);
    minimum.max(1)
}

fn sanitize_background_opacity(background_opacity: f64) -> f64 {
    if background_opacity.is_finite() {
        background_opacity.clamp(0.0, 1.0)
    } else {
        1.0
    }
}

fn use_opaque_window_background(background_opacity: f64) -> bool {
    sanitize_background_opacity(background_opacity) >= 1.0
}

fn apply_window_background_class(window: &adw::ApplicationWindow, background_opacity: f64) {
    if use_opaque_window_background(background_opacity) {
        window.add_css_class("background");
    } else {
        window.remove_css_class("background");
    }
}

// ---------------------------------------------------------------------------
// Actions
// ---------------------------------------------------------------------------

fn register_window_actions(window: &adw::ApplicationWindow, state: &State) {
    let action_defs: Vec<(&'static str, ShortcutCommand)> = {
        let s = state.borrow();
        s.shortcuts
            .shortcuts
            .iter()
            .filter(|shortcut| shortcut.definition.action_name.starts_with("win."))
            .map(|shortcut| {
                (
                    shortcut.definition.action_basename(),
                    shortcut.definition.command,
                )
            })
            .collect()
    };

    for (name, command) in action_defs {
        let action = gtk::gio::SimpleAction::new(name, None);
        let state = state.clone();
        action.connect_activate(move |_, _| {
            dispatch_shortcut_command(&state, command);
        });
        window.add_action(&action);
    }
}

fn register_app_actions(app: &adw::Application, state: &State) {
    let action_defs: Vec<(&'static str, ShortcutCommand)> = {
        let s = state.borrow();
        s.shortcuts
            .shortcuts
            .iter()
            .filter(|shortcut| shortcut.definition.action_name.starts_with("app."))
            .map(|shortcut| {
                (
                    shortcut.definition.action_basename(),
                    shortcut.definition.command,
                )
            })
            .collect()
    };

    for (name, command) in action_defs {
        if app.lookup_action(name).is_some() {
            continue;
        }
        let action = gtk::gio::SimpleAction::new(name, None);
        let state = state.clone();
        action.connect_activate(move |_, _| {
            dispatch_shortcut_command(&state, command);
        });
        app.add_action(&action);
    }
}

/// Intercept keyboard shortcuts in the CAPTURE phase for window-level bindings.
fn install_key_capture(window: &adw::ApplicationWindow, state: &State) {
    let key_controller = gtk::EventControllerKey::new();
    key_controller.set_propagation_phase(gtk::PropagationPhase::Capture);

    let state = state.clone();
    key_controller.connect_key_pressed(move |controller, keyval, keycode, modifier| {
        let focused_listening_editor = controller
            .widget()
            .and_then(|widget| widget.downcast::<gtk::Window>().ok())
            .map(|window| focused_widget_is_listening_for_keybind_capture(&window))
            .unwrap_or(false);
        if focused_listening_editor {
            return glib::Propagation::Proceed;
        }

        let matched = {
            let s = state.borrow();
            let display = controller.widget().map(|widget| widget.display());
            shortcut_match_from_key_press(&s.shortcuts, display.as_ref(), keyval, keycode, modifier)
        }
        .filter(|matched| {
            let context = controller
                .widget()
                .and_then(|widget| widget.downcast::<gtk::Window>().ok())
                .map(|window| focused_editable_capture_context(&state, &window))
                .unwrap_or_default();
            !shortcut_blocked_by_editable(matched.command, matched.editable_capture_policy, context)
        })
        .map(|matched| dispatch_shortcut_command(&state, matched.command))
        .unwrap_or(false);

        shortcut_dispatch_propagation(matched)
    });

    window.add_controller(key_controller);
}

fn focused_widget_is_listening_for_keybind_capture(window: &gtk::Window) -> bool {
    let mut widget = gtk::prelude::GtkWindowExt::focus(window);
    while let Some(current) = widget {
        if current.has_css_class(keybind_editor::KEYBIND_EDITOR_LISTENING_CSS) {
            return true;
        }
        widget = current.parent();
    }
    false
}

fn focused_widget_is_editable(window: &gtk::Window) -> bool {
    let mut widget = gtk::prelude::GtkWindowExt::focus(window);
    while let Some(current) = widget {
        if current.is::<gtk::Entry>()
            || current.is::<gtk::SearchEntry>()
            || current.is::<gtk::TextView>()
        {
            return true;
        }
        widget = current.parent();
    }
    false
}

fn focused_editable_capture_context(state: &State, window: &gtk::Window) -> EditableCaptureContext {
    let gtk_editable = focused_widget_is_editable(window);
    match focused_shortcut_target(state) {
        pane::FocusedShortcutTarget::Browser(target) => EditableCaptureContext {
            gtk_editable,
            browser_dom_editable: target.is_page_editable(),
            browser_find_active: target.is_find_active(),
        },
        _ => EditableCaptureContext {
            gtk_editable,
            ..EditableCaptureContext::default()
        },
    }
}

fn shortcut_allowed_while_browser_find_active(command: ShortcutCommand) -> bool {
    matches!(
        command,
        ShortcutCommand::SurfaceFindNext
            | ShortcutCommand::SurfaceFindPrevious
            | ShortcutCommand::SurfaceFindHide
    )
}

fn shortcut_blocked_by_editable(
    command: ShortcutCommand,
    policy: EditableCapturePolicy,
    context: EditableCaptureContext,
) -> bool {
    if policy == EditableCapturePolicy::AlwaysCapture {
        return false;
    }

    if context.browser_find_active && shortcut_allowed_while_browser_find_active(command) {
        return false;
    }

    context.gtk_editable || context.browser_dom_editable
}

fn shortcut_dispatch_propagation(matched: bool) -> glib::Propagation {
    if matched {
        glib::Propagation::Stop
    } else {
        glib::Propagation::Proceed
    }
}

#[cfg(test)]
fn shortcut_command_from_key_event(
    shortcuts: &ResolvedShortcutConfig,
    keyval: gtk::gdk::Key,
    modifier: gtk::gdk::ModifierType,
) -> Option<ShortcutCommand> {
    shortcut_config::NormalizedShortcut::from_gdk_key(keyval, modifier)
        .map(|shortcut| shortcut.to_runtime_combo())
        .and_then(|combo| shortcuts.command_for_runtime_combo(&combo))
}

struct MatchedShortcut {
    command: ShortcutCommand,
    editable_capture_policy: EditableCapturePolicy,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
struct EditableCaptureContext {
    gtk_editable: bool,
    browser_dom_editable: bool,
    browser_find_active: bool,
}

fn shortcut_match_from_key_press(
    shortcuts: &ResolvedShortcutConfig,
    display: Option<&gtk::gdk::Display>,
    keyval: gtk::gdk::Key,
    keycode: u32,
    modifier: gtk::gdk::ModifierType,
) -> Option<MatchedShortcut> {
    shortcut_config::NormalizedShortcut::from_gdk_key_event(display, keyval, keycode, modifier)
        .map(|shortcut| shortcut.to_runtime_combo())
        .and_then(|combo| shortcuts.shortcut_for_runtime_combo(&combo))
        .map(|shortcut| MatchedShortcut {
            command: shortcut.definition.command,
            editable_capture_policy: shortcut.definition.editable_capture_policy,
        })
}

fn dispatch_shortcut_command(state: &State, command: ShortcutCommand) -> bool {
    match command {
        ShortcutCommand::NewWorkspace => {
            add_workspace(state, None);
            true
        }
        ShortcutCommand::CloseWorkspace => {
            close_workspace(state);
            true
        }
        ShortcutCommand::QuitApp => {
            quit_app(state);
            true
        }
        ShortcutCommand::NewInstance => spawn_new_instance(state),
        ShortcutCommand::ToggleSidebar => {
            toggle_sidebar(state);
            true
        }
        ShortcutCommand::ToggleTopBar => {
            toggle_top_bar(state);
            true
        }
        ShortcutCommand::ToggleFullscreen => {
            toggle_fullscreen(state);
            true
        }
        ShortcutCommand::NextWorkspace => {
            cycle_workspace(state, 1);
            true
        }
        ShortcutCommand::PrevWorkspace => {
            cycle_workspace(state, -1);
            true
        }
        ShortcutCommand::CycleTabPrev => {
            cycle_focused_pane_tab(state, -1);
            true
        }
        ShortcutCommand::CycleTabNext => {
            cycle_focused_pane_tab(state, 1);
            true
        }
        ShortcutCommand::SplitDown => {
            split_focused_pane(state, gtk::Orientation::Vertical);
            true
        }
        ShortcutCommand::NewTerminal => {
            add_tab_to_focused_pane(state, false);
            true
        }
        ShortcutCommand::SplitRight => {
            split_focused_pane(state, gtk::Orientation::Horizontal);
            true
        }
        ShortcutCommand::CloseFocusedPane => {
            close_focused_tab(state);
            true
        }
        ShortcutCommand::ToggleFocusedPaneZoom => {
            toggle_focused_pane_zoom(state);
            true
        }
        ShortcutCommand::FocusLeft => {
            focus_pane_in_direction(state, Direction::Left);
            true
        }
        ShortcutCommand::FocusRight => {
            focus_pane_in_direction(state, Direction::Right);
            true
        }
        ShortcutCommand::FocusUp => {
            focus_pane_in_direction(state, Direction::Up);
            true
        }
        ShortcutCommand::FocusDown => {
            focus_pane_in_direction(state, Direction::Down);
            true
        }
        ShortcutCommand::ActivateWorkspace1 => {
            activate_workspace_shortcut(state, 0);
            true
        }
        ShortcutCommand::ActivateWorkspace2 => {
            activate_workspace_shortcut(state, 1);
            true
        }
        ShortcutCommand::ActivateWorkspace3 => {
            activate_workspace_shortcut(state, 2);
            true
        }
        ShortcutCommand::ActivateWorkspace4 => {
            activate_workspace_shortcut(state, 3);
            true
        }
        ShortcutCommand::ActivateWorkspace5 => {
            activate_workspace_shortcut(state, 4);
            true
        }
        ShortcutCommand::ActivateWorkspace6 => {
            activate_workspace_shortcut(state, 5);
            true
        }
        ShortcutCommand::ActivateWorkspace7 => {
            activate_workspace_shortcut(state, 6);
            true
        }
        ShortcutCommand::ActivateWorkspace8 => {
            activate_workspace_shortcut(state, 7);
            true
        }
        ShortcutCommand::ActivateLastWorkspace => {
            activate_last_workspace_shortcut(state);
            true
        }
        ShortcutCommand::OpenBrowserInSplit
        | ShortcutCommand::BrowserFocusLocation
        | ShortcutCommand::BrowserBack
        | ShortcutCommand::BrowserForward
        | ShortcutCommand::BrowserReload
        | ShortcutCommand::BrowserInspector
        | ShortcutCommand::BrowserConsole => dispatch_browser_command(state, command),
        ShortcutCommand::SurfaceFind
        | ShortcutCommand::SurfaceFindNext
        | ShortcutCommand::SurfaceFindPrevious
        | ShortcutCommand::SurfaceFindHide
        | ShortcutCommand::SurfaceUseSelectionForFind => {
            dispatch_terminal_command(state, command) || dispatch_browser_command(state, command)
        }
        ShortcutCommand::TerminalClearScrollback
        | ShortcutCommand::TerminalCopy
        | ShortcutCommand::TerminalPaste
        | ShortcutCommand::TerminalIncreaseFontSize
        | ShortcutCommand::TerminalDecreaseFontSize
        | ShortcutCommand::TerminalResetFontSize => dispatch_terminal_command(state, command),
    }
}

fn apply_shortcuts_to_application(app: &adw::Application, shortcuts: &ResolvedShortcutConfig) {
    for (action_name, accels) in shortcuts.gtk_accel_entries() {
        let accel_refs: Vec<&str> = accels.iter().map(String::as_str).collect();
        app.set_accels_for_action(action_name, &accel_refs);
    }
}

fn apply_shortcut_config(state: &State, shortcuts: ResolvedShortcutConfig) {
    let (app, workspace_roots, shortcuts_rc) = {
        let mut s = state.borrow_mut();
        s.shortcuts = Rc::new(shortcuts);
        (
            s.app.clone(),
            s.workspaces
                .iter()
                .map(|ws| ws.root.clone())
                .collect::<Vec<_>>(),
            s.shortcuts.clone(),
        )
    };

    apply_shortcuts_to_application(&app, &shortcuts_rc);
    for root in workspace_roots {
        refresh_shortcut_tooltips_in_layout(&root, &shortcuts_rc);
    }
}

fn refresh_shortcut_tooltips_in_layout(widget: &gtk::Widget, shortcuts: &ResolvedShortcutConfig) {
    if let Some(paned) = widget.downcast_ref::<gtk::Paned>() {
        if let Some(start) = paned.start_child() {
            refresh_shortcut_tooltips_in_layout(&start, shortcuts);
        }
        if let Some(end) = paned.end_child() {
            refresh_shortcut_tooltips_in_layout(&end, shortcuts);
        }
        return;
    }

    pane::refresh_shortcut_tooltips(widget, shortcuts);
}

fn persist_shortcut_binding(
    state: &State,
    id: ShortcutId,
    binding: Option<shortcut_config::NormalizedShortcut>,
) -> Result<ResolvedShortcutConfig, String> {
    let updated = {
        let s = state.borrow();
        s.shortcuts
            .with_binding(id, binding)
            .map_err(|err| err.to_string())?
    };

    let Some(path) = shortcut_config::shortcuts_path() else {
        return Err("config directory unavailable".to_string());
    };

    shortcut_config::write_shortcuts(&path, &updated).map_err(|err| err.to_string())?;
    let display = {
        let s = state.borrow();
        s.stack.display()
    };
    let reloaded = shortcut_config::load_shortcuts_or_default_with_display(&path, Some(&display));
    if !reloaded.warnings.is_empty() {
        return Err(reloaded.warnings.join("; "));
    }

    apply_shortcut_config(state, reloaded.clone());
    Ok(reloaded)
}

fn adw_color_scheme_for(scheme: app_config::ColorScheme) -> adw::ColorScheme {
    match scheme {
        app_config::ColorScheme::System => adw::ColorScheme::Default,
        app_config::ColorScheme::Dark => adw::ColorScheme::ForceDark,
        app_config::ColorScheme::Light => adw::ColorScheme::ForceLight,
    }
}

fn gnome_interface_settings() -> Option<gio::Settings> {
    let schema = gio::SettingsSchemaSource::default()?.lookup(GNOME_INTERFACE_SCHEMA, true)?;
    if !schema.has_key(GNOME_COLOR_SCHEME_KEY) {
        return None;
    }

    Some(gio::Settings::new_full(
        &schema,
        None::<&gio::SettingsBackend>,
        None::<&str>,
    ))
}

fn gnome_prefers_dark_from_raw(raw: &str) -> Option<bool> {
    match raw {
        "prefer-dark" => Some(true),
        "default" | "prefer-light" => Some(false),
        _ => None,
    }
}

fn gnome_prefers_dark(settings: &gio::Settings) -> Option<bool> {
    gnome_prefers_dark_from_raw(settings.string(GNOME_COLOR_SCHEME_KEY).as_str())
}

#[cfg(test)]
fn gtk_system_prefers_dark_from_raw(raw: Option<i32>) -> Option<bool> {
    match raw {
        Some(value) if value == gtk::ffi::GTK_INTERFACE_COLOR_SCHEME_DARK => Some(true),
        Some(value)
            if value == gtk::ffi::GTK_INTERFACE_COLOR_SCHEME_LIGHT
                || value == gtk::ffi::GTK_INTERFACE_COLOR_SCHEME_DEFAULT =>
        {
            Some(false)
        }
        Some(value) if value == gtk::ffi::GTK_INTERFACE_COLOR_SCHEME_UNSUPPORTED => None,
        Some(_) => Some(false),
        None => None,
    }
}

fn resolve_system_prefers_dark(
    portal_color_scheme_preference: PortalColorSchemePreference,
    gnome_interface_settings: Option<&gio::Settings>,
) -> Option<bool> {
    resolved_system_prefers_dark(
        portal_color_scheme_preference,
        gnome_interface_settings.and_then(gnome_prefers_dark),
    )
}

fn resolved_system_prefers_dark(
    portal_color_scheme_preference: PortalColorSchemePreference,
    gnome_prefers_dark: Option<bool>,
) -> Option<bool> {
    portal_color_scheme_preference.resolved(gnome_prefers_dark)
}

fn portal_color_scheme_preference_from_response(
    response: &glib::Variant,
) -> Option<PortalColorSchemePreference> {
    let value = response.try_child_get::<glib::Variant>(0).ok().flatten()?;
    PortalColorSchemePreference::from_raw(value.try_get::<u32>().ok()?)
}

fn portal_setting_changed_preference(
    parameters: &glib::Variant,
) -> Option<PortalColorSchemePreference> {
    let (namespace, key, value) = parameters
        .try_get::<(String, String, glib::Variant)>()
        .ok()?;
    if namespace != PORTAL_APPEARANCE_NAMESPACE || key != PORTAL_COLOR_SCHEME_KEY {
        return None;
    }

    PortalColorSchemePreference::from_raw(value.try_get::<u32>().ok()?)
}

fn sync_system_prefers_dark_change(
    state: &State,
    style_manager: &adw::StyleManager,
    system_prefers_dark: &Cell<Option<bool>>,
    updated_preference: Option<bool>,
) {
    if updated_preference == system_prefers_dark.get() {
        return;
    }

    system_prefers_dark.set(updated_preference);
    sync_ghostty_color_scheme_for_config(
        style_manager,
        updated_preference,
        &state.borrow().config.borrow().appearance,
    );
}

fn sync_portal_color_scheme_preference_change(
    state: &State,
    style_manager: &adw::StyleManager,
    system_prefers_dark: &Cell<Option<bool>>,
    portal_color_scheme_preference: &Cell<PortalColorSchemePreference>,
    gnome_interface_settings: Option<&gio::Settings>,
    updated_preference: PortalColorSchemePreference,
) {
    if updated_preference == portal_color_scheme_preference.get() {
        return;
    }

    portal_color_scheme_preference.set(updated_preference);
    let resolved_preference =
        resolve_system_prefers_dark(updated_preference, gnome_interface_settings);
    sync_system_prefers_dark_change(
        state,
        style_manager,
        system_prefers_dark,
        resolved_preference,
    );
}

fn connect_portal_appearance_watch_async(
    gnome_interface_settings: Option<gio::Settings>,
    state: State,
    style_manager: adw::StyleManager,
    system_prefers_dark: Rc<Cell<Option<bool>>>,
    portal_color_scheme_preference: Rc<Cell<PortalColorSchemePreference>>,
) {
    gio::DBusProxy::for_bus(
        gio::BusType::Session,
        gio::DBusProxyFlags::NONE,
        None::<&gio::DBusInterfaceInfo>,
        PORTAL_DESKTOP_SERVICE,
        PORTAL_DESKTOP_PATH,
        PORTAL_SETTINGS_INTERFACE,
        None::<&gio::Cancellable>,
        move |result| {
            let Ok(proxy) = result else {
                return;
            };

            read_portal_appearance_preference_async(
                &proxy,
                gnome_interface_settings.clone(),
                state.clone(),
                style_manager.clone(),
                system_prefers_dark.clone(),
                portal_color_scheme_preference.clone(),
            );

            let subscription = connect_portal_appearance_watch(
                &proxy,
                gnome_interface_settings.clone(),
                state.clone(),
                style_manager.clone(),
                system_prefers_dark.clone(),
                portal_color_scheme_preference.clone(),
            );
            state.borrow_mut()._theme_portal_signal = subscription;
        },
    );
}

fn read_portal_appearance_preference_async(
    proxy: &gio::DBusProxy,
    gnome_interface_settings: Option<gio::Settings>,
    state: State,
    style_manager: adw::StyleManager,
    system_prefers_dark: Rc<Cell<Option<bool>>>,
    portal_color_scheme_preference: Rc<Cell<PortalColorSchemePreference>>,
) {
    let params = (PORTAL_APPEARANCE_NAMESPACE, PORTAL_COLOR_SCHEME_KEY).to_variant();
    proxy.call(
        "Read",
        Some(&params),
        gio::DBusCallFlags::NONE,
        PORTAL_THEME_READ_TIMEOUT_MS,
        None::<&gio::Cancellable>,
        move |result| {
            let Ok(response) = result else {
                return;
            };
            let Some(updated_preference) = portal_color_scheme_preference_from_response(&response)
            else {
                return;
            };
            sync_portal_color_scheme_preference_change(
                &state,
                &style_manager,
                system_prefers_dark.as_ref(),
                portal_color_scheme_preference.as_ref(),
                gnome_interface_settings.as_ref(),
                updated_preference,
            );
        },
    );
}

fn connect_portal_appearance_watch(
    proxy: &gio::DBusProxy,
    gnome_interface_settings: Option<gio::Settings>,
    state: State,
    style_manager: adw::StyleManager,
    system_prefers_dark: Rc<Cell<Option<bool>>>,
    portal_color_scheme_preference: Rc<Cell<PortalColorSchemePreference>>,
) -> Option<gio::SignalSubscription> {
    let connection = proxy.connection();
    Some(connection.subscribe_to_signal(
        Some(PORTAL_DESKTOP_SERVICE),
        Some(PORTAL_SETTINGS_INTERFACE),
        Some("SettingChanged"),
        Some(PORTAL_DESKTOP_PATH),
        Some(PORTAL_APPEARANCE_NAMESPACE),
        gio::DBusSignalFlags::NONE,
        move |signal| {
            let Some(updated_preference) = portal_setting_changed_preference(signal.parameters)
            else {
                return;
            };

            sync_portal_color_scheme_preference_change(
                &state,
                &style_manager,
                system_prefers_dark.as_ref(),
                portal_color_scheme_preference.as_ref(),
                gnome_interface_settings.as_ref(),
                updated_preference,
            );
        },
    ))
}

fn connect_desktop_notification_watch_async(state: State) {
    gio::DBusProxy::for_bus(
        gio::BusType::Session,
        gio::DBusProxyFlags::NONE,
        None::<&gio::DBusInterfaceInfo>,
        FREEDESKTOP_NOTIFICATIONS_SERVICE,
        FREEDESKTOP_NOTIFICATIONS_PATH,
        FREEDESKTOP_NOTIFICATIONS_INTERFACE,
        None::<&gio::Cancellable>,
        move |result| {
            let Ok(proxy) = result else {
                return;
            };

            let token_subscription =
                connect_desktop_notification_token_watch(&proxy, state.clone());
            let action_subscription =
                connect_desktop_notification_action_watch(&proxy, state.clone());
            let closed_subscription =
                connect_desktop_notification_closed_watch(&proxy, state.clone());
            let mut s = state.borrow_mut();
            s._desktop_notification_token_signal = token_subscription;
            s._desktop_notification_action_signal = action_subscription;
            s._desktop_notification_closed_signal = closed_subscription;
        },
    );
}

fn desktop_notification_id_from_response(response: &glib::Variant) -> Option<u32> {
    response
        .try_child_get::<u32>(0)
        .ok()
        .flatten()
        .or_else(|| response.try_get::<u32>().ok())
}

fn desktop_notification_action_from_signal(parameters: &glib::Variant) -> Option<(u32, String)> {
    parameters.try_get::<(u32, String)>().ok()
}

fn desktop_notification_activation_token_from_signal(
    parameters: &glib::Variant,
) -> Option<(u32, String)> {
    parameters.try_get::<(u32, String)>().ok()
}

fn desktop_notification_closed_id_from_signal(parameters: &glib::Variant) -> Option<u32> {
    parameters.try_get::<(u32, u32)>().ok().map(|(id, _)| id)
}

fn connect_desktop_notification_token_watch(
    proxy: &gio::DBusProxy,
    state: State,
) -> Option<gio::SignalSubscription> {
    let connection = proxy.connection();
    Some(connection.subscribe_to_signal(
        Some(FREEDESKTOP_NOTIFICATIONS_SERVICE),
        Some(FREEDESKTOP_NOTIFICATIONS_INTERFACE),
        Some("ActivationToken"),
        Some(FREEDESKTOP_NOTIFICATIONS_PATH),
        None,
        gio::DBusSignalFlags::NONE,
        move |signal| {
            let Some((notification_id, activation_token)) =
                desktop_notification_activation_token_from_signal(signal.parameters)
            else {
                return;
            };

            let mut s = state.borrow_mut();
            if let Some(route) = s.desktop_notification_routes.get_mut(&notification_id) {
                route.activation_token = Some(activation_token);
            }
        },
    ))
}

fn connect_desktop_notification_action_watch(
    proxy: &gio::DBusProxy,
    state: State,
) -> Option<gio::SignalSubscription> {
    let connection = proxy.connection();
    Some(connection.subscribe_to_signal(
        Some(FREEDESKTOP_NOTIFICATIONS_SERVICE),
        Some(FREEDESKTOP_NOTIFICATIONS_INTERFACE),
        Some("ActionInvoked"),
        Some(FREEDESKTOP_NOTIFICATIONS_PATH),
        None,
        gio::DBusSignalFlags::NONE,
        move |signal| {
            let Some((notification_id, action_key)) =
                desktop_notification_action_from_signal(signal.parameters)
            else {
                return;
            };

            let route = {
                let mut s = state.borrow_mut();
                s.desktop_notification_routes.remove(&notification_id)
            };
            let Some(route) = route else {
                return;
            };

            if action_key == "default" {
                activate_desktop_notification_target(
                    &state,
                    &route.target,
                    route.activation_token.as_deref(),
                );
                return;
            }
            let Some(decision) = route.feed_actions.get(&action_key) else {
                eprintln!("FATAL: Unknown desktop notification action key: {action_key}");
                return;
            };
            if let Err(error) = apply_feed_desktop_notification_decision(decision) {
                eprintln!("FATAL: Feed desktop notification action failed: {error:?}");
            }
        },
    ))
}

// purpose: Resolve one inline Feed desktop notification action.
// inputs: Feed decision stored in the desktop notification route.
// returns/effects: Calls the shared Feed reply path and wakes any blocked Feed push.
fn apply_feed_desktop_notification_decision(
    decision: &crate::feed::FeedNotificationDecision,
) -> Result<(), BridgeError> {
    match decision {
        crate::feed::FeedNotificationDecision::Permission { request_id, mode } => {
            reply_to_feed_permission_request(request_id, mode)
        }
        crate::feed::FeedNotificationDecision::ExitPlan { request_id, mode } => {
            reply_to_feed_exit_plan_request(request_id, mode)
        }
        crate::feed::FeedNotificationDecision::Question {
            request_id,
            selections,
        } => reply_to_feed_question_request(request_id, selections.clone()),
    }
}

fn connect_desktop_notification_closed_watch(
    proxy: &gio::DBusProxy,
    state: State,
) -> Option<gio::SignalSubscription> {
    let connection = proxy.connection();
    Some(connection.subscribe_to_signal(
        Some(FREEDESKTOP_NOTIFICATIONS_SERVICE),
        Some(FREEDESKTOP_NOTIFICATIONS_INTERFACE),
        Some("NotificationClosed"),
        Some(FREEDESKTOP_NOTIFICATIONS_PATH),
        None,
        gio::DBusSignalFlags::NONE,
        move |signal| {
            let Some(notification_id) =
                desktop_notification_closed_id_from_signal(signal.parameters)
            else {
                return;
            };

            state
                .borrow_mut()
                .desktop_notification_routes
                .remove(&notification_id);
        },
    ))
}

fn activate_desktop_notification_target(
    state: &State,
    target: &DesktopNotificationTarget,
    activation_token: Option<&str>,
) {
    let (workspace_idx, row, sidebar_list, window, workspace_changed) = {
        let s = state.borrow();
        let Some((idx, workspace)) = s
            .workspaces
            .iter()
            .enumerate()
            .find(|(_, workspace)| workspace.id == target.workspace_id)
        else {
            return;
        };

        (
            idx,
            workspace.sidebar_row.clone(),
            s.sidebar_list.clone(),
            s.window.clone(),
            idx != s.active_idx,
        )
    };

    if let Some(token) = activation_token.filter(|token| !token.is_empty()) {
        window.set_startup_id(token);
    }
    window.present();
    switch_workspace(state, workspace_idx);
    sidebar_list.select_row(Some(&row));

    let state_for_focus = state.clone();
    let target_for_focus = target.clone();
    if workspace_changed {
        glib::idle_add_local_once(move || {
            glib::idle_add_local_once(move || {
                focus_desktop_notification_target(&state_for_focus, &target_for_focus);
            });
        });
    } else {
        glib::idle_add_local_once(move || {
            focus_desktop_notification_target(&state_for_focus, &target_for_focus);
        });
    }
}

fn focus_desktop_notification_target(state: &State, target: &DesktopNotificationTarget) -> bool {
    if let Some(pane_id) = target.pane_id {
        if let Some(pane_widget) = pane::find_pane_widget_by_id(pane_id) {
            if let Some(tab_id) = target.tab_id.as_deref() {
                if pane::activate_tab_in_pane(&pane_widget, tab_id) {
                    return true;
                }
            }

            if pane::focus_active_tab_in_pane(&pane_widget) {
                return true;
            }
        }
    }

    let root = {
        let s = state.borrow();
        s.workspaces
            .iter()
            .find(|workspace| workspace.id == target.workspace_id)
            .map(|workspace| workspace.root.clone())
    };

    if let Some(root) = root {
        focus_workspace_entrypoint(&root);
        return true;
    }

    false
}

fn connect_gnome_appearance_watch(
    settings: &gio::Settings,
    state: State,
    style_manager: adw::StyleManager,
    system_prefers_dark: Rc<Cell<Option<bool>>>,
    portal_color_scheme_preference: Rc<Cell<PortalColorSchemePreference>>,
) -> glib::SignalHandlerId {
    settings.connect_changed(Some(GNOME_COLOR_SCHEME_KEY), move |settings, _| {
        let updated_preference =
            resolve_system_prefers_dark(portal_color_scheme_preference.get(), Some(settings));
        sync_system_prefers_dark_change(
            &state,
            &style_manager,
            system_prefers_dark.as_ref(),
            updated_preference,
        );
    })
}

fn ghostty_prefers_dark(
    scheme: app_config::ColorScheme,
    system_prefers_dark: Option<bool>,
    fallback_dark: bool,
) -> bool {
    match scheme {
        app_config::ColorScheme::Dark => true,
        app_config::ColorScheme::Light => false,
        app_config::ColorScheme::System => system_prefers_dark.unwrap_or(fallback_dark),
    }
}

fn sync_ghostty_color_scheme_for_config(
    style_manager: &adw::StyleManager,
    system_prefers_dark: Option<bool>,
    appearance: &app_config::AppearanceConfig,
) {
    let dark = ghostty_prefers_dark(
        appearance.ghostty_color_scheme,
        system_prefers_dark,
        style_manager.is_dark(),
    );
    crate::terminal::sync_color_scheme(dark);
}

fn apply_appearance(
    style_manager: &adw::StyleManager,
    system_prefers_dark: Option<bool>,
    appearance: &app_config::AppearanceConfig,
) {
    style_manager.set_color_scheme(adw_color_scheme_for(appearance.color_scheme));
    sync_ghostty_color_scheme_for_config(style_manager, system_prefers_dark, appearance);
}

// purpose: Convert a caught panic payload into a control-socket error detail.
// inputs: Panic payload from config loading.
// returns/effects: Returns a human-readable message without hiding the reload failure.
fn panic_payload_message(payload: Box<dyn std::any::Any + Send>) -> String {
    if let Some(message) = payload.downcast_ref::<&str>() {
        return (*message).to_string();
    }
    if let Some(message) = payload.downcast_ref::<String>() {
        return message.clone();
    }
    "unknown panic while reloading config".to_string()
}

// purpose: Apply a newly-loaded app config to live GTK and terminal state.
// inputs: App state, loaded config, and the previously active font size.
// returns/effects: Updates in-memory config, appearance, and terminal font size bindings.
fn apply_reloaded_app_config(
    state: &State,
    config: app_config::AppConfig,
    previous_font_size: Option<f32>,
) {
    let system_prefers_dark = state.borrow().system_prefers_dark.get();
    let style_manager = adw::StyleManager::default();
    apply_appearance(&style_manager, system_prefers_dark, &config.appearance);
    let next_font_size = config.font_size;
    let sidebar = config.sidebar.clone();
    state.borrow().config.borrow_mut().clone_from(&config);
    sync_sidebar_detail_rows(state, &sidebar);
    if next_font_size == previous_font_size {
        return;
    }
    match next_font_size {
        Some(size) => broadcast_font_size(size),
        None => crate::terminal::broadcast_binding_action("reset_font_size"),
    }
}

// purpose: Reload Limux settings and shortcuts for the running host.
// inputs: Shared app state receiving a control-socket reload request.
// returns/effects: Applies config, shortcuts, emits a retained CMUX config event, or returns an explicit error.
fn reload_config_for_control(state: &State) -> Result<serde_json::Value, BridgeError> {
    let loaded = std::panic::catch_unwind(std::panic::AssertUnwindSafe(app_config::load))
        .map_err(|payload| BridgeError::internal(panic_payload_message(payload)))?;
    let (display, previous_font_size) = {
        let app_state = state.borrow();
        let display = app_state.stack.display();
        let previous_font_size = app_state.config.borrow().font_size;
        (display, previous_font_size)
    };
    let shortcuts = shortcut_config::load_shortcuts_for_display(&display);
    apply_reloaded_app_config(state, loaded.config, previous_font_size);
    apply_shortcut_config(state, shortcuts.clone());
    let settings_path = app_config::settings_path()
        .map(|path| path.display().to_string())
        .unwrap_or_else(|| "<unavailable>".to_string());
    let shortcuts_path = shortcut_config::shortcuts_path()
        .map(|path| path.display().to_string())
        .unwrap_or_else(|| "<unavailable>".to_string());
    let payload = serde_json::json!({
        "ok": true,
        "reloaded": true,
        "settings_path": settings_path,
        "shortcuts_path": shortcuts_path,
        "settings_warnings": loaded.warnings,
        "shortcut_warnings": shortcuts.warnings,
        "applied": {
            "settings": true,
            "shortcuts": true,
            "appearance": true,
            "terminal_font_size": true,
        },
    });
    crate::event_bus::bus().publish(crate::event_bus::EventPublish {
        name: "config.reloaded",
        category: "config",
        source: "config.reload",
        workspace_id: None,
        surface_id: None,
        pane_id: None,
        payload: payload.clone(),
    });
    Ok(payload)
}

// purpose: Present the live host settings dialog for CMUX `settings open` parity.
// inputs: Shared app state plus optional CMUX settings target and activation flag.
// returns/effects: Opens a modal settings dialog and returns an acknowledgement.
fn open_settings_for_control(
    state: &State,
    target: Option<String>,
    activate: bool,
) -> Result<serde_json::Value, BridgeError> {
    let (window, config, shortcuts) = {
        let app_state = state.borrow();
        (
            app_state.window.clone(),
            app_state.config.clone(),
            app_state.shortcuts.clone(),
        )
    };
    let input = crate::settings_editor::SettingsEditorInput {
        config,
        shortcuts,
        initial_page: target.clone(),
        on_capture: {
            let state = state.clone();
            Rc::new(move |id, binding| persist_shortcut_binding(&state, id, binding))
        },
        on_config_changed: settings_dialog_config_changed_handler(state),
    };
    crate::settings_editor::present_settings_dialog(&window, input);
    if activate {
        window.present();
    }
    let settings_path = app_config::settings_path()
        .map(|path| path.display().to_string())
        .unwrap_or_else(|| "<unavailable>".to_string());
    let target = target.unwrap_or_else(|| "general".to_string());
    Ok(serde_json::json!({
        "ok": true,
        "opened": true,
        "target": target,
        "settings_path": settings_path,
    }))
}

// purpose: Build the settings dialog config-change callback used by host UI entry points.
// inputs: Shared app state.
// returns/effects: Returns a callback that applies appearance and persists settings.
fn settings_dialog_config_changed_handler(state: &State) -> Rc<SettingsConfigChangedHandler> {
    let state = state.clone();
    Rc::new(
        move |previous: &app_config::AppConfig, updated: &app_config::AppConfig| {
            let style_manager = adw::StyleManager::default();
            let system_prefers_dark = state.borrow().system_prefers_dark.get();
            apply_appearance(&style_manager, system_prefers_dark, &updated.appearance);
            if let Err(err) = app_config::save(updated) {
                state.borrow().config.borrow_mut().clone_from(previous);
                apply_appearance(&style_manager, system_prefers_dark, &previous.appearance);

                let detail = format!("Failed to save Limux settings: {err}");
                eprintln!("limux: {detail}");
                show_runtime_error(&state, "Failed to save settings", &detail);
            }
        },
    )
}

fn open_keybind_editor_tab(state: &State, pane_widget: &gtk::Widget) {
    let shortcuts = {
        let s = state.borrow();
        s.shortcuts.clone()
    };
    let on_capture: Rc<
        dyn Fn(
            ShortcutId,
            Option<shortcut_config::NormalizedShortcut>,
        ) -> Result<ResolvedShortcutConfig, String>,
    > = {
        let state = state.clone();
        Rc::new(move |id, binding| persist_shortcut_binding(&state, id, binding))
    };
    pane::add_keybind_editor_tab_to_pane(pane_widget, shortcuts, on_capture);
}

fn activate_workspace_shortcut(state: &State, idx: usize) {
    let row_and_list = {
        let s = state.borrow();
        s.workspaces
            .get(idx)
            .map(|ws| (idx, ws.sidebar_row.clone(), s.sidebar_list.clone()))
    };

    if let Some((idx, row, list)) = row_and_list {
        switch_workspace(state, idx);
        list.select_row(Some(&row));
    }
}

fn activate_last_workspace_shortcut(state: &State) {
    let last_idx = {
        let s = state.borrow();
        if s.workspaces.is_empty() {
            return;
        }
        s.workspaces.len() - 1
    };
    activate_workspace_shortcut(state, last_idx);
}

// ---------------------------------------------------------------------------
// Sidebar row
// ---------------------------------------------------------------------------

fn build_sidebar_row(
    name: &str,
    description: Option<&str>,
    folder_path: Option<&str>,
    sidebar: &app_config::SidebarConfig,
) -> (
    gtk::ListBoxRow,
    gtk::Label,
    gtk::Button,
    gtk::Label,
    gtk::Label,
    gtk::Label,
    gtk::Label,
) {
    let notify_dot = gtk::Label::builder().label("\u{25CF}").build();
    notify_dot.add_css_class("limux-notify-dot-hidden");

    let name_label = gtk::Label::builder()
        .label(name)
        .xalign(0.0)
        .hexpand(true)
        .ellipsize(gtk::pango::EllipsizeMode::End)
        .build();
    name_label.add_css_class("limux-ws-name");
    apply_sidebar_name_label_policy(&name_label, sidebar.wrap_workspace_titles);

    let favorite_button = gtk::Button::with_label("\u{2606}");
    favorite_button.add_css_class("flat");
    favorite_button.add_css_class("limux-ws-star-btn");
    favorite_button.set_focus_on_click(false);
    favorite_button.set_valign(gtk::Align::Center);
    favorite_button.set_halign(gtk::Align::End);
    favorite_button.set_tooltip_text(Some("Favorite workspace"));

    let top_row = gtk::Box::new(gtk::Orientation::Horizontal, 0);
    top_row.append(&notify_dot);
    top_row.append(&name_label);
    top_row.append(&favorite_button);

    let path_label = gtk::Label::builder()
        .xalign(0.0)
        .ellipsize(gtk::pango::EllipsizeMode::End)
        .margin_start(8)
        .build();
    path_label.add_css_class("limux-ws-path");

    let description_label = gtk::Label::builder()
        .xalign(0.0)
        .ellipsize(gtk::pango::EllipsizeMode::End)
        .margin_start(8)
        .build();
    description_label.add_css_class("limux-ws-path");
    apply_sidebar_detail_labels(
        &description_label,
        &path_label,
        description,
        folder_path,
        sidebar,
    );

    let notify_label = gtk::Label::builder()
        .xalign(0.0)
        .ellipsize(gtk::pango::EllipsizeMode::End)
        .visible(false)
        .margin_start(8)
        .build();
    notify_label.add_css_class("limux-notify-msg");

    let vbox = gtk::Box::builder()
        .orientation(gtk::Orientation::Vertical)
        .spacing(2)
        .build();
    vbox.add_css_class("limux-sidebar-row-box");
    vbox.append(&top_row);
    vbox.append(&description_label);
    vbox.append(&path_label);
    vbox.append(&notify_label);

    let row = gtk::ListBoxRow::new();
    row.set_child(Some(&vbox));

    (
        row,
        name_label,
        favorite_button,
        notify_dot,
        notify_label,
        path_label,
        description_label,
    )
}

// purpose: Apply CMUX title wrapping policy to a workspace sidebar row label.
// inputs: GTK name label and the configured wrapWorkspaceTitles value.
// returns/effects: Mutates label wrapping and ellipsizing to match the setting.
fn apply_sidebar_name_label_policy(name_label: &gtk::Label, wrap_workspace_titles: bool) {
    name_label.set_wrap(wrap_workspace_titles);
    name_label.set_ellipsize(if wrap_workspace_titles {
        gtk::pango::EllipsizeMode::None
    } else {
        gtk::pango::EllipsizeMode::End
    });
}

// purpose: Apply CMUX sidebar detail text and visibility to labels.
// inputs: Description/path labels, optional workspace detail values, and sidebar settings.
// returns/effects: Mutates labels to show or hide configured detail rows.
fn apply_sidebar_detail_labels(
    description_label: &gtk::Label,
    path_label: &gtk::Label,
    description: Option<&str>,
    folder_path: Option<&str>,
    sidebar: &app_config::SidebarConfig,
) {
    let show_details = !sidebar.hide_all_details;
    if let Some(description) = description.filter(|value| !value.is_empty()) {
        description_label.set_label(description);
        description_label.set_visible(show_details && sidebar.show_workspace_description);
    } else {
        description_label.set_visible(false);
    }
    if let Some(path) = folder_path.filter(|_| show_details) {
        path_label.set_label(&abbreviate_path(path));
        path_label.set_tooltip_text(Some(path));
        path_label.set_visible(sidebar.show_branch_directory);
    } else {
        path_label.set_visible(false);
    }
}

// purpose: Apply CMUX sidebar detail visibility to an existing workspace row.
// inputs: Workspace row model and sidebar settings.
// returns/effects: Updates title wrapping, description, path, and notification detail rows.
fn sync_workspace_sidebar_detail_row(
    workspace: &mut Workspace,
    sidebar: &app_config::SidebarConfig,
) {
    apply_sidebar_name_label_policy(&workspace.name_label, sidebar.wrap_workspace_titles);
    apply_sidebar_detail_labels(
        &workspace.description_label,
        &workspace.path_label,
        workspace.description.as_deref(),
        workspace.folder_path.as_deref(),
        sidebar,
    );
    let show_notification =
        workspace.unread && should_show_sidebar_notification_message(true, sidebar);
    workspace.notify_label.set_visible(show_notification);
}

// purpose: Resync all open workspace sidebar rows after config reload.
// inputs: App state and freshly loaded sidebar config.
// returns/effects: Mutates existing GTK labels without rebuilding workspaces.
fn sync_sidebar_detail_rows(state: &State, sidebar: &app_config::SidebarConfig) {
    let mut app_state = state.borrow_mut();
    for workspace in &mut app_state.workspaces {
        sync_workspace_sidebar_detail_row(workspace, sidebar);
    }
}

/// Abbreviate a path by replacing the home directory with ~.
fn abbreviate_path(path: &str) -> String {
    if let Some(home) = dirs::home_dir() {
        let home_str = home.to_string_lossy();
        if path.starts_with(home_str.as_ref()) {
            return format!("~{}", &path[home_str.len()..]);
        }
    }
    path.to_string()
}

// ---------------------------------------------------------------------------
// Workspace management
// ---------------------------------------------------------------------------

fn favorites_prefix_len(flags: &[bool]) -> usize {
    flags.iter().take_while(|is_favorite| **is_favorite).count()
}

#[cfg(test)]
fn workspace_drop_layout_path(layout: &LayoutNodeState) -> Vec<bool> {
    match layout {
        LayoutNodeState::Pane(_) => Vec::new(),
        LayoutNodeState::Split(split) => {
            let mut path = vec![true];
            path.extend(workspace_drop_layout_path(&split.start));
            path
        }
    }
}

fn tab_drag_workspace_seed(
    source: WorkspaceSeedSource,
    title: &str,
    tab_cwd: Option<String>,
) -> TabDragWorkspaceSeed {
    let name = {
        let trimmed = title.trim();
        if trimmed.is_empty() {
            "Workspace".to_string()
        } else {
            trimmed.to_string()
        }
    };
    let cwd = tab_cwd
        .clone()
        .or_else(|| source.workspace_folder_path.clone())
        .or(source.workspace_cwd.clone());
    let folder_path = tab_cwd
        .filter(|cwd| !cwd.trim().is_empty())
        .or(source.workspace_folder_path)
        .filter(|path| !path.trim().is_empty());

    TabDragWorkspaceSeed {
        name,
        cwd,
        folder_path,
    }
}

fn next_active_workspace_index(
    remaining_workspace_ids: &[&str],
    preferred_active_workspace_id: Option<&str>,
    removed_idx: usize,
) -> usize {
    if remaining_workspace_ids.is_empty() {
        return 0;
    }
    if let Some(preferred_id) = preferred_active_workspace_id {
        if let Some(idx) = remaining_workspace_ids
            .iter()
            .position(|workspace_id| *workspace_id == preferred_id)
        {
            return idx;
        }
    }
    removed_idx.min(remaining_workspace_ids.len() - 1)
}

fn show_workspace_context_menu(state: &State, workspace_id: &str, row: &gtk::ListBoxRow) {
    let menu_box = gtk::Box::new(gtk::Orientation::Vertical, 2);
    menu_box.set_margin_top(4);
    menu_box.set_margin_bottom(4);
    menu_box.set_margin_start(4);
    menu_box.set_margin_end(4);

    let rename_btn = gtk::Button::with_label("Rename");
    rename_btn.add_css_class("flat");
    let delete_btn = gtk::Button::with_label("Delete");
    delete_btn.add_css_class("flat");
    delete_btn.add_css_class("destructive-action");

    menu_box.append(&rename_btn);
    menu_box.append(&delete_btn);

    let popover = gtk::Popover::new();
    popover.set_child(Some(&menu_box));
    popover.set_parent(row);
    popover.set_position(gtk::PositionType::Right);

    {
        let state = state.clone();
        let ws_id = workspace_id.to_string();
        let pop = popover.clone();
        rename_btn.connect_clicked(move |_| {
            pop.popdown();
            begin_workspace_inline_rename(&state, &ws_id);
        });
    }
    {
        let state = state.clone();
        let ws_id = workspace_id.to_string();
        let pop = popover.clone();
        delete_btn.connect_clicked(move |_| {
            pop.popdown();
            close_workspace_by_id(&state, &ws_id);
            request_session_save(&state);
        });
    }
    {
        popover.connect_closed(move |p| {
            p.unparent();
        });
    }

    popover.popup();
}

fn clamp_workspace_insert_index_for_pinning(
    favorite_flags_after_removal: &[bool],
    moving_is_favorite: bool,
    proposed_index: usize,
) -> usize {
    let favorites_top = favorites_prefix_len(favorite_flags_after_removal);
    if moving_is_favorite {
        proposed_index.min(favorites_top)
    } else {
        proposed_index.max(favorites_top)
    }
}

// purpose: Resolve CMUX app.newWorkspacePlacement into a workspace insertion index.
// inputs: Favorite flags including the source row, selected/reference index, source index, and placement.
// returns/effects: Returns an insertion index before source removal without mutating state.
fn workspace_insert_index_for_placement(
    favorite_flags: &[bool],
    selected_index: Option<usize>,
    source_index: usize,
    placement: app_config::WorkspaceGroupNewPlacement,
) -> usize {
    let total = favorite_flags.len();
    let pinned_count = favorites_prefix_len(favorite_flags);
    match placement {
        app_config::WorkspaceGroupNewPlacement::Top => pinned_count,
        app_config::WorkspaceGroupNewPlacement::End => total,
        app_config::WorkspaceGroupNewPlacement::AfterCurrent => selected_index
            .filter(|index| *index < total)
            .map(|index| {
                if favorite_flags[index] {
                    pinned_count
                } else {
                    index.saturating_add(1)
                }
            })
            .unwrap_or(source_index),
    }
}

fn sync_sidebar_row_order(state: &mut AppState) {
    while let Some(child) = state.sidebar_list.first_child() {
        state.sidebar_list.remove(&child);
    }
    for (index, workspace) in state.workspaces.iter().enumerate() {
        if workspace_hidden_by_collapsed_group_id(
            &workspace.id,
            workspace.group_id.as_deref(),
            index == state.active_idx,
            &state.workspace_groups,
        ) {
            continue;
        }
        state.sidebar_list.append(&workspace.sidebar_row);
    }
}

/// purpose: Decide whether a workspace row is hidden by collapsed group state.
/// inputs: Workspace id, optional group id, active selection flag, and known groups.
/// returns/effects: Returns true for non-active non-anchor members of collapsed groups.
fn workspace_hidden_by_collapsed_group_id(
    workspace_id: &str,
    group_id: Option<&str>,
    active: bool,
    groups: &[WorkspaceGroupState],
) -> bool {
    if active {
        return false;
    }
    let Some(group_id) = group_id else {
        return false;
    };
    groups
        .iter()
        .find(|group| group.id == group_id)
        .is_some_and(|group| {
            group.is_collapsed && group.anchor_workspace_id.as_deref() != Some(workspace_id)
        })
}

fn set_workspace_favorite_visual(workspace: &Workspace) {
    let symbol = if workspace.favorite {
        "\u{2605}"
    } else {
        "\u{2606}"
    };
    workspace.favorite_button.set_label(symbol);
    if workspace.favorite {
        workspace
            .favorite_button
            .add_css_class("limux-ws-star-btn-active");
    } else {
        workspace
            .favorite_button
            .remove_css_class("limux-ws-star-btn-active");
    }
}

/// Find an active rename Entry in the sidebar (if any).
fn find_active_rename_entry(sidebar_list: &gtk::ListBox) -> Option<gtk::Entry> {
    fn find_entry(widget: &gtk::Widget) -> Option<gtk::Entry> {
        if let Some(entry) = widget.downcast_ref::<gtk::Entry>() {
            return Some(entry.clone());
        }
        let mut child = widget.first_child();
        while let Some(c) = child {
            if let Some(entry) = find_entry(&c) {
                return Some(entry);
            }
            child = c.next_sibling();
        }
        None
    }
    let mut row = sidebar_list.first_child();
    while let Some(r) = row {
        if let Some(entry) = find_entry(&r) {
            return Some(entry);
        }
        row = r.next_sibling();
    }
    None
}

/// Find any active rename Entry in the sidebar and trigger its activate signal to commit.
fn commit_any_active_rename(sidebar_list: &gtk::ListBox) {
    let mut row = sidebar_list.first_child();
    while let Some(r) = row {
        // Walk into the row's children to find a gtk::Entry
        fn find_entry(widget: &gtk::Widget) -> Option<gtk::Entry> {
            if let Some(entry) = widget.downcast_ref::<gtk::Entry>() {
                return Some(entry.clone());
            }
            let mut child = widget.first_child();
            while let Some(c) = child {
                if let Some(entry) = find_entry(&c) {
                    return Some(entry);
                }
                child = c.next_sibling();
            }
            None
        }
        if let Some(entry) = find_entry(&r) {
            entry.emit_activate();
            return;
        }
        row = r.next_sibling();
    }
}

fn begin_workspace_inline_rename(state: &State, workspace_id: &str) {
    let (label, current_name) = {
        let s = state.borrow();
        let Some(workspace) = s
            .workspaces
            .iter()
            .find(|workspace| workspace.id == workspace_id)
        else {
            return;
        };
        (workspace.name_label.clone(), workspace.name.clone())
    };

    let Some(parent) = label.parent().and_then(|p| p.downcast::<gtk::Box>().ok()) else {
        return;
    };

    // Avoid stacking multiple rename entries if the user right-clicks repeatedly.
    let mut child = parent.first_child();
    while let Some(widget) = child {
        if widget.is::<gtk::Entry>() {
            return;
        }
        child = widget.next_sibling();
    }

    let entry = gtk::Entry::builder()
        .text(&current_name)
        .hexpand(true)
        .build();
    for css_class in WORKSPACE_RENAME_ENTRY_CSS_CLASSES {
        entry.add_css_class(css_class);
    }

    label.set_visible(false);
    parent.insert_child_after(&entry, Some(&label));
    entry.grab_focus();
    entry.select_region(0, -1);

    let commit_guard = Rc::new(std::cell::Cell::new(false));
    let state_for_commit = state.clone();
    let workspace_id = workspace_id.to_string();
    let label_for_commit = label.clone();
    let parent_for_commit = parent.clone();
    let commit = {
        let commit_guard = commit_guard.clone();
        move |entry: &gtk::Entry| {
            if commit_guard.get() {
                return;
            }
            commit_guard.set(true);

            let next_name = entry.text().trim().to_string();
            if !next_name.is_empty() {
                label_for_commit.set_label(&next_name);
                let snapshot = {
                    let mut s = state_for_commit.borrow_mut();
                    if let Some(workspace) = s
                        .workspaces
                        .iter_mut()
                        .find(|workspace| workspace.id == workspace_id)
                    {
                        workspace.name = next_name;
                    }
                    let index = s
                        .workspaces
                        .iter()
                        .position(|workspace| workspace.id == workspace_id);
                    index.and_then(|index| workspace_event_snapshot(&s, index))
                };
                if let Some(snapshot) = snapshot {
                    publish_workspace_lifecycle_event(
                        "workspace.renamed",
                        &snapshot,
                        None,
                        serde_json::json!({ "origin": "ui" }),
                    );
                }
                request_session_save(&state_for_commit);
            }

            label_for_commit.set_visible(true);
            parent_for_commit.remove(entry);
        }
    };

    {
        let commit = commit.clone();
        entry.connect_activate(move |entry| {
            commit(entry);
        });
    }
    {
        let commit = commit.clone();
        let focus = gtk::EventControllerFocus::new();
        focus.connect_leave(move |controller| {
            if let Some(widget) = controller.widget() {
                if let Some(entry) = widget.downcast_ref::<gtk::Entry>() {
                    commit(entry);
                }
            }
        });
        entry.add_controller(focus);
    }
}

fn reorder_workspace_by_id(
    state: &State,
    source_id: &str,
    target_id: &str,
    drop_below: bool,
) -> bool {
    let (sidebar_list, row_to_select, ordered_ids, pinned_ids, selected_id, selected_index) = {
        let mut s = state.borrow_mut();
        let Some(source_idx) = s
            .workspaces
            .iter()
            .position(|workspace| workspace.id == source_id)
        else {
            return false;
        };
        let Some(target_idx) = s
            .workspaces
            .iter()
            .position(|workspace| workspace.id == target_id)
        else {
            return false;
        };
        if source_idx == target_idx {
            return false;
        }

        let active_workspace_id = s.active_workspace().map(|workspace| workspace.id.clone());
        let moving_workspace = s.workspaces.remove(source_idx);
        let Some(target_idx_after_removal) = s
            .workspaces
            .iter()
            .position(|workspace| workspace.id == target_id)
        else {
            s.workspaces.insert(source_idx, moving_workspace);
            return false;
        };

        // Insert after the target when dropping on the bottom half
        let raw_insert_idx = if drop_below {
            target_idx_after_removal + 1
        } else {
            target_idx_after_removal
        };

        let favorite_flags: Vec<bool> = s
            .workspaces
            .iter()
            .map(|workspace| workspace.favorite)
            .collect();
        let insert_idx = clamp_workspace_insert_index_for_pinning(
            &favorite_flags,
            moving_workspace.favorite,
            raw_insert_idx,
        );
        s.workspaces.insert(insert_idx, moving_workspace);

        if let Some(active_workspace_id) = active_workspace_id {
            if let Some(new_active_idx) = s
                .workspaces
                .iter()
                .position(|workspace| workspace.id == active_workspace_id)
            {
                s.active_idx = new_active_idx;
            }
        }

        sync_sidebar_row_order(&mut s);
        let row_to_select = s
            .workspaces
            .get(s.active_idx)
            .map(|workspace| workspace.sidebar_row.clone());
        let ordered_ids = s
            .workspaces
            .iter()
            .map(|workspace| workspace.id.clone())
            .collect::<Vec<_>>();
        let pinned_ids = s
            .workspaces
            .iter()
            .filter(|workspace| workspace.favorite)
            .map(|workspace| workspace.id.clone())
            .collect::<Vec<_>>();
        let selected_id = s
            .workspaces
            .get(s.active_idx)
            .map(|workspace| workspace.id.clone());
        (
            s.sidebar_list.clone(),
            row_to_select,
            ordered_ids,
            pinned_ids,
            selected_id,
            s.active_idx,
        )
    };

    if let Some(row) = row_to_select {
        sidebar_list.select_row(Some(&row));
    }
    publish_workspace_reordered_event(
        ordered_ids,
        vec![source_id.to_string()],
        pinned_ids,
        selected_id,
        selected_index,
    );
    request_session_save(state);

    true
}

fn toggle_workspace_favorite(state: &State, workspace_id: &str) {
    let (sidebar_list, row_to_select, ordered_ids, pinned_ids, selected_id, selected_index) = {
        let mut s = state.borrow_mut();
        let Some(idx) = s
            .workspaces
            .iter()
            .position(|workspace| workspace.id == workspace_id)
        else {
            return;
        };

        let active_workspace_id = s.active_workspace().map(|workspace| workspace.id.clone());
        s.workspaces[idx].favorite = !s.workspaces[idx].favorite;
        set_workspace_favorite_visual(&s.workspaces[idx]);

        let workspace = s.workspaces.remove(idx);
        let favorite_flags: Vec<bool> = s
            .workspaces
            .iter()
            .map(|candidate| candidate.favorite)
            .collect();
        let insert_idx = favorites_prefix_len(&favorite_flags);
        s.workspaces.insert(insert_idx, workspace);

        if let Some(active_workspace_id) = active_workspace_id {
            if let Some(new_active_idx) = s
                .workspaces
                .iter()
                .position(|workspace| workspace.id == active_workspace_id)
            {
                s.active_idx = new_active_idx;
            }
        }

        sync_sidebar_row_order(&mut s);
        let row_to_select = s
            .workspaces
            .get(s.active_idx)
            .map(|workspace| workspace.sidebar_row.clone());
        let ordered_ids = s
            .workspaces
            .iter()
            .map(|workspace| workspace.id.clone())
            .collect::<Vec<_>>();
        let pinned_ids = s
            .workspaces
            .iter()
            .filter(|workspace| workspace.favorite)
            .map(|workspace| workspace.id.clone())
            .collect::<Vec<_>>();
        let selected_id = s
            .workspaces
            .get(s.active_idx)
            .map(|workspace| workspace.id.clone());
        (
            s.sidebar_list.clone(),
            row_to_select,
            ordered_ids,
            pinned_ids,
            selected_id,
            s.active_idx,
        )
    };

    if let Some(row) = row_to_select {
        sidebar_list.select_row(Some(&row));
    }
    publish_workspace_reordered_event(
        ordered_ids,
        vec![workspace_id.to_string()],
        pinned_ids,
        selected_id,
        selected_index,
    );
    request_session_save(state);
}

fn handle_tab_drop_to_workspace(state: &State, target_workspace_id: &str, payload: &str) -> bool {
    let Some((pane_id, tab_id)) = payload.split_once(':') else {
        return false;
    };
    let Ok(source_pane_id) = pane_id.parse::<u32>() else {
        return false;
    };
    let Some(source_pane) = pane::find_pane_widget_by_id(source_pane_id) else {
        return false;
    };

    let target_pane = {
        let app_state = state.borrow();
        let Some(workspace) = app_state
            .workspaces
            .iter()
            .find(|workspace| workspace.id == target_workspace_id)
        else {
            return false;
        };
        find_leaf_pane(&workspace.root, gtk::Orientation::Horizontal, true)
    };

    pane::move_tab_to_pane(&source_pane, tab_id, &target_pane)
}

fn create_workspace_for_tab(state: &State, payload: &str) -> bool {
    create_workspace_for_tab_payload(state, payload, None, true).is_ok()
}

// purpose: Move a tab into a newly-created workspace and return its control payload.
// inputs: Shared app state, a `<pane_id>:<tab_id>` tab payload, optional title, and focus policy.
// returns/effects: Creates a workspace, moves the tab there, optionally selects it, and persists state.
fn create_workspace_for_tab_payload(
    state: &State,
    payload: &str,
    title_override: Option<&str>,
    focus: bool,
) -> Result<serde_json::Value, BridgeError> {
    let Some((pane_id, tab_id)) = payload.split_once(':') else {
        return Err(BridgeError::invalid_params("invalid tab payload"));
    };
    let Ok(source_pane_id) = pane_id.parse::<u32>() else {
        return Err(BridgeError::invalid_params("invalid pane id"));
    };
    let Some(source_pane) = pane::find_pane_widget_by_id(source_pane_id) else {
        return Err(BridgeError::not_found("pane not found"));
    };

    let Some(title) = pane::tab_title(&source_pane, tab_id) else {
        return Err(BridgeError::not_found("surface not found"));
    };
    let tab_cwd = pane::tab_working_directory(&source_pane, tab_id);
    let seed = {
        let app_state = state.borrow();
        let source = app_state
            .workspace_for_widget(&source_pane)
            .map(|workspace| WorkspaceSeedSource {
                workspace_cwd: workspace.cwd.borrow().clone(),
                workspace_folder_path: workspace.folder_path.clone(),
            })
            .unwrap_or(WorkspaceSeedSource {
                workspace_cwd: None,
                workspace_folder_path: None,
            });
        tab_drag_workspace_seed(source, title_override.unwrap_or(&title), tab_cwd)
    };
    let previous_active_workspace_id = {
        let app_state = state.borrow();
        app_state
            .active_workspace()
            .map(|workspace| workspace.id.clone())
    };

    let shortcuts = {
        let app_state = state.borrow();
        app_state.shortcuts.clone()
    };
    let new_workspace_id = uuid::Uuid::new_v4().to_string();
    let stack_name = format!("ws-{new_workspace_id}");
    let pane = create_pane_for_workspace(
        state,
        &shortcuts,
        &new_workspace_id,
        seed.cwd.as_deref(),
        None,
        true,
    );
    let split_container = SplitTreeContainer::new(state, pane.clone().upcast());
    let root = split_container.widget().clone();

    let sidebar_config = {
        let app_state = state.borrow();
        let sidebar = app_state.config.borrow().sidebar.clone();
        sidebar
    };
    let (row, name_label, favorite_button, notify_dot, notify_label, path_label, description_label) =
        build_sidebar_row(
            &seed.name,
            None,
            seed.folder_path.as_deref(),
            &sidebar_config,
        );
    let row_clone = row.clone();
    {
        let mut app_state = state.borrow_mut();
        app_state.stack.add_named(&root, Some(&stack_name));
        app_state.sidebar_list.append(&row);
        install_workspace_row_interactions(state, &new_workspace_id, &row, &favorite_button);

        app_state.workspaces.push(Workspace {
            id: new_workspace_id.clone(),
            name: seed.name.clone(),
            description: None,
            root: root.clone().upcast(),
            split_container,
            sidebar_row: row,
            name_label,
            favorite_button,
            notify_dot,
            notify_label,
            description_label,
            unread: false,
            favorite: false,
            last_pane_id: None,
            group_id: None,
            environment: BTreeMap::new(),
            cwd: Rc::new(RefCell::new(seed.cwd.clone())),
            folder_path: seed.folder_path.clone(),
            path_label,
            sidebar_status: BTreeMap::new(),
            sidebar_progress: None,
            sidebar_log: Vec::new(),
        });
        if focus {
            app_state.active_idx = app_state.workspaces.len() - 1;
            sync_right_sidebar_panel(&mut app_state);
            app_state.stack.set_visible_child_name(&stack_name);
        }
    }

    if focus {
        let sidebar_list = state.borrow().sidebar_list.clone();
        sidebar_list.select_row(Some(&row_clone));
    }

    if pane::move_tab_to_pane(&source_pane, tab_id, &pane.clone().upcast()) {
        let result = {
            let app_state = state.borrow();
            let index = app_state
                .workspaces
                .iter()
                .position(|workspace| workspace.id == new_workspace_id)
                .ok_or_else(|| BridgeError::not_found("workspace not found"))?;
            let workspace = &app_state.workspaces[index];
            let surface = pane::active_surface_summary(&pane.clone().upcast())
                .ok_or_else(|| BridgeError::not_found("surface not found"))?;
            let mut payload = pane_create_response_payload(&workspace.id, &workspace.name, surface);
            if let Some(workspace_payload) = workspace_payload(&app_state, index) {
                payload["workspace"] = workspace_payload["workspace"].clone();
            }
            payload
        };
        request_session_save(state);
        return Ok(result);
    }
    close_workspace_by_id_internal(
        state,
        &new_workspace_id,
        false,
        previous_active_workspace_id.as_deref(),
    );
    Err(BridgeError::not_found("surface not found"))
}

fn install_workspace_row_interactions(
    state: &State,
    workspace_id: &str,
    row: &gtk::ListBoxRow,
    favorite_button: &gtk::Button,
) {
    let right_click = gtk::GestureClick::new();
    right_click.set_button(3);
    {
        let state = state.clone();
        let workspace_id = workspace_id.to_string();
        let r = row.clone();
        right_click.connect_pressed(move |_, _, _, _| {
            show_workspace_context_menu(&state, &workspace_id, &r);
        });
    }
    row.add_controller(right_click);

    let drag_source = gtk::DragSource::new();
    drag_source.set_actions(gtk::gdk::DragAction::MOVE);
    {
        let workspace_id = workspace_id.to_string();
        drag_source.connect_prepare(move |_, _, _| {
            let payload = glib::Value::from(&workspace_id);
            Some(gtk::gdk::ContentProvider::for_value(&payload))
        });
    }
    {
        let state = state.clone();
        let row = row.clone();
        let workspace_id = workspace_id.to_string();
        drag_source.connect_drag_begin(move |source, _| {
            let mut s = state.borrow_mut();
            s.workspace_dragging = Some(workspace_id.clone());
            s.new_ws_btn.set_label("\u{1F5D1}\u{FE0E}");
            s.new_ws_btn.add_css_class("limux-sidebar-btn-trash");
            drop(s);
            pane::set_workspace_dragging_all(true);
            let icon = gtk::WidgetPaintable::new(Some(&row));
            source.set_icon(Some(&icon), 0, 0);
        });
    }
    {
        let state = state.clone();
        drag_source.connect_drag_end(move |_, _, _| {
            let mut s = state.borrow_mut();
            s.workspace_dragging = None;
            s.new_ws_btn.set_label("New Workspace");
            s.new_ws_btn.remove_css_class("limux-sidebar-btn-trash");
            s.new_ws_btn
                .remove_css_class("limux-sidebar-btn-trash-hover");
            pane::set_workspace_dragging_all(false);
        });
    }
    row.add_controller(drag_source);

    let drop_target = gtk::DropTarget::new(glib::Type::STRING, gtk::gdk::DragAction::MOVE);
    drop_target.set_preload(true);
    let hover_timer: Rc<RefCell<Option<glib::SourceId>>> = Rc::new(RefCell::new(None));
    let drop_handled = Rc::new(Cell::new(false));
    {
        let r = row.clone();
        let state = state.clone();
        let hover_timer = hover_timer.clone();
        let target_workspace_id = workspace_id.to_string();
        let drop_handled = drop_handled.clone();
        drop_target.connect_motion(move |_, _x, y| {
            drop_handled.set(false);
            let h = r.height() as f64;
            r.remove_css_class("limux-drop-above");
            r.remove_css_class("limux-drop-below");
            r.remove_css_class("limux-tab-drop-target");

            let dragged_workspace = state.borrow().workspace_dragging.clone();
            match dragged_workspace {
                Some(ref dragged_workspace_id) if dragged_workspace_id != &target_workspace_id => {
                    if y < h / 2.0 {
                        r.add_css_class("limux-drop-above");
                    } else {
                        r.add_css_class("limux-drop-below");
                    }
                }
                None => {
                    r.add_css_class("limux-tab-drop-target");
                }
                _ => {}
            }

            if hover_timer.borrow().is_none() {
                let state = state.clone();
                let target_workspace_id = target_workspace_id.clone();
                let hover_timer = hover_timer.clone();
                let drop_handled = drop_handled.clone();
                let timer_for_callback = hover_timer.clone();
                let source = glib::timeout_add_local_once(
                    std::time::Duration::from_millis(500),
                    move || {
                        *timer_for_callback.borrow_mut() = None;
                        if drop_handled.get() {
                            return;
                        }
                        let (target_idx, sidebar_row, sidebar_list) = {
                            let app_state = state.borrow();
                            let idx = app_state
                                .workspaces
                                .iter()
                                .position(|workspace| workspace.id == target_workspace_id);
                            let sidebar_row = idx.and_then(|idx| {
                                app_state
                                    .workspaces
                                    .get(idx)
                                    .map(|workspace| workspace.sidebar_row.clone())
                            });
                            (idx, sidebar_row, app_state.sidebar_list.clone())
                        };
                        if let Some(target_idx) = target_idx {
                            switch_workspace(&state, target_idx);
                        }
                        if let Some(sidebar_row) = sidebar_row {
                            sidebar_list.select_row(Some(&sidebar_row));
                        }
                    },
                );
                *hover_timer.borrow_mut() = Some(source);
            }
            gtk::gdk::DragAction::MOVE
        });
    }
    {
        let r = row.clone();
        let hover_timer = hover_timer.clone();
        drop_target.connect_leave(move |_| {
            r.remove_css_class("limux-drop-above");
            r.remove_css_class("limux-drop-below");
            r.remove_css_class("limux-tab-drop-target");
            if let Some(source) = hover_timer.borrow_mut().take() {
                source.remove();
            }
        });
    }
    {
        let state = state.clone();
        let target_workspace_id = workspace_id.to_string();
        let r = row.clone();
        let hover_timer = hover_timer.clone();
        let drop_handled = drop_handled.clone();
        drop_target.connect_drop(move |_dt, value, _, y| {
            drop_handled.set(true);
            r.remove_css_class("limux-drop-above");
            r.remove_css_class("limux-drop-below");
            r.remove_css_class("limux-tab-drop-target");
            if let Some(source) = hover_timer.borrow_mut().take() {
                source.remove();
            }
            if let Ok(payload) = value.get::<String>() {
                if payload.contains(':') {
                    return handle_tab_drop_to_workspace(&state, &target_workspace_id, &payload);
                }
                let drop_below = y >= r.height() as f64 / 2.0;
                if payload != target_workspace_id {
                    return reorder_workspace_by_id(
                        &state,
                        &payload,
                        &target_workspace_id,
                        drop_below,
                    );
                }
            }
            false
        });
    }
    row.add_controller(drop_target);

    {
        let state = state.clone();
        let workspace_id = workspace_id.to_string();
        favorite_button.connect_clicked(move |_| {
            toggle_workspace_favorite(&state, &workspace_id);
        });
    }
}

fn add_workspace(state: &State, _working_directory: Option<&str>) {
    show_workspace_path_dialog(state);
}

fn active_window(state: &State) -> Option<gtk::Window> {
    let s = state.borrow();
    s.stack
        .root()
        .and_then(|root| root.downcast::<gtk::Window>().ok())
}

fn show_workspace_path_dialog(state: &State) {
    let dialog = gtk::Window::builder()
        .title("Open Folder as Workspace")
        .modal(true)
        .default_width(520)
        .build();
    if let Some(window) = active_window(state) {
        dialog.set_transient_for(Some(&window));
    }

    let default_folder = dirs::home_dir()
        .or_else(|| std::env::current_dir().ok())
        .unwrap_or_else(|| PathBuf::from("/"));
    let entry = gtk::Entry::builder()
        .text(default_folder.to_string_lossy())
        .hexpand(true)
        .activates_default(true)
        .build();
    let browse_button = gtk::Button::with_label("Browse...");
    let error_label = gtk::Label::builder()
        .halign(gtk::Align::Start)
        .visible(false)
        .wrap(true)
        .build();
    error_label.add_css_class("error");

    let content = gtk::Box::builder()
        .orientation(gtk::Orientation::Vertical)
        .spacing(12)
        .margin_top(12)
        .margin_bottom(12)
        .margin_start(12)
        .margin_end(12)
        .build();

    let path_row = gtk::Box::builder()
        .orientation(gtk::Orientation::Horizontal)
        .spacing(8)
        .build();
    path_row.append(&entry);
    path_row.append(&browse_button);
    content.append(&path_row);
    content.append(&error_label);

    let buttons = gtk::Box::builder()
        .orientation(gtk::Orientation::Horizontal)
        .halign(gtk::Align::End)
        .spacing(8)
        .build();
    let cancel_button = gtk::Button::with_label("Cancel");
    let open_button = gtk::Button::with_label("Open");
    open_button.add_css_class("suggested-action");
    buttons.append(&cancel_button);
    buttons.append(&open_button);
    content.append(&buttons);
    dialog.set_child(Some(&content));

    entry.grab_focus();
    entry.select_region(0, -1);
    let state_for_open = state.clone();
    let entry_for_open = entry.clone();
    let error_label_for_open = error_label.clone();
    let dialog_for_open = dialog.clone();
    open_button.connect_clicked(move |_| {
        match validate_workspace_folder_input(entry_for_open.text().as_str()) {
            Ok(selection) => {
                create_workspace_with_folder(
                    &state_for_open,
                    &selection.name,
                    selection.path_text.as_str(),
                );
                dialog_for_open.close();
            }
            Err(message) => {
                error_label_for_open.set_label(&message);
                error_label_for_open.set_visible(true);
                entry_for_open.grab_focus();
            }
        }
    });

    let open_button_for_entry = open_button.clone();
    entry.connect_activate(move |_| {
        open_button_for_entry.emit_clicked();
    });

    let entry_for_browse = entry.clone();
    let error_label_for_browse = error_label.clone();
    let browse_button_for_browse = browse_button.clone();
    let transient_for_browse = active_window(state);
    browse_button.connect_clicked(move |_| {
        error_label_for_browse.set_visible(false);
        browse_button_for_browse.set_sensitive(false);

        let picker = gtk::FileDialog::builder()
            .title("Choose Workspace Folder")
            .accept_label("Choose")
            .modal(true)
            .build();

        if let Ok(selection) = validate_workspace_folder_input(entry_for_browse.text().as_str()) {
            picker.set_initial_folder(Some(&gio::File::for_path(selection.path_text)));
        }

        let entry_for_result = entry_for_browse.clone();
        let error_label_for_result = error_label_for_browse.clone();
        let browse_button_for_result = browse_button_for_browse.clone();
        picker.select_folder(
            transient_for_browse.as_ref(),
            None::<&gio::Cancellable>,
            move |result| {
                browse_button_for_result.set_sensitive(true);
                match result {
                    Ok(file) => {
                        if let Some(path) = file.path() {
                            entry_for_result.set_text(&path.to_string_lossy());
                            entry_for_result.grab_focus();
                            entry_for_result.set_position(-1);
                        }
                    }
                    Err(err) if is_workspace_picker_cancel(&err) => {}
                    Err(err) => {
                        error_label_for_result.set_label(&format!("Folder picker failed: {err}"));
                        error_label_for_result.set_visible(true);
                    }
                }
            },
        );
    });

    let dialog_for_cancel = dialog.clone();
    cancel_button.connect_clicked(move |_| {
        dialog_for_cancel.close();
    });

    dialog.present();
}

fn is_workspace_picker_cancel(err: &glib::Error) -> bool {
    matches!(
        err.kind::<gtk::DialogError>(),
        Some(gtk::DialogError::Cancelled | gtk::DialogError::Dismissed)
    )
}

#[derive(Debug)]
struct WorkspaceFolderSelection {
    name: String,
    path_text: String,
}

fn validate_workspace_folder_input(input: &str) -> Result<WorkspaceFolderSelection, String> {
    let home_dir = dirs::home_dir();
    let current_dir = std::env::current_dir().ok();
    validate_workspace_folder_input_with_dirs(input, home_dir.as_deref(), current_dir.as_deref())
}

fn validate_workspace_folder_input_with_dirs(
    input: &str,
    home_dir: Option<&Path>,
    current_dir: Option<&Path>,
) -> Result<WorkspaceFolderSelection, String> {
    let path = workspace_folder_path_from_input(input, home_dir, current_dir)?;
    let metadata =
        std::fs::metadata(&path).map_err(|err| format!("Cannot open {}: {err}", path.display()))?;
    if !metadata.is_dir() {
        return Err(format!("{} is not a folder", path.display()));
    }

    let path_text = path.to_string_lossy().to_string();
    let name = path
        .file_name()
        .map(|segment| segment.to_string_lossy().to_string())
        .filter(|name| !name.is_empty())
        .unwrap_or_else(|| path_text.clone());
    Ok(WorkspaceFolderSelection { name, path_text })
}

fn workspace_folder_path_from_input(
    input: &str,
    home_dir: Option<&Path>,
    current_dir: Option<&Path>,
) -> Result<PathBuf, String> {
    let trimmed = input.trim();
    if trimmed.is_empty() {
        return Err("Enter a folder path".to_string());
    }

    let expanded = if trimmed == "~" {
        home_dir
            .ok_or_else(|| "Home directory is unavailable".to_string())?
            .to_path_buf()
    } else if let Some(rest) = trimmed.strip_prefix("~/") {
        home_dir
            .ok_or_else(|| "Home directory is unavailable".to_string())?
            .join(rest)
    } else {
        PathBuf::from(trimmed)
    };

    if expanded.is_absolute() {
        Ok(expanded)
    } else if let Some(current_dir) = current_dir {
        Ok(current_dir.join(expanded))
    } else {
        Err("Current directory is unavailable".to_string())
    }
}

// purpose: Resolve the effective cwd for a new CMUX-compatible workspace.
// inputs: App config, explicit cwd, and active workspace cwd/folder snapshot.
// returns/effects: Returns explicit cwd first, inherited cwd when enabled, or None.
fn resolve_workspace_creation_directory(
    config: &app_config::AppConfig,
    requested_cwd: Option<&str>,
    active_workspace_directory: Option<&str>,
) -> Option<String> {
    requested_cwd.map(ToOwned::to_owned).or_else(|| {
        config
            .app
            .workspace_inherit_working_directory
            .then(|| active_workspace_directory.map(ToOwned::to_owned))
            .flatten()
    })
}

// purpose: Snapshot the active workspace directory used for CMUX cwd inheritance.
// inputs: Current app state.
// returns/effects: Prefers folder_path over mutable terminal-reported cwd.
fn active_workspace_directory(state: &AppState) -> Option<String> {
    state
        .active_workspace()
        .and_then(|workspace| {
            workspace
                .folder_path
                .clone()
                .or_else(|| workspace.cwd.borrow().clone())
        })
        .filter(|directory| !directory.trim().is_empty())
}

// purpose: Resolve workspace-create cwd from current live state and explicit params.
// inputs: Shared host state and optional explicit cwd.
// returns/effects: Borrows state briefly and returns owned cwd or None.
fn workspace_creation_directory_from_state(
    state: &State,
    requested_cwd: Option<&str>,
) -> Option<String> {
    let s = state.borrow();
    let config = s.config.borrow();
    let active_directory = active_workspace_directory(&s);
    resolve_workspace_creation_directory(&config, requested_cwd, active_directory.as_deref())
}

// purpose: Derive a default workspace title from an optional cwd.
// inputs: Effective workspace directory.
// returns/effects: Uses the final path segment or "workspace" when cwd is unset.
fn workspace_title_from_directory(directory: Option<&str>) -> String {
    directory
        .and_then(|directory| {
            std::path::Path::new(directory)
                .file_name()
                .map(|name| name.to_string_lossy().to_string())
                .filter(|name| !name.is_empty())
        })
        .unwrap_or_else(|| "workspace".to_string())
}

// purpose: Resolve the CMUX Cmd-N target for folder-created workspaces.
// inputs: Current host state and active workspace/group selection.
// returns/effects: Returns group placement metadata or None for ungrouped active workspaces.
fn active_workspace_group_folder_target(state: &AppState) -> Option<WorkspaceGroupFolderTarget> {
    let active_workspace = state.active_workspace()?;
    let group_id = active_workspace.group_id.as_deref()?;
    let group = state
        .workspace_groups
        .iter()
        .find(|group| group.id == group_id)?;
    let anchor_cwd = group.anchor_workspace_id.as_deref().and_then(|anchor_id| {
        state
            .workspaces
            .iter()
            .find(|workspace| workspace.id == anchor_id)
            .and_then(|workspace| workspace.cwd.borrow().clone())
    });
    let placement = state
        .config
        .borrow()
        .workspace_groups
        .new_workspace_placement_for_cwd(anchor_cwd.as_deref());
    Some(WorkspaceGroupFolderTarget {
        group_id: group_id.to_string(),
        reference_workspace_id: active_workspace.id.clone(),
        placement,
    })
}

// purpose: Snapshot CMUX placement context before a folder workspace is created.
// inputs: Current host state and active workspace selection.
// returns/effects: Returns group or app placement metadata without mutating state.
fn active_workspace_folder_target(state: &AppState) -> WorkspaceFolderTarget {
    WorkspaceFolderTarget {
        group: active_workspace_group_folder_target(state),
        reference_workspace_id: state
            .active_workspace()
            .map(|workspace| workspace.id.clone()),
        placement: state.config.borrow().new_workspace_placement,
    }
}

// purpose: Reorder a newly-created non-group workspace according to CMUX app placement.
// inputs: Created workspace id, placement, and optional pre-creation reference workspace id.
// returns/effects: Mutates workspace order/sidebar order or returns a placement error.
fn place_created_workspace(
    state: &State,
    workspace_id: &str,
    placement: app_config::WorkspaceGroupNewPlacement,
    reference_workspace_id: Option<&str>,
) -> Result<(), BridgeError> {
    let mut s = state.borrow_mut();
    let Some(source_index) = workspace_index_for_raw_id(&s, workspace_id) else {
        return Err(BridgeError::not_found("workspace not found"));
    };
    let active_workspace_id = s.active_workspace().map(|workspace| workspace.id.clone());
    let favorite_flags = s
        .workspaces
        .iter()
        .map(|workspace| workspace.favorite)
        .collect::<Vec<_>>();
    let reference_index =
        reference_workspace_id.and_then(|raw| workspace_index_for_raw_id(&s, raw));
    let target_index = workspace_insert_index_for_placement(
        &favorite_flags,
        reference_index,
        source_index,
        placement,
    );
    if source_index != target_index {
        let workspace = s.workspaces.remove(source_index);
        let adjusted = if target_index > source_index {
            target_index - 1
        } else {
            target_index
        };
        s.workspaces.insert(adjusted, workspace);
        if let Some(active_workspace_id) = active_workspace_id {
            if let Some(active_index) = s
                .workspaces
                .iter()
                .position(|workspace| workspace.id == active_workspace_id)
            {
                s.active_idx = active_index;
            }
        }
    }
    sync_sidebar_row_order(&mut s);
    Ok(())
}

fn create_workspace_with_folder(state: &State, name: &str, folder_path: &str) {
    let target = {
        let s = state.borrow();
        active_workspace_folder_target(&s)
    };
    let workspace = WorkspaceState {
        id: None,
        name: name.to_string(),
        description: None,
        favorite: false,
        cwd: Some(folder_path.to_string()),
        folder_path: Some(folder_path.to_string()),
        group_id: target.group.as_ref().map(|group| group.group_id.clone()),
        environment: BTreeMap::new(),
        layout: LayoutNodeState::Pane(PaneState::fallback(Some(folder_path))),
    };
    add_workspace_from_state(state, &workspace);
    let created_workspace_id = {
        let s = state.borrow();
        s.workspaces
            .last()
            .map(|workspace| workspace.id.clone())
            .expect("folder workspace creation must append a workspace")
    };
    if let Some(group) = target.group {
        place_created_workspace_in_group(
            state,
            &created_workspace_id,
            &group.group_id,
            Some(group.placement.as_str()),
            Some(&group.reference_workspace_id),
        )
        .expect("active workspace group target must remain valid after folder workspace creation");
    } else {
        place_created_workspace(
            state,
            &created_workspace_id,
            target.placement,
            target.reference_workspace_id.as_deref(),
        )
        .expect("created folder workspace must be orderable");
    }
    request_session_save(state);
}

// purpose: Resolve a workspace id/ref against current host state.
// inputs: Live app state plus raw UUID or CMUX/Limux workspace ref.
// returns/effects: Returns the workspace vector index without mutating state.
fn workspace_index_for_raw_id(state: &AppState, raw: &str) -> Option<usize> {
    let id = normalize_workspace_handle(raw);
    state
        .workspaces
        .iter()
        .position(|workspace| workspace.id == id)
}

// purpose: Choose the next visible CMUX group name.
// inputs: Current group list.
// returns/effects: Returns an unused "Group N" label without mutating state.
fn next_workspace_group_name(groups: &[WorkspaceGroupState]) -> String {
    let mut index = groups.len() + 1;
    loop {
        let name = format!("Group {index}");
        if !groups.iter().any(|group| group.name == name) {
            return name;
        }
        index += 1;
    }
}

// purpose: Add a new anchor workspace for a workspace group.
// inputs: Live state, group id/name, optional cwd, and activation preference.
// returns/effects: Mutates GTK state by appending and optionally activating a workspace.
fn add_group_anchor_workspace(
    state: &State,
    group_id: &str,
    name: &str,
    cwd: Option<&str>,
    activate: bool,
) -> String {
    let workspace_id = uuid::Uuid::new_v4().to_string();
    let workspace = WorkspaceState {
        id: Some(workspace_id.clone()),
        name: name.to_string(),
        description: None,
        favorite: false,
        cwd: cwd.map(ToOwned::to_owned),
        folder_path: cwd.map(ToOwned::to_owned),
        group_id: Some(group_id.to_string()),
        environment: BTreeMap::new(),
        layout: LayoutNodeState::Pane(PaneState::fallback(cwd)),
    };
    add_workspace_from_state_internal(state, &workspace, activate);
    workspace_id
}

// purpose: Apply one CMUX workspace-group operation to live host state.
// inputs: Group action parsed by the control bridge.
// returns/effects: Mutates group/workspace state, may create/close workspaces, and queues save.
fn apply_workspace_group_action(
    state: &State,
    action: WorkspaceGroupAction,
) -> Result<serde_json::Value, BridgeError> {
    match action {
        WorkspaceGroupAction::Create {
            name,
            cwd,
            from_workspace_ids,
        } => create_workspace_group(state, name, cwd, from_workspace_ids),
        WorkspaceGroupAction::Ungroup { group_id } => ungroup_workspace_group(state, &group_id),
        WorkspaceGroupAction::Delete { group_id } => delete_workspace_group(state, &group_id),
        WorkspaceGroupAction::Rename { group_id, name } => {
            update_workspace_group(state, &group_id, |group| group.name = name)
        }
        WorkspaceGroupAction::Collapse { group_id } => {
            update_workspace_group(state, &group_id, |group| group.is_collapsed = true)
        }
        WorkspaceGroupAction::Expand { group_id } => {
            update_workspace_group(state, &group_id, |group| group.is_collapsed = false)
        }
        WorkspaceGroupAction::Pin { group_id } => {
            update_workspace_group(state, &group_id, |group| group.is_pinned = true)
        }
        WorkspaceGroupAction::Unpin { group_id } => {
            update_workspace_group(state, &group_id, |group| group.is_pinned = false)
        }
        WorkspaceGroupAction::Add {
            group_id,
            workspace_id,
        } => add_workspace_to_group(state, &group_id, &workspace_id),
        WorkspaceGroupAction::Remove { workspace_id } => {
            remove_workspace_from_group(state, &workspace_id)
        }
        WorkspaceGroupAction::SetAnchor {
            group_id,
            workspace_id,
        } => set_workspace_group_anchor(state, &group_id, &workspace_id),
        WorkspaceGroupAction::NewWorkspace {
            group_id,
            placement,
        } => new_workspace_in_group(state, &group_id, placement.as_deref()),
        WorkspaceGroupAction::SetColor { group_id, color } => {
            update_workspace_group(state, &group_id, |group| group.custom_color = color)
        }
        WorkspaceGroupAction::SetIcon { group_id, symbol } => {
            update_workspace_group(state, &group_id, |group| group.icon_symbol = symbol)
        }
        WorkspaceGroupAction::Move { group_id, index } => {
            move_workspace_group(state, &group_id, index)
        }
        WorkspaceGroupAction::Focus { group_id } => focus_workspace_group(state, &group_id),
    }
}

// purpose: Create a group with a fresh anchor and optional existing members.
// inputs: Requested name, cwd, and workspace ids to include.
// returns/effects: Mutates workspace/group state and queues session persistence.
fn create_workspace_group(
    state: &State,
    name: Option<String>,
    cwd: Option<String>,
    from_workspace_ids: Vec<String>,
) -> Result<serde_json::Value, BridgeError> {
    let group_id = uuid::Uuid::new_v4().to_string();
    let group_name = {
        let s = state.borrow();
        name.filter(|value| !value.trim().is_empty())
            .unwrap_or_else(|| next_workspace_group_name(&s.workspace_groups))
    };
    let anchor_workspace_id =
        add_group_anchor_workspace(state, &group_id, &group_name, cwd.as_deref(), true);

    let group = WorkspaceGroupState {
        id: group_id.clone(),
        name: group_name,
        is_collapsed: false,
        is_pinned: false,
        anchor_workspace_id: Some(anchor_workspace_id.clone()),
        custom_color: None,
        icon_symbol: None,
    };

    {
        let mut s = state.borrow_mut();
        for raw in from_workspace_ids {
            let Some(index) = workspace_index_for_raw_id(&s, &raw) else {
                return Err(BridgeError::not_found("workspace not found"));
            };
            s.workspaces[index].group_id = Some(group_id.clone());
        }
        s.workspace_groups.push(group.clone());
        sync_sidebar_row_order(&mut s);
    }
    request_session_save(state);

    let mut payload = workspace_group_payload(&group);
    payload["anchor_workspace_id"] = serde_json::Value::String(anchor_workspace_id.clone());
    payload["anchor_workspace_ref"] =
        serde_json::Value::String(workspace_ref(&anchor_workspace_id));
    Ok(payload)
}

// purpose: Remove a group wrapper while keeping all member workspaces.
// inputs: Group id/ref.
// returns/effects: Clears member group ids, removes the group, and queues persistence.
fn ungroup_workspace_group(
    state: &State,
    group_id: &str,
) -> Result<serde_json::Value, BridgeError> {
    let group = {
        let mut s = state.borrow_mut();
        let Some(index) = workspace_group_index(&s, group_id) else {
            return Err(BridgeError::not_found("workspace group not found"));
        };
        let group = s.workspace_groups.remove(index);
        for workspace in &mut s.workspaces {
            if workspace.group_id.as_deref() == Some(group.id.as_str()) {
                workspace.group_id = None;
            }
        }
        sync_sidebar_row_order(&mut s);
        group
    };
    request_session_save(state);
    Ok(serde_json::json!({ "ok": true, "group": workspace_group_row(&group) }))
}

// purpose: Delete a group and close every workspace that belongs to it.
// inputs: Group id/ref.
// returns/effects: Removes grouped workspaces from GTK state and queues persistence.
fn delete_workspace_group(state: &State, group_id: &str) -> Result<serde_json::Value, BridgeError> {
    let (group, workspace_ids) = {
        let s = state.borrow();
        let Some(index) = workspace_group_index(&s, group_id) else {
            return Err(BridgeError::not_found("workspace group not found"));
        };
        let group = s.workspace_groups[index].clone();
        let workspace_ids = s
            .workspaces
            .iter()
            .filter(|workspace| workspace.group_id.as_deref() == Some(group.id.as_str()))
            .map(|workspace| workspace.id.clone())
            .collect::<Vec<_>>();
        (group, workspace_ids)
    };
    for workspace_id in &workspace_ids {
        close_workspace_by_id(state, workspace_id);
    }
    {
        let mut s = state.borrow_mut();
        s.workspace_groups
            .retain(|candidate| candidate.id != group.id);
        sync_sidebar_row_order(&mut s);
    }
    request_session_save(state);
    Ok(serde_json::json!({
        "ok": true,
        "deleted_workspace_count": workspace_ids.len(),
        "group": workspace_group_row(&group),
    }))
}

// purpose: Mutate one group row and return the updated payload.
// inputs: Group id/ref plus a narrow mutation callback.
// returns/effects: Updates group metadata and queues persistence.
fn update_workspace_group(
    state: &State,
    group_id: &str,
    mutate: impl FnOnce(&mut WorkspaceGroupState),
) -> Result<serde_json::Value, BridgeError> {
    let group = {
        let mut s = state.borrow_mut();
        let Some(index) = workspace_group_index(&s, group_id) else {
            return Err(BridgeError::not_found("workspace group not found"));
        };
        mutate(&mut s.workspace_groups[index]);
        let group = s.workspace_groups[index].clone();
        sync_sidebar_row_order(&mut s);
        group
    };
    request_session_save(state);
    Ok(workspace_group_payload(&group))
}

// purpose: Attach an existing workspace to a group.
// inputs: Group id/ref and workspace id/ref.
// returns/effects: Updates membership and queues persistence.
fn add_workspace_to_group(
    state: &State,
    group_id: &str,
    workspace_id: &str,
) -> Result<serde_json::Value, BridgeError> {
    let (group, workspace) = {
        let mut s = state.borrow_mut();
        let Some(group_index) = workspace_group_index(&s, group_id) else {
            return Err(BridgeError::not_found("workspace group not found"));
        };
        let Some(workspace_index) = workspace_index_for_raw_id(&s, workspace_id) else {
            return Err(BridgeError::not_found("workspace not found"));
        };
        let group_id = s.workspace_groups[group_index].id.clone();
        s.workspaces[workspace_index].group_id = Some(group_id);
        sync_sidebar_row_order(&mut s);
        (
            s.workspace_groups[group_index].clone(),
            workspace_row(
                workspace_index,
                s.active_idx,
                &s.workspaces[workspace_index],
            ),
        )
    };
    request_session_save(state);
    Ok(serde_json::json!({ "group": workspace_group_row(&group), "workspace": workspace }))
}

// purpose: Validate CMUX workspace.create group placement before creating a workspace.
// inputs: Live state, optional group id/ref, placement, and explicit reference workspace.
// returns/effects: Returns an error for impossible group placement without mutating state.
fn validate_workspace_create_group_request(
    state: &State,
    group_id: Option<&str>,
    group_placement: Option<&str>,
    reference_workspace_id: Option<&str>,
) -> Result<(), BridgeError> {
    let s = state.borrow();
    if group_id.is_none() && (group_placement.is_some() || reference_workspace_id.is_some()) {
        return Err(BridgeError::invalid_params(
            "workspace.create group placement requires group_id",
        ));
    }
    if let Some(group_id) = group_id {
        if workspace_group_index(&s, group_id).is_none() {
            return Err(BridgeError::not_found("workspace group not found"));
        }
    }
    if let Some(reference_workspace_id) = reference_workspace_id {
        if workspace_index_for_raw_id(&s, reference_workspace_id).is_none() {
            return Err(BridgeError::not_found(
                "group reference workspace not found",
            ));
        }
    }
    Ok(())
}

// purpose: Attach a newly-created workspace to a CMUX group and place it in row order.
// inputs: Created workspace id plus requested group placement metadata.
// returns/effects: Mutates workspace membership/order or returns a placement error.
fn place_created_workspace_in_group(
    state: &State,
    workspace_id: &str,
    group_id: &str,
    placement: Option<&str>,
    reference_workspace_id: Option<&str>,
) -> Result<(), BridgeError> {
    let mut s = state.borrow_mut();
    let Some(group_index) = workspace_group_index(&s, group_id) else {
        return Err(BridgeError::not_found("workspace group not found"));
    };
    let group_id = s.workspace_groups[group_index].id.clone();
    let Some(source_index) = workspace_index_for_raw_id(&s, workspace_id) else {
        return Err(BridgeError::not_found("workspace not found"));
    };
    let active_workspace_id = s.active_workspace().map(|workspace| workspace.id.clone());
    s.workspaces[source_index].group_id = Some(group_id.clone());
    let target_index = workspace_create_group_insert_index(
        &s,
        &group_id,
        source_index,
        placement.unwrap_or("top"),
        reference_workspace_id,
    );
    let moved = source_index != target_index;
    if moved {
        let workspace = s.workspaces.remove(source_index);
        let adjusted = if target_index > source_index {
            target_index - 1
        } else {
            target_index
        };
        s.workspaces.insert(adjusted, workspace);
        if let Some(active_workspace_id) = active_workspace_id {
            if let Some(active_index) = s
                .workspaces
                .iter()
                .position(|workspace| workspace.id == active_workspace_id)
            {
                s.active_idx = active_index;
            }
        }
    }
    sync_sidebar_row_order(&mut s);
    Ok(())
}

// purpose: Resolve CMUX group placement into a workspace-vector insertion index.
// inputs: Host state, group id, source index, placement, and optional reference id/ref.
// returns/effects: Returns an insertion index without mutating state.
fn workspace_create_group_insert_index(
    state: &AppState,
    group_id: &str,
    source_index: usize,
    placement: &str,
    reference_workspace_id: Option<&str>,
) -> usize {
    let group_ids = state
        .workspaces
        .iter()
        .map(|workspace| workspace.group_id.as_deref())
        .collect::<Vec<_>>();
    let reference_index =
        reference_workspace_id.and_then(|raw| workspace_index_for_raw_id(state, raw));
    workspace_group_insert_index(
        &group_ids,
        state.active_idx,
        reference_index,
        group_id,
        source_index,
        placement,
    )
}

// purpose: Resolve CMUX group placement against ordered workspace group ids.
// inputs: Workspace group ids, active/reference indexes, group id, source index, and placement.
// returns/effects: Returns an insertion index without mutating state.
fn workspace_group_insert_index(
    group_ids: &[Option<&str>],
    active_index: usize,
    reference_index: Option<usize>,
    group_id: &str,
    source_index: usize,
    placement: &str,
) -> usize {
    match placement {
        "end" => group_ids
            .iter()
            .rposition(|candidate| *candidate == Some(group_id))
            .map(|index| index + 1)
            .unwrap_or(source_index),
        "afterCurrent" => reference_index
            .or(Some(active_index))
            .map(|index| index + 1)
            .unwrap_or(source_index),
        _ => group_ids
            .iter()
            .position(|candidate| *candidate == Some(group_id))
            .unwrap_or(source_index),
    }
}

// purpose: Detach an existing workspace from whichever group contains it.
// inputs: Workspace id/ref.
// returns/effects: Clears membership and queues persistence.
fn remove_workspace_from_group(
    state: &State,
    workspace_id: &str,
) -> Result<serde_json::Value, BridgeError> {
    let workspace = {
        let mut s = state.borrow_mut();
        let Some(workspace_index) = workspace_index_for_raw_id(&s, workspace_id) else {
            return Err(BridgeError::not_found("workspace not found"));
        };
        s.workspaces[workspace_index].group_id = None;
        sync_sidebar_row_order(&mut s);
        workspace_row(
            workspace_index,
            s.active_idx,
            &s.workspaces[workspace_index],
        )
    };
    request_session_save(state);
    Ok(serde_json::json!({ "workspace": workspace }))
}

// purpose: Update the anchor workspace for an existing group.
// inputs: Group id/ref and workspace id/ref.
// returns/effects: Ensures the workspace belongs to the group and queues persistence.
fn set_workspace_group_anchor(
    state: &State,
    group_id: &str,
    workspace_id: &str,
) -> Result<serde_json::Value, BridgeError> {
    let (group, workspace) = {
        let mut s = state.borrow_mut();
        let Some(group_index) = workspace_group_index(&s, group_id) else {
            return Err(BridgeError::not_found("workspace group not found"));
        };
        let Some(workspace_index) = workspace_index_for_raw_id(&s, workspace_id) else {
            return Err(BridgeError::not_found("workspace not found"));
        };
        let group_id = s.workspace_groups[group_index].id.clone();
        let workspace_id = s.workspaces[workspace_index].id.clone();
        s.workspaces[workspace_index].group_id = Some(group_id);
        s.workspace_groups[group_index].anchor_workspace_id = Some(workspace_id);
        sync_sidebar_row_order(&mut s);
        (
            s.workspace_groups[group_index].clone(),
            workspace_row(
                workspace_index,
                s.active_idx,
                &s.workspaces[workspace_index],
            ),
        )
    };
    request_session_save(state);
    Ok(serde_json::json!({ "group": workspace_group_row(&group), "workspace": workspace }))
}

// purpose: Create a workspace inside an existing group using CMUX placement.
// inputs: Group id/ref and optional top/end/afterCurrent placement.
// returns/effects: Creates, activates, reorders, and persists the grouped workspace.
fn new_workspace_in_group(
    state: &State,
    group_id: &str,
    placement: Option<&str>,
) -> Result<serde_json::Value, BridgeError> {
    let (group_id, name, cwd, anchor_workspace_id, configured_placement) = {
        let s = state.borrow();
        let Some(group_index) = workspace_group_index(&s, group_id) else {
            return Err(BridgeError::not_found("workspace group not found"));
        };
        let group = &s.workspace_groups[group_index];
        let cwd = group.anchor_workspace_id.as_deref().and_then(|anchor_id| {
            s.workspaces
                .iter()
                .find(|workspace| workspace.id == anchor_id)
                .and_then(|workspace| workspace.cwd.borrow().clone())
        });
        let configured_placement = s
            .config
            .borrow()
            .workspace_groups
            .new_workspace_placement_for_cwd(cwd.as_deref());
        (
            group.id.clone(),
            group.name.clone(),
            cwd,
            group.anchor_workspace_id.clone(),
            configured_placement,
        )
    };
    let workspace_id = add_group_anchor_workspace(state, &group_id, &name, cwd.as_deref(), true);
    let placement = placement.unwrap_or_else(|| configured_placement.as_str());
    let reference = (placement == "afterCurrent")
        .then_some(anchor_workspace_id.as_deref())
        .flatten();
    place_created_workspace_in_group(state, &workspace_id, &group_id, Some(placement), reference)?;
    let payload = {
        let s = state.borrow();
        let Some(index) = workspace_index_for_raw_id(&s, &workspace_id) else {
            return Err(BridgeError::internal(
                "workspace.group.new_workspace did not create workspace",
            ));
        };
        workspace_payload(&s, index)
    };
    request_session_save(state);
    payload.ok_or_else(|| BridgeError::internal("workspace payload missing after create"))
}

// purpose: Move a group metadata row to a requested order index.
// inputs: Group id/ref and target index.
// returns/effects: Reorders group metadata and queues persistence.
fn move_workspace_group(
    state: &State,
    group_id: &str,
    index: usize,
) -> Result<serde_json::Value, BridgeError> {
    let group = {
        let mut s = state.borrow_mut();
        let Some(current_index) = workspace_group_index(&s, group_id) else {
            return Err(BridgeError::not_found("workspace group not found"));
        };
        let group = s.workspace_groups.remove(current_index);
        let target_index = index.min(s.workspace_groups.len());
        s.workspace_groups.insert(target_index, group.clone());
        group
    };
    request_session_save(state);
    Ok(workspace_group_payload(&group))
}

// purpose: Focus a group's anchor workspace.
// inputs: Group id/ref.
// returns/effects: Switches active workspace when the anchor exists.
fn focus_workspace_group(state: &State, group_id: &str) -> Result<serde_json::Value, BridgeError> {
    let anchor_id = {
        let s = state.borrow();
        let Some(group_index) = workspace_group_index(&s, group_id) else {
            return Err(BridgeError::not_found("workspace group not found"));
        };
        s.workspace_groups[group_index].anchor_workspace_id.clone()
    };
    let Some(anchor_id) = anchor_id else {
        return Err(BridgeError::not_found("workspace group anchor not found"));
    };
    let index = {
        let s = state.borrow();
        workspace_index_for_raw_id(&s, &anchor_id)
    };
    let Some(index) = index else {
        return Err(BridgeError::not_found("workspace group anchor not found"));
    };
    switch_workspace(state, index);
    let s = state.borrow();
    workspace_payload(&s, index).ok_or_else(|| BridgeError::not_found("workspace not found"))
}

fn terminal_pane_state(
    tab_count: usize,
    working_directory: Option<&str>,
    pane_index: usize,
) -> PaneState {
    let tabs = (0..tab_count)
        .map(|tab_index| {
            TabState::terminal(
                format!("terminal-{pane_index}-{tab_index}"),
                working_directory,
            )
        })
        .collect::<Vec<_>>();
    let active_tab_id = tabs.first().map(|tab| tab.id.clone());
    PaneState {
        pane_id: None,
        active_tab_id,
        tabs,
    }
}

fn split_layout_from_panes(mut panes: Vec<PaneState>, depth: usize) -> LayoutNodeState {
    if panes.len() == 1 {
        return LayoutNodeState::Pane(panes.remove(0));
    }

    let right = panes.split_off(panes.len() / 2);
    let orientation = if depth.is_multiple_of(2) {
        SplitOrientation::Horizontal
    } else {
        SplitOrientation::Vertical
    };
    LayoutNodeState::Split(SplitState {
        orientation,
        ratio: layout_state::DEFAULT_SPLIT_RATIO,
        start: Box::new(split_layout_from_panes(panes, depth + 1)),
        end: Box::new(split_layout_from_panes(right, depth + 1)),
    })
}

fn mixed_workspace_layout(
    panes_per_workspace: usize,
    terminals_per_workspace: usize,
    working_directory: Option<&str>,
) -> LayoutNodeState {
    let extra_tabs = terminals_per_workspace - panes_per_workspace;
    let panes = (0..panes_per_workspace)
        .map(|pane_index| {
            let tab_count = if pane_index == 0 { 1 + extra_tabs } else { 1 };
            terminal_pane_state(tab_count, working_directory, pane_index)
        })
        .collect::<Vec<_>>();
    split_layout_from_panes(panes, 0)
}

// purpose: Build a CMUX-shaped result payload for one live tab action.
// inputs: Workspace id, normalized action key, pane action summary, and optional unread state.
// returns/effects: Returns refs and action-specific metadata without mutating host state.
fn tab_action_payload(
    workspace_id: &str,
    action: &str,
    summary: &pane::TabActionSummary,
    unread: Option<bool>,
) -> serde_json::Value {
    let closed_refs = summary
        .closed
        .iter()
        .map(|surface| json!(format!("surface:{}", surface.surface_id)))
        .collect::<Vec<_>>();
    let mut payload = json!({
        "ok": true,
        "action": action,
        "workspace_id": workspace_id,
        "workspace_ref": format!("workspace:{workspace_id}"),
        "surface_id": summary.surface.surface_id,
        "surface_ref": format!("surface:{}", summary.surface.surface_id),
        "tab_ref": format!("tab:{}", summary.surface.surface_id),
        "pane_id": summary.surface.pane_id,
        "pane_ref": format!("pane:{}", summary.surface.pane_id),
        "title": summary.surface.title,
        "pinned": summary.pinned,
        "closed": summary.closed.len(),
        "closed_surface_refs": closed_refs,
        "skipped_pinned": summary.skipped_pinned,
        "reloaded": summary.reloaded,
    });
    if let Some(created) = &summary.created {
        payload["created_surface_id"] = json!(created.surface_id);
        payload["created_surface_ref"] = json!(format!("surface:{}", created.surface_id));
        payload["created_tab_ref"] = json!(format!("tab:{}", created.surface_id));
    }
    if let Some(value) = unread {
        payload["unread"] = json!(value);
    }
    payload
}

// purpose: Apply CMUX mark-read/mark-unread tab actions to Limux workspace unread UI.
// inputs: Live app state, workspace id, action key, and resolved surface summary.
// returns/effects: Mutates workspace unread styling for read/unread actions.
fn apply_tab_read_state_action(
    state: &State,
    workspace_id: &str,
    summary: &pane::TabActionSummary,
    action: &str,
) -> Option<bool> {
    match action {
        "mark_unread" | "mark_as_unread" => {
            mark_workspace_unread_for_tab(state, workspace_id, &summary.surface);
            Some(true)
        }
        "mark_read" => {
            let mut app_state = state.borrow_mut();
            if let Some(workspace) = app_state
                .workspaces
                .iter_mut()
                .find(|workspace| workspace.id == workspace_id)
            {
                clear_workspace_unread_visual(workspace);
            }
            Some(false)
        }
        _ => None,
    }
}

// purpose: Mark the owning workspace unread for a CMUX tab action.
// inputs: Live app state, workspace id, and surface metadata for notification targeting.
// returns/effects: Sets Limux workspace unread styling without emitting desktop notifications.
fn mark_workspace_unread_for_tab(
    state: &State,
    workspace_id: &str,
    surface: &pane::SurfaceSummary,
) {
    let mut app_state = state.borrow_mut();
    let Some(workspace) = app_state
        .workspaces
        .iter_mut()
        .find(|workspace| workspace.id == workspace_id)
    else {
        return;
    };
    workspace.unread = true;
    workspace
        .notify_dot
        .remove_css_class("limux-notify-dot-hidden");
    workspace.notify_dot.add_css_class("limux-notify-dot");
    workspace
        .notify_label
        .set_label(&format!("{} needs attention", surface.title));
    workspace.notify_label.remove_css_class("limux-notify-msg");
    workspace
        .notify_label
        .add_css_class("limux-notify-msg-unread");
    workspace.notify_label.set_visible(true);
    if let Some(row_box) = workspace.sidebar_row.child() {
        row_box.add_css_class("limux-sidebar-row-unread");
    }
}

fn dispatch_control_command(command: ControlCommand) {
    CONTROL_STATE.with(|slot| {
        let state = slot.borrow().clone();
        if let Some(state) = state {
            handle_control_command(&state, command);
        } else {
            command.respond(Err(crate::control_bridge::BridgeError::internal(
                "control bridge not initialized",
            )));
        }
    });
}

fn handle_control_command(state: &State, command: ControlCommand) {
    match command {
        ControlCommand::Identify { caller, reply } => {
            let result = {
                let focused = focused_surface_payload(state).unwrap_or(serde_json::Value::Null);
                serde_json::json!({
                    "name": "limux-control",
                    "protocol": "v1+v2",
                    "version": env!("CARGO_PKG_VERSION"),
                    "focused": focused,
                    "caller": caller.unwrap_or_else(|| focused.clone()),
                })
            };
            let _ = reply.send(Ok(result));
        }
        ControlCommand::CurrentWorkspace { reply } => {
            let result = {
                let app_state = state.borrow();
                workspace_payload(&app_state, app_state.active_idx)
            };
            let _ = reply.send(result.ok_or_else(|| {
                crate::control_bridge::BridgeError::not_found("no active workspace")
            }));
        }
        ControlCommand::Memory {
            top_group_limit,
            reply,
        } => {
            let result = crate::memory_diagnostics::memory_diagnostic_payload(top_group_limit)
                .map_err(BridgeError::internal);
            let _ = reply.send(result);
        }
        ControlCommand::SystemTop {
            top_group_limit,
            sample_ms,
            workspace_target,
            window_id,
            include_all,
            reply,
        } => {
            let result = system_top_payload(
                state,
                SystemTopPayloadRequest {
                    top_group_limit,
                    sample_ms,
                    workspace_target: workspace_target.as_ref(),
                    window_id: window_id.as_deref(),
                    include_all,
                },
            );
            let _ = reply.send(result);
        }
        ControlCommand::SystemTree {
            workspace_target,
            window_id,
            include_all,
            reply,
        } => {
            let result = system_tree_payload(
                state,
                workspace_target.as_ref(),
                window_id.as_deref(),
                include_all,
            );
            let _ = reply.send(result);
        }
        ControlCommand::ReloadConfig { reply } => {
            let result = reload_config_for_control(state);
            let _ = reply.send(result);
        }
        ControlCommand::OpenSettings {
            target,
            activate,
            reply,
        } => {
            let result = open_settings_for_control(state, target, activate);
            let _ = reply.send(result);
        }
        ControlCommand::ListWorkspaces { reply } => {
            let workspaces = {
                let app_state = state.borrow();
                app_state
                    .workspaces
                    .iter()
                    .enumerate()
                    .map(|(index, workspace)| workspace_row(index, app_state.active_idx, workspace))
                    .collect::<Vec<_>>()
            };
            let _ = reply.send(Ok(serde_json::json!({ "workspaces": workspaces })));
        }
        ControlCommand::ListWindows { reply } => {
            let result = {
                let app_state = state.borrow();
                window_list_payload(&app_state)
            };
            let _ = reply.send(Ok(result));
        }
        ControlCommand::WorkspaceEnv {
            target,
            mask,
            reply,
        } => {
            let result = {
                let app_state = state.borrow();
                workspace_env_payload(&app_state, &target, mask)
            };
            let _ = reply.send(result);
        }
        ControlCommand::ListWorkspaceGroups { reply } => {
            let groups = {
                let app_state = state.borrow();
                app_state
                    .workspace_groups
                    .iter()
                    .map(workspace_group_row)
                    .collect::<Vec<_>>()
            };
            let _ = reply.send(Ok(serde_json::json!({ "groups": groups })));
        }
        ControlCommand::WorkspaceGroupAction { action, reply } => {
            let result = apply_workspace_group_action(state, action);
            let _ = reply.send(result);
        }
        ControlCommand::ListPanes { target, reply } => {
            let resolved = {
                let app_state = state.borrow();
                workspace_index_for_target(&app_state, &target)
            };

            let Some(index) = resolved else {
                let _ = reply.send(Err(crate::control_bridge::BridgeError::not_found(
                    "workspace not found",
                )));
                return;
            };

            let result = {
                let app_state = state.borrow();
                pane_list_payload(state, &app_state.workspaces[index])
            };
            let _ = reply.send(Ok(result));
        }
        ControlCommand::ListPaneSurfaces {
            target,
            pane_id,
            reply,
        } => {
            let resolved = {
                let app_state = state.borrow();
                workspace_index_for_target(&app_state, &target)
            };

            let Some(index) = resolved else {
                let _ = reply.send(Err(crate::control_bridge::BridgeError::not_found(
                    "workspace not found",
                )));
                return;
            };

            let pane_filter = pane_id
                .as_deref()
                .and_then(parse_pane_handle)
                .or_else(|| pane_id.as_deref().and_then(|raw| raw.parse::<u32>().ok()));
            if pane_id.is_some() && pane_filter.is_none() {
                let _ = reply.send(Err(crate::control_bridge::BridgeError::invalid_params(
                    "pane.surfaces requires a valid pane_id",
                )));
                return;
            }

            let result = {
                let app_state = state.borrow();
                surface_list_payload(state, &app_state.workspaces[index], pane_filter)
            };

            if pane_id.is_some()
                && result["surfaces"]
                    .as_array()
                    .is_some_and(|surfaces| surfaces.is_empty())
            {
                let _ = reply.send(Err(crate::control_bridge::BridgeError::not_found(
                    "pane not found",
                )));
                return;
            }

            let _ = reply.send(Ok(result));
        }
        ControlCommand::FocusPane {
            target,
            pane_id,
            reply,
        } => {
            let resolved = {
                let app_state = state.borrow();
                workspace_index_for_target(&app_state, &target)
            };

            let Some(index) = resolved else {
                let _ = reply.send(Err(crate::control_bridge::BridgeError::not_found(
                    "workspace not found",
                )));
                return;
            };

            let pane_id = parse_pane_handle(&pane_id).or_else(|| pane_id.parse::<u32>().ok());
            let Some(pane_id) = pane_id else {
                let _ = reply.send(Err(crate::control_bridge::BridgeError::invalid_params(
                    "pane.focus requires a valid pane_id",
                )));
                return;
            };

            let _ = reply.send(focus_pane_for_control(state, index, pane_id));
        }
        ControlCommand::LastPane { target, reply } => {
            let resolved = {
                let app_state = state.borrow();
                workspace_index_for_target(&app_state, &target)
            };

            let Some(index) = resolved else {
                let _ = reply.send(Err(crate::control_bridge::BridgeError::not_found(
                    "workspace not found",
                )));
                return;
            };

            let last_pane_id = {
                let app_state = state.borrow();
                app_state.workspaces[index].last_pane_id
            };
            let Some(last_pane_id) = last_pane_id else {
                let _ = reply.send(Err(crate::control_bridge::BridgeError::not_found(
                    "last pane not found",
                )));
                return;
            };

            let _ = reply.send(focus_pane_for_control(state, index, last_pane_id));
        }
        ControlCommand::ResizePane {
            target,
            pane_id,
            direction,
            amount,
            reply,
        } => {
            let resolved = {
                let app_state = state.borrow();
                workspace_index_for_target(&app_state, &target)
            };

            let Some(index) = resolved else {
                let _ = reply.send(Err(crate::control_bridge::BridgeError::not_found(
                    "workspace not found",
                )));
                return;
            };
            let pane_id = parse_pane_handle(&pane_id).or_else(|| pane_id.parse::<u32>().ok());
            let Some(pane_id) = pane_id else {
                let _ = reply.send(Err(crate::control_bridge::BridgeError::invalid_params(
                    "pane.resize requires a valid pane_id",
                )));
                return;
            };

            let resize_target = {
                let app_state = state.borrow();
                let workspace = &app_state.workspaces[index];
                let Some(pane_widget) = pane::pane_widget_for_root(&workspace.root, pane_id) else {
                    let _ = reply.send(Err(crate::control_bridge::BridgeError::not_found(
                        "pane not found",
                    )));
                    return;
                };
                (
                    workspace.id.clone(),
                    workspace.name.clone(),
                    workspace.split_container.clone(),
                    pane_widget,
                )
            };
            let (workspace_id, workspace_name, split_container, pane_widget) = resize_target;
            let Some(ratio) = split_container.resize_pane(&pane_widget, &direction, amount) else {
                let _ = reply.send(Err(crate::control_bridge::BridgeError::conflict(
                    "pane cannot resize in that direction",
                )));
                return;
            };
            request_session_save(state);

            let Some(surface) = pane::active_surface_summary(&pane_widget) else {
                let _ = reply.send(Err(crate::control_bridge::BridgeError::not_found(
                    "pane not found",
                )));
                return;
            };
            let mut payload = pane_create_response_payload(&workspace_id, &workspace_name, surface);
            if let Some(map) = payload.as_object_mut() {
                map.insert(
                    "direction".to_string(),
                    serde_json::Value::String(direction),
                );
                map.insert("amount".to_string(), serde_json::json!(amount));
                map.insert("ratio".to_string(), serde_json::json!(ratio));
            }
            let _ = reply.send(Ok(payload));
        }
        ControlCommand::SwapPane {
            target,
            pane_id,
            target_pane_id,
            reply,
        } => {
            let resolved = {
                let app_state = state.borrow();
                workspace_index_for_target(&app_state, &target)
            };

            let Some(index) = resolved else {
                let _ = reply.send(Err(crate::control_bridge::BridgeError::not_found(
                    "workspace not found",
                )));
                return;
            };
            let pane_id = parse_pane_handle(&pane_id).or_else(|| pane_id.parse::<u32>().ok());
            let target_pane_id =
                parse_pane_handle(&target_pane_id).or_else(|| target_pane_id.parse::<u32>().ok());
            let (Some(pane_id), Some(target_pane_id)) = (pane_id, target_pane_id) else {
                let _ = reply.send(Err(crate::control_bridge::BridgeError::invalid_params(
                    "pane.swap requires valid pane ids",
                )));
                return;
            };
            if pane_id == target_pane_id {
                let _ = reply.send(Err(crate::control_bridge::BridgeError::conflict(
                    "cannot swap pane with itself",
                )));
                return;
            }

            let result = {
                let app_state = state.borrow();
                let workspace = &app_state.workspaces[index];
                let Some(first_surface) =
                    pane::selected_surface_for_pane_in_root(&workspace.root, pane_id)
                else {
                    let _ = reply.send(Err(crate::control_bridge::BridgeError::not_found(
                        "pane not found",
                    )));
                    return;
                };
                let Some(second_surface) =
                    pane::selected_surface_for_pane_in_root(&workspace.root, target_pane_id)
                else {
                    let _ = reply.send(Err(crate::control_bridge::BridgeError::not_found(
                        "target pane not found",
                    )));
                    return;
                };
                let Some(first_moved) = pane::move_surface_for_root(
                    &workspace.root,
                    &first_surface.surface_id,
                    target_pane_id,
                    Some(0),
                ) else {
                    let _ = reply.send(Err(crate::control_bridge::BridgeError::not_found(
                        "surface or target pane not found",
                    )));
                    return;
                };
                let Some(second_moved) = pane::move_surface_for_root(
                    &workspace.root,
                    &second_surface.surface_id,
                    pane_id,
                    Some(0),
                ) else {
                    let _ = reply.send(Err(crate::control_bridge::BridgeError::not_found(
                        "surface or pane not found",
                    )));
                    return;
                };
                let first_payload =
                    pane_create_response_payload(&workspace.id, &workspace.name, first_moved);
                let second_payload =
                    pane_create_response_payload(&workspace.id, &workspace.name, second_moved);
                serde_json::json!({
                    "ok": true,
                    "workspace_id": workspace.id.as_str(),
                    "workspace_ref": workspace_ref(&workspace.id),
                    "panes": [first_payload, second_payload],
                })
            };

            request_session_save(state);
            let _ = reply.send(Ok(result));
        }
        ControlCommand::JoinPane {
            target,
            source_pane_id,
            source_surface_id,
            target_pane_id,
            reply,
        } => {
            let resolved = {
                let app_state = state.borrow();
                workspace_index_for_target(&app_state, &target)
            };

            let Some(index) = resolved else {
                let _ = reply.send(Err(crate::control_bridge::BridgeError::not_found(
                    "workspace not found",
                )));
                return;
            };
            let target_pane_id =
                parse_pane_handle(&target_pane_id).or_else(|| target_pane_id.parse::<u32>().ok());
            let Some(target_pane_id) = target_pane_id else {
                let _ = reply.send(Err(crate::control_bridge::BridgeError::invalid_params(
                    "pane.join requires a valid target_pane_id",
                )));
                return;
            };
            let source_pane_raw = source_pane_id.clone();
            let source_pane_id = source_pane_id
                .as_deref()
                .and_then(parse_pane_handle)
                .or_else(|| {
                    source_pane_id
                        .as_deref()
                        .and_then(|raw| raw.parse::<u32>().ok())
                });
            if source_pane_raw.is_some() && source_pane_id.is_none() {
                let _ = reply.send(Err(crate::control_bridge::BridgeError::invalid_params(
                    "pane.join requires a valid pane_id",
                )));
                return;
            }
            if source_pane_id.is_none() && source_surface_id.is_none() {
                let app_state = state.borrow();
                if app_state.active_idx != index {
                    let _ = reply.send(Err(crate::control_bridge::BridgeError::invalid_params(
                        "pane.join requires source pane or surface for inactive workspaces",
                    )));
                    return;
                }
            }

            let moved = {
                let app_state = state.borrow();
                let workspace = &app_state.workspaces[index];
                let (_, focused_surface_id) = focused_ids_for_workspace(state, &workspace.id);
                let source_surface = if let Some(surface_id) = source_surface_id.as_deref() {
                    Some(surface_id.to_string())
                } else if let Some(source_pane_id) = source_pane_id {
                    pane::pane_widget_for_root(&workspace.root, source_pane_id)
                        .and_then(|pane_widget| pane::active_surface_summary(&pane_widget))
                        .map(|surface| surface.surface_id)
                } else {
                    focused_surface_id
                };
                let Some(source_surface) = source_surface else {
                    let _ = reply.send(Err(crate::control_bridge::BridgeError::not_found(
                        "surface not found",
                    )));
                    return;
                };
                let moved = pane::move_surface_for_root(
                    &workspace.root,
                    &source_surface,
                    target_pane_id,
                    None,
                );
                moved.map(|surface| {
                    pane_create_response_payload(&workspace.id, &workspace.name, surface)
                })
            };

            let Some(payload) = moved else {
                let _ = reply.send(Err(crate::control_bridge::BridgeError::not_found(
                    "pane or surface not found",
                )));
                return;
            };
            request_session_save(state);
            let _ = reply.send(Ok(payload));
        }
        ControlCommand::BreakPane {
            target,
            pane_id,
            surface_hint,
            reply,
        } => {
            let resolved = {
                let app_state = state.borrow();
                workspace_index_for_target(&app_state, &target)
            };

            let Some(index) = resolved else {
                let _ = reply.send(Err(crate::control_bridge::BridgeError::not_found(
                    "workspace not found",
                )));
                return;
            };
            let pane_raw = pane_id.clone();
            let pane_id = pane_id
                .as_deref()
                .and_then(parse_pane_handle)
                .or_else(|| pane_id.as_deref().and_then(|raw| raw.parse::<u32>().ok()));
            if pane_raw.is_some() && pane_id.is_none() {
                let _ = reply.send(Err(crate::control_bridge::BridgeError::invalid_params(
                    "pane.break requires a valid pane_id",
                )));
                return;
            }
            if pane_id.is_none() && surface_hint.is_none() {
                let app_state = state.borrow();
                if app_state.active_idx != index {
                    let _ = reply.send(Err(crate::control_bridge::BridgeError::invalid_params(
                        "pane.break requires pane or surface for inactive workspaces",
                    )));
                    return;
                }
            }

            let source_surface = {
                let app_state = state.borrow();
                let workspace = &app_state.workspaces[index];
                if let Some(surface_hint) = surface_hint.as_deref() {
                    pane::surface_summaries_for_root(&workspace.root)
                        .into_iter()
                        .find(|surface| surface_hint_matches(&surface.surface_id, surface_hint))
                        .map(|surface| surface.surface_id)
                } else if let Some(pane_id) = pane_id {
                    pane::pane_widget_for_root(&workspace.root, pane_id)
                        .and_then(|pane_widget| pane::active_surface_summary(&pane_widget))
                        .map(|surface| surface.surface_id)
                } else {
                    focused_ids_for_workspace(state, &workspace.id).1
                }
            };
            let Some(source_surface) = source_surface else {
                let _ = reply.send(Err(crate::control_bridge::BridgeError::not_found(
                    "surface not found",
                )));
                return;
            };
            let payload = match create_workspace_for_tab_payload(state, &source_surface, None, true)
            {
                Ok(payload) => payload,
                Err(error) => {
                    let _ = reply.send(Err(error));
                    return;
                }
            };
            let _ = reply.send(Ok(payload));
        }
        ControlCommand::MoveTabToNewWorkspace {
            target,
            surface_hint,
            title,
            focus,
            reply,
        } => {
            let resolved = {
                let app_state = state.borrow();
                workspace_index_for_target(&app_state, &target)
            };

            let Some(index) = resolved else {
                let _ = reply.send(Err(crate::control_bridge::BridgeError::not_found(
                    "workspace not found",
                )));
                return;
            };

            let source_surface = {
                let app_state = state.borrow();
                let workspace = &app_state.workspaces[index];
                surface_hint
                    .as_deref()
                    .and_then(|surface_hint| {
                        pane::surface_summaries_for_root(&workspace.root)
                            .into_iter()
                            .find(|surface| surface_hint_matches(&surface.surface_id, surface_hint))
                            .map(|surface| surface.surface_id)
                    })
                    .or_else(|| focused_ids_for_workspace(state, &workspace.id).1)
            };
            let Some(source_surface) = source_surface else {
                let _ = reply.send(Err(crate::control_bridge::BridgeError::not_found(
                    "surface not found",
                )));
                return;
            };
            let payload = match create_workspace_for_tab_payload(
                state,
                &source_surface,
                title.as_deref(),
                focus,
            ) {
                Ok(payload) => payload,
                Err(error) => {
                    let _ = reply.send(Err(error));
                    return;
                }
            };
            let _ = reply.send(Ok(payload));
        }
        ControlCommand::TabAction {
            target,
            surface_hint,
            action,
            title,
            reply,
        } => {
            let resolved = {
                let app_state = state.borrow();
                workspace_index_for_target(&app_state, &target)
            };
            let Some(index) = resolved else {
                let _ = reply.send(Err(BridgeError::not_found("workspace not found")));
                return;
            };

            let (workspace_id, workspace_root) = {
                let app_state = state.borrow();
                let workspace = &app_state.workspaces[index];
                (workspace.id.clone(), workspace.root.clone())
            };
            let summary = match pane::apply_tab_action_for_root(
                &workspace_root,
                surface_hint.as_deref(),
                &action,
                title.as_deref(),
            ) {
                Ok(summary) => summary,
                Err(pane::TabActionError::NotFound) => {
                    let _ = reply.send(Err(BridgeError::not_found("tab not found")));
                    return;
                }
                Err(pane::TabActionError::UnsupportedForSurface) => {
                    let _ = reply.send(Err(BridgeError::invalid_params(format!(
                        "tab.action {action} unsupported for surface type"
                    ))));
                    return;
                }
            };
            let unread = apply_tab_read_state_action(state, &workspace_id, &summary, &action);
            let payload = tab_action_payload(&workspace_id, &action, &summary, unread);
            request_session_save(state);
            let _ = reply.send(Ok(payload));
        }
        ControlCommand::BrowserTabAction {
            target,
            surface_hint,
            action,
            reply,
        } => {
            let resolved = {
                let app_state = state.borrow();
                workspace_index_for_target(&app_state, &target)
            };
            let Some(index) = resolved else {
                let _ = reply.send(Err(BridgeError::not_found("workspace not found")));
                return;
            };
            let (workspace_id, workspace_name, workspace_root) = {
                let app_state = state.borrow();
                let workspace = &app_state.workspaces[index];
                (
                    workspace.id.clone(),
                    workspace.name.clone(),
                    workspace.root.clone(),
                )
            };

            let result = match action {
                BrowserTabAction::List => {
                    let tabs = pane::browser_tab_summaries_for_root(&workspace_root, &surface_hint);
                    let Some(tabs) = tabs else {
                        let _ =
                            reply.send(Err(BridgeError::not_found("browser surface not found")));
                        return;
                    };
                    let (_, focused_surface_id) = focused_ids_for_workspace(state, &workspace_id);
                    Ok(browser_tab_list_payload(focused_surface_id, tabs))
                }
                BrowserTabAction::New { url } => {
                    let surface = pane::add_browser_tab_for_root(
                        &workspace_root,
                        &surface_hint,
                        url.as_deref(),
                    );
                    surface
                        .map(|surface| {
                            pane_create_response_payload(&workspace_id, &workspace_name, surface)
                        })
                        .ok_or_else(|| BridgeError::not_found("browser surface not found"))
                }
                BrowserTabAction::Switch {
                    target_surface_hint,
                } => {
                    let tabs = pane::browser_tab_summaries_for_root(&workspace_root, &surface_hint);
                    let Some(tabs) = tabs else {
                        let _ =
                            reply.send(Err(BridgeError::not_found("browser surface not found")));
                        return;
                    };
                    let target_in_context = tabs
                        .iter()
                        .any(|tab| surface_hint_matches(&tab.surface_id, &target_surface_hint));
                    if !target_in_context {
                        let _ = reply.send(Err(BridgeError::not_found("browser tab not found")));
                        return;
                    }
                    pane::focus_surface_for_root(&workspace_root, &target_surface_hint)
                        .filter(|surface| surface.kind == "browser")
                        .map(|surface| {
                            pane_create_response_payload(&workspace_id, &workspace_name, surface)
                        })
                        .ok_or_else(|| BridgeError::not_found("browser tab not found"))
                }
                BrowserTabAction::Close {
                    target_surface_hint,
                } => match pane::close_browser_tab_for_root(
                    &workspace_root,
                    &surface_hint,
                    target_surface_hint.as_deref(),
                ) {
                    Ok(surface) => {
                        let mut payload =
                            pane_create_response_payload(&workspace_id, &workspace_name, surface);
                        payload["closed"] = serde_json::Value::Bool(true);
                        Ok(payload)
                    }
                    Err(pane::BrowserTabCloseError::LastBrowserTab) => {
                        Err(BridgeError::conflict("cannot close last browser tab"))
                    }
                    Err(pane::BrowserTabCloseError::ContextNotFound) => {
                        Err(BridgeError::not_found("browser surface not found"))
                    }
                    Err(pane::BrowserTabCloseError::TargetNotFound) => {
                        Err(BridgeError::not_found("browser tab not found"))
                    }
                },
            };

            let _ = reply.send(result);
        }
        ControlCommand::BrowserAction {
            target,
            surface_hint,
            action,
            reply,
        } => {
            let resolved = {
                let app_state = state.borrow();
                workspace_index_for_target(&app_state, &target)
            };
            let Some(index) = resolved else {
                let _ = reply.send(Err(BridgeError::not_found("workspace not found")));
                return;
            };
            let (workspace_id, workspace_name, workspace_root) = {
                let app_state = state.borrow();
                let workspace = &app_state.workspaces[index];
                (
                    workspace.id.clone(),
                    workspace.name.clone(),
                    workspace.root.clone(),
                )
            };
            let Some(browser) = pane::browser_target_for_root(&workspace_root, &surface_hint)
            else {
                let _ = reply.send(Err(BridgeError::not_found("browser surface not found")));
                return;
            };
            match &action {
                BrowserAction::IsFocused => {
                    let mut payload =
                        browser_action_response_payload(&workspace_id, &workspace_name, &browser);
                    payload["focused"] = serde_json::Value::Bool(browser.is_content_focused());
                    let _ = reply.send(Ok(payload));
                    return;
                }
                BrowserAction::Eval { script } => {
                    let payload =
                        browser_action_response_payload(&workspace_id, &workspace_name, &browser);
                    send_browser_eval_response(browser, script.clone(), payload, "value", reply);
                    return;
                }
                BrowserAction::GetTitle => {
                    let payload =
                        browser_action_response_payload(&workspace_id, &workspace_name, &browser);
                    send_browser_eval_response(
                        browser,
                        "document.title".to_string(),
                        payload,
                        "title",
                        reply,
                    );
                    return;
                }
                BrowserAction::GetText { selector } => {
                    let payload =
                        browser_action_response_payload(&workspace_id, &workspace_name, &browser);
                    let script = browser_optional_element_script(
                        selector.as_deref(),
                        "document.body ? document.body.innerText : ''",
                        "node.innerText || node.textContent || ''",
                    );
                    send_browser_eval_response(browser, script, payload, "text", reply);
                    return;
                }
                BrowserAction::GetValue { selector } => {
                    let payload =
                        browser_action_response_payload(&workspace_id, &workspace_name, &browser);
                    let script = browser_required_element_script(
                        selector,
                        "node.value !== undefined ? node.value : node.textContent || ''",
                    );
                    send_browser_eval_response(browser, script, payload, "box", reply);
                    return;
                }
                BrowserAction::GetAttr { selector, name } => {
                    let payload =
                        browser_action_response_payload(&workspace_id, &workspace_name, &browser);
                    let name = serde_json::to_string(name).expect("json attr name");
                    let script = browser_required_element_script(
                        selector,
                        &format!("node.getAttribute({name})"),
                    );
                    send_browser_eval_response(browser, script, payload, "styles", reply);
                    return;
                }
                BrowserAction::GetCount { selector } => {
                    let payload =
                        browser_action_response_payload(&workspace_id, &workspace_name, &browser);
                    let script = browser_count_script(selector);
                    send_browser_eval_response(browser, script, payload, "count", reply);
                    return;
                }
                BrowserAction::GetBox { selector } => {
                    let payload =
                        browser_action_response_payload(&workspace_id, &workspace_name, &browser);
                    let script = browser_required_element_script(
                        selector,
                        r#"(() => {
  const rect = node.getBoundingClientRect();
  return {
    x: rect.x,
    y: rect.y,
    width: rect.width,
    height: rect.height,
    top: rect.top,
    right: rect.right,
    bottom: rect.bottom,
    left: rect.left
  };
})()"#,
                    );
                    send_browser_eval_response(browser, script, payload, "value", reply);
                    return;
                }
                BrowserAction::GetHtml { selector } => {
                    let payload =
                        browser_action_response_payload(&workspace_id, &workspace_name, &browser);
                    let script = browser_optional_element_script(
                        selector.as_deref(),
                        "document.documentElement ? document.documentElement.outerHTML : ''",
                        "node.outerHTML",
                    );
                    send_browser_eval_response(browser, script, payload, "html", reply);
                    return;
                }
                BrowserAction::GetStyles { selector, property } => {
                    let payload =
                        browser_action_response_payload(&workspace_id, &workspace_name, &browser);
                    let script = browser_styles_script(selector, property.as_deref());
                    send_browser_eval_response(browser, script, payload, "value", reply);
                    return;
                }
                BrowserAction::IsChecked { selector } => {
                    let payload =
                        browser_action_response_payload(&workspace_id, &workspace_name, &browser);
                    let script = browser_is_script(selector, "checked");
                    send_browser_eval_response(browser, script, payload, "checked", reply);
                    return;
                }
                BrowserAction::IsEnabled { selector } => {
                    let payload =
                        browser_action_response_payload(&workspace_id, &workspace_name, &browser);
                    let script = browser_is_script(selector, "enabled");
                    send_browser_eval_response(browser, script, payload, "enabled", reply);
                    return;
                }
                BrowserAction::IsVisible { selector } => {
                    let payload =
                        browser_action_response_payload(&workspace_id, &workspace_name, &browser);
                    let script = browser_is_script(selector, "visible");
                    send_browser_eval_response(browser, script, payload, "visible", reply);
                    return;
                }
                BrowserAction::Find { .. } => {
                    let payload =
                        browser_action_response_payload(&workspace_id, &workspace_name, &browser);
                    let script = browser_find_script(&action);
                    send_browser_object_response(browser, script, payload, reply);
                    return;
                }
                BrowserAction::FrameSelect { selector } => {
                    let mut payload =
                        browser_action_response_payload(&workspace_id, &workspace_name, &browser);
                    browser.select_frame(selector, move |result| match result {
                        Ok(frame_id) => {
                            payload["frame_id"] = serde_json::Value::String(frame_id);
                            let _ = reply.send(Ok(payload));
                        }
                        Err(error) => {
                            let _ = reply.send(Err(BridgeError::not_found(error)));
                        }
                    });
                    return;
                }
                BrowserAction::FrameMain => {
                    let mut payload =
                        browser_action_response_payload(&workspace_id, &workspace_name, &browser);
                    browser.reset_frame();
                    payload["frame_id"] = serde_json::Value::String("main".to_string());
                    let _ = reply.send(Ok(payload));
                    return;
                }
                BrowserAction::DialogAccept { text } => {
                    let mut payload =
                        browser_action_response_payload(&workspace_id, &workspace_name, &browser);
                    browser.respond_to_dialog(true, text.clone(), move |result| match result {
                        Ok(result) => {
                            payload["accepted"] = serde_json::Value::Bool(result.accepted);
                            payload["dialog"] = serde_json::json!({
                                "type": result.kind,
                                "message": result.message,
                                "default_text": result.default_text,
                                "text": result.text,
                            });
                            let _ = reply.send(Ok(payload));
                        }
                        Err(error) => {
                            let _ = reply.send(Err(BridgeError::not_found(error)));
                        }
                    });
                    return;
                }
                BrowserAction::DialogDismiss => {
                    let mut payload =
                        browser_action_response_payload(&workspace_id, &workspace_name, &browser);
                    browser.respond_to_dialog(false, None, move |result| match result {
                        Ok(result) => {
                            payload["accepted"] = serde_json::Value::Bool(result.accepted);
                            payload["dialog"] = serde_json::json!({
                                "type": result.kind,
                                "message": result.message,
                                "default_text": result.default_text,
                                "text": result.text,
                            });
                            let _ = reply.send(Ok(payload));
                        }
                        Err(error) => {
                            let _ = reply.send(Err(BridgeError::not_found(error)));
                        }
                    });
                    return;
                }
                BrowserAction::DownloadWait { path, timeout_ms } => {
                    let mut payload =
                        browser_action_response_payload(&workspace_id, &workspace_name, &browser);
                    let path = path.as_ref().map(PathBuf::from);
                    browser.wait_for_download(path, *timeout_ms, move |result| match result {
                        Ok(path) => {
                            payload["downloaded"] = serde_json::Value::Bool(true);
                            payload["path"] =
                                serde_json::Value::String(path.to_string_lossy().into_owned());
                            let _ = reply.send(Ok(payload));
                        }
                        Err(error) => {
                            let _ = reply.send(Err(BridgeError::not_found(error)));
                        }
                    });
                    return;
                }
                BrowserAction::Screenshot { path, full_page } => {
                    let payload =
                        browser_action_response_payload(&workspace_id, &workspace_name, &browser);
                    send_browser_screenshot_response(
                        browser,
                        path.clone(),
                        *full_page,
                        payload,
                        reply,
                    );
                    return;
                }
                BrowserAction::Snapshot {
                    interactive,
                    compact,
                    max_depth,
                } => {
                    let mut payload =
                        browser_action_response_payload(&workspace_id, &workspace_name, &browser);
                    if let Some(url) = browser.current_uri() {
                        payload["url"] = serde_json::Value::String(url);
                    }
                    let script = browser_snapshot_script(*interactive, *compact, *max_depth);
                    send_browser_eval_response(browser, script, payload, "snapshot", reply);
                    return;
                }
                BrowserAction::Wait { .. } => {
                    let mut payload =
                        browser_action_response_payload(&workspace_id, &workspace_name, &browser);
                    let current_uri = browser.current_uri();
                    if let Some(url) = current_uri.clone() {
                        payload["url"] = serde_json::Value::String(url);
                    }
                    let script = browser_wait_script(&action, current_uri.as_deref());
                    let timeout_ms = match &action {
                        BrowserAction::Wait { timeout_ms, .. } => *timeout_ms,
                        _ => unreachable!("browser wait action matched above"),
                    };
                    send_browser_wait_response(browser, script, payload, reply, timeout_ms);
                    return;
                }
                BrowserAction::Click { selector } => {
                    let payload =
                        browser_action_response_payload(&workspace_id, &workspace_name, &browser);
                    let event = browser_event(
                        "browser.interaction",
                        &workspace_id,
                        &browser,
                        "browser.click",
                        serde_json::json!({ "selector": selector }),
                    );
                    let script = browser_element_action_script(
                        selector,
                        "node.click(); return { action: 'click', selector, ok: true };",
                    );
                    send_browser_eval_response_with_event(
                        browser, script, payload, "action", event, reply,
                    );
                    return;
                }
                BrowserAction::DblClick { selector } => {
                    let payload =
                        browser_action_response_payload(&workspace_id, &workspace_name, &browser);
                    let event = browser_event(
                        "browser.interaction",
                        &workspace_id,
                        &browser,
                        "browser.dblclick",
                        serde_json::json!({ "selector": selector }),
                    );
                    let script = browser_element_action_script(selector, browser_dblclick_body());
                    send_browser_eval_response_with_event(
                        browser, script, payload, "action", event, reply,
                    );
                    return;
                }
                BrowserAction::Fill { selector, text } => {
                    let payload =
                        browser_action_response_payload(&workspace_id, &workspace_name, &browser);
                    let event = browser_event(
                        "browser.input",
                        &workspace_id,
                        &browser,
                        "browser.fill",
                        serde_json::json!({
                            "selector": selector,
                            "text_length": text.len(),
                            "redacted_fields": ["text"],
                        }),
                    );
                    let script = browser_element_action_script(selector, &browser_fill_body(text));
                    send_browser_eval_response_with_event(
                        browser, script, payload, "action", event, reply,
                    );
                    return;
                }
                BrowserAction::Type { selector, text } => {
                    let payload =
                        browser_action_response_payload(&workspace_id, &workspace_name, &browser);
                    let event = browser_event(
                        "browser.input",
                        &workspace_id,
                        &browser,
                        "browser.type",
                        serde_json::json!({
                            "selector": selector,
                            "text_length": text.len(),
                            "redacted_fields": ["text"],
                        }),
                    );
                    let script = browser_element_action_script(selector, &browser_type_body(text));
                    send_browser_eval_response_with_event(
                        browser, script, payload, "action", event, reply,
                    );
                    return;
                }
                BrowserAction::Select { selector, value } => {
                    let payload =
                        browser_action_response_payload(&workspace_id, &workspace_name, &browser);
                    let event = browser_event(
                        "browser.interaction",
                        &workspace_id,
                        &browser,
                        "browser.select",
                        serde_json::json!({
                            "selector": selector,
                            "value_length": value.len(),
                            "redacted_fields": ["value"],
                        }),
                    );
                    let script =
                        browser_element_action_script(selector, &browser_select_body(value));
                    send_browser_eval_response_with_event(
                        browser, script, payload, "action", event, reply,
                    );
                    return;
                }
                BrowserAction::Hover { selector } => {
                    let payload =
                        browser_action_response_payload(&workspace_id, &workspace_name, &browser);
                    let event = browser_event(
                        "browser.interaction",
                        &workspace_id,
                        &browser,
                        "browser.hover",
                        serde_json::json!({ "selector": selector }),
                    );
                    let script = browser_element_action_script(selector, browser_hover_body());
                    send_browser_eval_response_with_event(
                        browser, script, payload, "action", event, reply,
                    );
                    return;
                }
                BrowserAction::FocusElement { selector } => {
                    let payload =
                        browser_action_response_payload(&workspace_id, &workspace_name, &browser);
                    let event = browser_event(
                        "browser.interaction",
                        &workspace_id,
                        &browser,
                        "browser.focus",
                        serde_json::json!({ "selector": selector }),
                    );
                    let script = browser_element_action_script(selector, browser_focus_body());
                    send_browser_eval_response_with_event(
                        browser, script, payload, "action", event, reply,
                    );
                    return;
                }
                BrowserAction::Check { selector } | BrowserAction::Uncheck { selector } => {
                    let checked = matches!(action, BrowserAction::Check { .. });
                    let payload =
                        browser_action_response_payload(&workspace_id, &workspace_name, &browser);
                    let command = if checked {
                        "browser.check"
                    } else {
                        "browser.uncheck"
                    };
                    let event = browser_event(
                        "browser.interaction",
                        &workspace_id,
                        &browser,
                        command,
                        serde_json::json!({ "selector": selector }),
                    );
                    let script =
                        browser_element_action_script(selector, &browser_check_body(checked));
                    send_browser_eval_response_with_event(
                        browser, script, payload, "action", event, reply,
                    );
                    return;
                }
                BrowserAction::Press { key } => {
                    let payload =
                        browser_action_response_payload(&workspace_id, &workspace_name, &browser);
                    let event = browser_event(
                        "browser.interaction",
                        &workspace_id,
                        &browser,
                        "browser.press",
                        serde_json::json!({ "key": key }),
                    );
                    let script = browser_key_action_script(key, "keydown");
                    send_browser_eval_response_with_event(
                        browser, script, payload, "action", event, reply,
                    );
                    return;
                }
                BrowserAction::KeyDown { key } => {
                    let payload =
                        browser_action_response_payload(&workspace_id, &workspace_name, &browser);
                    let event = browser_event(
                        "browser.interaction",
                        &workspace_id,
                        &browser,
                        "browser.keydown",
                        serde_json::json!({ "key": key }),
                    );
                    let script = browser_key_action_script(key, "keydown");
                    send_browser_eval_response_with_event(
                        browser, script, payload, "action", event, reply,
                    );
                    return;
                }
                BrowserAction::KeyUp { key } => {
                    let payload =
                        browser_action_response_payload(&workspace_id, &workspace_name, &browser);
                    let event = browser_event(
                        "browser.interaction",
                        &workspace_id,
                        &browser,
                        "browser.keyup",
                        serde_json::json!({ "key": key }),
                    );
                    let script = browser_key_action_script(key, "keyup");
                    send_browser_eval_response_with_event(
                        browser, script, payload, "action", event, reply,
                    );
                    return;
                }
                BrowserAction::Scroll { selector, dx, dy } => {
                    let payload =
                        browser_action_response_payload(&workspace_id, &workspace_name, &browser);
                    let event = browser_event(
                        "browser.interaction",
                        &workspace_id,
                        &browser,
                        "browser.scroll",
                        serde_json::json!({ "selector": selector, "dx": dx, "dy": dy }),
                    );
                    let script = browser_scroll_script(selector.as_deref(), *dx, *dy);
                    send_browser_eval_response_with_event(
                        browser, script, payload, "action", event, reply,
                    );
                    return;
                }
                BrowserAction::ScrollIntoView { selector } => {
                    let payload =
                        browser_action_response_payload(&workspace_id, &workspace_name, &browser);
                    let event = browser_event(
                        "browser.interaction",
                        &workspace_id,
                        &browser,
                        "browser.scroll_into_view",
                        serde_json::json!({ "selector": selector }),
                    );
                    let script = browser_element_action_script(
                        selector,
                        "node.scrollIntoView({ block: 'center', inline: 'center' }); return { action: 'scroll_into_view', selector, ok: true };",
                    );
                    send_browser_eval_response_with_event(
                        browser, script, payload, "action", event, reply,
                    );
                    return;
                }
                BrowserAction::AddScript { script } | BrowserAction::AddInitScript { script } => {
                    let payload =
                        browser_action_response_payload(&workspace_id, &workspace_name, &browser);
                    send_browser_eval_response(browser, script.clone(), payload, "value", reply);
                    return;
                }
                BrowserAction::AddStyle { css } => {
                    let payload =
                        browser_action_response_payload(&workspace_id, &workspace_name, &browser);
                    let script = browser_add_style_script(css);
                    send_browser_eval_response(browser, script, payload, "action", reply);
                    return;
                }
                BrowserAction::ConsoleList => {
                    let mut payload =
                        browser_action_response_payload(&workspace_id, &workspace_name, &browser);
                    let snapshot = browser.console_entries();
                    payload["entries"] = serde_json::Value::Array(snapshot.entries);
                    payload["count"] = serde_json::Value::Number(snapshot.count.into());
                    let _ = reply.send(Ok(payload));
                    return;
                }
                BrowserAction::ConsoleClear => {
                    let mut payload =
                        browser_action_response_payload(&workspace_id, &workspace_name, &browser);
                    let cleared = browser.clear_console_entries();
                    payload["cleared"] = serde_json::Value::Bool(true);
                    payload["count"] = serde_json::Value::Number(cleared.into());
                    let _ = reply.send(Ok(payload));
                    return;
                }
                BrowserAction::ErrorsList => {
                    let mut payload =
                        browser_action_response_payload(&workspace_id, &workspace_name, &browser);
                    let snapshot = browser.error_entries();
                    payload["errors"] = serde_json::Value::Array(snapshot.entries);
                    payload["count"] = serde_json::Value::Number(snapshot.count.into());
                    let _ = reply.send(Ok(payload));
                    return;
                }
                BrowserAction::ErrorsClear => {
                    let mut payload =
                        browser_action_response_payload(&workspace_id, &workspace_name, &browser);
                    let cleared = browser.clear_error_entries();
                    payload["cleared"] = serde_json::Value::Bool(true);
                    payload["count"] = serde_json::Value::Number(cleared.into());
                    let _ = reply.send(Ok(payload));
                    return;
                }
                BrowserAction::Highlight { selector } => {
                    let payload =
                        browser_action_response_payload(&workspace_id, &workspace_name, &browser);
                    let script = browser_highlight_script(selector);
                    send_browser_eval_response(browser, script, payload, "action", reply);
                    return;
                }
                BrowserAction::CookiesGet { name } => {
                    let payload =
                        browser_action_response_payload(&workspace_id, &workspace_name, &browser);
                    let script = browser_cookies_get_script(name.as_deref());
                    send_browser_eval_response(browser, script, payload, "cookies", reply);
                    return;
                }
                BrowserAction::CookiesSet { name, value } => {
                    let payload =
                        browser_action_response_payload(&workspace_id, &workspace_name, &browser);
                    let script = browser_cookie_set_script(name, value);
                    send_browser_eval_response(browser, script, payload, "action", reply);
                    return;
                }
                BrowserAction::CookiesClear { name } => {
                    let payload =
                        browser_action_response_payload(&workspace_id, &workspace_name, &browser);
                    let script = browser_cookies_clear_script(name.as_deref());
                    send_browser_eval_response(browser, script, payload, "action", reply);
                    return;
                }
                BrowserAction::StorageGet { storage_type, key } => {
                    let payload =
                        browser_action_response_payload(&workspace_id, &workspace_name, &browser);
                    let script = browser_storage_get_script(storage_type, key);
                    send_browser_eval_response(browser, script, payload, "value", reply);
                    return;
                }
                BrowserAction::StorageSet {
                    storage_type,
                    key,
                    value,
                } => {
                    let payload =
                        browser_action_response_payload(&workspace_id, &workspace_name, &browser);
                    let script = browser_storage_set_script(storage_type, key, value);
                    send_browser_eval_response(browser, script, payload, "action", reply);
                    return;
                }
                BrowserAction::StorageClear { storage_type, key } => {
                    let payload =
                        browser_action_response_payload(&workspace_id, &workspace_name, &browser);
                    let script = browser_storage_clear_script(storage_type, key.as_deref());
                    send_browser_eval_response(browser, script, payload, "action", reply);
                    return;
                }
                BrowserAction::StateSave { path } => {
                    let payload =
                        browser_action_response_payload(&workspace_id, &workspace_name, &browser);
                    send_browser_state_save_response(browser, path.clone(), payload, reply);
                    return;
                }
                BrowserAction::StateLoad { path } => {
                    let payload =
                        browser_action_response_payload(&workspace_id, &workspace_name, &browser);
                    let raw = match std::fs::read_to_string(path) {
                        Ok(raw) => raw,
                        Err(error) => {
                            let _ = reply.send(Err(BridgeError::invalid_params(format!(
                                "browser state read failed: {error}"
                            ))));
                            return;
                        }
                    };
                    let state_json = match serde_json::from_str::<serde_json::Value>(&raw) {
                        Ok(value) => value,
                        Err(error) => {
                            let _ = reply.send(Err(BridgeError::invalid_params(format!(
                                "browser state JSON is invalid: {error}"
                            ))));
                            return;
                        }
                    };
                    let script = match browser_state_load_script(&state_json) {
                        Ok(script) => script,
                        Err(error) => {
                            let _ = reply.send(Err(BridgeError::internal(format!(
                                "browser state restore encoding failed: {error}"
                            ))));
                            return;
                        }
                    };
                    send_browser_eval_response(browser, script, payload, "state", reply);
                    return;
                }
                _ => {}
            }
            let ok = match &action {
                BrowserAction::Navigate { url } => browser.navigate(url),
                BrowserAction::GetUrl => true,
                BrowserAction::Back => browser.go_back(),
                BrowserAction::Forward => browser.go_forward(),
                BrowserAction::Reload => browser.reload(),
                BrowserAction::Focus => browser.focus_content(),
                BrowserAction::Click { .. }
                | BrowserAction::DblClick { .. }
                | BrowserAction::Fill { .. }
                | BrowserAction::Type { .. }
                | BrowserAction::Select { .. }
                | BrowserAction::Hover { .. }
                | BrowserAction::FocusElement { .. }
                | BrowserAction::Check { .. }
                | BrowserAction::Uncheck { .. }
                | BrowserAction::Press { .. }
                | BrowserAction::KeyDown { .. }
                | BrowserAction::KeyUp { .. }
                | BrowserAction::Scroll { .. }
                | BrowserAction::ScrollIntoView { .. }
                | BrowserAction::GetTitle
                | BrowserAction::GetText { .. }
                | BrowserAction::GetValue { .. }
                | BrowserAction::GetAttr { .. }
                | BrowserAction::GetCount { .. }
                | BrowserAction::GetBox { .. }
                | BrowserAction::GetHtml { .. }
                | BrowserAction::GetStyles { .. }
                | BrowserAction::IsChecked { .. }
                | BrowserAction::IsEnabled { .. }
                | BrowserAction::IsVisible { .. }
                | BrowserAction::Screenshot { .. }
                | BrowserAction::Find { .. }
                | BrowserAction::FrameSelect { .. }
                | BrowserAction::FrameMain
                | BrowserAction::DialogAccept { .. }
                | BrowserAction::DialogDismiss
                | BrowserAction::DownloadWait { .. }
                | BrowserAction::Snapshot { .. }
                | BrowserAction::Wait { .. }
                | BrowserAction::AddScript { .. }
                | BrowserAction::AddInitScript { .. }
                | BrowserAction::AddStyle { .. }
                | BrowserAction::ConsoleList
                | BrowserAction::ConsoleClear
                | BrowserAction::ErrorsList
                | BrowserAction::ErrorsClear
                | BrowserAction::Highlight { .. }
                | BrowserAction::CookiesGet { .. }
                | BrowserAction::CookiesSet { .. }
                | BrowserAction::CookiesClear { .. }
                | BrowserAction::StorageGet { .. }
                | BrowserAction::StorageSet { .. }
                | BrowserAction::StorageClear { .. }
                | BrowserAction::StateSave { .. }
                | BrowserAction::StateLoad { .. } => {
                    unreachable!("read-only browser action handled above")
                }
                BrowserAction::IsFocused | BrowserAction::Eval { .. } => {
                    unreachable!("read-only browser action handled above")
                }
            };
            if !ok {
                let _ = reply.send(Err(BridgeError::invalid_params(
                    "browser action is not supported by this build",
                )));
                return;
            }
            let url = match &action {
                BrowserAction::Navigate { url } => Some(url.clone()),
                _ => browser.current_uri(),
            };
            let mut payload =
                browser_action_response_payload(&workspace_id, &workspace_name, &browser);
            if let Some(url) = url {
                payload["url"] = serde_json::Value::String(url);
            }
            let event = match &action {
                BrowserAction::Navigate { url } => Some(browser_event(
                    "browser.navigation",
                    &workspace_id,
                    &browser,
                    "browser.navigate",
                    serde_json::json!({ "url": url }),
                )),
                BrowserAction::Back => Some(browser_event(
                    "browser.navigation",
                    &workspace_id,
                    &browser,
                    "browser.back",
                    serde_json::json!({ "url": payload.get("url").cloned() }),
                )),
                BrowserAction::Forward => Some(browser_event(
                    "browser.navigation",
                    &workspace_id,
                    &browser,
                    "browser.forward",
                    serde_json::json!({ "url": payload.get("url").cloned() }),
                )),
                BrowserAction::Reload => Some(browser_event(
                    "browser.navigation",
                    &workspace_id,
                    &browser,
                    "browser.reload",
                    serde_json::json!({ "url": payload.get("url").cloned() }),
                )),
                BrowserAction::Focus => Some(browser_event(
                    "browser.interaction",
                    &workspace_id,
                    &browser,
                    "browser.focus_webview",
                    serde_json::json!({}),
                )),
                BrowserAction::GetUrl => None,
                _ => unreachable!("only synchronous browser actions reach event publication"),
            };
            if let Some(event) = event {
                publish_browser_event(event);
            }
            let _ = reply.send(Ok(payload));
        }
        ControlCommand::CreatePane { request, reply } => {
            let source_pane_id = request
                .source_pane_id
                .as_deref()
                .and_then(parse_pane_handle);
            if request.source_pane_id.is_some() && source_pane_id.is_none() {
                let _ = reply.send(Err(BridgeError::invalid_params(
                    "pane.create requires a valid pane_id",
                )));
                return;
            }

            let direction = PaneCreateDirection::from(request.direction);
            let resolved = match resolve_pane_create_target(
                state,
                &request.target,
                request.source_surface_id.as_deref(),
                source_pane_id,
                direction,
            ) {
                Ok(resolved) => resolved,
                Err(error) => {
                    let _ = reply.send(Err(pane_create_target_error(error)));
                    return;
                }
            };

            let workspace_name = {
                let app_state = state.borrow();
                app_state
                    .workspaces
                    .iter()
                    .find(|workspace| workspace.id == resolved.workspace_id)
                    .map(|workspace| workspace.name.clone())
            };
            let Some(workspace_name) = workspace_name else {
                let _ = reply.send(Err(BridgeError::not_found("workspace not found")));
                return;
            };

            let startup_requested = request.initial_command.is_some()
                || request.working_directory.is_some()
                || !request.startup_environment.is_empty();
            let new_pane = split_pane(
                state,
                &resolved.workspace_id,
                &resolved.pane_widget,
                resolved.placement.orientation,
                SplitPaneOptions {
                    initial_state: match request.pane_type {
                        PaneCreateType::Terminal => None,
                        PaneCreateType::Browser => {
                            Some(PaneState::browser_only(request.url.as_deref()))
                        }
                    },
                    skip_default_tab: startup_requested,
                    new_pane_first: resolved.placement.new_pane_first,
                    initial_ratio: request.initial_divider_position,
                    persist: true,
                },
            );
            let Some(new_pane) = new_pane else {
                let _ = reply.send(Err(BridgeError::invalid_params(
                    "not enough room to split pane",
                )));
                return;
            };

            let surface = if startup_requested {
                pane::add_terminal_tab_to_pane_with_launch_options(
                    &new_pane,
                    pane::TerminalLaunchOptions {
                        command: request.initial_command.or_else(|| request.command.clone()),
                        working_directory: request.working_directory,
                        extra_env: request.startup_environment.into_iter().collect(),
                        activate: request.focus,
                    },
                )
            } else {
                pane::active_surface_summary(&new_pane)
            };
            let Some(surface) = surface else {
                let _ = reply.send(Err(BridgeError::internal(
                    "pane.create did not produce a surface",
                )));
                return;
            };

            let surface_id = surface.surface_id.clone();
            let response =
                pane_create_response_payload(&resolved.workspace_id, &workspace_name, surface);

            if !startup_requested {
                if let Some(command) = request.command {
                    send_pane_create_response_after_command(
                        new_pane, surface_id, command, response, reply,
                    );
                    return;
                }
            }

            let _ = reply.send(Ok(response));
        }
        ControlCommand::CreatePanes { request, reply } => {
            let source_pane_id = request
                .source_pane_id
                .as_deref()
                .and_then(parse_pane_handle);
            if request.source_pane_id.is_some() && source_pane_id.is_none() {
                let _ = reply.send(Err(BridgeError::invalid_params(
                    "pane.create_many requires a valid pane_id",
                )));
                return;
            }

            let first_direction = PaneCreateDirection::from(request.directions[0].clone());
            let resolved = match resolve_pane_create_target(
                state,
                &request.target,
                request.source_surface_id.as_deref(),
                source_pane_id,
                first_direction,
            ) {
                Ok(resolved) => resolved,
                Err(error) => {
                    let _ = reply.send(Err(pane_create_target_error(error)));
                    return;
                }
            };

            let workspace_name = {
                let app_state = state.borrow();
                app_state
                    .workspaces
                    .iter()
                    .find(|workspace| workspace.id == resolved.workspace_id)
                    .map(|workspace| workspace.name.clone())
            };
            let Some(workspace_name) = workspace_name else {
                let _ = reply.send(Err(BridgeError::not_found("workspace not found")));
                return;
            };

            let batch = PendingPaneBatch {
                state: state.clone(),
                workspace_id: resolved.workspace_id,
                workspace_name,
                source_pane_id: resolved.pane_id,
                directions: request.directions,
                count: request.count,
                created: 0,
                panes: Vec::with_capacity(request.count),
                reply: Some(reply),
            };
            schedule_pane_batch_step(Rc::new(RefCell::new(batch)));
        }
        ControlCommand::CreateSurface {
            target,
            command,
            reply,
        } => {
            let resolved = {
                let app_state = state.borrow();
                workspace_index_for_target(&app_state, &target)
            };

            let Some(index) = resolved else {
                let _ = reply.send(Err(BridgeError::not_found("workspace not found")));
                return;
            };

            let (workspace_id, workspace_name, workspace_root) = {
                let app_state = state.borrow();
                let workspace = &app_state.workspaces[index];
                (
                    workspace.id.clone(),
                    workspace.name.clone(),
                    workspace.root.clone(),
                )
            };
            let pane_id = focused_ids_for_workspace(state, &workspace_id)
                .0
                .or_else(|| {
                    pane::pane_summaries_for_root(&workspace_root)
                        .first()
                        .map(|summary| summary.pane_id)
                });
            let Some(pane_id) = pane_id else {
                let _ = reply.send(Err(BridgeError::not_found("pane not found")));
                return;
            };
            let Some(pane_widget) = pane::pane_widget_for_root(&workspace_root, pane_id) else {
                let _ = reply.send(Err(BridgeError::not_found("pane not found")));
                return;
            };
            let Some(surface) =
                pane::add_terminal_tab_to_pane_with_command(&pane_widget, command, false)
            else {
                let _ = reply.send(Err(BridgeError::internal(
                    "surface.create did not produce a terminal surface",
                )));
                return;
            };

            request_session_save(state);
            publish_surface_lifecycle_event(
                "surface.created",
                &workspace_id,
                &surface,
                serde_json::json!({ "origin": "surface.create" }),
            );
            let response = pane_create_response_payload(&workspace_id, &workspace_name, surface);
            let _ = reply.send(Ok(response));
        }
        ControlCommand::CreateSurfaces {
            target,
            pane_id,
            count,
            command_template,
            reply,
        } => {
            let resolved = {
                let app_state = state.borrow();
                workspace_index_for_target(&app_state, &target)
            };

            let Some(index) = resolved else {
                let _ = reply.send(Err(BridgeError::not_found("workspace not found")));
                return;
            };

            let (workspace_id, workspace_name, workspace_root) = {
                let app_state = state.borrow();
                let workspace = &app_state.workspaces[index];
                (
                    workspace.id.clone(),
                    workspace.name.clone(),
                    workspace.root.clone(),
                )
            };
            let requested_pane_id = pane_id
                .as_deref()
                .and_then(parse_pane_handle)
                .or_else(|| pane_id.as_deref().and_then(|raw| raw.parse::<u32>().ok()));
            if pane_id.is_some() && requested_pane_id.is_none() {
                let _ = reply.send(Err(BridgeError::invalid_params(
                    "surface.create_many requires a valid pane_id",
                )));
                return;
            }
            let target_pane_id = requested_pane_id
                .or_else(|| focused_ids_for_workspace(state, &workspace_id).0)
                .or_else(|| {
                    pane::pane_summaries_for_root(&workspace_root)
                        .first()
                        .map(|summary| summary.pane_id)
                });
            let Some(target_pane_id) = target_pane_id else {
                let _ = reply.send(Err(BridgeError::not_found("pane not found")));
                return;
            };
            let Some(pane_widget) = pane::pane_widget_for_root(&workspace_root, target_pane_id)
            else {
                let _ = reply.send(Err(BridgeError::not_found("pane not found")));
                return;
            };

            let mut surfaces = Vec::with_capacity(count);
            for index in 1..=count {
                let command = command_template
                    .as_ref()
                    .map(|template| template.replace("{i}", &index.to_string()));
                let Some(surface) =
                    pane::add_terminal_tab_to_pane_with_command(&pane_widget, command, false)
                else {
                    let _ = reply.send(Err(BridgeError::internal(
                        "surface.create_many did not produce a terminal surface",
                    )));
                    return;
                };
                publish_surface_lifecycle_event(
                    "surface.created",
                    &workspace_id,
                    &surface,
                    serde_json::json!({ "origin": "surface.create_many", "batch_index": index }),
                );
                surfaces.push(pane_create_response_payload(
                    &workspace_id,
                    &workspace_name,
                    surface,
                ));
            }

            request_session_save(state);
            let _ = reply.send(Ok(serde_json::json!({
                "ok": true,
                "count": count,
                "workspace_id": workspace_id,
                "workspace_ref": workspace_ref(&workspace_id),
                "surfaces": surfaces,
            })));
        }
        ControlCommand::ListSurfaces { target, reply } => {
            let resolved = {
                let app_state = state.borrow();
                workspace_index_for_target(&app_state, &target)
            };

            let Some(index) = resolved else {
                let _ = reply.send(Err(crate::control_bridge::BridgeError::not_found(
                    "workspace not found",
                )));
                return;
            };

            let result = {
                let app_state = state.borrow();
                surface_list_payload(state, &app_state.workspaces[index], None)
            };
            let _ = reply.send(Ok(result));
        }
        ControlCommand::FocusSurface {
            target,
            surface_hint,
            reply,
        } => {
            let resolved = {
                let app_state = state.borrow();
                workspace_index_for_target(&app_state, &target)
            };

            let Some(index) = resolved else {
                let _ = reply.send(Err(crate::control_bridge::BridgeError::not_found(
                    "workspace not found",
                )));
                return;
            };

            let result = {
                let app_state = state.borrow();
                let workspace = &app_state.workspaces[index];
                pane::focus_surface_for_root(&workspace.root, &surface_hint).map(|surface| {
                    publish_surface_lifecycle_event(
                        "surface.focused",
                        &workspace.id,
                        &surface,
                        serde_json::json!({ "origin": "surface.focus" }),
                    );
                    pane_create_response_payload(&workspace.id, &workspace.name, surface)
                })
            };

            let Some(result) = result else {
                let _ = reply.send(Err(crate::control_bridge::BridgeError::not_found(
                    "surface not found",
                )));
                return;
            };

            let _ = reply.send(Ok(result));
        }
        ControlCommand::CloseSurface {
            target,
            surface_hint,
            reply,
        } => {
            let resolved = {
                let app_state = state.borrow();
                workspace_index_for_target(&app_state, &target)
            };

            let Some(index) = resolved else {
                let _ = reply.send(Err(crate::control_bridge::BridgeError::not_found(
                    "workspace not found",
                )));
                return;
            };

            let (workspace_id, workspace_name, workspace_root) = {
                let app_state = state.borrow();
                let workspace = &app_state.workspaces[index];
                (
                    workspace.id.clone(),
                    workspace.name.clone(),
                    workspace.root.clone(),
                )
            };
            let resolved_hint =
                surface_hint.or_else(|| focused_ids_for_workspace(state, &workspace_id).1);
            let Some(surface_hint) = resolved_hint else {
                let _ = reply.send(Err(crate::control_bridge::BridgeError::not_found(
                    "surface not found",
                )));
                return;
            };

            let result = pane::close_surface_for_root(&workspace_root, &surface_hint)
                .map(|surface| {
                    publish_surface_lifecycle_event(
                        "surface.closed",
                        &workspace_id,
                        &surface,
                        serde_json::json!({ "origin": "surface.close" }),
                    );
                    let mut payload =
                        pane_create_response_payload(&workspace_id, &workspace_name, surface);
                    payload["closed"] = serde_json::Value::Bool(true);
                    payload
                })
                .map_err(|_| crate::control_bridge::BridgeError::not_found("surface not found"));

            if result.is_ok() {
                request_session_save(state);
            }
            let _ = reply.send(result);
        }
        ControlCommand::MoveSurface {
            target,
            surface_hint,
            target_pane_id,
            index,
            reply,
        } => {
            let resolved = {
                let app_state = state.borrow();
                workspace_index_for_target(&app_state, &target)
            };

            let Some(workspace_index) = resolved else {
                let _ = reply.send(Err(crate::control_bridge::BridgeError::not_found(
                    "workspace not found",
                )));
                return;
            };
            let Some(target_pane_id) =
                parse_pane_handle(&target_pane_id).or_else(|| target_pane_id.parse::<u32>().ok())
            else {
                let _ = reply.send(Err(crate::control_bridge::BridgeError::invalid_params(
                    "surface.move requires a valid target_pane_id",
                )));
                return;
            };

            let result = {
                let app_state = state.borrow();
                let workspace = &app_state.workspaces[workspace_index];
                pane::move_surface_for_root(&workspace.root, &surface_hint, target_pane_id, index)
                    .map(|surface| {
                        publish_surface_lifecycle_event(
                            "surface.moved",
                            &workspace.id,
                            &surface,
                            serde_json::json!({
                                "origin": "surface.move",
                                "target_pane_id": target_pane_id.to_string(),
                                "target_pane_ref": pane_ref(target_pane_id),
                                "requested_index": index,
                            }),
                        );
                        pane_create_response_payload(&workspace.id, &workspace.name, surface)
                    })
            };

            let Some(result) = result else {
                let _ = reply.send(Err(crate::control_bridge::BridgeError::not_found(
                    "surface or target pane not found",
                )));
                return;
            };

            request_session_save(state);
            let _ = reply.send(Ok(result));
        }
        ControlCommand::ReorderSurface {
            target,
            surface_hint,
            index,
            before_surface_hint,
            after_surface_hint,
            reply,
        } => {
            let resolved = {
                let app_state = state.borrow();
                workspace_index_for_target(&app_state, &target)
            };

            let Some(workspace_index) = resolved else {
                let _ = reply.send(Err(crate::control_bridge::BridgeError::not_found(
                    "workspace not found",
                )));
                return;
            };

            let result = {
                let app_state = state.borrow();
                let workspace = &app_state.workspaces[workspace_index];
                pane::reorder_surface_for_root(
                    &workspace.root,
                    &surface_hint,
                    index,
                    before_surface_hint.as_deref(),
                    after_surface_hint.as_deref(),
                )
                .map(|surface| {
                    publish_surface_lifecycle_event(
                        "surface.reordered",
                        &workspace.id,
                        &surface,
                        serde_json::json!({
                            "origin": "surface.reorder",
                            "requested_index": index,
                            "before_surface_id": before_surface_hint,
                            "after_surface_id": after_surface_hint,
                        }),
                    );
                    pane_create_response_payload(&workspace.id, &workspace.name, surface)
                })
            };

            let Some(result) = result else {
                let _ = reply.send(Err(crate::control_bridge::BridgeError::not_found(
                    "surface or reorder target not found",
                )));
                return;
            };

            request_session_save(state);
            let _ = reply.send(Ok(result));
        }
        ControlCommand::DragSurfaceToSplit {
            target,
            surface_hint,
            direction,
            reply,
        } => {
            let resolved = {
                let app_state = state.borrow();
                workspace_index_for_target(&app_state, &target)
            };

            let Some(workspace_index) = resolved else {
                let _ = reply.send(Err(crate::control_bridge::BridgeError::not_found(
                    "workspace not found",
                )));
                return;
            };

            let (workspace_id, workspace_name, source) = {
                let app_state = state.borrow();
                let workspace = &app_state.workspaces[workspace_index];
                let source = pane::surface_source_for_root(&workspace.root, &surface_hint);
                (workspace.id.clone(), workspace.name.clone(), source)
            };
            let Some((source_pane, tab_id)) = source else {
                let _ = reply.send(Err(crate::control_bridge::BridgeError::not_found(
                    "surface not found",
                )));
                return;
            };

            let direction_label = match &direction {
                BridgePaneCreateDirection::Left => "left",
                BridgePaneCreateDirection::Right => "right",
                BridgePaneCreateDirection::Up => "up",
                BridgePaneCreateDirection::Down => "down",
            };
            let placement = pane_create_split_placement(PaneCreateDirection::from(direction));
            let new_pane = split_pane(
                state,
                &workspace_id,
                &source_pane,
                placement.orientation,
                SplitPaneOptions {
                    initial_state: None,
                    skip_default_tab: true,
                    new_pane_first: placement.new_pane_first,
                    initial_ratio: None,
                    persist: false,
                },
            );
            let Some(new_pane) = new_pane else {
                let _ = reply.send(Err(crate::control_bridge::BridgeError::invalid_params(
                    "not enough room to split pane",
                )));
                return;
            };

            if !pane::move_tab_to_pane(&source_pane, &tab_id, &new_pane) {
                let _ = reply.send(Err(crate::control_bridge::BridgeError::not_found(
                    "surface not found",
                )));
                return;
            }
            let Some(surface) = pane::active_surface_summary(&new_pane) else {
                let _ = reply.send(Err(crate::control_bridge::BridgeError::internal(
                    "surface.drag_to_split did not produce a surface",
                )));
                return;
            };

            request_session_save(state);
            publish_surface_lifecycle_event(
                "surface.moved",
                &workspace_id,
                &surface,
                serde_json::json!({
                    "origin": "surface.drag_to_split",
                    "direction": direction_label,
                }),
            );
            let result = pane_create_response_payload(&workspace_id, &workspace_name, surface);
            let _ = reply.send(Ok(result));
        }
        ControlCommand::RefreshSurfaces {
            target,
            surface_hint,
            reply,
        } => {
            let resolved = {
                let app_state = state.borrow();
                workspace_index_for_target(&app_state, &target)
            };

            let Some(workspace_index) = resolved else {
                let _ = reply.send(Err(crate::control_bridge::BridgeError::not_found(
                    "workspace not found",
                )));
                return;
            };

            let (workspace_id, workspace_name, refreshed) = {
                let app_state = state.borrow();
                let workspace = &app_state.workspaces[workspace_index];
                let refreshed = pane::refresh_terminal_surfaces_for_root(
                    &workspace.root,
                    surface_hint.as_deref(),
                );
                (workspace.id.clone(), workspace.name.clone(), refreshed)
            };
            if surface_hint.is_some() && refreshed.is_empty() {
                let _ = reply.send(Err(crate::control_bridge::BridgeError::not_found(
                    "terminal surface not found",
                )));
                return;
            }

            let surfaces = refreshed
                .into_iter()
                .map(|surface| {
                    pane_create_response_payload(&workspace_id, &workspace_name, surface)
                })
                .collect::<Vec<_>>();
            let _ = reply.send(Ok(serde_json::json!({
                "ok": true,
                "refreshed": surfaces.len(),
                "surfaces": surfaces,
            })));
        }
        ControlCommand::ClearSurfaceHistory {
            target,
            surface_hint,
            reply,
        } => {
            let resolved = {
                let app_state = state.borrow();
                workspace_index_for_target(&app_state, &target)
            };

            let Some(index) = resolved else {
                let _ = reply.send(Err(crate::control_bridge::BridgeError::not_found(
                    "workspace not found",
                )));
                return;
            };

            let cleared = {
                let app_state = state.borrow();
                let workspace = &app_state.workspaces[index];
                let (_focused_pane_id, focused_surface_id) =
                    focused_ids_for_workspace(state, &workspace.id);
                let resolved_surface_hint =
                    surface_hint.as_deref().or(focused_surface_id.as_deref());
                let surface =
                    pane::clear_terminal_history_for_root(&workspace.root, resolved_surface_hint);
                surface.map(|surface| {
                    pane_create_response_payload(&workspace.id, &workspace.name, surface)
                })
            };

            let Some(mut payload) = cleared else {
                let _ = reply.send(Err(crate::control_bridge::BridgeError::not_found(
                    "terminal surface not found",
                )));
                return;
            };
            if let Some(map) = payload.as_object_mut() {
                map.insert("cleared".to_string(), serde_json::Value::Bool(true));
            }
            let _ = reply.send(Ok(payload));
        }
        ControlCommand::RespawnSurface {
            target,
            surface_hint,
            command,
            tmux_start_command,
            reply,
        } => {
            let resolved = {
                let app_state = state.borrow();
                workspace_index_for_target(&app_state, &target)
            };

            let Some(index) = resolved else {
                let _ = reply.send(Err(crate::control_bridge::BridgeError::not_found(
                    "workspace not found",
                )));
                return;
            };

            let respawned = {
                let app_state = state.borrow();
                let workspace = &app_state.workspaces[index];
                let (_focused_pane_id, focused_surface_id) =
                    focused_ids_for_workspace(state, &workspace.id);
                let resolved_surface_hint =
                    surface_hint.as_deref().or(focused_surface_id.as_deref());
                pane::respawn_terminal_surface_for_root(
                    &workspace.root,
                    resolved_surface_hint,
                    command.clone(),
                )
                .map(|surface| {
                    pane_create_response_payload(&workspace.id, &workspace.name, surface)
                })
            };

            let Some(mut payload) = respawned else {
                let _ = reply.send(Err(crate::control_bridge::BridgeError::not_found(
                    "terminal surface not found",
                )));
                return;
            };
            if let Some(map) = payload.as_object_mut() {
                map.insert("ok".to_string(), serde_json::Value::Bool(true));
                map.insert("respawned".to_string(), serde_json::Value::Bool(true));
                if let Some(start_command) = tmux_start_command {
                    map.insert(
                        "tmux_start_command".to_string(),
                        serde_json::Value::String(start_command),
                    );
                }
            }
            request_session_save(state);
            let _ = reply.send(Ok(payload));
        }
        ControlCommand::SurfaceHealth {
            target,
            surface_hint,
            reply,
        } => {
            let resolved = {
                let app_state = state.borrow();
                workspace_index_for_target(&app_state, &target)
            };

            let Some(index) = resolved else {
                let _ = reply.send(Err(crate::control_bridge::BridgeError::not_found(
                    "workspace not found",
                )));
                return;
            };

            let result = {
                let app_state = state.borrow();
                surface_health_payload(state, &app_state.workspaces[index], surface_hint.as_deref())
            };
            let _ = reply.send(result);
        }
        ControlCommand::CreateWorkspace {
            name,
            description,
            cwd,
            command,
            focus,
            layout,
            group_id,
            group_placement,
            group_reference_workspace_id,
            environment,
            reply,
        } => {
            if let Err(error) = validate_workspace_create_group_request(
                state,
                group_id.as_deref(),
                group_placement.as_deref(),
                group_reference_workspace_id.as_deref(),
            ) {
                let _ = reply.send(Err(error));
                return;
            }
            let working_directory = workspace_creation_directory_from_state(state, cwd.as_deref());
            let title = name
                .unwrap_or_else(|| workspace_title_from_directory(working_directory.as_deref()));
            let folder_path = working_directory.as_deref();

            let has_layout = layout.is_some();
            let workspace = WorkspaceState {
                id: None,
                name: title,
                description,
                favorite: false,
                cwd: working_directory.clone(),
                folder_path: working_directory.clone(),
                group_id: group_id
                    .as_deref()
                    .map(normalize_workspace_group_handle)
                    .map(ToOwned::to_owned),
                environment,
                layout: layout
                    .unwrap_or_else(|| LayoutNodeState::Pane(PaneState::fallback(folder_path))),
            };
            add_workspace_from_state_internal(state, &workspace, focus);

            let created_workspace_id = {
                let app_state = state.borrow();
                app_state
                    .workspaces
                    .last()
                    .map(|workspace| workspace.id.clone())
            };
            let Some(created_workspace_id) = created_workspace_id else {
                let _ = reply.send(Err(crate::control_bridge::BridgeError::internal(
                    "workspace.create did not produce a workspace",
                )));
                return;
            };
            if let Some(group_id) = group_id.as_deref() {
                if let Err(error) = place_created_workspace_in_group(
                    state,
                    &created_workspace_id,
                    group_id,
                    group_placement.as_deref(),
                    group_reference_workspace_id.as_deref(),
                ) {
                    let _ = reply.send(Err(error));
                    return;
                }
            }
            request_session_save(state);

            let result = {
                let app_state = state.borrow();
                app_state
                    .workspaces
                    .iter()
                    .position(|workspace| workspace.id == created_workspace_id)
                    .and_then(|index| workspace_payload(&app_state, index))
            };

            if let (false, Some(command), Some(workspace_id)) = (
                has_layout,
                command,
                result
                    .as_ref()
                    .and_then(|payload| payload["workspace_id"].as_str())
                    .map(ToOwned::to_owned),
            ) {
                let state = state.clone();
                glib::timeout_add_local_once(std::time::Duration::from_millis(500), move || {
                    let target = {
                        let app_state = state.borrow();
                        app_state
                            .workspaces
                            .iter()
                            .find(|workspace| workspace.id == workspace_id)
                            .and_then(|workspace| {
                                pane::terminal_handle_for_surface(&workspace.root, None)
                            })
                    };
                    if let Some((_surface_id, handle)) = target {
                        handle.send_text(&command);
                        handle.send_text("\n");
                    }
                });
            }

            let _ = reply.send(result.ok_or_else(|| {
                crate::control_bridge::BridgeError::internal(
                    "workspace.create did not produce a workspace",
                )
            }));
        }
        ControlCommand::CreateWorkspaces {
            count,
            name_prefix,
            cwd,
            panes_per_workspace,
            terminals_per_workspace,
            reply,
        } => {
            let working_directory = workspace_creation_directory_from_state(state, cwd.as_deref());
            let folder_path = working_directory.as_deref();

            let mut created = Vec::with_capacity(count);
            for index in 1..=count {
                let workspace = WorkspaceState {
                    id: None,
                    name: format!("{name_prefix}-{index}"),
                    description: None,
                    favorite: false,
                    cwd: working_directory.clone(),
                    folder_path: working_directory.clone(),
                    group_id: None,
                    environment: BTreeMap::new(),
                    layout: mixed_workspace_layout(
                        panes_per_workspace,
                        terminals_per_workspace,
                        folder_path,
                    ),
                };
                add_workspace_from_state_internal(state, &workspace, false);
                let payload = {
                    let app_state = state.borrow();
                    workspace_payload(&app_state, app_state.workspaces.len() - 1)
                };
                let Some(payload) = payload else {
                    let _ = reply.send(Err(BridgeError::internal(
                        "workspace.create_many did not produce a workspace",
                    )));
                    return;
                };
                created.push(payload);
            }

            let activation = {
                let mut app_state = state.borrow_mut();
                app_state.workspaces.len().checked_sub(1).map(|last_index| {
                    app_state.active_idx = last_index;
                    sync_right_sidebar_panel(&mut app_state);
                    let workspace = &app_state.workspaces[last_index];
                    (
                        app_state.stack.clone(),
                        app_state.sidebar_list.clone(),
                        format!("ws-{}", workspace.id),
                        workspace.sidebar_row.clone(),
                    )
                })
            };
            if let Some((stack, sidebar_list, stack_name, row)) = activation {
                stack.set_visible_child_name(&stack_name);
                sidebar_list.select_row(Some(&row));
            }
            request_session_save(state);
            let _ = reply.send(Ok(serde_json::json!({
                "ok": true,
                "count": count,
                "panes_per_workspace": panes_per_workspace,
                "terminals_per_workspace": terminals_per_workspace,
                "workspaces": created,
            })));
        }
        ControlCommand::SelectWorkspace { target, reply } => {
            let resolved = {
                let app_state = state.borrow();
                workspace_index_for_target(&app_state, &target)
            };

            let Some(index) = resolved else {
                let _ = reply.send(Err(crate::control_bridge::BridgeError::not_found(
                    "workspace not found",
                )));
                return;
            };

            let _ = reply.send(select_workspace_for_control(state, index));
        }
        ControlCommand::NavigateWorkspace { action, reply } => {
            let resolved = {
                let app_state = state.borrow();
                match action {
                    WorkspaceNavigation::Next if !app_state.workspaces.is_empty() => {
                        Some((app_state.active_idx + 1) % app_state.workspaces.len())
                    }
                    WorkspaceNavigation::Previous if !app_state.workspaces.is_empty() => Some(
                        app_state
                            .active_idx
                            .checked_sub(1)
                            .unwrap_or_else(|| app_state.workspaces.len() - 1),
                    ),
                    WorkspaceNavigation::Last => app_state
                        .previous_workspace_id
                        .as_deref()
                        .and_then(|previous_id| {
                            app_state
                                .workspaces
                                .iter()
                                .position(|workspace| workspace.id == previous_id)
                        }),
                    _ => None,
                }
            };

            let Some(index) = resolved else {
                let _ = reply.send(Err(crate::control_bridge::BridgeError::not_found(
                    "workspace not found",
                )));
                return;
            };

            let _ = reply.send(select_workspace_for_control(state, index));
        }
        ControlCommand::RenameWorkspace {
            target,
            title,
            reply,
        } => {
            let resolved = {
                let app_state = state.borrow();
                workspace_index_for_target(&app_state, &target)
            };

            let Some(index) = resolved else {
                let _ = reply.send(Err(crate::control_bridge::BridgeError::not_found(
                    "workspace not found",
                )));
                return;
            };

            let snapshot = {
                let mut app_state = state.borrow_mut();
                let workspace = &mut app_state.workspaces[index];
                workspace.name = title.clone();
                workspace.name_label.set_label(&title);
                workspace_event_snapshot(&app_state, index)
            };
            if let Some(snapshot) = snapshot {
                publish_workspace_lifecycle_event(
                    "workspace.renamed",
                    &snapshot,
                    None,
                    serde_json::json!({ "origin": "socket" }),
                );
            }
            request_session_save(state);

            let result = {
                let app_state = state.borrow();
                workspace_payload(&app_state, index)
            };
            let _ = reply.send(result.ok_or_else(|| {
                crate::control_bridge::BridgeError::not_found("workspace not found")
            }));
        }
        ControlCommand::CloseWorkspace { target, reply } => {
            let resolved = {
                let app_state = state.borrow();
                if app_state.workspaces.len() <= 1 {
                    None
                } else {
                    workspace_index_for_target(&app_state, &target)
                }
            };

            let Some(index) = resolved else {
                let can_close = state.borrow().workspaces.len() > 1;
                let error = if can_close {
                    crate::control_bridge::BridgeError::not_found("workspace not found")
                } else {
                    crate::control_bridge::BridgeError::conflict("cannot close workspace")
                };
                let _ = reply.send(Err(error));
                return;
            };

            let closed_workspace = {
                let app_state = state.borrow();
                workspace_payload(&app_state, index)
            };
            let workspace_id = state.borrow().workspaces[index].id.clone();
            close_workspace_by_id(state, &workspace_id);

            let _ = reply.send(closed_workspace.ok_or_else(|| {
                crate::control_bridge::BridgeError::not_found("workspace not found")
            }));
        }
        ControlCommand::SendText {
            target,
            surface_hint,
            text,
            reply,
        } => {
            let resolved = {
                let app_state = state.borrow();
                workspace_index_for_target(&app_state, &target)
            };

            let Some(index) = resolved else {
                let _ = reply.send(Err(crate::control_bridge::BridgeError::not_found(
                    "workspace not found",
                )));
                return;
            };

            let target = {
                let app_state = state.borrow();
                let workspace = &app_state.workspaces[index];
                let (_focused_pane_id, focused_surface_id) =
                    focused_ids_for_workspace(state, &workspace.id);
                let resolved_surface_hint =
                    surface_hint.as_deref().or(focused_surface_id.as_deref());
                pane::terminal_handle_for_root(&workspace.root, resolved_surface_hint).map(
                    |(surface_id, handle)| {
                        let workspace_id = workspace.id.clone();
                        (
                            serde_json::json!({
                                "workspace_id": workspace.id.as_str(),
                                "workspace_ref": workspace_ref(&workspace.id),
                                "surface_id": surface_id.as_str(),
                                "surface_ref": surface_ref(&surface_id),
                            }),
                            workspace_id,
                            surface_id,
                            handle,
                        )
                    },
                )
            };

            let Some((mut payload, workspace_id, surface_id, handle)) = target else {
                let _ = reply.send(Err(crate::control_bridge::BridgeError::not_found(
                    "terminal surface not found",
                )));
                return;
            };

            handle.send_text(&text);
            publish_surface_input_sent_event(&workspace_id, &surface_id, text.len());
            if let Some(map) = payload.as_object_mut() {
                map.insert("ok".to_string(), serde_json::Value::Bool(true));
            }
            let _ = reply.send(Ok(payload));
        }
        ControlCommand::ReadSurfaceText {
            target,
            surface_hint,
            lines,
            scrollback,
            reply,
        } => {
            let resolved = {
                let app_state = state.borrow();
                workspace_index_for_target(&app_state, &target)
            };

            let Some(index) = resolved else {
                let _ = reply.send(Err(crate::control_bridge::BridgeError::not_found(
                    "workspace not found",
                )));
                return;
            };

            let target = {
                let app_state = state.borrow();
                let workspace = &app_state.workspaces[index];
                pane::terminal_handle_for_root(&workspace.root, surface_hint.as_deref()).map(
                    |(surface_id, handle)| {
                        (
                            serde_json::json!({
                                "workspace_id": workspace.id.as_str(),
                                "workspace_ref": workspace_ref(&workspace.id),
                                "surface_id": surface_id.as_str(),
                                "surface_ref": surface_ref(&surface_id),
                                "lines": lines,
                                "scrollback_requested": scrollback,
                                "scrollback_included": false,
                                "source": "viewport",
                            }),
                            handle,
                        )
                    },
                )
            };

            let Some((mut payload, handle)) = target else {
                let _ = reply.send(Err(crate::control_bridge::BridgeError::not_found(
                    "terminal surface not found",
                )));
                return;
            };

            let Some(text) = handle.read_viewport_text() else {
                let _ = reply.send(Err(crate::control_bridge::BridgeError::internal(
                    "surface.read_text failed",
                )));
                return;
            };
            if let Some(map) = payload.as_object_mut() {
                map.insert(
                    "text".to_string(),
                    serde_json::Value::String(limit_text_to_last_lines(text, lines)),
                );
            }
            let _ = reply.send(Ok(payload));
        }
        ControlCommand::SendKey {
            target,
            surface_hint,
            key,
            reply,
        } => {
            let resolved = {
                let app_state = state.borrow();
                workspace_index_for_target(&app_state, &target)
            };

            let Some(index) = resolved else {
                let _ = reply.send(Err(crate::control_bridge::BridgeError::not_found(
                    "workspace not found",
                )));
                return;
            };

            let target = {
                let app_state = state.borrow();
                let workspace = &app_state.workspaces[index];
                pane::terminal_handle_for_root(&workspace.root, surface_hint.as_deref()).map(
                    |(surface_id, handle)| {
                        let workspace_id = workspace.id.clone();
                        (
                            serde_json::json!({
                                "workspace_id": workspace.id.as_str(),
                                "workspace_ref": workspace_ref(&workspace.id),
                                "surface_id": surface_id.as_str(),
                                "surface_ref": surface_ref(&surface_id),
                            }),
                            workspace_id,
                            surface_id,
                            handle,
                        )
                    },
                )
            };

            let Some((mut payload, workspace_id, surface_id, handle)) = target else {
                let _ = reply.send(Err(crate::control_bridge::BridgeError::not_found(
                    "terminal surface not found",
                )));
                return;
            };

            if !handle.send_key(&key) {
                let _ = reply.send(Err(crate::control_bridge::BridgeError::invalid_params(
                    "unsupported key",
                )));
                return;
            }
            publish_surface_key_sent_event(&workspace_id, &surface_id, &key);
            if let Some(map) = payload.as_object_mut() {
                map.insert("ok".to_string(), serde_json::Value::Bool(true));
            }
            let _ = reply.send(Ok(payload));
        }
        ControlCommand::CreateNotification {
            target,
            surface_hint,
            title,
            subtitle,
            body,
            agent_category,
            agent_pending,
            feed_actions,
            reply,
        } => {
            // Resolve the workspace target. `WorkspaceTarget::Active` maps to
            // the currently-focused workspace via workspace_index_for_target.
            let resolved = {
                let app_state = state.borrow();
                workspace_index_for_target(&app_state, &target)
            };

            let Some(index) = resolved else {
                let _ = reply.send(Err(crate::control_bridge::BridgeError::not_found(
                    "workspace not found",
                )));
                return;
            };

            let (ws_id, cwd, workspace_name, app_focused, workspace_is_active, notification_config) = {
                let app_state = state.borrow();
                let workspace = &app_state.workspaces[index];
                let notification_config = app_state.config.borrow().notifications.clone();
                (
                    workspace.id.clone(),
                    workspace
                        .folder_path
                        .clone()
                        .or_else(|| workspace.cwd.borrow().clone()),
                    workspace.name.clone(),
                    app_state.window.is_active(),
                    index == app_state.active_idx,
                    notification_config,
                )
            };
            let surface = {
                let app_state = state.borrow();
                notification_surface_metadata(&app_state.workspaces[index], surface_hint.as_deref())
            };
            let surface = match surface {
                Ok(surface) => surface,
                Err(error) => {
                    let _ = reply.send(Err(error));
                    return;
                }
            };

            // Build the sidebar message: title becomes the bold prefix,
            // subtitle + body are joined with " — " for the body text.
            let combined_body = match (subtitle.is_empty(), body.is_empty()) {
                (true, true) => String::new(),
                (true, false) => body.clone(),
                (false, true) => subtitle.clone(),
                (false, false) => format!("{subtitle} — {body}"),
            };
            let message = workspace_notification_message(&title, &combined_body);
            let parsed_agent_category = agent_category
                .as_deref()
                .and_then(app_config::AgentNotifyCategory::from_str);
            if agent_category.is_some() && parsed_agent_category.is_none() {
                let _ = reply.send(Err(crate::control_bridge::BridgeError::invalid_params(
                    "invalid agent notification category",
                )));
                return;
            }
            if !app_config::agent_notification_should_deliver(
                parsed_agent_category,
                agent_pending,
                &notification_config,
            ) {
                let category = parsed_agent_category.map(agent_notify_category_str);
                let _ = reply.send(Ok(serde_json::json!({
                    "notification_id": serde_json::Value::Null,
                    "delivered": false,
                    "agent_category": category,
                    "agent_pending": agent_pending,
                })));
                return;
            }
            let desktop_target = DesktopNotificationTarget {
                workspace_id: ws_id.clone(),
                pane_id: surface.as_ref().map(|surface| surface.pane_id),
                tab_id: surface
                    .as_ref()
                    .and_then(|surface| surface.surface_id.rsplit_once(':'))
                    .map(|(_, tab_id)| tab_id.to_string()),
            };
            let effects = match notification_policy_effects(
                &notification_config,
                &NotificationPolicyContext {
                    workspace_id: ws_id.clone(),
                    surface_id: surface.as_ref().map(|surface| surface.surface_id.clone()),
                    cwd,
                    title: title.clone(),
                    subtitle: subtitle.clone(),
                    body: body.clone(),
                    app_focused,
                    focused_panel: workspace_is_active,
                },
            ) {
                Ok(effects) => effects,
                Err(error) => {
                    let _ = reply.send(Err(error));
                    return;
                }
            };
            if effects.mark_unread {
                if let Some(mut request) = mark_workspace_unread_with_message(
                    state,
                    &ws_id,
                    &message,
                    false,
                    desktop_target.clone(),
                    feed_actions.clone(),
                ) {
                    if !effects.sound {
                        request.sound = app_config::NotificationSound::None;
                    }
                    if effects.desktop {
                        show_desktop_notification(state, request);
                    }
                }
            } else if effects.desktop
                && should_emit_desktop_notification(
                    notification_config.enabled,
                    app_focused,
                    workspace_is_active,
                    false,
                    notification_config.suppress_only_focused_surface,
                )
            {
                show_desktop_notification(
                    state,
                    DesktopNotificationRequest {
                        summary: workspace_name,
                        body: message.clone(),
                        sound: if effects.sound {
                            notification_config.sound
                        } else {
                            app_config::NotificationSound::None
                        },
                        custom_sound_file_path: notification_config.custom_sound_file_path.clone(),
                        target: desktop_target,
                        feed_actions: feed_actions.clone(),
                    },
                );
            }
            if effects.command {
                if let Err(error) = spawn_notification_command(
                    &notification_config.command,
                    &title,
                    &subtitle,
                    &body,
                ) {
                    let _ = reply.send(Err(error));
                    return;
                }
            }

            let notification = effects.record.then(|| {
                push_host_notification(
                    state,
                    ws_id.clone(),
                    surface,
                    title.clone(),
                    subtitle.clone(),
                    body.clone(),
                    message.clone(),
                )
            });
            let payload = serde_json::json!({
                "ok": true,
                "workspace_id": ws_id,
                "workspace_ref": workspace_ref(&ws_id),
                "title": title,
                "subtitle": subtitle,
                "body": body,
                "notification_id": notification.as_ref().map(|notification| notification.id),
                "notification": notification.as_ref().map(host_notification_row),
                "effects": {
                    "record": effects.record,
                    "markUnread": effects.mark_unread,
                    "desktop": effects.desktop,
                    "sound": effects.sound,
                    "command": effects.command,
                },
            });
            let _ = reply.send(Ok(payload));
        }
        ControlCommand::ListNotifications { unread_only, reply } => {
            let _ = reply.send(Ok(list_host_notifications(state, unread_only)));
        }
        ControlCommand::DismissNotification {
            notification_id,
            all_read,
            reply,
        } => {
            let _ = reply.send(dismiss_host_notifications(state, notification_id, all_read));
        }
        ControlCommand::MarkNotificationRead {
            notification_id,
            target,
            all,
            reply,
        } => {
            let result = match (notification_id, target, all) {
                (Some(id), None, false) => mark_host_notification_read(state, id),
                (None, Some(target), false) => mark_workspace_notifications_read(state, &target),
                (None, None, true) => Ok(mark_all_host_notifications_read(state)),
                _ => Err(crate::control_bridge::BridgeError::invalid_params(
                    "notification.mark_read requires exactly one selector",
                )),
            };
            let _ = reply.send(result);
        }
        ControlCommand::OpenNotification {
            notification_id,
            reply,
        } => {
            let _ = reply.send(open_host_notification(state, notification_id));
        }
        ControlCommand::JumpToUnreadNotification { reply } => {
            let _ = reply.send(jump_to_unread_notification(state));
        }
        ControlCommand::ClearNotifications {
            notification_id,
            reply,
        } => {
            let _ = reply.send(clear_host_notifications(state, notification_id));
        }
        ControlCommand::RightSidebar {
            action,
            target,
            reply,
        } => {
            let _ = reply.send(apply_right_sidebar_action(state, action, target));
        }
        ControlCommand::Sidebar {
            action,
            target,
            reply,
        } => {
            let _ = reply.send(apply_sidebar_action(state, action, target));
        }
    }
}

fn add_workspace_from_state(state: &State, workspace: &WorkspaceState) {
    add_workspace_from_state_internal(state, workspace, true);
}

fn add_workspace_from_state_internal(state: &State, workspace: &WorkspaceState, activate: bool) {
    let shortcuts = {
        let s = state.borrow();
        s.shortcuts.clone()
    };
    let (stack, sidebar_list) = {
        let s = state.borrow();
        (s.stack.clone(), s.sidebar_list.clone())
    };
    let id = workspace
        .id
        .as_deref()
        .filter(|id| uuid::Uuid::parse_str(id).is_ok())
        .map(ToOwned::to_owned)
        .unwrap_or_else(|| uuid::Uuid::new_v4().to_string());
    let stack_name = format!("ws-{id}");
    let working_dir = workspace
        .folder_path
        .as_deref()
        .or(workspace.cwd.as_deref());
    let (root, split_container) =
        build_workspace_root(state, &shortcuts, &id, working_dir, &workspace.layout);
    stack.add_named(&root, Some(&stack_name));

    let sidebar_config = {
        let app_state = state.borrow();
        let sidebar = app_state.config.borrow().sidebar.clone();
        sidebar
    };
    let (row, name_label, favorite_button, notify_dot, notify_label, path_label, description_label) =
        build_sidebar_row(
            &workspace.name,
            workspace.description.as_deref(),
            workspace.folder_path.as_deref(),
            &sidebar_config,
        );
    sidebar_list.append(&row);
    install_workspace_row_interactions(state, &id, &row, &favorite_button);

    let cwd: Rc<RefCell<Option<String>>> = Rc::new(RefCell::new(workspace.cwd.clone()));
    let ws = Workspace {
        id,
        name: workspace.name.clone(),
        description: workspace.description.clone(),
        root,
        split_container,
        sidebar_row: row.clone(),
        name_label,
        favorite_button,
        notify_dot,
        notify_label,
        description_label,
        unread: false,
        favorite: workspace.favorite,
        last_pane_id: None,
        group_id: workspace.group_id.clone(),
        environment: workspace.environment.clone(),
        cwd,
        folder_path: workspace.folder_path.clone(),
        path_label,
        sidebar_status: BTreeMap::new(),
        sidebar_progress: None,
        sidebar_log: Vec::new(),
    };

    if workspace.favorite {
        set_workspace_favorite_visual(&ws);
    }

    let (snapshot, selected_snapshot, previous_workspace_id) = {
        let mut s = state.borrow_mut();
        let was_empty = s.workspaces.is_empty();
        let previous_workspace_id = s.active_workspace().map(|workspace| workspace.id.clone());
        s.workspaces.push(ws);
        if activate || was_empty {
            s.active_idx = s.workspaces.len() - 1;
            sync_right_sidebar_panel(&mut s);
        }
        let index = s.workspaces.len() - 1;
        let snapshot = workspace_event_snapshot(&s, index);
        let selected_snapshot = (activate || was_empty)
            .then(|| workspace_event_snapshot(&s, s.active_idx))
            .flatten();
        (snapshot, selected_snapshot, previous_workspace_id)
    };
    if let Some(snapshot) = snapshot {
        publish_workspace_lifecycle_event(
            "workspace.created",
            &snapshot,
            None,
            serde_json::json!({ "origin": "model" }),
        );
    }
    if let Some(snapshot) = selected_snapshot {
        publish_workspace_lifecycle_event(
            "workspace.selected",
            &snapshot,
            previous_workspace_id.as_deref(),
            serde_json::json!({ "origin": "model" }),
        );
    }

    if activate {
        stack.set_visible_child_name(&stack_name);
        sidebar_list.select_row(Some(&row));
    }
}

/// Create a PaneWidget wired up with callbacks for a specific workspace.
pub(crate) fn create_pane_for_workspace(
    state: &State,
    shortcuts: &Rc<ResolvedShortcutConfig>,
    ws_id: &str,
    working_directory: Option<&str>,
    initial_state: Option<&PaneState>,
    skip_default_tab: bool,
) -> gtk::Box {
    let state_for_split = state.clone();
    let state_for_close = state.clone();
    let state_for_bell = state.clone();
    let state_for_desktop_notification = state.clone();
    let state_for_keybinds = state.clone();
    let state_for_pwd = state.clone();
    let state_for_empty = state.clone();
    let ws_id_split = ws_id.to_string();
    let ws_id_close = ws_id.to_string();
    let ws_id_bell = ws_id.to_string();
    let ws_id_desktop_notification = ws_id.to_string();
    let ws_id_pwd = ws_id.to_string();
    let ws_id_empty = ws_id.to_string();
    let state_for_split_with_tab = state.clone();
    let state_for_config = state.clone();
    let on_config_changed = settings_dialog_config_changed_handler(state);
    let state_for_workspace_env = state.clone();
    let ws_id_split_with_tab = ws_id.to_string();
    let ws_id_for_env = ws_id.to_string();
    let ws_id_for_workspace_env = ws_id.to_string();

    let callbacks = Rc::new(PaneCallbacks {
        on_split: Box::new(move |pane_widget, orientation| {
            split_pane(
                &state_for_split,
                &ws_id_split,
                pane_widget,
                orientation,
                SplitPaneOptions {
                    initial_state: None,
                    skip_default_tab: false,
                    new_pane_first: false,
                    initial_ratio: None,
                    persist: true,
                },
            );
        }),
        on_close_pane: Box::new(move |pane_widget| {
            remove_pane_internal(&state_for_close, &ws_id_close, pane_widget, true);
        }),
        on_bell: Box::new(move |source_focused: bool, pane_id: u32, tab_id: &str| {
            // Defer to avoid RefCell borrow conflicts — bell can fire during state mutation
            let state = state_for_bell.clone();
            let ws_id = ws_id_bell.clone();
            let tab_id = tab_id.to_string();
            let target = DesktopNotificationTarget {
                workspace_id: ws_id.clone(),
                pane_id: Some(pane_id),
                tab_id: Some(tab_id),
            };
            glib::idle_add_local_once(move || {
                if let Some(request) = mark_workspace_unread(&state, &ws_id, source_focused, target)
                {
                    show_desktop_notification(&state, request);
                }
            });
        }),
        on_desktop_notification: Box::new(
            move |title: &str, body: &str, source_focused: bool, pane_id: u32, tab_id: &str| {
                let state = state_for_desktop_notification.clone();
                let ws_id = ws_id_desktop_notification.clone();
                let tab_id = tab_id.to_string();
                let target = DesktopNotificationTarget {
                    workspace_id: ws_id.clone(),
                    pane_id: Some(pane_id),
                    tab_id: Some(tab_id),
                };
                let message = workspace_notification_message(title, body);
                glib::idle_add_local_once(move || {
                    if let Some(request) = mark_workspace_unread_with_message(
                        &state,
                        &ws_id,
                        &message,
                        source_focused,
                        target,
                        Vec::new(),
                    ) {
                        show_desktop_notification(&state, request);
                    }
                });
            },
        ),
        on_open_browser_here: Box::new(move |pane_widget| {
            pane::add_browser_tab_to_pane(pane_widget);
        }),
        on_open_keybinds: Box::new(move |anchor| {
            open_keybind_editor_tab(&state_for_keybinds, anchor);
        }),
        current_shortcuts: Box::new({
            let state = state.clone();
            move || {
                let s = state.borrow();
                s.shortcuts.clone()
            }
        }),
        on_capture_shortcut: {
            let state = state.clone();
            Rc::new(move |id, binding| persist_shortcut_binding(&state, id, binding))
        },
        on_pwd_changed: Box::new(move |pwd: &str| {
            let state = state_for_pwd.clone();
            let ws_id = ws_id_pwd.clone();
            let pwd = pwd.to_string();
            glib::idle_add_local_once(move || {
                let s = state.borrow();
                if let Some(ws) = s.workspaces.iter().find(|w| w.id == ws_id) {
                    let mut cwd = ws.cwd.borrow_mut();
                    if cwd.as_deref() != Some(pwd.as_str()) {
                        *cwd = Some(pwd);
                    }
                }
            });
        }),
        on_empty: Box::new(move |pane_widget, reason| {
            let persist = matches!(reason, pane::PaneEmptyReason::ClosedLastTab);
            let should_keep_workspace_open = {
                let s = state_for_empty.borrow();
                let config = s.config.borrow();
                let remaining_surfaces = s
                    .workspaces
                    .iter()
                    .find(|workspace| workspace.id == ws_id_empty)
                    .map(|workspace| pane::surface_summaries_for_root(&workspace.root).len())
                    .unwrap_or(0);
                should_keep_workspace_open_after_empty_pane(&config, reason, remaining_surfaces)
            };
            if should_keep_workspace_open {
                pane::add_terminal_tab_to_pane(pane_widget);
                if persist {
                    request_session_save(&state_for_empty);
                }
                return;
            }
            remove_pane_internal(&state_for_empty, &ws_id_empty, pane_widget, persist);
        }),
        on_state_changed: Box::new({
            let state = state.clone();
            move || request_session_save(&state)
        }),
        on_split_with_tab: Box::new(
            move |source_pane, target_pane, orientation, tab_id, new_pane_first| {
                handle_split_with_tab(
                    &state_for_split_with_tab,
                    &ws_id_split_with_tab,
                    source_pane,
                    target_pane,
                    orientation,
                    &tab_id,
                    new_pane_first,
                );
            },
        ),
        current_config: Box::new(move || {
            let s = state_for_config.borrow();
            s.config.clone()
        }),
        on_config_changed,
        workspace_for_pane: Box::new(move |_pane_widget| Some(ws_id_for_env.clone())),
        workspace_environment_for_pane: Box::new(move |_pane_widget| {
            state_for_workspace_env
                .borrow()
                .workspaces
                .iter()
                .find(|workspace| workspace.id == ws_id_for_workspace_env)
                .map(|workspace| {
                    workspace
                        .environment
                        .iter()
                        .map(|(key, value)| (key.clone(), value.clone()))
                        .collect()
                })
                .unwrap_or_default()
        }),
    });

    pane::create_pane(
        callbacks,
        shortcuts.clone(),
        working_directory,
        initial_state,
        skip_default_tab,
    )
}

fn close_workspace(state: &State) {
    let id = {
        let s = state.borrow();
        s.active_workspace().map(|w| w.id.clone())
    };
    if let Some(id) = id {
        close_workspace_by_id(state, &id);
    }
}

fn close_workspace_by_id(state: &State, id: &str) {
    close_workspace_by_id_internal(state, id, true, None);
}

fn close_workspace_by_id_internal(
    state: &State,
    id: &str,
    persist: bool,
    preferred_active_workspace_id: Option<&str>,
) {
    let mut s = state.borrow_mut();
    let Some(idx) = s.workspaces.iter().position(|w| w.id == id) else {
        return;
    };
    let desired_active_workspace_id = preferred_active_workspace_id
        .map(ToOwned::to_owned)
        .or_else(|| s.active_workspace().map(|workspace| workspace.id.clone()));
    let previous_workspace_id = s.active_workspace().map(|workspace| workspace.id.clone());
    let closed_was_active = previous_workspace_id.as_deref() == Some(id);
    let closed_snapshot = workspace_event_snapshot(&s, idx);

    let ws = s.workspaces.remove(idx);
    s.stack.remove(&ws.root);
    s.sidebar_list.remove(&ws.sidebar_row);

    if s.workspaces.is_empty() {
        s.active_idx = 0;
        sync_right_sidebar_panel(&mut s);
        drop(s);
        if let Some(snapshot) = closed_snapshot {
            publish_workspace_lifecycle_event(
                "workspace.closed",
                &snapshot,
                previous_workspace_id.as_deref(),
                serde_json::json!({ "origin": "model" }),
            );
        }
        if persist {
            request_session_save(state);
        }
        return;
    }

    let remaining_workspace_ids: Vec<&str> = s
        .workspaces
        .iter()
        .map(|workspace| workspace.id.as_str())
        .collect();
    let new_idx = next_active_workspace_index(
        &remaining_workspace_ids,
        desired_active_workspace_id.as_deref(),
        idx,
    );
    s.active_idx = new_idx;
    sync_right_sidebar_panel(&mut s);

    let stack_name = format!("ws-{}", s.workspaces[new_idx].id);
    s.stack.set_visible_child_name(&stack_name);

    let row = s.workspaces[new_idx].sidebar_row.clone();
    let sidebar_list = s.sidebar_list.clone();
    let selected_snapshot = closed_was_active
        .then(|| workspace_event_snapshot(&s, new_idx))
        .flatten();
    drop(s);

    if let Some(snapshot) = closed_snapshot {
        publish_workspace_lifecycle_event(
            "workspace.closed",
            &snapshot,
            previous_workspace_id.as_deref(),
            serde_json::json!({ "origin": "model" }),
        );
    }
    if let Some(snapshot) = selected_snapshot {
        publish_workspace_lifecycle_event(
            "workspace.selected",
            &snapshot,
            previous_workspace_id.as_deref(),
            serde_json::json!({ "origin": "model" }),
        );
    }
    sidebar_list.select_row(Some(&row));
    if persist {
        request_session_save(state);
    }
}

fn switch_workspace(state: &State, idx: usize) {
    let (stack, stack_name, unread_handles, focus_root, snapshot, previous_workspace_id) = {
        let mut s = state.borrow_mut();
        if idx >= s.workspaces.len() || idx == s.active_idx {
            return;
        }
        let previous_workspace_id = s
            .workspaces
            .get(s.active_idx)
            .map(|workspace| workspace.id.clone());
        s.previous_workspace_id = previous_workspace_id.clone();
        s.active_idx = idx;
        sync_right_sidebar_panel(&mut s);
        let stack = s.stack.clone();
        let stack_name = format!("ws-{}", s.workspaces[idx].id);
        let focus_root = s.workspaces[idx].root.clone();

        let unread_handles = if s.workspaces[idx].unread {
            let workspace_id = s.workspaces[idx].id.clone();
            for notification in &mut s.notifications {
                if notification.workspace_id == workspace_id {
                    notification.unread = false;
                }
            }
            let ws = &mut s.workspaces[idx];
            ws.unread = false;
            Some((
                ws.notify_dot.clone(),
                ws.notify_label.clone(),
                ws.sidebar_row.clone(),
            ))
        } else {
            None
        };

        let snapshot = workspace_event_snapshot(&s, idx);
        (
            stack,
            stack_name,
            unread_handles,
            focus_root,
            snapshot,
            previous_workspace_id,
        )
    };

    stack.set_visible_child_name(&stack_name);
    glib::idle_add_local_once(move || {
        focus_workspace_entrypoint(&focus_root);
    });

    if let Some((notify_dot, notify_label, sidebar_row)) = unread_handles {
        notify_dot.remove_css_class("limux-notify-dot");
        notify_dot.add_css_class("limux-notify-dot-hidden");
        notify_label.remove_css_class("limux-notify-msg-unread");
        notify_label.add_css_class("limux-notify-msg");
        notify_label.set_visible(false);
        if let Some(row_box) = sidebar_row.child() {
            row_box.remove_css_class("limux-sidebar-row-unread");
        }
    }

    if let Some(snapshot) = snapshot {
        publish_workspace_lifecycle_event(
            "workspace.selected",
            &snapshot,
            previous_workspace_id.as_deref(),
            serde_json::json!({ "origin": "model" }),
        );
    }
    request_session_save(state);
}

fn cycle_workspace(state: &State, direction: i32) {
    let (new_idx, row, sidebar_list) = {
        let s = state.borrow();
        let len = s.workspaces.len();
        if len <= 1 {
            return;
        }
        let new_idx = ((s.active_idx as i32 + direction).rem_euclid(len as i32)) as usize;
        (
            new_idx,
            s.workspaces[new_idx].sidebar_row.clone(),
            s.sidebar_list.clone(),
        )
    };
    switch_workspace(state, new_idx);
    sidebar_list.select_row(Some(&row));
}

fn focus_workspace_entrypoint(root: &gtk::Widget) {
    let pane = first_leaf_pane(root);
    if !pane::focus_active_tab_in_pane(&pane) {
        if let Some(gl) = find_gl_area(&pane) {
            gl.grab_focus();
        } else if pane.is_focusable() || pane.can_focus() {
            pane.grab_focus();
        } else {
            pane.child_focus(gtk::DirectionType::TabForward);
        }
    }
}

fn first_leaf_pane(widget: &gtk::Widget) -> gtk::Widget {
    if pane::is_pane_widget(widget) {
        return widget.clone();
    }

    if let Some(paned) = widget.downcast_ref::<gtk::Paned>() {
        if let Some(child) = paned.start_child().or_else(|| paned.end_child()) {
            return first_leaf_pane(&child);
        }
    }

    if let Some(stack) = widget.downcast_ref::<gtk::Stack>() {
        if let Some(visible) = stack.visible_child() {
            return first_leaf_pane(&visible);
        }
    }

    let mut child = widget.first_child();
    while let Some(current) = child {
        let candidate = first_leaf_pane(&current);
        if pane::is_pane_widget(&candidate) {
            return candidate;
        }
        child = current.next_sibling();
    }

    widget.clone()
}

/// Default sidebar width in pixels.
const SIDEBAR_WIDTH: i32 = 220;

fn sync_top_bar_visibility(state: &State) {
    let (top_bar, preferred_visible, fullscreened) = {
        let s = state.borrow();
        (
            s.top_bar.clone(),
            s.top_bar_visible,
            gtk::prelude::GtkWindowExt::is_fullscreen(&s.window),
        )
    };

    if let Some(top_bar) = top_bar {
        top_bar.set_visible(preferred_visible && !fullscreened);
    }
}

fn toggle_top_bar(state: &State) {
    {
        let mut s = state.borrow_mut();
        s.top_bar_visible = !s.top_bar_visible;
    }
    sync_top_bar_visibility(state);
    request_session_save(state);
}

fn toggle_fullscreen(state: &State) {
    let window = state.borrow().window.clone();
    if gtk::prelude::GtkWindowExt::is_fullscreen(&window) {
        window.unfullscreen();
    } else {
        window.fullscreen();
    }
}

fn toggle_sidebar(state: &State) {
    let (sidebar_shell, sidebar_handle, current, is_visible, target_width, prior_animation, epoch) = {
        let mut s = state.borrow_mut();
        let current = sidebar_width(&s.sidebar_shell);
        let is_visible = current > 10; // treat < 10px as collapsed
        if is_visible {
            s.sidebar_expanded_width = current;
        }
        let target_width = s.sidebar_expanded_width.max(SIDEBAR_WIDTH);
        let prior_animation = s.sidebar_animation.take();
        s.sidebar_animation_epoch = s.sidebar_animation_epoch.wrapping_add(1);
        (
            s.sidebar_shell.clone(),
            s.sidebar_handle.clone(),
            current,
            is_visible,
            target_width,
            prior_animation,
            s.sidebar_animation_epoch,
        )
    };

    if let Some(animation) = prior_animation {
        animation.pause();
    }

    if is_visible {
        // Collapse: animate position to 0, then hide sidebar.
        let target = adw::CallbackAnimationTarget::new({
            let sidebar_shell = sidebar_shell.clone();
            move |value| {
                set_sidebar_width(&sidebar_shell, value as i32);
            }
        });
        let animation = adw::TimedAnimation::builder()
            .widget(&sidebar_shell)
            .value_from(current as f64)
            .value_to(0.0)
            .duration(200)
            .easing(adw::Easing::EaseInOutCubic)
            .target(&target)
            .build();
        let state_for_done = state.clone();
        animation.connect_done(move |_| {
            let is_current = {
                let mut s = state_for_done.borrow_mut();
                if s.sidebar_animation_epoch != epoch {
                    false
                } else {
                    s.sidebar_animation = None;
                    true
                }
            };
            if is_current {
                set_sidebar_state_widgets(&sidebar_shell, &sidebar_handle, 0, false);
                request_session_save(&state_for_done);
            }
        });
        state.borrow_mut().sidebar_animation = Some(animation.clone());
        animation.play();
    } else {
        // Expand: make sidebar visible, then animate position from 0 to remembered width.
        set_sidebar_state_widgets(&sidebar_shell, &sidebar_handle, 0, true);
        let target = adw::CallbackAnimationTarget::new({
            let sidebar_shell = sidebar_shell.clone();
            move |value| {
                set_sidebar_width(&sidebar_shell, value as i32);
            }
        });
        let animation = adw::TimedAnimation::builder()
            .widget(&sidebar_shell)
            .value_from(0.0)
            .value_to(target_width as f64)
            .duration(200)
            .easing(adw::Easing::EaseInOutCubic)
            .target(&target)
            .build();
        let state_for_done = state.clone();
        animation.connect_done(move |_| {
            let is_current = {
                let mut s = state_for_done.borrow_mut();
                if s.sidebar_animation_epoch != epoch {
                    false
                } else {
                    s.sidebar_animation = None;
                    true
                }
            };
            if is_current {
                request_session_save(&state_for_done);
            }
        });
        state.borrow_mut().sidebar_animation = Some(animation.clone());
        animation.play();
    }
}

// ---------------------------------------------------------------------------
// Split / close pane operations
// ---------------------------------------------------------------------------

struct SplitPaneOptions {
    initial_state: Option<PaneState>,
    skip_default_tab: bool,
    new_pane_first: bool,
    initial_ratio: Option<f64>,
    persist: bool,
}

struct PendingPaneBatch {
    state: State,
    workspace_id: String,
    workspace_name: String,
    source_pane_id: u32,
    directions: Vec<BridgePaneCreateDirection>,
    count: usize,
    created: usize,
    panes: Vec<serde_json::Value>,
    reply: Option<std::sync::mpsc::Sender<Result<serde_json::Value, BridgeError>>>,
}

fn schedule_pane_batch_step(batch: Rc<RefCell<PendingPaneBatch>>) {
    glib::timeout_add_local_once(std::time::Duration::from_millis(25), move || {
        let mut pending = batch.borrow_mut();
        let next_index = pending.created + 1;
        let direction_index = pending.created % pending.directions.len();
        let direction = PaneCreateDirection::from(pending.directions[direction_index].clone());
        let placement = pane_create_split_placement(direction);
        let workspace_root = {
            let app_state = pending.state.borrow();
            app_state
                .workspaces
                .iter()
                .find(|workspace| workspace.id == pending.workspace_id)
                .map(|workspace| workspace.root.clone())
        };
        let Some(workspace_root) = workspace_root else {
            if let Some(reply) = pending.reply.take() {
                let _ = reply.send(Err(BridgeError::not_found("workspace not found")));
            }
            return;
        };
        let source_widget = pane::pane_widget_for_root(&workspace_root, pending.source_pane_id);
        let Some(source_widget) = source_widget else {
            if let Some(reply) = pending.reply.take() {
                let _ = reply.send(Err(BridgeError::not_found("pane not found")));
            }
            return;
        };
        let new_pane = split_pane(
            &pending.state,
            &pending.workspace_id,
            &source_widget,
            placement.orientation,
            SplitPaneOptions {
                initial_state: None,
                skip_default_tab: false,
                new_pane_first: placement.new_pane_first,
                initial_ratio: None,
                persist: false,
            },
        );
        let Some(new_pane) = new_pane else {
            if let Some(reply) = pending.reply.take() {
                let _ = reply.send(Err(BridgeError::invalid_params(
                    "not enough room to split pane",
                )));
            }
            return;
        };

        let surface = pane::active_surface_summary(&new_pane);
        let Some(surface) = surface else {
            if let Some(reply) = pending.reply.take() {
                let _ = reply.send(Err(BridgeError::internal(
                    "pane.create_many did not produce a terminal surface",
                )));
            }
            return;
        };

        let workspace_id = pending.workspace_id.clone();
        let workspace_name = pending.workspace_name.clone();
        pending.panes.push(pane_create_response_payload(
            &workspace_id,
            &workspace_name,
            surface,
        ));
        pending.created = next_index;

        if pending.created == pending.count {
            request_session_save(&pending.state);
            let panes = std::mem::take(&mut pending.panes);
            let payload = serde_json::json!({
                "ok": true,
                "count": pending.count,
                "workspace_id": pending.workspace_id,
                "workspace_ref": workspace_ref(&pending.workspace_id),
                "panes": panes,
            });
            if let Some(reply) = pending.reply.take() {
                let _ = reply.send(Ok(payload));
            }
            return;
        }

        drop(pending);
        schedule_pane_batch_step(batch);
    });
}

fn split_pane(
    state: &State,
    ws_id: &str,
    pane_widget: &gtk::Widget,
    orientation: gtk::Orientation,
    options: SplitPaneOptions,
) -> Option<gtk::Widget> {
    let (shortcuts, wd, container) = {
        let s = state.borrow();
        (
            s.shortcuts.clone(),
            s.workspaces
                .iter()
                .find(|w| w.id == ws_id)
                .and_then(|ws| ws.folder_path.clone().or_else(|| ws.cwd.borrow().clone())),
            s.workspaces
                .iter()
                .find(|w| w.id == ws_id)
                .map(|ws| ws.split_container.clone()),
        )
    };
    let container = container?;
    if !container.can_split(pane_widget, orientation) {
        return None;
    }

    let new_pane = create_pane_for_workspace(
        state,
        &shortcuts,
        ws_id,
        wd.as_deref(),
        options.initial_state.as_ref(),
        options.skip_default_tab,
    );

    // Mutate the data model and trigger async widget tree rebuild.
    // The existing pane's GLArea will be unrealized then re-realized
    // on separate ticks, avoiding the GTK4 GLArea breakage.
    if !container.split(
        pane_widget,
        new_pane.clone().upcast(),
        orientation,
        options.new_pane_first,
        options
            .initial_ratio
            .unwrap_or(layout_state::DEFAULT_SPLIT_RATIO),
    ) {
        return None;
    }
    if options.persist {
        request_session_save(state);
    }
    Some(new_pane.upcast())
}

fn remove_pane(state: &State, ws_id: &str, pane_widget: &gtk::Widget) {
    remove_pane_internal(state, ws_id, pane_widget, true);
}

// purpose: Decide whether closing an empty pane should preserve its workspace.
// inputs: Current app config, empty reason, and remaining surfaces in the workspace.
// returns/effects: Returns true only for CMUX keep-open-on-last-surface semantics.
fn should_keep_workspace_open_after_empty_pane(
    config: &app_config::AppConfig,
    reason: pane::PaneEmptyReason,
    remaining_surfaces: usize,
) -> bool {
    config.app.keep_workspace_open_when_closing_last_surface
        && reason == pane::PaneEmptyReason::ClosedLastTab
        && remaining_surfaces == 0
}

fn remove_pane_internal(state: &State, ws_id: &str, pane_widget: &gtk::Widget, persist: bool) {
    let container = {
        let s = state.borrow();
        s.workspaces
            .iter()
            .find(|w| w.id == ws_id)
            .map(|ws| ws.split_container.clone())
    };

    let Some(container) = container else { return };

    // If this is the only pane, close the entire workspace
    if container.is_single_pane() {
        close_workspace_by_id(state, ws_id);
        return;
    }

    // Mutate the data model and trigger async widget tree rebuild
    container.remove(pane_widget);

    if persist {
        request_session_save(state);
    }
}

fn handle_split_with_tab(
    state: &State,
    ws_id: &str,
    source_pane: &gtk::Widget,
    target_pane: &gtk::Widget,
    orientation: gtk::Orientation,
    tab_id: &str,
    new_pane_first: bool,
) {
    if pane::tab_title(source_pane, tab_id).is_none() {
        return;
    }
    let new_pane = split_pane(
        state,
        ws_id,
        target_pane,
        orientation,
        SplitPaneOptions {
            initial_state: None,
            skip_default_tab: true,
            new_pane_first,
            initial_ratio: None,
            persist: false,
        },
    );
    let Some(new_pane) = new_pane else { return };
    if pane::move_tab_to_pane(source_pane, tab_id, &new_pane) {
        request_session_save(state);
    }
}

/// Find the focused pane widget (a gtk::Box with class limux-pane-toolbar child)
/// by walking up from the currently focused widget.
fn find_leaf_focused_pane(state: &State) -> Option<(String, gtk::Widget)> {
    let (ws_id, root, stack) = {
        let s = state.borrow();
        let ws = s.active_workspace()?;
        (ws.id.clone(), ws.root.clone(), s.stack.clone())
    };

    // Get the window's focus widget and walk up to find a pane Box
    let window = stack.root()?.downcast::<gtk::Window>().ok()?;
    let focus = gtk::prelude::GtkWindowExt::focus(&window)?;

    let mut widget: Option<gtk::Widget> = Some(focus);
    while let Some(w) = widget {
        if let Some(bx) = w.downcast_ref::<gtk::Box>() {
            let mut child = bx.first_child();
            while let Some(c) = child {
                if c.has_css_class("limux-pane-header") {
                    return Some((ws_id, w));
                }
                child = c.next_sibling();
            }
        }
        widget = w.parent();
    }

    let _ = root;
    None
}

fn find_focused_pane(state: &State) -> Option<(String, gtk::Widget)> {
    if let Some(found) = find_leaf_focused_pane(state) {
        return Some(found);
    }

    let (ws_id, root) = {
        let s = state.borrow();
        let ws = s.active_workspace()?;
        (ws.id.clone(), ws.root.clone())
    };

    Some((ws_id, first_leaf_pane(&root)))
}

fn focused_shortcut_target(state: &State) -> pane::FocusedShortcutTarget {
    let Some((_ws_id, pane_widget)) = find_leaf_focused_pane(state) else {
        return pane::FocusedShortcutTarget::None;
    };
    pane::focused_shortcut_target(&pane_widget)
}

fn show_runtime_error(state: &State, title: &str, detail: &str) {
    let window = state.borrow().window.clone();
    let dialog = gtk::AlertDialog::builder()
        .modal(true)
        .message(title)
        .detail(detail)
        .build();
    dialog.show(Some(&window));
}

fn quit_app(state: &State) {
    save_session_now(state);
    state.borrow().app.quit();
}

fn spawn_new_instance(state: &State) -> bool {
    let exe = match std::env::current_exe() {
        Ok(exe) => exe,
        Err(err) => {
            let detail = format!("Failed to resolve the current Limux executable: {err}");
            eprintln!("limux: {detail}");
            show_runtime_error(state, "Failed to open a new Limux instance", &detail);
            return false;
        }
    };

    match std::process::Command::new(exe).spawn() {
        Ok(_) => true,
        Err(err) => {
            let detail = format!("Failed to launch a new Limux instance: {err}");
            eprintln!("limux: {detail}");
            show_runtime_error(state, "Failed to open a new Limux instance", &detail);
            false
        }
    }
}

fn dispatch_terminal_command(state: &State, command: ShortcutCommand) -> bool {
    let pane::FocusedShortcutTarget::Terminal(target) = focused_shortcut_target(state) else {
        return false;
    };

    match command {
        ShortcutCommand::SurfaceFind => target.show_find(),
        ShortcutCommand::SurfaceFindNext => target.find_next(),
        ShortcutCommand::SurfaceFindPrevious => target.find_previous(),
        ShortcutCommand::SurfaceFindHide => target.hide_find(),
        ShortcutCommand::SurfaceUseSelectionForFind => target.use_selection_for_find(),
        ShortcutCommand::TerminalClearScrollback => target.perform_binding_action("clear_screen"),
        ShortcutCommand::TerminalCopy => target.perform_binding_action("copy_to_clipboard"),
        ShortcutCommand::TerminalPaste => target.perform_binding_action("paste_from_clipboard"),
        ShortcutCommand::TerminalIncreaseFontSize => persist_font_size_delta(state, 1.0),
        ShortcutCommand::TerminalDecreaseFontSize => persist_font_size_delta(state, -1.0),
        ShortcutCommand::TerminalResetFontSize => persist_font_size_reset(state),
        _ => false,
    }
}

fn persist_font_size_delta(state: &State, delta: f32) -> bool {
    let current = {
        let s = state.borrow();
        let current = s.config.borrow().font_size;
        current
    };
    let new_size = font_size_after_delta(current, crate::terminal::default_font_size(), delta);

    if let Err(err) = persist_font_size(state, Some(new_size)) {
        show_font_size_save_error(state, err);
        return false;
    }

    broadcast_font_size(new_size);
    true
}

fn persist_font_size_reset(state: &State) -> bool {
    if let Err(err) = persist_font_size(state, None) {
        show_font_size_save_error(state, err);
        return false;
    }

    crate::terminal::broadcast_binding_action("reset_font_size");
    true
}

fn persist_font_size(state: &State, font_size: Option<f32>) -> Result<(), String> {
    let mut updated = {
        let s = state.borrow();
        let updated = s.config.borrow().clone();
        updated
    };
    updated.font_size = font_size;
    app_config::save(&updated)?;

    state.borrow().config.borrow_mut().font_size = font_size;
    Ok(())
}

fn font_size_after_delta(current: Option<f32>, default: f32, delta: f32) -> f32 {
    (current.unwrap_or(default) + delta).clamp(1.0, 255.0)
}

fn show_font_size_save_error(state: &State, err: String) {
    let detail = format!("Failed to save Limux settings: {err}");
    eprintln!("limux: {detail}");
    show_runtime_error(state, "Failed to save settings", &detail);
}

fn broadcast_font_size(size: f32) {
    let action = format!("set_font_size:{size}");
    crate::terminal::broadcast_binding_action(&action);
}

fn dispatch_browser_command(state: &State, command: ShortcutCommand) -> bool {
    let pane::FocusedShortcutTarget::Browser(target) = focused_shortcut_target(state) else {
        return false;
    };

    match command {
        ShortcutCommand::BrowserFocusLocation => target.focus_location(),
        ShortcutCommand::BrowserBack => target.go_back(),
        ShortcutCommand::BrowserForward => target.go_forward(),
        ShortcutCommand::BrowserReload => target.reload(),
        ShortcutCommand::BrowserInspector => target.show_inspector(),
        ShortcutCommand::BrowserConsole => target.show_console(),
        ShortcutCommand::SurfaceFind => target.show_find(),
        ShortcutCommand::SurfaceFindNext => target.find_next(),
        ShortcutCommand::SurfaceFindPrevious => target.find_previous(),
        ShortcutCommand::SurfaceFindHide => target.hide_find(),
        ShortcutCommand::SurfaceUseSelectionForFind => target.use_selection_for_find(),
        ShortcutCommand::OpenBrowserInSplit => {
            let uri = target.current_uri();
            let Some((ws_id, pane_widget)) = find_leaf_focused_pane(state) else {
                return false;
            };
            split_pane(
                state,
                &ws_id,
                &pane_widget,
                gtk::Orientation::Horizontal,
                SplitPaneOptions {
                    initial_state: Some(PaneState::browser_only(uri.as_deref())),
                    skip_default_tab: false,
                    new_pane_first: false,
                    initial_ratio: None,
                    persist: true,
                },
            )
            .is_some()
        }
        _ => false,
    }
}

fn split_focused_pane(state: &State, orientation: gtk::Orientation) {
    if let Some((ws_id, pane_widget)) = find_focused_pane(state) {
        let _ = split_pane(
            state,
            &ws_id,
            &pane_widget,
            orientation,
            SplitPaneOptions {
                initial_state: None,
                skip_default_tab: false,
                new_pane_first: false,
                initial_ratio: None,
                persist: true,
            },
        );
    }
}

fn cycle_focused_pane_tab(state: &State, delta: i32) {
    if let Some((_ws_id, pane_widget)) = find_focused_pane(state) {
        pane::cycle_tab_in_pane(&pane_widget, delta);
    }
}

fn close_focused_tab(state: &State) {
    if let Some((ws_id, pane_widget)) = find_focused_pane(state) {
        let parent = pane_widget.parent();
        // If this is the only pane (parent is Stack), don't close — keep workspace alive
        if let Some(ref p) = parent {
            if p.downcast_ref::<gtk::Stack>().is_some() {
                return;
            }
        }
        remove_pane(state, &ws_id, &pane_widget);
    }
}

fn toggle_focused_pane_zoom(state: &State) {
    let Some((ws_id, pane_widget)) = find_focused_pane(state) else {
        return;
    };
    let container = {
        let s = state.borrow();
        s.workspaces
            .iter()
            .find(|workspace| workspace.id == ws_id)
            .map(|workspace| workspace.split_container.clone())
    };
    if let Some(container) = container {
        container.toggle_zoom(&pane_widget);
    }
}

fn add_tab_to_focused_pane(_state: &State, _browser: bool) {
    if let Some((_ws_id, pane_widget)) = find_focused_pane(_state) {
        if _browser {
            pane::add_browser_tab_to_pane(&pane_widget);
        } else {
            pane::add_terminal_tab_to_pane(&pane_widget);
        }
    }
}

/// Direction for pane navigation.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum Direction {
    Left,
    Right,
    Up,
    Down,
}

#[derive(Clone, Copy, Debug, PartialEq)]
struct PaneBounds {
    left: f64,
    top: f64,
    right: f64,
    bottom: f64,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct NeighborScore {
    has_overlap: bool,
    overlap: i32,
    gap: i32,
    center_delta: i32,
}

/// Focus the neighboring pane in the given direction by walking the gtk::Paned tree.
fn focus_pane_in_direction(state: &State, direction: Direction) {
    let (_ws_id, pane_widget) = match find_focused_pane(state) {
        Some(v) => v,
        None => return,
    };
    let root = state.borrow().window.clone().upcast::<gtk::Widget>();

    // Determine which axis and sides we care about.
    let (target_orientation, must_be_start) = match direction {
        Direction::Left => (gtk::Orientation::Horizontal, false), // must be end_child to go left
        Direction::Right => (gtk::Orientation::Horizontal, true), // must be start_child to go right
        Direction::Up => (gtk::Orientation::Vertical, false),     // must be end_child to go up
        Direction::Down => (gtk::Orientation::Vertical, true),    // must be start_child to go down
    };

    // Walk up from the focused pane to find a gtk::Paned with the right
    // orientation where the current subtree is on the correct side.
    let mut current: gtk::Widget = pane_widget.clone();
    loop {
        let parent = match current.parent() {
            Some(p) => p,
            None => return, // reached the top without finding a valid split
        };
        if let Some(paned) = parent.downcast_ref::<gtk::Paned>() {
            if paned.orientation() == target_orientation {
                let is_start = paned.start_child().map(|c| c == current).unwrap_or(false);
                if is_start == must_be_start {
                    // Found the split point. Navigate to the sibling subtree.
                    let sibling = if must_be_start {
                        paned.end_child()
                    } else {
                        paned.start_child()
                    };
                    if let Some(sibling) = sibling {
                        let leaf =
                            best_directional_leaf_pane(&pane_widget, &sibling, &root, direction)
                                .unwrap_or_else(|| {
                                    // Fall back to the old edge-based heuristic if bounds
                                    // are unavailable for some reason.
                                    let prefer_start = !must_be_start;
                                    find_leaf_pane(&sibling, target_orientation, prefer_start)
                                });
                        // Find the GLArea inside the pane and focus it directly
                        if let Some(gl) = find_gl_area(&leaf) {
                            gl.grab_focus();
                        }
                    }
                    return;
                }
            }
        }
        current = parent;
    }
}

fn widget_bounds_in_root(widget: &gtk::Widget, root: &gtk::Widget) -> Option<PaneBounds> {
    let allocation = widget.allocation();
    let width = allocation.width();
    let height = allocation.height();
    if width <= 0 || height <= 0 {
        return None;
    }

    let (left, top) = widget.translate_coordinates(root, 0.0, 0.0)?;
    Some(PaneBounds {
        left,
        top,
        right: left + f64::from(width),
        bottom: top + f64::from(height),
    })
}

fn overlap_1d(a_start: f64, a_end: f64, b_start: f64, b_end: f64) -> i32 {
    (a_end.min(b_end) - a_start.max(b_start)).max(0.0).round() as i32
}

fn directional_neighbor_score(
    current: PaneBounds,
    candidate: PaneBounds,
    direction: Direction,
) -> Option<NeighborScore> {
    let (gap, overlap, current_center, candidate_center) = match direction {
        Direction::Left => (
            current.left - candidate.right,
            overlap_1d(current.top, current.bottom, candidate.top, candidate.bottom),
            (current.top + current.bottom) / 2.0,
            (candidate.top + candidate.bottom) / 2.0,
        ),
        Direction::Right => (
            candidate.left - current.right,
            overlap_1d(current.top, current.bottom, candidate.top, candidate.bottom),
            (current.top + current.bottom) / 2.0,
            (candidate.top + candidate.bottom) / 2.0,
        ),
        Direction::Up => (
            current.top - candidate.bottom,
            overlap_1d(current.left, current.right, candidate.left, candidate.right),
            (current.left + current.right) / 2.0,
            (candidate.left + candidate.right) / 2.0,
        ),
        Direction::Down => (
            candidate.top - current.bottom,
            overlap_1d(current.left, current.right, candidate.left, candidate.right),
            (current.left + current.right) / 2.0,
            (candidate.left + candidate.right) / 2.0,
        ),
    };

    if gap < -0.5 {
        return None;
    }

    Some(NeighborScore {
        has_overlap: overlap > 0,
        overlap,
        gap: gap.max(0.0).round() as i32,
        center_delta: (candidate_center - current_center).abs().round() as i32,
    })
}

fn neighbor_score_better(candidate: NeighborScore, best: NeighborScore) -> bool {
    (
        candidate.has_overlap,
        candidate.overlap,
        -candidate.gap,
        -candidate.center_delta,
    ) > (
        best.has_overlap,
        best.overlap,
        -best.gap,
        -best.center_delta,
    )
}

fn collect_leaf_panes(widget: &gtk::Widget, panes: &mut Vec<gtk::Widget>) {
    if pane::is_pane_widget(widget) {
        panes.push(widget.clone());
        return;
    }

    if let Some(paned) = widget.downcast_ref::<gtk::Paned>() {
        if let Some(child) = paned.start_child() {
            collect_leaf_panes(&child, panes);
        }
        if let Some(child) = paned.end_child() {
            collect_leaf_panes(&child, panes);
        }
        return;
    }

    if let Some(stack) = widget.downcast_ref::<gtk::Stack>() {
        if let Some(visible) = stack.visible_child() {
            collect_leaf_panes(&visible, panes);
        }
        return;
    }

    let mut child = widget.first_child();
    while let Some(current) = child {
        collect_leaf_panes(&current, panes);
        child = current.next_sibling();
    }
}

fn best_directional_leaf_pane(
    current_pane: &gtk::Widget,
    sibling_subtree: &gtk::Widget,
    root: &gtk::Widget,
    direction: Direction,
) -> Option<gtk::Widget> {
    let current_bounds = widget_bounds_in_root(current_pane, root)?;
    let mut leaves = Vec::new();
    collect_leaf_panes(sibling_subtree, &mut leaves);

    let mut best: Option<(gtk::Widget, NeighborScore)> = None;
    for leaf in leaves {
        let Some(bounds) = widget_bounds_in_root(&leaf, root) else {
            continue;
        };
        let Some(score) = directional_neighbor_score(current_bounds, bounds, direction) else {
            continue;
        };

        let should_replace = best
            .as_ref()
            .map(|(_, best_score)| neighbor_score_better(score, *best_score))
            .unwrap_or(true);
        if should_replace {
            best = Some((leaf, score));
        }
    }

    best.map(|(leaf, _)| leaf)
}

/// Recursively find the first visible GLArea inside a widget tree.
/// For gtk::Stack containers, only descend into the visible child.
pub(crate) fn find_gl_area(widget: &gtk::Widget) -> Option<gtk::GLArea> {
    if let Some(gl) = widget.downcast_ref::<gtk::GLArea>() {
        return Some(gl.clone());
    }
    // For Stack widgets, only search the visible child
    if let Some(stack) = widget.downcast_ref::<gtk::Stack>() {
        if let Some(visible) = stack.visible_child() {
            return find_gl_area(&visible);
        }
        return None;
    }
    let mut child = widget.first_child();
    while let Some(c) = child {
        if let Some(gl) = find_gl_area(&c) {
            return Some(gl);
        }
        child = c.next_sibling();
    }
    None
}

/// Descend a pane/split subtree to find a leaf pane widget.
/// When encountering a gtk::Paned matching `axis`, prefer `start_child` if
/// `prefer_start` is true (to find the nearest edge). For Paned widgets on
/// the other axis, prefer start_child (arbitrary but consistent).
fn find_leaf_pane(widget: &gtk::Widget, axis: gtk::Orientation, prefer_start: bool) -> gtk::Widget {
    if let Some(paned) = widget.downcast_ref::<gtk::Paned>() {
        let pick_start = if paned.orientation() == axis {
            prefer_start
        } else {
            true // arbitrary default for orthogonal splits
        };
        let child = if pick_start {
            paned.start_child()
        } else {
            paned.end_child()
        };
        match child {
            Some(c) => find_leaf_pane(&c, axis, prefer_start),
            None => widget.clone(),
        }
    } else {
        // Leaf pane — this is a pane gtk::Box
        widget.clone()
    }
}

// purpose: Serialize agent notification category for API responses.
// inputs: Parsed category from notification request params.
// returns/effects: Returns CMUX category spelling.
fn agent_notify_category_str(category: app_config::AgentNotifyCategory) -> &'static str {
    match category {
        app_config::AgentNotifyCategory::TurnComplete => "turn-complete",
        app_config::AgentNotifyCategory::NeedsPermission => "needs-permission",
        app_config::AgentNotifyCategory::IdleReminder => "idle-reminder",
        app_config::AgentNotifyCategory::Other => "other",
    }
}

fn should_emit_desktop_notification(
    desktop_notifications_enabled: bool,
    window_active: bool,
    workspace_is_active: bool,
    source_focused: bool,
    suppress_only_focused_surface: bool,
) -> bool {
    desktop_notifications_enabled
        && (!window_active
            || !workspace_is_active
            || (suppress_only_focused_surface && !source_focused))
}

fn mark_workspace_unread(
    state: &State,
    ws_id: &str,
    source_focused: bool,
    target: DesktopNotificationTarget,
) -> Option<DesktopNotificationRequest> {
    mark_workspace_unread_with_message(
        state,
        ws_id,
        "Process needs attention",
        source_focused,
        target,
        Vec::new(),
    )
}

fn workspace_notification_message(title: &str, body: &str) -> String {
    let title = title.trim();
    let body = body.trim();
    match (title.is_empty(), body.is_empty()) {
        (false, false) => format!("{title}: {body}"),
        (false, true) => title.to_string(),
        (true, false) => body.to_string(),
        (true, true) => "Process needs attention".to_string(),
    }
}

// purpose: Create the public timestamp used by host notification rows.
// inputs: System wall clock.
// returns/effects: Panics if the system clock is before the Unix epoch.
fn notification_created_at() -> String {
    let millis = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("system time should be after UNIX_EPOCH")
        .as_millis();
    format!("unix_ms:{millis}")
}

// purpose: Resolve optional notification surface metadata within one workspace.
// inputs: Workspace and an optional CMUX surface or tab hint.
// returns/effects: Returns None for workspace notifications or an error for unknown hints.
fn notification_surface_metadata(
    workspace: &Workspace,
    surface_hint: Option<&str>,
) -> Result<Option<pane::SurfaceSummary>, BridgeError> {
    let Some(hint) = surface_hint else {
        return Ok(None);
    };
    pane::surface_summaries_for_root(&workspace.root)
        .into_iter()
        .find(|surface| surface_hint_matches(&surface.surface_id, hint))
        .map(Some)
        .ok_or_else(|| BridgeError::not_found("surface not found"))
}

// purpose: Build CMUX-shaped notification policy JSON for configured hooks.
// inputs: Workspace/surface context, notification fields, current effects, and hook id.
// returns/effects: Returns JSON passed to a notification hook on stdin.
fn notification_hook_policy_payload(
    hook_id: &str,
    context: &NotificationPolicyContext,
    effects: NotificationPolicyEffects,
) -> serde_json::Value {
    serde_json::json!({
        "version": 1,
        "notification": {
            "workspaceId": context.workspace_id,
            "surfaceId": context.surface_id,
            "title": context.title,
            "subtitle": context.subtitle,
            "body": context.body,
        },
        "context": {
            "cwd": context.cwd,
            "configPath": app_config::settings_path().map(|path| path.display().to_string()),
            "hookId": hook_id,
            "appFocused": context.app_focused,
            "focusedPanel": context.focused_panel,
        },
        "effects": {
            "record": effects.record,
            "markUnread": effects.mark_unread,
            "reorderWorkspace": true,
            "desktop": effects.desktop,
            "sound": effects.sound,
            "command": effects.command,
            "paneFlash": effects.pane_flash,
        }
    })
}

// purpose: Parse a hook-returned CMUX notification effects object.
// inputs: Previous effects and hook output JSON.
// returns/effects: Returns updated effects, preserving previous values for omitted fields.
fn notification_policy_effects_from_value(
    previous: NotificationPolicyEffects,
    value: &serde_json::Value,
) -> Result<NotificationPolicyEffects, BridgeError> {
    let effects = value
        .get("effects")
        .and_then(serde_json::Value::as_object)
        .ok_or_else(|| BridgeError::internal("notification hook output missing effects object"))?;
    Ok(NotificationPolicyEffects {
        record: effects
            .get("record")
            .and_then(serde_json::Value::as_bool)
            .unwrap_or(previous.record),
        mark_unread: effects
            .get("markUnread")
            .or_else(|| effects.get("mark_unread"))
            .and_then(serde_json::Value::as_bool)
            .unwrap_or(previous.mark_unread),
        desktop: effects
            .get("desktop")
            .and_then(serde_json::Value::as_bool)
            .unwrap_or(previous.desktop),
        sound: effects
            .get("sound")
            .and_then(serde_json::Value::as_bool)
            .unwrap_or(previous.sound),
        command: effects
            .get("command")
            .and_then(serde_json::Value::as_bool)
            .unwrap_or(previous.command),
        pane_flash: effects
            .get("paneFlash")
            .or_else(|| effects.get("pane_flash"))
            .and_then(serde_json::Value::as_bool)
            .unwrap_or(previous.pane_flash),
    })
}

// purpose: Run one configured notification hook with bounded runtime.
// inputs: Hook config and policy JSON.
// returns/effects: Feeds JSON on stdin and returns parsed stdout JSON or a bridge error.
fn run_notification_hook_command(
    hook: &app_config::NotificationHookConfig,
    payload: &serde_json::Value,
) -> Result<serde_json::Value, BridgeError> {
    let input = serde_json::to_vec(payload)
        .map_err(|err| BridgeError::internal(format!("notification hook payload failed: {err}")))?;
    let mut child = Command::new("sh")
        .arg("-c")
        .arg(&hook.command)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|err| {
            BridgeError::internal(format!(
                "notification hook `{}` failed to start: {err}",
                hook.id
            ))
        })?;
    {
        let mut stdin = child.stdin.take().ok_or_else(|| {
            BridgeError::internal(format!("notification hook `{}` stdin unavailable", hook.id))
        })?;
        stdin.write_all(&input).map_err(|err| {
            BridgeError::internal(format!(
                "notification hook `{}` stdin write failed: {err}",
                hook.id
            ))
        })?;
    }
    let deadline = Instant::now() + Duration::from_secs(hook.timeout_seconds.max(1));
    loop {
        if child
            .try_wait()
            .map_err(|err| {
                BridgeError::internal(format!(
                    "notification hook `{}` wait failed: {err}",
                    hook.id
                ))
            })?
            .is_some()
        {
            let output = child.wait_with_output().map_err(|err| {
                BridgeError::internal(format!(
                    "notification hook `{}` output failed: {err}",
                    hook.id
                ))
            })?;
            if !output.status.success() {
                let stderr = String::from_utf8_lossy(&output.stderr);
                return Err(BridgeError::internal(format!(
                    "notification hook `{}` exited with status {}: {}",
                    hook.id,
                    output.status,
                    stderr.trim()
                )));
            }
            return serde_json::from_slice(&output.stdout).map_err(|err| {
                BridgeError::internal(format!(
                    "notification hook `{}` returned invalid JSON: {err}",
                    hook.id
                ))
            });
        }
        if Instant::now() >= deadline {
            let _ = child.kill();
            let _ = child.wait();
            return Err(BridgeError::internal(format!(
                "notification hook `{}` timed out after {}s",
                hook.id, hook.timeout_seconds
            )));
        }
        std::thread::sleep(Duration::from_millis(10));
    }
}

// purpose: Run configured notification hooks and collect final delivery effects.
// inputs: App config, workspace/surface context, and notification fields.
// returns/effects: Executes enabled hooks in order and returns final delivery policy.
fn notification_policy_effects(
    config: &app_config::NotificationConfig,
    context: &NotificationPolicyContext,
) -> Result<NotificationPolicyEffects, BridgeError> {
    let mut effects = NotificationPolicyEffects {
        pane_flash: config.pane_flash,
        ..NotificationPolicyEffects::default()
    };
    for hook in config.hooks.iter().filter(|hook| hook.enabled) {
        let payload = notification_hook_policy_payload(&hook.id, context, effects);
        let output = run_notification_hook_command(hook, &payload)?;
        effects = notification_policy_effects_from_value(effects, &output)?;
    }
    Ok(effects)
}

// purpose: Build CMUX-compatible environment variables for a notification command.
// inputs: Notification title, subtitle, and body strings.
// returns/effects: Returns env pairs without mutating process state.
fn notification_command_env(
    title: &str,
    subtitle: &str,
    body: &str,
) -> [(&'static str, String); 6] {
    [
        ("CMUX_NOTIFICATION_TITLE", title.to_string()),
        ("CMUX_NOTIFICATION_SUBTITLE", subtitle.to_string()),
        ("CMUX_NOTIFICATION_BODY", body.to_string()),
        ("LIMUX_NOTIFICATION_TITLE", title.to_string()),
        ("LIMUX_NOTIFICATION_SUBTITLE", subtitle.to_string()),
        ("LIMUX_NOTIFICATION_BODY", body.to_string()),
    ]
}

// purpose: Run the configured CMUX notification command without blocking the GTK loop.
// inputs: Shell command and notification fields.
// returns/effects: Spawns the command or returns a bridge error before reporting success.
fn spawn_notification_command(
    command: &str,
    title: &str,
    subtitle: &str,
    body: &str,
) -> Result<(), BridgeError> {
    let trimmed = command.trim();
    if trimmed.is_empty() {
        return Ok(());
    }
    let env = notification_command_env(title, subtitle, body);
    let mut child = Command::new("sh")
        .arg("-c")
        .arg(trimmed)
        .envs(env)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::inherit())
        .spawn()
        .map_err(|err| {
            BridgeError::internal(format!("notification command failed to start: {err}"))
        })?;
    std::thread::spawn(move || match child.wait() {
        Ok(status) if status.success() => {}
        Ok(status) => eprintln!("notification command exited with status {status}"),
        Err(err) => eprintln!("notification command wait failed: {err}"),
    });
    Ok(())
}

/// purpose: Render one live-host notification in the public control API shape.
/// inputs: notification is a stored host notification.
/// returns/effects: Returns JSON without mutating state.
fn host_notification_row(notification: &HostNotification) -> serde_json::Value {
    serde_json::json!({
        "id": notification.id,
        "notification_id": notification.id,
        "message": notification.message,
        "title": notification.title,
        "subtitle": notification.subtitle,
        "body": notification.body,
        "created_at": notification.created_at,
        "workspace_id": notification.workspace_id,
        "workspace_ref": workspace_ref(&notification.workspace_id),
        "surface_id": notification.surface_id,
        "surface_ref": notification.surface_id.as_ref().map(|surface_id| surface_ref(surface_id)),
        "pane_id": notification.pane_id.map(|pane_id| pane_id.to_string()),
        "pane_ref": notification.pane_id.map(pane_ref),
        "tab_title": notification.tab_title,
        "is_read": !notification.unread,
        "unread": notification.unread,
    })
}

fn publish_notification_event(name: &str, notification: &HostNotification) {
    crate::event_bus::bus().publish(crate::event_bus::EventPublish {
        name,
        category: "notification",
        source: "notification.store",
        workspace_id: Some(serde_json::Value::String(notification.workspace_id.clone())),
        surface_id: notification
            .surface_id
            .clone()
            .map(serde_json::Value::String),
        pane_id: notification
            .pane_id
            .map(|pane_id| serde_json::Value::String(pane_id.to_string())),
        payload: serde_json::json!({
            "notification_id": notification.id,
            "created_at": notification.created_at,
            "tab_title": notification.tab_title,
            "title_length": notification.title.len(),
            "subtitle_length": notification.subtitle.len(),
            "body_length": notification.body.len(),
            "message_length": notification.message.len(),
            "redacted_fields": ["title", "subtitle", "body", "message"],
        }),
    });
}

fn publish_notification_bulk_event(name: &str, count: usize) {
    crate::event_bus::bus().publish(crate::event_bus::EventPublish {
        name,
        category: "notification",
        source: "notification.store",
        workspace_id: None,
        surface_id: None,
        pane_id: None,
        payload: serde_json::json!({ "count": count }),
    });
}

/// purpose: Store a notification in the live host's bounded inbox.
/// inputs: state plus normalized notification fields.
/// returns/effects: Mutates the in-memory inbox and returns the stored notification.
fn push_host_notification(
    state: &State,
    workspace_id: String,
    surface: Option<pane::SurfaceSummary>,
    title: String,
    subtitle: String,
    body: String,
    message: String,
) -> HostNotification {
    let mut s = state.borrow_mut();
    let id = s.next_notification_id;
    s.next_notification_id = s.next_notification_id.saturating_add(1);
    let surface_id = surface.as_ref().map(|surface| surface.surface_id.clone());
    let pane_id = surface.as_ref().map(|surface| surface.pane_id);
    let tab_title = surface.map(|surface| surface.title);
    let notification = HostNotification {
        id,
        workspace_id,
        surface_id,
        pane_id,
        tab_title,
        created_at: notification_created_at(),
        title,
        subtitle,
        body,
        message,
        unread: true,
    };
    s.notifications.push(notification.clone());
    if s.notifications.len() > MAX_HOST_NOTIFICATIONS {
        s.notifications.remove(0);
    }
    publish_notification_event("notification.created", &notification);
    notification
}

/// purpose: List notifications retained by the live GTK host.
/// inputs: unread_only filters read entries when true.
/// returns/effects: Returns JSON rows without mutating state.
fn list_host_notifications(state: &State, unread_only: bool) -> serde_json::Value {
    let rows = {
        let s = state.borrow();
        s.notifications
            .iter()
            .filter(|notification| !unread_only || notification.unread)
            .map(host_notification_row)
            .collect::<Vec<_>>()
    };
    serde_json::json!({ "notifications": rows })
}

/// purpose: Clear unread styling when a workspace has no remaining unread notifications.
/// inputs: Mutable app state and a workspace id that may have changed notification state.
/// returns/effects: Mutates sidebar row state when no unread notifications remain.
fn clear_workspace_unread_if_empty(state: &mut AppState, workspace_id: &str) {
    let still_unread = state
        .notifications
        .iter()
        .any(|notification| notification.workspace_id == workspace_id && notification.unread);
    if still_unread {
        return;
    }
    if let Some(workspace) = state
        .workspaces
        .iter_mut()
        .find(|workspace| workspace.id == workspace_id)
    {
        clear_workspace_unread_visual(workspace);
    }
}

/// purpose: Clear the sidebar unread visual state for one workspace row.
/// inputs: workspace is a mutable live workspace row.
/// returns/effects: Mutates row CSS/classes to match a read workspace.
fn clear_workspace_unread_visual(workspace: &mut Workspace) {
    workspace.unread = false;
    workspace.notify_dot.remove_css_class("limux-notify-dot");
    workspace
        .notify_dot
        .add_css_class("limux-notify-dot-hidden");
    workspace
        .notify_label
        .remove_css_class("limux-notify-msg-unread");
    workspace.notify_label.add_css_class("limux-notify-msg");
    workspace.notify_label.set_visible(false);
    if let Some(row_box) = workspace.sidebar_row.child() {
        row_box.remove_css_class("limux-sidebar-row-unread");
    }
}

/// purpose: Remove one notification or all read notifications from the live inbox.
/// inputs: notification_id targets one row; all_read removes only read rows.
/// returns/effects: Mutates inbox and affected workspace unread state.
fn dismiss_host_notifications(
    state: &State,
    notification_id: Option<u64>,
    all_read: bool,
) -> Result<serde_json::Value, crate::control_bridge::BridgeError> {
    let rows = {
        let mut s = state.borrow_mut();
        let affected = if let Some(target_id) = notification_id {
            let Some(workspace_id) = s
                .notifications
                .iter()
                .find(|notification| notification.id == target_id)
                .map(|notification| notification.workspace_id.clone())
            else {
                return Err(crate::control_bridge::BridgeError::not_found(
                    "notification not found",
                ));
            };
            s.notifications
                .retain(|notification| notification.id != target_id);
            vec![workspace_id]
        } else if all_read {
            let affected = s
                .notifications
                .iter()
                .filter(|notification| !notification.unread)
                .map(|notification| notification.workspace_id.clone())
                .collect::<Vec<_>>();
            s.notifications.retain(|notification| notification.unread);
            affected
        } else {
            return Err(crate::control_bridge::BridgeError::invalid_params(
                "dismiss requires id or all_read",
            ));
        };
        let removed_count = affected.len();
        for workspace_id in affected {
            clear_workspace_unread_if_empty(&mut s, &workspace_id);
        }
        publish_notification_bulk_event("notification.removed", removed_count);
        s.notifications
            .iter()
            .map(host_notification_row)
            .collect::<Vec<_>>()
    };
    Ok(serde_json::json!({ "notifications": rows }))
}

/// purpose: Mark one notification as read and update the owning workspace row.
/// inputs: notification_id identifies an existing live-host notification.
/// returns/effects: Mutates notification unread state and sidebar visual state.
fn mark_host_notification_read(
    state: &State,
    notification_id: u64,
) -> Result<serde_json::Value, crate::control_bridge::BridgeError> {
    let row = {
        let mut s = state.borrow_mut();
        let Some(index) = s
            .notifications
            .iter()
            .position(|notification| notification.id == notification_id)
        else {
            return Err(crate::control_bridge::BridgeError::not_found(
                "notification not found",
            ));
        };
        s.notifications[index].unread = false;
        let workspace_id = s.notifications[index].workspace_id.clone();
        clear_workspace_unread_if_empty(&mut s, &workspace_id);
        publish_notification_event("notification.read", &s.notifications[index]);
        host_notification_row(&s.notifications[index])
    };
    Ok(serde_json::json!({ "notification": row }))
}

/// purpose: Mark all notifications for a workspace selector as read.
/// inputs: target resolves to one live workspace.
/// returns/effects: Mutates notifications and sidebar visual state for that workspace.
fn mark_workspace_notifications_read(
    state: &State,
    target: &WorkspaceTarget,
) -> Result<serde_json::Value, crate::control_bridge::BridgeError> {
    let rows = {
        let mut s = state.borrow_mut();
        let Some(index) = workspace_index_for_target(&s, target) else {
            return Err(crate::control_bridge::BridgeError::not_found(
                "workspace not found",
            ));
        };
        let workspace_id = s.workspaces[index].id.clone();
        for notification in &mut s.notifications {
            if notification.workspace_id == workspace_id {
                notification.unread = false;
                publish_notification_event("notification.read", notification);
            }
        }
        clear_workspace_unread_visual(&mut s.workspaces[index]);
        s.notifications
            .iter()
            .filter(|notification| notification.workspace_id == workspace_id)
            .map(host_notification_row)
            .collect::<Vec<_>>()
    };
    Ok(serde_json::json!({ "notifications": rows }))
}

/// purpose: Mark every live-host notification as read.
/// inputs: Mutable app state.
/// returns/effects: Mutates all retained notifications and workspace unread visuals.
fn mark_all_host_notifications_read(state: &State) -> serde_json::Value {
    let rows = {
        let mut s = state.borrow_mut();
        for notification in &mut s.notifications {
            notification.unread = false;
            publish_notification_event("notification.read", notification);
        }
        for workspace in &mut s.workspaces {
            clear_workspace_unread_visual(workspace);
        }
        s.notifications
            .iter()
            .map(host_notification_row)
            .collect::<Vec<_>>()
    };
    serde_json::json!({ "notifications": rows })
}

/// purpose: Focus the workspace behind one notification and mark it read.
/// inputs: notification_id identifies an existing live-host notification.
/// returns/effects: Mutates notification read state and switches workspace focus.
fn open_host_notification(
    state: &State,
    notification_id: u64,
) -> Result<serde_json::Value, crate::control_bridge::BridgeError> {
    let (workspace_id, row) = {
        let mut s = state.borrow_mut();
        let Some(index) = s
            .notifications
            .iter()
            .position(|notification| notification.id == notification_id)
        else {
            return Err(crate::control_bridge::BridgeError::not_found(
                "notification not found",
            ));
        };
        s.notifications[index].unread = false;
        let workspace_id = s.notifications[index].workspace_id.clone();
        clear_workspace_unread_if_empty(&mut s, &workspace_id);
        publish_notification_event("notification.read", &s.notifications[index]);
        (workspace_id, host_notification_row(&s.notifications[index]))
    };
    let target_index = {
        let s = state.borrow();
        s.workspaces
            .iter()
            .position(|workspace| workspace.id == workspace_id)
    };
    let Some(target_index) = target_index else {
        return Err(crate::control_bridge::BridgeError::not_found(
            "workspace not found",
        ));
    };
    switch_workspace(state, target_index);
    Ok(serde_json::json!({
        "notification": row,
        "workspace_id": workspace_id,
        "workspace_ref": workspace_ref(&workspace_id),
    }))
}

/// purpose: Open the newest unread notification.
/// inputs: Current live-host notification store.
/// returns/effects: Mutates the selected notification read state and switches workspace focus.
fn jump_to_unread_notification(
    state: &State,
) -> Result<serde_json::Value, crate::control_bridge::BridgeError> {
    let notification_id = {
        let s = state.borrow();
        s.notifications
            .iter()
            .rev()
            .find(|notification| notification.unread)
            .map(|notification| notification.id)
    };
    let Some(notification_id) = notification_id else {
        return Err(crate::control_bridge::BridgeError::not_found(
            "unread notification not found",
        ));
    };
    open_host_notification(state, notification_id)
}

/// purpose: Clear one notification or the entire live-host notification inbox.
/// inputs: notification_id targets one item; None clears all.
/// returns/effects: Mutates inbox and affected workspace unread state, returns remaining rows.
fn clear_host_notifications(
    state: &State,
    notification_id: Option<u64>,
) -> Result<serde_json::Value, crate::control_bridge::BridgeError> {
    let rows = {
        let mut s = state.borrow_mut();
        let affected = if let Some(target_id) = notification_id {
            let affected = s
                .notifications
                .iter()
                .filter(|notification| notification.id == target_id)
                .map(|notification| notification.workspace_id.clone())
                .collect::<Vec<_>>();
            if affected.is_empty() {
                return Err(crate::control_bridge::BridgeError::not_found(
                    "notification not found",
                ));
            }
            s.notifications
                .retain(|notification| notification.id != target_id);
            affected
        } else {
            let affected = s
                .notifications
                .iter()
                .map(|notification| notification.workspace_id.clone())
                .collect::<Vec<_>>();
            s.notifications.clear();
            affected
        };

        let still_unread = s
            .notifications
            .iter()
            .filter(|notification| notification.unread)
            .map(|notification| notification.workspace_id.clone())
            .collect::<Vec<_>>();
        let cleared_count = affected.len();
        for workspace in &mut s.workspaces {
            if affected.contains(&workspace.id) && !still_unread.contains(&workspace.id) {
                clear_workspace_unread_visual(workspace);
            }
        }
        publish_notification_bulk_event("notification.cleared", cleared_count);
        s.notifications
            .iter()
            .map(host_notification_row)
            .collect::<Vec<_>>()
    };
    Ok(serde_json::json!({ "notifications": rows }))
}

// purpose: Decide whether notification state should add an unread visual marker.
// inputs: Whether the workspace is active and whether unread visual rings are enabled.
// returns/effects: Returns true for inactive workspaces when the visual setting is enabled.
fn should_show_unread_visual(workspace_is_active: bool, unread_pane_ring: bool) -> bool {
    !workspace_is_active && unread_pane_ring
}

// purpose: Decide whether CMUX sidebar notification text should accompany unread visuals.
// inputs: Current unread visual decision and sidebar message preference.
// returns/effects: Returns true only when both the visual and text setting are enabled.
fn should_show_sidebar_notification_message(
    unread_visual: bool,
    sidebar: &app_config::SidebarConfig,
) -> bool {
    unread_visual && !sidebar.hide_all_details && sidebar.show_notification_message
}

fn mark_workspace_unread_with_message(
    state: &State,
    ws_id: &str,
    message: &str,
    source_focused: bool,
    target: DesktopNotificationTarget,
    feed_actions: Vec<crate::feed::FeedNotificationAction>,
) -> Option<DesktopNotificationRequest> {
    let mut s = state.borrow_mut();
    let active_idx = s.active_idx;
    let window_active = s.window.is_active();
    let config = s.config.borrow().clone();
    let notifications = config.notifications;
    let sidebar = config.sidebar;
    if let Some((idx, ws)) = s
        .workspaces
        .iter_mut()
        .enumerate()
        .find(|(_, w)| w.id == ws_id)
    {
        let workspace_is_active = idx == active_idx;
        let desktop_request = should_emit_desktop_notification(
            notifications.enabled,
            window_active,
            workspace_is_active,
            source_focused,
            notifications.suppress_only_focused_surface,
        )
        .then(|| DesktopNotificationRequest {
            summary: ws.name.clone(),
            body: message.to_string(),
            sound: notifications.sound,
            custom_sound_file_path: notifications.custom_sound_file_path.clone(),
            target: target.clone(),
            feed_actions,
        });

        let show_unread_visual =
            should_show_unread_visual(workspace_is_active, notifications.unread_pane_ring);
        if show_unread_visual {
            ws.unread = true;
            ws.notify_dot.remove_css_class("limux-notify-dot-hidden");
            ws.notify_dot.add_css_class("limux-notify-dot");
            let show_message =
                should_show_sidebar_notification_message(show_unread_visual, &sidebar);
            ws.notify_label.set_visible(show_message);
            if show_message {
                ws.notify_label.set_label(message);
                ws.notify_label.remove_css_class("limux-notify-msg");
                ws.notify_label.add_css_class("limux-notify-msg-unread");
            }
            // Add glow pulse to the sidebar row box
            if let Some(row_box) = ws.sidebar_row.child() {
                row_box.add_css_class("limux-sidebar-row-unread");
            }
        }

        return desktop_request;
    }

    None
}

fn desktop_notification_hints(
    sound: app_config::NotificationSound,
    custom_sound_file_path: &str,
) -> HashMap<String, glib::Variant> {
    let mut hints = HashMap::from([("desktop-entry".to_string(), crate::APP_ID.to_variant())]);

    match sound {
        app_config::NotificationSound::Default => {}
        app_config::NotificationSound::CustomFile => {
            if !custom_sound_file_path.trim().is_empty() {
                hints.insert(
                    "sound-file".to_string(),
                    custom_sound_file_path.to_variant(),
                );
            }
        }
        app_config::NotificationSound::None => {
            hints.insert("suppress-sound".to_string(), true.to_variant());
        }
        _ => {
            if let Some(sound_name) = sound.freedesktop_sound_name() {
                let sound_variant = sound_name.to_variant();
                hints.insert("sound-name".to_string(), sound_variant.clone());
                hints.insert("x-canonical-sound-name".to_string(), sound_variant);
            }
        }
    }

    hints
}

fn desktop_notification_actions() -> Vec<String> {
    vec!["default".to_string(), "Open".to_string()]
}

// purpose: Build DBus action pairs and a Feed decision route map for one notification.
// inputs: Feed notification actions attached to the request.
// returns/effects: Returns freedesktop action key/label pairs and action-key decisions.
fn desktop_notification_action_entries(
    feed_actions: &[crate::feed::FeedNotificationAction],
) -> (
    Vec<String>,
    HashMap<String, crate::feed::FeedNotificationDecision>,
) {
    let mut actions = desktop_notification_actions();
    let mut routes = HashMap::new();
    for (index, action) in feed_actions.iter().enumerate() {
        let key = format!("feed-{index}");
        actions.push(key.clone());
        actions.push(action.label.clone());
        routes.insert(key, action.decision.clone());
    }
    (actions, routes)
}

fn show_desktop_notification(state: &State, request: DesktopNotificationRequest) {
    let state = state.clone();
    gio::DBusProxy::for_bus(
        gio::BusType::Session,
        gio::DBusProxyFlags::NONE,
        None::<&gio::DBusInterfaceInfo>,
        FREEDESKTOP_NOTIFICATIONS_SERVICE,
        FREEDESKTOP_NOTIFICATIONS_PATH,
        FREEDESKTOP_NOTIFICATIONS_INTERFACE,
        None::<&gio::Cancellable>,
        move |result| {
            let Ok(proxy) = result else {
                return;
            };
            let (actions, feed_actions) =
                desktop_notification_action_entries(&request.feed_actions);
            let route = DesktopNotificationRoute {
                target: request.target.clone(),
                activation_token: None,
                feed_actions,
            };

            let params = (
                "Limux",
                0u32,
                crate::APP_ID,
                request.summary.as_str(),
                request.body.as_str(),
                actions,
                desktop_notification_hints(request.sound, &request.custom_sound_file_path),
                DESKTOP_NOTIFICATION_EXPIRE_TIMEOUT_MS,
            )
                .to_variant();

            proxy.call(
                "Notify",
                Some(&params),
                gio::DBusCallFlags::NONE,
                DESKTOP_NOTIFICATION_DBUS_TIMEOUT_MS,
                None::<&gio::Cancellable>,
                move |result| {
                    let Ok(response) = result else {
                        return;
                    };
                    let Some(notification_id) = desktop_notification_id_from_response(&response)
                    else {
                        return;
                    };

                    state
                        .borrow_mut()
                        .desktop_notification_routes
                        .insert(notification_id, route.clone());
                },
            );
        },
    );
}

#[cfg(test)]
mod tests {
    use std::cell::RefCell;
    use std::io::{BufRead, BufReader};
    use std::os::unix::net::UnixStream;
    use std::rc::Rc;
    use std::thread;

    use super::glib;
    use super::gtk::ffi;
    use super::gtk::gdk;
    use super::ToVariant;
    use super::{
        browser_count_script, browser_element_action_script, browser_find_script,
        browser_required_element_script, browser_scroll_script, browser_snapshot_script,
        browser_styles_script, build_window_css, clamp_workspace_insert_index_for_pinning,
        desktop_notification_action_entries, desktop_notification_action_from_signal,
        desktop_notification_actions, desktop_notification_activation_token_from_signal,
        desktop_notification_closed_id_from_signal, desktop_notification_hints,
        desktop_notification_id_from_response, directional_neighbor_score, favorites_prefix_len,
        feed_exit_plan_action_specs, feed_question_action_specs, font_size_after_delta,
        ghostty_prefers_dark, gtk_system_prefers_dark_from_raw, host_notification_row,
        limit_text_to_last_lines, next_active_workspace_index, notification_command_env,
        notification_hook_policy_payload, notification_policy_effects_from_value,
        pane_create_split_placement, pending_exit_plan_request_id, pending_permission_request_id,
        pending_question_request_id, publish_browser_event, publish_surface_input_sent_event,
        publish_surface_key_sent_event, publish_surface_lifecycle_event,
        publish_workspace_lifecycle_event, queue_session_save_request,
        resolve_pane_create_source_id, resolve_workspace_creation_directory,
        resolved_system_prefers_dark, right_sidebar_mode_description, right_sidebar_mode_title,
        run_notification_hook_command, sanitize_background_opacity,
        shortcut_allowed_while_browser_find_active, shortcut_blocked_by_editable,
        shortcut_command_from_key_event, shortcut_dispatch_propagation,
        should_emit_desktop_notification, should_keep_workspace_open_after_empty_pane,
        should_show_sidebar_notification_message, should_show_unread_visual,
        sidebar_feed_preview_lines_from_value, sidebar_feed_visible_items,
        sidebar_file_preview_lines, sidebar_log_preview_lines_from_entries,
        sidebar_progress_preview_line, sidebar_status_preview_lines_from_entries,
        surface_input_event_payload, surface_key_event_payload, surface_lifecycle_event_payload,
        tab_drag_workspace_seed, use_opaque_window_background,
        validate_workspace_folder_input_with_dirs, workspace_drop_layout_path,
        workspace_folder_path_from_input, workspace_group_insert_index,
        workspace_hidden_by_collapsed_group_id, workspace_insert_index_for_placement,
        workspace_lifecycle_payload, workspace_notification_message, workspace_reordered_payload,
        workspace_title_from_directory, BrowserEvent, Direction, EditableCaptureContext,
        HostNotification, NeighborScore, NotificationPolicyContext, NotificationPolicyEffects,
        PaneBounds, PaneCreateDirection, PaneCreateTargetError, PortalColorSchemePreference,
        SessionSaveAccess, SessionSaveRequest, SidebarLogEntry, SidebarProgress,
        SidebarStatusEntry, WorkspaceEventSnapshot, WorkspaceSeedSource, BASE_CSS,
        HOST_ENTRY_CSS_CLASS, WORKSPACE_RENAME_ENTRY_CSS_CLASS, WORKSPACE_RENAME_ENTRY_CSS_CLASSES,
    };
    use crate::app_config::{NotificationSound, WorkspaceGroupNewPlacement};
    use crate::control_bridge::{BrowserAction, RightSidebarMode};
    use crate::layout_state::{
        LayoutNodeState, PaneState, SplitOrientation, SplitState, WorkspaceGroupState,
    };
    use crate::shortcut_config::{
        default_shortcuts, resolve_shortcuts_from_str, EditableCapturePolicy, ShortcutCommand,
    };
    use serde_json::json;
    #[derive(Default)]
    struct TestSessionSaveState {
        persistence_suspended: bool,
        save_queued: bool,
    }

    impl SessionSaveAccess for TestSessionSaveState {
        fn persistence_suspended(&self) -> bool {
            self.persistence_suspended
        }

        fn save_queued(&self) -> bool {
            self.save_queued
        }

        fn set_save_queued(&mut self, queued: bool) {
            self.save_queued = queued;
        }
    }

    #[test]
    fn favorites_prefix_len_counts_only_leading_favorites() {
        let flags = [true, true, false, true, false];
        assert_eq!(favorites_prefix_len(&flags), 2);
    }

    #[test]
    fn sanitize_background_opacity_clamps_invalid_values() {
        assert_eq!(sanitize_background_opacity(f64::NAN), 1.0);
        assert_eq!(sanitize_background_opacity(-0.2), 0.0);
        assert_eq!(sanitize_background_opacity(1.7), 1.0);
        assert_eq!(sanitize_background_opacity(0.42), 0.42);
    }

    #[test]
    fn transparent_window_background_only_applies_below_full_opacity() {
        assert!(!use_opaque_window_background(0.8));
        assert!(use_opaque_window_background(1.0));
        assert!(use_opaque_window_background(5.0));
        assert!(use_opaque_window_background(f64::NAN));
    }

    #[test]
    fn directional_neighbor_score_prefers_row_overlap_when_moving_left() {
        let current = PaneBounds {
            left: 100.0,
            top: 100.0,
            right: 200.0,
            bottom: 200.0,
        };
        let top_left = PaneBounds {
            left: 0.0,
            top: 0.0,
            right: 100.0,
            bottom: 100.0,
        };
        let bottom_left = PaneBounds {
            left: 0.0,
            top: 100.0,
            right: 100.0,
            bottom: 200.0,
        };

        let top_score =
            directional_neighbor_score(current, top_left, Direction::Left).expect("top score");
        let bottom_score = directional_neighbor_score(current, bottom_left, Direction::Left)
            .expect("bottom score");

        assert_eq!(
            top_score,
            NeighborScore {
                has_overlap: false,
                overlap: 0,
                gap: 0,
                center_delta: 100,
            }
        );
        assert_eq!(
            bottom_score,
            NeighborScore {
                has_overlap: true,
                overlap: 100,
                gap: 0,
                center_delta: 0,
            }
        );
    }

    #[test]
    fn directional_neighbor_score_prefers_column_overlap_when_moving_up() {
        let current = PaneBounds {
            left: 100.0,
            top: 100.0,
            right: 200.0,
            bottom: 200.0,
        };
        let top_left = PaneBounds {
            left: 0.0,
            top: 0.0,
            right: 100.0,
            bottom: 100.0,
        };
        let top_right = PaneBounds {
            left: 100.0,
            top: 0.0,
            right: 200.0,
            bottom: 100.0,
        };

        let left_score =
            directional_neighbor_score(current, top_left, Direction::Up).expect("left score");
        let right_score =
            directional_neighbor_score(current, top_right, Direction::Up).expect("right score");

        assert_eq!(left_score.overlap, 0);
        assert_eq!(right_score.overlap, 100);
        assert!(right_score.has_overlap);
    }

    #[test]
    fn pane_create_split_placement_maps_direction_to_orientation_and_order() {
        assert_eq!(
            pane_create_split_placement(PaneCreateDirection::Left),
            super::PaneCreateSplitPlacement {
                orientation: super::gtk::Orientation::Horizontal,
                new_pane_first: true,
            }
        );
        assert_eq!(
            pane_create_split_placement(PaneCreateDirection::Right),
            super::PaneCreateSplitPlacement {
                orientation: super::gtk::Orientation::Horizontal,
                new_pane_first: false,
            }
        );
        assert_eq!(
            pane_create_split_placement(PaneCreateDirection::Up),
            super::PaneCreateSplitPlacement {
                orientation: super::gtk::Orientation::Vertical,
                new_pane_first: true,
            }
        );
        assert_eq!(
            pane_create_split_placement(PaneCreateDirection::Down),
            super::PaneCreateSplitPlacement {
                orientation: super::gtk::Orientation::Vertical,
                new_pane_first: false,
            }
        );
    }

    #[test]
    fn pane_create_source_prefers_surface_then_pane_then_active_focus_then_first_leaf() {
        let panes = [10, 20, 30];
        let surfaces = [("10:aaa", 10), ("20:bbb", 20)];

        assert_eq!(
            resolve_pane_create_source_id(
                Some("surface:20:bbb"),
                Some(10),
                Some(30),
                true,
                &panes,
                &surfaces,
            ),
            Ok(20)
        );
        assert_eq!(
            resolve_pane_create_source_id(None, Some(10), Some(30), true, &panes, &surfaces),
            Ok(10)
        );
        assert_eq!(
            resolve_pane_create_source_id(None, None, Some(30), true, &panes, &surfaces),
            Ok(30)
        );
        assert_eq!(
            resolve_pane_create_source_id(None, None, Some(30), false, &panes, &surfaces),
            Ok(10)
        );
    }

    #[test]
    fn pane_create_source_reports_invalid_surface_pane_and_empty_workspace() {
        let panes = [10, 20];
        let surfaces = [("10:aaa", 10)];

        assert_eq!(
            resolve_pane_create_source_id(
                Some("missing"),
                Some(10),
                Some(20),
                true,
                &panes,
                &surfaces,
            ),
            Err(PaneCreateTargetError::InvalidSurfaceId(
                "missing".to_string()
            ))
        );
        assert_eq!(
            resolve_pane_create_source_id(None, Some(99), Some(20), true, &panes, &surfaces),
            Err(PaneCreateTargetError::InvalidPaneId(99))
        );
        assert_eq!(
            resolve_pane_create_source_id(None, None, None, true, &[], &[]),
            Err(PaneCreateTargetError::NoPanes)
        );
    }

    #[test]
    fn collapsed_workspace_groups_hide_only_inactive_non_anchor_members() {
        let groups = [WorkspaceGroupState {
            id: "group-1".to_string(),
            name: "Agents".to_string(),
            is_collapsed: true,
            is_pinned: false,
            anchor_workspace_id: Some("ws-anchor".to_string()),
            custom_color: None,
            icon_symbol: None,
        }];

        assert!(!workspace_hidden_by_collapsed_group_id(
            "ws-anchor",
            Some("group-1"),
            false,
            &groups
        ));
        assert!(!workspace_hidden_by_collapsed_group_id(
            "ws-member",
            Some("group-1"),
            true,
            &groups
        ));
        assert!(workspace_hidden_by_collapsed_group_id(
            "ws-member",
            Some("group-1"),
            false,
            &groups
        ));
        assert!(!workspace_hidden_by_collapsed_group_id(
            "ws-free", None, false, &groups
        ));
    }

    #[test]
    fn build_window_css_uses_resolved_background_opacity() {
        let css = build_window_css(0.42);
        assert!(css.contains(".limux-host-entry"));
        assert!(css.contains(".limux-host-entry text"));
        assert!(css.contains(".limux-host-entry text placeholder"));
        assert!(css.contains(".limux-content"));
        assert!(css.contains("background-color: rgba(23, 23, 23, 0.420);"));
    }

    #[test]
    fn font_size_after_delta_uses_default_when_unset() {
        assert_eq!(font_size_after_delta(None, 12.0, 1.0), 13.0);
    }

    #[test]
    fn font_size_after_delta_clamps_to_supported_range() {
        assert_eq!(font_size_after_delta(Some(1.0), 12.0, -5.0), 1.0);
        assert_eq!(font_size_after_delta(Some(255.0), 12.0, 5.0), 255.0);
    }

    #[test]
    fn base_css_defines_theme_aware_host_entry_styles() {
        assert!(BASE_CSS.contains(".limux-host-entry"));
        assert!(!BASE_CSS.contains("var("));
        assert!(BASE_CSS.contains(".limux-host-entry text"));
        assert!(BASE_CSS.contains(".limux-host-entry text placeholder"));
        assert!(BASE_CSS.contains("caret-color: currentColor;"));
    }

    #[test]
    fn workspace_rename_entry_uses_shared_host_entry_class() {
        assert_eq!(
            WORKSPACE_RENAME_ENTRY_CSS_CLASSES,
            [HOST_ENTRY_CSS_CLASS, WORKSPACE_RENAME_ENTRY_CSS_CLASS]
        );
        assert!(BASE_CSS.contains(".limux-ws-rename-entry"));
    }

    #[test]
    fn right_sidebar_preview_helpers_format_mode_and_progress() {
        assert_eq!(right_sidebar_mode_title(&RightSidebarMode::Find), "Find");
        assert!(
            right_sidebar_mode_description(&RightSidebarMode::Feed).contains("Feed"),
            "feed mode description should be readable in the rendered panel"
        );
        assert_eq!(
            sidebar_progress_preview_line(&SidebarProgress {
                value: 0.625,
                label: Some("Building".to_string()),
            }),
            "63% - Building"
        );
    }

    #[test]
    fn right_sidebar_status_preview_sorts_by_priority_then_key() {
        let entries = [
            SidebarStatusEntry {
                key: "test".to_string(),
                value: "queued".to_string(),
                icon: None,
                color: None,
                url: None,
                priority: 10,
            },
            SidebarStatusEntry {
                key: "build".to_string(),
                value: "running".to_string(),
                icon: None,
                color: None,
                url: None,
                priority: 80,
            },
            SidebarStatusEntry {
                key: "agent".to_string(),
                value: "waiting".to_string(),
                icon: None,
                color: None,
                url: None,
                priority: 80,
            },
        ];

        assert_eq!(
            sidebar_status_preview_lines_from_entries(entries.iter()),
            vec![
                "agent = waiting (80)".to_string(),
                "build = running (80)".to_string(),
                "test = queued (10)".to_string()
            ]
        );
    }

    #[test]
    fn right_sidebar_log_preview_limits_to_recent_entries() {
        let entries = (0..3)
            .map(|id| SidebarLogEntry {
                id,
                created_at: "2026-07-02T04:00:00Z".to_string(),
                level: "info".to_string(),
                source: Some("build".to_string()),
                message: format!("message {id}"),
            })
            .collect::<Vec<_>>();

        assert_eq!(
            sidebar_log_preview_lines_from_entries(&entries, 2),
            vec![
                "[info] build: message 1".to_string(),
                "[info] build: message 2".to_string()
            ]
        );
    }

    #[test]
    fn right_sidebar_file_preview_is_bounded_and_non_recursive() {
        let dir = tempfile::tempdir().expect("tempdir");
        std::fs::create_dir(dir.path().join("src")).expect("create folder");
        std::fs::write(dir.path().join("Cargo.toml"), "[package]\n").expect("write file");
        std::fs::write(dir.path().join("README.md"), "# test\n").expect("write file");

        let rows = sidebar_file_preview_lines(dir.path(), 2).expect("preview files");

        assert_eq!(rows.len(), 3);
        assert!(rows.iter().any(|row| row == "dir  src/"));
        assert!(rows.iter().any(|row| row == "... more entries"));
    }

    #[test]
    fn right_sidebar_feed_preview_formats_newest_rows_first() {
        let payload = json!({
            "items": [
                { "source": "codex", "kind": "PostToolUse", "status": "telemetry", "tool_name": "read" },
                {
                    "source": "claude",
                    "kind": "exitPlan",
                    "status": "pending",
                    "tool_name": "ExitPlanMode",
                    "request_id": "req-plan"
                },
                {
                    "source": "claude",
                    "kind": "question",
                    "status": "pending",
                    "tool_name": "AskUserQuestion",
                    "request_id": "req-question",
                    "tool_input": {
                        "questions": [
                            { "question": "Deploy?", "options": ["Yes", "No"] }
                        ]
                    }
                },
                {
                    "source": "codex",
                    "kind": "PermissionRequest",
                    "status": "pending",
                    "tool_name": "shell",
                    "request_id": "req-1"
                }
            ]
        });

        assert_eq!(
            sidebar_feed_preview_lines_from_value(&payload, 2),
            vec![
                "[pending] codex PermissionRequest: shell".to_string(),
                "[pending] claude question: AskUserQuestion".to_string()
            ]
        );
        let visible = sidebar_feed_visible_items(&payload, 4);
        assert_eq!(
            pending_permission_request_id(visible[0]).as_deref(),
            Some("req-1")
        );
        assert_eq!(
            pending_question_request_id(visible[1]).as_deref(),
            Some("req-question")
        );
        assert_eq!(
            feed_question_action_specs(visible[1]),
            vec![
                ("Yes".to_string(), vec!["Yes".to_string()]),
                ("No".to_string(), vec!["No".to_string()])
            ]
        );
        assert_eq!(
            pending_exit_plan_request_id(visible[2]).as_deref(),
            Some("req-plan")
        );
        assert_eq!(
            feed_exit_plan_action_specs(),
            &[
                ("Manual", "manual"),
                ("Auto", "autoAccept"),
                ("Bypass", "bypassPermissions"),
                ("Ultraplan", "ultraplan"),
                ("Deny", "deny")
            ]
        );
        assert_eq!(
            crate::feed_actions::permission_action_specs(
                visible[3]
                    .get("source")
                    .and_then(serde_json::Value::as_str)
                    .unwrap_or(""),
                visible[3]
            ),
            vec![
                ("Once", "once"),
                ("Always", "always"),
                ("Bypass", "bypass"),
                ("Deny", "deny")
            ]
        );
        assert_eq!(pending_permission_request_id(visible[3]), None);
    }

    // purpose: Verify right-sidebar Feed permission actions honor Codex app-server capabilities.
    // inputs: Pending Codex app-server Feed row with only amendment and decline decisions.
    // returns/effects: Asserts sidebar action policy exposes `all` and `deny`, not unsupported modes.
    #[test]
    fn right_sidebar_codex_app_server_permission_actions_follow_capabilities() {
        let item = json!({
            "source": "codex",
            "kind": "PermissionRequest",
            "status": "pending",
            "request_id": "req-app-server",
            "tool_input": {
                "app_server_method": "item/commandExecution/requestApproval",
                "available_decisions": [
                    {"acceptWithExecpolicyAmendment": {}},
                    "decline"
                ],
                "proposed_execpolicy_amendment": [{"kind": "prefix", "value": "cargo test"}]
            }
        });

        assert_eq!(
            crate::feed_actions::permission_action_specs("codex", &item),
            vec![("All", "all"), ("Deny", "deny")]
        );
    }

    #[test]
    fn desktop_notification_actions_include_default_open_action() {
        assert_eq!(
            desktop_notification_actions(),
            vec!["default".to_string(), "Open".to_string()]
        );
    }

    #[test]
    fn desktop_notification_hints_include_cmux_sound_and_custom_file() {
        let ping_hints = desktop_notification_hints(NotificationSound::Ping, "");
        assert_eq!(
            ping_hints.get("sound-name").and_then(|value| value.str()),
            Some("Ping")
        );

        let custom_hints =
            desktop_notification_hints(NotificationSound::CustomFile, "/tmp/notify.wav");
        assert_eq!(
            custom_hints.get("sound-file").and_then(|value| value.str()),
            Some("/tmp/notify.wav")
        );

        let silent_hints = desktop_notification_hints(NotificationSound::None, "");
        assert!(silent_hints.contains_key("suppress-sound"));
    }

    #[test]
    fn desktop_notification_action_entries_include_feed_decisions() {
        let (actions, routes) = desktop_notification_action_entries(&[
            crate::feed::FeedNotificationAction {
                label: "Once".to_string(),
                decision: crate::feed::FeedNotificationDecision::Permission {
                    request_id: "req-1".to_string(),
                    mode: "once".to_string(),
                },
            },
            crate::feed::FeedNotificationAction {
                label: "Deny".to_string(),
                decision: crate::feed::FeedNotificationDecision::Permission {
                    request_id: "req-1".to_string(),
                    mode: "deny".to_string(),
                },
            },
        ]);

        assert_eq!(
            actions,
            vec![
                "default".to_string(),
                "Open".to_string(),
                "feed-0".to_string(),
                "Once".to_string(),
                "feed-1".to_string(),
                "Deny".to_string()
            ]
        );
        assert_eq!(
            routes.get("feed-1"),
            Some(&crate::feed::FeedNotificationDecision::Permission {
                request_id: "req-1".to_string(),
                mode: "deny".to_string(),
            })
        );
    }

    #[test]
    fn desktop_notification_response_and_signal_parsers_match_dbus_shapes() {
        assert_eq!(
            desktop_notification_id_from_response(&(42u32,).to_variant()),
            Some(42)
        );
        assert_eq!(
            desktop_notification_action_from_signal(&(42u32, "default".to_string()).to_variant()),
            Some((42, "default".to_string()))
        );
        assert_eq!(
            desktop_notification_activation_token_from_signal(
                &(42u32, "token-123".to_string()).to_variant()
            ),
            Some((42, "token-123".to_string()))
        );
        assert_eq!(
            desktop_notification_closed_id_from_signal(&(42u32, 2u32).to_variant()),
            Some(42)
        );
    }

    #[test]
    fn queue_session_save_request_sets_queued_once() {
        let state = Rc::new(RefCell::new(TestSessionSaveState::default()));

        assert_eq!(
            queue_session_save_request(&state),
            SessionSaveRequest::FlushOnIdle
        );
        assert!(state.borrow().save_queued);
        assert_eq!(
            queue_session_save_request(&state),
            SessionSaveRequest::Ignore
        );
    }

    #[test]
    fn queue_session_save_request_retries_when_state_is_already_borrowed() {
        let state = Rc::new(RefCell::new(TestSessionSaveState::default()));
        let borrow = state.borrow_mut();

        assert_eq!(
            queue_session_save_request(&state),
            SessionSaveRequest::RetryOnIdle
        );

        drop(borrow);
        assert!(!state.borrow().save_queued);
    }

    #[test]
    fn unpinned_workspace_cannot_move_above_favorites() {
        // Remaining order after removing dragged workspace:
        // [fav, fav, unfav, unfav]
        let after_removal = [true, true, false, false];
        let clamped = clamp_workspace_insert_index_for_pinning(&after_removal, false, 0);
        assert_eq!(clamped, 2);
    }

    #[test]
    fn favorite_workspace_cannot_move_below_unpinned() {
        // Remaining order after removing dragged favorite:
        // [fav, fav, unfav, unfav]
        let after_removal = [true, true, false, false];
        let clamped =
            clamp_workspace_insert_index_for_pinning(&after_removal, true, after_removal.len());
        assert_eq!(clamped, 2);
    }

    // purpose: Verify the CMUX keep-workspace-open behavior only applies to the final closed surface.
    // inputs: App config, pane-empty reason, and remaining surface count.
    // returns/effects: Asserts non-last and moved-tab cases still use normal pane removal.
    #[test]
    fn keep_workspace_open_on_empty_pane_requires_last_closed_surface() {
        let mut config = crate::app_config::AppConfig::default();
        assert!(!should_keep_workspace_open_after_empty_pane(
            &config,
            crate::pane::PaneEmptyReason::ClosedLastTab,
            0
        ));

        config.app.keep_workspace_open_when_closing_last_surface = true;
        assert!(should_keep_workspace_open_after_empty_pane(
            &config,
            crate::pane::PaneEmptyReason::ClosedLastTab,
            0
        ));
        assert!(!should_keep_workspace_open_after_empty_pane(
            &config,
            crate::pane::PaneEmptyReason::ClosedLastTab,
            1
        ));
        assert!(!should_keep_workspace_open_after_empty_pane(
            &config,
            crate::pane::PaneEmptyReason::MovedLastTabOut,
            0
        ));
    }

    // purpose: Verify CMUX workspace cwd inheritance resolution for workspace creation.
    // inputs: App config, explicit cwd, and active workspace cwd snapshot.
    // returns/effects: Asserts explicit cwd wins and disabled inheritance returns None.
    #[test]
    fn workspace_creation_directory_follows_cmux_inheritance_setting() {
        let mut config = crate::app_config::AppConfig::default();

        assert_eq!(
            resolve_workspace_creation_directory(&config, Some("/explicit"), Some("/active")),
            Some("/explicit".to_string())
        );
        assert_eq!(
            resolve_workspace_creation_directory(&config, None, Some("/active")),
            Some("/active".to_string())
        );

        config.app.workspace_inherit_working_directory = false;
        assert_eq!(
            resolve_workspace_creation_directory(&config, None, Some("/active")),
            None
        );
        assert_eq!(
            resolve_workspace_creation_directory(&config, Some("/explicit"), Some("/active")),
            Some("/explicit".to_string())
        );
    }

    // purpose: Verify default workspace names when cwd is inherited or intentionally unset.
    // inputs: Optional effective workspace directory.
    // returns/effects: Asserts path basename or generic workspace title.
    #[test]
    fn workspace_title_from_directory_uses_basename_or_generic_title() {
        assert_eq!(
            workspace_title_from_directory(Some("/tmp/project")),
            "project"
        );
        assert_eq!(workspace_title_from_directory(None), "workspace");
    }

    #[test]
    fn system_prefers_dark_from_raw_maps_known_values() {
        assert_eq!(
            gtk_system_prefers_dark_from_raw(Some(ffi::GTK_INTERFACE_COLOR_SCHEME_DARK)),
            Some(true)
        );
        assert_eq!(
            gtk_system_prefers_dark_from_raw(Some(ffi::GTK_INTERFACE_COLOR_SCHEME_LIGHT)),
            Some(false)
        );
        assert_eq!(
            gtk_system_prefers_dark_from_raw(Some(ffi::GTK_INTERFACE_COLOR_SCHEME_DEFAULT)),
            Some(false)
        );
        assert_eq!(
            gtk_system_prefers_dark_from_raw(Some(ffi::GTK_INTERFACE_COLOR_SCHEME_UNSUPPORTED)),
            None
        );
    }

    #[test]
    fn portal_color_scheme_preference_resolves_with_gnome_fallback() {
        assert_eq!(
            PortalColorSchemePreference::from_raw(1),
            Some(PortalColorSchemePreference::Dark)
        );
        assert_eq!(
            PortalColorSchemePreference::from_raw(2),
            Some(PortalColorSchemePreference::Light)
        );
        assert_eq!(
            PortalColorSchemePreference::from_raw(0),
            Some(PortalColorSchemePreference::Default)
        );
        assert_eq!(
            resolved_system_prefers_dark(PortalColorSchemePreference::Dark, Some(false)),
            Some(true)
        );
        assert_eq!(
            resolved_system_prefers_dark(PortalColorSchemePreference::Light, Some(true)),
            Some(false)
        );
        assert_eq!(
            resolved_system_prefers_dark(PortalColorSchemePreference::Default, Some(true)),
            Some(true)
        );
        assert_eq!(
            resolved_system_prefers_dark(PortalColorSchemePreference::Unknown, Some(false)),
            Some(false)
        );
    }

    #[test]
    fn ghostty_prefers_dark_uses_system_preference_when_requested() {
        assert!(ghostty_prefers_dark(
            crate::app_config::ColorScheme::System,
            Some(true),
            false
        ));
        assert!(!ghostty_prefers_dark(
            crate::app_config::ColorScheme::System,
            Some(false),
            true
        ));
        assert!(ghostty_prefers_dark(
            crate::app_config::ColorScheme::System,
            None,
            true
        ));
    }

    #[test]
    fn ghostty_prefers_dark_honors_explicit_overrides() {
        assert!(ghostty_prefers_dark(
            crate::app_config::ColorScheme::Dark,
            Some(false),
            false
        ));
        assert!(!ghostty_prefers_dark(
            crate::app_config::ColorScheme::Light,
            Some(true),
            true
        ));
    }

    #[test]
    fn workspace_notification_message_prefers_title_and_body() {
        assert_eq!(
            workspace_notification_message("Codex", "Turn complete"),
            "Codex: Turn complete"
        );
        assert_eq!(workspace_notification_message("Codex", ""), "Codex");
        assert_eq!(
            workspace_notification_message("", "Turn complete"),
            "Turn complete"
        );
        assert_eq!(
            workspace_notification_message("  ", "  "),
            "Process needs attention"
        );
    }

    #[test]
    fn surface_io_payloads_redact_text_and_include_key_metadata() {
        let input_payload = surface_input_event_payload("workspace-a", "7:tab-a", Some(7), 42);

        assert_eq!(input_payload["workspace_id"], "workspace-a");
        assert_eq!(input_payload["workspace_ref"], "workspace:workspace-a");
        assert_eq!(input_payload["surface_id"], "7:tab-a");
        assert_eq!(input_payload["surface_ref"], "surface:7:tab-a");
        assert_eq!(input_payload["pane_id"], "7");
        assert_eq!(input_payload["pane_ref"], "pane:7");
        assert_eq!(input_payload["text_length"], 42);
        assert_eq!(
            input_payload["redacted_fields"],
            serde_json::json!(["text"])
        );
        assert!(input_payload.get("text").is_none());

        let key_payload = surface_key_event_payload("workspace-a", "7:tab-a", Some(7), "Enter");

        assert_eq!(key_payload["workspace_id"], "workspace-a");
        assert_eq!(key_payload["surface_id"], "7:tab-a");
        assert_eq!(key_payload["pane_id"], "7");
        assert_eq!(key_payload["key"], "Enter");
    }

    #[test]
    fn tab_action_payload_includes_created_closed_and_unread_metadata() {
        let surface = crate::pane::SurfaceSummary {
            pane_id: 7,
            surface_id: "7:tab-a".to_string(),
            title: "build".to_string(),
            kind: "browser".to_string(),
            selected: true,
            cwd: None,
            uri: Some("https://example.com".to_string()),
        };
        let created = crate::pane::SurfaceSummary {
            pane_id: 7,
            surface_id: "7:tab-b".to_string(),
            title: "build copy".to_string(),
            kind: "browser".to_string(),
            selected: true,
            cwd: None,
            uri: Some("https://example.com".to_string()),
        };
        let summary = crate::pane::TabActionSummary {
            surface,
            pinned: false,
            created: Some(created),
            closed: vec![crate::pane::SurfaceSummary {
                pane_id: 7,
                surface_id: "7:tab-c".to_string(),
                title: "old".to_string(),
                kind: "terminal".to_string(),
                selected: false,
                cwd: Some("/tmp".to_string()),
                uri: None,
            }],
            skipped_pinned: 1,
            reloaded: true,
        };

        let payload = super::tab_action_payload("workspace-a", "duplicate", &summary, Some(true));

        assert_eq!(payload["workspace_ref"], "workspace:workspace-a");
        assert_eq!(payload["surface_ref"], "surface:7:tab-a");
        assert_eq!(payload["tab_ref"], "tab:7:tab-a");
        assert_eq!(payload["created_surface_ref"], "surface:7:tab-b");
        assert_eq!(payload["created_tab_ref"], "tab:7:tab-b");
        assert_eq!(payload["closed"], 1);
        assert_eq!(payload["closed_surface_refs"], json!(["surface:7:tab-c"]));
        assert_eq!(payload["skipped_pinned"], 1);
        assert_eq!(payload["reloaded"], true);
        assert_eq!(payload["unread"], true);
    }

    #[test]
    fn limit_text_to_last_lines_preserves_line_endings() {
        assert_eq!(
            limit_text_to_last_lines("one\ntwo\nthree\n".to_string(), Some(2)),
            "two\nthree\n"
        );
        assert_eq!(
            limit_text_to_last_lines("one\ntwo\nthree".to_string(), Some(2)),
            "two\nthree"
        );
        assert_eq!(
            limit_text_to_last_lines("one\ntwo".to_string(), Some(5)),
            "one\ntwo"
        );
        assert_eq!(limit_text_to_last_lines("one".to_string(), None), "one");
    }

    #[test]
    fn surface_lifecycle_payload_includes_cmux_surface_metadata() {
        let surface = crate::pane::SurfaceSummary {
            pane_id: 11,
            surface_id: "11:tab-life".to_string(),
            title: "server".to_string(),
            kind: "terminal".to_string(),
            selected: true,
            cwd: Some("/tmp/project".to_string()),
            uri: None,
        };

        let payload = surface_lifecycle_event_payload(
            "workspace-life",
            &surface,
            serde_json::json!({ "origin": "test" }),
        );

        assert_eq!(payload["workspace_id"], "workspace-life");
        assert_eq!(payload["workspace_ref"], "workspace:workspace-life");
        assert_eq!(payload["surface_id"], "11:tab-life");
        assert_eq!(payload["surface_ref"], "surface:11:tab-life");
        assert_eq!(payload["pane_id"], "11");
        assert_eq!(payload["pane_ref"], "pane:11");
        assert_eq!(payload["surface_title"], "server");
        assert_eq!(payload["surface_type"], "terminal");
        assert_eq!(payload["selected"], true);
        assert_eq!(payload["cwd"], "/tmp/project");
        assert_eq!(payload["origin"], "test");
    }

    #[test]
    fn surface_lifecycle_publish_streams_cmux_event_frame() {
        let surface = crate::pane::SurfaceSummary {
            pane_id: 12,
            surface_id: "12:tab-created".to_string(),
            title: "created".to_string(),
            kind: "terminal".to_string(),
            selected: true,
            cwd: None,
            uri: None,
        };
        let seq = publish_surface_lifecycle_event(
            "surface.created",
            "workspace-surface-test",
            &surface,
            serde_json::json!({ "origin": "test" }),
        );

        let (mut writer, reader) = UnixStream::pair().expect("socket pair");
        let handle = thread::spawn(move || {
            crate::event_bus::bus().stream(
                &serde_json::json!({
                    "after_seq": seq.saturating_sub(1),
                    "name": "surface.created",
                    "category": "surface",
                    "include_heartbeats": false,
                }),
                &mut writer,
            )
        });

        let mut reader = BufReader::new(reader);
        let mut ack = String::new();
        reader.read_line(&mut ack).expect("read ack");
        let frame: serde_json::Value = serde_json::from_str(ack.trim()).expect("ack json");
        assert_eq!(frame["type"], "ack");

        let mut event = String::new();
        reader.read_line(&mut event).expect("read event");
        let frame: serde_json::Value = serde_json::from_str(event.trim()).expect("event json");
        assert_eq!(frame["type"], "event");
        assert_eq!(frame["name"], "surface.created");
        assert_eq!(frame["category"], "surface");
        assert_eq!(frame["source"], "surface.lifecycle");
        assert_eq!(frame["workspace_id"], "workspace-surface-test");
        assert_eq!(frame["surface_id"], "12:tab-created");
        assert_eq!(frame["pane_id"], "12");
        assert_eq!(frame["payload"]["origin"], "test");

        drop(reader);
        crate::event_bus::bus().publish(crate::event_bus::EventPublish {
            name: "surface.created",
            category: "surface",
            source: "test",
            workspace_id: Some(serde_json::Value::String("workspace-wakeup".to_string())),
            surface_id: Some(serde_json::Value::String("1:wakeup".to_string())),
            pane_id: Some(serde_json::Value::String("1".to_string())),
            payload: serde_json::json!({}),
        });
        let _ = handle.join().expect("event stream thread");
    }

    #[test]
    fn surface_input_publish_streams_redacted_cmux_event_frame() {
        let seq = publish_surface_input_sent_event("workspace-io-test", "9:tab-io", 13);

        let (mut writer, reader) = UnixStream::pair().expect("socket pair");
        let handle = thread::spawn(move || {
            crate::event_bus::bus().stream(
                &serde_json::json!({
                    "after_seq": seq.saturating_sub(1),
                    "name": "surface.input_sent",
                    "category": "surface",
                    "include_heartbeats": false,
                }),
                &mut writer,
            )
        });

        let mut reader = BufReader::new(reader);
        let mut ack = String::new();
        reader.read_line(&mut ack).expect("read ack");
        let frame: serde_json::Value = serde_json::from_str(ack.trim()).expect("ack json");
        assert_eq!(frame["type"], "ack");

        let mut event = String::new();
        reader.read_line(&mut event).expect("read event");
        let frame: serde_json::Value = serde_json::from_str(event.trim()).expect("event json");
        assert_eq!(frame["type"], "event");
        assert_eq!(frame["name"], "surface.input_sent");
        assert_eq!(frame["category"], "surface");
        assert_eq!(frame["source"], "surface.io");
        assert_eq!(frame["workspace_id"], "workspace-io-test");
        assert_eq!(frame["surface_id"], "9:tab-io");
        assert_eq!(frame["pane_id"], "9");
        assert_eq!(frame["payload"]["text_length"], 13);
        assert_eq!(
            frame["payload"]["redacted_fields"],
            serde_json::json!(["text"])
        );
        assert!(frame["payload"].get("text").is_none());

        drop(reader);
        crate::event_bus::bus().publish(crate::event_bus::EventPublish {
            name: "surface.input_sent",
            category: "surface",
            source: "test",
            workspace_id: Some(serde_json::Value::String("workspace-wakeup".to_string())),
            surface_id: Some(serde_json::Value::String("1:wakeup".to_string())),
            pane_id: Some(serde_json::Value::String("1".to_string())),
            payload: serde_json::json!({}),
        });
        let _ = handle.join().expect("event stream thread");
    }

    #[test]
    fn surface_key_publish_streams_cmux_event_frame() {
        let seq = publish_surface_key_sent_event("workspace-key-test", "10:tab-key", "Enter");

        let (mut writer, reader) = UnixStream::pair().expect("socket pair");
        let handle = thread::spawn(move || {
            crate::event_bus::bus().stream(
                &serde_json::json!({
                    "after_seq": seq.saturating_sub(1),
                    "name": "surface.key_sent",
                    "category": "surface",
                    "include_heartbeats": false,
                }),
                &mut writer,
            )
        });

        let mut reader = BufReader::new(reader);
        let mut ack = String::new();
        reader.read_line(&mut ack).expect("read ack");
        let frame: serde_json::Value = serde_json::from_str(ack.trim()).expect("ack json");
        assert_eq!(frame["type"], "ack");

        let mut event = String::new();
        reader.read_line(&mut event).expect("read event");
        let frame: serde_json::Value = serde_json::from_str(event.trim()).expect("event json");
        assert_eq!(frame["type"], "event");
        assert_eq!(frame["name"], "surface.key_sent");
        assert_eq!(frame["category"], "surface");
        assert_eq!(frame["source"], "surface.io");
        assert_eq!(frame["workspace_id"], "workspace-key-test");
        assert_eq!(frame["surface_id"], "10:tab-key");
        assert_eq!(frame["pane_id"], "10");
        assert_eq!(frame["payload"]["key"], "Enter");

        drop(reader);
        crate::event_bus::bus().publish(crate::event_bus::EventPublish {
            name: "surface.key_sent",
            category: "surface",
            source: "test",
            workspace_id: Some(serde_json::Value::String("workspace-wakeup".to_string())),
            surface_id: Some(serde_json::Value::String("1:wakeup".to_string())),
            pane_id: Some(serde_json::Value::String("1".to_string())),
            payload: serde_json::json!({}),
        });
        let _ = handle.join().expect("event stream thread");
    }

    #[test]
    fn browser_input_publishes_redacted_cmux_event() {
        let seq = publish_browser_event(BrowserEvent {
            name: "browser.input",
            workspace_id: "workspace-browser-test".to_string(),
            surface_id: "13:tab-browser".to_string(),
            pane_id: 13,
            payload: serde_json::json!({
                "workspace_id": "workspace-browser-test",
                "workspace_ref": "workspace:workspace-browser-test",
                "surface_id": "13:tab-browser",
                "surface_ref": "surface:13:tab-browser",
                "pane_id": "13",
                "pane_ref": "pane:13",
                "command": "browser.fill",
                "selector": "#token",
                "text_length": 9,
                "redacted_fields": ["text"],
            }),
        });

        assert!(seq > 0);
    }

    #[test]
    fn workspace_lifecycle_payload_includes_cmux_selection_fields() {
        let snapshot = WorkspaceEventSnapshot {
            workspace_id: "workspace-a".to_string(),
            workspace_ref: "workspace:workspace-a".to_string(),
            title: "Agents".to_string(),
            description: Some("Agent group".to_string()),
            index: 2,
            selected: true,
            favorite: false,
            group_id: Some("group-a".to_string()),
            tab_count: 3,
        };

        let payload = workspace_lifecycle_payload(
            &snapshot,
            Some("workspace-old"),
            serde_json::json!({ "origin": "test" }),
        );

        assert_eq!(payload["workspace_id"], "workspace-a");
        assert_eq!(payload["workspace_ref"], "workspace:workspace-a");
        assert_eq!(payload["title"], "Agents");
        assert_eq!(payload["description"], "Agent group");
        assert_eq!(payload["index"], 2);
        assert_eq!(payload["selected"], true);
        assert_eq!(payload["favorite"], false);
        assert_eq!(payload["group_id"], "group-a");
        assert_eq!(payload["tab_count"], 3);
        assert_eq!(payload["previous_workspace_id"], "workspace-old");
        assert_eq!(payload["previous_workspace_ref"], "workspace:workspace-old");
        assert_eq!(payload["origin"], "test");
    }

    #[test]
    fn workspace_group_insert_index_matches_cmux_placements() {
        let group_ids = [
            None,
            Some("group-a"),
            Some("group-a"),
            Some("group-b"),
            Some("group-a"),
        ];

        assert_eq!(
            workspace_group_insert_index(&group_ids, 2, None, "group-a", 4, "top"),
            1
        );
        assert_eq!(
            workspace_group_insert_index(&group_ids, 2, None, "group-a", 4, "end"),
            5
        );
        assert_eq!(
            workspace_group_insert_index(&group_ids, 2, Some(1), "group-a", 4, "afterCurrent"),
            2
        );
        assert_eq!(
            workspace_group_insert_index(&group_ids, 2, None, "group-a", 4, "afterCurrent"),
            3
        );
    }

    #[test]
    fn workspace_insert_index_matches_cmux_app_placements_with_pins() {
        let favorite_flags = [true, true, false, false, false, false];
        assert_eq!(
            workspace_insert_index_for_placement(
                &favorite_flags,
                Some(3),
                5,
                WorkspaceGroupNewPlacement::Top,
            ),
            2
        );
        assert_eq!(
            workspace_insert_index_for_placement(
                &favorite_flags,
                Some(3),
                5,
                WorkspaceGroupNewPlacement::End,
            ),
            6
        );
        assert_eq!(
            workspace_insert_index_for_placement(
                &favorite_flags,
                Some(3),
                5,
                WorkspaceGroupNewPlacement::AfterCurrent,
            ),
            4
        );
        assert_eq!(
            workspace_insert_index_for_placement(
                &favorite_flags,
                Some(1),
                5,
                WorkspaceGroupNewPlacement::AfterCurrent,
            ),
            2
        );
    }

    #[test]
    fn workspace_lifecycle_publish_streams_cmux_event_frame() {
        let snapshot = WorkspaceEventSnapshot {
            workspace_id: "workspace-stream-test".to_string(),
            workspace_ref: "workspace:workspace-stream-test".to_string(),
            title: "Stream Test".to_string(),
            description: None,
            index: 1,
            selected: true,
            favorite: false,
            group_id: None,
            tab_count: 2,
        };
        let seq = publish_workspace_lifecycle_event(
            "workspace.selected",
            &snapshot,
            Some("workspace-previous"),
            serde_json::json!({ "origin": "test" }),
        );

        let (mut writer, reader) = UnixStream::pair().expect("socket pair");
        let handle = thread::spawn(move || {
            crate::event_bus::bus().stream(
                &serde_json::json!({
                    "after_seq": seq.saturating_sub(1),
                    "name": "workspace.selected",
                    "category": "workspace",
                    "include_heartbeats": false,
                }),
                &mut writer,
            )
        });

        let mut reader = BufReader::new(reader);
        let mut ack = String::new();
        reader.read_line(&mut ack).expect("read ack");
        let frame: serde_json::Value = serde_json::from_str(ack.trim()).expect("ack json");
        assert_eq!(frame["type"], "ack");

        let mut event = String::new();
        reader.read_line(&mut event).expect("read event");
        let frame: serde_json::Value = serde_json::from_str(event.trim()).expect("event json");
        assert_eq!(frame["type"], "event");
        assert_eq!(frame["name"], "workspace.selected");
        assert_eq!(frame["category"], "workspace");
        assert_eq!(frame["source"], "workspace.lifecycle");
        assert_eq!(frame["workspace_id"], "workspace-stream-test");
        assert_eq!(
            frame["payload"]["previous_workspace_id"],
            "workspace-previous"
        );
        assert_eq!(frame["payload"]["tab_count"], 2);

        drop(reader);
        crate::event_bus::bus().publish(crate::event_bus::EventPublish {
            name: "workspace.selected",
            category: "workspace",
            source: "test",
            workspace_id: Some(serde_json::Value::String("workspace-wakeup".to_string())),
            surface_id: None,
            pane_id: None,
            payload: serde_json::json!({}),
        });
        let _ = handle.join().expect("event stream thread");
    }

    #[test]
    fn workspace_reordered_payload_includes_order_moved_pinned_and_count() {
        let payload = workspace_reordered_payload(
            vec!["workspace-a".to_string(), "workspace-b".to_string()],
            vec!["workspace-b".to_string()],
            vec!["workspace-a".to_string()],
            1,
        );

        assert_eq!(
            payload["workspace_ids"],
            serde_json::json!(["workspace-a", "workspace-b"])
        );
        assert_eq!(
            payload["moved_workspace_ids"],
            serde_json::json!(["workspace-b"])
        );
        assert_eq!(
            payload["pinned_workspace_ids"],
            serde_json::json!(["workspace-a"])
        );
        assert_eq!(payload["selected_workspace_index"], 1);
        assert_eq!(payload["count"], 2);
    }

    #[test]
    fn host_notification_row_includes_surface_and_created_metadata() {
        let notification = HostNotification {
            id: 7,
            workspace_id: "workspace-a".to_string(),
            surface_id: Some("3:tab-a".to_string()),
            pane_id: Some(3),
            tab_title: Some("Build".to_string()),
            created_at: "unix_ms:123".to_string(),
            title: "Codex".to_string(),
            subtitle: "Done".to_string(),
            body: "Turn complete".to_string(),
            message: "Codex: Turn complete".to_string(),
            unread: true,
        };

        let row = host_notification_row(&notification);

        assert_eq!(row["created_at"], "unix_ms:123");
        assert_eq!(row["surface_id"], "3:tab-a");
        assert_eq!(row["surface_ref"], "surface:3:tab-a");
        assert_eq!(row["pane_id"], "3");
        assert_eq!(row["pane_ref"], "pane:3");
        assert_eq!(row["tab_title"], "Build");
    }

    #[test]
    fn desktop_notifications_only_fire_for_background_workspaces() {
        assert!(should_emit_desktop_notification(
            true, false, false, false, false
        ));
        assert!(should_emit_desktop_notification(
            true, true, false, false, false
        ));
        assert!(!should_emit_desktop_notification(
            true, true, true, false, false
        ));
        assert!(!should_emit_desktop_notification(
            false, false, false, false, false
        ));
        assert!(!should_emit_desktop_notification(
            true, true, true, true, false
        ));
    }

    #[test]
    fn desktop_notifications_can_suppress_only_focused_surface() {
        assert!(should_emit_desktop_notification(
            true, true, true, false, true
        ));
        assert!(!should_emit_desktop_notification(
            true, true, true, true, true
        ));
    }

    #[test]
    fn notification_policy_payload_includes_current_effects_and_context() {
        let payload = notification_hook_policy_payload(
            "agent-filter",
            &NotificationPolicyContext {
                workspace_id: "workspace-a".to_string(),
                surface_id: Some("3:tab-a".to_string()),
                cwd: Some("/project".to_string()),
                title: "Codex".to_string(),
                subtitle: "Done".to_string(),
                body: "Turn complete".to_string(),
                app_focused: true,
                focused_panel: false,
            },
            NotificationPolicyEffects {
                record: false,
                mark_unread: true,
                desktop: false,
                sound: true,
                command: false,
                pane_flash: false,
            },
        );

        assert_eq!(payload["version"], 1);
        assert_eq!(payload["notification"]["workspaceId"], "workspace-a");
        assert_eq!(payload["notification"]["surfaceId"], "3:tab-a");
        assert_eq!(payload["context"]["cwd"], "/project");
        assert_eq!(payload["context"]["hookId"], "agent-filter");
        assert_eq!(payload["context"]["appFocused"], true);
        assert_eq!(payload["context"]["focusedPanel"], false);
        assert_eq!(payload["effects"]["record"], false);
        assert_eq!(payload["effects"]["markUnread"], true);
        assert_eq!(payload["effects"]["desktop"], false);
        assert_eq!(payload["effects"]["sound"], true);
        assert_eq!(payload["effects"]["command"], false);
        assert_eq!(payload["effects"]["paneFlash"], false);
    }

    #[test]
    fn notification_policy_effects_update_only_returned_fields() {
        let previous = NotificationPolicyEffects {
            record: true,
            mark_unread: true,
            desktop: true,
            sound: true,
            command: true,
            pane_flash: true,
        };

        let updated = notification_policy_effects_from_value(
            previous,
            &serde_json::json!({
                "effects": {
                    "record": false,
                    "markUnread": false,
                    "sound": false,
                    "command": false,
                    "paneFlash": false
                }
            }),
        )
        .expect("policy effects");

        assert_eq!(
            updated,
            NotificationPolicyEffects {
                record: false,
                mark_unread: false,
                desktop: true,
                sound: false,
                command: false,
                pane_flash: false,
            }
        );
    }

    #[test]
    fn notification_command_env_uses_cmux_and_limux_names() {
        let env = notification_command_env("Title", "Sub", "Body");

        assert!(env.contains(&("CMUX_NOTIFICATION_TITLE", "Title".to_string())));
        assert!(env.contains(&("CMUX_NOTIFICATION_SUBTITLE", "Sub".to_string())));
        assert!(env.contains(&("CMUX_NOTIFICATION_BODY", "Body".to_string())));
        assert!(env.contains(&("LIMUX_NOTIFICATION_TITLE", "Title".to_string())));
        assert!(env.contains(&("LIMUX_NOTIFICATION_SUBTITLE", "Sub".to_string())));
        assert!(env.contains(&("LIMUX_NOTIFICATION_BODY", "Body".to_string())));
    }

    #[test]
    fn notification_policy_effects_seed_pane_flash_from_config() {
        let config = crate::app_config::NotificationConfig {
            pane_flash: false,
            ..crate::app_config::NotificationConfig::default()
        };
        let effects = super::notification_policy_effects(
            &config,
            &NotificationPolicyContext {
                workspace_id: "workspace-a".to_string(),
                surface_id: Some("3:tab-a".to_string()),
                cwd: Some("/project".to_string()),
                title: "Codex".to_string(),
                subtitle: "Done".to_string(),
                body: "Turn complete".to_string(),
                app_focused: false,
                focused_panel: false,
            },
        )
        .expect("policy effects");

        assert!(!effects.pane_flash);
    }

    #[test]
    fn unread_visual_gate_respects_notification_setting() {
        assert!(should_show_unread_visual(false, true));
        assert!(!should_show_unread_visual(true, true));
        assert!(!should_show_unread_visual(false, false));
        let sidebar = crate::app_config::SidebarConfig::default();
        assert!(should_show_sidebar_notification_message(true, &sidebar));
        assert!(!should_show_sidebar_notification_message(false, &sidebar));
        assert!(!should_show_sidebar_notification_message(
            true,
            &crate::app_config::SidebarConfig {
                show_notification_message: false,
                ..crate::app_config::SidebarConfig::default()
            }
        ));
        assert!(!should_show_sidebar_notification_message(
            true,
            &crate::app_config::SidebarConfig {
                hide_all_details: true,
                ..crate::app_config::SidebarConfig::default()
            }
        ));
    }

    #[test]
    fn notification_policy_effects_reject_missing_effects_object() {
        let result = notification_policy_effects_from_value(
            NotificationPolicyEffects::default(),
            &serde_json::json!({ "ok": true }),
        );

        assert!(result.is_err());
    }

    #[test]
    fn notification_hook_command_closes_stdin_after_payload_write() {
        let hook = crate::app_config::NotificationHookConfig {
            id: "stdin-reader".to_string(),
            command: "cat >/dev/null; printf '{\"effects\":{\"desktop\":false}}'".to_string(),
            enabled: true,
            timeout_seconds: 1,
        };

        let output = run_notification_hook_command(&hook, &serde_json::json!({ "ok": true }))
            .expect("hook output");

        assert_eq!(output["effects"]["desktop"], false);
    }

    #[test]
    fn shortcut_command_from_key_event_uses_default_registry_bindings() {
        let shortcuts = default_shortcuts();

        assert_eq!(
            shortcut_command_from_key_event(
                &shortcuts,
                gdk::Key::T,
                gdk::ModifierType::CONTROL_MASK
            ),
            Some(ShortcutCommand::NewTerminal)
        );
        assert_eq!(
            shortcut_command_from_key_event(
                &shortcuts,
                gdk::Key::Page_Down,
                gdk::ModifierType::CONTROL_MASK
            ),
            Some(ShortcutCommand::NextWorkspace)
        );
        assert_eq!(
            shortcut_command_from_key_event(
                &shortcuts,
                gdk::Key::F,
                gdk::ModifierType::CONTROL_MASK
            ),
            Some(ShortcutCommand::SurfaceFind)
        );
        assert_eq!(
            shortcut_command_from_key_event(
                &shortcuts,
                gdk::Key::C,
                gdk::ModifierType::CONTROL_MASK
            ),
            None
        );
        assert_eq!(
            shortcut_command_from_key_event(
                &shortcuts,
                gdk::Key::C,
                gdk::ModifierType::CONTROL_MASK | gdk::ModifierType::SHIFT_MASK
            ),
            Some(ShortcutCommand::TerminalCopy)
        );
        assert_eq!(
            shortcut_command_from_key_event(
                &shortcuts,
                gdk::Key::Q,
                gdk::ModifierType::CONTROL_MASK
            ),
            Some(ShortcutCommand::QuitApp)
        );
        assert_eq!(
            shortcut_command_from_key_event(
                &shortcuts,
                gdk::Key::N,
                gdk::ModifierType::CONTROL_MASK | gdk::ModifierType::ALT_MASK
            ),
            Some(ShortcutCommand::NewInstance)
        );
        assert_eq!(
            shortcut_command_from_key_event(&shortcuts, gdk::Key::F11, gdk::ModifierType::empty()),
            Some(ShortcutCommand::ToggleFullscreen)
        );
        assert_eq!(
            shortcut_command_from_key_event(
                &shortcuts,
                gdk::Key::M,
                gdk::ModifierType::CONTROL_MASK
            ),
            Some(ShortcutCommand::ToggleSidebar)
        );
        assert_eq!(
            shortcut_command_from_key_event(
                &shortcuts,
                gdk::Key::M,
                gdk::ModifierType::CONTROL_MASK | gdk::ModifierType::SHIFT_MASK
            ),
            Some(ShortcutCommand::ToggleTopBar)
        );
    }

    #[test]
    fn shortcut_command_from_key_event_honors_remaps_and_disables_old_binding() {
        let shortcuts = resolve_shortcuts_from_str(
            r#"{
                "shortcuts": {
                    "toggle_sidebar": "<Ctrl><Alt>b"
                }
            }"#,
        )
        .unwrap();

        assert_eq!(
            shortcut_command_from_key_event(
                &shortcuts,
                gdk::Key::M,
                gdk::ModifierType::CONTROL_MASK
            ),
            None
        );
        assert_eq!(
            shortcut_command_from_key_event(
                &shortcuts,
                gdk::Key::B,
                gdk::ModifierType::CONTROL_MASK | gdk::ModifierType::ALT_MASK
            ),
            Some(ShortcutCommand::ToggleSidebar)
        );
    }

    #[test]
    fn shortcut_command_from_key_event_respects_explicit_unbinds() {
        let shortcuts = resolve_shortcuts_from_str(
            r#"{
                "shortcuts": {
                    "toggle_sidebar": null
                }
            }"#,
        )
        .unwrap();

        assert_eq!(
            shortcut_command_from_key_event(
                &shortcuts,
                gdk::Key::M,
                gdk::ModifierType::CONTROL_MASK
            ),
            None
        );
    }

    #[test]
    fn shortcut_command_from_key_event_honors_super_remaps() {
        let shortcuts = resolve_shortcuts_from_str(
            r#"{
                "shortcuts": {
                    "toggle_sidebar": "<Super>b"
                }
            }"#,
        )
        .unwrap();

        assert_eq!(
            shortcut_command_from_key_event(
                &shortcuts,
                gdk::Key::M,
                gdk::ModifierType::CONTROL_MASK
            ),
            None
        );
        assert_eq!(
            shortcut_command_from_key_event(&shortcuts, gdk::Key::B, gdk::ModifierType::SUPER_MASK),
            Some(ShortcutCommand::ToggleSidebar)
        );
    }

    #[test]
    fn shortcut_dispatch_propagation_stops_only_when_window_claims_shortcut() {
        assert_eq!(shortcut_dispatch_propagation(true), glib::Propagation::Stop);
        assert_eq!(
            shortcut_dispatch_propagation(false),
            glib::Propagation::Proceed
        );
    }

    #[test]
    fn shortcut_blocked_by_editable_only_bypasses_non_global_shortcuts() {
        assert!(shortcut_blocked_by_editable(
            ShortcutCommand::SurfaceFind,
            EditableCapturePolicy::BypassInEditable,
            EditableCaptureContext {
                gtk_editable: true,
                ..EditableCaptureContext::default()
            }
        ));
        assert!(!shortcut_blocked_by_editable(
            ShortcutCommand::SurfaceFind,
            EditableCapturePolicy::AlwaysCapture,
            EditableCaptureContext {
                gtk_editable: true,
                ..EditableCaptureContext::default()
            }
        ));
        assert!(!shortcut_blocked_by_editable(
            ShortcutCommand::SurfaceFind,
            EditableCapturePolicy::BypassInEditable,
            EditableCaptureContext::default()
        ));
    }

    #[test]
    fn shortcut_blocked_by_editable_blocks_dom_editable_browser_content() {
        assert!(shortcut_blocked_by_editable(
            ShortcutCommand::BrowserReload,
            EditableCapturePolicy::BypassInEditable,
            EditableCaptureContext {
                browser_dom_editable: true,
                ..EditableCaptureContext::default()
            }
        ));
    }

    #[test]
    fn browser_find_navigation_shortcuts_are_allowed_while_find_ui_is_active() {
        let context = EditableCaptureContext {
            gtk_editable: true,
            browser_find_active: true,
            ..EditableCaptureContext::default()
        };

        assert!(!shortcut_blocked_by_editable(
            ShortcutCommand::SurfaceFindNext,
            EditableCapturePolicy::BypassInEditable,
            context
        ));
        assert!(!shortcut_blocked_by_editable(
            ShortcutCommand::SurfaceFindPrevious,
            EditableCapturePolicy::BypassInEditable,
            context
        ));
        assert!(!shortcut_blocked_by_editable(
            ShortcutCommand::SurfaceFindHide,
            EditableCapturePolicy::BypassInEditable,
            context
        ));
        assert!(shortcut_blocked_by_editable(
            ShortcutCommand::SurfaceFind,
            EditableCapturePolicy::BypassInEditable,
            context
        ));
    }

    #[test]
    fn browser_find_active_exception_is_limited_to_navigation_shortcuts() {
        assert!(shortcut_allowed_while_browser_find_active(
            ShortcutCommand::SurfaceFindNext
        ));
        assert!(shortcut_allowed_while_browser_find_active(
            ShortcutCommand::SurfaceFindPrevious
        ));
        assert!(shortcut_allowed_while_browser_find_active(
            ShortcutCommand::SurfaceFindHide
        ));
        assert!(!shortcut_allowed_while_browser_find_active(
            ShortcutCommand::SurfaceFind
        ));
    }

    #[test]
    fn browser_element_ref_scripts_store_and_resolve_refs() {
        let snapshot = browser_snapshot_script(true, false, Some(2));
        assert!(snapshot.contains("limuxResetElementRefs();"));
        assert!(snapshot.contains("limuxStoreElementRef(node)"));

        let find = browser_find_script(&BrowserAction::Find {
            locator: "text".to_string(),
            selector: None,
            query: Some("Save".to_string()),
            role: None,
            name: None,
            index: None,
        });
        assert!(find.contains("const elementRef = limuxStoreElementRef(node);"));
        assert!(find.contains("element_ref: elementRef"));

        let action = browser_element_action_script("e1", "return { ok: true, selector };");
        assert!(action.contains("limuxResolveElement(target)"));
        assert!(action.contains("const element_ref = resolved.element_ref;"));

        let getter = browser_required_element_script("@e2", "node.textContent");
        assert!(getter.contains("const resolved = limuxResolveElement(target);"));

        let count = browser_count_script("@e3");
        assert!(count.contains("limuxNormalizeElementRef(target) !== null"));
        assert!(count.contains("return 1;"));

        let styles = browser_styles_script("e4", Some("color"));
        assert!(styles.contains("const resolved = limuxResolveElement(target);"));

        let scroll = browser_scroll_script(Some("e5"), 0, 10);
        assert!(scroll.contains("const resolved = limuxResolveElement(selector);"));
    }

    #[test]
    fn workspace_drop_layout_path_prefers_deterministic_startmost_leaf() {
        let layout = LayoutNodeState::Split(SplitState {
            orientation: SplitOrientation::Horizontal,
            ratio: 0.5,
            start: Box::new(LayoutNodeState::Split(SplitState {
                orientation: SplitOrientation::Vertical,
                ratio: 0.5,
                start: Box::new(LayoutNodeState::Pane(PaneState::fallback(Some("/a")))),
                end: Box::new(LayoutNodeState::Pane(PaneState::fallback(Some("/b")))),
            })),
            end: Box::new(LayoutNodeState::Pane(PaneState::fallback(Some("/c")))),
        });

        assert_eq!(workspace_drop_layout_path(&layout), vec![true, true]);
    }

    #[test]
    fn next_active_workspace_index_preserves_current_active_workspace() {
        let remaining = ["source-b", "destination", "other"];
        assert_eq!(
            next_active_workspace_index(&remaining, Some("destination"), 0),
            1
        );
    }

    #[test]
    fn next_active_workspace_index_falls_back_to_removed_slot_when_active_is_gone() {
        let remaining = ["left", "right"];
        assert_eq!(next_active_workspace_index(&remaining, Some("gone"), 1), 1);
    }

    #[test]
    fn tab_drag_workspace_seed_uses_terminal_cwd_for_folder_path() {
        let seed = tab_drag_workspace_seed(
            WorkspaceSeedSource {
                workspace_cwd: Some("/workspace".to_string()),
                workspace_folder_path: Some("/workspace".to_string()),
            },
            "Project Shell",
            Some("/project".to_string()),
        );

        assert_eq!(seed.name, "Project Shell");
        assert_eq!(seed.cwd.as_deref(), Some("/project"));
        assert_eq!(seed.folder_path.as_deref(), Some("/project"));
    }

    #[test]
    fn tab_drag_workspace_seed_uses_workspace_directory_for_non_terminal_tab() {
        let seed = tab_drag_workspace_seed(
            WorkspaceSeedSource {
                workspace_cwd: Some("/workspace-cwd".to_string()),
                workspace_folder_path: Some("/workspace-folder".to_string()),
            },
            "Browser",
            None,
        );

        assert_eq!(seed.name, "Browser");
        assert_eq!(seed.cwd.as_deref(), Some("/workspace-folder"));
        assert_eq!(seed.folder_path.as_deref(), Some("/workspace-folder"));
    }

    #[test]
    fn workspace_folder_path_input_expands_home_and_relative_paths() {
        let home = std::path::Path::new("/home/tester");
        let current = std::path::Path::new("/tmp/current");

        assert_eq!(
            workspace_folder_path_from_input("~/project", Some(home), Some(current)).unwrap(),
            std::path::PathBuf::from("/home/tester/project")
        );
        assert_eq!(
            workspace_folder_path_from_input("relative", Some(home), Some(current)).unwrap(),
            std::path::PathBuf::from("/tmp/current/relative")
        );
    }

    #[test]
    fn workspace_folder_path_input_rejects_empty_value() {
        assert_eq!(
            workspace_folder_path_from_input("  ", None, None).unwrap_err(),
            "Enter a folder path"
        );
    }

    #[test]
    fn workspace_folder_validation_accepts_existing_directory() {
        let dir = tempfile::tempdir().unwrap();
        let selection =
            validate_workspace_folder_input_with_dirs(dir.path().to_str().unwrap(), None, None)
                .unwrap();

        assert_eq!(selection.path_text, dir.path().to_string_lossy());
        assert_eq!(
            selection.name,
            dir.path().file_name().unwrap().to_string_lossy()
        );
    }

    #[test]
    fn workspace_folder_validation_rejects_files() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("not-a-folder");
        std::fs::write(&file, "content").unwrap();

        let error = validate_workspace_folder_input_with_dirs(file.to_str().unwrap(), None, None)
            .unwrap_err();

        assert!(error.ends_with(" is not a folder"));
    }
}
