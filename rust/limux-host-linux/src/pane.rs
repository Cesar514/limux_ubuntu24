//! PaneWidget: a tabbed container with action icons in the tab bar.
//!
//! Layout: [tab1 x] [tab2 x] ... ←spacer→ [terminal] [browser] [split-h] [split-v] [close]
//!
//! All on one line. Tabs left-justified, icons right-justified.

use std::cell::{Cell, RefCell};
use std::collections::VecDeque;
use std::fs;
use std::io;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::sync::atomic::{AtomicU32, Ordering};
use std::time::{Duration, Instant};

use gtk::glib;
#[allow(unused_imports)]
use gtk::prelude::*;
use gtk4 as gtk;
use serde_json::Value;
#[cfg(feature = "webkit")]
use webkit6::prelude::*;

use crate::app_config::AppConfig;
use crate::keybind_editor;
use crate::layout_state::{
    limux_cli_executable, PaneState, RestorableAgentState, TabContentState,
    TabState as SavedTabState,
};
use crate::settings_editor;
use crate::shortcut_config::{NormalizedShortcut, ResolvedShortcutConfig, ShortcutId};
use crate::terminal::{self, TerminalCallbacks};

static NEXT_PANE_ID: AtomicU32 = AtomicU32::new(1);
const BROWSER_DIAGNOSTIC_BUFFER_LIMIT: usize = 512;
const LIMUX_BROWSER_DIAGNOSTICS_HANDLER: &str = "limuxBrowserDiagnostics";
const CODEX_WRAPPER_SCRIPT: &str = r#"#!/bin/sh
set -eu
shim="$0"
shim_dir="${CMUX_CODEX_WRAPPER_SHIM_ROOT:-$(dirname "$shim")}"
old_ifs="$IFS"
IFS=:
clean_path=""
for path_entry in ${PATH:-}
do
  if [ "$path_entry" = "$shim_dir" ]
  then
    continue
  fi
  if [ -z "$clean_path" ]
  then
    clean_path="$path_entry"
  else
    clean_path="$clean_path:$path_entry"
  fi
done
IFS="$old_ifs"
PATH="$clean_path"
if ! real_codex="$(PATH="$PATH" command -v codex)"
then
  printf '%s\n' "limux: real codex executable not found after wrapper shim" >&2
  exit 127
fi
if [ "$real_codex" = "$shim" ]
then
  printf '%s\n' "limux: codex wrapper resolved to itself" >&2
  exit 127
fi
export LIMUX_AGENT_LAUNCH_EXECUTABLE="codex"
export CMUX_AGENT_LAUNCH_EXECUTABLE="codex"
export LIMUX_AGENT_LAUNCH_ARGV="codex $*"
export CMUX_AGENT_LAUNCH_ARGV="codex $*"
export LIMUX_AGENT_LAUNCH_CWD="$(pwd -P)"
export CMUX_AGENT_LAUNCH_CWD="$LIMUX_AGENT_LAUNCH_CWD"
surface="${LIMUX_SURFACE_ID:-${CMUX_SURFACE_ID:-}}"
workspace="${LIMUX_WORKSPACE_ID:-${CMUX_WORKSPACE_ID:-}}"
socket="${LIMUX_SOCKET:-${CMUX_SOCKET_PATH:-${CMUX_SOCKET:-}}}"
if [ -n "$socket" ] && [ -n "$surface" ]
then
  export LIMUX_AGENT_SESSION_ID="codex-wrapper-${surface}-$$"
  export CMUX_AGENT_SESSION_ID="$LIMUX_AGENT_SESSION_ID"
  export LIMUX_AGENT_PID="$$"
  export CMUX_AGENT_PID="$LIMUX_AGENT_PID"
  limux_cli="${LIMUX_CLI:-${CMUX_CLI:-limux}}"
  if [ -n "$workspace" ]
  then
    printf '{}\n' | "$limux_cli" --json hooks codex session-start --workspace "$workspace" --surface "$surface" >/dev/null 2>&1 || true
  else
    printf '{}\n' | "$limux_cli" --json hooks codex session-start --surface "$surface" >/dev/null 2>&1 || true
  fi
fi
exec "$real_codex" "$@"
"#;

fn next_pane_id() -> u32 {
    NEXT_PANE_ID.fetch_add(1, Ordering::Relaxed)
}

fn reserve_pane_id(id: u32) {
    let mut current = NEXT_PANE_ID.load(Ordering::Relaxed);
    while current <= id {
        match NEXT_PANE_ID.compare_exchange_weak(
            current,
            id.saturating_add(1),
            Ordering::Relaxed,
            Ordering::Relaxed,
        ) {
            Ok(_) => return,
            Err(updated) => current = updated,
        }
    }
}

fn pane_id_for_initial_state(initial_state: Option<&PaneState>) -> u32 {
    if let Some(id) = initial_state
        .and_then(|state| state.pane_id)
        .filter(|id| *id > 0)
    {
        reserve_pane_id(id);
        return id;
    }
    next_pane_id()
}

// purpose: Build a per-surface PATH shim that routes typed `codex` through Limux tracking.
// inputs: Surface id and current terminal environment vector.
// returns/effects: Writes an executable shim and appends CMUX/Limux wrapper variables.
fn install_codex_wrapper_env(surface_id: &str, extra_env: &mut Vec<(String, String)>) {
    let shim_root = codex_wrapper_root(surface_id);
    fs::create_dir_all(&shim_root).expect("failed to create Limux Codex wrapper directory");
    let shim_path = shim_root.join("codex");
    write_executable_file(&shim_path, CODEX_WRAPPER_SCRIPT)
        .expect("failed to write Limux Codex wrapper shim");
    prepend_env_path(extra_env, &shim_root);
    let shim = shim_path.to_string_lossy().to_string();
    let root = shim_root.to_string_lossy().to_string();
    extra_env.push(("CMUX_CODEX_WRAPPER_SHIM".to_string(), shim.clone()));
    extra_env.push(("LIMUX_CODEX_WRAPPER_SHIM".to_string(), shim));
    extra_env.push(("CMUX_CODEX_WRAPPER_SHIM_ROOT".to_string(), root.clone()));
    extra_env.push(("LIMUX_CODEX_WRAPPER_SHIM_ROOT".to_string(), root));
    let cli = limux_cli_executable();
    extra_env.push(("CMUX_CLI".to_string(), cli.clone()));
    extra_env.push(("LIMUX_CLI".to_string(), cli));
}

// purpose: Apply CMUX-compatible sidebar git and pull-request watch flags to terminal env.
// inputs: Current host config and mutable terminal environment vector.
// returns/effects: Adds managed CMUX_NO_GIT_WATCH and CMUX_NO_PR_WATCH values.
fn append_git_watch_env(config: &AppConfig, extra_env: &mut Vec<(String, String)>) {
    let watch_git = config.sidebar.watch_git_status;
    let show_pull_requests = config.sidebar.show_pull_requests;
    extra_env.push((
        "CMUX_NO_GIT_WATCH".to_string(),
        if watch_git { "" } else { "1" }.to_string(),
    ));
    extra_env.push((
        "CMUX_NO_PR_WATCH".to_string(),
        if watch_git && show_pull_requests {
            ""
        } else {
            "1"
        }
        .to_string(),
    ));
}

// purpose: Resolve the shim directory for one terminal surface.
// inputs: CMUX/Limux surface id.
// returns/effects: Returns a private temp path without touching disk.
fn codex_wrapper_root(surface_id: &str) -> PathBuf {
    let safe_surface = surface_id
        .chars()
        .map(|ch| if ch.is_ascii_alphanumeric() { ch } else { '-' })
        .collect::<String>();
    std::env::temp_dir()
        .join("limux-codex-wrapper")
        .join(safe_surface)
}

// purpose: Write an executable text file, replacing stale contents if present.
// inputs: Destination path and full script contents.
// returns/effects: Creates parent-owned executable file or returns an IO error.
fn write_executable_file(path: &Path, contents: &str) -> io::Result<()> {
    fs::write(path, contents)?;
    fs::set_permissions(path, fs::Permissions::from_mode(0o700))
}

// purpose: Prepend one directory to the terminal PATH environment.
// inputs: Mutable terminal env vector and directory to prepend.
// returns/effects: Adds or updates PATH for the spawned shell.
fn prepend_env_path(extra_env: &mut Vec<(String, String)>, directory: &Path) {
    let directory = directory.to_string_lossy();
    if let Some((_, path)) = extra_env.iter_mut().find(|(key, _)| key == "PATH") {
        *path = format!("{directory}:{path}");
        return;
    }
    let base_path = std::env::var("PATH").expect("PATH is required to install Codex wrapper shim");
    extra_env.push(("PATH".to_string(), format!("{directory}:{base_path}")));
}

type TabDragCallback = dyn Fn(bool);

thread_local! {
    static TAB_DRAGGING: Cell<bool> = const { Cell::new(false) };
    static TAB_DRAG_LISTENERS: RefCell<std::collections::HashMap<usize, Box<TabDragCallback>>> =
        RefCell::new(std::collections::HashMap::new());
    static TAB_DRAG_NEXT_ID: Cell<usize> = const { Cell::new(1) };
    static PANE_REGISTRY: RefCell<std::collections::HashMap<u32, std::rc::Weak<PaneInternals>>> =
        RefCell::new(std::collections::HashMap::new());
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct TabDragPayload {
    pane_id: u32,
    tab_id: String,
}

impl TabDragPayload {
    fn new(pane_id: u32, tab_id: impl Into<String>) -> Self {
        Self {
            pane_id,
            tab_id: tab_id.into(),
        }
    }

    fn encode(&self) -> String {
        format!("{}:{}", self.pane_id, self.tab_id)
    }

    fn decode(raw: &str) -> Option<Self> {
        let (pane_id, tab_id) = raw.split_once(':')?;
        if tab_id.is_empty() {
            return None;
        }
        Some(Self::new(pane_id.parse::<u32>().ok()?, tab_id))
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ContentDropZone {
    Center,
    Left,
    Right,
    Top,
    Bottom,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PaneEmptyReason {
    ClosedLastTab,
    MovedLastTabOut,
}

const HOST_ENTRY_CSS_CLASS: &str = "limux-host-entry";
const TAB_RENAME_ENTRY_CSS_CLASS: &str = "limux-tab-rename-entry";
const TAB_RENAME_ENTRY_CSS_CLASSES: [&str; 2] = [HOST_ENTRY_CSS_CLASS, TAB_RENAME_ENTRY_CSS_CLASS];
const BROWSER_URL_ENTRY_CSS_CLASS: &str = "limux-browser-url-entry";
const BROWSER_URL_ENTRY_CSS_CLASSES: [&str; 2] =
    [HOST_ENTRY_CSS_CLASS, BROWSER_URL_ENTRY_CSS_CLASS];
const BROWSER_SEARCH_ENTRY_CSS_CLASS: &str = "limux-browser-search-entry";
const BROWSER_SEARCH_ENTRY_CSS_CLASSES: [&str; 2] =
    [HOST_ENTRY_CSS_CLASS, BROWSER_SEARCH_ENTRY_CSS_CLASS];
#[cfg(feature = "webkit")]
const BROWSER_WEB_VIEW_CSS_CLASS: &str = "limux-browser-web-view";
pub(crate) const MIN_PANE_WIDTH: i32 = 260;
pub(crate) const MIN_PANE_HEIGHT: i32 = 160;

pub fn is_tab_dragging() -> bool {
    TAB_DRAGGING.with(|value| value.get())
}

pub fn on_tab_drag_change(callback: impl Fn(bool) + 'static) -> usize {
    TAB_DRAG_LISTENERS.with(|listeners| {
        let id = TAB_DRAG_NEXT_ID.with(|next| {
            let id = next.get();
            next.set(id + 1);
            id
        });
        listeners.borrow_mut().insert(id, Box::new(callback));
        id
    })
}

pub fn remove_tab_drag_listener(id: usize) {
    TAB_DRAG_LISTENERS.with(|listeners| {
        listeners.borrow_mut().remove(&id);
    });
}

fn set_tab_dragging(active: bool) {
    TAB_DRAGGING.with(|value| value.set(active));
    TAB_DRAG_LISTENERS.with(|listeners| {
        for callback in listeners.borrow().values() {
            callback(active);
        }
    });
}

fn register_pane(id: u32, internals: &Rc<PaneInternals>) {
    PANE_REGISTRY.with(|registry| {
        registry.borrow_mut().insert(id, Rc::downgrade(internals));
    });
}

fn unregister_pane(id: u32) {
    PANE_REGISTRY.with(|registry| {
        registry.borrow_mut().remove(&id);
    });
}

fn lookup_pane_internals(id: u32) -> Option<Rc<PaneInternals>> {
    PANE_REGISTRY.with(|registry| registry.borrow().get(&id)?.upgrade())
}

pub fn find_pane_widget_by_id(pane_id: u32) -> Option<gtk::Widget> {
    lookup_pane_internals(pane_id).map(|internals| internals.pane_outer.clone().upcast())
}

pub fn set_workspace_dragging_all(active: bool) {
    PANE_REGISTRY.with(|registry| {
        for weak in registry.borrow().values() {
            if let Some(internals) = weak.upgrade() {
                internals.workspace_dragging.set(active);
            }
        }
    });
}

// ---------------------------------------------------------------------------
// Types
// ---------------------------------------------------------------------------

type PaneSplitCallback = dyn Fn(&gtk::Widget, gtk::Orientation);
type PaneWidgetCallback = dyn Fn(&gtk::Widget);
type PaneSignalCallback = dyn Fn();
type PaneBellCallback = dyn Fn(bool, u32, &str);
type PanePathCallback = dyn Fn(&str);
type PaneDesktopNotificationCallback = dyn Fn(&str, &str, bool, u32, &str);
type PaneEmptyCallback = dyn Fn(&gtk::Widget, PaneEmptyReason);
type PaneOpenBrowserHereCallback = dyn Fn(&gtk::Widget);
type PaneShortcutStateCallback = dyn Fn() -> Rc<ResolvedShortcutConfig>;
type PaneShortcutCaptureCallback =
    dyn Fn(ShortcutId, Option<NormalizedShortcut>) -> Result<ResolvedShortcutConfig, String>;
type PaneSplitWithTabCallback = dyn Fn(&gtk::Widget, &gtk::Widget, gtk::Orientation, String, bool);
type PaneConfigCallback = dyn Fn() -> Rc<RefCell<AppConfig>>;
type PaneConfigChangedCallback = dyn Fn(&AppConfig, &AppConfig);
type PaneCustomSidebarBuilderCallback = dyn Fn(&str) -> gtk::Widget;
/// Returns the workspace id that owns a given pane widget, or `None` if the
/// pane is not yet attached to a workspace. Used to stamp `LIMUX_WORKSPACE_ID`
/// onto every terminal spawned inside the pane.
type PaneWorkspaceLookupCallback = dyn Fn(&gtk::Widget) -> Option<String>;
type PaneWorkspaceEnvironmentCallback = dyn Fn(&gtk::Widget) -> Vec<(String, String)>;

pub struct PaneCallbacks {
    pub on_split: Box<PaneSplitCallback>,
    pub on_close_pane: Box<PaneWidgetCallback>,
    pub on_bell: Box<PaneBellCallback>,
    pub on_desktop_notification: Box<PaneDesktopNotificationCallback>,
    pub on_open_browser_here: Box<PaneOpenBrowserHereCallback>,
    pub on_open_keybinds: Box<PaneWidgetCallback>,
    pub current_shortcuts: Box<PaneShortcutStateCallback>,
    pub on_capture_shortcut: Rc<PaneShortcutCaptureCallback>,
    pub on_pwd_changed: Box<PanePathCallback>,
    pub on_empty: Box<PaneEmptyCallback>,
    pub on_state_changed: Box<PaneSignalCallback>,
    pub on_split_with_tab: Box<PaneSplitWithTabCallback>,
    pub current_config: Box<PaneConfigCallback>,
    pub on_config_changed: Rc<PaneConfigChangedCallback>,
    pub build_custom_sidebar: Box<PaneCustomSidebarBuilderCallback>,
    /// Resolve the workspace id for a given pane widget. May be `None` while
    /// the pane is still being constructed; callers treat that as "unknown".
    pub workspace_for_pane: Box<PaneWorkspaceLookupCallback>,
    /// Resolve user-defined workspace environment for terminals in this pane.
    pub workspace_environment_for_pane: Box<PaneWorkspaceEnvironmentCallback>,
}

#[derive(Clone)]
struct TerminalTabState {
    cwd: Rc<RefCell<Option<String>>>,
    handle: terminal::TerminalHandle,
}

#[derive(Clone)]
pub struct TerminalShortcutTarget {
    handle: terminal::TerminalHandle,
}

impl TerminalShortcutTarget {
    pub fn perform_binding_action(&self, action: &str) -> bool {
        self.handle.perform_binding_action(action)
    }

    pub fn show_find(&self) -> bool {
        self.handle.show_find()
    }

    pub fn find_next(&self) -> bool {
        self.handle.find_next()
    }

    pub fn find_previous(&self) -> bool {
        self.handle.find_previous()
    }

    pub fn hide_find(&self) -> bool {
        self.handle.hide_find()
    }

    pub fn use_selection_for_find(&self) -> bool {
        self.handle.use_selection_for_find()
    }
}

#[derive(Clone)]
struct BrowserTabState {
    uri: Rc<RefCell<Option<String>>>,
    handles: BrowserHandles,
}

#[derive(Clone)]
pub struct BrowserShortcutTarget {
    uri: Rc<RefCell<Option<String>>>,
    handles: BrowserHandles,
}

#[derive(Clone)]
pub struct BrowserSurfaceTarget {
    pub surface: SurfaceSummary,
    target: BrowserShortcutTarget,
}

#[derive(Clone, Debug, PartialEq, Eq)]
// purpose: Report the file and dimensions produced by browser screenshot capture.
// inputs: Created by WebKit/GDK snapshot capture after writing a PNG file.
// returns/effects: Passed to the live control bridge response without retaining image bytes.
pub struct BrowserScreenshotResult {
    pub path: String,
    pub width: i32,
    pub height: i32,
    pub bytes: u64,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BrowserDialogResult {
    pub kind: String,
    pub message: String,
    pub default_text: Option<String>,
    pub accepted: bool,
    pub text: Option<String>,
}

#[derive(Clone, Debug, Default, PartialEq)]
struct BrowserDiagnosticsBuffer {
    console: VecDeque<Value>,
    errors: VecDeque<Value>,
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct BrowserDiagnosticsSnapshot {
    pub entries: Vec<Value>,
    pub count: usize,
}

// purpose: Append one diagnostic entry to a bounded ring without unbounded memory growth.
// inputs: Mutable diagnostic ring and JSON entry captured from WebKit.
// returns/effects: Drops the oldest entry when the ring reaches the configured cap.
fn push_bounded_diagnostic(entries: &mut VecDeque<Value>, entry: Value) {
    if entries.len() >= BROWSER_DIAGNOSTIC_BUFFER_LIMIT {
        entries.pop_front();
    }
    entries.push_back(entry);
}

// purpose: Store a WebKit browser diagnostic event in its matching retained buffer.
// inputs: Mutable per-browser buffers and a JSON event with a `kind` field.
// returns/effects: Appends console or error entries and ignores unknown diagnostic kinds.
fn push_browser_diagnostic(buffers: &mut BrowserDiagnosticsBuffer, event: Value) {
    match event.get("kind").and_then(Value::as_str) {
        Some("console") => push_bounded_diagnostic(&mut buffers.console, event),
        Some("error") => push_bounded_diagnostic(&mut buffers.errors, event),
        _ => {}
    }
}

// purpose: Clone a retained diagnostic ring into a stable response snapshot.
// inputs: Diagnostic ring for one browser surface.
// returns/effects: Returns entries in capture order and the exact count.
fn browser_diagnostics_snapshot(entries: &VecDeque<Value>) -> BrowserDiagnosticsSnapshot {
    BrowserDiagnosticsSnapshot {
        entries: entries.iter().cloned().collect(),
        count: entries.len(),
    }
}

impl BrowserSurfaceTarget {
    pub fn current_uri(&self) -> Option<String> {
        self.target.current_uri()
    }

    pub fn navigate(&self, url: &str) -> bool {
        self.target.navigate(url)
    }

    pub fn focus_content(&self) -> bool {
        self.target.focus_content()
    }

    pub fn go_back(&self) -> bool {
        self.target.go_back()
    }

    pub fn go_forward(&self) -> bool {
        self.target.go_forward()
    }

    pub fn reload(&self) -> bool {
        self.target.reload()
    }

    pub fn is_content_focused(&self) -> bool {
        self.target.is_content_focused()
    }

    pub fn evaluate_javascript<F>(&self, script: &str, callback: F) -> bool
    where
        F: FnOnce(Result<Value, String>) + 'static,
    {
        self.target.evaluate_javascript(script, callback)
    }

    // purpose: Select an iframe as the browser automation context.
    // inputs: CSS selector or frame id plus a completion callback.
    // returns/effects: Stores the selected frame only after WebKit confirms it exists.
    pub fn select_frame<F>(&self, selector: &str, callback: F) -> bool
    where
        F: FnOnce(Result<String, String>) + 'static,
    {
        self.target.select_frame(selector, callback)
    }

    pub fn reset_frame(&self) -> bool {
        self.target.reset_frame()
    }

    // purpose: Wait for a download target path to appear without busy spinning.
    // inputs: Optional destination path, timeout, and completion callback.
    // returns/effects: Polls locally on the GTK main loop and reports the resolved file path.
    pub fn wait_for_download<F>(&self, path: Option<PathBuf>, timeout_ms: u64, callback: F) -> bool
    where
        F: FnOnce(Result<PathBuf, String>) + 'static,
    {
        self.target.wait_for_download(path, timeout_ms, callback)
    }

    // purpose: Respond to the oldest pending JavaScript dialog on this browser surface.
    // inputs: Accept/dismiss flag, optional prompt text, and completion callback.
    // returns/effects: Unblocks WebKit's script dialog or reports an empty queue.
    pub fn respond_to_dialog<F>(&self, accept: bool, text: Option<String>, callback: F) -> bool
    where
        F: FnOnce(Result<BrowserDialogResult, String>) + 'static,
    {
        self.target.respond_to_dialog(accept, text, callback)
    }

    // purpose: Return retained browser console messages for CMUX diagnostics parity.
    // inputs: Addressed browser surface.
    // returns/effects: Clones the bounded console ring without mutating it.
    pub fn console_entries(&self) -> BrowserDiagnosticsSnapshot {
        self.target.console_entries()
    }

    // purpose: Clear retained browser console messages for CMUX diagnostics parity.
    // inputs: Addressed browser surface.
    // returns/effects: Empties the console ring and returns the number removed.
    pub fn clear_console_entries(&self) -> usize {
        self.target.clear_console_entries()
    }

    // purpose: Return retained page error messages for CMUX diagnostics parity.
    // inputs: Addressed browser surface.
    // returns/effects: Clones the bounded error ring without mutating it.
    pub fn error_entries(&self) -> BrowserDiagnosticsSnapshot {
        self.target.error_entries()
    }

    // purpose: Clear retained page error messages for CMUX diagnostics parity.
    // inputs: Addressed browser surface.
    // returns/effects: Empties the error ring and returns the number removed.
    pub fn clear_error_entries(&self) -> usize {
        self.target.clear_error_entries()
    }

    // purpose: Save the browser surface as a PNG screenshot.
    // inputs: Destination path, full-page capture flag, and completion callback.
    // returns/effects: Starts async WebKit capture when available and reports success through callback.
    pub fn save_screenshot<F>(&self, path: PathBuf, full_page: bool, callback: F) -> bool
    where
        F: FnOnce(Result<BrowserScreenshotResult, String>) + 'static,
    {
        self.target.save_screenshot(path, full_page, callback)
    }
}

#[derive(Clone)]
pub enum FocusedShortcutTarget {
    None,
    Terminal(TerminalShortcutTarget),
    Browser(BrowserShortcutTarget),
    Keybinds,
}

#[derive(Clone)]
struct TabContextMenuContext {
    tab_strip: gtk::Box,
    content_stack: gtk::Stack,
    tab_state: Rc<RefCell<TabState>>,
    callbacks: Rc<PaneCallbacks>,
    pane_outer: gtk::Box,
    label: gtk::Label,
    pin_icon: gtk::Label,
}

// ---------------------------------------------------------------------------
// CSS
// ---------------------------------------------------------------------------

pub const PANE_CSS: &str = r#"
.limux-pane-header {
    background-color: @window_bg_color;
    color: @window_fg_color;
    border-bottom: 1px solid alpha(@window_fg_color, 0.08);
    min-height: 30px;
    padding: 0 2px;
}
.limux-tab {
    background: none;
    border: none;
    border-radius: 4px 4px 0 0;
    padding: 4px 4px 4px 10px;
    color: alpha(@window_fg_color, 0.5);
    min-height: 0;
    font-size: 12px;
}
.limux-tab:hover {
    color: alpha(@window_fg_color, 0.72);
    background: alpha(@window_fg_color, 0.04);
}
.limux-tab-active {
    color: @window_fg_color;
    background: alpha(@window_fg_color, 0.08);
}
.limux-tab-close {
    background: none;
    border: none;
    border-radius: 3px;
    padding: 1px;
    min-height: 0;
    min-width: 0;
    color: alpha(@window_fg_color, 0.28);
    margin-left: 4px;
}
.limux-tab-close:hover {
    color: alpha(@window_fg_color, 0.8);
    background: alpha(@window_fg_color, 0.1);
}
.limux-pane-action {
    background: none;
    border: none;
    border-radius: 4px;
    padding: 4px 5px;
    min-height: 0;
    min-width: 0;
    color: alpha(@window_fg_color, 0.4);
}
.limux-pane-action:hover {
    background: alpha(@window_fg_color, 0.08);
    color: alpha(@window_fg_color, 0.8);
}
.limux-split-icon {
    border: 1px solid alpha(@window_fg_color, 0.4);
    border-radius: 2px;
    min-width: 16px;
    min-height: 12px;
    padding: 0;
}
.limux-split-icon:hover {
    border-color: alpha(@window_fg_color, 0.8);
}
.limux-split-half-v {
    min-width: 6px;
    min-height: 10px;
}
.limux-split-half-h {
    min-width: 14px;
    min-height: 4px;
}
.limux-split-btn {
    background: none;
    border: none;
    border-radius: 4px;
    padding: 4px 5px;
    min-height: 0;
    min-width: 0;
}
.limux-split-btn:hover {
    background: alpha(@window_fg_color, 0.08);
}
.limux-pin-icon {
    font-size: 9px;
    margin-right: 2px;
}
.limux-tab-rename-entry {
    padding: 1px 4px;
    min-height: 0;
    font-size: 12px;
}
.limux-browser-url-entry {
    min-height: 0;
    font-size: 12px;
}
.limux-browser-search-entry {
    min-height: 0;
    font-size: 12px;
}
.limux-browser,
.limux-browser-web-view {
    min-width: 0;
    min-height: 0;
}
.limux-tab-drop-indicator {
    background-color: @accent_bg_color;
    min-width: 2px;
    margin: 2px 0;
}
.limux-tab-overlay:drop(active) {
    box-shadow: none;
}
.limux-drop-preview {
    background: alpha(@accent_bg_color, 0.24);
    border: 1px solid alpha(@accent_bg_color, 0.65);
    border-radius: 10px;
}
.limux-drop-preview-center {
    background: alpha(@accent_bg_color, 0.14);
}
"#;

// ---------------------------------------------------------------------------
// PaneWidget builder
// ---------------------------------------------------------------------------

pub fn create_pane(
    callbacks: Rc<PaneCallbacks>,
    shortcuts: Rc<ResolvedShortcutConfig>,
    working_directory: Option<&str>,
    initial_state: Option<&PaneState>,
    skip_default_tab: bool,
) -> gtk::Box {
    let outer = gtk::Box::builder()
        .orientation(gtk::Orientation::Vertical)
        .hexpand(true)
        .vexpand(true)
        .build();
    outer.set_size_request(MIN_PANE_WIDTH, MIN_PANE_HEIGHT);

    // The single header line: tabs (left) + action icons (right)
    let header = gtk::Box::builder()
        .orientation(gtk::Orientation::Horizontal)
        .spacing(0)
        .build();
    header.add_css_class("limux-pane-header");

    let tab_overlay = gtk::Overlay::new();
    tab_overlay.add_css_class("limux-tab-overlay");
    tab_overlay.set_hexpand(true);

    let tab_strip = gtk::Box::builder()
        .orientation(gtk::Orientation::Horizontal)
        .spacing(0)
        .hexpand(true)
        .build();
    tab_overlay.set_child(Some(&tab_strip));

    let drop_indicator = gtk::Box::new(gtk::Orientation::Vertical, 0);
    drop_indicator.add_css_class("limux-tab-drop-indicator");
    drop_indicator.set_halign(gtk::Align::Start);
    drop_indicator.set_valign(gtk::Align::Fill);
    drop_indicator.set_visible(false);
    tab_overlay.add_overlay(&drop_indicator);
    tab_overlay.set_clip_overlay(&drop_indicator, false);

    let content_stack = gtk::Stack::new();
    content_stack.set_transition_type(gtk::StackTransitionType::None);
    content_stack.set_hexpand(true);
    content_stack.set_vexpand(true);

    let content_overlay = gtk::Overlay::new();
    content_overlay.set_hexpand(true);
    content_overlay.set_vexpand(true);
    content_overlay.set_child(Some(&content_stack));

    let content_drop_overlay = gtk::Box::new(gtk::Orientation::Horizontal, 0);
    content_drop_overlay.set_halign(gtk::Align::Start);
    content_drop_overlay.set_valign(gtk::Align::Start);
    content_drop_overlay.set_visible(false);
    content_drop_overlay.set_can_target(false);
    content_overlay.add_overlay(&content_drop_overlay);

    // Action icons (right side)
    let actions = gtk::Box::builder()
        .orientation(gtk::Orientation::Horizontal)
        .spacing(1)
        .build();

    let new_term_btn = icon_button(
        "utilities-terminal-symbolic",
        &pane_action_tooltip(
            &shortcuts,
            "New terminal tab",
            Some(ShortcutId::NewTerminal),
        ),
    );
    let new_browser_btn = icon_button(
        "limux-globe-symbolic",
        &pane_action_tooltip(&shortcuts, "New browser tab", None),
    );
    let split_h_btn = icon_button(
        "limux-split-horizontal-symbolic",
        &pane_action_tooltip(&shortcuts, "Split right", Some(ShortcutId::SplitRight)),
    );
    let split_v_btn = icon_button(
        "limux-split-vertical-symbolic",
        &pane_action_tooltip(&shortcuts, "Split down", Some(ShortcutId::SplitDown)),
    );
    let settings_btn = icon_button("emblem-system-symbolic", "Settings");
    let close_btn = icon_button(
        "window-close-symbolic",
        &pane_action_tooltip(&shortcuts, "Close pane", Some(ShortcutId::CloseFocusedPane)),
    );

    actions.append(&new_term_btn);
    actions.append(&new_browser_btn);
    actions.append(&split_h_btn);
    actions.append(&split_v_btn);
    actions.append(&settings_btn);
    actions.append(&close_btn);

    header.append(&tab_overlay);
    header.append(&actions);

    outer.append(&header);
    outer.append(&content_overlay);

    let ws_wd = Rc::new(RefCell::new(
        working_directory.map(|value| value.to_string()),
    ));
    let tab_state = Rc::new(RefCell::new(TabState {
        tabs: Vec::new(),
        active_tab: None,
    }));
    let workspace_dragging = Rc::new(Cell::new(false));
    let pane_id = pane_id_for_initial_state(initial_state);
    let internals = Rc::new(PaneInternals {
        pane_id,
        tab_state: tab_state.clone(),
        tab_strip: tab_strip.clone(),
        content_stack: content_stack.clone(),
        drop_indicator: drop_indicator.clone(),
        content_drop_overlay: content_drop_overlay.clone(),
        pane_outer: outer.clone(),
        callbacks: callbacks.clone(),
        working_directory: ws_wd.clone(),
        workspace_dragging: workspace_dragging.clone(),
        new_terminal_button: new_term_btn.clone(),
        split_right_button: split_h_btn.clone(),
        split_down_button: split_v_btn.clone(),
        close_pane_button: close_btn.clone(),
    });

    if let Some(saved_state) = initial_state {
        restore_tabs_from_state(&internals, working_directory, saved_state);
    } else if !skip_default_tab {
        add_terminal_tab_inner(&internals, working_directory, None);
    }

    {
        let internals = internals.clone();
        let wd = ws_wd.clone();
        new_term_btn.connect_clicked(move |_| {
            let dir = wd.borrow().clone();
            add_terminal_tab_inner(&internals, dir.as_deref(), None);
        });
    }
    {
        let internals = internals.clone();
        new_browser_btn.connect_clicked(move |_| {
            add_browser_tab_inner(&internals, None);
        });
    }
    {
        let pw = outer.clone();
        let cb = callbacks.clone();
        split_h_btn.connect_clicked(move |_| {
            (cb.on_split)(&pw.clone().upcast(), gtk::Orientation::Horizontal);
        });
    }
    {
        let pw = outer.clone();
        let cb = callbacks.clone();
        split_v_btn.connect_clicked(move |_| {
            (cb.on_split)(&pw.clone().upcast(), gtk::Orientation::Vertical);
        });
    }
    {
        let pw = outer.clone();
        let cb = callbacks.clone();
        close_btn.connect_clicked(move |_| {
            (cb.on_close_pane)(&pw.clone().upcast());
        });
    }
    {
        let internals = internals.clone();
        settings_btn.connect_clicked(move |_| {
            settings_editor::present_settings_dialog(
                &internals.pane_outer,
                settings_editor::SettingsEditorInput {
                    config: (internals.callbacks.current_config)(),
                    shortcuts: (internals.callbacks.current_shortcuts)(),
                    initial_page: None,
                    on_capture: internals.callbacks.on_capture_shortcut.clone(),
                    on_config_changed: internals.callbacks.on_config_changed.clone(),
                },
            );
        });
    }

    install_tab_strip_drop_target(&tab_overlay, &internals);
    install_content_drop_target(&internals);

    register_pane(pane_id, &internals);
    unsafe {
        outer.set_data("limux-pane-internals", internals);
    }
    outer.connect_destroy(move |_| {
        unregister_pane(pane_id);
    });

    outer
}

/// Cycle tabs in the focused pane. `delta`: 1 = next, -1 = prev.
pub fn cycle_tab_in_pane(pane_widget: &gtk::Widget, delta: i32) {
    let outer = pane_widget.downcast_ref::<gtk::Box>();
    let outer = match outer {
        Some(o) => o,
        None => return,
    };
    let internals: Rc<PaneInternals> = unsafe {
        match outer.data::<Rc<PaneInternals>>("limux-pane-internals") {
            Some(ptr) => ptr.as_ref().clone(),
            None => return,
        }
    };

    let ts = internals.tab_state.borrow();
    let len = ts.tabs.len();
    if len <= 1 {
        return;
    }

    let active_idx = ts
        .active_tab
        .as_ref()
        .and_then(|id| ts.tabs.iter().position(|e| e.id == *id))
        .unwrap_or(0);

    let new_idx = (active_idx as i32 + delta).rem_euclid(len as i32) as usize;
    let new_id = ts.tabs[new_idx].id.clone();
    drop(ts);

    activate_tab(
        &internals.tab_strip,
        &internals.content_stack,
        &internals.tab_state,
        &new_id,
    );
    (internals.callbacks.on_state_changed)();
}

pub fn focus_active_tab_in_pane(pane_widget: &gtk::Widget) -> bool {
    let Some(internals) = find_pane_internals(pane_widget) else {
        return false;
    };

    let target_tab_id = {
        let tab_state = internals.tab_state.borrow();
        tab_state
            .active_tab
            .clone()
            .or_else(|| tab_state.tabs.first().map(|entry| entry.id.clone()))
    };

    let Some(tab_id) = target_tab_id else {
        return false;
    };

    activate_tab(
        &internals.tab_strip,
        &internals.content_stack,
        &internals.tab_state,
        &tab_id,
    );
    true
}

pub fn refresh_terminal_displays_in_root(root: &gtk::Widget) {
    for internals in pane_internals_for_root(root) {
        for entry in &internals.tab_state.borrow().tabs {
            if let TabKind::Terminal { state } = &entry.kind {
                state.handle.refresh_display();
            }
        }
    }
}

pub fn activate_tab_in_pane(pane_widget: &gtk::Widget, tab_id: &str) -> bool {
    let Some(internals) = find_pane_internals(pane_widget) else {
        return false;
    };

    let has_tab = internals
        .tab_state
        .borrow()
        .tabs
        .iter()
        .any(|entry| entry.id == tab_id);
    if !has_tab {
        return false;
    }

    activate_tab(
        &internals.tab_strip,
        &internals.content_stack,
        &internals.tab_state,
        tab_id,
    );
    true
}

fn normalize_surface_hint(raw: &str) -> &str {
    raw.trim()
        .strip_prefix("surface:")
        .unwrap_or_else(|| raw.trim())
}

fn composite_surface_id(pane_id: u32, tab_id: &str) -> String {
    format!("{pane_id}:{tab_id}")
}

fn surface_hint_matches(surface_id: &str, tab_id: &str, surface_hint: &str) -> bool {
    let requested = normalize_surface_hint(surface_hint);
    !requested.is_empty() && (requested == tab_id || requested == surface_id)
}

pub fn terminal_handle_for_surface(
    pane_widget: &gtk::Widget,
    surface_hint: Option<&str>,
) -> Option<(String, terminal::TerminalHandle)> {
    let internals = find_pane_internals(pane_widget)?;
    let pane_id = internals.pane_id;
    let tab_state = internals.tab_state.borrow();
    let requested = surface_hint
        .map(normalize_surface_hint)
        .filter(|value| !value.is_empty());
    let active_tab = tab_state.active_tab.as_deref();
    let mut fallback = None;

    for entry in &tab_state.tabs {
        let TabKind::Terminal { state } = &entry.kind else {
            continue;
        };

        let full_surface_id = composite_surface_id(pane_id, &entry.id);

        if requested.is_some_and(|value| value == entry.id || value == full_surface_id) {
            return Some((full_surface_id, state.handle.clone()));
        }

        if requested.is_some() {
            continue;
        }

        if active_tab == Some(entry.id.as_str()) {
            return Some((full_surface_id, state.handle.clone()));
        }

        if fallback.is_none() {
            fallback = Some((full_surface_id, state.handle.clone()));
        }
    }

    fallback
}

pub fn exact_terminal_handle_for_surface(
    pane_widget: &gtk::Widget,
    surface_hint: &str,
) -> Option<(String, terminal::TerminalHandle)> {
    let internals = find_pane_internals(pane_widget)?;
    let pane_id = internals.pane_id;
    let tab_state = internals.tab_state.borrow();

    for entry in &tab_state.tabs {
        let TabKind::Terminal { state } = &entry.kind else {
            continue;
        };

        let full_surface_id = composite_surface_id(pane_id, &entry.id);
        if surface_hint_matches(&full_surface_id, &entry.id, surface_hint) {
            return Some((full_surface_id, state.handle.clone()));
        }
    }

    None
}

// ---------------------------------------------------------------------------
// Internal tab state
// ---------------------------------------------------------------------------

#[derive(Clone)]
enum TabKind {
    Terminal { state: TerminalTabState },
    Browser { state: BrowserTabState },
    CustomSidebar { name: String },
    Keybinds,
}

enum TabFocusTarget {
    Terminal(terminal::TerminalHandle),
    Browser(BrowserHandles),
    Widget(gtk::Widget),
}

impl TabFocusTarget {
    fn from_entry(entry: &TabEntry) -> Self {
        match &entry.kind {
            TabKind::Terminal { state } => Self::Terminal(state.handle.clone()),
            TabKind::Browser { state } => Self::Browser(state.handles.clone()),
            TabKind::CustomSidebar { .. } => Self::Widget(entry.content.clone()),
            TabKind::Keybinds => Self::Widget(entry.content.clone()),
        }
    }

    fn focus(self) {
        match self {
            Self::Terminal(handle) => {
                handle.focus_surface();
            }
            Self::Browser(handles) => {
                handles.focus_content();
            }
            Self::Widget(widget) => {
                if widget.is_focus() || widget.can_focus() {
                    widget.grab_focus();
                } else {
                    widget.child_focus(gtk::DirectionType::TabForward);
                }
            }
        }
    }
}

struct TabEntry {
    id: String,
    tab_button: gtk::Box,
    title_label: gtk::Label,
    content: gtk::Widget,
    custom_name: Option<String>,
    pinned: bool,
    kind: TabKind,
}

struct TabState {
    tabs: Vec<TabEntry>,
    active_tab: Option<String>,
}

/// Shared internals stored on the pane outer Box for external access.
pub struct PaneInternals {
    pane_id: u32,
    tab_state: Rc<std::cell::RefCell<TabState>>,
    tab_strip: gtk::Box,
    content_stack: gtk::Stack,
    drop_indicator: gtk::Box,
    content_drop_overlay: gtk::Box,
    pane_outer: gtk::Box,
    callbacks: Rc<PaneCallbacks>,
    working_directory: Rc<std::cell::RefCell<Option<String>>>,
    workspace_dragging: Rc<Cell<bool>>,
    new_terminal_button: gtk::Button,
    split_right_button: gtk::Button,
    split_down_button: gtk::Button,
    close_pane_button: gtk::Button,
}

impl TabState {
    fn find_tab_mut(&mut self, id: &str) -> Option<&mut TabEntry> {
        self.tabs.iter_mut().find(|e| e.id == id)
    }
}

fn next_tab_id() -> String {
    uuid::Uuid::new_v4().to_string()
}

// ---------------------------------------------------------------------------
// Icon button helper
// ---------------------------------------------------------------------------

fn icon_button(icon_name: &str, tooltip: &str) -> gtk::Button {
    let btn = gtk::Button::builder()
        .icon_name(icon_name)
        .tooltip_text(tooltip)
        .has_frame(false)
        .build();
    btn.add_css_class("limux-pane-action");
    btn
}

fn pane_action_tooltip(
    shortcuts: &ResolvedShortcutConfig,
    base: &str,
    shortcut_id: Option<ShortcutId>,
) -> String {
    shortcut_id
        .map(|id| shortcuts.tooltip_text(id, base))
        .unwrap_or_else(|| base.to_string())
}

/// Create a split-pane icon button with two rectangles separated by a divider.
/// Horizontal = left|right panes, Vertical = top/bottom panes.
#[allow(dead_code)]
fn split_icon_button(orientation: gtk::Orientation, tooltip: &str) -> gtk::Button {
    let icon = gtk::Box::new(orientation, 1);
    icon.add_css_class("limux-split-icon");

    let (class_name, count) = match orientation {
        gtk::Orientation::Horizontal => ("limux-split-half-v", 2),
        _ => ("limux-split-half-h", 2),
    };

    for _ in 0..count {
        let half = gtk::Box::new(gtk::Orientation::Vertical, 0);
        half.add_css_class(class_name);
        icon.append(&half);
    }

    let btn = gtk::Button::builder()
        .child(&icon)
        .tooltip_text(tooltip)
        .has_frame(false)
        .build();
    btn.add_css_class("limux-split-btn");
    btn
}

// ---------------------------------------------------------------------------
// Tab creation
// ---------------------------------------------------------------------------

struct TerminalTabOptions<'a> {
    id: Option<&'a str>,
    custom_name: Option<&'a str>,
    pinned: bool,
    cwd: Option<&'a str>,
    agent: Option<RestorableAgentState>,
    startup_command: Option<String>,
    extra_env: Vec<(String, String)>,
    activate: bool,
}

pub(crate) struct TerminalLaunchOptions {
    pub command: Option<String>,
    pub working_directory: Option<String>,
    pub extra_env: Vec<(String, String)>,
    pub activate: bool,
}

struct BrowserTabOptions<'a> {
    id: Option<&'a str>,
    custom_name: Option<&'a str>,
    pinned: bool,
    uri: Option<&'a str>,
}

struct KeybindsTabOptions<'a> {
    id: Option<&'a str>,
    custom_name: Option<&'a str>,
    pinned: bool,
}

struct CustomSidebarTabOptions<'a> {
    id: Option<&'a str>,
    custom_name: Option<&'a str>,
    pinned: bool,
    activate: bool,
}

struct KeybindsTabInput<'a> {
    shortcuts: Rc<ResolvedShortcutConfig>,
    on_capture: Rc<PaneShortcutCaptureCallback>,
    options: Option<KeybindsTabOptions<'a>>,
}

fn restore_tabs_from_state(
    internals: &Rc<PaneInternals>,
    working_directory: Option<&str>,
    saved_state: &PaneState,
) {
    if saved_state.tabs.is_empty() {
        add_terminal_tab_inner(internals, working_directory, None);
        return;
    }

    for saved_tab in &saved_state.tabs {
        let restore_active = saved_state
            .active_tab_id
            .as_deref()
            .map(|active_id| active_id == saved_tab.id)
            .unwrap_or_else(|| {
                saved_state
                    .tabs
                    .first()
                    .is_some_and(|first| first.id == saved_tab.id)
            });
        match &saved_tab.content {
            TabContentState::Terminal {
                cwd,
                startup_command,
                agent,
            } => {
                add_terminal_tab_inner(
                    internals,
                    cwd.as_deref().or(working_directory),
                    Some(TerminalTabOptions {
                        id: Some(saved_tab.id.as_str()),
                        custom_name: saved_tab.custom_name.as_deref(),
                        pinned: saved_tab.pinned,
                        cwd: cwd.as_deref().or(working_directory),
                        agent: agent.clone(),
                        startup_command: startup_command.clone(),
                        extra_env: Vec::new(),
                        activate: restore_active,
                    }),
                );
            }
            TabContentState::Browser { uri } => {
                add_browser_tab_inner(
                    internals,
                    Some(BrowserTabOptions {
                        id: Some(saved_tab.id.as_str()),
                        custom_name: saved_tab.custom_name.as_deref(),
                        pinned: saved_tab.pinned,
                        uri: uri.as_deref(),
                    }),
                );
            }
            TabContentState::CustomSidebar { name } => {
                let widget = (internals.callbacks.build_custom_sidebar)(name);
                add_custom_sidebar_tab_inner(
                    internals,
                    name,
                    widget,
                    Some(CustomSidebarTabOptions {
                        id: Some(saved_tab.id.as_str()),
                        custom_name: saved_tab.custom_name.as_deref(),
                        pinned: saved_tab.pinned,
                        activate: restore_active,
                    }),
                );
            }
            TabContentState::Keybinds {} => add_keybind_editor_tab_inner(
                internals,
                KeybindsTabInput {
                    shortcuts: (internals.callbacks.current_shortcuts)(),
                    on_capture: internals.callbacks.on_capture_shortcut.clone(),
                    options: Some(KeybindsTabOptions {
                        id: Some(saved_tab.id.as_str()),
                        custom_name: saved_tab.custom_name.as_deref(),
                        pinned: saved_tab.pinned,
                    }),
                },
            ),
            // Settings now open in a transient dialog rather than a persisted tab.
            TabContentState::Settings {} => {}
        }
    }

    if internals.tab_state.borrow().tabs.is_empty() {
        add_terminal_tab_inner(internals, working_directory, None);
    }

    let active_tab_id = saved_state
        .active_tab_id
        .as_deref()
        .filter(|candidate| {
            internals
                .tab_state
                .borrow()
                .tabs
                .iter()
                .any(|tab| tab.id == *candidate)
        })
        .map(|value| value.to_string())
        .or_else(|| {
            internals
                .tab_state
                .borrow()
                .tabs
                .first()
                .map(|tab| tab.id.clone())
        });

    if let Some(active_tab_id) = active_tab_id {
        activate_tab(
            &internals.tab_strip,
            &internals.content_stack,
            &internals.tab_state,
            &active_tab_id,
        );
    }
}

fn make_terminal_callbacks(
    internals: &Rc<PaneInternals>,
    tab_id: &str,
    title_label: &gtk::Label,
    term_cwd: &Rc<RefCell<Option<String>>>,
) -> TerminalCallbacks {
    let tid_for_title = tab_id.to_string();
    let title_label = title_label.clone();
    let state_for_title = internals.tab_state.clone();
    let callbacks_for_bell = internals.callbacks.clone();
    let callbacks_for_pwd = internals.callbacks.clone();
    let callbacks_for_close = internals.callbacks.clone();
    let callbacks_for_browser_here = internals.callbacks.clone();
    let callbacks_for_split_right = internals.callbacks.clone();
    let callbacks_for_split_down = internals.callbacks.clone();
    let callbacks_for_keybinds = internals.callbacks.clone();
    let callbacks_for_identity = internals.callbacks.clone();
    let tab_strip = internals.tab_strip.clone();
    let content_stack = internals.content_stack.clone();
    let tab_state = internals.tab_state.clone();
    let pane_outer = internals.pane_outer.clone();
    let term_cwd_for_pwd = term_cwd.clone();
    let tid_for_close = tab_id.to_string();
    let tid_for_notification = tab_id.to_string();
    let pane_id = internals.pane_id;

    TerminalCallbacks {
        on_title_changed: Box::new(move |title: &str| {
            let title_state = state_for_title.borrow();
            let has_custom = title_state
                .tabs
                .iter()
                .any(|entry| entry.id == tid_for_title && entry.custom_name.is_some());
            if has_custom || title.is_empty() {
                return;
            }
            let display = if title.len() > 22 {
                format!("{}…", &title[..21])
            } else {
                title.to_string()
            };
            if title_label.label().as_str() == display {
                return;
            }
            title_label.set_label(&display);
        }),
        on_pwd_changed: Box::new(move |pwd: &str| {
            let mut term_cwd = term_cwd_for_pwd.borrow_mut();
            if term_cwd.as_deref() == Some(pwd) {
                return;
            }
            *term_cwd = Some(pwd.to_string());
            drop(term_cwd);
            (callbacks_for_pwd.on_pwd_changed)(pwd);
            (callbacks_for_pwd.on_state_changed)();
        }),
        on_desktop_notification: Box::new({
            let callbacks = internals.callbacks.clone();
            let tab_id = tid_for_notification.clone();
            move |title: &str, body: &str, source_focused: bool| {
                (callbacks.on_desktop_notification)(title, body, source_focused, pane_id, &tab_id);
            }
        }),
        on_bell: Box::new({
            let tab_id = tid_for_notification.clone();
            move |source_focused| {
                (callbacks_for_bell.on_bell)(source_focused, pane_id, &tab_id);
            }
        }),
        on_close: Box::new(move || {
            let tab_strip = tab_strip.clone();
            let content_stack = content_stack.clone();
            let tab_state = tab_state.clone();
            let callbacks = callbacks_for_close.clone();
            let pane_outer = pane_outer.clone();
            let tab_id = tid_for_close.clone();
            glib::idle_add_local_once(move || {
                remove_tab(
                    &tab_strip,
                    &content_stack,
                    &tab_state,
                    &tab_id,
                    &callbacks,
                    &pane_outer,
                    PaneEmptyReason::ClosedLastTab,
                );
            });
        }),
        on_open_url: Box::new({
            let pane_outer = internals.pane_outer.clone();
            move |url, external| {
                if external {
                    open_url_in_external_browser(url);
                    return;
                }

                let pane_widget: gtk::Widget = pane_outer.clone().upcast();
                add_browser_tab_to_pane_with_uri(&pane_widget, Some(url));
            }
        }),
        on_open_browser_here: Box::new({
            let pane_outer = internals.pane_outer.clone();
            move || {
                let pane_widget: gtk::Widget = pane_outer.clone().upcast();
                (callbacks_for_browser_here.on_open_browser_here)(&pane_widget);
            }
        }),
        on_split_right: Box::new({
            let pane_outer = internals.pane_outer.clone();
            move || {
                let pane_widget: gtk::Widget = pane_outer.clone().upcast();
                (callbacks_for_split_right.on_split)(&pane_widget, gtk::Orientation::Horizontal);
            }
        }),
        on_split_down: Box::new({
            let pane_outer = internals.pane_outer.clone();
            move || {
                let pane_widget: gtk::Widget = pane_outer.clone().upcast();
                (callbacks_for_split_down.on_split)(&pane_widget, gtk::Orientation::Vertical);
            }
        }),
        on_open_keybinds: Box::new({
            let pane_outer = internals.pane_outer.clone();
            move |_anchor| {
                let pane_widget: gtk::Widget = pane_outer.clone().upcast();
                (callbacks_for_keybinds.on_open_keybinds)(&pane_widget);
            }
        }),
        identity: Box::new({
            let pane_outer = internals.pane_outer.clone();
            let surface_id = format!("{}:{}", internals.pane_id, tab_id);
            move || {
                let pane_widget: gtk::Widget = pane_outer.clone().upcast();
                terminal::TerminalIdentity {
                    workspace_id: (callbacks_for_identity.workspace_for_pane)(&pane_widget),
                    surface_id: surface_id.clone(),
                }
            }
        }),
    }
}

fn open_url_in_external_browser(url: &str) {
    if let Err(err) =
        gtk::gio::AppInfo::launch_default_for_uri(url, None::<&gtk::gio::AppLaunchContext>)
    {
        eprintln!("limux: failed to open URL in external browser: {err}");
    }
}

fn add_terminal_tab_inner(
    internals: &Rc<PaneInternals>,
    working_directory: Option<&str>,
    options: Option<TerminalTabOptions<'_>>,
) -> String {
    let tab_id = options
        .as_ref()
        .and_then(|value| value.id.map(|id| id.to_string()))
        .unwrap_or_else(next_tab_id);
    let (tab_btn, title_label) = build_tab_button("Terminal", &tab_id, internals);

    let term_cwd = Rc::new(RefCell::new(
        options
            .as_ref()
            .and_then(|value| value.cwd.map(|cwd| cwd.to_string()))
            .or_else(|| working_directory.map(|cwd| cwd.to_string())),
    ));
    let term_callbacks = make_terminal_callbacks(internals, &tab_id, &title_label, &term_cwd);
    let hover_focus = {
        let callbacks = internals.callbacks.clone();
        Rc::new(move || {
            let config = (callbacks.current_config)();
            let hover_focus = config.borrow().focus.hover_terminal_focus;
            hover_focus
        })
    };
    let focus_pane_on_first_click = {
        let callbacks = internals.callbacks.clone();
        Rc::new(move || {
            let config = (callbacks.current_config)();
            let focus_pane_on_first_click = config.borrow().app.focus_pane_on_first_click;
            focus_pane_on_first_click
        })
    };

    // Build the env the spawned shell will see. Encodes this terminal's
    // identity so CLI calls (e.g. `limux identify`, `limux send`) auto-target
    // the current surface without flags. Mirrors cmux's env auto-wiring.
    let pane_widget: gtk::Widget = internals.pane_outer.clone().upcast();
    let workspace_id_for_env = (internals.callbacks.workspace_for_pane)(&pane_widget);
    let surface_id_for_env = format!("{}:{}", internals.pane_id, tab_id);
    let mut extra_env = (internals.callbacks.workspace_environment_for_pane)(&pane_widget);
    if let Some(ws) = workspace_id_for_env {
        extra_env.push(("LIMUX_WORKSPACE_ID".to_string(), ws.clone()));
        extra_env.push(("CMUX_WORKSPACE_ID".to_string(), ws));
    }
    extra_env.push(("LIMUX_SURFACE_ID".to_string(), surface_id_for_env.clone()));
    extra_env.push(("CMUX_SURFACE_ID".to_string(), surface_id_for_env.clone()));
    extra_env.push(("LIMUX_PANE_ID".to_string(), internals.pane_id.to_string()));
    extra_env.push(("LIMUX_TAB_ID".to_string(), tab_id.clone()));
    extra_env.push(("CMUX_TAB_ID".to_string(), tab_id.clone()));
    {
        let config = (internals.callbacks.current_config)();
        append_git_watch_env(&config.borrow(), &mut extra_env);
    }
    install_codex_wrapper_env(&surface_id_for_env, &mut extra_env);
    if let Some(sock) = limux_control::socket_path::resolve_socket_path(
        None,
        limux_control::socket_path::SocketMode::Runtime,
    )
    .to_str()
    {
        extra_env.push(("LIMUX_SOCKET".to_string(), sock.to_string()));
        extra_env.push(("LIMUX_SOCKET_PATH".to_string(), sock.to_string()));
        extra_env.push(("CMUX_SOCKET".to_string(), sock.to_string()));
        extra_env.push(("CMUX_SOCKET_PATH".to_string(), sock.to_string()));
    }
    if let Some(tab_options) = options.as_ref() {
        extra_env.extend(tab_options.extra_env.iter().cloned());
    }
    let startup_command = options
        .as_ref()
        .and_then(|value| value.startup_command.clone())
        .or_else(|| {
            options
                .as_ref()
                .and_then(|value| value.agent.as_ref())
                .and_then(|agent| agent.resume_command())
        });
    if let Some(command) = startup_command.as_deref() {
        eprintln!(
            "limux: restoring agent terminal surface={}:{} command={}",
            internals.pane_id, tab_id, command
        );
    }

    let term = terminal::create_terminal(
        working_directory,
        terminal::TerminalOptions {
            hover_focus,
            focus_pane_on_first_click,
            saved_font_size: (internals.callbacks.current_config)().borrow().font_size,
            startup_command,
            extra_env,
        },
        term_callbacks,
    );
    let widget = term.root.clone();
    internals.content_stack.add_named(&widget, Some(&tab_id));

    {
        let mut ts = internals.tab_state.borrow_mut();
        ts.tabs.push(TabEntry {
            id: tab_id.clone(),
            tab_button: tab_btn,
            title_label: title_label.clone(),
            content: widget,
            custom_name: options
                .as_ref()
                .and_then(|value| value.custom_name.map(|name| name.to_string())),
            pinned: options.as_ref().map(|value| value.pinned).unwrap_or(false),
            kind: TabKind::Terminal {
                state: TerminalTabState {
                    cwd: term_cwd.clone(),
                    handle: term.handle.clone(),
                },
            },
        });
    }
    internals.tab_strip.append(
        &internals
            .tab_state
            .borrow()
            .tabs
            .iter()
            .find(|entry| entry.id == tab_id)
            .expect("terminal tab inserted")
            .tab_button,
    );

    if let Some(custom_name) = options.as_ref().and_then(|value| value.custom_name) {
        title_label.set_label(custom_name);
    }
    if options.as_ref().map(|value| value.pinned).unwrap_or(false) {
        if let Some(entry) = internals
            .tab_state
            .borrow()
            .tabs
            .iter()
            .find(|entry| entry.id == tab_id)
        {
            apply_pin_visuals(&entry.tab_button, true);
        }
    }

    let activate = options.as_ref().map(|value| value.activate).unwrap_or(true);
    if activate {
        activate_tab(
            &internals.tab_strip,
            &internals.content_stack,
            &internals.tab_state,
            &tab_id,
        );
        term.handle.focus_surface();
    }
    if options.is_none() {
        (internals.callbacks.on_state_changed)();
    }
    tab_id
}

fn add_browser_tab_inner(
    internals: &Rc<PaneInternals>,
    options: Option<BrowserTabOptions<'_>>,
) -> String {
    let tab_id = options
        .as_ref()
        .and_then(|value| value.id.map(|id| id.to_string()))
        .unwrap_or_else(next_tab_id);
    let saved_uri = Rc::new(RefCell::new(
        options
            .as_ref()
            .and_then(|value| value.uri.map(|uri| uri.to_string())),
    ));
    let (widget, title, handles) = create_browser_widget(
        options.as_ref().and_then(|value| value.uri),
        saved_uri.clone(),
        internals.callbacks.clone(),
    );

    let (tab_btn, title_label) = build_tab_button(&title, &tab_id, internals);

    internals.content_stack.add_named(&widget, Some(&tab_id));

    {
        let mut ts = internals.tab_state.borrow_mut();
        ts.tabs.push(TabEntry {
            id: tab_id.clone(),
            tab_button: tab_btn,
            title_label: title_label.clone(),
            content: widget,
            custom_name: options
                .as_ref()
                .and_then(|value| value.custom_name.map(|name| name.to_string())),
            pinned: options.as_ref().map(|value| value.pinned).unwrap_or(false),
            kind: TabKind::Browser {
                state: BrowserTabState {
                    uri: saved_uri.clone(),
                    handles,
                },
            },
        });
    }
    internals.tab_strip.append(
        &internals
            .tab_state
            .borrow()
            .tabs
            .iter()
            .find(|entry| entry.id == tab_id)
            .expect("browser tab inserted")
            .tab_button,
    );

    if let Some(custom_name) = options.as_ref().and_then(|value| value.custom_name) {
        title_label.set_label(custom_name);
    }
    if options.as_ref().map(|value| value.pinned).unwrap_or(false) {
        if let Some(entry) = internals
            .tab_state
            .borrow()
            .tabs
            .iter()
            .find(|entry| entry.id == tab_id)
        {
            apply_pin_visuals(&entry.tab_button, true);
        }
    }

    activate_tab(
        &internals.tab_strip,
        &internals.content_stack,
        &internals.tab_state,
        &tab_id,
    );
    if options.is_none() {
        (internals.callbacks.on_state_changed)();
    }
    tab_id
}

fn add_keybind_editor_tab_inner(internals: &Rc<PaneInternals>, input: KeybindsTabInput<'_>) {
    let tab_id = input
        .options
        .as_ref()
        .and_then(|value| value.id.map(|id| id.to_string()))
        .unwrap_or_else(next_tab_id);

    let (tab_btn, title_label) = build_tab_button("Keybinds", &tab_id, internals);

    let widget = keybind_editor::build_keybind_editor(&input.shortcuts, input.on_capture);
    internals.content_stack.add_named(&widget, Some(&tab_id));

    {
        let mut ts = internals.tab_state.borrow_mut();
        ts.tabs.push(TabEntry {
            id: tab_id.clone(),
            tab_button: tab_btn,
            title_label: title_label.clone(),
            content: widget,
            custom_name: input
                .options
                .as_ref()
                .and_then(|value| value.custom_name.map(|name| name.to_string())),
            pinned: input
                .options
                .as_ref()
                .map(|value| value.pinned)
                .unwrap_or(false),
            kind: TabKind::Keybinds,
        });
    }
    internals.tab_strip.append(
        &internals
            .tab_state
            .borrow()
            .tabs
            .iter()
            .find(|entry| entry.id == tab_id)
            .expect("keybinds tab inserted")
            .tab_button,
    );

    if let Some(custom_name) = input.options.as_ref().and_then(|value| value.custom_name) {
        title_label.set_label(custom_name);
    }
    if input
        .options
        .as_ref()
        .map(|value| value.pinned)
        .unwrap_or(false)
    {
        if let Some(entry) = internals
            .tab_state
            .borrow()
            .tabs
            .iter()
            .find(|entry| entry.id == tab_id)
        {
            apply_pin_visuals(&entry.tab_button, true);
        }
    }

    activate_tab(
        &internals.tab_strip,
        &internals.content_stack,
        &internals.tab_state,
        &tab_id,
    );
    if input.options.is_none() {
        (internals.callbacks.on_state_changed)();
    }
}

// purpose: Add a custom-sidebar widget tab to a pane.
// inputs: Pane internals, sidebar name, rendered widget, and optional restored tab metadata.
// returns/effects: Inserts/focuses a customSidebar surface and returns the tab id.
fn add_custom_sidebar_tab_inner(
    internals: &Rc<PaneInternals>,
    name: &str,
    widget: gtk::Widget,
    options: Option<CustomSidebarTabOptions<'_>>,
) -> String {
    let metadata = custom_sidebar_tab_metadata(name, options.as_ref());
    let (tab_btn, title_label) = build_tab_button(&metadata.title, &metadata.tab_id, internals);
    internals
        .content_stack
        .add_named(&widget, Some(&metadata.tab_id));
    push_custom_sidebar_tab_entry(
        internals,
        CustomSidebarTabEntryInput {
            tab_id: metadata.tab_id.clone(),
            tab_btn,
            title_label,
            widget,
            name: name.to_string(),
            custom_name: metadata.custom_name,
            pinned: metadata.pinned,
        },
    );
    append_custom_sidebar_tab_button(internals, &metadata.tab_id, metadata.pinned);
    activate_custom_sidebar_tab_if_requested(internals, &metadata.tab_id, metadata.activate);
    if options.is_none() {
        (internals.callbacks.on_state_changed)();
    }
    metadata.tab_id
}

struct CustomSidebarTabMetadata {
    tab_id: String,
    title: String,
    custom_name: Option<String>,
    pinned: bool,
    activate: bool,
}

// purpose: Normalize optional custom-sidebar tab insertion metadata.
// inputs: Sidebar name and optional restored tab options.
// returns/effects: Returns owned metadata without mutating GTK state.
fn custom_sidebar_tab_metadata(
    name: &str,
    options: Option<&CustomSidebarTabOptions<'_>>,
) -> CustomSidebarTabMetadata {
    CustomSidebarTabMetadata {
        tab_id: options
            .and_then(|value| value.id.map(|id| id.to_string()))
            .unwrap_or_else(next_tab_id),
        title: options
            .and_then(|value| value.custom_name)
            .unwrap_or(name)
            .to_string(),
        custom_name: options.and_then(|value| value.custom_name.map(|name| name.to_string())),
        pinned: options.map(|value| value.pinned).unwrap_or(false),
        activate: options.map(|value| value.activate).unwrap_or(true),
    }
}

struct CustomSidebarTabEntryInput {
    tab_id: String,
    tab_btn: gtk::Box,
    title_label: gtk::Label,
    widget: gtk::Widget,
    name: String,
    custom_name: Option<String>,
    pinned: bool,
}

// purpose: Store one custom-sidebar tab entry in pane state.
// inputs: Pane internals and prebuilt GTK tab/content widgets.
// returns/effects: Mutates tab state without changing active selection.
fn push_custom_sidebar_tab_entry(internals: &Rc<PaneInternals>, input: CustomSidebarTabEntryInput) {
    internals.tab_state.borrow_mut().tabs.push(TabEntry {
        id: input.tab_id,
        tab_button: input.tab_btn,
        title_label: input.title_label,
        content: input.widget,
        custom_name: input.custom_name,
        pinned: input.pinned,
        kind: TabKind::CustomSidebar { name: input.name },
    });
}

// purpose: Append a custom-sidebar tab button and apply pinned visuals.
// inputs: Pane internals, inserted tab id, and pin flag.
// returns/effects: Mutates the GTK tab strip for the inserted tab.
fn append_custom_sidebar_tab_button(internals: &Rc<PaneInternals>, tab_id: &str, pinned: bool) {
    let tab_state = internals.tab_state.borrow();
    let entry = tab_state
        .tabs
        .iter()
        .find(|entry| entry.id == tab_id)
        .expect("custom sidebar tab inserted");
    internals.tab_strip.append(&entry.tab_button);
    if pinned {
        apply_pin_visuals(&entry.tab_button, true);
    }
}

// purpose: Activate a custom-sidebar tab when the caller requested focus.
// inputs: Pane internals, tab id, and activation flag.
// returns/effects: Changes active tab only when activation is requested.
fn activate_custom_sidebar_tab_if_requested(
    internals: &Rc<PaneInternals>,
    tab_id: &str,
    activate: bool,
) {
    if activate {
        activate_tab(
            &internals.tab_strip,
            &internals.content_stack,
            &internals.tab_state,
            tab_id,
        );
    }
}

// Public wrappers for keyboard shortcut use
#[allow(dead_code)]
pub fn add_terminal_tab_to_pane(pane_widget: &gtk::Widget) {
    if let Some(internals) = find_pane_internals(pane_widget) {
        let dir = internals.working_directory.borrow().clone();
        add_terminal_tab_inner(&internals, dir.as_deref(), None);
    }
}

#[allow(dead_code)]
pub fn add_terminal_tab_to_pane_with_command(
    pane_widget: &gtk::Widget,
    command: Option<String>,
    activate: bool,
) -> Option<SurfaceSummary> {
    let internals = find_pane_internals(pane_widget)?;
    let dir = internals.working_directory.borrow().clone();
    let options = if command.is_some() || !activate {
        Some(TerminalTabOptions {
            id: None,
            custom_name: None,
            pinned: false,
            cwd: None,
            agent: None,
            startup_command: command,
            extra_env: Vec::new(),
            activate,
        })
    } else {
        None
    };
    let tab_id = add_terminal_tab_inner(&internals, dir.as_deref(), options);
    let surface_id = composite_surface_id(internals.pane_id, &tab_id);
    let tab_state = internals.tab_state.borrow();
    let entry = tab_state.tabs.iter().find(|entry| entry.id == tab_id)?;
    let (kind, cwd, uri) = match &entry.kind {
        TabKind::Terminal { state } => ("terminal".to_string(), state.cwd.borrow().clone(), None),
        TabKind::Browser { state } => ("browser".to_string(), None, state.uri.borrow().clone()),
        TabKind::CustomSidebar { .. } => ("customSidebar".to_string(), None, None),
        TabKind::Keybinds => ("keybinds".to_string(), None, None),
    };
    Some(SurfaceSummary {
        pane_id: internals.pane_id,
        surface_id,
        title: entry.title_label.label().to_string(),
        kind,
        selected: tab_state.active_tab.as_deref() == Some(tab_id.as_str()),
        cwd,
        uri,
    })
}

// purpose: Add a terminal tab with startup cwd, command, and environment.
// inputs: Pane widget and launch options supplied by the live control bridge.
// returns/effects: Creates one terminal surface and returns its summary.
pub fn add_terminal_tab_to_pane_with_launch_options(
    pane_widget: &gtk::Widget,
    launch: TerminalLaunchOptions,
) -> Option<SurfaceSummary> {
    let internals = find_pane_internals(pane_widget)?;
    let fallback_dir = internals.working_directory.borrow().clone();
    let cwd = launch
        .working_directory
        .as_deref()
        .or(fallback_dir.as_deref());
    let tab_id = add_terminal_tab_inner(
        &internals,
        cwd,
        Some(TerminalTabOptions {
            id: None,
            custom_name: None,
            pinned: false,
            cwd,
            agent: None,
            startup_command: launch.command,
            extra_env: launch.extra_env,
            activate: launch.activate,
        }),
    );
    surface_summary_for_tab(&internals, &tab_id)
}

// purpose: Replace a terminal tab with a new process while preserving its surface ID.
// inputs: Pane widget, optional surface hint, and command to run in the new terminal.
// returns/effects: Recreates the terminal tab in place and returns its updated surface summary.
pub fn respawn_terminal_surface(
    pane_widget: &gtk::Widget,
    surface_hint: Option<&str>,
    command: String,
) -> Option<SurfaceSummary> {
    let internals = find_pane_internals(pane_widget)?;
    let requested = surface_hint
        .map(normalize_surface_hint)
        .filter(|value| !value.is_empty());
    let (tab_id, custom_name, pinned, cwd, was_active) = {
        let tab_state = internals.tab_state.borrow();
        let active_tab = tab_state.active_tab.as_deref();
        let entry = tab_state.tabs.iter().find(|entry| {
            let surface_id = composite_surface_id(internals.pane_id, &entry.id);
            matches!(entry.kind, TabKind::Terminal { .. })
                && requested
                    .map(|hint| hint == entry.id || hint == surface_id)
                    .unwrap_or(active_tab == Some(entry.id.as_str()))
        })?;
        let cwd = match &entry.kind {
            TabKind::Terminal { state } => state.cwd.borrow().clone(),
            TabKind::Browser { .. } | TabKind::CustomSidebar { .. } | TabKind::Keybinds => None,
        };
        (
            entry.id.clone(),
            entry.custom_name.clone(),
            entry.pinned,
            cwd,
            active_tab == Some(entry.id.as_str()),
        )
    };

    {
        let mut tab_state = internals.tab_state.borrow_mut();
        let index = tab_state.tabs.iter().position(|entry| entry.id == tab_id)?;
        let entry = tab_state.tabs.remove(index);
        internals.tab_strip.remove(&entry.tab_button);
        internals.content_stack.remove(&entry.content);
        if tab_state.active_tab.as_deref() == Some(tab_id.as_str()) {
            tab_state.active_tab = None;
        }
    }
    let options = TerminalTabOptions {
        id: Some(&tab_id),
        custom_name: custom_name.as_deref(),
        pinned,
        cwd: cwd.as_deref(),
        agent: None,
        startup_command: Some(command),
        extra_env: Vec::new(),
        activate: was_active,
    };
    let working_directory = internals.working_directory.borrow().clone();
    let new_tab_id =
        add_terminal_tab_inner(&internals, working_directory.as_deref(), Some(options));
    (internals.callbacks.on_state_changed)();
    surface_summary_for_tab(&internals, &new_tab_id)
}

// purpose: Respawn a terminal surface found anywhere under a workspace root.
// inputs: Workspace root, optional surface hint, and command to run.
// returns/effects: Replaces the matching terminal tab process and preserves the surface ID.
pub fn respawn_terminal_surface_for_root(
    root: &gtk::Widget,
    surface_hint: Option<&str>,
    command: String,
) -> Option<SurfaceSummary> {
    pane_internals_for_root(root)
        .into_iter()
        .find_map(|internals| {
            let pane_widget: gtk::Widget = internals.pane_outer.clone().upcast();
            respawn_terminal_surface(&pane_widget, surface_hint, command.clone())
        })
}

#[allow(dead_code)]
pub fn add_browser_tab_to_pane(pane_widget: &gtk::Widget) {
    add_browser_tab_to_pane_with_uri(pane_widget, None);
}

#[allow(dead_code)]
pub fn add_browser_tab_to_pane_with_uri(
    pane_widget: &gtk::Widget,
    uri: Option<&str>,
) -> Option<SurfaceSummary> {
    let internals = find_pane_internals(pane_widget)?;
    let options = uri.map(|uri| BrowserTabOptions {
        id: None,
        custom_name: None,
        pinned: false,
        uri: Some(uri),
    });
    let notify_after_insert = options.is_some();
    let tab_id = add_browser_tab_inner(&internals, options);
    if notify_after_insert {
        (internals.callbacks.on_state_changed)();
    }
    surface_summary_for_tab(&internals, &tab_id)
}

// purpose: Add or focus a named custom sidebar tab in one pane.
// inputs: Pane widget, sidebar name, rendered widget, and focus policy.
// returns/effects: Reuses an existing matching customSidebar tab or creates one.
pub fn add_custom_sidebar_tab_to_pane(
    pane_widget: &gtk::Widget,
    name: &str,
    widget: gtk::Widget,
    activate: bool,
) -> Option<SurfaceSummary> {
    let internals = find_pane_internals(pane_widget)?;
    if let Some(existing_id) = internals
        .tab_state
        .borrow()
        .tabs
        .iter()
        .find(|entry| matches!(&entry.kind, TabKind::CustomSidebar { name: current } if current == name))
        .map(|entry| entry.id.clone())
    {
        if activate {
            activate_tab(
                &internals.tab_strip,
                &internals.content_stack,
                &internals.tab_state,
                &existing_id,
            );
        }
        return surface_summary_for_tab(&internals, &existing_id);
    }
    let tab_id = add_custom_sidebar_tab_inner(
        &internals,
        name,
        widget,
        Some(CustomSidebarTabOptions {
            id: None,
            custom_name: None,
            pinned: false,
            activate,
        }),
    );
    (internals.callbacks.on_state_changed)();
    surface_summary_for_tab(&internals, &tab_id)
}

pub fn add_keybind_editor_tab_to_pane(
    pane_widget: &gtk::Widget,
    shortcuts: Rc<ResolvedShortcutConfig>,
    on_capture: Rc<PaneShortcutCaptureCallback>,
) {
    if let Some(internals) = find_pane_internals(pane_widget) {
        if let Some(existing_id) = internals
            .tab_state
            .borrow()
            .tabs
            .iter()
            .find(|entry| matches!(entry.kind, TabKind::Keybinds))
            .map(|entry| entry.id.clone())
        {
            activate_tab(
                &internals.tab_strip,
                &internals.content_stack,
                &internals.tab_state,
                &existing_id,
            );
            (internals.callbacks.on_state_changed)();
            return;
        }

        add_keybind_editor_tab_inner(
            &internals,
            KeybindsTabInput {
                shortcuts,
                on_capture,
                options: None,
            },
        );
    }
}

pub fn refresh_shortcut_tooltips(pane_widget: &gtk::Widget, shortcuts: &ResolvedShortcutConfig) {
    let Some(internals) = find_pane_internals(pane_widget) else {
        return;
    };

    internals
        .new_terminal_button
        .set_tooltip_text(Some(&pane_action_tooltip(
            shortcuts,
            "New terminal tab",
            Some(ShortcutId::NewTerminal),
        )));
    internals
        .split_right_button
        .set_tooltip_text(Some(&pane_action_tooltip(
            shortcuts,
            "Split right",
            Some(ShortcutId::SplitRight),
        )));
    internals
        .split_down_button
        .set_tooltip_text(Some(&pane_action_tooltip(
            shortcuts,
            "Split down",
            Some(ShortcutId::SplitDown),
        )));
    internals
        .close_pane_button
        .set_tooltip_text(Some(&pane_action_tooltip(
            shortcuts,
            "Close pane",
            Some(ShortcutId::CloseFocusedPane),
        )));
}

pub fn snapshot_pane_state(pane_widget: &gtk::Widget) -> Option<PaneState> {
    let internals = find_pane_internals(pane_widget)?;
    let ts = internals.tab_state.borrow();
    let tabs = ts
        .tabs
        .iter()
        .map(|entry| {
            let content = match &entry.kind {
                TabKind::Terminal { state } => TabContentState::Terminal {
                    cwd: state.cwd.borrow().clone(),
                    startup_command: None,
                    agent: None,
                },
                TabKind::Browser { state } => TabContentState::Browser {
                    uri: state.uri.borrow().clone(),
                },
                TabKind::CustomSidebar { name } => {
                    TabContentState::CustomSidebar { name: name.clone() }
                }
                TabKind::Keybinds => TabContentState::Keybinds {},
            };
            SavedTabState {
                id: entry.id.clone(),
                custom_name: entry.custom_name.clone(),
                pinned: entry.pinned,
                content,
            }
        })
        .collect();
    Some(PaneState {
        pane_id: Some(internals.pane_id),
        active_tab_id: ts.active_tab.clone(),
        tabs,
    })
}

fn find_pane_internals(pane_widget: &gtk::Widget) -> Option<Rc<PaneInternals>> {
    let outer = pane_widget.downcast_ref::<gtk::Box>()?;
    unsafe {
        outer
            .data::<Rc<PaneInternals>>("limux-pane-internals")
            .map(|ptr| ptr.as_ref().clone())
    }
}

pub fn is_pane_widget(widget: &gtk::Widget) -> bool {
    let Some(container) = widget.downcast_ref::<gtk::Box>() else {
        return false;
    };

    let mut child = container.first_child();
    while let Some(current) = child {
        if current.has_css_class("limux-pane-header") {
            return true;
        }
        child = current.next_sibling();
    }

    false
}

pub fn tab_title(pane_widget: &gtk::Widget, tab_id: &str) -> Option<String> {
    let internals = find_pane_internals(pane_widget)?;
    let tab_state = internals.tab_state.borrow();
    let entry = tab_state.tabs.iter().find(|entry| entry.id == tab_id)?;
    Some(entry.title_label.label().to_string())
}

pub fn tab_working_directory(pane_widget: &gtk::Widget, tab_id: &str) -> Option<String> {
    let internals = find_pane_internals(pane_widget)?;
    let tab_state = internals.tab_state.borrow();
    let entry = tab_state.tabs.iter().find(|entry| entry.id == tab_id)?;
    match &entry.kind {
        TabKind::Terminal { state } => state.cwd.borrow().clone(),
        TabKind::Browser { .. } | TabKind::CustomSidebar { .. } | TabKind::Keybinds => None,
    }
}

/// purpose: Update the tracked working directory for a terminal surface.
/// inputs: Workspace root, surface hint, and normalized working directory.
/// returns/effects: Mutates the matching terminal tab cwd and returns its summary.
pub fn set_terminal_working_directory_for_root(
    root: &gtk::Widget,
    surface_hint: &str,
    cwd: &str,
) -> Option<SurfaceSummary> {
    for internals in pane_internals_for_root(root) {
        let pane_id = internals.pane_id;
        let mut matched_tab_id = None;
        let mut tab_state = internals.tab_state.borrow_mut();
        for entry in &mut tab_state.tabs {
            if !surface_hint_matches(
                &composite_surface_id(pane_id, &entry.id),
                &entry.id,
                surface_hint,
            ) {
                continue;
            }
            let TabKind::Terminal { state } = &entry.kind else {
                return None;
            };
            *state.cwd.borrow_mut() = Some(cwd.to_string());
            matched_tab_id = Some(entry.id.clone());
            break;
        }
        drop(tab_state);
        if let Some(tab_id) = matched_tab_id {
            return surface_summary_for_tab(&internals, &tab_id);
        }
    }
    None
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PaneSummary {
    pub pane_id: u32,
    pub surface_count: usize,
    pub active_surface_id: Option<String>,
    pub active_terminal_health: Option<terminal::TerminalHealth>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SurfaceSummary {
    pub pane_id: u32,
    pub surface_id: String,
    pub title: String,
    pub kind: String,
    pub selected: bool,
    pub cwd: Option<String>,
    pub uri: Option<String>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct TabActionSummary {
    pub surface: SurfaceSummary,
    pub pinned: bool,
    pub created: Option<SurfaceSummary>,
    pub closed: Vec<SurfaceSummary>,
    pub skipped_pinned: usize,
    pub reloaded: bool,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum TabActionError {
    NotFound,
    UnsupportedForSurface,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum BrowserTabCloseError {
    ContextNotFound,
    TargetNotFound,
    LastBrowserTab,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SurfaceCloseError {
    NotFound,
}

// purpose: Build a public surface summary for one tab entry.
// inputs: Live pane internals and the tab id to summarize.
// returns/effects: Returns metadata for the matching tab without mutating GTK state.
fn surface_summary_for_tab(internals: &Rc<PaneInternals>, tab_id: &str) -> Option<SurfaceSummary> {
    let tab_state = internals.tab_state.borrow();
    let entry = tab_state.tabs.iter().find(|entry| entry.id == tab_id)?;
    let (kind, cwd, uri) = match &entry.kind {
        TabKind::Terminal { state } => ("terminal".to_string(), state.cwd.borrow().clone(), None),
        TabKind::Browser { state } => ("browser".to_string(), None, state.uri.borrow().clone()),
        TabKind::CustomSidebar { .. } => ("customSidebar".to_string(), None, None),
        TabKind::Keybinds => ("keybinds".to_string(), None, None),
    };
    Some(SurfaceSummary {
        pane_id: internals.pane_id,
        surface_id: composite_surface_id(internals.pane_id, &entry.id),
        title: entry.title_label.label().to_string(),
        kind,
        selected: tab_state.active_tab.as_deref() == Some(entry.id.as_str()),
        cwd,
        uri,
    })
}

fn pane_internals_for_root(root: &gtk::Widget) -> Vec<Rc<PaneInternals>> {
    let mut panes = PANE_REGISTRY.with(|registry| {
        registry
            .borrow()
            .values()
            .filter_map(|weak| weak.upgrade())
            .filter(|internals| internals.pane_outer.is_ancestor(root))
            .collect::<Vec<_>>()
    });
    panes.sort_by_key(|internals| internals.pane_id);
    panes
}

pub fn pane_summaries_for_root(root: &gtk::Widget) -> Vec<PaneSummary> {
    pane_internals_for_root(root)
        .into_iter()
        .map(|internals| {
            let pane_id = internals.pane_id;
            let tab_state = internals.tab_state.borrow();
            let active_surface_id = tab_state
                .active_tab
                .as_deref()
                .map(|tab_id| composite_surface_id(pane_id, tab_id))
                .or_else(|| {
                    tab_state
                        .tabs
                        .first()
                        .map(|entry| composite_surface_id(pane_id, &entry.id))
                });
            let active_terminal_health = tab_state
                .active_tab
                .as_deref()
                .and_then(|active_tab| tab_state.tabs.iter().find(|entry| entry.id == active_tab))
                .and_then(|entry| match &entry.kind {
                    TabKind::Terminal { state } => Some(state.handle.health()),
                    TabKind::Browser { .. } | TabKind::CustomSidebar { .. } | TabKind::Keybinds => {
                        None
                    }
                });
            PaneSummary {
                pane_id,
                surface_count: tab_state.tabs.len(),
                active_surface_id,
                active_terminal_health,
            }
        })
        .collect()
}

#[allow(dead_code)]
pub(crate) fn pane_widget_for_root(root: &gtk::Widget, pane_id: u32) -> Option<gtk::Widget> {
    pane_internals_for_root(root)
        .into_iter()
        .find(|internals| internals.pane_id == pane_id)
        .map(|internals| internals.pane_outer.clone().upcast())
}

// purpose: Resolve the selected surface for a pane in a workspace root.
// inputs: Workspace root widget and a live pane id.
// returns/effects: Returns the active surface summary or the first tab summary without mutating state.
pub fn selected_surface_for_pane_in_root(
    root: &gtk::Widget,
    pane_id: u32,
) -> Option<SurfaceSummary> {
    let internals = pane_internals_for_root(root)
        .into_iter()
        .find(|internals| internals.pane_id == pane_id)?;
    let selected_tab_id = {
        let tab_state = internals.tab_state.borrow();
        tab_state
            .active_tab
            .clone()
            .or_else(|| tab_state.tabs.first().map(|entry| entry.id.clone()))?
    };
    surface_summary_for_tab(&internals, &selected_tab_id)
}

pub fn surface_summaries_for_root(root: &gtk::Widget) -> Vec<SurfaceSummary> {
    let mut surfaces = Vec::new();

    for internals in pane_internals_for_root(root) {
        let pane_id = internals.pane_id;
        let tab_state = internals.tab_state.borrow();
        let active_tab = tab_state.active_tab.as_deref();
        for entry in &tab_state.tabs {
            let selected = active_tab
                .map(|current| current == entry.id)
                .unwrap_or_else(|| {
                    tab_state
                        .tabs
                        .first()
                        .is_some_and(|first| first.id == entry.id)
                });
            let (kind, cwd, uri) = match &entry.kind {
                TabKind::Terminal { state } => {
                    ("terminal".to_string(), state.cwd.borrow().clone(), None)
                }
                TabKind::Browser { state } => {
                    ("browser".to_string(), None, state.uri.borrow().clone())
                }
                TabKind::CustomSidebar { .. } => ("customSidebar".to_string(), None, None),
                TabKind::Keybinds => ("keybinds".to_string(), None, None),
            };
            surfaces.push(SurfaceSummary {
                pane_id,
                surface_id: composite_surface_id(pane_id, &entry.id),
                title: entry.title_label.label().to_string(),
                kind,
                selected,
                cwd,
                uri,
            });
        }
    }

    surfaces.sort_by(|left, right| {
        left.pane_id
            .cmp(&right.pane_id)
            .then_with(|| right.selected.cmp(&left.selected))
            .then_with(|| left.surface_id.cmp(&right.surface_id))
    });
    surfaces
}

pub fn active_surface_summary(pane_widget: &gtk::Widget) -> Option<SurfaceSummary> {
    let internals = find_pane_internals(pane_widget)?;
    let pane_id = internals.pane_id;
    let tab_state = internals.tab_state.borrow();
    let active_id = tab_state
        .active_tab
        .clone()
        .or_else(|| tab_state.tabs.first().map(|entry| entry.id.clone()))?;
    let entry = tab_state.tabs.iter().find(|entry| entry.id == active_id)?;
    let (kind, cwd, uri) = match &entry.kind {
        TabKind::Terminal { state } => ("terminal".to_string(), state.cwd.borrow().clone(), None),
        TabKind::Browser { state } => ("browser".to_string(), None, state.uri.borrow().clone()),
        TabKind::CustomSidebar { .. } => ("customSidebar".to_string(), None, None),
        TabKind::Keybinds => ("keybinds".to_string(), None, None),
    };
    Some(SurfaceSummary {
        pane_id,
        surface_id: composite_surface_id(pane_id, &entry.id),
        title: entry.title_label.label().to_string(),
        kind,
        selected: true,
        cwd,
        uri,
    })
}

// purpose: Resolve the live pane internals and tab id for a CMUX tab action.
// inputs: Workspace root and optional surface hint.
// returns/effects: Returns the active or addressed tab without mutating state.
fn tab_action_target_for_root(
    root: &gtk::Widget,
    surface_hint: Option<&str>,
) -> Option<(Rc<PaneInternals>, String)> {
    let requested = surface_hint
        .map(normalize_surface_hint)
        .filter(|value| !value.is_empty());

    for internals in pane_internals_for_root(root) {
        let target_tab_id = {
            let tab_state = internals.tab_state.borrow();
            let active_tab = tab_state.active_tab.as_deref();
            tab_state.tabs.iter().find_map(|entry| {
                let surface_id = composite_surface_id(internals.pane_id, &entry.id);
                let matched = requested
                    .map(|hint| surface_hint_matches(&surface_id, &entry.id, hint))
                    .unwrap_or(active_tab == Some(entry.id.as_str()));
                matched.then(|| entry.id.clone())
            })
        };
        let Some(tab_id) = target_tab_id else {
            continue;
        };
        return Some((internals, tab_id));
    }
    None
}

// purpose: Apply one supported CMUX metadata action to a resolved tab.
// inputs: Pane internals, tab id, normalized action key, and optional title.
// returns/effects: Mutates tab metadata and returns the new pinned state.
fn apply_tab_metadata_action(
    internals: &Rc<PaneInternals>,
    tab_id: &str,
    action_key: &str,
    title: Option<&str>,
) -> Option<bool> {
    let mut tab_state = internals.tab_state.borrow_mut();
    let entry = tab_state.find_tab_mut(tab_id)?;
    match action_key {
        "rename" => {
            let new_title = title?;
            entry.custom_name = Some(new_title.to_string());
            entry.title_label.set_label(new_title);
        }
        "clear_name" => {
            entry.custom_name = None;
            entry.title_label.set_label(&entry.id);
        }
        "pin" => {
            entry.pinned = true;
            apply_pin_visuals(&entry.tab_button, true);
        }
        "unpin" => {
            entry.pinned = false;
            apply_pin_visuals(&entry.tab_button, false);
        }
        _ => return None,
    }
    Some(entry.pinned)
}

// purpose: Close tabs to the left, right, or all others around an anchor tab.
// inputs: Pane internals, anchor tab id, and normalized close action key.
// returns/effects: Removes non-pinned target tabs and reports closed/skipped counts.
fn close_relative_tabs(
    internals: &Rc<PaneInternals>,
    tab_id: &str,
    action_key: &str,
) -> Vec<SurfaceSummary> {
    let target_ids = {
        let tab_state = internals.tab_state.borrow();
        let Some(anchor_index) = tab_state.tabs.iter().position(|entry| entry.id == tab_id) else {
            return Vec::new();
        };
        tab_state
            .tabs
            .iter()
            .enumerate()
            .filter_map(|(index, entry)| {
                let close = match action_key {
                    "close_left" => index < anchor_index,
                    "close_right" => index > anchor_index,
                    "close_others" => index != anchor_index,
                    _ => false,
                };
                (close && !entry.pinned).then(|| entry.id.clone())
            })
            .collect::<Vec<_>>()
    };
    close_tabs_by_id(internals, target_ids)
}

// purpose: Count pinned tabs skipped by a relative close action.
// inputs: Pane internals, anchor tab id, and normalized close action key.
// returns/effects: Returns the number of target tabs left open because they are pinned.
fn skipped_pinned_relative_tabs(
    internals: &Rc<PaneInternals>,
    tab_id: &str,
    action_key: &str,
) -> usize {
    let tab_state = internals.tab_state.borrow();
    let Some(anchor_index) = tab_state.tabs.iter().position(|entry| entry.id == tab_id) else {
        return 0;
    };
    tab_state
        .tabs
        .iter()
        .enumerate()
        .filter(|(index, entry)| {
            let close = match action_key {
                "close_left" => *index < anchor_index,
                "close_right" => *index > anchor_index,
                "close_others" => *index != anchor_index,
                _ => false,
            };
            close && entry.pinned
        })
        .count()
}

// purpose: Remove selected tab ids and return summaries captured before removal.
// inputs: Pane internals and tab ids in the pane.
// returns/effects: Removes tabs from GTK state and invokes existing empty-pane behavior.
fn close_tabs_by_id(internals: &Rc<PaneInternals>, tab_ids: Vec<String>) -> Vec<SurfaceSummary> {
    let mut closed = Vec::new();
    for tab_id in tab_ids {
        if let Some(summary) = surface_summary_for_tab(internals, &tab_id) {
            remove_tab(
                &internals.tab_strip,
                &internals.content_stack,
                &internals.tab_state,
                &tab_id,
                &internals.callbacks,
                &internals.pane_outer,
                PaneEmptyReason::ClosedLastTab,
            );
            closed.push(summary);
        }
    }
    closed
}

// purpose: Duplicate a browser tab in the same pane.
// inputs: Pane internals and addressed browser tab id.
// returns/effects: Creates a new browser tab with the same URI and returns its summary.
fn duplicate_browser_tab(
    internals: &Rc<PaneInternals>,
    tab_id: &str,
) -> Result<SurfaceSummary, TabActionError> {
    let uri = {
        let tab_state = internals.tab_state.borrow();
        let entry = tab_state
            .tabs
            .iter()
            .find(|entry| entry.id == tab_id)
            .ok_or(TabActionError::NotFound)?;
        match &entry.kind {
            TabKind::Browser { state } => state.uri.borrow().clone(),
            TabKind::Terminal { .. } | TabKind::CustomSidebar { .. } | TabKind::Keybinds => {
                return Err(TabActionError::UnsupportedForSurface);
            }
        }
    };
    let options = BrowserTabOptions {
        id: None,
        custom_name: None,
        pinned: false,
        uri: uri.as_deref(),
    };
    let new_tab_id = add_browser_tab_inner(internals, Some(options));
    surface_summary_for_tab(internals, &new_tab_id).ok_or(TabActionError::NotFound)
}

// purpose: Reload an addressed browser tab.
// inputs: Pane internals and addressed tab id.
// returns/effects: Invokes WebKit reload when the surface is a browser.
fn reload_browser_tab(internals: &Rc<PaneInternals>, tab_id: &str) -> Result<(), TabActionError> {
    let tab_state = internals.tab_state.borrow();
    let entry = tab_state
        .tabs
        .iter()
        .find(|entry| entry.id == tab_id)
        .ok_or(TabActionError::NotFound)?;
    match &entry.kind {
        TabKind::Browser { state } => state
            .handles
            .reload()
            .then_some(())
            .ok_or(TabActionError::NotFound),
        TabKind::Terminal { .. } | TabKind::CustomSidebar { .. } | TabKind::Keybinds => {
            Err(TabActionError::UnsupportedForSurface)
        }
    }
}

type TabActionMutation = (Option<SurfaceSummary>, Vec<SurfaceSummary>, bool);

// purpose: Execute the mutating portion of a normalized CMUX tab action.
// inputs: Pane internals, tab id, action key, and optional title for rename.
// returns/effects: Mutates tab/browser state and returns created, closed, and reload outputs.
fn apply_tab_action_mutation(
    internals: &Rc<PaneInternals>,
    tab_id: &str,
    action_key: &str,
    title: Option<&str>,
) -> Result<TabActionMutation, TabActionError> {
    if matches!(action_key, "rename" | "clear_name" | "pin" | "unpin") {
        apply_tab_metadata_action(internals, tab_id, action_key, title)
            .ok_or(TabActionError::NotFound)?;
        return Ok((None, Vec::new(), false));
    }
    if matches!(action_key, "close_left" | "close_right" | "close_others") {
        return Ok((
            None,
            close_relative_tabs(internals, tab_id, action_key),
            false,
        ));
    }
    if matches!(action_key, "mark_read" | "mark_unread" | "mark_as_unread") {
        return Ok((None, Vec::new(), false));
    }
    if action_key == "duplicate" {
        return Ok((
            Some(duplicate_browser_tab(internals, tab_id)?),
            Vec::new(),
            false,
        ));
    }
    if action_key == "reload" {
        reload_browser_tab(internals, tab_id)?;
        return Ok((None, Vec::new(), true));
    }
    Err(TabActionError::UnsupportedForSurface)
}

// purpose: Apply supported CMUX tab actions to a live tab.
// inputs: Workspace root, optional surface hint, normalized action key, and optional title.
// returns/effects: Mutates tab state or browser state and returns updated action metadata.
pub fn apply_tab_action_for_root(
    root: &gtk::Widget,
    surface_hint: Option<&str>,
    action_key: &str,
    title: Option<&str>,
) -> Result<TabActionSummary, TabActionError> {
    let (internals, tab_id) =
        tab_action_target_for_root(root, surface_hint).ok_or(TabActionError::NotFound)?;
    let skipped_pinned = skipped_pinned_relative_tabs(&internals, &tab_id, action_key);
    let (created, closed, reloaded) =
        apply_tab_action_mutation(&internals, &tab_id, action_key, title)?;
    (internals.callbacks.on_state_changed)();
    let surface = surface_summary_for_tab(&internals, &tab_id).ok_or(TabActionError::NotFound)?;
    let pinned = internals
        .tab_state
        .borrow()
        .tabs
        .iter()
        .find(|entry| entry.id == tab_id)
        .map(|entry| entry.pinned)
        .unwrap_or(false);
    Ok(TabActionSummary {
        surface,
        pinned,
        created,
        closed,
        skipped_pinned,
        reloaded,
    })
}

/// purpose: Focus the active tab in a pane identified by pane id.
/// inputs: root is the workspace root widget and pane_id is a live Limux pane id.
/// returns/effects: Activates and focuses the pane's active tab when the pane exists.
pub fn focus_pane_for_root(root: &gtk::Widget, pane_id: u32) -> Option<SurfaceSummary> {
    for internals in pane_internals_for_root(root) {
        if internals.pane_id != pane_id {
            continue;
        }
        let pane_widget: gtk::Widget = internals.pane_outer.clone().upcast();
        if !focus_active_tab_in_pane(&pane_widget) {
            return None;
        }
        return active_surface_summary(&pane_widget);
    }
    None
}

/// purpose: Focus a surface by surface handle within a workspace root.
/// inputs: root is the workspace root widget and surface_hint is a raw or `surface:` handle.
/// returns/effects: Activates and focuses the matching tab when present.
pub fn focus_surface_for_root(root: &gtk::Widget, surface_hint: &str) -> Option<SurfaceSummary> {
    let requested = normalize_surface_hint(surface_hint);
    if requested.is_empty() {
        return None;
    }

    for internals in pane_internals_for_root(root) {
        let target_tab_id = {
            let tab_state = internals.tab_state.borrow();
            tab_state.tabs.iter().find_map(|entry| {
                let surface_id = composite_surface_id(internals.pane_id, &entry.id);
                surface_hint_matches(&surface_id, &entry.id, requested).then(|| entry.id.clone())
            })
        };
        let Some(tab_id) = target_tab_id else {
            continue;
        };
        activate_tab(
            &internals.tab_strip,
            &internals.content_stack,
            &internals.tab_state,
            &tab_id,
        );
        (internals.callbacks.on_state_changed)();
        let pane_widget: gtk::Widget = internals.pane_outer.clone().upcast();
        return active_surface_summary(&pane_widget);
    }
    None
}

// purpose: Close a surface by raw or `surface:` handle within a workspace root.
// inputs: root is the workspace root widget and surface_hint identifies a live tab.
// returns/effects: Removes the tab, invokes pane-empty callbacks when needed, and returns the closed surface.
pub fn close_surface_for_root(
    root: &gtk::Widget,
    surface_hint: &str,
) -> Result<SurfaceSummary, SurfaceCloseError> {
    let requested = normalize_surface_hint(surface_hint);
    if requested.is_empty() {
        return Err(SurfaceCloseError::NotFound);
    }

    for internals in pane_internals_for_root(root) {
        let target_tab_id = {
            let tab_state = internals.tab_state.borrow();
            tab_state.tabs.iter().find_map(|entry| {
                let surface_id = composite_surface_id(internals.pane_id, &entry.id);
                surface_hint_matches(&surface_id, &entry.id, requested).then(|| entry.id.clone())
            })
        };
        let Some(tab_id) = target_tab_id else {
            continue;
        };
        let Some(summary) = surface_summary_for_tab(&internals, &tab_id) else {
            return Err(SurfaceCloseError::NotFound);
        };
        remove_tab(
            &internals.tab_strip,
            &internals.content_stack,
            &internals.tab_state,
            &tab_id,
            &internals.callbacks,
            &internals.pane_outer,
            PaneEmptyReason::ClosedLastTab,
        );
        return Ok(summary);
    }
    Err(SurfaceCloseError::NotFound)
}

// purpose: List browser tabs in the same pane as an addressed browser surface.
// inputs: Workspace root and a raw or `surface:` browser surface hint.
// returns/effects: Returns browser-only surface summaries without changing focus.
pub fn browser_tab_summaries_for_root(
    root: &gtk::Widget,
    surface_hint: &str,
) -> Option<Vec<SurfaceSummary>> {
    let requested = normalize_surface_hint(surface_hint);
    if requested.is_empty() {
        return None;
    }

    for internals in pane_internals_for_root(root) {
        let tab_state = internals.tab_state.borrow();
        let has_context = tab_state.tabs.iter().any(|entry| {
            let surface_id = composite_surface_id(internals.pane_id, &entry.id);
            surface_hint_matches(&surface_id, &entry.id, requested)
                && matches!(entry.kind, TabKind::Browser { .. })
        });
        if !has_context {
            continue;
        }
        let tabs = tab_state
            .tabs
            .iter()
            .filter_map(|entry| match &entry.kind {
                TabKind::Browser { state } => Some(SurfaceSummary {
                    pane_id: internals.pane_id,
                    surface_id: composite_surface_id(internals.pane_id, &entry.id),
                    title: entry.title_label.label().to_string(),
                    kind: "browser".to_string(),
                    selected: tab_state.active_tab.as_deref() == Some(entry.id.as_str()),
                    cwd: None,
                    uri: state.uri.borrow().clone(),
                }),
                TabKind::Terminal { .. } | TabKind::CustomSidebar { .. } | TabKind::Keybinds => {
                    None
                }
            })
            .collect::<Vec<_>>();
        return Some(tabs);
    }
    None
}

// purpose: Create a browser tab beside an addressed browser surface.
// inputs: Workspace root, context browser surface hint, and optional initial URI.
// returns/effects: Adds and activates the browser tab, returning its surface summary.
pub fn add_browser_tab_for_root(
    root: &gtk::Widget,
    surface_hint: &str,
    uri: Option<&str>,
) -> Option<SurfaceSummary> {
    let requested = normalize_surface_hint(surface_hint);
    if requested.is_empty() {
        return None;
    }

    for internals in pane_internals_for_root(root) {
        let has_context = {
            let tab_state = internals.tab_state.borrow();
            tab_state.tabs.iter().any(|entry| {
                let surface_id = composite_surface_id(internals.pane_id, &entry.id);
                surface_hint_matches(&surface_id, &entry.id, requested)
                    && matches!(entry.kind, TabKind::Browser { .. })
            })
        };
        if !has_context {
            continue;
        }
        let pane_widget: gtk::Widget = internals.pane_outer.clone().upcast();
        return add_browser_tab_to_pane_with_uri(&pane_widget, uri);
    }
    None
}

// purpose: Close a browser tab in the same pane as an addressed browser surface.
// inputs: Workspace root, context browser hint, and optional explicit target hint.
// returns/effects: Removes the browser tab or reports context, target, or last-tab errors.
pub fn close_browser_tab_for_root(
    root: &gtk::Widget,
    context_surface_hint: &str,
    target_surface_hint: Option<&str>,
) -> Result<SurfaceSummary, BrowserTabCloseError> {
    let requested_context = normalize_surface_hint(context_surface_hint);
    let requested_target = target_surface_hint
        .map(normalize_surface_hint)
        .filter(|value| !value.is_empty())
        .unwrap_or(requested_context);
    if requested_context.is_empty() || requested_target.is_empty() {
        return Err(BrowserTabCloseError::ContextNotFound);
    }

    for internals in pane_internals_for_root(root) {
        let (has_context, target_id, browser_count) = {
            let tab_state = internals.tab_state.borrow();
            let has_context = tab_state.tabs.iter().any(|entry| {
                let surface_id = composite_surface_id(internals.pane_id, &entry.id);
                surface_hint_matches(&surface_id, &entry.id, requested_context)
                    && matches!(entry.kind, TabKind::Browser { .. })
            });
            let target_id = tab_state.tabs.iter().find_map(|entry| {
                let surface_id = composite_surface_id(internals.pane_id, &entry.id);
                let matches_target = surface_hint_matches(&surface_id, &entry.id, requested_target);
                (matches_target && matches!(entry.kind, TabKind::Browser { .. }))
                    .then(|| entry.id.clone())
            });
            let browser_count = tab_state
                .tabs
                .iter()
                .filter(|entry| matches!(entry.kind, TabKind::Browser { .. }))
                .count();
            (has_context, target_id, browser_count)
        };
        if !has_context {
            continue;
        }
        let Some(tab_id) = target_id else {
            return Err(BrowserTabCloseError::TargetNotFound);
        };
        if browser_count <= 1 {
            return Err(BrowserTabCloseError::LastBrowserTab);
        }
        let Some(summary) = surface_summary_for_tab(&internals, &tab_id) else {
            return Err(BrowserTabCloseError::TargetNotFound);
        };
        remove_tab(
            &internals.tab_strip,
            &internals.content_stack,
            &internals.tab_state,
            &tab_id,
            &internals.callbacks,
            &internals.pane_outer,
            PaneEmptyReason::ClosedLastTab,
        );
        return Ok(summary);
    }
    Err(BrowserTabCloseError::ContextNotFound)
}

/// purpose: Resolve a browser surface target inside one workspace root.
/// inputs: root is the workspace root widget; surface_hint targets a browser tab.
/// returns/effects: Returns browser control handles without mutating focus.
pub fn browser_target_for_root(
    root: &gtk::Widget,
    surface_hint: &str,
) -> Option<BrowserSurfaceTarget> {
    let requested = normalize_surface_hint(surface_hint);
    if requested.is_empty() {
        return None;
    }

    for internals in pane_internals_for_root(root) {
        let tab_state = internals.tab_state.borrow();
        for entry in &tab_state.tabs {
            let surface_id = composite_surface_id(internals.pane_id, &entry.id);
            if !surface_hint_matches(&surface_id, &entry.id, requested) {
                continue;
            }
            let TabKind::Browser { state } = &entry.kind else {
                return None;
            };
            let surface = SurfaceSummary {
                pane_id: internals.pane_id,
                surface_id,
                title: entry.title_label.label().to_string(),
                kind: "browser".to_string(),
                selected: tab_state.active_tab.as_deref() == Some(entry.id.as_str()),
                cwd: None,
                uri: state.uri.borrow().clone(),
            };
            let target = BrowserShortcutTarget {
                uri: state.uri.clone(),
                handles: state.handles.clone(),
            };
            return Some(BrowserSurfaceTarget { surface, target });
        }
    }
    None
}

pub fn terminal_handle_for_root(
    root: &gtk::Widget,
    surface_hint: Option<&str>,
) -> Option<(String, terminal::TerminalHandle)> {
    let requested = surface_hint
        .map(normalize_surface_hint)
        .filter(|value| !value.is_empty());

    if let Some(requested) = requested {
        for internals in pane_internals_for_root(root) {
            let pane_widget: gtk::Widget = internals.pane_outer.clone().upcast();
            if let Some((surface_id, handle)) =
                terminal_handle_for_surface(&pane_widget, Some(requested))
            {
                if normalize_surface_hint(&surface_id) == requested {
                    return Some((surface_id, handle));
                }
            }
        }
        return None;
    }

    pane_internals_for_root(root)
        .into_iter()
        .find_map(|internals| {
            let pane_widget: gtk::Widget = internals.pane_outer.clone().upcast();
            terminal_handle_for_surface(&pane_widget, None)
        })
}

pub fn move_tab_to_pane(
    source_pane: &gtk::Widget,
    tab_id: &str,
    target_pane: &gtk::Widget,
) -> bool {
    move_tab_to_pane_at(source_pane, tab_id, target_pane, None)
}

// purpose: Move one tab between live panes with an optional destination index.
// inputs: Source pane widget, tab id, target pane widget, and optional target insertion index.
// returns/effects: Transfers the tab, activates it in the target pane, and updates pane state.
pub fn move_tab_to_pane_at(
    source_pane: &gtk::Widget,
    tab_id: &str,
    target_pane: &gtk::Widget,
    index: Option<usize>,
) -> bool {
    let Some(source) = find_pane_internals(source_pane) else {
        return false;
    };
    let Some(target) = find_pane_internals(target_pane) else {
        return false;
    };
    let insert_idx = index.unwrap_or_else(|| target.tab_state.borrow().tabs.len());
    transfer_tab_between_panes(&source, &target, tab_id, insert_idx)
}

// purpose: Move a surface to another pane within the same workspace root.
// inputs: Workspace root, source surface handle, target pane id, and optional insertion index.
// returns/effects: Moves and focuses the surface, returning its new summary.
pub fn move_surface_for_root(
    root: &gtk::Widget,
    surface_hint: &str,
    target_pane_id: u32,
    index: Option<usize>,
) -> Option<SurfaceSummary> {
    let requested = normalize_surface_hint(surface_hint);
    if requested.is_empty() {
        return None;
    }

    for internals in pane_internals_for_root(root) {
        let tab_id = {
            let tab_state = internals.tab_state.borrow();
            tab_state.tabs.iter().find_map(|entry| {
                let surface_id = composite_surface_id(internals.pane_id, &entry.id);
                surface_hint_matches(&surface_id, &entry.id, requested).then(|| entry.id.clone())
            })
        };
        let Some(tab_id) = tab_id else {
            continue;
        };
        let source_pane: gtk::Widget = internals.pane_outer.clone().upcast();
        let target_pane = pane_widget_for_root(root, target_pane_id)?;
        if !move_tab_to_pane_at(&source_pane, &tab_id, &target_pane, index) {
            return None;
        }
        return active_surface_summary(&target_pane);
    }
    None
}

// purpose: Resolve a live surface to the pane widget and tab id that own it.
// inputs: Workspace root and surface handle or tab id.
// returns/effects: Returns source pane widget plus tab id without mutating state.
pub fn surface_source_for_root(
    root: &gtk::Widget,
    surface_hint: &str,
) -> Option<(gtk::Widget, String)> {
    let requested = normalize_surface_hint(surface_hint);
    if requested.is_empty() {
        return None;
    }

    for internals in pane_internals_for_root(root) {
        let tab_id = {
            let tab_state = internals.tab_state.borrow();
            tab_state.tabs.iter().find_map(|entry| {
                let surface_id = composite_surface_id(internals.pane_id, &entry.id);
                surface_hint_matches(&surface_id, &entry.id, requested).then(|| entry.id.clone())
            })
        };
        if let Some(tab_id) = tab_id {
            return Some((internals.pane_outer.clone().upcast(), tab_id));
        }
    }
    None
}

// purpose: Reorder a surface within its current pane.
// inputs: Workspace root, source surface handle, and one target index/before/after hint.
// returns/effects: Reorders the tab strip and returns the reordered surface summary.
pub fn reorder_surface_for_root(
    root: &gtk::Widget,
    surface_hint: &str,
    index: Option<usize>,
    before_surface_hint: Option<&str>,
    after_surface_hint: Option<&str>,
) -> Option<SurfaceSummary> {
    let requested = normalize_surface_hint(surface_hint);
    if requested.is_empty() {
        return None;
    }

    for internals in pane_internals_for_root(root) {
        let tab_id = {
            let tab_state = internals.tab_state.borrow();
            tab_state.tabs.iter().find_map(|entry| {
                let surface_id = composite_surface_id(internals.pane_id, &entry.id);
                surface_hint_matches(&surface_id, &entry.id, requested).then(|| entry.id.clone())
            })
        };
        let Some(tab_id) = tab_id else {
            continue;
        };
        let insert_idx = {
            let tab_state = internals.tab_state.borrow();
            if let Some(index) = index {
                index
            } else if let Some(before_hint) = before_surface_hint.map(normalize_surface_hint) {
                tab_state.tabs.iter().position(|entry| {
                    let surface_id = composite_surface_id(internals.pane_id, &entry.id);
                    surface_hint_matches(&surface_id, &entry.id, before_hint)
                })?
            } else if let Some(after_hint) = after_surface_hint.map(normalize_surface_hint) {
                tab_state
                    .tabs
                    .iter()
                    .position(|entry| {
                        let surface_id = composite_surface_id(internals.pane_id, &entry.id);
                        surface_hint_matches(&surface_id, &entry.id, after_hint)
                    })?
                    .saturating_add(1)
            } else {
                return None;
            }
        };
        let _ = reorder_tab_to_index(
            &internals.tab_strip,
            &internals.tab_state,
            &internals.callbacks,
            &tab_id,
            insert_idx,
        );
        return surface_summary_for_tab(&internals, &tab_id);
    }
    None
}

// purpose: Refresh terminal surface rendering inside one workspace root.
// inputs: Workspace root and an optional surface handle to narrow the refresh.
// returns/effects: Calls Ghostty refresh on matching terminal surfaces and returns summaries.
pub fn refresh_terminal_surfaces_for_root(
    root: &gtk::Widget,
    surface_hint: Option<&str>,
) -> Vec<SurfaceSummary> {
    let requested = surface_hint.map(normalize_surface_hint);
    if requested.is_some_and(str::is_empty) {
        return Vec::new();
    }

    let mut refreshed = Vec::new();
    for internals in pane_internals_for_root(root) {
        let matches = {
            let tab_state = internals.tab_state.borrow();
            tab_state
                .tabs
                .iter()
                .filter_map(|entry| {
                    let TabKind::Terminal { state } = &entry.kind else {
                        return None;
                    };
                    let surface_id = composite_surface_id(internals.pane_id, &entry.id);
                    let requested_matches = requested
                        .map(|hint| surface_hint_matches(&surface_id, &entry.id, hint))
                        .unwrap_or(true);
                    requested_matches.then(|| (entry.id.clone(), state.handle.clone()))
                })
                .collect::<Vec<_>>()
        };
        for (tab_id, handle) in matches {
            handle.refresh_display();
            if let Some(summary) = surface_summary_for_tab(&internals, &tab_id) {
                refreshed.push(summary);
            }
        }
    }
    refreshed
}

// purpose: Clear one terminal surface's scrollback and visible buffer.
// inputs: Workspace root and an optional raw or `surface:` surface handle.
// returns/effects: Invokes Ghostty's clear_screen binding on the addressed terminal.
pub fn clear_terminal_history_for_root(
    root: &gtk::Widget,
    surface_hint: Option<&str>,
) -> Option<SurfaceSummary> {
    let requested = surface_hint
        .map(normalize_surface_hint)
        .filter(|value| !value.is_empty());

    for internals in pane_internals_for_root(root) {
        let target = {
            let tab_state = internals.tab_state.borrow();
            let active_tab = tab_state.active_tab.as_deref();
            let mut fallback = None;
            for entry in &tab_state.tabs {
                let TabKind::Terminal { state } = &entry.kind else {
                    continue;
                };
                let surface_id = composite_surface_id(internals.pane_id, &entry.id);
                if requested.is_some_and(|hint| surface_hint_matches(&surface_id, &entry.id, hint))
                {
                    fallback = Some((entry.id.clone(), state.handle.clone()));
                    break;
                }
                if requested.is_some() {
                    continue;
                }
                if active_tab == Some(entry.id.as_str()) {
                    fallback = Some((entry.id.clone(), state.handle.clone()));
                    break;
                }
                if fallback.is_none() {
                    fallback = Some((entry.id.clone(), state.handle.clone()));
                }
            }
            fallback
        };

        if let Some((tab_id, handle)) = target {
            return clear_terminal_tab(&internals, &tab_id, &handle);
        }
    }
    None
}

// purpose: Execute the terminal clear action and summarize the affected tab.
// inputs: Pane internals, terminal tab id, and terminal handle.
// returns/effects: Returns the surface summary only when Ghostty accepted the action.
fn clear_terminal_tab(
    internals: &Rc<PaneInternals>,
    tab_id: &str,
    handle: &terminal::TerminalHandle,
) -> Option<SurfaceSummary> {
    handle
        .perform_binding_action("clear_screen")
        .then(|| surface_summary_for_tab(internals, tab_id))
        .flatten()
}

pub fn focused_shortcut_target(pane_widget: &gtk::Widget) -> FocusedShortcutTarget {
    let Some(internals) = find_pane_internals(pane_widget) else {
        return FocusedShortcutTarget::None;
    };

    let target = {
        let tab_state = internals.tab_state.borrow();
        let Some(active_id) = tab_state.active_tab.as_deref() else {
            return FocusedShortcutTarget::None;
        };
        match tab_state.tabs.iter().find(|entry| entry.id == active_id) {
            Some(TabEntry {
                kind: TabKind::Terminal { state },
                ..
            }) => FocusedShortcutTarget::Terminal(TerminalShortcutTarget {
                handle: state.handle.clone(),
            }),
            Some(TabEntry {
                kind: TabKind::Browser { state },
                ..
            }) => FocusedShortcutTarget::Browser(BrowserShortcutTarget {
                uri: state.uri.clone(),
                handles: state.handles.clone(),
            }),
            Some(TabEntry {
                kind: TabKind::Keybinds,
                ..
            }) => FocusedShortcutTarget::Keybinds,
            Some(TabEntry {
                kind: TabKind::CustomSidebar { .. },
                ..
            }) => FocusedShortcutTarget::None,
            None => FocusedShortcutTarget::None,
        }
    };

    target
}

fn apply_pin_visuals(tab_button: &gtk::Box, pinned: bool) {
    if let Some(close_widget) = tab_button.last_child() {
        close_widget.set_visible(!pinned);
    }
    if let Some(inner_box) = tab_button
        .first_child()
        .and_then(|child| child.downcast::<gtk::Box>().ok())
    {
        if let Some(pin_icon) = inner_box
            .first_child()
            .and_then(|child| child.downcast::<gtk::Label>().ok())
        {
            pin_icon.set_label(if pinned { "📌" } else { "" });
            pin_icon.set_visible(pinned);
        }
    }
}

// ---------------------------------------------------------------------------
// Tab button (label + close)
// ---------------------------------------------------------------------------

fn new_tab_title_label(title: &str) -> gtk::Label {
    let label = gtk::Label::builder()
        .label(title)
        .ellipsize(gtk::pango::EllipsizeMode::End)
        .max_width_chars(20)
        .build();
    label.set_can_target(false);
    label
}

fn build_tab_button(
    title: &str,
    tab_id: &str,
    internals: &Rc<PaneInternals>,
) -> (gtk::Box, gtk::Label) {
    let label = new_tab_title_label(title);
    let tab_button = build_tab_button_from_label(&label, tab_id, internals);
    (tab_button, label)
}

fn build_tab_button_from_label(
    label: &gtk::Label,
    tab_id: &str,
    internals: &Rc<PaneInternals>,
) -> gtk::Box {
    if let Some(parent) = label
        .parent()
        .and_then(|parent| parent.downcast::<gtk::Box>().ok())
    {
        parent.remove(label);
    }

    let pin_icon = gtk::Label::new(None);
    pin_icon.add_css_class("limux-pin-icon");
    pin_icon.set_visible(false);
    pin_icon.set_can_target(false);

    let close_btn = gtk::Button::builder()
        .icon_name("window-close-symbolic")
        .has_frame(false)
        .build();
    close_btn.add_css_class("limux-tab-close");

    let inner_box = gtk::Box::new(gtk::Orientation::Horizontal, 2);
    inner_box.set_can_target(false);
    inner_box.append(&pin_icon);
    inner_box.append(label);

    let tab_btn = gtk::Box::new(gtk::Orientation::Horizontal, 0);
    tab_btn.add_css_class("limux-tab");
    tab_btn.append(&inner_box);
    tab_btn.append(&close_btn);

    let click = gtk::GestureClick::new();
    click.set_button(1);
    {
        let tab_id = tab_id.to_string();
        let tab_strip = internals.tab_strip.clone();
        let content_stack = internals.content_stack.clone();
        let tab_state = internals.tab_state.clone();
        let callbacks = internals.callbacks.clone();
        click.connect_pressed(move |_, _, _, _| {
            activate_tab(&tab_strip, &content_stack, &tab_state, &tab_id);
            (callbacks.on_state_changed)();
        });
    }
    tab_btn.add_controller(click);

    let right_click = gtk::GestureClick::new();
    right_click.set_button(3);
    {
        let tab_id = tab_id.to_string();
        let context = TabContextMenuContext {
            tab_strip: internals.tab_strip.clone(),
            content_stack: internals.content_stack.clone(),
            tab_state: internals.tab_state.clone(),
            callbacks: internals.callbacks.clone(),
            pane_outer: internals.pane_outer.clone(),
            label: label.clone(),
            pin_icon: pin_icon.clone(),
        };
        let tab_button = tab_btn.clone();
        right_click.connect_pressed(move |_, _, _, _| {
            show_tab_context_menu(&tab_button, &tab_id, &context);
        });
    }
    tab_btn.add_controller(right_click);

    let drag_source = gtk::DragSource::new();
    drag_source.set_actions(gtk::gdk::DragAction::MOVE);
    {
        let tab_id = tab_id.to_string();
        let pane_id = internals.pane_id;
        drag_source.connect_prepare(move |_src, _x, _y| {
            let payload = glib::Value::from(&TabDragPayload::new(pane_id, &tab_id).encode());
            Some(gtk::gdk::ContentProvider::for_value(&payload))
        });
    }
    {
        let drop_indicator = internals.drop_indicator.clone();
        let tab_state = internals.tab_state.clone();
        drag_source.connect_drag_begin(move |source, _drag| {
            set_tab_dragging(true);
            if let Some(widget) = source.widget() {
                let allocation = widget.allocation();
                position_indicator(
                    &tab_state,
                    &drop_indicator,
                    (allocation.x() + allocation.width()) as f64,
                );
                let icon = gtk::WidgetPaintable::new(Some(&widget));
                source.set_icon(Some(&icon), 0, 0);
            }
        });
    }
    {
        let drop_indicator = internals.drop_indicator.clone();
        let content_overlay = internals.content_drop_overlay.clone();
        drag_source.connect_drag_end(move |_, _, _| {
            set_tab_dragging(false);
            drop_indicator.set_visible(false);
            clear_content_drop_zone(&content_overlay);
        });
    }
    tab_btn.add_controller(drag_source);

    {
        let tab_id = tab_id.to_string();
        let tab_strip = internals.tab_strip.clone();
        let content_stack = internals.content_stack.clone();
        let tab_state = internals.tab_state.clone();
        let callbacks = internals.callbacks.clone();
        let pane_outer = internals.pane_outer.clone();
        close_btn.connect_clicked(move |_| {
            let is_pinned = tab_state
                .borrow()
                .tabs
                .iter()
                .any(|entry| entry.id == tab_id && entry.pinned);
            if !is_pinned {
                remove_tab(
                    &tab_strip,
                    &content_stack,
                    &tab_state,
                    &tab_id,
                    &callbacks,
                    &pane_outer,
                    PaneEmptyReason::ClosedLastTab,
                );
            }
        });
    }

    tab_btn
}

fn show_tab_context_menu(tab_btn: &gtk::Box, tab_id: &str, context: &TabContextMenuContext) {
    let menu = gtk::PopoverMenu::from_model(None::<&gtk::gio::MenuModel>);
    let menu_box = gtk::Box::new(gtk::Orientation::Vertical, 2);
    menu_box.set_margin_top(4);
    menu_box.set_margin_bottom(4);
    menu_box.set_margin_start(4);
    menu_box.set_margin_end(4);

    // Rename
    let rename_btn = gtk::Button::with_label("Rename");
    rename_btn.add_css_class("flat");
    {
        let lbl = context.label.clone();
        let state = context.tab_state.clone();
        let tid = tab_id.to_string();
        let menu_ref = menu.clone();
        let callbacks = context.callbacks.clone();
        rename_btn.connect_clicked(move |_| {
            menu_ref.popdown();
            show_rename_dialog(&lbl, &state, &tid, &callbacks);
        });
    }

    // Pin / Unpin
    let is_pinned = context
        .tab_state
        .borrow()
        .tabs
        .iter()
        .any(|e| e.id == tab_id && e.pinned);
    let pin_label = if is_pinned { "Unpin" } else { "Pin" };
    let pin_btn = gtk::Button::with_label(pin_label);
    pin_btn.add_css_class("flat");
    {
        let state = context.tab_state.clone();
        let tid = tab_id.to_string();
        let pin = context.pin_icon.clone();
        let close = tab_btn.last_child(); // close button
        let menu_ref = menu.clone();
        let callbacks = context.callbacks.clone();
        pin_btn.connect_clicked(move |_| {
            menu_ref.popdown();
            let mut ts = state.borrow_mut();
            if let Some(entry) = ts.find_tab_mut(&tid) {
                entry.pinned = !entry.pinned;
                apply_pin_visuals(&entry.tab_button, entry.pinned);
                pin.set_label(if entry.pinned { "📌" } else { "" });
                pin.set_visible(entry.pinned);
                if let Some(close_widget) = &close {
                    close_widget.set_visible(!entry.pinned);
                }
            }
            drop(ts);
            (callbacks.on_state_changed)();
        });
    }

    // Close
    let close_btn = gtk::Button::with_label("Close");
    close_btn.add_css_class("flat");
    {
        let tid = tab_id.to_string();
        let ts = context.tab_strip.clone();
        let cs = context.content_stack.clone();
        let state = context.tab_state.clone();
        let cb = context.callbacks.clone();
        let po = context.pane_outer.clone();
        let menu_ref = menu.clone();
        close_btn.connect_clicked(move |_| {
            menu_ref.popdown();
            remove_tab(
                &ts,
                &cs,
                &state,
                &tid,
                &cb,
                &po,
                PaneEmptyReason::ClosedLastTab,
            );
        });
    }

    menu_box.append(&rename_btn);
    menu_box.append(&pin_btn);
    menu_box.append(&close_btn);
    menu.set_child(Some(&menu_box));
    menu.set_parent(tab_btn);
    menu.set_has_arrow(false);

    // Clean up popover when it closes
    menu.connect_closed(move |popover| {
        popover.unparent();
    });

    menu.popup();
}

fn show_rename_dialog(
    label: &gtk::Label,
    tab_state: &Rc<RefCell<TabState>>,
    tab_id: &str,
    callbacks: &Rc<PaneCallbacks>,
) {
    let current_name = label.label().to_string();

    // Replace label with an entry temporarily
    let parent = label.parent().and_then(|p| p.downcast::<gtk::Box>().ok());
    let Some(parent) = parent else {
        return;
    };

    let entry = gtk::Entry::builder()
        .text(&current_name)
        .width_chars(15)
        .build();
    for css_class in TAB_RENAME_ENTRY_CSS_CLASSES {
        entry.add_css_class(css_class);
    }

    label.set_visible(false);
    // Insert entry before the close button
    parent.insert_child_after(&entry, Some(label));
    entry.grab_focus();
    entry.select_region(0, -1);

    // On activate (Enter) or focus-out, commit rename
    let lbl = label.clone();
    let state = tab_state.clone();
    let tid = tab_id.to_string();
    let parent_for_cleanup = parent.clone();

    let commit = Rc::new(std::cell::Cell::new(false));

    let do_rename = {
        let commit = commit.clone();
        let lbl = lbl.clone();
        let state = state.clone();
        let tid = tid.clone();
        let parent = parent_for_cleanup.clone();
        let callbacks = callbacks.clone();
        move |entry: &gtk::Entry| {
            if commit.get() {
                return;
            }
            commit.set(true);
            let new_name = entry.text().to_string();
            if !new_name.trim().is_empty() {
                lbl.set_label(&new_name);
                let mut ts = state.borrow_mut();
                if let Some(tab) = ts.find_tab_mut(&tid) {
                    tab.custom_name = Some(new_name);
                }
            }
            lbl.set_visible(true);
            parent.remove(entry);
            (callbacks.on_state_changed)();
        }
    };

    {
        let do_rename = do_rename.clone();
        entry.connect_activate(move |e| {
            do_rename(e);
        });
    }
    {
        let do_rename = do_rename.clone();
        let focus_controller = gtk::EventControllerFocus::new();
        focus_controller.connect_leave(move |ctrl| {
            if let Some(widget) = ctrl.widget() {
                if let Some(entry) = widget.downcast_ref::<gtk::Entry>() {
                    do_rename(entry);
                }
            }
        });
        entry.add_controller(focus_controller);
    }
}

fn normalize_reorder_insert_index(source_idx: usize, insert_idx: usize) -> Option<usize> {
    if source_idx == insert_idx || source_idx + 1 == insert_idx {
        return None;
    }
    Some(if source_idx < insert_idx {
        insert_idx - 1
    } else {
        insert_idx
    })
}

fn next_active_after_tab_removal(
    tab_ids: &[&str],
    active_id: Option<&str>,
    removed_idx: usize,
) -> Option<String> {
    if tab_ids.len() <= 1 {
        return None;
    }
    let removed_id = tab_ids.get(removed_idx).copied()?;
    if active_id != Some(removed_id) {
        return active_id.map(ToOwned::to_owned);
    }
    let next_idx = removed_idx.min(tab_ids.len() - 2);
    tab_ids
        .iter()
        .enumerate()
        .find_map(|(idx, tab_id)| (idx != removed_idx).then_some(*tab_id))
        .and_then(|_| {
            tab_ids
                .iter()
                .enumerate()
                .filter_map(|(idx, tab_id)| (idx != removed_idx).then_some(*tab_id))
                .nth(next_idx)
        })
        .map(ToOwned::to_owned)
}

fn classify_content_drop_zone(width: f64, height: f64, x: f64, y: f64) -> Option<ContentDropZone> {
    if width <= 0.0 || height <= 0.0 {
        return None;
    }
    if x < width * 0.25 {
        Some(ContentDropZone::Left)
    } else if x > width * 0.75 {
        Some(ContentDropZone::Right)
    } else if y < height * 0.25 {
        Some(ContentDropZone::Top)
    } else if y > height * 0.75 {
        Some(ContentDropZone::Bottom)
    } else {
        Some(ContentDropZone::Center)
    }
}

fn content_drop_preview_rect(zone: ContentDropZone) -> (f64, f64, f64, f64) {
    match zone {
        ContentDropZone::Left => (0.0, 0.0, 0.5, 1.0),
        ContentDropZone::Right => (0.5, 0.0, 0.5, 1.0),
        ContentDropZone::Top => (0.0, 0.0, 1.0, 0.5),
        ContentDropZone::Bottom => (0.0, 0.5, 1.0, 0.5),
        ContentDropZone::Center => (0.25, 0.25, 0.5, 0.5),
    }
}

fn effective_drop_target_dimensions(
    preview_width: i32,
    preview_height: i32,
    content_width: i32,
    content_height: i32,
) -> Option<(f64, f64)> {
    let width = preview_width.max(content_width);
    let height = preview_height.max(content_height);
    if width <= 0 || height <= 0 {
        return None;
    }
    Some((width as f64, height as f64))
}

fn clear_content_drop_zone(overlay: &gtk::Box) {
    overlay.remove_css_class("limux-drop-preview");
    overlay.remove_css_class("limux-drop-preview-center");
    overlay.set_size_request(-1, -1);
    overlay.set_margin_start(0);
    overlay.set_margin_top(0);
}

fn highlight_content_drop_zone(overlay: &gtk::Box, zone: ContentDropZone) {
    clear_content_drop_zone(overlay);
    overlay.add_css_class("limux-drop-preview");
    if zone == ContentDropZone::Center {
        overlay.add_css_class("limux-drop-preview-center");
    }
    let (x_frac, y_frac, width_frac, height_frac) = content_drop_preview_rect(zone);
    let total_width = overlay
        .parent()
        .map(|parent| parent.allocation().width())
        .unwrap_or_else(|| overlay.width())
        .max(1);
    let total_height = overlay
        .parent()
        .map(|parent| parent.allocation().height())
        .unwrap_or_else(|| overlay.height())
        .max(1);
    overlay.set_margin_start((total_width as f64 * x_frac).round() as i32);
    overlay.set_margin_top((total_height as f64 * y_frac).round() as i32);
    overlay.set_size_request(
        (total_width as f64 * width_frac).round() as i32,
        (total_height as f64 * height_frac).round() as i32,
    );
}

fn position_indicator(tab_state: &Rc<RefCell<TabState>>, indicator: &gtk::Box, x: f64) {
    let tab_state = tab_state.borrow();
    if tab_state.tabs.is_empty() {
        indicator.set_visible(false);
        return;
    }

    let mut position = 0;
    for entry in &tab_state.tabs {
        let allocation = entry.tab_button.allocation();
        let left = allocation.x();
        let right = allocation.x() + allocation.width();
        let midpoint = allocation.x() as f64 + allocation.width() as f64 / 2.0;
        if x < midpoint {
            position = left;
            break;
        }
        position = right;
    }
    indicator.set_margin_start(position);
    indicator.set_visible(true);
}

fn insert_index_for_drop(
    tab_state: &Rc<RefCell<TabState>>,
    x: f64,
    ignored_tab_id: Option<&str>,
) -> usize {
    let tab_state = tab_state.borrow();
    for (idx, entry) in tab_state.tabs.iter().enumerate() {
        if ignored_tab_id == Some(entry.id.as_str()) {
            continue;
        }
        let allocation = entry.tab_button.allocation();
        let midpoint = allocation.x() as f64 + allocation.width() as f64 / 2.0;
        if x < midpoint {
            return idx;
        }
    }
    tab_state.tabs.len()
}

fn rebuild_tab_strip(tab_strip: &gtk::Box, tab_state: &Rc<RefCell<TabState>>) {
    let buttons: Vec<gtk::Box> = tab_state
        .borrow()
        .tabs
        .iter()
        .map(|entry| entry.tab_button.clone())
        .collect();
    for button in &buttons {
        if button.parent().is_some() {
            tab_strip.remove(button);
        }
    }
    for button in &buttons {
        tab_strip.append(button);
    }
}

fn rebind_moved_tab_entry(entry: &mut TabEntry, target: &Rc<PaneInternals>) {
    if let TabKind::Terminal { state } = &entry.kind {
        state.handle.replace_callbacks(make_terminal_callbacks(
            target,
            &entry.id,
            &entry.title_label,
            &state.cwd,
        ));
    }
    entry.tab_button = build_tab_button_from_label(&entry.title_label, &entry.id, target);
    if entry.pinned {
        apply_pin_visuals(&entry.tab_button, true);
    }
}

fn reorder_tab_to_index(
    tab_strip: &gtk::Box,
    tab_state: &Rc<RefCell<TabState>>,
    callbacks: &Rc<PaneCallbacks>,
    source_id: &str,
    insert_idx: usize,
) -> bool {
    let mut state = tab_state.borrow_mut();
    let Some(source_idx) = state.tabs.iter().position(|entry| entry.id == source_id) else {
        return false;
    };
    let Some(normalized_idx) = normalize_reorder_insert_index(source_idx, insert_idx) else {
        return false;
    };
    let entry = state.tabs.remove(source_idx);
    state.tabs.insert(normalized_idx, entry);
    drop(state);
    rebuild_tab_strip(tab_strip, tab_state);
    (callbacks.on_state_changed)();
    true
}

fn transfer_tab_between_panes(
    source: &Rc<PaneInternals>,
    target: &Rc<PaneInternals>,
    tab_id: &str,
    insert_idx: usize,
) -> bool {
    if source.pane_id == target.pane_id {
        return false;
    }

    let (mut entry, source_next_active) = {
        let mut source_state = source.tab_state.borrow_mut();
        let Some(source_idx) = source_state.tabs.iter().position(|item| item.id == tab_id) else {
            return false;
        };
        let all_ids: Vec<&str> = source_state
            .tabs
            .iter()
            .map(|item| item.id.as_str())
            .collect();
        let next_active =
            next_active_after_tab_removal(&all_ids, source_state.active_tab.as_deref(), source_idx);
        (source_state.tabs.remove(source_idx), next_active)
    };

    if let Some(window) = entry
        .content
        .root()
        .and_then(|root| root.downcast::<gtk::Window>().ok())
    {
        gtk::prelude::GtkWindowExt::set_focus(&window, gtk::Widget::NONE);
    }

    if entry.tab_button.parent().is_some() {
        source.tab_strip.remove(&entry.tab_button);
    }
    if entry.content.parent().is_some() {
        source.content_stack.remove(&entry.content);
    }

    rebind_moved_tab_entry(&mut entry, target);
    let moved_tab_id = entry.id.clone();
    target
        .content_stack
        .add_named(&entry.content, Some(&moved_tab_id));

    {
        let mut target_state = target.tab_state.borrow_mut();
        let clamped_idx = insert_idx.min(target_state.tabs.len());
        target_state.tabs.insert(clamped_idx, entry);
    }
    rebuild_tab_strip(&target.tab_strip, &target.tab_state);

    let source_empty = source.tab_state.borrow().tabs.is_empty();
    if source_empty {
        (source.callbacks.on_empty)(
            &source.pane_outer.clone().upcast(),
            PaneEmptyReason::MovedLastTabOut,
        );
    } else if let Some(next_active) = source_next_active {
        activate_tab(
            &source.tab_strip,
            &source.content_stack,
            &source.tab_state,
            &next_active,
        );
    }

    activate_tab(
        &target.tab_strip,
        &target.content_stack,
        &target.tab_state,
        &moved_tab_id,
    );
    (target.callbacks.on_state_changed)();
    true
}

fn install_tab_strip_drop_target(tab_overlay: &gtk::Overlay, internals: &Rc<PaneInternals>) {
    let drop_target = gtk::DropTarget::new(glib::Type::STRING, gtk::gdk::DragAction::MOVE);
    drop_target.set_preload(true);
    {
        let tab_state = internals.tab_state.clone();
        let indicator = internals.drop_indicator.clone();
        let workspace_dragging = internals.workspace_dragging.clone();
        drop_target.connect_motion(move |_, x, _| {
            if workspace_dragging.get() || !is_tab_dragging() {
                indicator.set_visible(false);
                return gtk::gdk::DragAction::empty();
            }
            position_indicator(&tab_state, &indicator, x);
            gtk::gdk::DragAction::MOVE
        });
    }
    {
        let indicator = internals.drop_indicator.clone();
        drop_target.connect_leave(move |_| {
            indicator.set_visible(false);
        });
    }
    {
        let target = internals.clone();
        let indicator = internals.drop_indicator.clone();
        drop_target.connect_drop(move |_, value, x, _| {
            indicator.set_visible(false);
            let Ok(raw) = value.get::<String>() else {
                return false;
            };
            let Some(payload) = TabDragPayload::decode(&raw) else {
                return false;
            };
            let same_pane = payload.pane_id == target.pane_id;
            let insert_idx = insert_index_for_drop(
                &target.tab_state,
                x,
                same_pane.then_some(payload.tab_id.as_str()),
            );
            if same_pane {
                return reorder_tab_to_index(
                    &target.tab_strip,
                    &target.tab_state,
                    &target.callbacks,
                    &payload.tab_id,
                    insert_idx,
                );
            }
            let Some(source) = lookup_pane_internals(payload.pane_id) else {
                return false;
            };
            transfer_tab_between_panes(&source, &target, &payload.tab_id, insert_idx)
        });
    }
    tab_overlay.add_controller(drop_target);
}

fn set_browser_targeting_enabled(content_stack: &gtk::Stack, enabled: bool) {
    let mut child = content_stack.first_child();
    while let Some(widget) = child {
        child = widget.next_sibling();
        if !widget.has_css_class("limux-browser") {
            continue;
        }
        let webview = widget
            .first_child()
            .and_then(|child| child.next_sibling())
            .and_then(|child| child.next_sibling());
        if let Some(webview) = webview {
            webview.set_can_target(enabled);
        }
    }
}

fn install_content_drop_target(internals: &Rc<PaneInternals>) {
    let drop_target = gtk::DropTarget::new(glib::Type::STRING, gtk::gdk::DragAction::MOVE);
    drop_target.set_preload(true);
    {
        let overlay = internals.content_drop_overlay.clone();
        let content_stack = internals.content_stack.clone();
        let workspace_dragging = internals.workspace_dragging.clone();
        drop_target.connect_motion(move |_, x, y| {
            if workspace_dragging.get() || !is_tab_dragging() {
                clear_content_drop_zone(&overlay);
                return gtk::gdk::DragAction::empty();
            }
            let Some((width, height)) = effective_drop_target_dimensions(
                overlay.width(),
                overlay.height(),
                content_stack.allocation().width(),
                content_stack.allocation().height(),
            ) else {
                clear_content_drop_zone(&overlay);
                return gtk::gdk::DragAction::empty();
            };
            let Some(zone) = classify_content_drop_zone(width, height, x, y) else {
                clear_content_drop_zone(&overlay);
                return gtk::gdk::DragAction::empty();
            };
            highlight_content_drop_zone(&overlay, zone);
            gtk::gdk::DragAction::MOVE
        });
    }
    {
        let overlay = internals.content_drop_overlay.clone();
        drop_target.connect_leave(move |_| {
            clear_content_drop_zone(&overlay);
        });
    }
    {
        let target = internals.clone();
        let overlay = internals.content_drop_overlay.clone();
        let content_stack = internals.content_stack.clone();
        drop_target.connect_drop(move |_, value, x, y| {
            clear_content_drop_zone(&overlay);
            let Ok(raw) = value.get::<String>() else {
                return false;
            };
            let Some(payload) = TabDragPayload::decode(&raw) else {
                return false;
            };
            let Some((width, height)) = effective_drop_target_dimensions(
                overlay.width(),
                overlay.height(),
                content_stack.allocation().width(),
                content_stack.allocation().height(),
            ) else {
                return false;
            };
            let Some(zone) = classify_content_drop_zone(width, height, x, y) else {
                return false;
            };
            match zone {
                ContentDropZone::Center => {
                    if payload.pane_id == target.pane_id {
                        return false;
                    }
                    let Some(source) = lookup_pane_internals(payload.pane_id) else {
                        return false;
                    };
                    let insert_idx = target.tab_state.borrow().tabs.len();
                    transfer_tab_between_panes(&source, &target, &payload.tab_id, insert_idx)
                }
                ContentDropZone::Left
                | ContentDropZone::Top
                | ContentDropZone::Right
                | ContentDropZone::Bottom => {
                    let Some(source_widget) = find_pane_widget_by_id(payload.pane_id) else {
                        return false;
                    };
                    let target_widget: gtk::Widget = target.pane_outer.clone().upcast();
                    let (orientation, new_pane_first) = match zone {
                        ContentDropZone::Left => (gtk::Orientation::Horizontal, true),
                        ContentDropZone::Right => (gtk::Orientation::Horizontal, false),
                        ContentDropZone::Top => (gtk::Orientation::Vertical, true),
                        ContentDropZone::Bottom => (gtk::Orientation::Vertical, false),
                        ContentDropZone::Center => unreachable!(),
                    };
                    (target.callbacks.on_split_with_tab)(
                        &source_widget,
                        &target_widget,
                        orientation,
                        payload.tab_id.clone(),
                        new_pane_first,
                    );
                    true
                }
            }
        });
    }
    internals.content_stack.add_controller(drop_target);

    let overlay = internals.content_drop_overlay.clone();
    let content_stack = internals.content_stack.clone();
    let workspace_dragging = internals.workspace_dragging.clone();
    let listener_id = on_tab_drag_change(move |dragging| {
        let visible = dragging && !workspace_dragging.get();
        overlay.set_visible(visible);
        if !visible {
            clear_content_drop_zone(&overlay);
        }
        set_browser_targeting_enabled(&content_stack, !dragging);
    });
    internals.pane_outer.connect_destroy(move |_| {
        remove_tab_drag_listener(listener_id);
    });
}

// ---------------------------------------------------------------------------
// Tab activation / removal
// ---------------------------------------------------------------------------

fn activate_tab(
    _tab_strip: &gtk::Box,
    content_stack: &gtk::Stack,
    tab_state: &Rc<RefCell<TabState>>,
    tab_id: &str,
) {
    let mut ts = tab_state.borrow_mut();
    ts.active_tab = Some(tab_id.to_string());

    // Update visual state on all tabs
    for entry in &ts.tabs {
        if entry.id == tab_id {
            entry.tab_button.add_css_class("limux-tab-active");
        } else {
            entry.tab_button.remove_css_class("limux-tab-active");
        }
    }

    if content_stack.child_by_name(tab_id).is_some() {
        content_stack.set_visible_child_name(tab_id);
    }

    let focus_target = ts
        .tabs
        .iter()
        .find(|entry| entry.id == tab_id)
        .map(TabFocusTarget::from_entry);
    drop(ts);

    if let Some(target) = focus_target {
        // Mouse-initiated tab switches can leave focus on the click target if we
        // refocus synchronously. Deferring to the next idle tick makes the newly
        // active surface or webview the final focus owner.
        glib::idle_add_local_once(move || {
            target.focus();
        });
    }
}

fn remove_tab(
    tab_strip: &gtk::Box,
    content_stack: &gtk::Stack,
    tab_state: &Rc<RefCell<TabState>>,
    tab_id: &str,
    callbacks: &Rc<PaneCallbacks>,
    pane_outer: &gtk::Box,
    empty_reason: PaneEmptyReason,
) {
    let mut ts = tab_state.borrow_mut();
    let Some(idx) = ts.tabs.iter().position(|e| e.id == tab_id) else {
        return;
    };
    let entry = ts.tabs.remove(idx);

    tab_strip.remove(&entry.tab_button);
    content_stack.remove(&entry.content);

    if ts.tabs.is_empty() {
        drop(ts);
        (callbacks.on_empty)(&pane_outer.clone().upcast(), empty_reason);
        return;
    }

    // Activate neighbor tab
    let new_idx = idx.min(ts.tabs.len() - 1);
    let new_id = ts.tabs[new_idx].id.clone();
    let was_active = ts.active_tab.as_deref() == Some(tab_id);
    drop(ts);

    if was_active {
        activate_tab(tab_strip, content_stack, tab_state, &new_id);
    }
    (callbacks.on_state_changed)();
}

// ---------------------------------------------------------------------------
// Browser widget
// ---------------------------------------------------------------------------

#[cfg(feature = "webkit")]
#[derive(Clone)]
struct BrowserHandles {
    webview: webkit6::WebView,
    url_entry: gtk::Entry,
    search_bar: gtk::SearchBar,
    search_entry: gtk::SearchEntry,
    find_controller: webkit6::FindController,
    dom_editable: Rc<Cell<bool>>,
    frame_selector: Rc<RefCell<Option<String>>>,
    pending_dialogs: Rc<RefCell<VecDeque<BrowserPendingDialog>>>,
    diagnostics: Rc<RefCell<BrowserDiagnosticsBuffer>>,
}

#[cfg(feature = "webkit")]
#[derive(Clone)]
struct BrowserPendingDialog {
    dialog: webkit6::ScriptDialog,
    kind: String,
    message: String,
    default_text: Option<String>,
}

#[cfg(not(feature = "webkit"))]
#[derive(Clone)]
struct BrowserHandles;

impl BrowserShortcutTarget {
    pub fn current_uri(&self) -> Option<String> {
        self.uri.borrow().clone()
    }

    pub fn navigate(&self, url: &str) -> bool {
        self.handles.navigate(url, self.uri.clone())
    }

    pub fn focus_content(&self) -> bool {
        self.handles.focus_content()
    }

    pub fn focus_location(&self) -> bool {
        self.handles.focus_location()
    }

    pub fn go_back(&self) -> bool {
        self.handles.go_back()
    }

    pub fn go_forward(&self) -> bool {
        self.handles.go_forward()
    }

    pub fn reload(&self) -> bool {
        self.handles.reload()
    }

    pub fn show_inspector(&self) -> bool {
        self.handles.show_inspector()
    }

    pub fn show_console(&self) -> bool {
        self.handles.show_console()
    }

    pub fn show_find(&self) -> bool {
        self.handles.show_find()
    }

    pub fn find_next(&self) -> bool {
        self.handles.find_next()
    }

    pub fn find_previous(&self) -> bool {
        self.handles.find_previous()
    }

    pub fn hide_find(&self) -> bool {
        self.handles.hide_find()
    }

    pub fn use_selection_for_find(&self) -> bool {
        self.handles.use_selection_for_find()
    }

    pub fn is_find_active(&self) -> bool {
        self.handles.is_find_active()
    }

    pub fn is_page_editable(&self) -> bool {
        self.handles.is_page_editable()
    }

    pub fn is_content_focused(&self) -> bool {
        self.handles.is_content_focused()
    }

    pub fn evaluate_javascript<F>(&self, script: &str, callback: F) -> bool
    where
        F: FnOnce(Result<Value, String>) + 'static,
    {
        self.handles.evaluate_javascript(script, callback)
    }

    // purpose: Select an iframe for subsequent browser automation commands.
    // inputs: CSS selector or frame id plus a completion callback.
    // returns/effects: Persists the selected frame in this browser target.
    pub fn select_frame<F>(&self, selector: &str, callback: F) -> bool
    where
        F: FnOnce(Result<String, String>) + 'static,
    {
        self.handles.select_frame(selector, callback)
    }

    pub fn reset_frame(&self) -> bool {
        self.handles.reset_frame()
    }

    pub fn wait_for_download<F>(&self, path: Option<PathBuf>, timeout_ms: u64, callback: F) -> bool
    where
        F: FnOnce(Result<PathBuf, String>) + 'static,
    {
        self.handles.wait_for_download(path, timeout_ms, callback)
    }

    pub fn respond_to_dialog<F>(&self, accept: bool, text: Option<String>, callback: F) -> bool
    where
        F: FnOnce(Result<BrowserDialogResult, String>) + 'static,
    {
        self.handles.respond_to_dialog(accept, text, callback)
    }

    pub fn console_entries(&self) -> BrowserDiagnosticsSnapshot {
        self.handles.console_entries()
    }

    pub fn clear_console_entries(&self) -> usize {
        self.handles.clear_console_entries()
    }

    pub fn error_entries(&self) -> BrowserDiagnosticsSnapshot {
        self.handles.error_entries()
    }

    pub fn clear_error_entries(&self) -> usize {
        self.handles.clear_error_entries()
    }

    // purpose: Save the browser shortcut target as a PNG screenshot.
    // inputs: Destination path, full-page capture flag, and completion callback.
    // returns/effects: Forwards async capture to the active browser handle.
    pub fn save_screenshot<F>(&self, path: PathBuf, full_page: bool, callback: F) -> bool
    where
        F: FnOnce(Result<BrowserScreenshotResult, String>) + 'static,
    {
        self.handles.save_screenshot(path, full_page, callback)
    }
}

#[cfg(feature = "webkit")]
impl BrowserHandles {
    fn is_find_active(&self) -> bool {
        self.search_bar.is_search_mode()
    }

    fn focus_content(&self) -> bool {
        if self.is_find_active() {
            self.search_entry.grab_focus();
            self.search_entry.select_region(0, -1);
        } else {
            self.webview.grab_focus();
        }
        true
    }

    fn navigate(&self, url: &str, saved_uri: Rc<RefCell<Option<String>>>) -> bool {
        let normalized = normalize_browser_entry_input(url);
        self.url_entry.set_text(&normalized);
        *saved_uri.borrow_mut() = Some(normalized.clone());
        self.webview.load_uri(&normalized);
        true
    }

    fn is_page_editable(&self) -> bool {
        self.dom_editable.get()
    }

    fn is_content_focused(&self) -> bool {
        self.webview.is_focus()
    }

    fn focus_location(&self) -> bool {
        self.url_entry.grab_focus();
        self.url_entry.select_region(0, -1);
        true
    }

    fn go_back(&self) -> bool {
        self.webview.go_back();
        true
    }

    fn go_forward(&self) -> bool {
        self.webview.go_forward();
        true
    }

    fn reload(&self) -> bool {
        self.webview.reload();
        true
    }

    fn show_inspector(&self) -> bool {
        if let Some(inspector) = self.webview.inspector() {
            inspector.show();
            return true;
        }
        false
    }

    fn show_console(&self) -> bool {
        self.show_inspector()
    }

    fn show_find(&self) -> bool {
        self.search_bar.set_search_mode(true);
        self.search_entry.grab_focus();
        self.search_entry.select_region(0, -1);
        if !self.search_entry.text().is_empty() {
            self.search_for_entry_text();
        }
        true
    }

    fn find_next(&self) -> bool {
        if self.is_find_active() {
            self.find_controller.search_next();
            return true;
        }
        false
    }

    fn find_previous(&self) -> bool {
        if self.is_find_active() {
            self.find_controller.search_previous();
            return true;
        }
        false
    }

    fn hide_find(&self) -> bool {
        if !self.is_find_active() {
            return false;
        }
        self.find_controller.search_finish();
        self.search_bar.set_search_mode(false);
        self.webview.grab_focus();
        true
    }

    fn use_selection_for_find(&self) -> bool {
        let search_entry = self.search_entry.clone();
        let search_bar = self.search_bar.clone();
        let find_controller = self.find_controller.clone();
        let webview = self.webview.clone();
        self.webview.evaluate_javascript(
            "window.getSelection ? window.getSelection().toString() : '';",
            None,
            None,
            None::<&gtk::gio::Cancellable>,
            move |result| {
                let Ok(value) = result else {
                    return;
                };
                let selection = value.to_str();
                if selection.is_empty() {
                    return;
                }
                search_bar.set_search_mode(true);
                search_entry.set_text(selection.as_str());
                find_controller.search(
                    selection.as_str(),
                    webkit6::FindOptions::CASE_INSENSITIVE.bits()
                        | webkit6::FindOptions::WRAP_AROUND.bits(),
                    u32::MAX,
                );
                search_entry.grab_focus();
                search_entry.select_region(0, -1);
                webview.queue_draw();
            },
        );
        true
    }

    fn evaluate_javascript<F>(&self, script: &str, callback: F) -> bool
    where
        F: FnOnce(Result<Value, String>) + 'static,
    {
        let wrapped = self
            .frame_selector
            .borrow()
            .as_deref()
            .map(|selector| browser_frame_script(selector, script))
            .unwrap_or_else(|| script.to_string());
        self.webview.evaluate_javascript(
            &wrapped,
            None,
            None,
            None::<&gtk::gio::Cancellable>,
            move |result| match result {
                Ok(value) => callback(Ok(javascript_value_to_json(&value))),
                Err(error) => callback(Err(error.to_string())),
            },
        );
        true
    }

    // purpose: Validate and store a selected iframe for CMUX browser.frame.select parity.
    // inputs: CSS selector or frame id plus completion callback.
    // returns/effects: Future JavaScript automation runs against the selected frame document.
    fn select_frame<F>(&self, selector: &str, callback: F) -> bool
    where
        F: FnOnce(Result<String, String>) + 'static,
    {
        let selector = selector.trim().to_string();
        if selector.is_empty() {
            callback(Err(
                "browser.frame.select requires non-empty selector".to_string()
            ));
            return true;
        }
        let frame_selector = self.frame_selector.clone();
        let script = browser_frame_probe_script(&selector);
        self.webview.evaluate_javascript(
            &script,
            None,
            None,
            None::<&gtk::gio::Cancellable>,
            move |result| match result {
                Ok(_) => {
                    *frame_selector.borrow_mut() = Some(selector.clone());
                    callback(Ok(selector));
                }
                Err(error) => callback(Err(error.to_string())),
            },
        );
        true
    }

    fn reset_frame(&self) -> bool {
        *self.frame_selector.borrow_mut() = None;
        true
    }

    // purpose: Wait until a requested download path exists, using GTK timeouts instead of busy loops.
    // inputs: Optional path and timeout in milliseconds.
    // returns/effects: Calls back with the existing path or a timeout error.
    fn wait_for_download<F>(&self, path: Option<PathBuf>, timeout_ms: u64, callback: F) -> bool
    where
        F: FnOnce(Result<PathBuf, String>) + 'static,
    {
        let path = path.unwrap_or_else(|| std::env::temp_dir().join("download.bin"));
        let deadline = Instant::now() + Duration::from_millis(timeout_ms);
        let callback = Rc::new(RefCell::new(Some(callback)));
        glib::timeout_add_local(Duration::from_millis(50), move || {
            if path.exists() {
                if let Some(callback) = callback.borrow_mut().take() {
                    callback(Ok(path.clone()));
                }
                return glib::ControlFlow::Break;
            }
            if Instant::now() >= deadline {
                if let Some(callback) = callback.borrow_mut().take() {
                    callback(Err(format!(
                        "timed out waiting for download: {}",
                        path.display()
                    )));
                }
                return glib::ControlFlow::Break;
            }
            glib::ControlFlow::Continue
        });
        true
    }

    // purpose: Accept or dismiss the oldest queued WebKit JavaScript dialog.
    // inputs: Accept flag, optional prompt text, and completion callback.
    // returns/effects: Sets confirm/prompt values, closes the WebKit dialog, and reports metadata.
    fn respond_to_dialog<F>(&self, accept: bool, text: Option<String>, callback: F) -> bool
    where
        F: FnOnce(Result<BrowserDialogResult, String>) + 'static,
    {
        let Some(pending) = self.pending_dialogs.borrow_mut().pop_front() else {
            callback(Err("dialog queue empty".to_string()));
            return true;
        };
        if pending.kind == "confirm" || pending.kind == "beforeunload" {
            pending.dialog.confirm_set_confirmed(accept);
        }
        if pending.kind == "prompt" && accept {
            if let Some(text) = text.as_deref() {
                pending.dialog.prompt_set_text(text);
            }
        }
        pending.dialog.close();
        callback(Ok(BrowserDialogResult {
            kind: pending.kind,
            message: pending.message,
            default_text: pending.default_text,
            accepted: accept,
            text,
        }));
        true
    }

    // purpose: Return retained browser console entries for automation diagnostics.
    // inputs: Browser handle for one surface.
    // returns/effects: Clones the current bounded console ring in capture order.
    fn console_entries(&self) -> BrowserDiagnosticsSnapshot {
        browser_diagnostics_snapshot(&self.diagnostics.borrow().console)
    }

    // purpose: Clear retained browser console entries for automation diagnostics.
    // inputs: Browser handle for one surface.
    // returns/effects: Empties the console ring and returns the cleared count.
    fn clear_console_entries(&self) -> usize {
        let mut diagnostics = self.diagnostics.borrow_mut();
        let count = diagnostics.console.len();
        diagnostics.console.clear();
        count
    }

    // purpose: Return retained browser page error entries for automation diagnostics.
    // inputs: Browser handle for one surface.
    // returns/effects: Clones the current bounded error ring in capture order.
    fn error_entries(&self) -> BrowserDiagnosticsSnapshot {
        browser_diagnostics_snapshot(&self.diagnostics.borrow().errors)
    }

    // purpose: Clear retained browser page error entries for automation diagnostics.
    // inputs: Browser handle for one surface.
    // returns/effects: Empties the error ring and returns the cleared count.
    fn clear_error_entries(&self) -> usize {
        let mut diagnostics = self.diagnostics.borrow_mut();
        let count = diagnostics.errors.len();
        diagnostics.errors.clear();
        count
    }

    fn save_screenshot<F>(&self, path: PathBuf, full_page: bool, callback: F) -> bool
    where
        F: FnOnce(Result<BrowserScreenshotResult, String>) + 'static,
    {
        let region = if full_page {
            webkit6::SnapshotRegion::FullDocument
        } else {
            webkit6::SnapshotRegion::Visible
        };
        self.webview.snapshot(
            region,
            webkit6::SnapshotOptions::NONE,
            None::<&gtk::gio::Cancellable>,
            move |result| {
                let outcome = result
                    .map_err(|error| error.to_string())
                    .and_then(|texture| save_browser_texture(path, texture));
                callback(outcome);
            },
        );
        true
    }

    fn search_for_entry_text(&self) {
        let query = self.search_entry.text();
        if query.is_empty() {
            self.find_controller.search_finish();
            return;
        }
        self.find_controller.search(
            query.as_str(),
            webkit6::FindOptions::CASE_INSENSITIVE.bits()
                | webkit6::FindOptions::WRAP_AROUND.bits(),
            u32::MAX,
        );
    }
}

#[cfg(feature = "webkit")]
// purpose: Build a JavaScript probe for CMUX browser.frame.select.
// inputs: Frame CSS selector or element id.
// returns/effects: Returns script that throws when the frame cannot be automated.
fn browser_frame_probe_script(selector: &str) -> String {
    let selector = serde_json::to_string(selector).expect("json frame selector");
    format!(
        r#"(() => {{
  const selector = {selector};
  const frame = document.querySelector(selector) || document.getElementById(selector);
  if (!frame || !frame.contentWindow || !frame.contentDocument) {{
    throw new Error(`frame not found: ${{selector}}`);
  }}
  return {{ frame_id: selector, url: frame.contentWindow.location.href || "about:blank" }};
}})()"#
    )
}

#[cfg(feature = "webkit")]
// purpose: Wrap browser automation JavaScript so it executes inside the selected iframe.
// inputs: Selected frame selector and existing JavaScript source.
// returns/effects: Returns script that evaluates against the frame document/window or throws loudly.
fn browser_frame_script(selector: &str, script: &str) -> String {
    let selector = serde_json::to_string(selector).expect("json frame selector");
    let source = serde_json::to_string(script).expect("json frame script");
    format!(
        r#"(() => {{
  const selector = {selector};
  const frame = document.querySelector(selector) || document.getElementById(selector);
  if (!frame || !frame.contentWindow || !frame.contentDocument) {{
    throw new Error(`frame not found: ${{selector}}`);
  }}
  const source = {source};
  const frameWindow = frame.contentWindow;
  const frameDocument = frame.contentDocument;
  try {{
    return frameWindow.Function("document", "window", `return (${{source}});`)(frameDocument, frameWindow);
  }} catch (expressionError) {{
    return frameWindow.Function("document", "window", source)(frameDocument, frameWindow);
  }}
}})()"#
    )
}

#[cfg(feature = "webkit")]
// purpose: Normalize WebKit dialog enum values into CMUX-compatible names.
// inputs: WebKit script dialog type.
// returns/effects: Returns stable string names for JSON responses.
fn browser_dialog_kind(kind: webkit6::ScriptDialogType) -> &'static str {
    match kind {
        webkit6::ScriptDialogType::Alert => "alert",
        webkit6::ScriptDialogType::Confirm => "confirm",
        webkit6::ScriptDialogType::Prompt => "prompt",
        webkit6::ScriptDialogType::BeforeUnloadConfirm => "beforeunload",
        webkit6::ScriptDialogType::__Unknown(_) => "unknown",
        _ => "unknown",
    }
}

#[cfg(feature = "webkit")]
fn javascript_value_to_json(value: &webkit6::javascriptcore::Value) -> Value {
    if value.is_null() || value.is_undefined() {
        Value::Null
    } else if value.is_boolean() {
        Value::Bool(value.to_boolean())
    } else if value.is_number() {
        serde_json::Number::from_f64(value.to_double())
            .map(Value::Number)
            .unwrap_or(Value::Null)
    } else if value.is_string() {
        Value::String(value.to_str().to_string())
    } else if let Some(json) = value.to_json(0) {
        serde_json::from_str(json.as_str())
            .unwrap_or_else(|_| Value::String(value.to_str().to_string()))
    } else {
        Value::String(value.to_str().to_string())
    }
}

#[cfg(feature = "webkit")]
// purpose: Save a WebKit snapshot texture to PNG and return metadata for the control bridge.
// inputs: Destination path and captured GDK texture.
// returns/effects: Writes a PNG file or returns the concrete filesystem/PNG failure.
fn save_browser_texture(
    path: PathBuf,
    texture: gtk::gdk::Texture,
) -> Result<BrowserScreenshotResult, String> {
    use gtk::gdk::prelude::TextureExt;

    if let Some(parent) = path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
    {
        std::fs::create_dir_all(parent).map_err(|error| {
            format!(
                "failed to create screenshot directory {}: {error}",
                parent.display()
            )
        })?;
    }
    texture.save_to_png(&path).map_err(|error| {
        format!(
            "failed to save browser screenshot {}: {error}",
            path.display()
        )
    })?;
    let metadata = std::fs::metadata(&path).map_err(|error| {
        format!(
            "failed to stat browser screenshot {}: {error}",
            path.display()
        )
    })?;
    Ok(BrowserScreenshotResult {
        path: path.to_string_lossy().into_owned(),
        width: texture.width(),
        height: texture.height(),
        bytes: metadata.len(),
    })
}

#[cfg(not(feature = "webkit"))]
impl BrowserHandles {
    fn is_find_active(&self) -> bool {
        false
    }

    fn focus_content(&self) -> bool {
        false
    }

    fn navigate(&self, _url: &str, _saved_uri: Rc<RefCell<Option<String>>>) -> bool {
        false
    }

    fn is_page_editable(&self) -> bool {
        false
    }

    fn is_content_focused(&self) -> bool {
        false
    }

    fn evaluate_javascript<F>(&self, _script: &str, _callback: F) -> bool
    where
        F: FnOnce(Result<Value, String>) + 'static,
    {
        _callback(Err(
            "browser JavaScript evaluation requires webkit support".to_string()
        ));
        false
    }

    fn select_frame<F>(&self, _selector: &str, callback: F) -> bool
    where
        F: FnOnce(Result<String, String>) + 'static,
    {
        callback(Err(
            "browser frame selection requires webkit support".to_string()
        ));
        false
    }

    fn reset_frame(&self) -> bool {
        false
    }

    fn wait_for_download<F>(&self, _path: Option<PathBuf>, _timeout_ms: u64, callback: F) -> bool
    where
        F: FnOnce(Result<PathBuf, String>) + 'static,
    {
        callback(Err(
            "browser download wait requires webkit support".to_string()
        ));
        false
    }

    fn respond_to_dialog<F>(&self, _accept: bool, _text: Option<String>, callback: F) -> bool
    where
        F: FnOnce(Result<BrowserDialogResult, String>) + 'static,
    {
        callback(Err(
            "browser dialog response requires webkit support".to_string()
        ));
        false
    }

    fn console_entries(&self) -> BrowserDiagnosticsSnapshot {
        BrowserDiagnosticsSnapshot::default()
    }

    fn clear_console_entries(&self) -> usize {
        0
    }

    fn error_entries(&self) -> BrowserDiagnosticsSnapshot {
        BrowserDiagnosticsSnapshot::default()
    }

    fn clear_error_entries(&self) -> usize {
        0
    }

    fn save_screenshot<F>(&self, _path: PathBuf, _full_page: bool, callback: F) -> bool
    where
        F: FnOnce(Result<BrowserScreenshotResult, String>) + 'static,
    {
        callback(Err(
            "browser screenshot capture requires webkit support".to_string()
        ));
        false
    }

    fn focus_location(&self) -> bool {
        false
    }

    fn go_back(&self) -> bool {
        false
    }

    fn go_forward(&self) -> bool {
        false
    }

    fn reload(&self) -> bool {
        false
    }

    fn show_inspector(&self) -> bool {
        false
    }

    fn show_console(&self) -> bool {
        false
    }

    fn show_find(&self) -> bool {
        false
    }

    fn find_next(&self) -> bool {
        false
    }

    fn find_previous(&self) -> bool {
        false
    }

    fn hide_find(&self) -> bool {
        false
    }

    fn use_selection_for_find(&self) -> bool {
        false
    }
}

#[cfg(feature = "webkit")]
const LIMUX_BROWSER_EDITABLE_STATE_HANDLER: &str = "limuxEditableState";

#[cfg(feature = "webkit")]
fn env_value_contains_token(value: &str, token: &str) -> bool {
    value
        .split(|ch: char| !ch.is_ascii_alphanumeric())
        .any(|part| part.eq_ignore_ascii_case(token))
}

#[cfg(feature = "webkit")]
fn is_kde_wayland_session_from_env<'a>(
    values: impl IntoIterator<Item = (&'a str, &'a str)>,
) -> bool {
    let mut is_wayland = false;
    let mut is_kde = false;

    for (key, value) in values {
        match key {
            "WAYLAND_DISPLAY" if !value.trim().is_empty() => is_wayland = true,
            "XDG_SESSION_TYPE" if value.eq_ignore_ascii_case("wayland") => is_wayland = true,
            "XDG_CURRENT_DESKTOP" | "XDG_SESSION_DESKTOP" | "DESKTOP_SESSION" => {
                is_kde |= env_value_contains_token(value, "kde")
                    || env_value_contains_token(value, "plasma");
            }
            "KDE_FULL_SESSION" if value.eq_ignore_ascii_case("true") || value == "1" => {
                is_kde = true;
            }
            _ => {}
        }
    }

    is_wayland && is_kde
}

#[cfg(feature = "webkit")]
fn is_kde_wayland_session() -> bool {
    let keys = [
        "WAYLAND_DISPLAY",
        "XDG_SESSION_TYPE",
        "XDG_CURRENT_DESKTOP",
        "XDG_SESSION_DESKTOP",
        "DESKTOP_SESSION",
        "KDE_FULL_SESSION",
    ];
    let values = keys
        .into_iter()
        .filter_map(|key| std::env::var(key).ok().map(|value| (key, value)))
        .collect::<Vec<_>>();

    is_kde_wayland_session_from_env(values.iter().map(|(key, value)| (*key, value.as_str())))
}

#[cfg(feature = "webkit")]
fn configure_browser_settings(settings: &webkit6::Settings) {
    settings.set_enable_developer_extras(true);
    settings.set_javascript_can_open_windows_automatically(true);

    if is_kde_wayland_session() {
        settings.set_hardware_acceleration_policy(webkit6::HardwareAccelerationPolicy::Never);
    }
}

#[cfg(feature = "webkit")]
const LIMUX_BROWSER_EDITABLE_STATE_SCRIPT: &str = r#"
(() => {
  const handler = globalThis.webkit?.messageHandlers?.limuxEditableState;
  if (!handler || typeof handler.postMessage !== 'function') {
    return;
  }

  const nonTextInputTypes = new Set([
    'button',
    'checkbox',
    'color',
    'file',
    'hidden',
    'image',
    'radio',
    'range',
    'reset',
    'submit'
  ]);

  const isEditableElement = (element) => {
    if (!element) {
      return false;
    }
    if (element.isContentEditable) {
      return true;
    }

    const tagName = (element.tagName || '').toUpperCase();
    if (tagName === 'TEXTAREA') {
      return !element.readOnly && !element.disabled;
    }
    if (tagName === 'SELECT') {
      return !element.disabled;
    }
    if (tagName !== 'INPUT') {
      return false;
    }

    const type = (element.type || '').toLowerCase();
    return !nonTextInputTypes.has(type) && !element.readOnly && !element.disabled;
  };

  const publish = () => {
    handler.postMessage(Boolean(isEditableElement(document.activeElement)));
  };

  publish();
  document.addEventListener('focusin', publish, true);
  document.addEventListener('focusout', () => queueMicrotask(publish), true);
  window.addEventListener('pageshow', publish, true);
})();
"#;

#[cfg(feature = "webkit")]
const LIMUX_BROWSER_DIAGNOSTICS_SCRIPT: &str = r#"
(() => {
  const handler = globalThis.webkit?.messageHandlers?.limuxBrowserDiagnostics;
  if (!handler || typeof handler.postMessage !== 'function') {
    throw new Error('limuxBrowserDiagnostics message handler is unavailable');
  }
  if (globalThis.__limuxBrowserDiagnosticsInstalled) {
    return;
  }
  Object.defineProperty(globalThis, '__limuxBrowserDiagnosticsInstalled', {
    value: true,
    configurable: false
  });

  const serialize = (value) => {
    if (value instanceof Error) {
      return {
        name: value.name,
        message: value.message,
        stack: value.stack || null
      };
    }
    if (typeof value === 'string') {
      return value;
    }
    try {
      return JSON.parse(JSON.stringify(value));
    } catch (_error) {
      return String(value);
    }
  };

  const post = (kind, level, args, extra) => {
    const serialized = Array.from(args || []).map(serialize);
    handler.postMessage({
      kind,
      level,
      message: serialized.map((item) => {
        return typeof item === 'string' ? item : JSON.stringify(item);
      }).join(' '),
      args: serialized,
      url: String(location.href || ''),
      timestamp_ms: Date.now(),
      extra: extra || null
    });
  };

  for (const level of ['debug', 'log', 'info', 'warn', 'error']) {
    const original = console[level];
    if (typeof original !== 'function') {
      continue;
    }
    console[level] = function (...args) {
      post('console', level, args, null);
      return original.apply(this, args);
    };
  }

  window.addEventListener('error', (event) => {
    post('error', 'error', [event.message || ''], {
      source: event.filename || null,
      line: event.lineno || null,
      column: event.colno || null,
      stack: event.error && event.error.stack ? String(event.error.stack) : null
    });
  }, true);

  window.addEventListener('unhandledrejection', (event) => {
    post('error', 'unhandledrejection', [event.reason], {
      stack: event.reason && event.reason.stack ? String(event.reason.stack) : null
    });
  }, true);
})();
"#;

#[cfg(feature = "webkit")]
fn create_browser_widget(
    initial_uri: Option<&str>,
    saved_uri: Rc<RefCell<Option<String>>>,
    callbacks: Rc<PaneCallbacks>,
) -> (gtk::Widget, String, BrowserHandles) {
    use webkit6::prelude::*;

    // Use a NetworkSession to avoid sandbox issues
    let network_session = webkit6::NetworkSession::default();
    let web_context = webkit6::WebContext::default();
    let user_content_manager = webkit6::UserContentManager::new();
    let dom_editable = Rc::new(Cell::new(false));
    let _ = user_content_manager
        .register_script_message_handler(LIMUX_BROWSER_EDITABLE_STATE_HANDLER, None);
    assert!(
        user_content_manager
            .register_script_message_handler(LIMUX_BROWSER_DIAGNOSTICS_HANDLER, None),
        "webkit should register browser diagnostics handler"
    );
    user_content_manager.add_script(&webkit6::UserScript::new(
        LIMUX_BROWSER_EDITABLE_STATE_SCRIPT,
        webkit6::UserContentInjectedFrames::AllFrames,
        webkit6::UserScriptInjectionTime::Start,
        &[],
        &[],
    ));
    user_content_manager.add_script(&webkit6::UserScript::new(
        LIMUX_BROWSER_DIAGNOSTICS_SCRIPT,
        webkit6::UserContentInjectedFrames::AllFrames,
        webkit6::UserScriptInjectionTime::Start,
        &[],
        &[],
    ));
    {
        let dom_editable = dom_editable.clone();
        user_content_manager.connect_script_message_received(
            Some(LIMUX_BROWSER_EDITABLE_STATE_HANDLER),
            move |_, value| {
                dom_editable.set(if value.is_boolean() {
                    value.to_boolean()
                } else {
                    value.to_str().as_str() == "true"
                });
            },
        );
    }
    let diagnostics = Rc::new(RefCell::new(BrowserDiagnosticsBuffer::default()));
    {
        let diagnostics = diagnostics.clone();
        user_content_manager.connect_script_message_received(
            Some(LIMUX_BROWSER_DIAGNOSTICS_HANDLER),
            move |_, value| {
                push_browser_diagnostic(
                    &mut diagnostics.borrow_mut(),
                    javascript_value_to_json(value),
                );
            },
        );
    }

    let webview = webkit6::WebView::builder()
        .user_content_manager(&user_content_manager)
        .hexpand(true)
        .vexpand(true)
        .build();
    webview.add_css_class(BROWSER_WEB_VIEW_CSS_CLASS);
    webview.set_halign(gtk::Align::Fill);
    webview.set_valign(gtk::Align::Fill);
    webview.set_overflow(gtk::Overflow::Hidden);

    if let Some(settings) = webkit6::prelude::WebViewExt::settings(&webview) {
        configure_browser_settings(&settings);
    }

    let url_entry = gtk::Entry::builder()
        .placeholder_text("Enter URL...")
        .hexpand(true)
        .build();
    for css_class in BROWSER_URL_ENTRY_CSS_CLASSES {
        url_entry.add_css_class(css_class);
    }

    let back_btn = icon_button("go-previous-symbolic", "Back");
    let fwd_btn = icon_button("go-next-symbolic", "Forward");
    let reload_btn = icon_button("view-refresh-symbolic", "Reload");

    let nav_bar = gtk::Box::new(gtk::Orientation::Horizontal, 4);
    nav_bar.add_css_class("limux-pane-header");
    nav_bar.append(&back_btn);
    nav_bar.append(&fwd_btn);
    nav_bar.append(&reload_btn);
    nav_bar.append(&url_entry);

    {
        let wv = webview.clone();
        back_btn.connect_clicked(move |_| {
            wv.go_back();
        });
    }
    {
        let wv = webview.clone();
        fwd_btn.connect_clicked(move |_| {
            wv.go_forward();
        });
    }
    {
        let wv = webview.clone();
        reload_btn.connect_clicked(move |_| {
            wv.reload();
        });
    }
    {
        let wv = webview.clone();
        url_entry.connect_activate(move |entry| {
            let url = normalize_browser_entry_input(&entry.text());
            wv.load_uri(&url);
        });
    }
    {
        let entry = url_entry.clone();
        let saved_uri = saved_uri.clone();
        let callbacks = callbacks.clone();
        let restoring = Rc::new(std::cell::Cell::new(initial_uri.is_some()));
        let restoring_flag = restoring.clone();
        webview.connect_uri_notify(move |wv| {
            if let Some(uri) = wv.uri() {
                let uri_str: String = uri.into();
                entry.set_text(&uri_str);
                if restoring_flag.get() && (uri_str.is_empty() || uri_str == "about:blank") {
                    return;
                }
                restoring_flag.set(false);
                *saved_uri.borrow_mut() = Some(uri_str);
                (callbacks.on_state_changed)();
            }
        });
    }

    let find_controller = webview
        .find_controller()
        .expect("webkit webview should expose a find controller");
    let search_entry = gtk::SearchEntry::builder()
        .hexpand(true)
        .placeholder_text("Find in page")
        .build();
    for css_class in BROWSER_SEARCH_ENTRY_CSS_CLASSES {
        search_entry.add_css_class(css_class);
    }
    let search_bar = gtk::SearchBar::new();
    search_bar.set_show_close_button(true);
    search_bar.connect_entry(&search_entry);
    search_bar.set_child(Some(&search_entry));
    {
        let search_bar = search_bar.clone();
        let find_controller = find_controller.clone();
        let webview = webview.clone();
        search_entry.connect_stop_search(move |_| {
            find_controller.search_finish();
            search_bar.set_search_mode(false);
            webview.grab_focus();
        });
    }
    {
        let dom_editable = dom_editable.clone();
        webview.connect_load_changed(move |_, _| {
            dom_editable.set(false);
        });
    }
    let pending_dialogs = Rc::new(RefCell::new(VecDeque::new()));
    {
        let pending_dialogs = pending_dialogs.clone();
        webview.connect_script_dialog(move |_, dialog| {
            pending_dialogs
                .borrow_mut()
                .push_back(BrowserPendingDialog {
                    dialog: dialog.clone(),
                    kind: browser_dialog_kind(dialog.dialog_type()).to_string(),
                    message: dialog
                        .message()
                        .map(|value| value.to_string())
                        .unwrap_or_default(),
                    default_text: dialog
                        .prompt_get_default_text()
                        .map(|value| value.to_string()),
                });
            true
        });
    }

    let vbox = gtk::Box::new(gtk::Orientation::Vertical, 0);
    vbox.append(&nav_bar);
    vbox.append(&search_bar);
    vbox.append(&webview.clone());
    vbox.set_hexpand(true);
    vbox.set_vexpand(true);
    vbox.set_halign(gtk::Align::Fill);
    vbox.set_valign(gtk::Align::Fill);
    vbox.set_overflow(gtk::Overflow::Hidden);
    vbox.add_css_class("limux-browser");

    let browser_handles = BrowserHandles {
        webview: webview.clone(),
        url_entry: url_entry.clone(),
        search_bar: search_bar.clone(),
        search_entry: search_entry.clone(),
        find_controller: find_controller.clone(),
        dom_editable,
        frame_selector: Rc::new(RefCell::new(None)),
        pending_dialogs,
        diagnostics,
    };

    {
        let browser_handles = browser_handles.clone();
        search_entry.connect_search_changed(move |_| {
            browser_handles.search_for_entry_text();
        });
    }

    // Load default URL only on the first map. The WebView preserves its
    // page and history across reparenting (splits), so we must not reload.
    {
        let wv = webview.clone();
        let loaded = std::cell::Cell::new(false);
        let initial_uri = initial_uri.map(|value| value.to_string());
        vbox.connect_map(move |_| {
            if !loaded.get() {
                loaded.set(true);
                if let Some(uri) = &initial_uri {
                    wv.load_uri(uri);
                } else {
                    wv.load_uri("https://google.com");
                }
            }
        });
    }

    // Suppress unused variable warnings
    let _ = network_session;
    let _ = web_context;

    (vbox.upcast(), "Browser".to_string(), browser_handles)
}

fn normalize_browser_entry_input(input: &str) -> String {
    if input.starts_with("http://") || input.starts_with("https://") {
        return input.to_string();
    }

    if is_localhost_input(input) {
        format!("http://{input}")
    } else if input.contains('.') {
        format!("https://{input}")
    } else {
        format!(
            "https://www.google.com/search?q={}",
            input.replace(' ', "+")
        )
    }
}

fn is_localhost_input(input: &str) -> bool {
    input == "localhost"
        || input
            .strip_prefix("localhost")
            .and_then(|rest| rest.chars().next())
            .is_some_and(|ch| matches!(ch, ':' | '/' | '?' | '#'))
}

#[cfg(not(feature = "webkit"))]
fn create_browser_widget(
    initial_uri: Option<&str>,
    saved_uri: Rc<RefCell<Option<String>>>,
    _callbacks: Rc<PaneCallbacks>,
) -> (gtk::Widget, String, BrowserHandles) {
    *saved_uri.borrow_mut() = initial_uri.map(|value| value.to_string());
    let placeholder = gtk::Box::builder()
        .orientation(gtk::Orientation::Vertical)
        .halign(gtk::Align::Center)
        .valign(gtk::Align::Center)
        .spacing(12)
        .build();

    let msg = gtk::Label::builder()
        .label("Browser requires webkit6")
        .build();
    msg.set_css_classes(&["dim-label"]);

    let hint = gtk::Label::builder()
        .label("sudo apt install libwebkitgtk-6.0-dev\ncargo build --features webkit")
        .justify(gtk::Justification::Center)
        .build();
    hint.set_css_classes(&["dim-label"]);

    placeholder.append(&msg);
    placeholder.append(&hint);
    placeholder.set_hexpand(true);
    placeholder.set_vexpand(true);

    let handles = BrowserHandles;

    (placeholder.upcast(), "Browser".to_string(), handles)
}

#[cfg(test)]
mod tests {
    use super::{
        append_git_watch_env, browser_diagnostics_snapshot, classify_content_drop_zone,
        codex_wrapper_root, content_drop_preview_rect, effective_drop_target_dimensions,
        install_codex_wrapper_env, is_localhost_input, next_active_after_tab_removal,
        normalize_browser_entry_input, normalize_reorder_insert_index, pane_action_tooltip,
        push_browser_diagnostic, surface_hint_matches, write_executable_file,
        BrowserDiagnosticsBuffer, ContentDropZone, TabDragPayload, BROWSER_DIAGNOSTIC_BUFFER_LIMIT,
        BROWSER_SEARCH_ENTRY_CSS_CLASS, BROWSER_SEARCH_ENTRY_CSS_CLASSES,
        BROWSER_URL_ENTRY_CSS_CLASS, BROWSER_URL_ENTRY_CSS_CLASSES, CODEX_WRAPPER_SCRIPT,
        HOST_ENTRY_CSS_CLASS, PANE_CSS, TAB_RENAME_ENTRY_CSS_CLASS, TAB_RENAME_ENTRY_CSS_CLASSES,
    };
    #[cfg(feature = "webkit")]
    use super::{
        env_value_contains_token, is_kde_wayland_session_from_env, BROWSER_WEB_VIEW_CSS_CLASS,
    };
    use crate::app_config::AppConfig;
    use crate::shortcut_config::{default_shortcuts, resolve_shortcuts_from_str, ShortcutId};
    use serde_json::json;
    use std::fs;
    use std::process::Command;

    #[test]
    fn browser_diagnostics_buffer_routes_caps_and_snapshots_entries() {
        let mut buffers = BrowserDiagnosticsBuffer::default();
        push_browser_diagnostic(&mut buffers, json!({"kind": "console", "message": "ready"}));
        push_browser_diagnostic(&mut buffers, json!({"kind": "error", "message": "boom"}));
        push_browser_diagnostic(
            &mut buffers,
            json!({"kind": "network", "message": "ignored"}),
        );

        let console = browser_diagnostics_snapshot(&buffers.console);
        assert_eq!(console.count, 1);
        assert_eq!(console.entries[0]["message"], "ready");

        let errors = browser_diagnostics_snapshot(&buffers.errors);
        assert_eq!(errors.count, 1);
        assert_eq!(errors.entries[0]["message"], "boom");

        buffers.console.clear();
        for idx in 0..=BROWSER_DIAGNOSTIC_BUFFER_LIMIT {
            push_browser_diagnostic(&mut buffers, json!({"kind": "console", "message": idx}));
        }
        let capped = browser_diagnostics_snapshot(&buffers.console);
        assert_eq!(capped.count, BROWSER_DIAGNOSTIC_BUFFER_LIMIT);
        assert_eq!(capped.entries[0]["message"], 1);
    }

    #[test]
    fn pane_action_tooltip_reflects_remaps_and_unbinds() {
        let defaults = default_shortcuts();
        assert_eq!(
            pane_action_tooltip(&defaults, "New terminal tab", Some(ShortcutId::NewTerminal)),
            "New terminal tab (Ctrl+T)"
        );
        assert_eq!(
            pane_action_tooltip(&defaults, "New browser tab", None),
            "New browser tab"
        );

        let remapped = resolve_shortcuts_from_str(
            r#"{
                "shortcuts": {
                    "split_right": "<Ctrl><Alt>d"
                }
            }"#,
        )
        .unwrap();
        assert_eq!(
            pane_action_tooltip(&remapped, "Split right", Some(ShortcutId::SplitRight)),
            "Split right (Ctrl+Alt+D)"
        );

        let unbound = resolve_shortcuts_from_str(
            r#"{
                "shortcuts": {
                    "close_focused_pane": null
                }
            }"#,
        )
        .unwrap();
        assert_eq!(
            pane_action_tooltip(&unbound, "Close pane", Some(ShortcutId::CloseFocusedPane)),
            "Close pane"
        );
    }

    #[test]
    fn pane_css_keeps_entry_layout_classes_separate_from_shared_theme() {
        assert!(PANE_CSS.contains(".limux-tab-rename-entry"));
        assert!(PANE_CSS.contains(".limux-browser-url-entry"));
        assert!(PANE_CSS.contains(".limux-browser-search-entry"));
        #[cfg(feature = "webkit")]
        assert!(PANE_CSS.contains(BROWSER_WEB_VIEW_CSS_CLASS));
        assert!(!PANE_CSS.contains("border: 1px solid rgba(0, 145, 255, 0.5);"));
    }

    #[test]
    fn pane_entries_use_shared_host_entry_class() {
        assert_eq!(
            TAB_RENAME_ENTRY_CSS_CLASSES,
            [HOST_ENTRY_CSS_CLASS, TAB_RENAME_ENTRY_CSS_CLASS]
        );
        assert_eq!(
            BROWSER_URL_ENTRY_CSS_CLASSES,
            [HOST_ENTRY_CSS_CLASS, BROWSER_URL_ENTRY_CSS_CLASS]
        );
        assert_eq!(
            BROWSER_SEARCH_ENTRY_CSS_CLASSES,
            [HOST_ENTRY_CSS_CLASS, BROWSER_SEARCH_ENTRY_CSS_CLASS]
        );
    }

    #[test]
    fn codex_wrapper_root_sanitizes_surface_id() {
        let root = codex_wrapper_root("10:tab/a b");
        assert!(root.ends_with("10-tab-a-b"));
    }

    #[test]
    fn codex_wrapper_env_installs_shim_and_exports_cmux_vars() {
        let surface_id = format!("test:{}:codex", std::process::id());
        let root = codex_wrapper_root(&surface_id);
        let _ = fs::remove_dir_all(&root);
        let mut env = vec![("PATH".to_string(), "/usr/bin".to_string())];

        install_codex_wrapper_env(&surface_id, &mut env);

        let shim = root.join("codex");
        let script = fs::read_to_string(&shim).expect("read wrapper shim");
        assert_eq!(script, CODEX_WRAPPER_SCRIPT);
        assert_eq!(
            env_value(&env, "PATH"),
            format!("{}:/usr/bin", root.display())
        );
        assert_eq!(
            env_value(&env, "CMUX_CODEX_WRAPPER_SHIM"),
            shim.display().to_string()
        );
        assert_eq!(
            env_value(&env, "LIMUX_CODEX_WRAPPER_SHIM_ROOT"),
            root.display().to_string()
        );
    }

    // purpose: Verify CMUX git and pull-request watch env follows sidebar settings.
    // inputs: Default config, disabled PRs, and disabled git-watch config variants.
    // returns/effects: Asserts managed CMUX_NO_GIT_WATCH and CMUX_NO_PR_WATCH values.
    #[test]
    fn git_watch_env_follows_sidebar_settings() {
        let mut config = AppConfig::default();
        let mut env = Vec::new();
        append_git_watch_env(&config, &mut env);
        assert_eq!(env_value(&env, "CMUX_NO_GIT_WATCH"), "");
        assert_eq!(env_value(&env, "CMUX_NO_PR_WATCH"), "");

        config.sidebar.show_pull_requests = false;
        let mut env = Vec::new();
        append_git_watch_env(&config, &mut env);
        assert_eq!(env_value(&env, "CMUX_NO_GIT_WATCH"), "");
        assert_eq!(env_value(&env, "CMUX_NO_PR_WATCH"), "1");

        config.sidebar.watch_git_status = false;
        let mut env = Vec::new();
        append_git_watch_env(&config, &mut env);
        assert_eq!(env_value(&env, "CMUX_NO_GIT_WATCH"), "1");
        assert_eq!(env_value(&env, "CMUX_NO_PR_WATCH"), "1");
    }

    #[test]
    fn codex_wrapper_executes_real_codex_with_launch_metadata() {
        let surface_id = format!("exec:{}:codex", std::process::id());
        let root = codex_wrapper_root(&surface_id);
        let _ = fs::remove_dir_all(&root);
        let mut env = vec![("PATH".to_string(), "/usr/bin".to_string())];
        install_codex_wrapper_env(&surface_id, &mut env);

        let real_dir = tempfile::tempdir().expect("real codex dir");
        let real_codex = real_dir.path().join("codex");
        write_executable_file(
            &real_codex,
            r#"#!/bin/sh
set -eu
printf 'exe=%s\n' "$LIMUX_AGENT_LAUNCH_EXECUTABLE"
printf 'argv=%s\n' "$CMUX_AGENT_LAUNCH_ARGV"
printf 'cwd=%s\n' "$CMUX_AGENT_LAUNCH_CWD"
printf 'arg1=%s\n' "$1"
"#,
        )
        .expect("write fake codex");
        let cli_dir = tempfile::tempdir().expect("fake limux cli dir");
        let hook_log = cli_dir.path().join("hook.log");
        let fake_limux = cli_dir.path().join("limux");
        write_executable_file(
            &fake_limux,
            &format!(
                r#"#!/bin/sh
set -eu
while IFS= read -r _line
do
  :
done
{{
  printf 'args=%s\n' "$*"
  printf 'session=%s\n' "${{LIMUX_AGENT_SESSION_ID:-}}"
  printf 'pid=%s\n' "${{LIMUX_AGENT_PID:-}}"
  printf 'surface=%s\n' "${{LIMUX_SURFACE_ID:-}}"
  printf 'workspace=%s\n' "${{LIMUX_WORKSPACE_ID:-}}"
}} >> {}
"#,
                hook_log.display()
            ),
        )
        .expect("write fake limux cli");

        let shim = root.join("codex");
        let path = format!("{}:{}", root.display(), real_dir.path().display());
        let output = Command::new(&shim)
            .arg("run")
            .env("PATH", path)
            .env("CMUX_CODEX_WRAPPER_SHIM_ROOT", root.as_os_str())
            .env("LIMUX_CLI", fake_limux.as_os_str())
            .env("LIMUX_SOCKET", "/tmp/limux-test.sock")
            .env("LIMUX_SURFACE_ID", "10:tab-a")
            .env("LIMUX_WORKSPACE_ID", "workspace-a")
            .output()
            .expect("execute wrapper");

        assert!(
            output.status.success(),
            "stdout: {}\nstderr: {}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        let stdout = String::from_utf8(output.stdout).expect("wrapper stdout utf8");
        assert!(stdout.contains("exe=codex"));
        assert!(stdout.contains("argv=codex run"));
        assert!(stdout.contains("arg1=run"));
        assert!(stdout.contains("cwd="));
        let hook = fs::read_to_string(hook_log).expect("read hook log");
        assert!(hook.contains(
            "args=--json hooks codex session-start --workspace workspace-a --surface 10:tab-a"
        ));
        assert!(hook.contains("session=codex-wrapper-10:tab-a-"));
        assert!(hook.contains("pid="));
        assert!(hook.contains("surface=10:tab-a"));
        assert!(hook.contains("workspace=workspace-a"));
    }

    fn env_value(env: &[(String, String)], key: &str) -> String {
        env.iter()
            .find(|(candidate, _)| candidate == key)
            .map(|(_, value)| value.clone())
            .unwrap_or_else(|| panic!("missing env key {key}"))
    }

    #[cfg(feature = "webkit")]
    #[test]
    fn browser_environment_token_matching_requires_real_tokens() {
        assert!(env_value_contains_token("KDE", "kde"));
        assert!(env_value_contains_token("GNOME:KDE", "kde"));
        assert!(env_value_contains_token("plasma-wayland", "plasma"));
        assert!(!env_value_contains_token("notkde", "kde"));
        assert!(!env_value_contains_token("kdevelopment", "kde"));
    }

    #[cfg(feature = "webkit")]
    #[test]
    fn kde_wayland_detection_matches_reported_browser_corruption_environment() {
        assert!(is_kde_wayland_session_from_env([
            ("XDG_CURRENT_DESKTOP", "KDE"),
            ("XDG_SESSION_TYPE", "wayland"),
        ]));
        assert!(is_kde_wayland_session_from_env([
            ("DESKTOP_SESSION", "plasma"),
            ("WAYLAND_DISPLAY", "wayland-0"),
        ]));
        assert!(!is_kde_wayland_session_from_env([
            ("XDG_CURRENT_DESKTOP", "KDE"),
            ("XDG_SESSION_TYPE", "x11"),
        ]));
        assert!(!is_kde_wayland_session_from_env([
            ("XDG_CURRENT_DESKTOP", "GNOME"),
            ("WAYLAND_DISPLAY", "wayland-0"),
        ]));
    }

    #[test]
    fn surface_hint_matches_only_exact_surface_or_tab_id() {
        assert!(surface_hint_matches(
            "42:tab-a",
            "tab-a",
            "surface:42:tab-a"
        ));
        assert!(surface_hint_matches("42:tab-a", "tab-a", "tab-a"));
        assert!(!surface_hint_matches("42:tab-a", "tab-a", "42:tab-b"));
        assert!(!surface_hint_matches("42:tab-a", "tab-a", ""));
    }

    #[test]
    fn tab_drag_payload_round_trips() {
        let payload = TabDragPayload::new(17, "tab-123");
        let encoded = payload.encode();
        assert_eq!(encoded, "17:tab-123");
        assert_eq!(TabDragPayload::decode(&encoded), Some(payload));
    }

    #[test]
    fn tab_drag_payload_rejects_invalid_values() {
        assert_eq!(TabDragPayload::decode(""), None);
        assert_eq!(TabDragPayload::decode("17"), None);
        assert_eq!(TabDragPayload::decode("abc:tab"), None);
        assert_eq!(TabDragPayload::decode("17:"), None);
    }

    #[test]
    fn normalize_reorder_insert_index_adjusts_forward_moves() {
        assert_eq!(normalize_reorder_insert_index(1, 4), Some(3));
        assert_eq!(normalize_reorder_insert_index(4, 1), Some(1));
        assert_eq!(normalize_reorder_insert_index(2, 2), None);
        assert_eq!(normalize_reorder_insert_index(2, 3), None);
    }

    #[test]
    fn next_active_after_tab_removal_prefers_neighbor_when_active_removed() {
        assert_eq!(
            next_active_after_tab_removal(&["a", "b", "c"], Some("b"), 1),
            Some("c".to_string())
        );
        assert_eq!(
            next_active_after_tab_removal(&["a", "b", "c"], Some("a"), 0),
            Some("b".to_string())
        );
        assert_eq!(
            next_active_after_tab_removal(&["a", "b", "c"], Some("a"), 2),
            Some("a".to_string())
        );
        assert_eq!(
            next_active_after_tab_removal(&["only"], Some("only"), 0),
            None
        );
    }

    #[test]
    fn classify_content_drop_zone_prefers_edges_before_center() {
        assert_eq!(
            classify_content_drop_zone(100.0, 80.0, 10.0, 40.0),
            Some(ContentDropZone::Left)
        );
        assert_eq!(
            classify_content_drop_zone(100.0, 80.0, 90.0, 40.0),
            Some(ContentDropZone::Right)
        );
        assert_eq!(
            classify_content_drop_zone(100.0, 80.0, 50.0, 5.0),
            Some(ContentDropZone::Top)
        );
        assert_eq!(
            classify_content_drop_zone(100.0, 80.0, 50.0, 75.0),
            Some(ContentDropZone::Bottom)
        );
        assert_eq!(
            classify_content_drop_zone(100.0, 80.0, 50.0, 40.0),
            Some(ContentDropZone::Center)
        );
        assert_eq!(classify_content_drop_zone(0.0, 80.0, 50.0, 40.0), None);
    }

    #[test]
    fn classify_content_drop_zone_uses_quarter_bands_not_thirds() {
        assert_eq!(
            classify_content_drop_zone(100.0, 100.0, 24.0, 50.0),
            Some(ContentDropZone::Left)
        );
        assert_eq!(
            classify_content_drop_zone(100.0, 100.0, 26.0, 50.0),
            Some(ContentDropZone::Center)
        );
        assert_eq!(
            classify_content_drop_zone(100.0, 100.0, 50.0, 24.0),
            Some(ContentDropZone::Top)
        );
        assert_eq!(
            classify_content_drop_zone(100.0, 100.0, 50.0, 26.0),
            Some(ContentDropZone::Center)
        );
    }

    #[test]
    fn content_drop_preview_rect_uses_even_halves() {
        assert_eq!(
            content_drop_preview_rect(ContentDropZone::Left),
            (0.0, 0.0, 0.5, 1.0)
        );
        assert_eq!(
            content_drop_preview_rect(ContentDropZone::Right),
            (0.5, 0.0, 0.5, 1.0)
        );
        assert_eq!(
            content_drop_preview_rect(ContentDropZone::Top),
            (0.0, 0.0, 1.0, 0.5)
        );
        assert_eq!(
            content_drop_preview_rect(ContentDropZone::Bottom),
            (0.0, 0.5, 1.0, 0.5)
        );
        assert_eq!(
            content_drop_preview_rect(ContentDropZone::Center),
            (0.25, 0.25, 0.5, 0.5)
        );
    }

    #[test]
    fn effective_drop_target_dimensions_fall_back_to_content_area() {
        assert_eq!(
            effective_drop_target_dimensions(0, 0, 320, 180),
            Some((320.0, 180.0))
        );
        assert_eq!(
            effective_drop_target_dimensions(120, 60, 320, 180),
            Some((320.0, 180.0))
        );
        assert_eq!(effective_drop_target_dimensions(0, 0, 0, 180), None);
    }

    #[test]
    fn localhost_inputs_only_match_real_localhost_hosts() {
        for input in [
            "localhost",
            "localhost:3000",
            "localhost/path",
            "localhost?q=1",
        ] {
            assert!(is_localhost_input(input), "{input} should be localhost");
        }

        for input in [
            "localhost.run",
            "localhost.example.com",
            "localhost docs",
            "mylocalhost:3000",
        ] {
            assert!(
                !is_localhost_input(input),
                "{input} should not be treated as localhost"
            );
        }
    }

    #[test]
    fn normalize_browser_entry_input_preserves_search_and_domain_behavior() {
        let cases = [
            ("https://example.com", "https://example.com"),
            ("localhost", "http://localhost"),
            ("localhost:3000", "http://localhost:3000"),
            ("localhost/path", "http://localhost/path"),
            ("localhost.run", "https://localhost.run"),
            ("localhost.example.com", "https://localhost.example.com"),
            (
                "localhost docs",
                "https://www.google.com/search?q=localhost+docs",
            ),
            ("example.com", "https://example.com"),
            (
                "example search",
                "https://www.google.com/search?q=example+search",
            ),
        ];

        for (input, expected) in cases {
            assert_eq!(normalize_browser_entry_input(input), expected, "{input}");
        }
    }
}
