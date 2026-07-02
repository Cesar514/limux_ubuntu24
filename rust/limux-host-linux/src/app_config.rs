// summary: Load, parse, and save Limux host application settings.
// purpose: Preserve user-facing appearance, focus, font, and notification preferences.
// inputs: XDG config paths and JSON settings files.
// returns/effects: Creates first-run defaults, rejects corrupt persisted settings, and writes settings atomically.

use std::fs;
use std::path::Path;
use std::time::{SystemTime, UNIX_EPOCH};

use serde::Deserialize;
use serde_json::{json, Value};

use crate::shortcut_config;

pub const SETTINGS_FILE_NAME: &str = "settings.json";

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum ColorScheme {
    #[default]
    System,
    Dark,
    Light,
}

impl ColorScheme {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::System => "system",
            Self::Dark => "dark",
            Self::Light => "light",
        }
    }

    fn from_str(s: &str) -> Option<Self> {
        match s {
            "system" => Some(Self::System),
            "dark" => Some(Self::Dark),
            "light" => Some(Self::Light),
            _ => None,
        }
    }
}

#[derive(Clone, Debug, Default, PartialEq, Deserialize)]
pub struct AppConfig {
    #[serde(default)]
    pub focus: FocusConfig,
    #[serde(skip)]
    pub appearance: AppearanceConfig,
    #[serde(skip)]
    pub notifications: NotificationConfig,
    #[serde(skip)]
    pub workspace_groups: WorkspaceGroupsConfig,
    #[serde(skip)]
    pub new_workspace_placement: WorkspaceGroupNewPlacement,
    #[serde(skip)]
    pub font_size: Option<f32>,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct AppearanceConfig {
    pub color_scheme: ColorScheme,
    pub ghostty_color_scheme: ColorScheme,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Deserialize)]
pub struct FocusConfig {
    #[serde(default)]
    pub hover_terminal_focus: bool,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum WorkspaceGroupNewPlacement {
    #[default]
    AfterCurrent,
    Top,
    End,
}

impl WorkspaceGroupNewPlacement {
    // purpose: Serialize the placement enum using CMUX's config spelling.
    // inputs: Placement value selected from parsed settings or live defaults.
    // returns/effects: Returns a stable config/API string without allocation.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::AfterCurrent => "afterCurrent",
            Self::Top => "top",
            Self::End => "end",
        }
    }

    // purpose: Parse CMUX workspace group placement strings.
    // inputs: Raw string from settings or API parameters.
    // returns/effects: Returns None for unsupported placement names.
    fn from_str(s: &str) -> Option<Self> {
        match s {
            "afterCurrent" => Some(Self::AfterCurrent),
            "top" => Some(Self::Top),
            "end" => Some(Self::End),
            _ => None,
        }
    }
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct WorkspaceGroupsConfig {
    pub new_workspace_placement: WorkspaceGroupNewPlacement,
    pub by_cwd: Vec<WorkspaceGroupCwdConfig>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct WorkspaceGroupCwdConfig {
    pub key: String,
    pub normalized_key: String,
    pub is_glob: bool,
    pub new_workspace_placement: Option<WorkspaceGroupNewPlacement>,
}

impl WorkspaceGroupsConfig {
    // purpose: Resolve the placement default for a workspace group child.
    // inputs: Optional cwd for the source/reference workspace.
    // returns/effects: Returns per-cwd placement when matched, otherwise the global default.
    pub fn new_workspace_placement_for_cwd(&self, cwd: Option<&str>) -> WorkspaceGroupNewPlacement {
        cwd.and_then(|cwd| self.matching_cwd_entry(cwd))
            .and_then(|entry| entry.new_workspace_placement)
            .unwrap_or(self.new_workspace_placement)
    }

    // purpose: Find the most specific byCwd entry for a normalized source cwd.
    // inputs: Raw cwd from the anchor/reference workspace.
    // returns/effects: Returns the longest matching prefix or glob entry.
    fn matching_cwd_entry(&self, cwd: &str) -> Option<&WorkspaceGroupCwdConfig> {
        if cwd.trim().is_empty() {
            return None;
        }
        let normalized_cwd = normalize_absolute_path(cwd);
        self.by_cwd
            .iter()
            .filter(|entry| cwd_entry_matches(entry, &normalized_cwd))
            .max_by_key(|entry| entry.normalized_key.len())
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum NotificationSound {
    #[default]
    Default,
    Message,
    Bell,
    Complete,
    Alert,
    Basso,
    Blow,
    Bottle,
    Frog,
    Funk,
    Glass,
    Hero,
    Morse,
    Ping,
    Pop,
    Purr,
    Sosumi,
    Submarine,
    Tink,
    CustomFile,
    None,
}

impl NotificationSound {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Default => "default",
            Self::Message => "message",
            Self::Bell => "bell",
            Self::Complete => "complete",
            Self::Alert => "alert",
            Self::Basso => "Basso",
            Self::Blow => "Blow",
            Self::Bottle => "Bottle",
            Self::Frog => "Frog",
            Self::Funk => "Funk",
            Self::Glass => "Glass",
            Self::Hero => "Hero",
            Self::Morse => "Morse",
            Self::Ping => "Ping",
            Self::Pop => "Pop",
            Self::Purr => "Purr",
            Self::Sosumi => "Sosumi",
            Self::Submarine => "Submarine",
            Self::Tink => "Tink",
            Self::CustomFile => "custom_file",
            Self::None => "none",
        }
    }

    pub fn labels() -> &'static [&'static str] {
        &[
            "Default",
            "Message",
            "Bell",
            "Complete",
            "Alert",
            "Basso",
            "Blow",
            "Bottle",
            "Frog",
            "Funk",
            "Glass",
            "Hero",
            "Morse",
            "Ping",
            "Pop",
            "Purr",
            "Sosumi",
            "Submarine",
            "Tink",
            "Custom file",
            "None",
        ]
    }

    pub fn dropdown_index(self) -> u32 {
        match self {
            Self::Default => 0,
            Self::Message => 1,
            Self::Bell => 2,
            Self::Complete => 3,
            Self::Alert => 4,
            Self::Basso => 5,
            Self::Blow => 6,
            Self::Bottle => 7,
            Self::Frog => 8,
            Self::Funk => 9,
            Self::Glass => 10,
            Self::Hero => 11,
            Self::Morse => 12,
            Self::Ping => 13,
            Self::Pop => 14,
            Self::Purr => 15,
            Self::Sosumi => 16,
            Self::Submarine => 17,
            Self::Tink => 18,
            Self::CustomFile => 19,
            Self::None => 20,
        }
    }

    pub fn from_dropdown_index(index: u32) -> Self {
        match index {
            1 => Self::Message,
            2 => Self::Bell,
            3 => Self::Complete,
            4 => Self::Alert,
            5 => Self::Basso,
            6 => Self::Blow,
            7 => Self::Bottle,
            8 => Self::Frog,
            9 => Self::Funk,
            10 => Self::Glass,
            11 => Self::Hero,
            12 => Self::Morse,
            13 => Self::Ping,
            14 => Self::Pop,
            15 => Self::Purr,
            16 => Self::Sosumi,
            17 => Self::Submarine,
            18 => Self::Tink,
            19 => Self::CustomFile,
            20 => Self::None,
            _ => Self::Default,
        }
    }

    pub fn freedesktop_sound_name(self) -> Option<&'static str> {
        match self {
            Self::Default | Self::None => None,
            Self::Message => Some("message-new-instant"),
            Self::Bell => Some("bell-terminal"),
            Self::Complete => Some("complete"),
            Self::Alert => Some("dialog-warning"),
            Self::Basso => Some("Basso"),
            Self::Blow => Some("Blow"),
            Self::Bottle => Some("Bottle"),
            Self::Frog => Some("Frog"),
            Self::Funk => Some("Funk"),
            Self::Glass => Some("Glass"),
            Self::Hero => Some("Hero"),
            Self::Morse => Some("Morse"),
            Self::Ping => Some("Ping"),
            Self::Pop => Some("Pop"),
            Self::Purr => Some("Purr"),
            Self::Sosumi => Some("Sosumi"),
            Self::Submarine => Some("Submarine"),
            Self::Tink => Some("Tink"),
            Self::CustomFile => None,
        }
    }

    fn from_str(s: &str) -> Option<Self> {
        match s {
            "default" => Some(Self::Default),
            "message" => Some(Self::Message),
            "bell" => Some(Self::Bell),
            "complete" => Some(Self::Complete),
            "alert" => Some(Self::Alert),
            "Basso" => Some(Self::Basso),
            "Blow" => Some(Self::Blow),
            "Bottle" => Some(Self::Bottle),
            "Frog" => Some(Self::Frog),
            "Funk" => Some(Self::Funk),
            "Glass" => Some(Self::Glass),
            "Hero" => Some(Self::Hero),
            "Morse" => Some(Self::Morse),
            "Ping" => Some(Self::Ping),
            "Pop" => Some(Self::Pop),
            "Purr" => Some(Self::Purr),
            "Sosumi" => Some(Self::Sosumi),
            "Submarine" => Some(Self::Submarine),
            "Tink" => Some(Self::Tink),
            "custom_file" => Some(Self::CustomFile),
            "none" => Some(Self::None),
            _ => None,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct NotificationConfig {
    pub enabled: bool,
    pub sound: NotificationSound,
    pub custom_sound_file_path: String,
    pub hooks: Vec<NotificationHookConfig>,
    pub agent_permission_prompt: bool,
    pub agent_turn_complete: AgentTurnCompleteMode,
    pub agent_idle_reminder: bool,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct NotificationHookConfig {
    pub id: String,
    pub command: String,
    pub enabled: bool,
    pub timeout_seconds: u64,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum AgentTurnCompleteMode {
    #[default]
    WhenIdle,
    Always,
    Never,
}

impl AgentTurnCompleteMode {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::WhenIdle => "whenIdle",
            Self::Always => "always",
            Self::Never => "never",
        }
    }

    fn from_str(s: &str) -> Option<Self> {
        match s {
            "whenIdle" => Some(Self::WhenIdle),
            "always" => Some(Self::Always),
            "never" => Some(Self::Never),
            _ => None,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AgentNotifyCategory {
    TurnComplete,
    NeedsPermission,
    IdleReminder,
    Other,
}

impl AgentNotifyCategory {
    pub fn from_str(raw: &str) -> Option<Self> {
        match raw {
            "turn-complete" => Some(Self::TurnComplete),
            "needs-permission" => Some(Self::NeedsPermission),
            "idle-reminder" => Some(Self::IdleReminder),
            "other" => Some(Self::Other),
            _ => None,
        }
    }
}

impl Default for NotificationConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            sound: NotificationSound::Default,
            custom_sound_file_path: String::new(),
            hooks: Vec::new(),
            agent_permission_prompt: true,
            agent_turn_complete: AgentTurnCompleteMode::WhenIdle,
            agent_idle_reminder: true,
        }
    }
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct LoadedAppConfig {
    pub config: AppConfig,
    pub warnings: Vec<String>,
}

pub fn load() -> LoadedAppConfig {
    let Some(path) = settings_path() else {
        panic!("config_dir unavailable; cannot load app settings");
    };

    if let Err(err) = ensure_default_config_file(&path) {
        panic!(
            "failed to create default app config `{}`: {err}",
            path.display()
        );
    }

    load_from_path(&path)
}

pub fn settings_path() -> Option<std::path::PathBuf> {
    shortcut_config::config_dir_path().map(|dir| dir.join(SETTINGS_FILE_NAME))
}

#[cfg(test)]
pub fn settings_path_in(base: &Path) -> std::path::PathBuf {
    shortcut_config::config_dir_path_in(base).join(SETTINGS_FILE_NAME)
}

pub fn load_from_path(path: &Path) -> LoadedAppConfig {
    if !path.exists() {
        return LoadedAppConfig::default();
    }

    let raw = match fs::read_to_string(path) {
        Ok(raw) => raw,
        Err(err) => {
            panic!("failed to read app config `{}`: {err}", path.display());
        }
    };

    match serde_json::from_str::<Value>(&raw) {
        Ok(root) => LoadedAppConfig {
            config: parse_app_config_value(&root),
            warnings: Vec::new(),
        },
        Err(err) => {
            panic!("failed to load app config `{}`: {err}", path.display());
        }
    }
}

fn parse_app_config_value(root: &Value) -> AppConfig {
    let hover_terminal_focus = root
        .get("focus")
        .and_then(Value::as_object)
        .and_then(|focus| focus.get("hover_terminal_focus"))
        .and_then(Value::as_bool)
        .unwrap_or(false);

    let appearance = root.get("appearance").and_then(Value::as_object);

    let color_scheme = appearance
        .and_then(|appearance| appearance.get("color_scheme"))
        .and_then(Value::as_str)
        .and_then(ColorScheme::from_str)
        .unwrap_or_default();

    let ghostty_color_scheme = appearance
        .and_then(|appearance| appearance.get("ghostty_color_scheme"))
        .and_then(Value::as_str)
        .and_then(ColorScheme::from_str)
        .unwrap_or(color_scheme);
    let app = root.get("app").map(|value| {
        value
            .as_object()
            .unwrap_or_else(|| panic!("app must be an object"))
    });
    let new_workspace_placement = app
        .and_then(|app| app.get("newWorkspacePlacement"))
        .map(|value| parse_workspace_new_placement(value, "app.newWorkspacePlacement"))
        .unwrap_or_default();

    let notifications = root.get("notifications").and_then(Value::as_object);
    let notification_defaults = NotificationConfig::default();
    let notifications_enabled = notifications
        .and_then(|notifications| notifications.get("enabled"))
        .and_then(Value::as_bool)
        .unwrap_or(notification_defaults.enabled);
    let notification_sound = notifications
        .and_then(|notifications| notifications.get("sound"))
        .map(|value| parse_notification_sound(value, "notifications.sound"))
        .unwrap_or(notification_defaults.sound);
    let custom_sound_file_path = notifications
        .and_then(|notifications| notifications.get("customSoundFilePath"))
        .map(|value| parse_string_setting(value, "notifications.customSoundFilePath"))
        .unwrap_or_default();
    let notification_hooks = notifications
        .and_then(|notifications| notifications.get("hooks"))
        .map(parse_notification_hooks)
        .unwrap_or_default();
    let agent_permission_prompt = notifications
        .and_then(|notifications| notifications.get("agentPermissionPrompt"))
        .map(|value| parse_bool_setting(value, "notifications.agentPermissionPrompt"))
        .unwrap_or(notification_defaults.agent_permission_prompt);
    let agent_turn_complete = notifications
        .and_then(|notifications| notifications.get("agentTurnComplete"))
        .map(|value| parse_agent_turn_complete_mode(value, "notifications.agentTurnComplete"))
        .unwrap_or(notification_defaults.agent_turn_complete);
    let agent_idle_reminder = notifications
        .and_then(|notifications| notifications.get("agentIdleReminder"))
        .map(|value| parse_bool_setting(value, "notifications.agentIdleReminder"))
        .unwrap_or(notification_defaults.agent_idle_reminder);
    let workspace_groups = root
        .get("workspaceGroups")
        .map(parse_workspace_groups_config)
        .unwrap_or_default();

    let font_size = root
        .get("font_size")
        .and_then(Value::as_f64)
        .map(|v| v as f32)
        .filter(|v| (1.0..=255.0).contains(v));

    AppConfig {
        focus: FocusConfig {
            hover_terminal_focus,
        },
        appearance: AppearanceConfig {
            color_scheme,
            ghostty_color_scheme,
        },
        notifications: NotificationConfig {
            enabled: notifications_enabled,
            sound: notification_sound,
            custom_sound_file_path,
            hooks: notification_hooks,
            agent_permission_prompt,
            agent_turn_complete,
            agent_idle_reminder,
        },
        workspace_groups,
        new_workspace_placement,
        font_size,
    }
}

// purpose: Parse a required boolean setting value without silent coercion.
// inputs: Raw JSON value and user-facing config path.
// returns/effects: Returns bool or panics for malformed existing config.
fn parse_bool_setting(value: &Value, path: &str) -> bool {
    value
        .as_bool()
        .unwrap_or_else(|| panic!("{path} must be a boolean"))
}

// purpose: Parse a required string setting value without silent coercion.
// inputs: Raw JSON value and user-facing config path.
// returns/effects: Returns the owned string or panics for malformed existing config.
fn parse_string_setting(value: &Value, path: &str) -> String {
    value
        .as_str()
        .unwrap_or_else(|| panic!("{path} must be a string"))
        .to_string()
}

// purpose: Parse CMUX-compatible notification sound names.
// inputs: Raw JSON value and user-facing config path.
// returns/effects: Returns the selected sound or panics for malformed existing config.
fn parse_notification_sound(value: &Value, path: &str) -> NotificationSound {
    let raw = value
        .as_str()
        .unwrap_or_else(|| panic!("{path} must be a string"));
    NotificationSound::from_str(raw).unwrap_or_else(|| {
        panic!(
            "{path} must be one of default, message, bell, complete, alert, \
             CMUX presets, custom_file, or none"
        )
    })
}

// purpose: Parse CMUX's agent turn-complete notification mode.
// inputs: Raw JSON value and user-facing config path.
// returns/effects: Returns mode or panics for malformed existing config.
fn parse_agent_turn_complete_mode(value: &Value, path: &str) -> AgentTurnCompleteMode {
    let raw = value
        .as_str()
        .unwrap_or_else(|| panic!("{path} must be a string"));
    AgentTurnCompleteMode::from_str(raw)
        .unwrap_or_else(|| panic!("{path} must be one of whenIdle, always, or never"))
}

// purpose: Decide whether an agent-tagged notification should deliver.
// inputs: Optional category, pending-work flag, and current notification config.
// returns/effects: Returns false when CMUX agent notification settings suppress it.
pub fn agent_notification_should_deliver(
    category: Option<AgentNotifyCategory>,
    pending: bool,
    config: &NotificationConfig,
) -> bool {
    match category.unwrap_or(AgentNotifyCategory::Other) {
        AgentNotifyCategory::NeedsPermission => config.agent_permission_prompt,
        AgentNotifyCategory::TurnComplete => match config.agent_turn_complete {
            AgentTurnCompleteMode::Always => true,
            AgentTurnCompleteMode::Never => false,
            AgentTurnCompleteMode::WhenIdle => !pending,
        },
        AgentNotifyCategory::IdleReminder => config.agent_idle_reminder && !pending,
        AgentNotifyCategory::Other => true,
    }
}

// purpose: Parse CMUX-compatible workspace group settings.
// inputs: Value from workspaceGroups in Limux settings JSON.
// returns/effects: Returns group placement config or panics on malformed placement.
fn parse_workspace_groups_config(value: &Value) -> WorkspaceGroupsConfig {
    let object = value
        .as_object()
        .unwrap_or_else(|| panic!("workspaceGroups must be an object"));
    let new_workspace_placement = object
        .get("newWorkspacePlacement")
        .map(|value| parse_workspace_new_placement(value, "workspaceGroups.newWorkspacePlacement"))
        .unwrap_or_default();
    let by_cwd = object
        .get("byCwd")
        .map(parse_workspace_group_cwd_configs)
        .unwrap_or_default();
    WorkspaceGroupsConfig {
        new_workspace_placement,
        by_cwd,
    }
}

// purpose: Parse workspaceGroups.byCwd map entries.
// inputs: Value from workspaceGroups.byCwd.
// returns/effects: Returns normalized cwd config entries or panics on malformed shape.
fn parse_workspace_group_cwd_configs(value: &Value) -> Vec<WorkspaceGroupCwdConfig> {
    let entries = value
        .as_object()
        .unwrap_or_else(|| panic!("workspaceGroups.byCwd must be an object"));
    entries
        .iter()
        .filter_map(|(key, entry)| {
            let trimmed = key.trim();
            if trimmed.is_empty() {
                return None;
            }
            let object = entry
                .as_object()
                .unwrap_or_else(|| panic!("workspaceGroups.byCwd[{trimmed}] must be an object"));
            let placement = object.get("newWorkspacePlacement").map(|value| {
                parse_workspace_new_placement(
                    value,
                    &format!("workspaceGroups.byCwd[{trimmed}].newWorkspacePlacement"),
                )
            });
            let is_glob = trimmed.contains('*') || trimmed.contains('?');
            let normalized_key = if is_glob {
                expand_tilde_preserving_glob(trimmed)
            } else {
                normalize_absolute_path(trimmed)
            };
            Some(WorkspaceGroupCwdConfig {
                key: trimmed.to_string(),
                normalized_key,
                is_glob,
                new_workspace_placement: placement,
            })
        })
        .collect()
}

// purpose: Parse one CMUX workspace placement setting.
// inputs: JSON string value plus a diagnostic label.
// returns/effects: Returns a valid placement or panics loudly.
fn parse_workspace_new_placement(value: &Value, label: &str) -> WorkspaceGroupNewPlacement {
    let raw = value
        .as_str()
        .unwrap_or_else(|| panic!("{label} must be a string"));
    WorkspaceGroupNewPlacement::from_str(raw)
        .unwrap_or_else(|| panic!("{label} must be afterCurrent, top, or end"))
}

// purpose: Parse CMUX-style notification hook definitions from settings JSON.
// inputs: Value from notifications.hooks.
// returns/effects: Returns enabled/disabled hooks or panics on malformed hook objects.
fn parse_notification_hooks(value: &Value) -> Vec<NotificationHookConfig> {
    let hooks = value
        .as_array()
        .unwrap_or_else(|| panic!("notifications.hooks must be an array"));
    hooks
        .iter()
        .enumerate()
        .map(|(index, hook)| {
            let object = hook
                .as_object()
                .unwrap_or_else(|| panic!("notifications.hooks[{index}] must be an object"));
            let id = object
                .get("id")
                .and_then(Value::as_str)
                .filter(|id| !id.trim().is_empty())
                .unwrap_or_else(|| panic!("notifications.hooks[{index}].id is required"))
                .to_string();
            let command = object
                .get("command")
                .and_then(Value::as_str)
                .filter(|command| !command.trim().is_empty())
                .unwrap_or_else(|| panic!("notifications.hooks[{index}].command is required"))
                .to_string();
            let enabled = object
                .get("enabled")
                .and_then(Value::as_bool)
                .unwrap_or(true);
            let timeout_seconds = object
                .get("timeoutSeconds")
                .or_else(|| object.get("timeout_seconds"))
                .and_then(Value::as_u64)
                .unwrap_or(20);
            NotificationHookConfig {
                id,
                command,
                enabled,
                timeout_seconds,
            }
        })
        .collect()
}

// purpose: Expand a leading tilde while preserving glob characters.
// inputs: Raw cwd key from workspaceGroups.byCwd.
// returns/effects: Returns a comparable path pattern without filesystem access.
fn expand_tilde_preserving_glob(pattern: &str) -> String {
    let trimmed = pattern.trim();
    let Some(suffix) = trimmed.strip_prefix('~') else {
        return trimmed.to_string();
    };
    let home = dirs::home_dir()
        .map(|path| path.to_string_lossy().to_string())
        .unwrap_or_else(|| panic!("home directory unavailable for workspaceGroups.byCwd tilde"));
    if suffix.is_empty() {
        home
    } else {
        format!("{home}{suffix}")
    }
}

// purpose: Normalize a cwd/prefix setting for deterministic matching.
// inputs: Raw cwd or non-glob byCwd key.
// returns/effects: Expands tilde and removes simple lexical path noise.
fn normalize_absolute_path(path: &str) -> String {
    let expanded = expand_tilde_preserving_glob(path);
    std::path::PathBuf::from(expanded)
        .components()
        .collect::<std::path::PathBuf>()
        .to_string_lossy()
        .to_string()
}

// purpose: Match one workspaceGroups.byCwd entry against a normalized cwd.
// inputs: Resolved config entry and normalized cwd.
// returns/effects: Returns true for glob or prefix matches.
fn cwd_entry_matches(entry: &WorkspaceGroupCwdConfig, cwd: &str) -> bool {
    let key = entry.normalized_key.as_str();
    if entry.is_glob {
        return glob_match(key, cwd);
    }
    if cwd == key {
        return true;
    }
    if key == "/" {
        return cwd.starts_with('/');
    }
    cwd.strip_prefix(key)
        .is_some_and(|suffix| suffix.starts_with('/'))
}

// purpose: Provide the minimal CMUX-style glob matching needed for byCwd keys.
// inputs: Pattern with '*' and '?' plus candidate cwd.
// returns/effects: Returns true when the pattern matches the whole candidate.
fn glob_match(pattern: &str, candidate: &str) -> bool {
    let pattern = pattern.as_bytes();
    let candidate = candidate.as_bytes();
    let anchored_start = !pattern.starts_with(b"*");
    let anchored_end = !pattern.ends_with(b"*");
    let parts = pattern
        .split(|byte| *byte == b'*')
        .filter(|part| !part.is_empty())
        .collect::<Vec<_>>();
    if parts.is_empty() {
        return true;
    }
    let mut offset = 0;
    for (index, part) in parts.iter().enumerate() {
        let is_first = index == 0;
        let is_last = index + 1 == parts.len();
        let found = glob_part_position(
            candidate,
            part,
            offset,
            anchored_start && is_first,
            anchored_end && is_last,
        );
        let Some(start) = found else {
            return false;
        };
        offset = start + part.len();
    }
    true
}

// purpose: Find one non-star glob part inside a candidate path.
// inputs: Candidate bytes, glob part bytes, current offset, and anchor requirements.
// returns/effects: Returns the matched byte offset or None when the part cannot match.
fn glob_part_position(
    candidate: &[u8],
    part: &[u8],
    offset: usize,
    anchored_start: bool,
    anchored_end: bool,
) -> Option<usize> {
    if anchored_start {
        return glob_part_matches_at(candidate, part, 0).filter(|_| offset == 0);
    }
    if anchored_end {
        return candidate
            .len()
            .checked_sub(part.len())
            .filter(|start| *start >= offset)
            .filter(|start| glob_part_matches_at(candidate, part, *start).is_some());
    }
    let end = candidate.len().checked_sub(part.len())?;
    (offset..=end).find(|start| glob_part_matches_at(candidate, part, *start).is_some())
}

// purpose: Match a non-star glob part at a fixed path offset.
// inputs: Candidate bytes, part bytes where '?' matches one byte, and start offset.
// returns/effects: Returns the start offset only when the whole part matches there.
fn glob_part_matches_at(candidate: &[u8], part: &[u8], start: usize) -> Option<usize> {
    let end = start.checked_add(part.len())?;
    let window = candidate.get(start..end)?;
    part.iter()
        .zip(window)
        .all(|(pattern, candidate)| *pattern == b'?' || pattern == candidate)
        .then_some(start)
}

pub fn save(config: &AppConfig) -> Result<(), String> {
    let Some(path) = settings_path() else {
        return Err("config_dir unavailable; cannot save app settings".to_string());
    };

    save_to_path(&path, config)
        .map_err(|err| format!("failed to save app config `{}`: {err}", path.display()))
}

fn save_to_path(path: &Path, config: &AppConfig) -> Result<(), String> {
    let mut root = read_existing_config_root_for_save(path)?;

    root.insert(
        "appearance".to_string(),
        json!({
            "color_scheme": config.appearance.color_scheme.as_str(),
            "ghostty_color_scheme": config.appearance.ghostty_color_scheme.as_str(),
        }),
    );
    root.insert(
        "focus".to_string(),
        json!({ "hover_terminal_focus": config.focus.hover_terminal_focus }),
    );
    let app = root.entry("app".to_string()).or_insert_with(|| json!({}));
    if !app.is_object() {
        *app = json!({});
    }
    app.as_object_mut().expect("app object").insert(
        "newWorkspacePlacement".to_string(),
        json!(config.new_workspace_placement.as_str()),
    );
    root.insert(
        "notifications".to_string(),
        json!({
            "enabled": config.notifications.enabled,
            "sound": config.notifications.sound.as_str(),
            "customSoundFilePath": config.notifications.custom_sound_file_path.clone(),
            "agentPermissionPrompt": config.notifications.agent_permission_prompt,
            "agentTurnComplete": config.notifications.agent_turn_complete.as_str(),
            "agentIdleReminder": config.notifications.agent_idle_reminder,
            "hooks": config.notifications.hooks.iter().map(|hook| json!({
                "id": hook.id,
                "command": hook.command,
                "enabled": hook.enabled,
                "timeoutSeconds": hook.timeout_seconds,
            })).collect::<Vec<_>>(),
        }),
    );
    let workspace_groups = root
        .entry("workspaceGroups".to_string())
        .or_insert_with(|| json!({}));
    if !workspace_groups.is_object() {
        *workspace_groups = json!({});
    }
    workspace_groups
        .as_object_mut()
        .expect("workspaceGroups object")
        .insert(
            "newWorkspacePlacement".to_string(),
            json!(config.workspace_groups.new_workspace_placement.as_str()),
        );

    if let Some(size) = config.font_size {
        root.insert("font_size".to_string(), json!(size));
    } else {
        root.remove("font_size");
    }

    let serialized =
        serde_json::to_string_pretty(&Value::Object(root)).expect("config should serialize");
    write_config_root_atomically(path, &serialized)
}

fn read_existing_config_root_for_save(
    path: &Path,
) -> Result<serde_json::Map<String, Value>, String> {
    if !path.exists() {
        return Ok(serde_json::Map::new());
    }

    let raw = fs::read_to_string(path).map_err(|err| err.to_string())?;
    match serde_json::from_str::<Value>(&raw) {
        Ok(Value::Object(map)) => Ok(map),
        Ok(_) => Err("existing app config root must be a JSON object".to_string()),
        Err(err) => Err(format!("existing app config is invalid JSON: {err}")),
    }
}

fn write_config_root_atomically(path: &Path, serialized: &str) -> Result<(), String> {
    let Some(parent) = path.parent() else {
        return Err("config path has no parent directory".to_string());
    };
    fs::create_dir_all(parent).map_err(|err| err.to_string())?;

    let temp_path = temp_config_path(path);
    fs::write(&temp_path, format!("{serialized}\n")).map_err(|err| err.to_string())?;

    if let Err(err) = fs::rename(&temp_path, path) {
        let _ = fs::remove_file(&temp_path);
        return Err(err.to_string());
    }

    Ok(())
}

fn temp_config_path(path: &Path) -> std::path::PathBuf {
    timestamped_sibling_path(path, "tmp")
}

fn timestamped_sibling_path(path: &Path, suffix: &str) -> std::path::PathBuf {
    let stem = path
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("settings.json");
    let nonce = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_nanos())
        .unwrap_or_default();
    let file_name = format!(".{stem}.{suffix}-{}-{nonce}", std::process::id());
    path.with_file_name(file_name)
}

fn ensure_default_config_file(path: &Path) -> std::io::Result<()> {
    if path.exists() {
        return Ok(());
    }

    let Some(parent) = path.parent() else {
        return Ok(());
    };

    fs::create_dir_all(parent)?;
    let default_root = json!({
        "appearance": {
            "color_scheme": "dark",
            "ghostty_color_scheme": "dark"
        },
        "focus": {
            "hover_terminal_focus": false
        },
        "notifications": {
            "enabled": true,
            "sound": "default"
        }
    });
    let serialized = serde_json::to_string_pretty(&default_root)
        .expect("default app config should always serialize");
    fs::write(path, format!("{serialized}\n"))
}

#[cfg(test)]
mod tests {
    use super::*;

    use std::ffi::OsString;

    use tempfile::TempDir;

    struct EnvVarGuard {
        key: &'static str,
        previous: Option<OsString>,
    }

    impl EnvVarGuard {
        fn set(key: &'static str, value: impl AsRef<std::ffi::OsStr>) -> Self {
            let previous = std::env::var_os(key);
            std::env::set_var(key, value);
            Self { key, previous }
        }
    }

    impl Drop for EnvVarGuard {
        fn drop(&mut self) {
            if let Some(value) = self.previous.as_ref() {
                std::env::set_var(self.key, value);
            } else {
                std::env::remove_var(self.key);
            }
        }
    }

    #[test]
    fn load_from_path_uses_defaults_when_file_is_missing() {
        let dir = TempDir::new().expect("temp dir");
        let path = settings_path_in(dir.path());

        let loaded = load_from_path(&path);

        assert_eq!(loaded, LoadedAppConfig::default());
    }

    #[test]
    fn settings_path_in_uses_limux_settings_json() {
        let path = settings_path_in(Path::new("/tmp/example"));

        assert_eq!(path, Path::new("/tmp/example/limux/settings.json"));
    }

    #[test]
    fn ensure_default_config_file_writes_dark_appearance_and_notification_defaults() {
        let dir = TempDir::new().expect("temp dir");
        let path = settings_path_in(dir.path());

        ensure_default_config_file(&path).expect("write default config");

        let raw = fs::read_to_string(&path).expect("read config");
        let parsed: Value = serde_json::from_str(&raw).expect("parse config");
        assert_eq!(parsed["focus"]["hover_terminal_focus"], Value::Bool(false));
        assert_eq!(
            parsed["appearance"]["color_scheme"],
            Value::String("dark".to_string())
        );
        assert_eq!(
            parsed["appearance"]["ghostty_color_scheme"],
            Value::String("dark".to_string())
        );
        assert_eq!(parsed["notifications"]["enabled"], Value::Bool(true));
        assert_eq!(
            parsed["notifications"]["sound"],
            Value::String("default".to_string())
        );
    }

    #[test]
    fn load_from_path_reads_focus_settings_and_ignores_other_sections() {
        let dir = TempDir::new().expect("temp dir");
        let path = settings_path_in(dir.path());
        fs::create_dir_all(path.parent().expect("config dir")).expect("create config dir");
        fs::write(
            &path,
            r#"{
  "focus": {
    "hover_terminal_focus": true
  }
}
"#,
        )
        .expect("write config");

        let loaded = load_from_path(&path);

        assert!(loaded.warnings.is_empty());
        assert!(loaded.config.focus.hover_terminal_focus);
    }

    #[test]
    fn load_from_path_defaults_ghostty_scheme_to_gtk_scheme_for_legacy_configs() {
        let dir = TempDir::new().expect("temp dir");
        let path = settings_path_in(dir.path());
        fs::create_dir_all(path.parent().expect("config dir")).expect("create config dir");
        fs::write(
            &path,
            r#"{
  "appearance": {
    "color_scheme": "dark"
  }
}
"#,
        )
        .expect("write config");

        let loaded = load_from_path(&path);

        assert!(loaded.warnings.is_empty());
        assert_eq!(loaded.config.appearance.color_scheme, ColorScheme::Dark);
        assert_eq!(
            loaded.config.appearance.ghostty_color_scheme,
            ColorScheme::Dark
        );
    }

    #[test]
    fn load_from_path_reads_font_size_when_valid() {
        let dir = TempDir::new().expect("temp dir");
        let path = settings_path_in(dir.path());
        fs::create_dir_all(path.parent().expect("config dir")).expect("create config dir");
        fs::write(
            &path,
            r#"{
  "font_size": 18.5
}
"#,
        )
        .expect("write config");

        let loaded = load_from_path(&path);

        assert!(loaded.warnings.is_empty());
        assert_eq!(loaded.config.font_size, Some(18.5));
    }

    #[test]
    fn load_from_path_reads_notification_preferences() {
        let dir = TempDir::new().expect("temp dir");
        let path = settings_path_in(dir.path());
        fs::create_dir_all(path.parent().expect("config dir")).expect("create config dir");
        fs::write(
            &path,
            r#"{
  "notifications": {
    "enabled": false,
    "sound": "bell",
    "customSoundFilePath": "/tmp/notify.wav",
    "agentPermissionPrompt": false,
    "agentTurnComplete": "always",
    "agentIdleReminder": false
  }
}
"#,
        )
        .expect("write config");

        let loaded = load_from_path(&path);

        assert!(loaded.warnings.is_empty());
        assert!(!loaded.config.notifications.enabled);
        assert_eq!(loaded.config.notifications.sound, NotificationSound::Bell);
        assert_eq!(
            loaded.config.notifications.custom_sound_file_path,
            "/tmp/notify.wav"
        );
        assert!(!loaded.config.notifications.agent_permission_prompt);
        assert_eq!(
            loaded.config.notifications.agent_turn_complete,
            AgentTurnCompleteMode::Always
        );
        assert!(!loaded.config.notifications.agent_idle_reminder);
    }

    #[test]
    fn load_from_path_reads_cmux_notification_sound_presets() {
        let dir = TempDir::new().expect("temp dir");
        let path = settings_path_in(dir.path());
        fs::create_dir_all(path.parent().expect("config dir")).expect("create config dir");
        fs::write(
            &path,
            r#"{
  "notifications": {
    "sound": "Ping",
    "customSoundFilePath": "/tmp/notify.wav"
  }
}
"#,
        )
        .expect("write config");

        let loaded = load_from_path(&path);

        assert_eq!(loaded.config.notifications.sound, NotificationSound::Ping);
        assert_eq!(
            loaded.config.notifications.custom_sound_file_path,
            "/tmp/notify.wav"
        );
    }

    #[test]
    #[should_panic(expected = "notifications.sound must be one of")]
    fn load_from_path_rejects_invalid_notification_sound() {
        let dir = TempDir::new().expect("temp dir");
        let path = settings_path_in(dir.path());
        fs::create_dir_all(path.parent().expect("config dir")).expect("create config dir");
        fs::write(&path, r#"{"notifications":{"sound":"Loud"}}"#).expect("write config");

        let _ = load_from_path(&path);
    }

    #[test]
    fn agent_notification_gate_follows_cmux_decision_table() {
        let mut config = NotificationConfig::default();
        assert!(agent_notification_should_deliver(
            Some(AgentNotifyCategory::NeedsPermission),
            true,
            &config
        ));
        config.agent_permission_prompt = false;
        assert!(!agent_notification_should_deliver(
            Some(AgentNotifyCategory::NeedsPermission),
            false,
            &config
        ));

        config.agent_turn_complete = AgentTurnCompleteMode::WhenIdle;
        assert!(agent_notification_should_deliver(
            Some(AgentNotifyCategory::TurnComplete),
            false,
            &config
        ));
        assert!(!agent_notification_should_deliver(
            Some(AgentNotifyCategory::TurnComplete),
            true,
            &config
        ));

        config.agent_turn_complete = AgentTurnCompleteMode::Always;
        assert!(agent_notification_should_deliver(
            Some(AgentNotifyCategory::TurnComplete),
            true,
            &config
        ));
        config.agent_turn_complete = AgentTurnCompleteMode::Never;
        assert!(!agent_notification_should_deliver(
            Some(AgentNotifyCategory::TurnComplete),
            false,
            &config
        ));

        config.agent_idle_reminder = true;
        assert!(agent_notification_should_deliver(
            Some(AgentNotifyCategory::IdleReminder),
            false,
            &config
        ));
        assert!(!agent_notification_should_deliver(
            Some(AgentNotifyCategory::IdleReminder),
            true,
            &config
        ));
        config.agent_idle_reminder = false;
        assert!(!agent_notification_should_deliver(
            Some(AgentNotifyCategory::IdleReminder),
            false,
            &config
        ));
        assert!(agent_notification_should_deliver(None, true, &config));
    }

    #[test]
    fn load_from_path_reads_notification_hooks() {
        let dir = TempDir::new().expect("temp dir");
        let path = settings_path_in(dir.path());
        fs::create_dir_all(path.parent().expect("config dir")).expect("create config dir");
        fs::write(
            &path,
            r#"{
  "notifications": {
    "hooks": [
      {
        "id": "agent-filter",
        "command": "sed 's/true/false/'",
        "enabled": false,
        "timeoutSeconds": 7
      }
    ]
  }
}
"#,
        )
        .expect("write config");

        let loaded = load_from_path(&path);

        assert_eq!(loaded.config.notifications.hooks.len(), 1);
        let hook = &loaded.config.notifications.hooks[0];
        assert_eq!(hook.id, "agent-filter");
        assert_eq!(hook.command, "sed 's/true/false/'");
        assert!(!hook.enabled);
        assert_eq!(hook.timeout_seconds, 7);
    }

    #[test]
    fn load_from_path_reads_app_new_workspace_placement() {
        let dir = TempDir::new().expect("temp dir");
        let path = settings_path_in(dir.path());
        fs::create_dir_all(path.parent().expect("config dir")).expect("create config dir");
        fs::write(
            &path,
            r#"{
  "app": {
    "newWorkspacePlacement": "top"
  }
}
"#,
        )
        .expect("write config");

        let loaded = load_from_path(&path);

        assert_eq!(
            loaded.config.new_workspace_placement,
            WorkspaceGroupNewPlacement::Top
        );
    }

    #[test]
    #[should_panic(expected = "app.newWorkspacePlacement must be afterCurrent, top, or end")]
    fn load_from_path_rejects_invalid_app_new_workspace_placement() {
        let dir = TempDir::new().expect("temp dir");
        let path = settings_path_in(dir.path());
        fs::create_dir_all(path.parent().expect("config dir")).expect("create config dir");
        fs::write(
            &path,
            r#"{
  "app": {
    "newWorkspacePlacement": "middle"
  }
}
"#,
        )
        .expect("write config");

        let _ = load_from_path(&path);
    }

    #[test]
    fn load_from_path_reads_workspace_group_placement_config() {
        let dir = TempDir::new().expect("temp dir");
        let path = settings_path_in(dir.path());
        fs::create_dir_all(path.parent().expect("config dir")).expect("create config dir");
        fs::write(
            &path,
            r#"{
  "workspaceGroups": {
    "newWorkspacePlacement": "end",
    "byCwd": {
      "/tmp/projects": {
        "newWorkspacePlacement": "top"
      },
      "/tmp/projects/special": {
        "newWorkspacePlacement": "afterCurrent"
      },
      "/tmp/worktrees/*": {
        "newWorkspacePlacement": "top"
      }
    }
  }
}
"#,
        )
        .expect("write config");

        let loaded = load_from_path(&path);
        let groups = &loaded.config.workspace_groups;

        assert_eq!(
            groups.new_workspace_placement,
            WorkspaceGroupNewPlacement::End
        );
        let cases = [
            ("/tmp/projects/app", WorkspaceGroupNewPlacement::Top),
            (
                "/tmp/projects/special/app",
                WorkspaceGroupNewPlacement::AfterCurrent,
            ),
            ("/tmp/worktrees/demo", WorkspaceGroupNewPlacement::Top),
            ("/tmp/elsewhere", WorkspaceGroupNewPlacement::End),
        ];
        for (cwd, expected) in cases {
            assert_eq!(groups.new_workspace_placement_for_cwd(Some(cwd)), expected);
        }
    }

    #[test]
    #[should_panic(
        expected = "workspaceGroups.newWorkspacePlacement must be afterCurrent, top, or end"
    )]
    fn load_from_path_rejects_invalid_workspace_group_placement() {
        let dir = TempDir::new().expect("temp dir");
        let path = settings_path_in(dir.path());
        fs::create_dir_all(path.parent().expect("config dir")).expect("create config dir");
        fs::write(
            &path,
            r#"{
  "workspaceGroups": {
    "newWorkspacePlacement": "middle"
  }
}
"#,
        )
        .expect("write config");

        let _ = load_from_path(&path);
    }

    #[test]
    #[should_panic(expected = "notifications.hooks[0].command is required")]
    fn load_from_path_rejects_malformed_notification_hooks() {
        let dir = TempDir::new().expect("temp dir");
        let path = settings_path_in(dir.path());
        fs::create_dir_all(path.parent().expect("config dir")).expect("create config dir");
        fs::write(
            &path,
            r#"{
  "notifications": {
    "hooks": [
      { "id": "missing-command" }
    ]
  }
}
"#,
        )
        .expect("write config");

        let _ = load_from_path(&path);
    }

    #[test]
    fn save_writes_gtk_and_ghostty_color_schemes() {
        let dir = TempDir::new().expect("temp dir");
        let path = settings_path_in(dir.path());
        fs::create_dir_all(path.parent().expect("config dir")).expect("create config dir");
        let _env_guard = EnvVarGuard::set("XDG_CONFIG_HOME", dir.path());

        let mut config = AppConfig::default();
        config.appearance.color_scheme = ColorScheme::Light;
        config.appearance.ghostty_color_scheme = ColorScheme::Dark;
        save(&config).expect("save config");

        let raw = fs::read_to_string(&path).expect("read config");
        let parsed: Value = serde_json::from_str(&raw).expect("parse config");
        assert_eq!(
            parsed["appearance"]["color_scheme"],
            Value::String("light".to_string())
        );
        assert_eq!(
            parsed["appearance"]["ghostty_color_scheme"],
            Value::String("dark".to_string())
        );
    }

    #[test]
    fn save_preserves_unrelated_top_level_keys() {
        let dir = TempDir::new().expect("temp dir");
        let path = settings_path_in(dir.path());
        fs::create_dir_all(path.parent().expect("config dir")).expect("create config dir");
        fs::write(
            &path,
            r#"{
  "custom": {
    "keep": true
  },
  "focus": {
    "hover_terminal_focus": false
  }
}
"#,
        )
        .expect("write config");

        let mut config = AppConfig::default();
        config.appearance.color_scheme = ColorScheme::Dark;
        save_to_path(&path, &config).expect("save config");

        let raw = fs::read_to_string(&path).expect("read config");
        let parsed: Value = serde_json::from_str(&raw).expect("parse config");
        assert_eq!(parsed["custom"]["keep"], Value::Bool(true));
        assert_eq!(
            parsed["appearance"]["color_scheme"],
            Value::String("dark".to_string())
        );
    }

    #[test]
    fn save_to_path_writes_app_workspace_placement_and_preserves_app_keys() {
        let dir = TempDir::new().expect("temp dir");
        let path = settings_path_in(dir.path());
        fs::create_dir_all(path.parent().expect("config dir")).expect("create config dir");
        fs::write(
            &path,
            r#"{
  "app": {
    "windowTitleTemplate": "{{workspace}}"
  }
}
"#,
        )
        .expect("write config");

        let mut config = load_from_path(&path).config;
        config.new_workspace_placement = WorkspaceGroupNewPlacement::End;
        save_to_path(&path, &config).expect("save app placement");

        let raw = fs::read_to_string(&path).expect("read config");
        let parsed: Value = serde_json::from_str(&raw).expect("parse config");
        assert_eq!(
            parsed["app"]["newWorkspacePlacement"],
            Value::String("end".to_string())
        );
        assert_eq!(
            parsed["app"]["windowTitleTemplate"],
            Value::String("{{workspace}}".to_string())
        );
    }

    #[test]
    fn save_to_path_writes_and_clears_font_size() {
        let dir = TempDir::new().expect("temp dir");
        let path = settings_path_in(dir.path());
        fs::create_dir_all(path.parent().expect("config dir")).expect("create config dir");

        let mut config = AppConfig {
            font_size: Some(16.25),
            ..AppConfig::default()
        };
        save_to_path(&path, &config).expect("save font size");

        let raw = fs::read_to_string(&path).expect("read config");
        let parsed: Value = serde_json::from_str(&raw).expect("parse config");
        assert_eq!(parsed["font_size"], json!(16.25));

        config.font_size = None;
        save_to_path(&path, &config).expect("clear font size");

        let raw = fs::read_to_string(&path).expect("read cleared config");
        let parsed: Value = serde_json::from_str(&raw).expect("parse cleared config");
        assert!(parsed.get("font_size").is_none());
    }

    #[test]
    fn save_to_path_writes_notification_preferences() {
        let dir = TempDir::new().expect("temp dir");
        let path = settings_path_in(dir.path());
        fs::create_dir_all(path.parent().expect("config dir")).expect("create config dir");

        let mut config = AppConfig::default();
        config.notifications.enabled = false;
        config.notifications.sound = NotificationSound::Alert;
        config.notifications.custom_sound_file_path = "/tmp/notify.wav".to_string();
        config.notifications.agent_permission_prompt = false;
        config.notifications.agent_turn_complete = AgentTurnCompleteMode::Never;
        config.notifications.agent_idle_reminder = false;
        save_to_path(&path, &config).expect("save notifications");

        let raw = fs::read_to_string(&path).expect("read config");
        let parsed: Value = serde_json::from_str(&raw).expect("parse config");
        assert_eq!(parsed["notifications"]["enabled"], Value::Bool(false));
        assert_eq!(
            parsed["notifications"]["sound"],
            Value::String("alert".to_string())
        );
        assert_eq!(
            parsed["notifications"]["customSoundFilePath"],
            Value::String("/tmp/notify.wav".to_string())
        );
        assert_eq!(
            parsed["notifications"]["agentPermissionPrompt"],
            Value::Bool(false)
        );
        assert_eq!(
            parsed["notifications"]["agentTurnComplete"],
            Value::String("never".to_string())
        );
        assert_eq!(
            parsed["notifications"]["agentIdleReminder"],
            Value::Bool(false)
        );
    }

    #[test]
    fn save_to_path_writes_workspace_group_default_and_preserves_by_cwd() {
        let dir = TempDir::new().expect("temp dir");
        let path = settings_path_in(dir.path());
        fs::create_dir_all(path.parent().expect("config dir")).expect("create config dir");
        fs::write(
            &path,
            r##"{
  "workspaceGroups": {
    "byCwd": {
      "/tmp/project": {
        "color": "#3366ff",
        "newWorkspacePlacement": "top"
      }
    }
  }
}
"##,
        )
        .expect("write config");

        let mut config = load_from_path(&path).config;
        config.workspace_groups.new_workspace_placement = WorkspaceGroupNewPlacement::End;
        save_to_path(&path, &config).expect("save workspace groups");

        let raw = fs::read_to_string(&path).expect("read config");
        let parsed: Value = serde_json::from_str(&raw).expect("parse config");
        assert_eq!(
            parsed["workspaceGroups"]["newWorkspacePlacement"],
            Value::String("end".to_string())
        );
        assert_eq!(
            parsed["workspaceGroups"]["byCwd"]["/tmp/project"]["color"],
            Value::String("#3366ff".to_string())
        );
        assert_eq!(
            parsed["workspaceGroups"]["byCwd"]["/tmp/project"]["newWorkspacePlacement"],
            Value::String("top".to_string())
        );
    }

    #[test]
    fn notification_sound_maps_supported_freedesktop_events() {
        assert_eq!(
            NotificationSound::Message.freedesktop_sound_name(),
            Some("message-new-instant")
        );
        assert_eq!(
            NotificationSound::Bell.freedesktop_sound_name(),
            Some("bell-terminal")
        );
        assert_eq!(
            NotificationSound::Complete.freedesktop_sound_name(),
            Some("complete")
        );
        assert_eq!(
            NotificationSound::Alert.freedesktop_sound_name(),
            Some("dialog-warning")
        );
        assert_eq!(
            NotificationSound::Ping.freedesktop_sound_name(),
            Some("Ping")
        );
        assert_eq!(NotificationSound::CustomFile.freedesktop_sound_name(), None);
        assert_eq!(NotificationSound::Default.freedesktop_sound_name(), None);
        assert_eq!(NotificationSound::None.freedesktop_sound_name(), None);
    }

    #[test]
    fn save_to_path_rejects_invalid_existing_json_without_rewriting() {
        let dir = TempDir::new().expect("temp dir");
        let path = settings_path_in(dir.path());
        fs::create_dir_all(path.parent().expect("config dir")).expect("create config dir");
        fs::write(&path, "not json").expect("write invalid config");

        let config = AppConfig::default();
        let error = save_to_path(&path, &config).expect_err("save should reject invalid config");

        assert!(error.contains("existing app config is invalid JSON"));
        assert_eq!(fs::read_to_string(&path).expect("read config"), "not json");
    }

    #[test]
    #[should_panic(expected = "failed to load app config")]
    fn load_from_path_rejects_invalid_json() {
        let dir = TempDir::new().expect("temp dir");
        let path = settings_path_in(dir.path());
        fs::create_dir_all(path.parent().expect("config dir")).expect("create config dir");
        fs::write(&path, "not json").expect("write config");

        let _ = load_from_path(&path);
    }
}
