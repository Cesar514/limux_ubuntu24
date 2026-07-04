// summary: Load, parse, and save Limux host application settings.
// purpose: Preserve user-facing appearance, focus, font, and notification preferences.
// inputs: XDG config paths and JSON settings files.
// returns/effects: Creates first-run defaults, rejects corrupt persisted settings, and writes settings atomically.

use std::collections::BTreeMap;
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
    pub account: AccountConfig,
    #[serde(skip)]
    pub integrations: IntegrationsConfig,
    #[serde(skip)]
    pub automation: AutomationConfig,
    #[serde(skip)]
    pub mobile: MobileConfig,
    #[serde(skip)]
    pub appearance: AppearanceConfig,
    #[serde(skip)]
    pub app: AppBehaviorConfig,
    #[serde(skip)]
    pub terminal: TerminalBehaviorConfig,
    #[serde(skip)]
    pub custom_sidebars: CustomSidebarsConfig,
    #[serde(skip)]
    pub beta_features: BetaFeaturesConfig,
    #[serde(skip)]
    pub markdown: MarkdownConfig,
    #[serde(skip)]
    pub file_editor: FileEditorConfig,
    #[serde(skip)]
    pub canvas: CanvasConfig,
    #[serde(skip)]
    pub pane_chrome: PaneChromeConfig,
    #[serde(skip)]
    pub workspace_colors: WorkspaceColorsConfig,
    #[serde(skip)]
    pub sidebar_appearance: SidebarAppearanceConfig,
    #[serde(skip)]
    pub sidebar: SidebarConfig,
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
pub enum PiiDisplayMode {
    #[default]
    Visible,
    Hidden,
}

impl PiiDisplayMode {
    fn as_str(self) -> &'static str {
        match self {
            Self::Visible => "visible",
            Self::Hidden => "hidden",
        }
    }

    fn from_str(raw: &str) -> Option<Self> {
        match raw {
            "visible" => Some(Self::Visible),
            "hidden" => Some(Self::Hidden),
            _ => None,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AccountConfig {
    pub pii_display_mode: PiiDisplayMode,
    pub selected_team_id: String,
    pub welcome_shown: bool,
}

impl Default for AccountConfig {
    fn default() -> Self {
        Self::cmux_default()
    }
}

impl AccountConfig {
    // purpose: Return CMUX account catalog defaults.
    // inputs: None.
    // returns/effects: Defaults local account-display settings without reading disk.
    fn cmux_default() -> Self {
        Self {
            pii_display_mode: PiiDisplayMode::Visible,
            selected_team_id: String::new(),
            welcome_shown: false,
        }
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum KiroNotificationLevel {
    Minimal,
    #[default]
    Standard,
    Verbose,
}

impl KiroNotificationLevel {
    fn as_str(self) -> &'static str {
        match self {
            Self::Minimal => "minimal",
            Self::Standard => "standard",
            Self::Verbose => "verbose",
        }
    }

    fn from_str(raw: &str) -> Option<Self> {
        match raw {
            "minimal" => Some(Self::Minimal),
            "standard" => Some(Self::Standard),
            "verbose" => Some(Self::Verbose),
            _ => None,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct IntegrationsConfig {
    pub claude_code_hooks_enabled: bool,
    pub claude_code_custom_claude_path: String,
    pub codex_hooks_enabled: bool,
    pub amp_hooks_enabled: bool,
    pub cursor_hooks_enabled: bool,
    pub gemini_hooks_enabled: bool,
    pub kiro_hooks_enabled: bool,
    pub kiro_notification_level: KiroNotificationLevel,
    pub ripgrep_custom_binary_path: String,
    pub suppress_subagent_notifications: bool,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum SocketControlMode {
    Off,
    #[default]
    CmuxOnly,
    Automation,
    Password,
    AllowAll,
}

impl SocketControlMode {
    fn as_str(self) -> &'static str {
        match self {
            Self::Off => "off",
            Self::CmuxOnly => "cmuxOnly",
            Self::Automation => "automation",
            Self::Password => "password",
            Self::AllowAll => "allowAll",
        }
    }

    fn from_str(raw: &str) -> Option<Self> {
        match raw {
            "off" => Some(Self::Off),
            "cmuxOnly" => Some(Self::CmuxOnly),
            "automation" => Some(Self::Automation),
            "password" => Some(Self::Password),
            "allowAll" => Some(Self::AllowAll),
            _ => None,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AutomationConfig {
    pub socket_control_mode: SocketControlMode,
    pub claude_code_integration: bool,
    pub claude_binary_path: String,
    pub workspace_auto_naming: bool,
    pub auto_naming_agent: String,
    pub ripgrep_binary_path: String,
    pub suppress_subagent_notifications: bool,
    pub amp_integration: bool,
    pub cursor_integration: bool,
    pub gemini_integration: bool,
    pub kiro_integration: bool,
    pub kiro_notification_level: KiroNotificationLevel,
    pub port_base: i32,
    pub port_range: i32,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MobileConfig {
    pub ios_pairing_host_enabled: bool,
    pub ios_pairing_host_port: i32,
    pub ios_pairing_host_display_name: String,
}

impl Default for IntegrationsConfig {
    fn default() -> Self {
        Self::cmux_default()
    }
}

impl IntegrationsConfig {
    // purpose: Return CMUX integration catalog defaults.
    // inputs: None.
    // returns/effects: Defaults integration settings without reading disk.
    fn cmux_default() -> Self {
        Self {
            claude_code_hooks_enabled: true,
            claude_code_custom_claude_path: String::new(),
            codex_hooks_enabled: true,
            amp_hooks_enabled: true,
            cursor_hooks_enabled: true,
            gemini_hooks_enabled: true,
            kiro_hooks_enabled: true,
            kiro_notification_level: KiroNotificationLevel::Standard,
            ripgrep_custom_binary_path: String::new(),
            suppress_subagent_notifications: true,
        }
    }
}

impl Default for AutomationConfig {
    fn default() -> Self {
        Self::cmux_default()
    }
}

impl AutomationConfig {
    fn cmux_default() -> Self {
        Self {
            socket_control_mode: SocketControlMode::CmuxOnly,
            claude_code_integration: true,
            claude_binary_path: String::new(),
            workspace_auto_naming: false,
            auto_naming_agent: "auto".to_string(),
            ripgrep_binary_path: String::new(),
            suppress_subagent_notifications: true,
            amp_integration: true,
            cursor_integration: true,
            gemini_integration: true,
            kiro_integration: true,
            kiro_notification_level: KiroNotificationLevel::Standard,
            port_base: 9100,
            port_range: 10,
        }
    }
}

impl Default for MobileConfig {
    fn default() -> Self {
        Self::cmux_default()
    }
}

impl MobileConfig {
    fn cmux_default() -> Self {
        Self {
            ios_pairing_host_enabled: false,
            ios_pairing_host_port: 58465,
            ios_pairing_host_display_name: String::new(),
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AppBehaviorConfig {
    pub keep_workspace_open_when_closing_last_surface: bool,
    pub workspace_inherit_working_directory: bool,
    pub focus_pane_on_first_click: bool,
    pub language: AppLanguage,
    pub app_icon: AppIconMode,
    pub window_title_template: String,
    pub menu_bar_only: bool,
    pub preferred_editor: String,
    pub open_supported_files_in_cmux: bool,
    pub open_markdown_in_cmux_viewer: bool,
    pub minimal_mode: WorkspacePresentationMode,
    pub global_font_magnification: i32,
    pub i_message_mode: bool,
    pub reorder_on_notification: bool,
    pub send_anonymous_telemetry: bool,
    pub confirm_quit: ConfirmQuitMode,
    pub warn_before_quit: bool,
    pub warn_before_closing_tab: bool,
    pub warn_before_closing_tab_x_button: bool,
    pub hide_tab_close_button: bool,
    pub rename_selects_existing_name: bool,
    pub command_palette_searches_all_surfaces: bool,
    pub file_drop_default_behavior: FileDropDefaultBehavior,
    pub titlebar_controls_style: i32,
    pub workspace_button_fade: WorkspaceButtonFadeMode,
    pub workspace_titlebar_visibility: bool,
    pub system_wide_hotkey_enabled: bool,
    pub dev_window_display: String,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum AppLanguage {
    #[default]
    System,
    En,
    Ar,
    Bs,
    ZhHans,
    ZhHant,
    Da,
    De,
    Es,
    Fr,
    It,
    Ja,
    Ko,
    Nb,
    Pl,
    PtBr,
    Ru,
    Th,
    Tr,
    Vi,
}

impl AppLanguage {
    fn as_str(self) -> &'static str {
        match self {
            Self::System => "system",
            Self::En => "en",
            Self::Ar => "ar",
            Self::Bs => "bs",
            Self::ZhHans => "zh-Hans",
            Self::ZhHant => "zh-Hant",
            Self::Da => "da",
            Self::De => "de",
            Self::Es => "es",
            Self::Fr => "fr",
            Self::It => "it",
            Self::Ja => "ja",
            Self::Ko => "ko",
            Self::Nb => "nb",
            Self::Pl => "pl",
            Self::PtBr => "pt-BR",
            Self::Ru => "ru",
            Self::Th => "th",
            Self::Tr => "tr",
            Self::Vi => "vi",
        }
    }

    fn from_str(raw: &str) -> Option<Self> {
        match raw {
            "system" => Some(Self::System),
            "en" => Some(Self::En),
            "ar" => Some(Self::Ar),
            "bs" => Some(Self::Bs),
            "zh-Hans" => Some(Self::ZhHans),
            "zh-Hant" => Some(Self::ZhHant),
            "da" => Some(Self::Da),
            "de" => Some(Self::De),
            "es" => Some(Self::Es),
            "fr" => Some(Self::Fr),
            "it" => Some(Self::It),
            "ja" => Some(Self::Ja),
            "ko" => Some(Self::Ko),
            "nb" => Some(Self::Nb),
            "pl" => Some(Self::Pl),
            "pt-BR" => Some(Self::PtBr),
            "ru" => Some(Self::Ru),
            "th" => Some(Self::Th),
            "tr" => Some(Self::Tr),
            "vi" => Some(Self::Vi),
            _ => None,
        }
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum AppIconMode {
    #[default]
    Automatic,
    Light,
    Dark,
}

impl AppIconMode {
    fn as_str(self) -> &'static str {
        match self {
            Self::Automatic => "automatic",
            Self::Light => "light",
            Self::Dark => "dark",
        }
    }

    fn from_str(raw: &str) -> Option<Self> {
        match raw {
            "automatic" => Some(Self::Automatic),
            "light" => Some(Self::Light),
            "dark" => Some(Self::Dark),
            _ => None,
        }
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum WorkspacePresentationMode {
    #[default]
    Standard,
    Minimal,
}

impl WorkspacePresentationMode {
    fn as_str(self) -> &'static str {
        match self {
            Self::Standard => "standard",
            Self::Minimal => "minimal",
        }
    }

    fn from_str(raw: &str) -> Option<Self> {
        match raw {
            "standard" => Some(Self::Standard),
            "minimal" => Some(Self::Minimal),
            _ => None,
        }
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum ConfirmQuitMode {
    #[default]
    Always,
    DirtyOnly,
    Never,
}

impl ConfirmQuitMode {
    fn as_str(self) -> &'static str {
        match self {
            Self::Always => "always",
            Self::DirtyOnly => "dirty-only",
            Self::Never => "never",
        }
    }

    fn from_str(raw: &str) -> Option<Self> {
        match raw {
            "always" => Some(Self::Always),
            "dirty-only" => Some(Self::DirtyOnly),
            "never" => Some(Self::Never),
            _ => None,
        }
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum FileDropDefaultBehavior {
    #[default]
    Text,
    Preview,
}

impl FileDropDefaultBehavior {
    fn as_str(self) -> &'static str {
        match self {
            Self::Text => "text",
            Self::Preview => "preview",
        }
    }

    fn from_str(raw: &str) -> Option<Self> {
        match raw {
            "text" => Some(Self::Text),
            "preview" => Some(Self::Preview),
            _ => None,
        }
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum WorkspaceButtonFadeMode {
    Enabled,
    #[default]
    Disabled,
}

impl WorkspaceButtonFadeMode {
    fn as_str(self) -> &'static str {
        match self {
            Self::Enabled => "enabled",
            Self::Disabled => "disabled",
        }
    }

    fn from_str(raw: &str) -> Option<Self> {
        match raw {
            "enabled" => Some(Self::Enabled),
            "disabled" => Some(Self::Disabled),
            _ => None,
        }
    }
}

impl Default for AppBehaviorConfig {
    fn default() -> Self {
        Self::cmux_default()
    }
}

impl AppBehaviorConfig {
    // purpose: Return CMUX app behavior defaults that differ from Rust primitive defaults.
    // inputs: None.
    // returns/effects: Defaults app behavior from CMUX runtime settings.
    fn cmux_default() -> Self {
        Self {
            keep_workspace_open_when_closing_last_surface: false,
            workspace_inherit_working_directory: true,
            focus_pane_on_first_click: false,
            language: AppLanguage::System,
            app_icon: AppIconMode::Automatic,
            window_title_template: String::new(),
            menu_bar_only: false,
            preferred_editor: String::new(),
            open_supported_files_in_cmux: true,
            open_markdown_in_cmux_viewer: true,
            minimal_mode: WorkspacePresentationMode::Standard,
            global_font_magnification: 100,
            i_message_mode: false,
            reorder_on_notification: true,
            send_anonymous_telemetry: true,
            confirm_quit: ConfirmQuitMode::Always,
            warn_before_quit: true,
            warn_before_closing_tab: true,
            warn_before_closing_tab_x_button: false,
            hide_tab_close_button: false,
            rename_selects_existing_name: true,
            command_palette_searches_all_surfaces: false,
            file_drop_default_behavior: FileDropDefaultBehavior::Text,
            titlebar_controls_style: 0,
            workspace_button_fade: WorkspaceButtonFadeMode::Disabled,
            workspace_titlebar_visibility: true,
            system_wide_hotkey_enabled: false,
            dev_window_display: String::new(),
        }
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct TerminalBehaviorConfig {
    pub show_scroll_bar: bool,
    pub copy_on_select: bool,
    pub auto_resume_agent_sessions: bool,
    pub agent_hibernation: TerminalAgentHibernationConfig,
    pub renderer_realization: TerminalRendererRealizationConfig,
    pub title_updates: TerminalTitleUpdatesConfig,
    pub show_text_box_on_new_terminals: bool,
    pub focus_text_box_on_new_terminals: bool,
    pub text_box_max_lines: i32,
    pub text_box_default_submit_action: String,
    pub text_box_submit_actions: String,
    pub resume_commands: Vec<String>,
    pub scroll_speed: f64,
    pub runaway_memory_guardrail: TerminalRunawayMemoryGuardrailConfig,
}

#[derive(Clone, Debug, PartialEq)]
pub struct TerminalAgentHibernationConfig {
    pub enabled: bool,
    pub idle_seconds: f64,
    pub max_live_terminals: i32,
}

#[derive(Clone, Debug, PartialEq)]
pub struct TerminalRendererRealizationConfig {
    pub enabled: bool,
    pub idle_seconds: f64,
    pub max_warm_renderers: i32,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TerminalTitleUpdatesConfig {
    pub coalescing_enabled: bool,
    pub coalescing_delay_milliseconds: i32,
    pub diagnostics: bool,
}

#[derive(Clone, Debug, PartialEq)]
pub struct TerminalRunawayMemoryGuardrailConfig {
    pub enabled: bool,
    pub threshold_gb: f64,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MarkdownConfig {
    pub font_size: i32,
    pub font_family: String,
    pub max_width: i32,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FileEditorConfig {
    pub word_wrap: bool,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CanvasConfig {
    pub pane_gap: i32,
    pub snapping_enabled: bool,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct PaneChromeConfig {
    pub pane_border_color: String,
    pub active_pane_border_color: String,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct WorkspaceColorsConfig {
    pub indicator_style: WorkspaceIndicatorStyle,
    pub selection_color: String,
    pub notification_badge_color: String,
    pub colors: BTreeMap<String, String>,
    pub palette_overrides: BTreeMap<String, String>,
    pub custom_colors: Vec<String>,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum WorkspaceIndicatorStyle {
    #[default]
    LeftRail,
    SolidFill,
}

impl WorkspaceIndicatorStyle {
    // purpose: Serialize the CMUX workspace indicator style.
    // inputs: Parsed or default workspace indicator style.
    // returns/effects: Returns the canonical JSON string.
    fn as_str(self) -> &'static str {
        match self {
            Self::LeftRail => "leftRail",
            Self::SolidFill => "solidFill",
        }
    }

    // purpose: Parse CMUX workspace indicator strings including legacy aliases.
    // inputs: Raw JSON string from workspaceColors.indicatorStyle.
    // returns/effects: Returns None for unsupported indicator names.
    fn from_str(raw: &str) -> Option<Self> {
        match raw {
            "leftRail" | "rail" => Some(Self::LeftRail),
            "solidFill" | "border" | "wash" | "lift" | "typography" | "washRail"
            | "blueWashColorRail" => Some(Self::SolidFill),
            _ => None,
        }
    }
}

impl Default for WorkspaceColorsConfig {
    fn default() -> Self {
        Self {
            indicator_style: WorkspaceIndicatorStyle::LeftRail,
            selection_color: String::new(),
            notification_badge_color: String::new(),
            colors: BTreeMap::new(),
            palette_overrides: BTreeMap::new(),
            custom_colors: Vec::new(),
        }
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct SidebarAppearanceConfig {
    pub match_terminal_background: bool,
    pub tint_color: String,
    pub light_mode_tint_color: String,
    pub dark_mode_tint_color: String,
    pub tint_opacity: f64,
    pub blur_opacity: f64,
    pub corner_radius: f64,
    pub preset: SidebarPresetOption,
    pub material: SidebarMaterialOption,
    pub blend_mode: SidebarBlendModeOption,
    pub state: SidebarStateOption,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum SidebarPresetOption {
    #[default]
    NativeSidebar,
    NativeTitlebar,
    Translucent,
    OpaqueDark,
    OpaqueLight,
    Custom,
}

impl SidebarPresetOption {
    // purpose: Serialize CMUX sidebar appearance presets.
    // inputs: Parsed or default preset.
    // returns/effects: Returns the canonical JSON string.
    fn as_str(self) -> &'static str {
        match self {
            Self::NativeSidebar => "nativeSidebar",
            Self::NativeTitlebar => "nativeTitlebar",
            Self::Translucent => "translucent",
            Self::OpaqueDark => "opaqueDark",
            Self::OpaqueLight => "opaqueLight",
            Self::Custom => "custom",
        }
    }

    // purpose: Parse CMUX sidebar appearance preset strings.
    // inputs: Raw JSON string from sidebarAppearance.preset.
    // returns/effects: Returns None for unsupported preset names.
    fn from_str(raw: &str) -> Option<Self> {
        match raw {
            "nativeSidebar" => Some(Self::NativeSidebar),
            "nativeTitlebar" => Some(Self::NativeTitlebar),
            "translucent" => Some(Self::Translucent),
            "opaqueDark" => Some(Self::OpaqueDark),
            "opaqueLight" => Some(Self::OpaqueLight),
            "custom" => Some(Self::Custom),
            _ => None,
        }
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum SidebarMaterialOption {
    #[default]
    Sidebar,
    Titlebar,
    Selection,
    Menu,
    Popover,
    HeaderView,
    Sheet,
    WindowBackground,
    HudWindow,
    FullScreenUi,
    ToolTip,
    ContentBackground,
    UnderWindowBackground,
    UnderPageBackground,
}

impl SidebarMaterialOption {
    // purpose: Serialize CMUX sidebar material options.
    // inputs: Parsed or default material.
    // returns/effects: Returns the canonical JSON string.
    fn as_str(self) -> &'static str {
        match self {
            Self::Sidebar => "sidebar",
            Self::Titlebar => "titlebar",
            Self::Selection => "selection",
            Self::Menu => "menu",
            Self::Popover => "popover",
            Self::HeaderView => "headerView",
            Self::Sheet => "sheet",
            Self::WindowBackground => "windowBackground",
            Self::HudWindow => "hudWindow",
            Self::FullScreenUi => "fullScreenUI",
            Self::ToolTip => "toolTip",
            Self::ContentBackground => "contentBackground",
            Self::UnderWindowBackground => "underWindowBackground",
            Self::UnderPageBackground => "underPageBackground",
        }
    }

    // purpose: Parse CMUX sidebar material option strings.
    // inputs: Raw JSON string from sidebarAppearance.material.
    // returns/effects: Returns None for unsupported material names.
    fn from_str(raw: &str) -> Option<Self> {
        match raw {
            "sidebar" => Some(Self::Sidebar),
            "titlebar" => Some(Self::Titlebar),
            "selection" => Some(Self::Selection),
            "menu" => Some(Self::Menu),
            "popover" => Some(Self::Popover),
            "headerView" => Some(Self::HeaderView),
            "sheet" => Some(Self::Sheet),
            "windowBackground" => Some(Self::WindowBackground),
            "hudWindow" => Some(Self::HudWindow),
            "fullScreenUI" => Some(Self::FullScreenUi),
            "toolTip" => Some(Self::ToolTip),
            "contentBackground" => Some(Self::ContentBackground),
            "underWindowBackground" => Some(Self::UnderWindowBackground),
            "underPageBackground" => Some(Self::UnderPageBackground),
            _ => None,
        }
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum SidebarBlendModeOption {
    BehindWindow,
    #[default]
    WithinWindow,
}

impl SidebarBlendModeOption {
    fn as_str(self) -> &'static str {
        match self {
            Self::BehindWindow => "behindWindow",
            Self::WithinWindow => "withinWindow",
        }
    }

    fn from_str(raw: &str) -> Option<Self> {
        match raw {
            "behindWindow" => Some(Self::BehindWindow),
            "withinWindow" => Some(Self::WithinWindow),
            _ => None,
        }
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum SidebarStateOption {
    Active,
    Inactive,
    #[default]
    FollowsWindowActiveState,
}

impl SidebarStateOption {
    fn as_str(self) -> &'static str {
        match self {
            Self::Active => "active",
            Self::Inactive => "inactive",
            Self::FollowsWindowActiveState => "followsWindowActiveState",
        }
    }

    fn from_str(raw: &str) -> Option<Self> {
        match raw {
            "active" => Some(Self::Active),
            "inactive" => Some(Self::Inactive),
            "followsWindowActiveState" => Some(Self::FollowsWindowActiveState),
            _ => None,
        }
    }
}

impl Default for SidebarAppearanceConfig {
    fn default() -> Self {
        Self {
            match_terminal_background: false,
            tint_color: "#000000".to_string(),
            light_mode_tint_color: String::new(),
            dark_mode_tint_color: String::new(),
            tint_opacity: 0.18,
            blur_opacity: 1.0,
            corner_radius: 0.0,
            preset: SidebarPresetOption::NativeSidebar,
            material: SidebarMaterialOption::Sidebar,
            blend_mode: SidebarBlendModeOption::WithinWindow,
            state: SidebarStateOption::FollowsWindowActiveState,
        }
    }
}

impl Default for MarkdownConfig {
    fn default() -> Self {
        Self::cmux_default()
    }
}

impl MarkdownConfig {
    // purpose: Return CMUX markdown viewer defaults from the upstream settings catalog.
    // inputs: None.
    // returns/effects: Defaults newly opened markdown viewers without reading disk.
    fn cmux_default() -> Self {
        Self {
            font_size: 15,
            font_family: String::new(),
            max_width: 980,
        }
    }
}

impl Default for FileEditorConfig {
    fn default() -> Self {
        Self::cmux_default()
    }
}

impl FileEditorConfig {
    // purpose: Return CMUX file-editor defaults from the upstream settings catalog.
    // inputs: None.
    // returns/effects: Defaults text-editor behavior without reading disk.
    fn cmux_default() -> Self {
        Self { word_wrap: false }
    }
}

impl Default for CanvasConfig {
    fn default() -> Self {
        Self::cmux_default()
    }
}

impl CanvasConfig {
    // purpose: Return CMUX canvas defaults from the upstream settings catalog.
    // inputs: None.
    // returns/effects: Defaults canvas spacing/snapping settings without reading disk.
    fn cmux_default() -> Self {
        Self {
            pane_gap: 16,
            snapping_enabled: true,
        }
    }
}

impl Default for TerminalBehaviorConfig {
    fn default() -> Self {
        Self::cmux_default()
    }
}

impl TerminalBehaviorConfig {
    // purpose: Return CMUX terminal behavior defaults that differ from Rust primitive defaults.
    // inputs: None.
    // returns/effects: Defaults terminal behavior from CMUX runtime settings.
    fn cmux_default() -> Self {
        Self {
            show_scroll_bar: true,
            copy_on_select: false,
            auto_resume_agent_sessions: true,
            agent_hibernation: TerminalAgentHibernationConfig {
                enabled: false,
                idle_seconds: 5.0,
                max_live_terminals: 12,
            },
            renderer_realization: TerminalRendererRealizationConfig {
                enabled: true,
                idle_seconds: 30.0,
                max_warm_renderers: 12,
            },
            title_updates: TerminalTitleUpdatesConfig {
                coalescing_enabled: false,
                coalescing_delay_milliseconds: 500,
                diagnostics: false,
            },
            show_text_box_on_new_terminals: false,
            focus_text_box_on_new_terminals: false,
            text_box_max_lines: 10,
            text_box_default_submit_action: "text-entry".to_string(),
            text_box_submit_actions: String::new(),
            resume_commands: Vec::new(),
            scroll_speed: 1.0,
            runaway_memory_guardrail: TerminalRunawayMemoryGuardrailConfig {
                enabled: true,
                threshold_gb: 8.0,
            },
        }
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum CustomSidebarRendererMode {
    #[default]
    InProcess,
    Remote,
}

impl CustomSidebarRendererMode {
    fn as_str(self) -> &'static str {
        match self {
            Self::InProcess => "inProcess",
            Self::Remote => "remote",
        }
    }

    fn from_str(s: &str) -> Option<Self> {
        match s {
            "inProcess" => Some(Self::InProcess),
            "remote" => Some(Self::Remote),
            _ => None,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CustomSidebarsConfig {
    pub renderer: CustomSidebarRendererMode,
    pub beta_enabled: bool,
}

impl Default for CustomSidebarsConfig {
    fn default() -> Self {
        Self {
            renderer: CustomSidebarRendererMode::InProcess,
            beta_enabled: true,
        }
    }
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct BetaFeaturesConfig {
    pub right_sidebar_feed_enabled: bool,
    pub right_sidebar_dock_enabled: bool,
    pub extensions_enabled: bool,
    pub remote_tmux_enabled: bool,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SidebarConfig {
    pub hide_all_details: bool,
    pub wrap_workspace_titles: bool,
    pub show_workspace_description: bool,
    pub show_notification_message: bool,
    pub show_branch_directory: bool,
    pub branch_layout: SidebarBranchLayout,
    pub show_pull_requests: bool,
    pub watch_git_status: bool,
    pub show_ports: bool,
    pub make_pull_requests_clickable: bool,
    pub open_pull_request_links_in_cmux_browser: bool,
    pub open_port_links_in_cmux_browser: bool,
    pub show_ssh: bool,
    pub show_custom_metadata: bool,
    pub show_progress: bool,
    pub show_log: bool,
    pub right_max_width: Option<i32>,
}

impl Default for SidebarConfig {
    fn default() -> Self {
        Self {
            hide_all_details: false,
            wrap_workspace_titles: false,
            show_workspace_description: true,
            show_notification_message: true,
            show_branch_directory: true,
            branch_layout: SidebarBranchLayout::default(),
            show_pull_requests: true,
            watch_git_status: true,
            show_ports: true,
            make_pull_requests_clickable: true,
            open_pull_request_links_in_cmux_browser: true,
            open_port_links_in_cmux_browser: true,
            show_ssh: true,
            show_custom_metadata: true,
            show_progress: true,
            show_log: true,
            right_max_width: None,
        }
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum SidebarBranchLayout {
    #[default]
    Vertical,
    Inline,
}

impl SidebarBranchLayout {
    // purpose: Serialize the sidebar branch layout using CMUX's config spelling.
    // inputs: Branch layout selected from parsed settings or defaults.
    // returns/effects: Returns a stable config/API string without allocation.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Vertical => "vertical",
            Self::Inline => "inline",
        }
    }

    // purpose: Parse CMUX sidebar branch layout strings.
    // inputs: Raw string from settings.
    // returns/effects: Returns None for unsupported layout names.
    fn from_str(s: &str) -> Option<Self> {
        match s {
            "vertical" => Some(Self::Vertical),
            "inline" => Some(Self::Inline),
            _ => None,
        }
    }
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
    pub dock_badge: bool,
    pub show_in_menu_bar: bool,
    pub unread_pane_ring: bool,
    pub pane_flash: bool,
    pub sound: NotificationSound,
    pub custom_sound_file_path: String,
    pub command: String,
    pub hooks_mode: NotificationHooksMode,
    pub hooks: Vec<NotificationHookConfig>,
    pub agent_permission_prompt: bool,
    pub agent_turn_complete: AgentTurnCompleteMode,
    pub agent_idle_reminder: bool,
    pub suppress_only_focused_surface: bool,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct NotificationHookConfig {
    pub id: String,
    pub command: String,
    pub enabled: bool,
    pub timeout_seconds: u64,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum NotificationHooksMode {
    #[default]
    Append,
    Replace,
}

impl NotificationHooksMode {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Append => "append",
            Self::Replace => "replace",
        }
    }

    fn from_str(s: &str) -> Option<Self> {
        match s {
            "append" => Some(Self::Append),
            "replace" => Some(Self::Replace),
            _ => None,
        }
    }
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
            dock_badge: true,
            show_in_menu_bar: true,
            unread_pane_ring: true,
            pane_flash: true,
            sound: NotificationSound::Default,
            custom_sound_file_path: String::new(),
            command: String::new(),
            hooks_mode: NotificationHooksMode::Append,
            hooks: Vec::new(),
            agent_permission_prompt: true,
            agent_turn_complete: AgentTurnCompleteMode::WhenIdle,
            agent_idle_reminder: true,
            suppress_only_focused_surface: false,
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
    let account = root
        .get("account")
        .map(parse_account_config)
        .unwrap_or_default();
    let integrations = root
        .get("integrations")
        .map(parse_integrations_config)
        .unwrap_or_default();
    let automation = root
        .get("automation")
        .map(parse_automation_config)
        .unwrap_or_default();
    let mobile = root
        .get("mobile")
        .map(parse_mobile_config)
        .unwrap_or_default();

    let app = root.get("app").map(|value| {
        value
            .as_object()
            .unwrap_or_else(|| panic!("app must be an object"))
    });
    let appearance = root.get("appearance").and_then(Value::as_object);

    let legacy_color_scheme = appearance
        .and_then(|appearance| appearance.get("color_scheme"))
        .map(|value| parse_color_scheme_setting(value, "appearance.color_scheme"));
    let color_scheme = app
        .and_then(|app| app.get("appearance"))
        .map(|value| parse_color_scheme_setting(value, "app.appearance"))
        .or(legacy_color_scheme)
        .unwrap_or_default();

    let ghostty_color_scheme = appearance
        .and_then(|appearance| appearance.get("ghostty_color_scheme"))
        .map(|value| parse_color_scheme_setting(value, "appearance.ghostty_color_scheme"))
        .unwrap_or(color_scheme);
    let new_workspace_placement = app
        .and_then(|app| app.get("newWorkspacePlacement"))
        .map(|value| parse_workspace_new_placement(value, "app.newWorkspacePlacement"))
        .unwrap_or_default();
    let app_defaults = AppBehaviorConfig::cmux_default();
    let workspace_inherit_working_directory = app
        .and_then(|app| app.get("workspaceInheritWorkingDirectory"))
        .map(|value| parse_bool_setting(value, "app.workspaceInheritWorkingDirectory"))
        .unwrap_or(app_defaults.workspace_inherit_working_directory);
    let focus_pane_on_first_click = app
        .and_then(|app| app.get("focusPaneOnFirstClick"))
        .map(|value| parse_bool_setting(value, "app.focusPaneOnFirstClick"))
        .unwrap_or(app_defaults.focus_pane_on_first_click);
    let keep_workspace_open_when_closing_last_surface = app
        .and_then(|app| app.get("keepWorkspaceOpenWhenClosingLastSurface"))
        .map(|value| parse_bool_setting(value, "app.keepWorkspaceOpenWhenClosingLastSurface"))
        .unwrap_or(app_defaults.keep_workspace_open_when_closing_last_surface);
    let language = app
        .and_then(|app| app.get("language"))
        .map(|value| parse_app_language(value, "app.language"))
        .unwrap_or(app_defaults.language);
    let app_icon = app
        .and_then(|app| app.get("appIcon"))
        .map(|value| parse_app_icon(value, "app.appIcon"))
        .unwrap_or(app_defaults.app_icon);
    let window_title_template = app
        .and_then(|app| app.get("windowTitleTemplate"))
        .map(|value| parse_string_setting(value, "app.windowTitleTemplate"))
        .unwrap_or(app_defaults.window_title_template);
    let menu_bar_only = app
        .and_then(|app| app.get("menuBarOnly"))
        .map(|value| parse_bool_setting(value, "app.menuBarOnly"))
        .unwrap_or(app_defaults.menu_bar_only);
    let preferred_editor = app
        .and_then(|app| app.get("preferredEditor"))
        .map(|value| parse_string_setting(value, "app.preferredEditor"))
        .unwrap_or(app_defaults.preferred_editor);
    let open_supported_files_in_cmux = app
        .and_then(|app| app.get("openSupportedFilesInCmux"))
        .map(|value| parse_bool_setting(value, "app.openSupportedFilesInCmux"))
        .unwrap_or(app_defaults.open_supported_files_in_cmux);
    let open_markdown_in_cmux_viewer = app
        .and_then(|app| app.get("openMarkdownInCmuxViewer"))
        .map(|value| parse_bool_setting(value, "app.openMarkdownInCmuxViewer"))
        .unwrap_or(app_defaults.open_markdown_in_cmux_viewer);
    let minimal_mode = app
        .and_then(|app| app.get("minimalMode"))
        .map(|value| parse_workspace_presentation_mode(value, "app.minimalMode"))
        .unwrap_or(app_defaults.minimal_mode);
    let global_font_magnification = app
        .and_then(|app| app.get("globalFontMagnification"))
        .map(|value| parse_global_font_magnification(value, "app.globalFontMagnification"))
        .unwrap_or(app_defaults.global_font_magnification);
    let i_message_mode = app
        .and_then(|app| app.get("iMessageMode"))
        .map(|value| parse_bool_setting(value, "app.iMessageMode"))
        .unwrap_or(app_defaults.i_message_mode);
    let reorder_on_notification = app
        .and_then(|app| app.get("reorderOnNotification"))
        .map(|value| parse_bool_setting(value, "app.reorderOnNotification"))
        .unwrap_or(app_defaults.reorder_on_notification);
    let send_anonymous_telemetry = app
        .and_then(|app| app.get("sendAnonymousTelemetry"))
        .map(|value| parse_bool_setting(value, "app.sendAnonymousTelemetry"))
        .unwrap_or(app_defaults.send_anonymous_telemetry);
    let confirm_quit = app
        .and_then(|app| app.get("confirmQuit"))
        .map(|value| parse_confirm_quit(value, "app.confirmQuit"))
        .unwrap_or(app_defaults.confirm_quit);
    let warn_before_quit = app
        .and_then(|app| app.get("warnBeforeQuit"))
        .map(|value| parse_bool_setting(value, "app.warnBeforeQuit"))
        .unwrap_or(app_defaults.warn_before_quit);
    let warn_before_closing_tab = app
        .and_then(|app| app.get("warnBeforeClosingTab"))
        .map(|value| parse_bool_setting(value, "app.warnBeforeClosingTab"))
        .unwrap_or(app_defaults.warn_before_closing_tab);
    let warn_before_closing_tab_x_button = app
        .and_then(|app| app.get("warnBeforeClosingTabXButton"))
        .map(|value| parse_bool_setting(value, "app.warnBeforeClosingTabXButton"))
        .unwrap_or(app_defaults.warn_before_closing_tab_x_button);
    let hide_tab_close_button = app
        .and_then(|app| app.get("hideTabCloseButton"))
        .map(|value| parse_bool_setting(value, "app.hideTabCloseButton"))
        .unwrap_or(app_defaults.hide_tab_close_button);
    let rename_selects_existing_name = app
        .and_then(|app| app.get("renameSelectsExistingName"))
        .map(|value| parse_bool_setting(value, "app.renameSelectsExistingName"))
        .unwrap_or(app_defaults.rename_selects_existing_name);
    let command_palette_searches_all_surfaces = app
        .and_then(|app| app.get("commandPaletteSearchesAllSurfaces"))
        .map(|value| parse_bool_setting(value, "app.commandPaletteSearchesAllSurfaces"))
        .unwrap_or(app_defaults.command_palette_searches_all_surfaces);
    let file_drop_default_behavior = app
        .and_then(|app| app.get("fileDropDefaultBehavior"))
        .map(|value| parse_file_drop_default_behavior(value, "app.fileDropDefaultBehavior"))
        .unwrap_or(app_defaults.file_drop_default_behavior);
    let titlebar_controls_style = app
        .and_then(|app| app.get("titlebarControlsStyle"))
        .map(|value| parse_titlebar_controls_style(value, "app.titlebarControlsStyle"))
        .unwrap_or(app_defaults.titlebar_controls_style);
    let workspace_button_fade = app
        .and_then(|app| app.get("workspaceButtonFade"))
        .map(|value| parse_workspace_button_fade(value, "app.workspaceButtonFade"))
        .unwrap_or(app_defaults.workspace_button_fade);
    let workspace_titlebar_visibility = app
        .and_then(|app| app.get("workspaceTitlebarVisibility"))
        .map(|value| parse_bool_setting(value, "app.workspaceTitlebarVisibility"))
        .unwrap_or(app_defaults.workspace_titlebar_visibility);
    let system_wide_hotkey_enabled = app
        .and_then(|app| app.get("systemWideHotkeyEnabled"))
        .map(|value| parse_bool_setting(value, "app.systemWideHotkeyEnabled"))
        .unwrap_or(app_defaults.system_wide_hotkey_enabled);
    let dev_window_display = app
        .and_then(|app| app.get("devWindowDisplay"))
        .map(|value| parse_string_setting(value, "app.devWindowDisplay"))
        .unwrap_or(app_defaults.dev_window_display);
    let terminal = root.get("terminal").map(|value| {
        value
            .as_object()
            .unwrap_or_else(|| panic!("terminal must be an object"))
    });
    let terminal_config = parse_terminal_behavior_config(terminal);
    let custom_sidebars = root
        .get("customSidebars")
        .map(parse_custom_sidebars_config)
        .unwrap_or_default();
    let beta_features = parse_beta_features_config(root);
    let sidebar = root.get("sidebar").map(|value| {
        value
            .as_object()
            .unwrap_or_else(|| panic!("sidebar must be an object"))
    });
    let sidebar_defaults = SidebarConfig::default();
    let hide_all_details = sidebar
        .and_then(|sidebar| sidebar.get("hideAllDetails"))
        .map(|value| parse_bool_setting(value, "sidebar.hideAllDetails"))
        .unwrap_or(sidebar_defaults.hide_all_details);
    let wrap_workspace_titles = sidebar
        .and_then(|sidebar| sidebar.get("wrapWorkspaceTitles"))
        .map(|value| parse_bool_setting(value, "sidebar.wrapWorkspaceTitles"))
        .unwrap_or(sidebar_defaults.wrap_workspace_titles);
    let show_workspace_description = sidebar
        .and_then(|sidebar| sidebar.get("showWorkspaceDescription"))
        .map(|value| parse_bool_setting(value, "sidebar.showWorkspaceDescription"))
        .unwrap_or(sidebar_defaults.show_workspace_description);
    let show_notification_message = sidebar
        .and_then(|sidebar| sidebar.get("showNotificationMessage"))
        .map(|value| parse_bool_setting(value, "sidebar.showNotificationMessage"))
        .unwrap_or(sidebar_defaults.show_notification_message);
    let show_branch_directory = sidebar
        .and_then(|sidebar| sidebar.get("showBranchDirectory"))
        .map(|value| parse_bool_setting(value, "sidebar.showBranchDirectory"))
        .unwrap_or(sidebar_defaults.show_branch_directory);
    let branch_layout = sidebar
        .and_then(|sidebar| sidebar.get("branchLayout"))
        .map(|value| parse_sidebar_branch_layout(value, "sidebar.branchLayout"))
        .unwrap_or(sidebar_defaults.branch_layout);
    let show_pull_requests = sidebar
        .and_then(|sidebar| sidebar.get("showPullRequests"))
        .map(|value| parse_bool_setting(value, "sidebar.showPullRequests"))
        .unwrap_or(sidebar_defaults.show_pull_requests);
    let watch_git_status = sidebar
        .and_then(|sidebar| sidebar.get("watchGitStatus"))
        .map(|value| parse_bool_setting(value, "sidebar.watchGitStatus"))
        .unwrap_or(sidebar_defaults.watch_git_status);
    let show_ports = sidebar
        .and_then(|sidebar| sidebar.get("showPorts"))
        .map(|value| parse_bool_setting(value, "sidebar.showPorts"))
        .unwrap_or(sidebar_defaults.show_ports);
    let make_pull_requests_clickable = sidebar
        .and_then(|sidebar| sidebar.get("makePullRequestsClickable"))
        .map(|value| parse_bool_setting(value, "sidebar.makePullRequestsClickable"))
        .unwrap_or(sidebar_defaults.make_pull_requests_clickable);
    let open_pull_request_links_in_cmux_browser = sidebar
        .and_then(|sidebar| sidebar.get("openPullRequestLinksInCmuxBrowser"))
        .map(|value| parse_bool_setting(value, "sidebar.openPullRequestLinksInCmuxBrowser"))
        .unwrap_or(sidebar_defaults.open_pull_request_links_in_cmux_browser);
    let open_port_links_in_cmux_browser = sidebar
        .and_then(|sidebar| sidebar.get("openPortLinksInCmuxBrowser"))
        .map(|value| parse_bool_setting(value, "sidebar.openPortLinksInCmuxBrowser"))
        .unwrap_or(sidebar_defaults.open_port_links_in_cmux_browser);
    let show_ssh = sidebar
        .and_then(|sidebar| sidebar.get("showSSH"))
        .map(|value| parse_bool_setting(value, "sidebar.showSSH"))
        .unwrap_or(sidebar_defaults.show_ssh);
    let show_custom_metadata = sidebar
        .and_then(|sidebar| sidebar.get("showCustomMetadata"))
        .map(|value| parse_bool_setting(value, "sidebar.showCustomMetadata"))
        .unwrap_or(sidebar_defaults.show_custom_metadata);
    let show_progress = sidebar
        .and_then(|sidebar| sidebar.get("showProgress"))
        .map(|value| parse_bool_setting(value, "sidebar.showProgress"))
        .unwrap_or(sidebar_defaults.show_progress);
    let show_log = sidebar
        .and_then(|sidebar| sidebar.get("showLog"))
        .map(|value| parse_bool_setting(value, "sidebar.showLog"))
        .unwrap_or(sidebar_defaults.show_log);
    let right_max_width = sidebar
        .and_then(|sidebar| sidebar.get("rightMaxWidth"))
        .map(|value| parse_sidebar_right_max_width(value, "sidebar.rightMaxWidth"));

    let notifications = root.get("notifications").and_then(Value::as_object);
    let notification_defaults = NotificationConfig::default();
    let notifications_enabled = notifications
        .and_then(|notifications| notifications.get("enabled"))
        .and_then(Value::as_bool)
        .unwrap_or(notification_defaults.enabled);
    let dock_badge = notifications
        .and_then(|notifications| notifications.get("dockBadge"))
        .map(|value| parse_bool_setting(value, "notifications.dockBadge"))
        .unwrap_or(notification_defaults.dock_badge);
    let show_in_menu_bar = notifications
        .and_then(|notifications| notifications.get("showInMenuBar"))
        .map(|value| parse_bool_setting(value, "notifications.showInMenuBar"))
        .unwrap_or(notification_defaults.show_in_menu_bar);
    let unread_pane_ring = notifications
        .and_then(|notifications| notifications.get("unreadPaneRing"))
        .map(|value| parse_bool_setting(value, "notifications.unreadPaneRing"))
        .unwrap_or(notification_defaults.unread_pane_ring);
    let pane_flash = notifications
        .and_then(|notifications| notifications.get("paneFlash"))
        .map(|value| parse_bool_setting(value, "notifications.paneFlash"))
        .unwrap_or(notification_defaults.pane_flash);
    let notification_sound = notifications
        .and_then(|notifications| notifications.get("sound"))
        .map(|value| parse_notification_sound(value, "notifications.sound"))
        .unwrap_or(notification_defaults.sound);
    let custom_sound_file_path = notifications
        .and_then(|notifications| notifications.get("customSoundFilePath"))
        .map(|value| parse_string_setting(value, "notifications.customSoundFilePath"))
        .unwrap_or_default();
    let notification_command = notifications
        .and_then(|notifications| notifications.get("command"))
        .map(|value| parse_string_setting(value, "notifications.command"))
        .unwrap_or_default();
    let hooks_mode = notifications
        .and_then(|notifications| notifications.get("hooksMode"))
        .map(|value| parse_notification_hooks_mode(value, "notifications.hooksMode"))
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
    let suppress_only_focused_surface = notifications
        .and_then(|notifications| notifications.get("suppressOnlyFocusedSurface"))
        .map(|value| parse_bool_setting(value, "notifications.suppressOnlyFocusedSurface"))
        .unwrap_or(notification_defaults.suppress_only_focused_surface);
    let workspace_groups = root
        .get("workspaceGroups")
        .map(parse_workspace_groups_config)
        .unwrap_or_default();
    let markdown = root
        .get("markdown")
        .map(parse_markdown_config)
        .unwrap_or_default();
    let file_editor = root
        .get("fileEditor")
        .map(parse_file_editor_config)
        .unwrap_or_default();
    let canvas = root
        .get("canvas")
        .map(parse_canvas_config)
        .unwrap_or_default();
    let pane_chrome = PaneChromeConfig {
        pane_border_color: root
            .get("paneBorderColor")
            .map(|value| parse_pane_chrome_color(value, "paneBorderColor"))
            .unwrap_or_default(),
        active_pane_border_color: root
            .get("activePaneBorderColor")
            .map(|value| parse_pane_chrome_color(value, "activePaneBorderColor"))
            .unwrap_or_default(),
    };
    let workspace_colors = root
        .get("workspaceColors")
        .map(parse_workspace_colors_config)
        .unwrap_or_default();
    let sidebar_appearance = root
        .get("sidebarAppearance")
        .map(parse_sidebar_appearance_config)
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
        account,
        integrations,
        automation,
        mobile,
        appearance: AppearanceConfig {
            color_scheme,
            ghostty_color_scheme,
        },
        app: AppBehaviorConfig {
            keep_workspace_open_when_closing_last_surface,
            workspace_inherit_working_directory,
            focus_pane_on_first_click,
            language,
            app_icon,
            window_title_template,
            menu_bar_only,
            preferred_editor,
            open_supported_files_in_cmux,
            open_markdown_in_cmux_viewer,
            minimal_mode,
            global_font_magnification,
            i_message_mode,
            reorder_on_notification,
            send_anonymous_telemetry,
            confirm_quit,
            warn_before_quit,
            warn_before_closing_tab,
            warn_before_closing_tab_x_button,
            hide_tab_close_button,
            rename_selects_existing_name,
            command_palette_searches_all_surfaces,
            file_drop_default_behavior,
            titlebar_controls_style,
            workspace_button_fade,
            workspace_titlebar_visibility,
            system_wide_hotkey_enabled,
            dev_window_display,
        },
        terminal: terminal_config,
        custom_sidebars,
        beta_features,
        markdown,
        file_editor,
        canvas,
        pane_chrome,
        workspace_colors,
        sidebar_appearance,
        sidebar: SidebarConfig {
            hide_all_details,
            wrap_workspace_titles,
            show_workspace_description,
            show_notification_message,
            show_branch_directory,
            branch_layout,
            show_pull_requests,
            watch_git_status,
            show_ports,
            make_pull_requests_clickable,
            open_pull_request_links_in_cmux_browser,
            open_port_links_in_cmux_browser,
            show_ssh,
            show_custom_metadata,
            show_progress,
            show_log,
            right_max_width,
        },
        notifications: NotificationConfig {
            enabled: notifications_enabled,
            dock_badge,
            show_in_menu_bar,
            unread_pane_ring,
            pane_flash,
            sound: notification_sound,
            custom_sound_file_path,
            command: notification_command,
            hooks_mode,
            hooks: notification_hooks,
            agent_permission_prompt,
            agent_turn_complete,
            agent_idle_reminder,
            suppress_only_focused_surface,
        },
        workspace_groups,
        new_workspace_placement,
        font_size,
    }
}

// purpose: Parse the CMUX account settings section.
// inputs: Optional account JSON value from settings.
// returns/effects: Returns CMUX defaults plus strict overrides for known account keys.
fn parse_account_config(value: &Value) -> AccountConfig {
    let account = required_object_setting(value, "account");
    let defaults = AccountConfig::cmux_default();
    AccountConfig {
        pii_display_mode: account
            .get("piiDisplayMode")
            .map(|value| parse_pii_display_mode(value, "account.piiDisplayMode"))
            .unwrap_or(defaults.pii_display_mode),
        selected_team_id: account
            .get("selectedTeamID")
            .map(|value| parse_string_setting(value, "account.selectedTeamID"))
            .unwrap_or(defaults.selected_team_id),
        welcome_shown: account
            .get("welcomeShown")
            .map(|value| parse_bool_setting(value, "account.welcomeShown"))
            .unwrap_or(defaults.welcome_shown),
    }
}

// purpose: Parse the CMUX integrations settings section.
// inputs: Optional integrations JSON value from settings.
// returns/effects: Returns CMUX defaults plus strict overrides for known integration keys.
fn parse_integrations_config(value: &Value) -> IntegrationsConfig {
    let integrations = required_object_setting(value, "integrations");
    let defaults = IntegrationsConfig::cmux_default();
    let claude_code = integrations
        .get("claudeCode")
        .map(|value| required_object_setting(value, "integrations.claudeCode"));
    let codex = integrations
        .get("codex")
        .map(|value| required_object_setting(value, "integrations.codex"));
    let amp = integrations
        .get("amp")
        .map(|value| required_object_setting(value, "integrations.amp"));
    let cursor = integrations
        .get("cursor")
        .map(|value| required_object_setting(value, "integrations.cursor"));
    let gemini = integrations
        .get("gemini")
        .map(|value| required_object_setting(value, "integrations.gemini"));
    let kiro = integrations
        .get("kiro")
        .map(|value| required_object_setting(value, "integrations.kiro"));
    let ripgrep = integrations
        .get("ripgrep")
        .map(|value| required_object_setting(value, "integrations.ripgrep"));
    IntegrationsConfig {
        claude_code_hooks_enabled: claude_code
            .and_then(|section| section.get("hooksEnabled"))
            .map(|value| parse_bool_setting(value, "integrations.claudeCode.hooksEnabled"))
            .unwrap_or(defaults.claude_code_hooks_enabled),
        claude_code_custom_claude_path: claude_code
            .and_then(|section| section.get("customClaudePath"))
            .map(|value| parse_string_setting(value, "integrations.claudeCode.customClaudePath"))
            .unwrap_or(defaults.claude_code_custom_claude_path),
        codex_hooks_enabled: codex
            .and_then(|section| section.get("hooksEnabled"))
            .map(|value| parse_bool_setting(value, "integrations.codex.hooksEnabled"))
            .unwrap_or(defaults.codex_hooks_enabled),
        amp_hooks_enabled: amp
            .and_then(|section| section.get("hooksEnabled"))
            .map(|value| parse_bool_setting(value, "integrations.amp.hooksEnabled"))
            .unwrap_or(defaults.amp_hooks_enabled),
        cursor_hooks_enabled: cursor
            .and_then(|section| section.get("hooksEnabled"))
            .map(|value| parse_bool_setting(value, "integrations.cursor.hooksEnabled"))
            .unwrap_or(defaults.cursor_hooks_enabled),
        gemini_hooks_enabled: gemini
            .and_then(|section| section.get("hooksEnabled"))
            .map(|value| parse_bool_setting(value, "integrations.gemini.hooksEnabled"))
            .unwrap_or(defaults.gemini_hooks_enabled),
        kiro_hooks_enabled: kiro
            .and_then(|section| section.get("hooksEnabled"))
            .map(|value| parse_bool_setting(value, "integrations.kiro.hooksEnabled"))
            .unwrap_or(defaults.kiro_hooks_enabled),
        kiro_notification_level: kiro
            .and_then(|section| section.get("notificationLevel"))
            .map(|value| {
                parse_kiro_notification_level(value, "integrations.kiro.notificationLevel")
            })
            .unwrap_or(defaults.kiro_notification_level),
        ripgrep_custom_binary_path: ripgrep
            .and_then(|section| section.get("customBinaryPath"))
            .map(|value| parse_string_setting(value, "integrations.ripgrep.customBinaryPath"))
            .unwrap_or(defaults.ripgrep_custom_binary_path),
        suppress_subagent_notifications: integrations
            .get("suppressSubagentNotifications")
            .map(|value| parse_bool_setting(value, "integrations.suppressSubagentNotifications"))
            .unwrap_or(defaults.suppress_subagent_notifications),
    }
}

// purpose: Parse the CMUX automation settings section.
// inputs: Optional automation JSON value from settings.
// returns/effects: Returns CMUX defaults plus strict overrides for non-secret automation keys.
fn parse_automation_config(value: &Value) -> AutomationConfig {
    let automation = required_object_setting(value, "automation");
    let defaults = AutomationConfig::cmux_default();
    AutomationConfig {
        socket_control_mode: automation
            .get("socketControlMode")
            .map(|value| parse_socket_control_mode(value, "automation.socketControlMode"))
            .unwrap_or(defaults.socket_control_mode),
        claude_code_integration: automation
            .get("claudeCodeIntegration")
            .map(|value| parse_bool_setting(value, "automation.claudeCodeIntegration"))
            .unwrap_or(defaults.claude_code_integration),
        claude_binary_path: automation
            .get("claudeBinaryPath")
            .map(|value| parse_string_setting(value, "automation.claudeBinaryPath"))
            .unwrap_or(defaults.claude_binary_path),
        workspace_auto_naming: automation
            .get("workspaceAutoNaming")
            .map(|value| parse_bool_setting(value, "automation.workspaceAutoNaming"))
            .unwrap_or(defaults.workspace_auto_naming),
        auto_naming_agent: automation
            .get("autoNamingAgent")
            .map(|value| parse_string_setting(value, "automation.autoNamingAgent"))
            .unwrap_or(defaults.auto_naming_agent),
        ripgrep_binary_path: automation
            .get("ripgrepBinaryPath")
            .map(|value| parse_string_setting(value, "automation.ripgrepBinaryPath"))
            .unwrap_or(defaults.ripgrep_binary_path),
        suppress_subagent_notifications: automation
            .get("suppressSubagentNotifications")
            .map(|value| parse_bool_setting(value, "automation.suppressSubagentNotifications"))
            .unwrap_or(defaults.suppress_subagent_notifications),
        amp_integration: automation
            .get("ampIntegration")
            .map(|value| parse_bool_setting(value, "automation.ampIntegration"))
            .unwrap_or(defaults.amp_integration),
        cursor_integration: automation
            .get("cursorIntegration")
            .map(|value| parse_bool_setting(value, "automation.cursorIntegration"))
            .unwrap_or(defaults.cursor_integration),
        gemini_integration: automation
            .get("geminiIntegration")
            .map(|value| parse_bool_setting(value, "automation.geminiIntegration"))
            .unwrap_or(defaults.gemini_integration),
        kiro_integration: automation
            .get("kiroIntegration")
            .map(|value| parse_bool_setting(value, "automation.kiroIntegration"))
            .unwrap_or(defaults.kiro_integration),
        kiro_notification_level: automation
            .get("kiroNotificationLevel")
            .map(|value| parse_kiro_notification_level(value, "automation.kiroNotificationLevel"))
            .unwrap_or(defaults.kiro_notification_level),
        port_base: automation
            .get("portBase")
            .map(|value| parse_positive_i32_setting(value, "automation.portBase", 1, 65535))
            .unwrap_or(defaults.port_base),
        port_range: automation
            .get("portRange")
            .map(|value| parse_positive_i32_setting(value, "automation.portRange", 1, 65535))
            .unwrap_or(defaults.port_range),
    }
}

// purpose: Parse the CMUX mobile settings section.
// inputs: Optional mobile JSON value from settings.
// returns/effects: Returns release-mode CMUX defaults plus strict overrides.
fn parse_mobile_config(value: &Value) -> MobileConfig {
    let mobile = required_object_setting(value, "mobile");
    let pairing = mobile
        .get("iOSPairingHost")
        .map(|value| required_object_setting(value, "mobile.iOSPairingHost"));
    let defaults = MobileConfig::cmux_default();
    MobileConfig {
        ios_pairing_host_enabled: pairing
            .and_then(|pairing| pairing.get("enabled"))
            .map(|value| parse_bool_setting(value, "mobile.iOSPairingHost.enabled"))
            .unwrap_or(defaults.ios_pairing_host_enabled),
        ios_pairing_host_port: pairing
            .and_then(|pairing| pairing.get("port"))
            .map(|value| parse_positive_i32_setting(value, "mobile.iOSPairingHost.port", 1, 65535))
            .unwrap_or(defaults.ios_pairing_host_port),
        ios_pairing_host_display_name: pairing
            .and_then(|pairing| pairing.get("displayName"))
            .map(|value| parse_string_setting(value, "mobile.iOSPairingHost.displayName"))
            .unwrap_or(defaults.ios_pairing_host_display_name),
    }
}

// purpose: Parse the CMUX file editor settings section.
// inputs: Optional fileEditor JSON value from settings.
// returns/effects: Returns CMUX defaults plus strict overrides for known file-editor keys.
fn parse_file_editor_config(value: &Value) -> FileEditorConfig {
    let file_editor = required_object_setting(value, "fileEditor");
    let defaults = FileEditorConfig::cmux_default();
    FileEditorConfig {
        word_wrap: file_editor
            .get("wordWrap")
            .map(|value| parse_bool_setting(value, "fileEditor.wordWrap"))
            .unwrap_or(defaults.word_wrap),
    }
}

// purpose: Parse the CMUX canvas settings section.
// inputs: Optional canvas JSON value from settings.
// returns/effects: Returns CMUX defaults plus strict overrides for known canvas keys.
fn parse_canvas_config(value: &Value) -> CanvasConfig {
    let canvas = required_object_setting(value, "canvas");
    let defaults = CanvasConfig::cmux_default();
    CanvasConfig {
        pane_gap: canvas
            .get("paneGap")
            .map(|value| parse_positive_i32_setting(value, "canvas.paneGap", 0, 4096))
            .unwrap_or(defaults.pane_gap),
        snapping_enabled: canvas
            .get("snappingEnabled")
            .map(|value| parse_bool_setting(value, "canvas.snappingEnabled"))
            .unwrap_or(defaults.snapping_enabled),
    }
}

// purpose: Parse the CMUX workspace color settings section.
// inputs: Optional workspaceColors JSON value from settings.
// returns/effects: Returns strict palette, custom color, and indicator settings.
fn parse_workspace_colors_config(value: &Value) -> WorkspaceColorsConfig {
    let workspace_colors = required_object_setting(value, "workspaceColors");
    let defaults = WorkspaceColorsConfig::default();
    WorkspaceColorsConfig {
        indicator_style: workspace_colors
            .get("indicatorStyle")
            .map(|value| parse_workspace_indicator_style(value, "workspaceColors.indicatorStyle"))
            .unwrap_or(defaults.indicator_style),
        selection_color: workspace_colors
            .get("selectionColor")
            .map(|value| parse_optional_color_hex_setting(value, "workspaceColors.selectionColor"))
            .unwrap_or(defaults.selection_color),
        notification_badge_color: workspace_colors
            .get("notificationBadgeColor")
            .map(|value| {
                parse_optional_color_hex_setting(value, "workspaceColors.notificationBadgeColor")
            })
            .unwrap_or(defaults.notification_badge_color),
        colors: workspace_colors
            .get("colors")
            .map(|value| parse_color_hex_map_setting(value, "workspaceColors.colors"))
            .unwrap_or(defaults.colors),
        palette_overrides: workspace_colors
            .get("paletteOverrides")
            .map(|value| parse_color_hex_map_setting(value, "workspaceColors.paletteOverrides"))
            .unwrap_or(defaults.palette_overrides),
        custom_colors: workspace_colors
            .get("customColors")
            .map(|value| parse_color_hex_array_setting(value, "workspaceColors.customColors"))
            .unwrap_or(defaults.custom_colors),
    }
}

// purpose: Parse the CMUX sidebar appearance settings section.
// inputs: Optional sidebarAppearance JSON value from settings.
// returns/effects: Returns strict tint, material, blend, state, and opacity settings.
fn parse_sidebar_appearance_config(value: &Value) -> SidebarAppearanceConfig {
    let sidebar_appearance = required_object_setting(value, "sidebarAppearance");
    let defaults = SidebarAppearanceConfig::default();
    SidebarAppearanceConfig {
        match_terminal_background: sidebar_appearance
            .get("matchTerminalBackground")
            .map(|value| parse_bool_setting(value, "sidebarAppearance.matchTerminalBackground"))
            .unwrap_or(defaults.match_terminal_background),
        tint_color: sidebar_appearance
            .get("tintColor")
            .map(|value| parse_required_color_hex_setting(value, "sidebarAppearance.tintColor"))
            .unwrap_or(defaults.tint_color),
        light_mode_tint_color: sidebar_appearance
            .get("lightModeTintColor")
            .map(|value| {
                parse_optional_color_hex_setting(value, "sidebarAppearance.lightModeTintColor")
            })
            .unwrap_or(defaults.light_mode_tint_color),
        dark_mode_tint_color: sidebar_appearance
            .get("darkModeTintColor")
            .map(|value| {
                parse_optional_color_hex_setting(value, "sidebarAppearance.darkModeTintColor")
            })
            .unwrap_or(defaults.dark_mode_tint_color),
        tint_opacity: sidebar_appearance
            .get("tintOpacity")
            .map(|value| {
                parse_non_negative_f64_setting(value, "sidebarAppearance.tintOpacity", 1.0)
            })
            .unwrap_or(defaults.tint_opacity),
        blur_opacity: sidebar_appearance
            .get("blurOpacity")
            .map(|value| {
                parse_non_negative_f64_setting(value, "sidebarAppearance.blurOpacity", 1.0)
            })
            .unwrap_or(defaults.blur_opacity),
        corner_radius: sidebar_appearance
            .get("cornerRadius")
            .map(|value| {
                parse_non_negative_f64_setting(value, "sidebarAppearance.cornerRadius", 4096.0)
            })
            .unwrap_or(defaults.corner_radius),
        preset: sidebar_appearance
            .get("preset")
            .map(|value| parse_sidebar_preset(value, "sidebarAppearance.preset"))
            .unwrap_or(defaults.preset),
        material: sidebar_appearance
            .get("material")
            .map(|value| parse_sidebar_material(value, "sidebarAppearance.material"))
            .unwrap_or(defaults.material),
        blend_mode: sidebar_appearance
            .get("blendMode")
            .map(|value| parse_sidebar_blend_mode(value, "sidebarAppearance.blendMode"))
            .unwrap_or(defaults.blend_mode),
        state: sidebar_appearance
            .get("state")
            .map(|value| parse_sidebar_state(value, "sidebarAppearance.state"))
            .unwrap_or(defaults.state),
    }
}

// purpose: Parse the CMUX markdown viewer settings section.
// inputs: Optional markdown JSON value from settings.
// returns/effects: Returns CMUX defaults plus strict overrides for known markdown keys.
fn parse_markdown_config(value: &Value) -> MarkdownConfig {
    let markdown = required_object_setting(value, "markdown");
    let defaults = MarkdownConfig::cmux_default();
    MarkdownConfig {
        font_size: markdown
            .get("fontSize")
            .map(|value| parse_positive_i32_setting(value, "markdown.fontSize", 8, 96))
            .unwrap_or(defaults.font_size),
        font_family: markdown
            .get("fontFamily")
            .map(|value| parse_string_setting(value, "markdown.fontFamily"))
            .unwrap_or(defaults.font_family),
        max_width: markdown
            .get("maxWidth")
            .map(|value| parse_positive_i32_setting(value, "markdown.maxWidth", 320, 4096))
            .unwrap_or(defaults.max_width),
    }
}

// purpose: Parse the CMUX terminal behavior section.
// inputs: Optional terminal JSON object from settings.
// returns/effects: Returns defaults plus strict overrides for known terminal keys.
fn parse_terminal_behavior_config(
    terminal: Option<&serde_json::Map<String, Value>>,
) -> TerminalBehaviorConfig {
    let defaults = TerminalBehaviorConfig::cmux_default();
    let agent_hibernation = terminal
        .and_then(|terminal| terminal.get("agentHibernation"))
        .map(|value| {
            let object = required_object_setting(value, "terminal.agentHibernation");
            TerminalAgentHibernationConfig {
                enabled: object
                    .get("enabled")
                    .map(|value| parse_bool_setting(value, "terminal.agentHibernation.enabled"))
                    .unwrap_or(defaults.agent_hibernation.enabled),
                idle_seconds: object
                    .get("idleSeconds")
                    .map(|value| {
                        parse_positive_f64_setting(
                            value,
                            "terminal.agentHibernation.idleSeconds",
                            0.1,
                            3600.0,
                        )
                    })
                    .unwrap_or(defaults.agent_hibernation.idle_seconds),
                max_live_terminals: object
                    .get("maxLiveTerminals")
                    .map(|value| {
                        parse_positive_i32_setting(
                            value,
                            "terminal.agentHibernation.maxLiveTerminals",
                            1,
                            4096,
                        )
                    })
                    .unwrap_or(defaults.agent_hibernation.max_live_terminals),
            }
        })
        .unwrap_or(defaults.agent_hibernation.clone());
    let renderer_realization = terminal
        .and_then(|terminal| terminal.get("rendererRealization"))
        .map(|value| {
            let object = required_object_setting(value, "terminal.rendererRealization");
            TerminalRendererRealizationConfig {
                enabled: object
                    .get("enabled")
                    .map(|value| parse_bool_setting(value, "terminal.rendererRealization.enabled"))
                    .unwrap_or(defaults.renderer_realization.enabled),
                idle_seconds: object
                    .get("idleSeconds")
                    .map(|value| {
                        parse_positive_f64_setting(
                            value,
                            "terminal.rendererRealization.idleSeconds",
                            0.1,
                            3600.0,
                        )
                    })
                    .unwrap_or(defaults.renderer_realization.idle_seconds),
                max_warm_renderers: object
                    .get("maxWarmRenderers")
                    .map(|value| {
                        parse_non_negative_i32_setting(
                            value,
                            "terminal.rendererRealization.maxWarmRenderers",
                            4096,
                        )
                    })
                    .unwrap_or(defaults.renderer_realization.max_warm_renderers),
            }
        })
        .unwrap_or(defaults.renderer_realization.clone());
    let title_updates = parse_terminal_title_updates_config(terminal, &defaults);
    let runaway_memory_guardrail = terminal
        .and_then(|terminal| terminal.get("runawayMemoryGuardrail"))
        .map(|value| {
            let object = required_object_setting(value, "terminal.runawayMemoryGuardrail");
            TerminalRunawayMemoryGuardrailConfig {
                enabled: object
                    .get("enabled")
                    .map(|value| {
                        parse_bool_setting(value, "terminal.runawayMemoryGuardrail.enabled")
                    })
                    .unwrap_or(defaults.runaway_memory_guardrail.enabled),
                threshold_gb: object
                    .get("thresholdGB")
                    .map(|value| {
                        parse_positive_f64_setting(
                            value,
                            "terminal.runawayMemoryGuardrail.thresholdGB",
                            0.1,
                            1024.0,
                        )
                    })
                    .unwrap_or(defaults.runaway_memory_guardrail.threshold_gb),
            }
        })
        .unwrap_or(defaults.runaway_memory_guardrail.clone());
    TerminalBehaviorConfig {
        show_scroll_bar: terminal
            .and_then(|terminal| terminal.get("showScrollBar"))
            .map(|value| parse_bool_setting(value, "terminal.showScrollBar"))
            .unwrap_or(defaults.show_scroll_bar),
        copy_on_select: terminal
            .and_then(|terminal| terminal.get("copyOnSelect"))
            .map(|value| parse_bool_setting(value, "terminal.copyOnSelect"))
            .unwrap_or(defaults.copy_on_select),
        auto_resume_agent_sessions: terminal
            .and_then(|terminal| terminal.get("autoResumeAgentSessions"))
            .map(|value| parse_bool_setting(value, "terminal.autoResumeAgentSessions"))
            .unwrap_or(defaults.auto_resume_agent_sessions),
        agent_hibernation,
        renderer_realization,
        title_updates,
        show_text_box_on_new_terminals: terminal
            .and_then(|terminal| terminal.get("showTextBoxOnNewTerminals"))
            .map(|value| parse_bool_setting(value, "terminal.showTextBoxOnNewTerminals"))
            .unwrap_or(defaults.show_text_box_on_new_terminals),
        focus_text_box_on_new_terminals: terminal
            .and_then(|terminal| terminal.get("focusTextBoxOnNewTerminals"))
            .map(|value| parse_bool_setting(value, "terminal.focusTextBoxOnNewTerminals"))
            .unwrap_or(defaults.focus_text_box_on_new_terminals),
        text_box_max_lines: terminal
            .and_then(|terminal| terminal.get("textBoxMaxLines"))
            .map(|value| parse_positive_i32_setting(value, "terminal.textBoxMaxLines", 1, 1000))
            .unwrap_or(defaults.text_box_max_lines),
        text_box_default_submit_action: terminal
            .and_then(|terminal| terminal.get("textBoxDefaultSubmitAction"))
            .map(|value| parse_string_setting(value, "terminal.textBoxDefaultSubmitAction"))
            .unwrap_or(defaults.text_box_default_submit_action),
        text_box_submit_actions: terminal
            .and_then(|terminal| terminal.get("textBoxSubmitActions"))
            .map(|value| parse_string_setting(value, "terminal.textBoxSubmitActions"))
            .unwrap_or(defaults.text_box_submit_actions),
        resume_commands: terminal
            .and_then(|terminal| terminal.get("resumeCommands"))
            .map(|value| parse_string_array_setting(value, "terminal.resumeCommands"))
            .unwrap_or(defaults.resume_commands),
        scroll_speed: terminal
            .and_then(|terminal| terminal.get("scrollSpeed"))
            .map(|value| parse_positive_f64_setting(value, "terminal.scrollSpeed", 0.25, 3.0))
            .unwrap_or(defaults.scroll_speed),
        runaway_memory_guardrail,
    }
}

// purpose: Parse nested terminal title-update settings.
// inputs: Optional terminal object and terminal defaults.
// returns/effects: Returns coalescing/diagnostic settings with loud malformed-value failures.
fn parse_terminal_title_updates_config(
    terminal: Option<&serde_json::Map<String, Value>>,
    defaults: &TerminalBehaviorConfig,
) -> TerminalTitleUpdatesConfig {
    let Some(title_updates) = terminal
        .and_then(|terminal| terminal.get("titleUpdates"))
        .map(|value| required_object_setting(value, "terminal.titleUpdates"))
    else {
        return defaults.title_updates.clone();
    };
    let coalescing = title_updates
        .get("coalescing")
        .map(|value| required_object_setting(value, "terminal.titleUpdates.coalescing"));
    TerminalTitleUpdatesConfig {
        coalescing_enabled: coalescing
            .and_then(|coalescing| coalescing.get("enabled"))
            .map(|value| parse_bool_setting(value, "terminal.titleUpdates.coalescing.enabled"))
            .unwrap_or(defaults.title_updates.coalescing_enabled),
        coalescing_delay_milliseconds: coalescing
            .and_then(|coalescing| coalescing.get("delayMilliseconds"))
            .map(|value| {
                parse_positive_i32_setting(
                    value,
                    "terminal.titleUpdates.coalescing.delayMilliseconds",
                    1,
                    60000,
                )
            })
            .unwrap_or(defaults.title_updates.coalescing_delay_milliseconds),
        diagnostics: title_updates
            .get("diagnostics")
            .map(|value| parse_bool_setting(value, "terminal.titleUpdates.diagnostics"))
            .unwrap_or(defaults.title_updates.diagnostics),
    }
}

// purpose: Parse CMUX custom-sidebar settings.
// inputs: Raw customSidebars JSON value.
// returns/effects: Returns supported renderer/beta settings or panics on malformed values.
fn parse_custom_sidebars_config(value: &Value) -> CustomSidebarsConfig {
    let object = required_object_setting(value, "customSidebars");
    let defaults = CustomSidebarsConfig::default();
    let beta = object
        .get("beta")
        .map(|value| required_object_setting(value, "customSidebars.beta"));
    CustomSidebarsConfig {
        renderer: object
            .get("renderer")
            .map(|value| parse_custom_sidebar_renderer(value, "customSidebars.renderer"))
            .unwrap_or(defaults.renderer),
        beta_enabled: beta
            .and_then(|beta| beta.get("enabled"))
            .map(|value| parse_bool_setting(value, "customSidebars.beta.enabled"))
            .unwrap_or(defaults.beta_enabled),
    }
}

// purpose: Parse CMUX beta feature gates from their top-level sections.
// inputs: Root settings JSON.
// returns/effects: Returns supported beta toggles or defaults.
fn parse_beta_features_config(root: &Value) -> BetaFeaturesConfig {
    let defaults = BetaFeaturesConfig::default();
    BetaFeaturesConfig {
        right_sidebar_feed_enabled: nested_bool_setting(
            root,
            &["rightSidebar", "beta", "feed", "enabled"],
            "rightSidebar.beta.feed.enabled",
        )
        .unwrap_or(defaults.right_sidebar_feed_enabled),
        right_sidebar_dock_enabled: nested_bool_setting(
            root,
            &["rightSidebar", "beta", "dock", "enabled"],
            "rightSidebar.beta.dock.enabled",
        )
        .unwrap_or(defaults.right_sidebar_dock_enabled),
        extensions_enabled: nested_bool_setting(
            root,
            &["extensions", "beta", "enabled"],
            "extensions.beta.enabled",
        )
        .unwrap_or(defaults.extensions_enabled),
        remote_tmux_enabled: nested_bool_setting(
            root,
            &["remoteTmux", "beta", "enabled"],
            "remoteTmux.beta.enabled",
        )
        .unwrap_or(defaults.remote_tmux_enabled),
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

// purpose: Require a settings value to be a JSON object.
// inputs: Raw JSON value and user-facing config path.
// returns/effects: Returns object reference or panics with the exact path.
fn required_object_setting<'a>(value: &'a Value, path: &str) -> &'a serde_json::Map<String, Value> {
    value
        .as_object()
        .unwrap_or_else(|| panic!("{path} must be an object"))
}

// purpose: Parse nested optional booleans without silently accepting malformed parents.
// inputs: Settings root, nested path, and user-facing config path.
// returns/effects: Returns None for absent paths or panics for non-object/non-boolean values.
fn nested_bool_setting(root: &Value, path: &[&str], display_path: &str) -> Option<bool> {
    let mut value = root;
    for (index, key) in path.iter().enumerate() {
        let object = value
            .as_object()
            .unwrap_or_else(|| panic!("{} must be an object", path[..index].join(".")));
        let next = object.get(*key)?;
        value = next;
    }
    Some(parse_bool_setting(value, display_path))
}

// purpose: Parse finite positive f64 settings with CMUX settings-editor clamp ranges.
// inputs: Raw JSON value, user-facing path, and accepted range.
// returns/effects: Returns clamped number or panics on malformed/non-positive values.
fn parse_positive_f64_setting(value: &Value, path: &str, min: f64, max: f64) -> f64 {
    let number = value
        .as_f64()
        .unwrap_or_else(|| panic!("{path} must be a positive number"));
    if !number.is_finite() || number <= 0.0 {
        panic!("{path} must be a positive number");
    }
    number.clamp(min, max)
}

// purpose: Parse finite positive integer settings with CMUX settings-editor clamp ranges.
// inputs: Raw JSON value, user-facing path, and accepted range.
// returns/effects: Returns rounded/clamped integer or panics on malformed/non-positive values.
fn parse_positive_i32_setting(value: &Value, path: &str, min: i32, max: i32) -> i32 {
    parse_positive_f64_setting(value, path, min as f64, max as f64).round() as i32
}

// purpose: Parse finite non-negative integer settings with an upper clamp.
// inputs: Raw JSON value, user-facing path, and accepted maximum.
// returns/effects: Returns rounded/clamped integer or panics on malformed/negative values.
fn parse_non_negative_i32_setting(value: &Value, path: &str, max: i32) -> i32 {
    let number = value
        .as_f64()
        .unwrap_or_else(|| panic!("{path} must be a non-negative number"));
    if !number.is_finite() || number < 0.0 {
        panic!("{path} must be a non-negative number");
    }
    number.round().clamp(0.0, max as f64) as i32
}

// purpose: Parse finite non-negative decimal settings with an upper clamp.
// inputs: Raw JSON value, user-facing path, and accepted maximum.
// returns/effects: Returns clamped decimal or panics on malformed/negative values.
fn parse_non_negative_f64_setting(value: &Value, path: &str, max: f64) -> f64 {
    let number = value
        .as_f64()
        .unwrap_or_else(|| panic!("{path} must be a non-negative number"));
    if !number.is_finite() || number < 0.0 {
        panic!("{path} must be a non-negative number");
    }
    number.clamp(0.0, max)
}

// purpose: Parse required JSON arrays of strings.
// inputs: Raw JSON value and user-facing config path.
// returns/effects: Returns owned strings or panics on malformed arrays.
fn parse_string_array_setting(value: &Value, path: &str) -> Vec<String> {
    let items = value
        .as_array()
        .unwrap_or_else(|| panic!("{path} must be a JSON array of strings"));
    items
        .iter()
        .map(|item| {
            item.as_str()
                .unwrap_or_else(|| panic!("{path} must be a JSON array of strings"))
                .to_string()
        })
        .collect()
}

// purpose: Parse CMUX custom-sidebar renderer mode values.
// inputs: Raw JSON value and user-facing config path.
// returns/effects: Returns renderer enum or panics on unsupported values.
fn parse_custom_sidebar_renderer(value: &Value, path: &str) -> CustomSidebarRendererMode {
    let raw = value
        .as_str()
        .unwrap_or_else(|| panic!("{path} must be inProcess or remote"));
    CustomSidebarRendererMode::from_str(raw)
        .unwrap_or_else(|| panic!("{path} must be inProcess or remote"))
}

// purpose: Parse CMUX automation socket-control mode values.
// inputs: Raw JSON value and user-facing config path.
// returns/effects: Returns mode enum or panics on unsupported values.
fn parse_socket_control_mode(value: &Value, path: &str) -> SocketControlMode {
    let raw = value
        .as_str()
        .unwrap_or_else(|| panic!("{path} must be a socket-control mode string"));
    SocketControlMode::from_str(raw).unwrap_or_else(|| {
        panic!("{path} must be one of: off, cmuxOnly, automation, password, allowAll")
    })
}

// purpose: Parse CMUX sidebar rightMaxWidth while preserving its settings-editor clamp.
// inputs: Raw JSON value and user-facing config path.
// returns/effects: Returns rounded pixels clamped to CMUX's supported 276..4096 range.
fn parse_sidebar_right_max_width(value: &Value, path: &str) -> i32 {
    const MIN_WIDTH: f64 = 276.0;
    const SETTINGS_MAX_WIDTH: f64 = 4096.0;

    let width = value
        .as_f64()
        .unwrap_or_else(|| panic!("{path} must be a positive number"));
    if !width.is_finite() || width <= 0.0 {
        panic!("{path} must be a positive number");
    }
    width.round().clamp(MIN_WIDTH, SETTINGS_MAX_WIDTH) as i32
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

// purpose: Parse CMUX root pane chrome color settings without silent fallback.
// inputs: Raw JSON value and user-facing root config key.
// returns/effects: Returns empty/default or normalized #RRGGBB; panics on malformed config.
fn parse_pane_chrome_color(value: &Value, path: &str) -> String {
    if value.is_null() {
        return String::new();
    }
    let raw = value
        .as_str()
        .unwrap_or_else(|| panic!("{path} must be a #RRGGBB color or null"));
    normalize_pane_chrome_color(raw)
        .unwrap_or_else(|| panic!("{path} must be a #RRGGBB color or null"))
}

// purpose: Match CMUX WorkspaceTabColorSettings.normalizedHex for pane chrome.
// inputs: Raw color with optional leading '#'.
// returns/effects: Returns empty/default, normalized uppercase hex, or None.
fn normalize_pane_chrome_color(raw: &str) -> Option<String> {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return Some(String::new());
    }
    let body = trimmed.strip_prefix('#').unwrap_or(trimmed);
    if body.len() != 6 || !body.chars().all(|ch| ch.is_ascii_hexdigit()) {
        return None;
    }
    Some(format!("#{}", body.to_ascii_uppercase()))
}

// purpose: Parse optional CMUX color strings that may be null or empty.
// inputs: Raw JSON value and user-facing config path.
// returns/effects: Returns empty/default or normalized #RRGGBB; panics on malformed config.
fn parse_optional_color_hex_setting(value: &Value, path: &str) -> String {
    parse_pane_chrome_color(value, path)
}

// purpose: Parse required CMUX color strings.
// inputs: Raw JSON value and user-facing config path.
// returns/effects: Returns normalized #RRGGBB or panics on empty/malformed config.
fn parse_required_color_hex_setting(value: &Value, path: &str) -> String {
    let color = parse_optional_color_hex_setting(value, path);
    if color.is_empty() {
        panic!("{path} must be a #RRGGBB color");
    }
    color
}

// purpose: Parse CMUX color-map settings.
// inputs: Raw JSON object and user-facing config path.
// returns/effects: Returns normalized color values or panics on malformed entries.
fn parse_color_hex_map_setting(value: &Value, path: &str) -> BTreeMap<String, String> {
    let object = value
        .as_object()
        .unwrap_or_else(|| panic!("{path} must be an object of #RRGGBB colors"));
    object
        .iter()
        .map(|(key, value)| {
            let entry_path = format!("{path}.{key}");
            (
                key.clone(),
                parse_required_color_hex_setting(value, &entry_path),
            )
        })
        .collect()
}

// purpose: Parse CMUX custom color arrays.
// inputs: Raw JSON array and user-facing config path.
// returns/effects: Returns normalized color list or panics on malformed entries.
fn parse_color_hex_array_setting(value: &Value, path: &str) -> Vec<String> {
    let array = value
        .as_array()
        .unwrap_or_else(|| panic!("{path} must be a JSON array of #RRGGBB colors"));
    array
        .iter()
        .map(|value| parse_required_color_hex_setting(value, path))
        .collect()
}

// purpose: Parse CMUX workspace indicator style values.
// inputs: Raw JSON value and user-facing config path.
// returns/effects: Returns the modern enum value or panics on unsupported names.
fn parse_workspace_indicator_style(value: &Value, path: &str) -> WorkspaceIndicatorStyle {
    let raw = value
        .as_str()
        .unwrap_or_else(|| panic!("{path} must be a string"));
    WorkspaceIndicatorStyle::from_str(raw).unwrap_or_else(|| {
        panic!(
            "{path} must be one of leftRail, solidFill, rail, border, wash, lift, typography, washRail, or blueWashColorRail"
        )
    })
}

// purpose: Parse CMUX sidebar appearance preset values.
// inputs: Raw JSON value and user-facing config path.
// returns/effects: Returns the preset enum value or panics on unsupported names.
fn parse_sidebar_preset(value: &Value, path: &str) -> SidebarPresetOption {
    let raw = value
        .as_str()
        .unwrap_or_else(|| panic!("{path} must be a string"));
    SidebarPresetOption::from_str(raw).unwrap_or_else(|| {
        panic!("{path} must be one of nativeSidebar, nativeTitlebar, translucent, opaqueDark, opaqueLight, or custom")
    })
}

// purpose: Parse CMUX sidebar material values.
// inputs: Raw JSON value and user-facing config path.
// returns/effects: Returns the material enum value or panics on unsupported names.
fn parse_sidebar_material(value: &Value, path: &str) -> SidebarMaterialOption {
    let raw = value
        .as_str()
        .unwrap_or_else(|| panic!("{path} must be a string"));
    SidebarMaterialOption::from_str(raw)
        .unwrap_or_else(|| panic!("{path} must be a supported sidebar material"))
}

// purpose: Parse CMUX sidebar blend-mode values.
// inputs: Raw JSON value and user-facing config path.
// returns/effects: Returns the blend enum value or panics on unsupported names.
fn parse_sidebar_blend_mode(value: &Value, path: &str) -> SidebarBlendModeOption {
    let raw = value
        .as_str()
        .unwrap_or_else(|| panic!("{path} must be a string"));
    SidebarBlendModeOption::from_str(raw)
        .unwrap_or_else(|| panic!("{path} must be behindWindow or withinWindow"))
}

// purpose: Parse CMUX sidebar state values.
// inputs: Raw JSON value and user-facing config path.
// returns/effects: Returns the state enum value or panics on unsupported names.
fn parse_sidebar_state(value: &Value, path: &str) -> SidebarStateOption {
    let raw = value
        .as_str()
        .unwrap_or_else(|| panic!("{path} must be a string"));
    SidebarStateOption::from_str(raw)
        .unwrap_or_else(|| panic!("{path} must be active, inactive, or followsWindowActiveState"))
}

// purpose: Parse CMUX account PII-display strings without silent fallback.
// inputs: Raw JSON value and user-facing config path.
// returns/effects: Returns account PII mode or panics for malformed existing config.
fn parse_pii_display_mode(value: &Value, path: &str) -> PiiDisplayMode {
    let raw = value
        .as_str()
        .unwrap_or_else(|| panic!("{path} must be visible or hidden"));
    PiiDisplayMode::from_str(raw).unwrap_or_else(|| panic!("{path} must be visible or hidden"))
}

// purpose: Parse CMUX Kiro notification level strings without silent fallback.
// inputs: Raw JSON value and user-facing config path.
// returns/effects: Returns notification level or panics for malformed existing config.
fn parse_kiro_notification_level(value: &Value, path: &str) -> KiroNotificationLevel {
    let raw = value
        .as_str()
        .unwrap_or_else(|| panic!("{path} must be minimal, standard, or verbose"));
    KiroNotificationLevel::from_str(raw)
        .unwrap_or_else(|| panic!("{path} must be minimal, standard, or verbose"))
}

// purpose: Parse CMUX app language strings without silent fallback.
// inputs: Raw JSON value and user-facing config path.
// returns/effects: Returns language mode or panics for malformed existing config.
fn parse_app_language(value: &Value, path: &str) -> AppLanguage {
    let raw = value
        .as_str()
        .unwrap_or_else(|| panic!("{path} must be a supported app language"));
    AppLanguage::from_str(raw).unwrap_or_else(|| panic!("{path} must be a supported app language"))
}

// purpose: Parse CMUX app icon mode strings without silent fallback.
// inputs: Raw JSON value and user-facing config path.
// returns/effects: Returns icon mode or panics for malformed existing config.
fn parse_app_icon(value: &Value, path: &str) -> AppIconMode {
    let raw = value
        .as_str()
        .unwrap_or_else(|| panic!("{path} must be automatic, light, or dark"));
    AppIconMode::from_str(raw).unwrap_or_else(|| panic!("{path} must be automatic, light, or dark"))
}

// purpose: Parse CMUX minimal-mode presentation strings without silent fallback.
// inputs: Raw JSON value and user-facing config path.
// returns/effects: Returns presentation mode or panics for malformed existing config.
fn parse_workspace_presentation_mode(value: &Value, path: &str) -> WorkspacePresentationMode {
    let raw = value
        .as_str()
        .unwrap_or_else(|| panic!("{path} must be standard or minimal"));
    WorkspacePresentationMode::from_str(raw)
        .unwrap_or_else(|| panic!("{path} must be standard or minimal"))
}

// purpose: Parse CMUX app global font magnification percent.
// inputs: Raw JSON value and user-facing config path.
// returns/effects: Returns rounded/clamped 50..200 percent or panics on malformed config.
fn parse_global_font_magnification(value: &Value, path: &str) -> i32 {
    let clamped = parse_positive_i32_setting(value, path, 50, 200);
    ((clamped - 50) as f64 / 10.0).round() as i32 * 10 + 50
}

// purpose: Parse CMUX quit-confirmation mode strings without silent fallback.
// inputs: Raw JSON value and user-facing config path.
// returns/effects: Returns confirm-quit mode or panics for malformed existing config.
fn parse_confirm_quit(value: &Value, path: &str) -> ConfirmQuitMode {
    let raw = value
        .as_str()
        .unwrap_or_else(|| panic!("{path} must be always, dirty-only, or never"));
    ConfirmQuitMode::from_str(raw)
        .unwrap_or_else(|| panic!("{path} must be always, dirty-only, or never"))
}

// purpose: Parse CMUX file-drop default behavior strings without silent fallback.
// inputs: Raw JSON value and user-facing config path.
// returns/effects: Returns file-drop behavior or panics for malformed existing config.
fn parse_file_drop_default_behavior(value: &Value, path: &str) -> FileDropDefaultBehavior {
    let raw = value
        .as_str()
        .unwrap_or_else(|| panic!("{path} must be text or preview"));
    FileDropDefaultBehavior::from_str(raw)
        .unwrap_or_else(|| panic!("{path} must be text or preview"))
}

// purpose: Parse CMUX titlebar-controls style raw integer.
// inputs: Raw JSON value and user-facing config path.
// returns/effects: Returns rounded/clamped 0..4 style index or panics on malformed config.
fn parse_titlebar_controls_style(value: &Value, path: &str) -> i32 {
    parse_non_negative_i32_setting(value, path, 4)
}

// purpose: Parse CMUX workspace button fade mode strings without silent fallback.
// inputs: Raw JSON value and user-facing config path.
// returns/effects: Returns fade mode or panics for malformed existing config.
fn parse_workspace_button_fade(value: &Value, path: &str) -> WorkspaceButtonFadeMode {
    let raw = value
        .as_str()
        .unwrap_or_else(|| panic!("{path} must be enabled or disabled"));
    WorkspaceButtonFadeMode::from_str(raw)
        .unwrap_or_else(|| panic!("{path} must be enabled or disabled"))
}

// purpose: Parse CMUX/Limux appearance strings without silent fallback.
// inputs: Raw JSON value and user-facing config path.
// returns/effects: Returns a color scheme or panics for malformed existing config.
fn parse_color_scheme_setting(value: &Value, path: &str) -> ColorScheme {
    let raw = value
        .as_str()
        .unwrap_or_else(|| panic!("{path} must be a string"));
    ColorScheme::from_str(raw).unwrap_or_else(|| panic!("{path} must be system, light, or dark"))
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

// purpose: Parse CMUX notification hook inheritance mode.
// inputs: Raw JSON value and user-facing config path.
// returns/effects: Returns mode or panics for malformed existing config.
fn parse_notification_hooks_mode(value: &Value, path: &str) -> NotificationHooksMode {
    let raw = value
        .as_str()
        .unwrap_or_else(|| panic!("{path} must be a string"));
    NotificationHooksMode::from_str(raw)
        .unwrap_or_else(|| panic!("{path} must be one of append or replace"))
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

// purpose: Parse one CMUX sidebar branch layout setting.
// inputs: JSON string value plus a diagnostic label.
// returns/effects: Returns a valid layout or panics loudly.
fn parse_sidebar_branch_layout(value: &Value, label: &str) -> SidebarBranchLayout {
    let raw = value
        .as_str()
        .unwrap_or_else(|| panic!("{label} must be a string"));
    SidebarBranchLayout::from_str(raw)
        .unwrap_or_else(|| panic!("{label} must be vertical or inline"))
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
    root.insert(
        "paneBorderColor".to_string(),
        json!(config.pane_chrome.pane_border_color.clone()),
    );
    root.insert(
        "activePaneBorderColor".to_string(),
        json!(config.pane_chrome.active_pane_border_color.clone()),
    );
    root.insert(
        "account".to_string(),
        json!({
            "piiDisplayMode": config.account.pii_display_mode.as_str(),
            "selectedTeamID": config.account.selected_team_id.clone(),
            "welcomeShown": config.account.welcome_shown,
        }),
    );
    root.insert(
        "integrations".to_string(),
        json!({
            "claudeCode": {
                "hooksEnabled": config.integrations.claude_code_hooks_enabled,
                "customClaudePath": config.integrations.claude_code_custom_claude_path.clone(),
            },
            "codex": { "hooksEnabled": config.integrations.codex_hooks_enabled },
            "amp": { "hooksEnabled": config.integrations.amp_hooks_enabled },
            "cursor": { "hooksEnabled": config.integrations.cursor_hooks_enabled },
            "gemini": { "hooksEnabled": config.integrations.gemini_hooks_enabled },
            "kiro": {
                "hooksEnabled": config.integrations.kiro_hooks_enabled,
                "notificationLevel": config.integrations.kiro_notification_level.as_str(),
            },
            "ripgrep": {
                "customBinaryPath": config.integrations.ripgrep_custom_binary_path.clone(),
            },
            "suppressSubagentNotifications": config.integrations.suppress_subagent_notifications,
        }),
    );
    root.insert(
        "automation".to_string(),
        json!({
            "socketControlMode": config.automation.socket_control_mode.as_str(),
            "claudeCodeIntegration": config.automation.claude_code_integration,
            "claudeBinaryPath": config.automation.claude_binary_path.clone(),
            "workspaceAutoNaming": config.automation.workspace_auto_naming,
            "autoNamingAgent": config.automation.auto_naming_agent.clone(),
            "ripgrepBinaryPath": config.automation.ripgrep_binary_path.clone(),
            "suppressSubagentNotifications": config.automation.suppress_subagent_notifications,
            "ampIntegration": config.automation.amp_integration,
            "cursorIntegration": config.automation.cursor_integration,
            "geminiIntegration": config.automation.gemini_integration,
            "kiroIntegration": config.automation.kiro_integration,
            "kiroNotificationLevel": config.automation.kiro_notification_level.as_str(),
            "portBase": config.automation.port_base,
            "portRange": config.automation.port_range,
        }),
    );
    root.insert(
        "mobile".to_string(),
        json!({
            "iOSPairingHost": {
                "enabled": config.mobile.ios_pairing_host_enabled,
                "port": config.mobile.ios_pairing_host_port,
                "displayName": config.mobile.ios_pairing_host_display_name.clone(),
            },
        }),
    );
    let app = root.entry("app".to_string()).or_insert_with(|| json!({}));
    if !app.is_object() {
        *app = json!({});
    }
    app.as_object_mut().expect("app object").insert(
        "newWorkspacePlacement".to_string(),
        json!(config.new_workspace_placement.as_str()),
    );
    app.as_object_mut().expect("app object").insert(
        "appearance".to_string(),
        json!(config.appearance.color_scheme.as_str()),
    );
    app.as_object_mut().expect("app object").insert(
        "workspaceInheritWorkingDirectory".to_string(),
        json!(config.app.workspace_inherit_working_directory),
    );
    app.as_object_mut().expect("app object").insert(
        "focusPaneOnFirstClick".to_string(),
        json!(config.app.focus_pane_on_first_click),
    );
    app.as_object_mut().expect("app object").insert(
        "keepWorkspaceOpenWhenClosingLastSurface".to_string(),
        json!(config.app.keep_workspace_open_when_closing_last_surface),
    );
    app.as_object_mut()
        .expect("app object")
        .insert("language".to_string(), json!(config.app.language.as_str()));
    app.as_object_mut()
        .expect("app object")
        .insert("appIcon".to_string(), json!(config.app.app_icon.as_str()));
    app.as_object_mut().expect("app object").insert(
        "windowTitleTemplate".to_string(),
        json!(config.app.window_title_template.clone()),
    );
    app.as_object_mut()
        .expect("app object")
        .insert("menuBarOnly".to_string(), json!(config.app.menu_bar_only));
    app.as_object_mut().expect("app object").insert(
        "preferredEditor".to_string(),
        json!(config.app.preferred_editor.clone()),
    );
    app.as_object_mut().expect("app object").insert(
        "openSupportedFilesInCmux".to_string(),
        json!(config.app.open_supported_files_in_cmux),
    );
    app.as_object_mut().expect("app object").insert(
        "openMarkdownInCmuxViewer".to_string(),
        json!(config.app.open_markdown_in_cmux_viewer),
    );
    app.as_object_mut().expect("app object").insert(
        "minimalMode".to_string(),
        json!(config.app.minimal_mode.as_str()),
    );
    app.as_object_mut().expect("app object").insert(
        "globalFontMagnification".to_string(),
        json!(config.app.global_font_magnification),
    );
    app.as_object_mut()
        .expect("app object")
        .insert("iMessageMode".to_string(), json!(config.app.i_message_mode));
    app.as_object_mut().expect("app object").insert(
        "reorderOnNotification".to_string(),
        json!(config.app.reorder_on_notification),
    );
    app.as_object_mut().expect("app object").insert(
        "sendAnonymousTelemetry".to_string(),
        json!(config.app.send_anonymous_telemetry),
    );
    app.as_object_mut().expect("app object").insert(
        "confirmQuit".to_string(),
        json!(config.app.confirm_quit.as_str()),
    );
    app.as_object_mut().expect("app object").insert(
        "warnBeforeQuit".to_string(),
        json!(config.app.warn_before_quit),
    );
    app.as_object_mut().expect("app object").insert(
        "warnBeforeClosingTab".to_string(),
        json!(config.app.warn_before_closing_tab),
    );
    app.as_object_mut().expect("app object").insert(
        "warnBeforeClosingTabXButton".to_string(),
        json!(config.app.warn_before_closing_tab_x_button),
    );
    app.as_object_mut().expect("app object").insert(
        "hideTabCloseButton".to_string(),
        json!(config.app.hide_tab_close_button),
    );
    app.as_object_mut().expect("app object").insert(
        "renameSelectsExistingName".to_string(),
        json!(config.app.rename_selects_existing_name),
    );
    app.as_object_mut().expect("app object").insert(
        "commandPaletteSearchesAllSurfaces".to_string(),
        json!(config.app.command_palette_searches_all_surfaces),
    );
    app.as_object_mut().expect("app object").insert(
        "fileDropDefaultBehavior".to_string(),
        json!(config.app.file_drop_default_behavior.as_str()),
    );
    app.as_object_mut().expect("app object").insert(
        "titlebarControlsStyle".to_string(),
        json!(config.app.titlebar_controls_style),
    );
    app.as_object_mut().expect("app object").insert(
        "workspaceButtonFade".to_string(),
        json!(config.app.workspace_button_fade.as_str()),
    );
    app.as_object_mut().expect("app object").insert(
        "workspaceTitlebarVisibility".to_string(),
        json!(config.app.workspace_titlebar_visibility),
    );
    app.as_object_mut().expect("app object").insert(
        "systemWideHotkeyEnabled".to_string(),
        json!(config.app.system_wide_hotkey_enabled),
    );
    app.as_object_mut().expect("app object").insert(
        "devWindowDisplay".to_string(),
        json!(config.app.dev_window_display.clone()),
    );
    let terminal = root
        .entry("terminal".to_string())
        .or_insert_with(|| json!({}));
    if !terminal.is_object() {
        *terminal = json!({});
    }
    terminal.as_object_mut().expect("terminal object").insert(
        "showScrollBar".to_string(),
        json!(config.terminal.show_scroll_bar),
    );
    terminal.as_object_mut().expect("terminal object").insert(
        "copyOnSelect".to_string(),
        json!(config.terminal.copy_on_select),
    );
    terminal.as_object_mut().expect("terminal object").insert(
        "autoResumeAgentSessions".to_string(),
        json!(config.terminal.auto_resume_agent_sessions),
    );
    terminal.as_object_mut().expect("terminal object").insert(
        "agentHibernation".to_string(),
        json!({
            "enabled": config.terminal.agent_hibernation.enabled,
            "idleSeconds": config.terminal.agent_hibernation.idle_seconds,
            "maxLiveTerminals": config.terminal.agent_hibernation.max_live_terminals,
        }),
    );
    terminal.as_object_mut().expect("terminal object").insert(
        "rendererRealization".to_string(),
        json!({
            "enabled": config.terminal.renderer_realization.enabled,
            "idleSeconds": config.terminal.renderer_realization.idle_seconds,
            "maxWarmRenderers": config.terminal.renderer_realization.max_warm_renderers,
        }),
    );
    terminal.as_object_mut().expect("terminal object").insert(
        "titleUpdates".to_string(),
        json!({
            "coalescing": {
                "enabled": config.terminal.title_updates.coalescing_enabled,
                "delayMilliseconds": config.terminal.title_updates.coalescing_delay_milliseconds,
            },
            "diagnostics": config.terminal.title_updates.diagnostics,
        }),
    );
    terminal.as_object_mut().expect("terminal object").insert(
        "showTextBoxOnNewTerminals".to_string(),
        json!(config.terminal.show_text_box_on_new_terminals),
    );
    terminal.as_object_mut().expect("terminal object").insert(
        "focusTextBoxOnNewTerminals".to_string(),
        json!(config.terminal.focus_text_box_on_new_terminals),
    );
    terminal.as_object_mut().expect("terminal object").insert(
        "textBoxMaxLines".to_string(),
        json!(config.terminal.text_box_max_lines),
    );
    terminal.as_object_mut().expect("terminal object").insert(
        "textBoxDefaultSubmitAction".to_string(),
        json!(config.terminal.text_box_default_submit_action),
    );
    terminal.as_object_mut().expect("terminal object").insert(
        "textBoxSubmitActions".to_string(),
        json!(config.terminal.text_box_submit_actions),
    );
    terminal.as_object_mut().expect("terminal object").insert(
        "resumeCommands".to_string(),
        json!(config.terminal.resume_commands),
    );
    terminal.as_object_mut().expect("terminal object").insert(
        "scrollSpeed".to_string(),
        json!(config.terminal.scroll_speed),
    );
    terminal.as_object_mut().expect("terminal object").insert(
        "runawayMemoryGuardrail".to_string(),
        json!({
            "enabled": config.terminal.runaway_memory_guardrail.enabled,
            "thresholdGB": config.terminal.runaway_memory_guardrail.threshold_gb,
        }),
    );
    root.insert(
        "markdown".to_string(),
        json!({
            "fontSize": config.markdown.font_size,
            "fontFamily": config.markdown.font_family,
            "maxWidth": config.markdown.max_width,
        }),
    );
    root.insert(
        "fileEditor".to_string(),
        json!({
            "wordWrap": config.file_editor.word_wrap,
        }),
    );
    root.insert(
        "canvas".to_string(),
        json!({
            "paneGap": config.canvas.pane_gap,
            "snappingEnabled": config.canvas.snapping_enabled,
        }),
    );
    root.insert(
        "customSidebars".to_string(),
        json!({
            "renderer": config.custom_sidebars.renderer.as_str(),
            "beta": {
                "enabled": config.custom_sidebars.beta_enabled,
            },
        }),
    );
    write_nested_bool_setting(
        &mut root,
        &["rightSidebar", "beta", "feed", "enabled"],
        config.beta_features.right_sidebar_feed_enabled,
    )?;
    write_nested_bool_setting(
        &mut root,
        &["rightSidebar", "beta", "dock", "enabled"],
        config.beta_features.right_sidebar_dock_enabled,
    )?;
    write_nested_bool_setting(
        &mut root,
        &["extensions", "beta", "enabled"],
        config.beta_features.extensions_enabled,
    )?;
    write_nested_bool_setting(
        &mut root,
        &["remoteTmux", "beta", "enabled"],
        config.beta_features.remote_tmux_enabled,
    )?;
    root.insert(
        "workspaceColors".to_string(),
        json!({
            "indicatorStyle": config.workspace_colors.indicator_style.as_str(),
            "selectionColor": config.workspace_colors.selection_color,
            "notificationBadgeColor": config.workspace_colors.notification_badge_color,
            "colors": config.workspace_colors.colors,
            "paletteOverrides": config.workspace_colors.palette_overrides,
            "customColors": config.workspace_colors.custom_colors,
        }),
    );
    root.insert(
        "sidebarAppearance".to_string(),
        json!({
            "matchTerminalBackground": config.sidebar_appearance.match_terminal_background,
            "tintColor": config.sidebar_appearance.tint_color,
            "lightModeTintColor": config.sidebar_appearance.light_mode_tint_color,
            "darkModeTintColor": config.sidebar_appearance.dark_mode_tint_color,
            "tintOpacity": config.sidebar_appearance.tint_opacity,
            "blurOpacity": config.sidebar_appearance.blur_opacity,
            "cornerRadius": config.sidebar_appearance.corner_radius,
            "preset": config.sidebar_appearance.preset.as_str(),
            "material": config.sidebar_appearance.material.as_str(),
            "blendMode": config.sidebar_appearance.blend_mode.as_str(),
            "state": config.sidebar_appearance.state.as_str(),
        }),
    );
    let mut sidebar = serde_json::Map::from_iter([
        (
            "hideAllDetails".to_string(),
            json!(config.sidebar.hide_all_details),
        ),
        (
            "wrapWorkspaceTitles".to_string(),
            json!(config.sidebar.wrap_workspace_titles),
        ),
        (
            "showWorkspaceDescription".to_string(),
            json!(config.sidebar.show_workspace_description),
        ),
        (
            "showNotificationMessage".to_string(),
            json!(config.sidebar.show_notification_message),
        ),
        (
            "showBranchDirectory".to_string(),
            json!(config.sidebar.show_branch_directory),
        ),
        (
            "branchLayout".to_string(),
            json!(config.sidebar.branch_layout.as_str()),
        ),
        (
            "showPullRequests".to_string(),
            json!(config.sidebar.show_pull_requests),
        ),
        (
            "watchGitStatus".to_string(),
            json!(config.sidebar.watch_git_status),
        ),
        ("showPorts".to_string(), json!(config.sidebar.show_ports)),
        (
            "makePullRequestsClickable".to_string(),
            json!(config.sidebar.make_pull_requests_clickable),
        ),
        (
            "openPullRequestLinksInCmuxBrowser".to_string(),
            json!(config.sidebar.open_pull_request_links_in_cmux_browser),
        ),
        (
            "openPortLinksInCmuxBrowser".to_string(),
            json!(config.sidebar.open_port_links_in_cmux_browser),
        ),
        ("showSSH".to_string(), json!(config.sidebar.show_ssh)),
        (
            "showCustomMetadata".to_string(),
            json!(config.sidebar.show_custom_metadata),
        ),
        (
            "showProgress".to_string(),
            json!(config.sidebar.show_progress),
        ),
        ("showLog".to_string(), json!(config.sidebar.show_log)),
    ]);
    if let Some(width) = config.sidebar.right_max_width {
        sidebar.insert("rightMaxWidth".to_string(), json!(width));
    }
    root.insert("sidebar".to_string(), Value::Object(sidebar));
    root.insert(
        "notifications".to_string(),
        json!({
            "enabled": config.notifications.enabled,
            "dockBadge": config.notifications.dock_badge,
            "showInMenuBar": config.notifications.show_in_menu_bar,
            "unreadPaneRing": config.notifications.unread_pane_ring,
            "paneFlash": config.notifications.pane_flash,
            "sound": config.notifications.sound.as_str(),
            "customSoundFilePath": config.notifications.custom_sound_file_path.clone(),
            "command": config.notifications.command.clone(),
            "hooksMode": config.notifications.hooks_mode.as_str(),
            "agentPermissionPrompt": config.notifications.agent_permission_prompt,
            "agentTurnComplete": config.notifications.agent_turn_complete.as_str(),
            "agentIdleReminder": config.notifications.agent_idle_reminder,
            "suppressOnlyFocusedSurface": config.notifications.suppress_only_focused_surface,
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

// purpose: Write a nested boolean setting while preserving unrelated sibling sections.
// inputs: Mutable settings root, nested key path, and boolean value.
// returns/effects: Creates missing JSON objects or errors on non-object parents.
fn write_nested_bool_setting(
    root: &mut serde_json::Map<String, Value>,
    path: &[&str],
    value: bool,
) -> Result<(), String> {
    if path.is_empty() {
        return Err("nested boolean settings path cannot be empty".to_string());
    }
    let mut current = root.entry(path[0].to_string()).or_insert_with(|| json!({}));
    for (index, key) in path[1..path.len() - 1].iter().enumerate() {
        if !current.is_object() {
            return Err(format!("{} must be an object", path[..=index].join(".")));
        }
        current = current
            .as_object_mut()
            .expect("checked object")
            .entry((*key).to_string())
            .or_insert_with(|| json!({}));
    }
    if !current.is_object() {
        return Err(format!(
            "{} must be an object",
            path[..path.len() - 1].join(".")
        ));
    }
    current
        .as_object_mut()
        .expect("checked object")
        .insert(path[path.len() - 1].to_string(), Value::Bool(value));
    Ok(())
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
            "dockBadge": true,
            "showInMenuBar": true,
            "unreadPaneRing": true,
            "paneFlash": true,
            "sound": "default",
            "command": "",
            "hooksMode": "append",
            "suppressOnlyFocusedSurface": false
        },
        "sidebar": {
            "hideAllDetails": false,
            "wrapWorkspaceTitles": false,
            "showWorkspaceDescription": true,
            "showNotificationMessage": true,
            "showBranchDirectory": true,
            "branchLayout": "vertical",
            "showPullRequests": true,
            "watchGitStatus": true,
            "showPorts": true,
            "makePullRequestsClickable": true,
            "openPullRequestLinksInCmuxBrowser": true,
            "openPortLinksInCmuxBrowser": true,
            "showSSH": true,
            "showCustomMetadata": true,
            "showProgress": true,
            "showLog": true
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
        assert_eq!(parsed["notifications"]["dockBadge"], Value::Bool(true));
        assert_eq!(parsed["notifications"]["showInMenuBar"], Value::Bool(true));
        assert_eq!(parsed["notifications"]["unreadPaneRing"], Value::Bool(true));
        assert_eq!(parsed["notifications"]["paneFlash"], Value::Bool(true));
        assert_eq!(
            parsed["notifications"]["sound"],
            Value::String("default".to_string())
        );
        assert_eq!(
            parsed["notifications"]["command"],
            Value::String(String::new())
        );
        assert_eq!(
            parsed["notifications"]["hooksMode"],
            Value::String("append".to_string())
        );
        assert_eq!(
            parsed["notifications"]["suppressOnlyFocusedSurface"],
            Value::Bool(false)
        );
        assert_eq!(
            parsed["sidebar"]["showNotificationMessage"],
            Value::Bool(true)
        );
        assert_eq!(parsed["sidebar"]["hideAllDetails"], Value::Bool(false));
        assert_eq!(parsed["sidebar"]["wrapWorkspaceTitles"], Value::Bool(false));
        assert_eq!(
            parsed["sidebar"]["showWorkspaceDescription"],
            Value::Bool(true)
        );
        assert_eq!(parsed["sidebar"]["showBranchDirectory"], Value::Bool(true));
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
    fn load_from_path_prefers_cmux_app_appearance() {
        let dir = TempDir::new().expect("temp dir");
        let path = settings_path_in(dir.path());
        fs::create_dir_all(path.parent().expect("config dir")).expect("create config dir");
        fs::write(
            &path,
            r#"{
  "app": {
    "appearance": "light"
  },
  "appearance": {
    "color_scheme": "dark",
    "ghostty_color_scheme": "system"
  }
}
"#,
        )
        .expect("write config");

        let loaded = load_from_path(&path);

        assert_eq!(loaded.config.appearance.color_scheme, ColorScheme::Light);
        assert_eq!(
            loaded.config.appearance.ghostty_color_scheme,
            ColorScheme::System
        );
    }

    #[test]
    #[should_panic(expected = "app.appearance must be system, light, or dark")]
    fn load_from_path_rejects_invalid_cmux_app_appearance() {
        let dir = TempDir::new().expect("temp dir");
        let path = settings_path_in(dir.path());
        fs::create_dir_all(path.parent().expect("config dir")).expect("create config dir");
        fs::write(&path, r#"{"app":{"appearance":"auto"}}"#).expect("write config");

        let _ = load_from_path(&path);
    }

    // purpose: Verify host config loading accepts the CMUX keep-workspace-open key.
    // inputs: Temporary settings JSON with app.keepWorkspaceOpenWhenClosingLastSurface.
    // returns/effects: Asserts the loaded behavior config is enabled.
    #[test]
    fn load_from_path_reads_keep_workspace_open_on_last_surface() {
        let dir = TempDir::new().expect("temp dir");
        let path = settings_path_in(dir.path());
        fs::create_dir_all(path.parent().expect("config dir")).expect("create config dir");
        fs::write(
            &path,
            r#"{
  "app": {
    "keepWorkspaceOpenWhenClosingLastSurface": true
  }
}
"#,
        )
        .expect("write config");

        let loaded = load_from_path(&path);

        assert!(
            loaded
                .config
                .app
                .keep_workspace_open_when_closing_last_surface
        );
    }

    // purpose: Verify host config loading rejects malformed keep-workspace-open values.
    // inputs: Temporary settings JSON with a string instead of a boolean.
    // returns/effects: Panics with the explicit CMUX key error.
    #[test]
    #[should_panic(expected = "app.keepWorkspaceOpenWhenClosingLastSurface must be a boolean")]
    fn load_from_path_rejects_invalid_keep_workspace_open_on_last_surface() {
        let dir = TempDir::new().expect("temp dir");
        let path = settings_path_in(dir.path());
        fs::create_dir_all(path.parent().expect("config dir")).expect("create config dir");
        fs::write(
            &path,
            r#"{"app":{"keepWorkspaceOpenWhenClosingLastSurface":"true"}}"#,
        )
        .expect("write config");

        let _ = load_from_path(&path);
    }

    // purpose: Verify host config loading accepts the CMUX workspace cwd inheritance key.
    // inputs: Temporary settings JSON with app.workspaceInheritWorkingDirectory.
    // returns/effects: Asserts explicit false overrides the CMUX true default.
    #[test]
    fn load_from_path_reads_workspace_inherit_working_directory() {
        let dir = TempDir::new().expect("temp dir");
        let path = settings_path_in(dir.path());
        fs::create_dir_all(path.parent().expect("config dir")).expect("create config dir");
        fs::write(
            &path,
            r#"{
  "app": {
    "workspaceInheritWorkingDirectory": false
  }
}
"#,
        )
        .expect("write config");

        let loaded = load_from_path(&path);

        assert!(!loaded.config.app.workspace_inherit_working_directory);
    }

    // purpose: Verify host config loading rejects malformed workspace cwd inheritance values.
    // inputs: Temporary settings JSON with a string instead of a boolean.
    // returns/effects: Panics with the explicit CMUX key error.
    #[test]
    #[should_panic(expected = "app.workspaceInheritWorkingDirectory must be a boolean")]
    fn load_from_path_rejects_invalid_workspace_inherit_working_directory() {
        let dir = TempDir::new().expect("temp dir");
        let path = settings_path_in(dir.path());
        fs::create_dir_all(path.parent().expect("config dir")).expect("create config dir");
        fs::write(
            &path,
            r#"{"app":{"workspaceInheritWorkingDirectory":"false"}}"#,
        )
        .expect("write config");

        let _ = load_from_path(&path);
    }

    // purpose: Verify host config loading accepts the CMUX first-click focus key.
    // inputs: Temporary settings JSON with app.focusPaneOnFirstClick.
    // returns/effects: Asserts explicit true overrides the CMUX runtime false default.
    #[test]
    fn load_from_path_reads_focus_pane_on_first_click() {
        let dir = TempDir::new().expect("temp dir");
        let path = settings_path_in(dir.path());
        fs::create_dir_all(path.parent().expect("config dir")).expect("create config dir");
        fs::write(
            &path,
            r#"{
  "app": {
    "focusPaneOnFirstClick": true
  }
}
"#,
        )
        .expect("write config");

        let loaded = load_from_path(&path);

        assert!(loaded.config.app.focus_pane_on_first_click);
    }

    // purpose: Verify host config loading rejects malformed first-click focus values.
    // inputs: Temporary settings JSON with a string instead of a boolean.
    // returns/effects: Panics with the explicit CMUX key error.
    #[test]
    #[should_panic(expected = "app.focusPaneOnFirstClick must be a boolean")]
    fn load_from_path_rejects_invalid_focus_pane_on_first_click() {
        let dir = TempDir::new().expect("temp dir");
        let path = settings_path_in(dir.path());
        fs::create_dir_all(path.parent().expect("config dir")).expect("create config dir");
        fs::write(&path, r#"{"app":{"focusPaneOnFirstClick":"true"}}"#).expect("write config");

        let _ = load_from_path(&path);
    }

    // purpose: Verify host loading accepts additional CMUX app catalog settings.
    // inputs: Settings JSON with app string, boolean, and enum values.
    // returns/effects: Asserts parsed values override CMUX defaults.
    #[test]
    fn load_from_path_reads_app_scalar_settings() {
        let dir = TempDir::new().expect("temp dir");
        let path = settings_path_in(dir.path());
        fs::create_dir_all(path.parent().expect("config dir")).expect("create config dir");
        fs::write(
            &path,
            r#"{
  "app": {
    "language": "pt-BR",
    "appIcon": "dark",
    "windowTitleTemplate": "{workspace}",
    "menuBarOnly": true,
    "preferredEditor": "code --reuse-window",
    "openSupportedFilesInCmux": false,
    "openMarkdownInCmuxViewer": false,
    "minimalMode": "minimal",
    "globalFontMagnification": 175,
    "iMessageMode": true,
    "reorderOnNotification": false,
    "sendAnonymousTelemetry": false,
    "confirmQuit": "dirty-only",
    "warnBeforeQuit": false,
    "warnBeforeClosingTab": false,
    "warnBeforeClosingTabXButton": true,
    "hideTabCloseButton": true,
    "renameSelectsExistingName": false,
    "commandPaletteSearchesAllSurfaces": true,
    "fileDropDefaultBehavior": "preview",
    "titlebarControlsStyle": 4,
    "workspaceButtonFade": "enabled",
    "workspaceTitlebarVisibility": false,
    "systemWideHotkeyEnabled": true,
    "devWindowDisplay": "LG HDR 4K"
  }
}
"#,
        )
        .expect("write config");

        let loaded = load_from_path(&path).config.app;

        assert_eq!(loaded.language, AppLanguage::PtBr);
        assert_eq!(loaded.app_icon, AppIconMode::Dark);
        assert_eq!(loaded.window_title_template, "{workspace}");
        assert!(loaded.menu_bar_only);
        assert_eq!(loaded.preferred_editor, "code --reuse-window");
        assert!(!loaded.open_supported_files_in_cmux);
        assert!(!loaded.open_markdown_in_cmux_viewer);
        assert_eq!(loaded.minimal_mode, WorkspacePresentationMode::Minimal);
        assert_eq!(loaded.global_font_magnification, 180);
        assert!(loaded.i_message_mode);
        assert!(!loaded.reorder_on_notification);
        assert!(!loaded.send_anonymous_telemetry);
        assert_eq!(loaded.confirm_quit, ConfirmQuitMode::DirtyOnly);
        assert!(!loaded.warn_before_quit);
        assert!(!loaded.warn_before_closing_tab);
        assert!(loaded.warn_before_closing_tab_x_button);
        assert!(loaded.hide_tab_close_button);
        assert!(!loaded.rename_selects_existing_name);
        assert!(loaded.command_palette_searches_all_surfaces);
        assert_eq!(
            loaded.file_drop_default_behavior,
            FileDropDefaultBehavior::Preview
        );
        assert_eq!(loaded.titlebar_controls_style, 4);
        assert_eq!(
            loaded.workspace_button_fade,
            WorkspaceButtonFadeMode::Enabled
        );
        assert!(!loaded.workspace_titlebar_visibility);
        assert!(loaded.system_wide_hotkey_enabled);
        assert_eq!(loaded.dev_window_display, "LG HDR 4K");
    }

    // purpose: Verify host loading rejects malformed additional CMUX app settings.
    // inputs: Settings JSON with invalid confirm quit mode.
    // returns/effects: Panics with the explicit CMUX key error.
    #[test]
    #[should_panic(expected = "app.confirmQuit must be always, dirty-only, or never")]
    fn load_from_path_rejects_invalid_app_scalar_settings() {
        let dir = TempDir::new().expect("temp dir");
        let path = settings_path_in(dir.path());
        fs::create_dir_all(path.parent().expect("config dir")).expect("create config dir");
        fs::write(&path, r#"{"app":{"confirmQuit":"sometimes"}}"#).expect("write config");

        let _ = load_from_path(&path);
    }

    // purpose: Verify host config loading accepts the CMUX agent auto-resume key.
    // inputs: Temporary settings JSON with terminal.autoResumeAgentSessions.
    // returns/effects: Asserts explicit false overrides the CMUX true default.
    #[test]
    fn load_from_path_reads_terminal_auto_resume_agent_sessions() {
        let dir = TempDir::new().expect("temp dir");
        let path = settings_path_in(dir.path());
        fs::create_dir_all(path.parent().expect("config dir")).expect("create config dir");
        fs::write(
            &path,
            r#"{
  "terminal": {
    "autoResumeAgentSessions": false
  }
}
"#,
        )
        .expect("write config");

        let loaded = load_from_path(&path);

        assert!(!loaded.config.terminal.auto_resume_agent_sessions);
    }

    // purpose: Verify host config loading rejects malformed terminal auto-resume values.
    // inputs: Temporary settings JSON with a string instead of a boolean.
    // returns/effects: Panics with the explicit CMUX key error.
    #[test]
    #[should_panic(expected = "terminal.autoResumeAgentSessions must be a boolean")]
    fn load_from_path_rejects_invalid_terminal_auto_resume_agent_sessions() {
        let dir = TempDir::new().expect("temp dir");
        let path = settings_path_in(dir.path());
        fs::create_dir_all(path.parent().expect("config dir")).expect("create config dir");
        fs::write(&path, r#"{"terminal":{"autoResumeAgentSessions":"false"}}"#)
            .expect("write config");

        let _ = load_from_path(&path);
    }

    // purpose: Verify host loading accepts remaining CMUX terminal config keys.
    // inputs: Settings JSON with nested terminal performance and text-box values.
    // returns/effects: Asserts parsed values override CMUX defaults.
    #[test]
    fn load_from_path_reads_terminal_scalar_settings() {
        let dir = TempDir::new().expect("temp dir");
        let path = settings_path_in(dir.path());
        fs::create_dir_all(path.parent().expect("config dir")).expect("create config dir");
        fs::write(
            &path,
            r#"{
  "terminal": {
    "showScrollBar": false,
    "copyOnSelect": true,
    "agentHibernation": { "enabled": true, "idleSeconds": 2.5, "maxLiveTerminals": 3 },
    "rendererRealization": { "enabled": false, "idleSeconds": 12.5, "maxWarmRenderers": 4 },
    "titleUpdates": { "coalescing": { "enabled": true, "delayMilliseconds": 250 }, "diagnostics": true },
    "showTextBoxOnNewTerminals": true,
    "focusTextBoxOnNewTerminals": true,
    "textBoxMaxLines": 5,
    "textBoxDefaultSubmitAction": "agent",
    "textBoxSubmitActions": "[{\"id\":\"agent\"}]",
    "resumeCommands": ["codex", "claude"],
    "scrollSpeed": 2.25,
    "runawayMemoryGuardrail": { "enabled": false, "thresholdGB": 4.5 }
  }
}
"#,
        )
        .expect("write config");

        let loaded = load_from_path(&path).config.terminal;

        assert!(!loaded.show_scroll_bar);
        assert!(loaded.copy_on_select);
        assert!(loaded.agent_hibernation.enabled);
        assert_eq!(loaded.agent_hibernation.idle_seconds, 2.5);
        assert_eq!(loaded.renderer_realization.max_warm_renderers, 4);
        assert!(loaded.title_updates.coalescing_enabled);
        assert_eq!(loaded.title_updates.coalescing_delay_milliseconds, 250);
        assert_eq!(loaded.text_box_default_submit_action, "agent");
        assert_eq!(loaded.resume_commands, vec!["codex", "claude"]);
        assert_eq!(loaded.scroll_speed, 2.25);
        assert_eq!(loaded.runaway_memory_guardrail.threshold_gb, 4.5);
    }

    // purpose: Verify host loading rejects malformed CMUX terminal scalar keys.
    // inputs: Settings JSON with a non-string terminal.resumeCommands element.
    // returns/effects: Panics with the explicit CMUX key error.
    #[test]
    #[should_panic(expected = "terminal.resumeCommands must be a JSON array of strings")]
    fn load_from_path_rejects_invalid_terminal_resume_commands() {
        let dir = TempDir::new().expect("temp dir");
        let path = settings_path_in(dir.path());
        fs::create_dir_all(path.parent().expect("config dir")).expect("create config dir");
        fs::write(&path, r#"{"terminal":{"resumeCommands":["codex",1]}}"#).expect("write config");

        let _ = load_from_path(&path);
    }

    // purpose: Verify host loading accepts CMUX markdown viewer settings.
    // inputs: Settings JSON with markdown font, family, and max-width values.
    // returns/effects: Asserts parsed values override CMUX defaults.
    #[test]
    fn load_from_path_reads_markdown_settings() {
        let dir = TempDir::new().expect("temp dir");
        let path = settings_path_in(dir.path());
        fs::create_dir_all(path.parent().expect("config dir")).expect("create config dir");
        fs::write(
            &path,
            r#"{
  "markdown": {
    "fontSize": 18,
    "fontFamily": "Inter",
    "maxWidth": 1200
  }
}
"#,
        )
        .expect("write config");

        let loaded = load_from_path(&path).config.markdown;

        assert_eq!(loaded.font_size, 18);
        assert_eq!(loaded.font_family, "Inter");
        assert_eq!(loaded.max_width, 1200);
    }

    // purpose: Verify host loading rejects malformed CMUX markdown scalar keys.
    // inputs: Settings JSON with an invalid markdown max-width type.
    // returns/effects: Panics with the explicit CMUX key error.
    #[test]
    #[should_panic(expected = "markdown.maxWidth must be a positive number")]
    fn load_from_path_rejects_invalid_markdown_settings() {
        let dir = TempDir::new().expect("temp dir");
        let path = settings_path_in(dir.path());
        fs::create_dir_all(path.parent().expect("config dir")).expect("create config dir");
        fs::write(&path, r#"{"markdown":{"maxWidth":"wide"}}"#).expect("write config");

        let _ = load_from_path(&path);
    }

    // purpose: Verify host loading accepts CMUX account catalog settings.
    // inputs: Settings JSON with account values.
    // returns/effects: Asserts parsed values override CMUX defaults.
    #[test]
    fn load_from_path_reads_account_settings() {
        let dir = TempDir::new().expect("temp dir");
        let path = settings_path_in(dir.path());
        fs::create_dir_all(path.parent().expect("config dir")).expect("create config dir");
        fs::write(
            &path,
            r#"{
  "account": {
    "piiDisplayMode": "hidden",
    "selectedTeamID": "team_123",
    "welcomeShown": true
  }
}
"#,
        )
        .expect("write config");

        let loaded = load_from_path(&path).config.account;

        assert_eq!(loaded.pii_display_mode, PiiDisplayMode::Hidden);
        assert_eq!(loaded.selected_team_id, "team_123");
        assert!(loaded.welcome_shown);
    }

    // purpose: Verify host loading rejects malformed CMUX account settings.
    // inputs: Settings JSON with an invalid PII display mode.
    // returns/effects: Panics with the explicit CMUX key error.
    #[test]
    #[should_panic(expected = "account.piiDisplayMode must be visible or hidden")]
    fn load_from_path_rejects_invalid_account_settings() {
        let dir = TempDir::new().expect("temp dir");
        let path = settings_path_in(dir.path());
        fs::create_dir_all(path.parent().expect("config dir")).expect("create config dir");
        fs::write(&path, r#"{"account":{"piiDisplayMode":"redacted"}}"#).expect("write config");

        let _ = load_from_path(&path);
    }

    // purpose: Verify host loading accepts CMUX integrations catalog settings.
    // inputs: Settings JSON with nested integration values.
    // returns/effects: Asserts parsed values override CMUX defaults.
    #[test]
    fn load_from_path_reads_integrations_settings() {
        let dir = TempDir::new().expect("temp dir");
        let path = settings_path_in(dir.path());
        fs::create_dir_all(path.parent().expect("config dir")).expect("create config dir");
        fs::write(
            &path,
            r#"{
  "integrations": {
    "claudeCode": { "hooksEnabled": false, "customClaudePath": "/opt/claude" },
    "codex": { "hooksEnabled": false },
    "amp": { "hooksEnabled": false },
    "cursor": { "hooksEnabled": false },
    "gemini": { "hooksEnabled": false },
    "kiro": { "hooksEnabled": false, "notificationLevel": "verbose" },
    "ripgrep": { "customBinaryPath": "/usr/local/bin/rg" },
    "suppressSubagentNotifications": false
  }
}
"#,
        )
        .expect("write config");

        let loaded = load_from_path(&path).config.integrations;

        assert!(!loaded.claude_code_hooks_enabled);
        assert_eq!(loaded.claude_code_custom_claude_path, "/opt/claude");
        assert!(!loaded.codex_hooks_enabled);
        assert!(!loaded.amp_hooks_enabled);
        assert!(!loaded.cursor_hooks_enabled);
        assert!(!loaded.gemini_hooks_enabled);
        assert!(!loaded.kiro_hooks_enabled);
        assert_eq!(
            loaded.kiro_notification_level,
            KiroNotificationLevel::Verbose
        );
        assert_eq!(loaded.ripgrep_custom_binary_path, "/usr/local/bin/rg");
        assert!(!loaded.suppress_subagent_notifications);
    }

    // purpose: Verify host loading rejects malformed CMUX integrations settings.
    // inputs: Settings JSON with an invalid Kiro notification level.
    // returns/effects: Panics with the explicit CMUX key error.
    #[test]
    #[should_panic(
        expected = "integrations.kiro.notificationLevel must be minimal, standard, or verbose"
    )]
    fn load_from_path_rejects_invalid_integrations_settings() {
        let dir = TempDir::new().expect("temp dir");
        let path = settings_path_in(dir.path());
        fs::create_dir_all(path.parent().expect("config dir")).expect("create config dir");
        fs::write(
            &path,
            r#"{"integrations":{"kiro":{"notificationLevel":"chatty"}}}"#,
        )
        .expect("write config");

        let _ = load_from_path(&path);
    }

    // purpose: Verify host loading rejects malformed CMUX integration subsections.
    // inputs: Settings JSON with a scalar where integrations.codex object is required.
    // returns/effects: Panics with the explicit CMUX subsection error.
    #[test]
    #[should_panic(expected = "integrations.codex must be an object")]
    fn load_from_path_rejects_malformed_integration_subsections() {
        let dir = TempDir::new().expect("temp dir");
        let path = settings_path_in(dir.path());
        fs::create_dir_all(path.parent().expect("config dir")).expect("create config dir");
        fs::write(&path, r#"{"integrations":{"codex":false}}"#).expect("write config");

        let _ = load_from_path(&path);
    }

    // purpose: Verify host loading accepts CMUX automation and mobile catalog settings.
    // inputs: Settings JSON with automation and mobile sections.
    // returns/effects: Asserts strict typed values override CMUX defaults.
    #[test]
    fn load_from_path_reads_automation_and_mobile_settings() {
        let dir = TempDir::new().expect("temp dir");
        let path = settings_path_in(dir.path());
        fs::create_dir_all(path.parent().expect("config dir")).expect("create config dir");
        fs::write(
            &path,
            r#"{
  "automation": {
    "socketControlMode": "password",
    "claudeCodeIntegration": false,
    "claudeBinaryPath": "/opt/claude",
    "workspaceAutoNaming": true,
    "autoNamingAgent": "codex",
    "ripgrepBinaryPath": "/usr/local/bin/rg",
    "suppressSubagentNotifications": false,
    "ampIntegration": false,
    "cursorIntegration": false,
    "geminiIntegration": false,
    "kiroIntegration": false,
    "kiroNotificationLevel": "verbose",
    "portBase": 9200,
    "portRange": 24
  },
  "mobile": {
    "iOSPairingHost": {
      "enabled": true,
      "port": 58466,
      "displayName": "Dev Linux"
    }
  }
}
"#,
        )
        .expect("write config");

        let loaded = load_from_path(&path).config;

        assert_eq!(
            loaded.automation.socket_control_mode,
            SocketControlMode::Password
        );
        assert!(!loaded.automation.claude_code_integration);
        assert_eq!(loaded.automation.claude_binary_path, "/opt/claude");
        assert!(loaded.automation.workspace_auto_naming);
        assert_eq!(loaded.automation.auto_naming_agent, "codex");
        assert_eq!(loaded.automation.ripgrep_binary_path, "/usr/local/bin/rg");
        assert!(!loaded.automation.suppress_subagent_notifications);
        assert!(!loaded.automation.amp_integration);
        assert!(!loaded.automation.cursor_integration);
        assert!(!loaded.automation.gemini_integration);
        assert!(!loaded.automation.kiro_integration);
        assert_eq!(
            loaded.automation.kiro_notification_level,
            KiroNotificationLevel::Verbose
        );
        assert_eq!(loaded.automation.port_base, 9200);
        assert_eq!(loaded.automation.port_range, 24);
        assert!(loaded.mobile.ios_pairing_host_enabled);
        assert_eq!(loaded.mobile.ios_pairing_host_port, 58466);
        assert_eq!(loaded.mobile.ios_pairing_host_display_name, "Dev Linux");
    }

    // purpose: Verify host loading rejects malformed CMUX automation settings.
    // inputs: Settings JSON with an invalid socket control mode.
    // returns/effects: Panics with the explicit automation key error.
    #[test]
    #[should_panic(expected = "automation.socketControlMode must be one of")]
    fn load_from_path_rejects_invalid_automation_settings() {
        let dir = TempDir::new().expect("temp dir");
        let path = settings_path_in(dir.path());
        fs::create_dir_all(path.parent().expect("config dir")).expect("create config dir");
        fs::write(&path, r#"{"automation":{"socketControlMode":"open"}}"#).expect("write config");

        let _ = load_from_path(&path);
    }

    // purpose: Verify host loading rejects malformed CMUX mobile settings.
    // inputs: Settings JSON with an out-of-range iOS pairing port.
    // returns/effects: Panics with the explicit mobile key error.
    #[test]
    #[should_panic(expected = "mobile.iOSPairingHost.port must be a positive number")]
    fn load_from_path_rejects_invalid_mobile_settings() {
        let dir = TempDir::new().expect("temp dir");
        let path = settings_path_in(dir.path());
        fs::create_dir_all(path.parent().expect("config dir")).expect("create config dir");
        fs::write(&path, r#"{"mobile":{"iOSPairingHost":{"port":0}}}"#).expect("write config");

        let _ = load_from_path(&path);
    }

    // purpose: Verify host loading accepts CMUX file editor and canvas settings.
    // inputs: Settings JSON with fileEditor and canvas values.
    // returns/effects: Asserts parsed values override CMUX defaults.
    #[test]
    fn load_from_path_reads_file_editor_and_canvas_settings() {
        let dir = TempDir::new().expect("temp dir");
        let path = settings_path_in(dir.path());
        fs::create_dir_all(path.parent().expect("config dir")).expect("create config dir");
        fs::write(
            &path,
            r#"{
  "fileEditor": { "wordWrap": true },
  "canvas": { "paneGap": 24, "snappingEnabled": false }
}
"#,
        )
        .expect("write config");

        let loaded = load_from_path(&path).config;

        assert!(loaded.file_editor.word_wrap);
        assert_eq!(loaded.canvas.pane_gap, 24);
        assert!(!loaded.canvas.snapping_enabled);
    }

    // purpose: Verify host loading rejects malformed CMUX file editor and canvas settings.
    // inputs: Settings JSON with an invalid canvas pane gap.
    // returns/effects: Panics with the explicit CMUX key error.
    #[test]
    #[should_panic(expected = "canvas.paneGap must be a positive number")]
    fn load_from_path_rejects_invalid_file_editor_and_canvas_settings() {
        let dir = TempDir::new().expect("temp dir");
        let path = settings_path_in(dir.path());
        fs::create_dir_all(path.parent().expect("config dir")).expect("create config dir");
        fs::write(&path, r#"{"canvas":{"paneGap":"wide"}}"#).expect("write config");

        let _ = load_from_path(&path);
    }

    // purpose: Verify host loading accepts CMUX root pane chrome color settings.
    // inputs: Settings JSON with hex colors and a null clear value.
    // returns/effects: Asserts colors normalize like CMUX and null clears to default.
    #[test]
    fn load_from_path_reads_pane_chrome_settings() {
        let dir = TempDir::new().expect("temp dir");
        let path = settings_path_in(dir.path());
        fs::create_dir_all(path.parent().expect("config dir")).expect("create config dir");
        fs::write(
            &path,
            r#"{"paneBorderColor":"33aaff","activePaneBorderColor":null}"#,
        )
        .expect("write config");

        let loaded = load_from_path(&path).config.pane_chrome;

        assert_eq!(loaded.pane_border_color, "#33AAFF");
        assert_eq!(loaded.active_pane_border_color, "");
    }

    // purpose: Verify host loading rejects malformed CMUX pane chrome colors.
    // inputs: Settings JSON with an invalid root pane color.
    // returns/effects: Panics with the explicit CMUX key error.
    #[test]
    #[should_panic(expected = "paneBorderColor must be a #RRGGBB color or null")]
    fn load_from_path_rejects_invalid_pane_chrome_settings() {
        let dir = TempDir::new().expect("temp dir");
        let path = settings_path_in(dir.path());
        fs::create_dir_all(path.parent().expect("config dir")).expect("create config dir");
        fs::write(&path, r#"{"paneBorderColor":"blue"}"#).expect("write config");

        let _ = load_from_path(&path);
    }

    // purpose: Verify host loading accepts CMUX custom sidebar and beta settings.
    // inputs: Settings JSON with renderer and beta feature toggles.
    // returns/effects: Asserts parsed values override defaults.
    #[test]
    fn load_from_path_reads_custom_sidebar_and_beta_settings() {
        let dir = TempDir::new().expect("temp dir");
        let path = settings_path_in(dir.path());
        fs::create_dir_all(path.parent().expect("config dir")).expect("create config dir");
        fs::write(
            &path,
            r#"{
  "customSidebars": { "renderer": "remote", "beta": { "enabled": false } },
  "rightSidebar": { "beta": { "feed": { "enabled": true }, "dock": { "enabled": true } } },
  "extensions": { "beta": { "enabled": true } },
  "remoteTmux": { "beta": { "enabled": true } }
}
"#,
        )
        .expect("write config");

        let loaded = load_from_path(&path).config;

        assert_eq!(
            loaded.custom_sidebars.renderer,
            CustomSidebarRendererMode::Remote
        );
        assert!(!loaded.custom_sidebars.beta_enabled);
        assert!(loaded.beta_features.right_sidebar_feed_enabled);
        assert!(loaded.beta_features.right_sidebar_dock_enabled);
        assert!(loaded.beta_features.extensions_enabled);
        assert!(loaded.beta_features.remote_tmux_enabled);
    }

    // purpose: Verify host loading rejects malformed CMUX custom sidebar renderer values.
    // inputs: Settings JSON with an unsupported customSidebars.renderer value.
    // returns/effects: Panics with the explicit CMUX key error.
    #[test]
    #[should_panic(expected = "customSidebars.renderer must be inProcess or remote")]
    fn load_from_path_rejects_invalid_custom_sidebar_renderer() {
        let dir = TempDir::new().expect("temp dir");
        let path = settings_path_in(dir.path());
        fs::create_dir_all(path.parent().expect("config dir")).expect("create config dir");
        fs::write(&path, r#"{"customSidebars":{"renderer":"sandboxed"}}"#).expect("write config");

        let _ = load_from_path(&path);
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
    "dockBadge": false,
    "showInMenuBar": false,
    "unreadPaneRing": false,
    "paneFlash": false,
    "sound": "bell",
    "customSoundFilePath": "/tmp/notify.wav",
    "command": "printf done",
    "hooksMode": "replace",
    "agentPermissionPrompt": false,
    "agentTurnComplete": "always",
    "agentIdleReminder": false,
    "suppressOnlyFocusedSurface": true
  }
}
"#,
        )
        .expect("write config");

        let loaded = load_from_path(&path);

        assert!(loaded.warnings.is_empty());
        assert!(!loaded.config.notifications.enabled);
        assert!(!loaded.config.notifications.dock_badge);
        assert!(!loaded.config.notifications.show_in_menu_bar);
        assert!(!loaded.config.notifications.unread_pane_ring);
        assert!(!loaded.config.notifications.pane_flash);
        assert_eq!(loaded.config.notifications.sound, NotificationSound::Bell);
        assert_eq!(
            loaded.config.notifications.custom_sound_file_path,
            "/tmp/notify.wav"
        );
        assert_eq!(loaded.config.notifications.command, "printf done");
        assert_eq!(
            loaded.config.notifications.hooks_mode,
            NotificationHooksMode::Replace
        );
        assert!(!loaded.config.notifications.agent_permission_prompt);
        assert_eq!(
            loaded.config.notifications.agent_turn_complete,
            AgentTurnCompleteMode::Always
        );
        assert!(!loaded.config.notifications.agent_idle_reminder);
        assert!(loaded.config.notifications.suppress_only_focused_surface);
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
    #[should_panic(expected = "notifications.suppressOnlyFocusedSurface must be a boolean")]
    fn load_from_path_rejects_invalid_focused_surface_suppression() {
        let dir = TempDir::new().expect("temp dir");
        let path = settings_path_in(dir.path());
        fs::create_dir_all(path.parent().expect("config dir")).expect("create config dir");
        fs::write(
            &path,
            r#"{"notifications":{"suppressOnlyFocusedSurface":"true"}}"#,
        )
        .expect("write config");

        let _ = load_from_path(&path);
    }

    #[test]
    #[should_panic(expected = "notifications.unreadPaneRing must be a boolean")]
    fn load_from_path_rejects_invalid_unread_pane_ring() {
        let dir = TempDir::new().expect("temp dir");
        let path = settings_path_in(dir.path());
        fs::create_dir_all(path.parent().expect("config dir")).expect("create config dir");
        fs::write(&path, r#"{"notifications":{"unreadPaneRing":"true"}}"#).expect("write config");

        let _ = load_from_path(&path);
    }

    #[test]
    #[should_panic(expected = "notifications.hooksMode must be one of append or replace")]
    fn load_from_path_rejects_invalid_notification_hooks_mode() {
        let dir = TempDir::new().expect("temp dir");
        let path = settings_path_in(dir.path());
        fs::create_dir_all(path.parent().expect("config dir")).expect("create config dir");
        fs::write(&path, r#"{"notifications":{"hooksMode":"merge"}}"#).expect("write config");

        let _ = load_from_path(&path);
    }

    #[test]
    fn load_from_path_reads_sidebar_preferences() {
        let dir = TempDir::new().expect("temp dir");
        let path = settings_path_in(dir.path());
        fs::create_dir_all(path.parent().expect("config dir")).expect("create config dir");
        fs::write(
            &path,
            r#"{
  "sidebar": {
    "hideAllDetails": true,
    "wrapWorkspaceTitles": true,
    "showWorkspaceDescription": false,
    "showNotificationMessage": false,
    "showBranchDirectory": false,
    "branchLayout": "inline",
    "showPullRequests": false,
    "watchGitStatus": false,
    "showPorts": false,
    "makePullRequestsClickable": false,
    "openPullRequestLinksInCmuxBrowser": false,
    "openPortLinksInCmuxBrowser": false,
    "showSSH": false,
    "showCustomMetadata": false,
    "showProgress": false,
    "showLog": false,
    "rightMaxWidth": 10000
  }
}
"#,
        )
        .expect("write config");

        let loaded = load_from_path(&path);

        assert!(loaded.warnings.is_empty());
        assert!(loaded.config.sidebar.hide_all_details);
        assert!(loaded.config.sidebar.wrap_workspace_titles);
        assert!(!loaded.config.sidebar.show_workspace_description);
        assert!(!loaded.config.sidebar.show_notification_message);
        assert!(!loaded.config.sidebar.show_branch_directory);
        assert_eq!(
            loaded.config.sidebar.branch_layout,
            SidebarBranchLayout::Inline
        );
        assert!(!loaded.config.sidebar.show_pull_requests);
        assert!(!loaded.config.sidebar.watch_git_status);
        assert!(!loaded.config.sidebar.show_ports);
        assert!(!loaded.config.sidebar.make_pull_requests_clickable);
        assert!(
            !loaded
                .config
                .sidebar
                .open_pull_request_links_in_cmux_browser
        );
        assert!(!loaded.config.sidebar.open_port_links_in_cmux_browser);
        assert!(!loaded.config.sidebar.show_ssh);
        assert!(!loaded.config.sidebar.show_custom_metadata);
        assert!(!loaded.config.sidebar.show_progress);
        assert!(!loaded.config.sidebar.show_log);
        assert_eq!(loaded.config.sidebar.right_max_width, Some(4096));
    }

    #[test]
    #[should_panic(expected = "sidebar.showProgress must be a boolean")]
    fn load_from_path_rejects_invalid_sidebar_progress() {
        let dir = TempDir::new().expect("temp dir");
        let path = settings_path_in(dir.path());
        fs::create_dir_all(path.parent().expect("config dir")).expect("create config dir");
        fs::write(&path, r#"{"sidebar":{"showProgress":"false"}}"#).expect("write config");

        let _ = load_from_path(&path);
    }

    // purpose: Verify malformed CMUX sidebar git-watch settings fail loudly.
    // inputs: Settings JSON with non-boolean sidebar.watchGitStatus.
    // returns/effects: Panics instead of accepting a silent fallback.
    #[test]
    #[should_panic(expected = "sidebar.watchGitStatus must be a boolean")]
    fn load_from_path_rejects_invalid_sidebar_watch_git_status() {
        let dir = TempDir::new().expect("temp dir");
        let path = settings_path_in(dir.path());
        fs::create_dir_all(path.parent().expect("config dir")).expect("create config dir");
        fs::write(&path, r#"{"sidebar":{"watchGitStatus":"false"}}"#).expect("write config");

        let _ = load_from_path(&path);
    }

    // purpose: Verify malformed CMUX sidebar branch layout settings fail loudly.
    // inputs: Settings JSON with unsupported sidebar.branchLayout.
    // returns/effects: Panics instead of accepting a silent fallback.
    #[test]
    #[should_panic(expected = "sidebar.branchLayout must be vertical or inline")]
    fn load_from_path_rejects_invalid_sidebar_branch_layout() {
        let dir = TempDir::new().expect("temp dir");
        let path = settings_path_in(dir.path());
        fs::create_dir_all(path.parent().expect("config dir")).expect("create config dir");
        fs::write(&path, r#"{"sidebar":{"branchLayout":"stacked"}}"#).expect("write config");

        let _ = load_from_path(&path);
    }

    // purpose: Verify malformed CMUX sidebar port visibility settings fail loudly.
    // inputs: Settings JSON with non-boolean sidebar.showPorts.
    // returns/effects: Panics instead of accepting a silent fallback.
    #[test]
    #[should_panic(expected = "sidebar.showPorts must be a boolean")]
    fn load_from_path_rejects_invalid_sidebar_show_ports() {
        let dir = TempDir::new().expect("temp dir");
        let path = settings_path_in(dir.path());
        fs::create_dir_all(path.parent().expect("config dir")).expect("create config dir");
        fs::write(&path, r#"{"sidebar":{"showPorts":"false"}}"#).expect("write config");

        let _ = load_from_path(&path);
    }

    // purpose: Verify malformed CMUX sidebar pull-request link settings fail loudly.
    // inputs: Settings JSON with non-boolean sidebar.makePullRequestsClickable.
    // returns/effects: Panics instead of accepting a silent fallback.
    #[test]
    #[should_panic(expected = "sidebar.makePullRequestsClickable must be a boolean")]
    fn load_from_path_rejects_invalid_sidebar_pull_request_click_policy() {
        let dir = TempDir::new().expect("temp dir");
        let path = settings_path_in(dir.path());
        fs::create_dir_all(path.parent().expect("config dir")).expect("create config dir");
        fs::write(
            &path,
            r#"{"sidebar":{"makePullRequestsClickable":"false"}}"#,
        )
        .expect("write config");

        let _ = load_from_path(&path);
    }

    // purpose: Verify malformed CMUX right sidebar width settings fail loudly.
    // inputs: Settings JSON with a non-positive sidebar.rightMaxWidth value.
    // returns/effects: Panics instead of accepting a silent fallback.
    #[test]
    #[should_panic(expected = "sidebar.rightMaxWidth must be a positive number")]
    fn load_from_path_rejects_invalid_sidebar_right_max_width() {
        let dir = TempDir::new().expect("temp dir");
        let path = settings_path_in(dir.path());
        fs::create_dir_all(path.parent().expect("config dir")).expect("create config dir");
        fs::write(&path, r#"{"sidebar":{"rightMaxWidth":0}}"#).expect("write config");

        let _ = load_from_path(&path);
    }

    // purpose: Verify CMUX workspace color and sidebar appearance settings load strictly.
    // inputs: Settings JSON with workspaceColors and sidebarAppearance sections.
    // returns/effects: Asserts normalized colors, legacy indicator aliases, and enum parsing.
    #[test]
    fn load_from_path_reads_workspace_colors_and_sidebar_appearance() {
        let dir = TempDir::new().expect("temp dir");
        let path = settings_path_in(dir.path());
        fs::create_dir_all(path.parent().expect("config dir")).expect("create config dir");
        fs::write(
            &path,
            r##"{
  "workspaceColors": {
    "indicatorStyle": "blueWashColorRail",
    "selectionColor": "336699",
    "notificationBadgeColor": "#aa5500",
    "colors": {"Red": "c0392b"},
    "paletteOverrides": {"Blue": "#1565c0"},
    "customColors": ["112233"]
  },
  "sidebarAppearance": {
    "matchTerminalBackground": true,
    "tintColor": "102030",
    "lightModeTintColor": "",
    "darkModeTintColor": "#445566",
    "tintOpacity": 0,
    "blurOpacity": 0.6,
    "cornerRadius": 12.5,
    "preset": "custom",
    "material": "hudWindow",
    "blendMode": "behindWindow",
    "state": "inactive"
  }
}
"##,
        )
        .expect("write config");

        let loaded = load_from_path(&path);

        assert_eq!(
            loaded.config.workspace_colors.indicator_style,
            WorkspaceIndicatorStyle::SolidFill
        );
        assert_eq!(loaded.config.workspace_colors.selection_color, "#336699");
        assert_eq!(
            loaded.config.workspace_colors.notification_badge_color,
            "#AA5500"
        );
        assert_eq!(
            loaded.config.workspace_colors.colors.get("Red"),
            Some(&"#C0392B".to_string())
        );
        assert_eq!(
            loaded.config.workspace_colors.palette_overrides.get("Blue"),
            Some(&"#1565C0".to_string())
        );
        assert_eq!(loaded.config.workspace_colors.custom_colors, ["#112233"]);
        assert!(loaded.config.sidebar_appearance.match_terminal_background);
        assert_eq!(loaded.config.sidebar_appearance.tint_color, "#102030");
        assert_eq!(loaded.config.sidebar_appearance.light_mode_tint_color, "");
        assert_eq!(
            loaded.config.sidebar_appearance.dark_mode_tint_color,
            "#445566"
        );
        assert_eq!(loaded.config.sidebar_appearance.tint_opacity, 0.0);
        assert_eq!(loaded.config.sidebar_appearance.blur_opacity, 0.6);
        assert_eq!(loaded.config.sidebar_appearance.corner_radius, 12.5);
        assert_eq!(
            loaded.config.sidebar_appearance.preset,
            SidebarPresetOption::Custom
        );
        assert_eq!(
            loaded.config.sidebar_appearance.material,
            SidebarMaterialOption::HudWindow
        );
        assert_eq!(
            loaded.config.sidebar_appearance.blend_mode,
            SidebarBlendModeOption::BehindWindow
        );
        assert_eq!(
            loaded.config.sidebar_appearance.state,
            SidebarStateOption::Inactive
        );
    }

    // purpose: Verify malformed CMUX workspace color map settings fail loudly.
    // inputs: Settings JSON with a non-string workspaceColors.colors entry.
    // returns/effects: Panics instead of accepting an invalid palette.
    #[test]
    #[should_panic(expected = "workspaceColors.colors.Red must be a #RRGGBB color")]
    fn load_from_path_rejects_invalid_workspace_color_map() {
        let dir = TempDir::new().expect("temp dir");
        let path = settings_path_in(dir.path());
        fs::create_dir_all(path.parent().expect("config dir")).expect("create config dir");
        fs::write(&path, r#"{"workspaceColors":{"colors":{"Red":7}}}"#).expect("write config");

        let _ = load_from_path(&path);
    }

    // purpose: Verify malformed CMUX sidebar appearance values fail loudly.
    // inputs: Settings JSON with an unsupported sidebar material.
    // returns/effects: Panics instead of accepting an invalid material setting.
    #[test]
    #[should_panic(expected = "sidebarAppearance.material must be a supported sidebar material")]
    fn load_from_path_rejects_invalid_sidebar_appearance_material() {
        let dir = TempDir::new().expect("temp dir");
        let path = settings_path_in(dir.path());
        fs::create_dir_all(path.parent().expect("config dir")).expect("create config dir");
        fs::write(
            &path,
            r#"{"sidebarAppearance":{"material":"transparentGlass"}}"#,
        )
        .expect("write config");

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
        config.app.keep_workspace_open_when_closing_last_surface = true;
        config.app.workspace_inherit_working_directory = false;
        config.app.focus_pane_on_first_click = true;
        config.app.language = AppLanguage::PtBr;
        config.app.app_icon = AppIconMode::Dark;
        config.app.window_title_template = "{workspace}".to_string();
        config.app.menu_bar_only = true;
        config.app.preferred_editor = "code --reuse-window".to_string();
        config.app.open_supported_files_in_cmux = false;
        config.app.open_markdown_in_cmux_viewer = false;
        config.app.minimal_mode = WorkspacePresentationMode::Minimal;
        config.app.global_font_magnification = 180;
        config.app.i_message_mode = true;
        config.app.reorder_on_notification = false;
        config.app.send_anonymous_telemetry = false;
        config.app.confirm_quit = ConfirmQuitMode::DirtyOnly;
        config.app.warn_before_quit = false;
        config.app.warn_before_closing_tab = false;
        config.app.warn_before_closing_tab_x_button = true;
        config.app.hide_tab_close_button = true;
        config.app.rename_selects_existing_name = false;
        config.app.command_palette_searches_all_surfaces = true;
        config.app.file_drop_default_behavior = FileDropDefaultBehavior::Preview;
        config.app.titlebar_controls_style = 4;
        config.app.workspace_button_fade = WorkspaceButtonFadeMode::Enabled;
        config.app.workspace_titlebar_visibility = false;
        config.app.system_wide_hotkey_enabled = true;
        config.app.dev_window_display = "LG HDR 4K".to_string();
        config.pane_chrome.pane_border_color = "#33AAFF".to_string();
        config.pane_chrome.active_pane_border_color = "#FF9500".to_string();
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
        assert_eq!(
            parsed["app"]["appearance"],
            Value::String("light".to_string())
        );
        assert_eq!(
            parsed["app"]["keepWorkspaceOpenWhenClosingLastSurface"],
            Value::Bool(true)
        );
        assert_eq!(
            parsed["app"]["workspaceInheritWorkingDirectory"],
            Value::Bool(false)
        );
        assert_eq!(parsed["app"]["focusPaneOnFirstClick"], Value::Bool(true));
        assert_eq!(parsed["app"]["language"], "pt-BR");
        assert_eq!(parsed["app"]["appIcon"], "dark");
        assert_eq!(parsed["app"]["windowTitleTemplate"], "{workspace}");
        assert_eq!(parsed["app"]["menuBarOnly"], Value::Bool(true));
        assert_eq!(parsed["app"]["preferredEditor"], "code --reuse-window");
        assert_eq!(
            parsed["app"]["openSupportedFilesInCmux"],
            Value::Bool(false)
        );
        assert_eq!(
            parsed["app"]["openMarkdownInCmuxViewer"],
            Value::Bool(false)
        );
        assert_eq!(parsed["app"]["minimalMode"], "minimal");
        assert_eq!(parsed["app"]["globalFontMagnification"], 180);
        assert_eq!(parsed["app"]["iMessageMode"], Value::Bool(true));
        assert_eq!(parsed["app"]["reorderOnNotification"], Value::Bool(false));
        assert_eq!(parsed["app"]["sendAnonymousTelemetry"], Value::Bool(false));
        assert_eq!(parsed["app"]["confirmQuit"], "dirty-only");
        assert_eq!(parsed["app"]["warnBeforeQuit"], Value::Bool(false));
        assert_eq!(parsed["app"]["warnBeforeClosingTab"], Value::Bool(false));
        assert_eq!(
            parsed["app"]["warnBeforeClosingTabXButton"],
            Value::Bool(true)
        );
        assert_eq!(parsed["app"]["hideTabCloseButton"], Value::Bool(true));
        assert_eq!(
            parsed["app"]["renameSelectsExistingName"],
            Value::Bool(false)
        );
        assert_eq!(
            parsed["app"]["commandPaletteSearchesAllSurfaces"],
            Value::Bool(true)
        );
        assert_eq!(parsed["app"]["fileDropDefaultBehavior"], "preview");
        assert_eq!(parsed["app"]["titlebarControlsStyle"], 4);
        assert_eq!(parsed["app"]["workspaceButtonFade"], "enabled");
        assert_eq!(
            parsed["app"]["workspaceTitlebarVisibility"],
            Value::Bool(false)
        );
        assert_eq!(parsed["app"]["systemWideHotkeyEnabled"], Value::Bool(true));
        assert_eq!(parsed["app"]["devWindowDisplay"], "LG HDR 4K");
        assert_eq!(parsed["paneBorderColor"], "#33AAFF");
        assert_eq!(parsed["activePaneBorderColor"], "#FF9500");
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
        config.notifications.dock_badge = false;
        config.notifications.show_in_menu_bar = false;
        config.notifications.unread_pane_ring = false;
        config.notifications.pane_flash = false;
        config.notifications.sound = NotificationSound::Alert;
        config.notifications.custom_sound_file_path = "/tmp/notify.wav".to_string();
        config.notifications.command = "printf done".to_string();
        config.notifications.hooks_mode = NotificationHooksMode::Replace;
        config.notifications.agent_permission_prompt = false;
        config.notifications.agent_turn_complete = AgentTurnCompleteMode::Never;
        config.notifications.agent_idle_reminder = false;
        config.notifications.suppress_only_focused_surface = true;
        save_to_path(&path, &config).expect("save notifications");

        let raw = fs::read_to_string(&path).expect("read config");
        let parsed: Value = serde_json::from_str(&raw).expect("parse config");
        assert_eq!(parsed["notifications"]["enabled"], Value::Bool(false));
        assert_eq!(parsed["notifications"]["dockBadge"], Value::Bool(false));
        assert_eq!(parsed["notifications"]["showInMenuBar"], Value::Bool(false));
        assert_eq!(
            parsed["notifications"]["unreadPaneRing"],
            Value::Bool(false)
        );
        assert_eq!(parsed["notifications"]["paneFlash"], Value::Bool(false));
        assert_eq!(
            parsed["notifications"]["sound"],
            Value::String("alert".to_string())
        );
        assert_eq!(
            parsed["notifications"]["customSoundFilePath"],
            Value::String("/tmp/notify.wav".to_string())
        );
        assert_eq!(
            parsed["notifications"]["command"],
            Value::String("printf done".to_string())
        );
        assert_eq!(
            parsed["notifications"]["hooksMode"],
            Value::String("replace".to_string())
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
        assert_eq!(
            parsed["notifications"]["suppressOnlyFocusedSurface"],
            Value::Bool(true)
        );
    }

    #[test]
    fn save_to_path_writes_sidebar_preferences() {
        let dir = TempDir::new().expect("temp dir");
        let path = settings_path_in(dir.path());
        fs::create_dir_all(path.parent().expect("config dir")).expect("create config dir");
        fs::write(&path, br#"{"app":{"appearance":"dark"}}"#).expect("write config");

        let mut config = AppConfig::default();
        config.sidebar.hide_all_details = true;
        config.sidebar.wrap_workspace_titles = true;
        config.sidebar.show_workspace_description = false;
        config.sidebar.show_notification_message = false;
        config.sidebar.show_branch_directory = false;
        config.sidebar.branch_layout = SidebarBranchLayout::Inline;
        config.sidebar.show_pull_requests = false;
        config.sidebar.watch_git_status = false;
        config.sidebar.show_ports = false;
        config.sidebar.make_pull_requests_clickable = false;
        config.sidebar.open_pull_request_links_in_cmux_browser = false;
        config.sidebar.open_port_links_in_cmux_browser = false;
        config.sidebar.show_ssh = false;
        config.sidebar.show_custom_metadata = false;
        config.sidebar.show_progress = false;
        config.sidebar.show_log = false;
        save_to_path(&path, &config).expect("save sidebar");

        let raw = fs::read_to_string(&path).expect("read config");
        let parsed: Value = serde_json::from_str(&raw).expect("parse config");
        assert_eq!(
            parsed["app"]["appearance"],
            Value::String("system".to_string())
        );
        assert_eq!(
            parsed["sidebar"]["showNotificationMessage"],
            Value::Bool(false)
        );
        assert_eq!(parsed["sidebar"]["hideAllDetails"], Value::Bool(true));
        assert_eq!(parsed["sidebar"]["wrapWorkspaceTitles"], Value::Bool(true));
        assert_eq!(
            parsed["sidebar"]["showWorkspaceDescription"],
            Value::Bool(false)
        );
        assert_eq!(parsed["sidebar"]["showBranchDirectory"], Value::Bool(false));
        assert_eq!(
            parsed["sidebar"]["branchLayout"],
            Value::String("inline".to_string())
        );
        assert_eq!(parsed["sidebar"]["showPullRequests"], Value::Bool(false));
        assert_eq!(parsed["sidebar"]["watchGitStatus"], Value::Bool(false));
        assert_eq!(parsed["sidebar"]["showPorts"], Value::Bool(false));
        assert_eq!(
            parsed["sidebar"]["makePullRequestsClickable"],
            Value::Bool(false)
        );
        assert_eq!(
            parsed["sidebar"]["openPullRequestLinksInCmuxBrowser"],
            Value::Bool(false)
        );
        assert_eq!(
            parsed["sidebar"]["openPortLinksInCmuxBrowser"],
            Value::Bool(false)
        );
        assert_eq!(parsed["sidebar"]["showSSH"], Value::Bool(false));
        assert_eq!(parsed["sidebar"]["showCustomMetadata"], Value::Bool(false));
        assert_eq!(parsed["sidebar"]["showProgress"], Value::Bool(false));
        assert_eq!(parsed["sidebar"]["showLog"], Value::Bool(false));
    }

    // purpose: Verify saving writes optional CMUX right sidebar maximum width.
    // inputs: AppConfig with sidebar.right_max_width configured.
    // returns/effects: Persists sidebar.rightMaxWidth while preserving sibling config.
    #[test]
    fn save_to_path_writes_sidebar_right_max_width() {
        let dir = TempDir::new().expect("temp dir");
        let path = settings_path_in(dir.path());
        fs::create_dir_all(path.parent().expect("config dir")).expect("create config dir");
        fs::write(&path, br#"{"app":{"appearance":"dark"}}"#).expect("write config");

        let mut config = AppConfig::default();
        config.sidebar.right_max_width = Some(1500);
        save_to_path(&path, &config).expect("save sidebar width");

        let raw = fs::read_to_string(&path).expect("read config");
        let parsed: Value = serde_json::from_str(&raw).expect("parse config");
        assert_eq!(
            parsed["sidebar"]["rightMaxWidth"],
            Value::Number(1500.into())
        );
    }

    // purpose: Verify saving writes CMUX workspace color and sidebar appearance sections.
    // inputs: AppConfig with non-default workspace color and sidebar appearance settings.
    // returns/effects: Persists sections while preserving unrelated sibling config.
    #[test]
    fn save_to_path_writes_workspace_colors_and_sidebar_appearance() {
        let dir = TempDir::new().expect("temp dir");
        let path = settings_path_in(dir.path());
        fs::create_dir_all(path.parent().expect("config dir")).expect("create config dir");
        fs::write(
            &path,
            br#"{"app":{"appearance":"dark"},"custom":{"keep":true}}"#,
        )
        .expect("write config");

        let mut config = AppConfig::default();
        config.workspace_colors.indicator_style = WorkspaceIndicatorStyle::SolidFill;
        config.workspace_colors.selection_color = "#336699".to_string();
        config.workspace_colors.notification_badge_color = "#AA5500".to_string();
        config
            .workspace_colors
            .colors
            .insert("Red".to_string(), "#C0392B".to_string());
        config
            .workspace_colors
            .palette_overrides
            .insert("Blue".to_string(), "#1565C0".to_string());
        config
            .workspace_colors
            .custom_colors
            .push("#112233".to_string());
        config.sidebar_appearance.match_terminal_background = true;
        config.sidebar_appearance.tint_color = "#102030".to_string();
        config.sidebar_appearance.dark_mode_tint_color = "#445566".to_string();
        config.sidebar_appearance.tint_opacity = 0.0;
        config.sidebar_appearance.blur_opacity = 0.6;
        config.sidebar_appearance.corner_radius = 12.5;
        config.sidebar_appearance.preset = SidebarPresetOption::Custom;
        config.sidebar_appearance.material = SidebarMaterialOption::HudWindow;
        config.sidebar_appearance.blend_mode = SidebarBlendModeOption::BehindWindow;
        config.sidebar_appearance.state = SidebarStateOption::Inactive;
        save_to_path(&path, &config).expect("save sidebar appearance");

        let raw = fs::read_to_string(&path).expect("read config");
        let parsed: Value = serde_json::from_str(&raw).expect("parse config");
        assert_eq!(parsed["workspaceColors"]["indicatorStyle"], "solidFill");
        assert_eq!(parsed["workspaceColors"]["selectionColor"], "#336699");
        assert_eq!(
            parsed["workspaceColors"]["notificationBadgeColor"],
            "#AA5500"
        );
        assert_eq!(parsed["workspaceColors"]["colors"]["Red"], "#C0392B");
        assert_eq!(
            parsed["workspaceColors"]["paletteOverrides"]["Blue"],
            "#1565C0"
        );
        assert_eq!(parsed["workspaceColors"]["customColors"][0], "#112233");
        assert_eq!(
            parsed["sidebarAppearance"]["matchTerminalBackground"],
            Value::Bool(true)
        );
        assert_eq!(parsed["sidebarAppearance"]["tintColor"], "#102030");
        assert_eq!(parsed["sidebarAppearance"]["darkModeTintColor"], "#445566");
        assert_eq!(parsed["sidebarAppearance"]["tintOpacity"], 0.0);
        assert_eq!(parsed["sidebarAppearance"]["blurOpacity"], 0.6);
        assert_eq!(parsed["sidebarAppearance"]["cornerRadius"], 12.5);
        assert_eq!(parsed["sidebarAppearance"]["preset"], "custom");
        assert_eq!(parsed["sidebarAppearance"]["material"], "hudWindow");
        assert_eq!(parsed["sidebarAppearance"]["blendMode"], "behindWindow");
        assert_eq!(parsed["sidebarAppearance"]["state"], "inactive");
        assert_eq!(parsed["custom"]["keep"], Value::Bool(true));
    }

    // purpose: Verify saving writes the CMUX terminal auto-resume setting without dropping siblings.
    // inputs: Existing settings JSON with an unrelated terminal key and AppConfig false setting.
    // returns/effects: Writes settings and asserts terminal.autoResumeAgentSessions is persisted.
    #[test]
    fn save_to_path_writes_terminal_auto_resume_agent_sessions() {
        let dir = TempDir::new().expect("temp dir");
        let path = settings_path_in(dir.path());
        fs::create_dir_all(path.parent().expect("config dir")).expect("create config dir");
        fs::write(&path, r#"{"terminal":{"bell":true}}"#).expect("write existing config");

        let mut config = AppConfig::default();
        config.terminal.auto_resume_agent_sessions = false;
        save_to_path(&path, &config).expect("save terminal config");

        let raw = fs::read_to_string(&path).expect("read config");
        let parsed: Value = serde_json::from_str(&raw).expect("parse config");
        assert_eq!(
            parsed["terminal"]["autoResumeAgentSessions"],
            Value::Bool(false)
        );
        assert_eq!(parsed["terminal"]["bell"], Value::Bool(true));
    }

    // purpose: Verify host saving writes remaining CMUX terminal/custom-sidebar settings.
    // inputs: Existing settings JSON and AppConfig values overriding CMUX defaults.
    // returns/effects: Persists nested settings while preserving sibling values.
    #[test]
    fn save_to_path_writes_terminal_custom_sidebar_and_beta_settings() {
        let dir = TempDir::new().expect("temp dir");
        let path = settings_path_in(dir.path());
        fs::create_dir_all(path.parent().expect("config dir")).expect("create config dir");
        fs::write(
            &path,
            r#"{"terminal":{"bell":true},"rightSidebar":{"keep":"yes"}}"#,
        )
        .expect("write existing config");

        let mut config = AppConfig::default();
        config.terminal.scroll_speed = 2.5;
        config.terminal.agent_hibernation.enabled = true;
        config.terminal.resume_commands = vec!["codex".to_string()];
        config.account.pii_display_mode = PiiDisplayMode::Hidden;
        config.account.selected_team_id = "team_123".to_string();
        config.account.welcome_shown = true;
        config.integrations.claude_code_hooks_enabled = false;
        config.integrations.claude_code_custom_claude_path = "/opt/claude".to_string();
        config.integrations.kiro_notification_level = KiroNotificationLevel::Verbose;
        config.integrations.ripgrep_custom_binary_path = "/usr/local/bin/rg".to_string();
        config.integrations.suppress_subagent_notifications = false;
        config.automation.socket_control_mode = SocketControlMode::Password;
        config.automation.workspace_auto_naming = true;
        config.automation.auto_naming_agent = "codex".to_string();
        config.automation.port_base = 9200;
        config.automation.port_range = 24;
        config.mobile.ios_pairing_host_enabled = true;
        config.mobile.ios_pairing_host_port = 58466;
        config.mobile.ios_pairing_host_display_name = "Dev Linux".to_string();
        config.markdown.font_size = 18;
        config.markdown.font_family = "Inter".to_string();
        config.markdown.max_width = 1200;
        config.file_editor.word_wrap = true;
        config.canvas.pane_gap = 24;
        config.canvas.snapping_enabled = false;
        config.custom_sidebars.renderer = CustomSidebarRendererMode::Remote;
        config.custom_sidebars.beta_enabled = false;
        config.beta_features.right_sidebar_feed_enabled = true;
        config.beta_features.extensions_enabled = true;
        save_to_path(&path, &config).expect("save config");

        let raw = fs::read_to_string(&path).expect("read config");
        let parsed: Value = serde_json::from_str(&raw).expect("parse config");
        assert_eq!(parsed["terminal"]["scrollSpeed"], 2.5);
        assert_eq!(
            parsed["terminal"]["agentHibernation"]["enabled"],
            Value::Bool(true)
        );
        assert_eq!(parsed["terminal"]["resumeCommands"][0], "codex");
        assert_eq!(parsed["terminal"]["bell"], Value::Bool(true));
        assert_eq!(parsed["account"]["piiDisplayMode"], "hidden");
        assert_eq!(parsed["account"]["selectedTeamID"], "team_123");
        assert_eq!(parsed["account"]["welcomeShown"], Value::Bool(true));
        assert_eq!(
            parsed["integrations"]["claudeCode"]["hooksEnabled"],
            Value::Bool(false)
        );
        assert_eq!(
            parsed["integrations"]["claudeCode"]["customClaudePath"],
            "/opt/claude"
        );
        assert_eq!(
            parsed["integrations"]["kiro"]["notificationLevel"],
            "verbose"
        );
        assert_eq!(
            parsed["integrations"]["ripgrep"]["customBinaryPath"],
            "/usr/local/bin/rg"
        );
        assert_eq!(
            parsed["integrations"]["suppressSubagentNotifications"],
            Value::Bool(false)
        );
        assert_eq!(parsed["automation"]["socketControlMode"], "password");
        assert_eq!(
            parsed["automation"]["workspaceAutoNaming"],
            Value::Bool(true)
        );
        assert_eq!(parsed["automation"]["autoNamingAgent"], "codex");
        assert_eq!(parsed["automation"]["portBase"], 9200);
        assert_eq!(parsed["automation"]["portRange"], 24);
        assert_eq!(
            parsed["mobile"]["iOSPairingHost"]["enabled"],
            Value::Bool(true)
        );
        assert_eq!(parsed["mobile"]["iOSPairingHost"]["port"], 58466);
        assert_eq!(
            parsed["mobile"]["iOSPairingHost"]["displayName"],
            "Dev Linux"
        );
        assert_eq!(parsed["markdown"]["fontSize"], 18);
        assert_eq!(parsed["markdown"]["fontFamily"], "Inter");
        assert_eq!(parsed["markdown"]["maxWidth"], 1200);
        assert_eq!(parsed["fileEditor"]["wordWrap"], Value::Bool(true));
        assert_eq!(parsed["canvas"]["paneGap"], 24);
        assert_eq!(parsed["canvas"]["snappingEnabled"], Value::Bool(false));
        assert_eq!(parsed["customSidebars"]["renderer"], "remote");
        assert_eq!(
            parsed["customSidebars"]["beta"]["enabled"],
            Value::Bool(false)
        );
        assert_eq!(
            parsed["rightSidebar"]["beta"]["feed"]["enabled"],
            Value::Bool(true)
        );
        assert_eq!(parsed["rightSidebar"]["keep"], "yes");
        assert_eq!(parsed["extensions"]["beta"]["enabled"], Value::Bool(true));
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
