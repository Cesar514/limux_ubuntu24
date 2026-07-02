use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::Path;
use std::path::PathBuf;
use std::time::{SystemTime, UNIX_EPOCH};

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum AgentKind {
    Claude,
    Codex,
    Grok,
    OpenCode,
    Cursor,
    Kiro,
    Antigravity,
    RovoDev,
    Pi,
    Omp,
    Amp,
    HermesAgent,
    Gemini,
    Copilot,
    CodeBuddy,
    Factory,
    Qoder,
}

impl AgentKind {
    pub(crate) fn all() -> [Self; 17] {
        [
            Self::Claude,
            Self::Codex,
            Self::Grok,
            Self::OpenCode,
            Self::Cursor,
            Self::Kiro,
            Self::Antigravity,
            Self::RovoDev,
            Self::Pi,
            Self::Omp,
            Self::Amp,
            Self::HermesAgent,
            Self::Gemini,
            Self::Copilot,
            Self::CodeBuddy,
            Self::Factory,
            Self::Qoder,
        ]
    }

    pub(crate) fn from_hook_name(name: &str) -> Option<Self> {
        match name.trim().to_ascii_lowercase().as_str() {
            "claude" | "claude-code" | "claudecode" => Some(Self::Claude),
            "codex" => Some(Self::Codex),
            "grok" => Some(Self::Grok),
            "opencode" | "open-code" => Some(Self::OpenCode),
            "cursor" | "cursor-agent" => Some(Self::Cursor),
            "kiro" | "kiro-cli" => Some(Self::Kiro),
            "antigravity" | "agy" => Some(Self::Antigravity),
            "rovodev" | "rovo" | "acli" => Some(Self::RovoDev),
            "pi" | "pi-coding-agent" => Some(Self::Pi),
            "omp" => Some(Self::Omp),
            "amp" => Some(Self::Amp),
            "hermes" | "hermes-agent" => Some(Self::HermesAgent),
            "gemini" => Some(Self::Gemini),
            "copilot" => Some(Self::Copilot),
            "codebuddy" | "code-buddy" => Some(Self::CodeBuddy),
            "factory" | "droid" => Some(Self::Factory),
            "qoder" | "qodercli" => Some(Self::Qoder),
            _ => None,
        }
    }

    pub(crate) fn store_name(self) -> &'static str {
        match self {
            Self::Claude => "claude",
            Self::Codex => "codex",
            Self::Grok => "grok",
            Self::OpenCode => "opencode",
            Self::Cursor => "cursor",
            Self::Kiro => "kiro",
            Self::Antigravity => "antigravity",
            Self::RovoDev => "rovodev",
            Self::Pi => "pi",
            Self::Omp => "omp",
            Self::Amp => "amp",
            Self::HermesAgent => "hermes-agent",
            Self::Gemini => "gemini",
            Self::Copilot => "copilot",
            Self::CodeBuddy => "codebuddy",
            Self::Factory => "factory",
            Self::Qoder => "qoder",
        }
    }

    pub(crate) fn label(self) -> &'static str {
        match self {
            Self::Claude => "Claude",
            Self::Codex => "Codex",
            Self::Grok => "Grok",
            Self::OpenCode => "OpenCode",
            Self::Cursor => "Cursor",
            Self::Kiro => "Kiro",
            Self::Antigravity => "Antigravity",
            Self::RovoDev => "Rovo Dev",
            Self::Pi => "Pi",
            Self::Omp => "OMP",
            Self::Amp => "Amp",
            Self::HermesAgent => "Hermes Agent",
            Self::Gemini => "Gemini",
            Self::Copilot => "Copilot",
            Self::CodeBuddy => "CodeBuddy",
            Self::Factory => "Factory",
            Self::Qoder => "Qoder",
        }
    }

    fn fallback_executable(self) -> &'static str {
        match self {
            Self::Claude => "claude",
            Self::Codex => "codex",
            Self::Grok => "grok",
            Self::OpenCode => "opencode",
            Self::Cursor => "cursor-agent",
            Self::Kiro => "kiro-cli",
            Self::Antigravity => "agy",
            Self::RovoDev => "acli",
            Self::Pi => "pi",
            Self::Omp => "omp",
            Self::Amp => "amp",
            Self::HermesAgent => "hermes",
            Self::Gemini => "gemini",
            Self::Copilot => "copilot",
            Self::CodeBuddy => "codebuddy",
            Self::Factory => "droid",
            Self::Qoder => "qodercli",
        }
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub(crate) struct AgentLaunchCommandRecord {
    pub(crate) executable: String,
    pub(crate) arguments: Vec<String>,
    #[serde(default)]
    pub(crate) cwd: Option<String>,
    #[serde(default)]
    pub(crate) environment: BTreeMap<String, String>,
    pub(crate) captured_at: f64,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub(crate) struct AgentHookSessionRecord {
    pub(crate) session_id: String,
    pub(crate) workspace_id: String,
    pub(crate) surface_id: String,
    #[serde(default)]
    pub(crate) cwd: Option<String>,
    #[serde(default)]
    pub(crate) pid: Option<u32>,
    #[serde(default)]
    pub(crate) launch_command: Option<AgentLaunchCommandRecord>,
    pub(crate) updated_at: f64,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
struct AgentHookSessionFile {
    version: u32,
    #[serde(default)]
    sessions: BTreeMap<String, AgentHookSessionRecord>,
}

pub(crate) struct AgentHookSessionStore {
    path: PathBuf,
}

#[derive(Clone, Debug, PartialEq)]
pub(crate) struct AgentHookSessionSnapshot {
    pub(crate) agent: AgentKind,
    pub(crate) path: PathBuf,
    pub(crate) exists: bool,
    pub(crate) session_count: usize,
    pub(crate) records: Vec<AgentHookSessionRecord>,
}

impl AgentHookSessionStore {
    pub(crate) fn new(agent: AgentKind) -> Self {
        Self::new_for_agent_name(agent.store_name())
    }

    pub(crate) fn new_for_agent_name(agent: &str) -> Self {
        let filename = format!("{}-hook-sessions.json", safe_store_name(agent));
        if let Some(dir) = std::env::var_os("LIMUX_AGENT_HOOK_STATE_DIR") {
            let dir = PathBuf::from(dir);
            return Self {
                path: dir.join(filename),
            };
        }
        Self {
            path: state_dir().join(filename),
        }
    }

    #[cfg(test)]
    pub(crate) fn new_for_dir(agent: &str, dir: &Path) -> Self {
        Self {
            path: dir.join(format!("{}-hook-sessions.json", safe_store_name(agent))),
        }
    }

    pub(crate) fn snapshot_for_dir(
        agent: AgentKind,
        dir: &Path,
    ) -> Result<AgentHookSessionSnapshot> {
        let store = Self {
            path: dir.join(format!("{}-hook-sessions.json", agent.store_name())),
        };
        let exists = store.path.exists();
        let file = store.load()?;
        let session_count = file.sessions.len();
        Ok(AgentHookSessionSnapshot {
            agent,
            path: store.path,
            exists,
            session_count,
            records: file.sessions.into_values().collect(),
        })
    }

    pub(crate) fn lookup(&self, session_id: &str) -> Result<Option<AgentHookSessionRecord>> {
        let session_id = normalized(session_id);
        if session_id.is_none() {
            return Ok(None);
        }
        let file = self.load()?;
        Ok(file.sessions.get(session_id.as_deref().unwrap()).cloned())
    }

    pub(crate) fn upsert(&self, record: AgentHookSessionRecord) -> Result<()> {
        let Some(session_id) = normalized(&record.session_id) else {
            return Ok(());
        };
        let mut file = self.load()?;
        file.version = 1;
        file.sessions.insert(session_id, record);
        self.save(&file)
    }

    pub(crate) fn remove(&self, session_id: &str) -> Result<()> {
        let Some(session_id) = normalized(session_id) else {
            return Ok(());
        };
        let mut file = self.load()?;
        file.sessions.remove(&session_id);
        self.save(&file)
    }

    fn load(&self) -> Result<AgentHookSessionFile> {
        if !self.path.exists() {
            return Ok(AgentHookSessionFile {
                version: 1,
                sessions: BTreeMap::new(),
            });
        }
        let raw = fs::read_to_string(&self.path)
            .with_context(|| format!("failed to read {}", self.path.display()))?;
        let mut file: AgentHookSessionFile = serde_json::from_str(&raw)
            .with_context(|| format!("failed to parse {}", self.path.display()))?;
        if file.version == 0 {
            file.version = 1;
        }
        Ok(file)
    }

    fn save(&self, file: &AgentHookSessionFile) -> Result<()> {
        if let Some(parent) = self.path.parent() {
            fs::create_dir_all(parent)
                .with_context(|| format!("failed to create {}", parent.display()))?;
        }
        let temp = self
            .path
            .with_extension(format!("json.{}.tmp", std::process::id()));
        let json = serde_json::to_vec_pretty(file).context("failed to encode hook store")?;
        fs::write(&temp, json).with_context(|| format!("failed to write {}", temp.display()))?;
        fs::rename(&temp, &self.path)
            .with_context(|| format!("failed to replace {}", self.path.display()))?;
        Ok(())
    }
}

pub(crate) fn sanitize_launch_arguments(kind: AgentKind, arguments: &[String]) -> Vec<String> {
    if arguments.is_empty() {
        return vec![kind.fallback_executable().to_string()];
    }

    let mut result = Vec::new();
    let mut index = 0;
    while index < arguments.len() {
        let arg = &arguments[index];
        if index == 0 {
            result.push(arg.clone());
            index += 1;
            continue;
        }
        if is_resume_selector(kind, arg) {
            index += 1;
            if index < arguments.len() && !arguments[index].starts_with('-') {
                index += 1;
            }
            continue;
        }
        if kind == AgentKind::Kiro && arg == "chat" {
            index += 1;
            continue;
        }
        if kind == AgentKind::RovoDev && matches!(arg.as_str(), "rovodev" | "run") {
            index += 1;
            continue;
        }
        if kind == AgentKind::Amp && amp_resume_subcommand_at(arguments, index) {
            index += 3;
            continue;
        }
        if option_takes_secret_value(arg) {
            index += 1;
            if index < arguments.len() && !arguments[index].starts_with('-') {
                index += 1;
            }
            continue;
        }
        if option_is_secret_assignment(arg) {
            index += 1;
            continue;
        }
        if option_takes_safe_value(arg) {
            result.push(arg.clone());
            if index + 1 < arguments.len() {
                result.push(arguments[index + 1].clone());
                index += 2;
            } else {
                index += 1;
            }
            continue;
        }
        if option_is_safe_flag_or_assignment(arg) {
            result.push(arg.clone());
            index += 1;
            continue;
        }
        if arg.starts_with('-') {
            index += 1;
            continue;
        }
        break;
    }

    if result.is_empty() {
        vec![kind.fallback_executable().to_string()]
    } else {
        result
    }
}

#[cfg(test)]
pub(crate) fn build_resume_command(
    kind: AgentKind,
    session_id: &str,
    launch: Option<&AgentLaunchCommandRecord>,
    cwd: Option<&str>,
) -> Option<String> {
    let session_id = normalized(session_id)?;
    let fallback = kind.fallback_executable().to_string();
    let raw_args = launch
        .map(|launch| launch.arguments.clone())
        .filter(|args| !args.is_empty())
        .unwrap_or_else(|| vec![fallback.clone()]);
    let sanitized = sanitize_launch_arguments(kind, &raw_args);
    let executable = launch
        .and_then(|launch| normalized(&launch.executable))
        .or_else(|| sanitized.first().cloned())
        .unwrap_or(fallback);
    let preserved_tail = sanitized
        .get(1..)
        .map(|tail| tail.to_vec())
        .unwrap_or_default();

    let mut parts = vec![executable];
    match kind {
        AgentKind::Codex => {
            parts.push("resume".to_string());
            parts.extend(preserved_tail);
            parts.push(session_id);
        }
        AgentKind::OpenCode => {
            parts.push("--session".to_string());
            parts.push(session_id);
            parts.extend(preserved_tail);
        }
        AgentKind::Grok => {
            parts.push("-r".to_string());
            parts.push(session_id);
            parts.extend(preserved_tail);
        }
        AgentKind::Kiro => {
            parts.push("chat".to_string());
            parts.push("--resume-id".to_string());
            parts.push(session_id);
            parts.extend(preserved_tail);
        }
        AgentKind::Antigravity => {
            parts.extend(preserved_tail);
        }
        AgentKind::RovoDev => {
            parts.push("rovodev".to_string());
            parts.push("run".to_string());
            parts.push("--restore".to_string());
            parts.push(session_id);
            parts.extend(preserved_tail);
        }
        AgentKind::Pi => {
            parts.push("--session".to_string());
            parts.push(session_id);
            parts.extend(preserved_tail);
        }
        AgentKind::Omp => {
            parts.push("--session".to_string());
            parts.push(session_id);
            parts.extend(preserved_tail);
        }
        AgentKind::Amp => {
            parts.push("threads".to_string());
            parts.push("continue".to_string());
            parts.push(session_id);
            parts.extend(preserved_tail);
        }
        AgentKind::HermesAgent => {
            parts.push("--resume".to_string());
            parts.push(session_id);
            parts.extend(preserved_tail);
        }
        AgentKind::Claude
        | AgentKind::Cursor
        | AgentKind::Gemini
        | AgentKind::Copilot
        | AgentKind::CodeBuddy
        | AgentKind::Factory
        | AgentKind::Qoder => {
            parts.push("--resume".to_string());
            parts.push(session_id);
            parts.extend(preserved_tail);
        }
    }

    let command = parts
        .iter()
        .map(|part| shell_single_quote(part))
        .collect::<Vec<_>>()
        .join(" ");
    let cwd = cwd.and_then(normalized).or_else(|| {
        launch
            .and_then(|launch| launch.cwd.as_deref())
            .and_then(normalized)
    });
    Some(match cwd {
        Some(cwd) => format!("cd {} && {command}", shell_single_quote(&cwd)),
        None => command,
    })
}

pub(crate) fn launch_record_from_env(
    kind: AgentKind,
    payload_cwd: Option<&str>,
) -> Option<AgentLaunchCommandRecord> {
    let args = launch_argv_from_env()
        .filter(|args| !args.is_empty())
        .unwrap_or_else(|| vec![kind.fallback_executable().to_string()]);
    let sanitized = sanitize_launch_arguments(kind, &args);
    let executable = first_env_value(&[
        "LIMUX_AGENT_LAUNCH_EXECUTABLE",
        "CMUX_AGENT_LAUNCH_EXECUTABLE",
    ])
    .and_then(|value| normalized(&value))
    .or_else(|| sanitized.first().cloned())?;
    Some(AgentLaunchCommandRecord {
        executable,
        arguments: sanitized,
        cwd: first_env_value(&["LIMUX_AGENT_LAUNCH_CWD", "CMUX_AGENT_LAUNCH_CWD"])
            .and_then(|value| normalized(&value))
            .or_else(|| payload_cwd.and_then(normalized))
            .or_else(|| {
                std::env::var("PWD")
                    .ok()
                    .and_then(|value| normalized(&value))
            }),
        environment: selected_environment(),
        captured_at: now_seconds(),
    })
}

fn launch_argv_from_env() -> Option<Vec<String>> {
    first_env_value(&["LIMUX_AGENT_LAUNCH_ARGV_B64", "CMUX_AGENT_LAUNCH_ARGV_B64"])
        .and_then(|raw| decode_base64_nul_separated(&raw))
        .or_else(|| {
            first_env_value(&["LIMUX_AGENT_LAUNCH_ARGV", "CMUX_AGENT_LAUNCH_ARGV"])
                .map(split_nul_or_space_separated)
        })
}

fn first_env_value(names: &[&str]) -> Option<String> {
    names.iter().find_map(|name| {
        std::env::var(name)
            .ok()
            .and_then(|value| normalized(&value))
    })
}

fn decode_base64_nul_separated(raw: &str) -> Option<Vec<String>> {
    let bytes = decode_base64_standard(raw.trim())?;
    Some(
        bytes
            .split(|byte| *byte == 0)
            .filter(|part| !part.is_empty())
            .filter_map(|part| std::str::from_utf8(part).ok())
            .filter_map(normalized)
            .collect::<Vec<_>>(),
    )
}

fn decode_base64_standard(raw: &str) -> Option<Vec<u8>> {
    let mut out = Vec::new();
    let mut chunk = [0u8; 4];
    let mut chunk_len = 0;
    for byte in raw.bytes().filter(|byte| !byte.is_ascii_whitespace()) {
        let value = match byte {
            b'A'..=b'Z' => byte - b'A',
            b'a'..=b'z' => byte - b'a' + 26,
            b'0'..=b'9' => byte - b'0' + 52,
            b'+' => 62,
            b'/' => 63,
            b'=' => 64,
            _ => return None,
        };
        chunk[chunk_len] = value;
        chunk_len += 1;
        if chunk_len == 4 {
            if chunk[0] == 64 || chunk[1] == 64 {
                return None;
            }
            out.push((chunk[0] << 2) | (chunk[1] >> 4));
            if chunk[2] != 64 {
                out.push((chunk[1] << 4) | (chunk[2] >> 2));
            }
            if chunk[3] != 64 {
                out.push((chunk[2] << 6) | chunk[3]);
            }
            chunk_len = 0;
        }
    }
    (chunk_len == 0).then_some(out)
}

pub(crate) fn now_seconds() -> f64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_secs_f64())
        .unwrap_or_default()
}

fn state_dir() -> PathBuf {
    if let Some(dir) = dirs::state_dir() {
        return dir.join("limux");
    }
    if let Some(home) = dirs::home_dir() {
        return home.join(".local/state/limux");
    }
    PathBuf::from(".limux")
}

pub(crate) fn default_state_dir() -> PathBuf {
    if let Some(dir) = std::env::var_os("LIMUX_AGENT_HOOK_STATE_DIR") {
        return PathBuf::from(dir);
    }
    state_dir()
}

fn safe_store_name(agent: &str) -> String {
    agent
        .chars()
        .filter(|ch| ch.is_ascii_alphanumeric() || *ch == '-' || *ch == '_')
        .collect::<String>()
}

fn normalized(value: &str) -> Option<String> {
    let trimmed = value.trim();
    (!trimmed.is_empty()).then(|| trimmed.to_string())
}

#[cfg(test)]
fn shell_single_quote(value: &str) -> String {
    format!("'{}'", value.replace('\'', "'\\''"))
}

fn is_resume_selector(kind: AgentKind, arg: &str) -> bool {
    match kind {
        AgentKind::Codex => arg == "resume" || arg == "--resume" || arg.starts_with("--resume="),
        AgentKind::OpenCode => arg == "--session" || arg.starts_with("--session="),
        AgentKind::Grok => arg == "-r" || arg == "--resume" || arg.starts_with("--resume="),
        AgentKind::Kiro => arg == "--resume-id" || arg.starts_with("--resume-id="),
        AgentKind::Antigravity => false,
        AgentKind::RovoDev => arg == "--restore" || arg.starts_with("--restore="),
        AgentKind::Pi => {
            arg == "--session"
                || arg == "-s"
                || arg == "--resume"
                || arg == "--fork"
                || arg.starts_with("--session=")
                || arg.starts_with("--resume=")
                || arg.starts_with("--fork=")
        }
        AgentKind::Omp => arg == "--session" || arg.starts_with("--session="),
        AgentKind::Amp => false,
        AgentKind::HermesAgent => arg == "--resume" || arg.starts_with("--resume="),
        AgentKind::Claude
        | AgentKind::Cursor
        | AgentKind::Gemini
        | AgentKind::Copilot
        | AgentKind::CodeBuddy
        | AgentKind::Factory
        | AgentKind::Qoder => {
            arg == "--resume" || arg.starts_with("--resume=") || arg == "--continue"
        }
    }
}

fn amp_resume_subcommand_at(arguments: &[String], index: usize) -> bool {
    index + 2 < arguments.len()
        && arguments[index] == "threads"
        && arguments[index + 1] == "continue"
}

fn option_takes_secret_value(arg: &str) -> bool {
    matches!(
        arg,
        "--api-key"
            | "--apikey"
            | "--token"
            | "--auth-token"
            | "--password"
            | "--credential"
            | "--credentials"
    )
}

fn option_is_secret_assignment(arg: &str) -> bool {
    let lower = arg.to_ascii_lowercase();
    lower.starts_with("--api-key=")
        || lower.starts_with("--apikey=")
        || lower.starts_with("--token=")
        || lower.starts_with("--auth-token=")
        || lower.starts_with("--password=")
        || lower.starts_with("--credential=")
        || lower.starts_with("--credentials=")
}

fn option_takes_safe_value(arg: &str) -> bool {
    matches!(
        arg,
        "--model"
            | "-m"
            | "--config"
            | "-c"
            | "--profile"
            | "--sandbox"
            | "--thinking"
            | "--provider"
            | "--extension"
            | "-e"
            | "--skill"
            | "--mcp-config"
            | "--permission-mode"
            | "--session-dir"
            | "--dir"
            | "--trust"
            | "--approval-policy"
            | "--cwd"
            | "--cd"
            | "--working-directory"
            | "--config-dir"
            | "--home"
            | "--agent"
    )
}

fn option_is_safe_flag_or_assignment(arg: &str) -> bool {
    if matches!(
        arg,
        "--dangerously-bypass-approvals-and-sandbox"
            | "--dangerously-skip-permissions"
            | "--full-auto"
            | "--search"
            | "--no-search"
            | "--yolo"
    ) {
        return true;
    }

    let Some((name, _)) = arg.split_once('=') else {
        return false;
    };
    option_takes_safe_value(name)
}

fn split_nul_or_space_separated(raw: String) -> Vec<String> {
    if raw.contains('\0') {
        raw.split('\0').filter_map(normalized).collect::<Vec<_>>()
    } else {
        raw.split_whitespace()
            .filter_map(normalized)
            .collect::<Vec<_>>()
    }
}

fn selected_environment() -> BTreeMap<String, String> {
    let allowlist: BTreeSet<&'static str> = [
        "CODEX_HOME",
        "CLAUDE_CONFIG_DIR",
        "GROK_HOME",
        "OPENCODE_CONFIG_DIR",
        "GEMINI_CONFIG_DIR",
        "COPILOT_HOME",
        "CODEBUDDY_CONFIG_DIR",
        "PI_CODING_AGENT_DIR",
        "PI_CONFIG_DIR",
        "AMP_CONFIG_DIR",
        "HERMES_HOME",
        "HERMES_CODEX_BASE_URL",
        "CUSTOM_BASE_URL",
        "QODER_CONFIG_DIR",
        "ANTHROPIC_BASE_URL",
        "ANTHROPIC_MODEL",
        "ANTHROPIC_SMALL_FAST_MODEL",
    ]
    .into_iter()
    .collect();

    std::env::vars()
        .filter(|(key, value)| allowlist.contains(key.as_str()) && !value.trim().is_empty())
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    #[test]
    fn hook_store_round_trips_session_records() {
        let dir = tempdir().expect("tempdir");
        let store = AgentHookSessionStore::new_for_dir("codex", dir.path());
        let record = AgentHookSessionRecord {
            session_id: "codex-session-1".to_string(),
            workspace_id: "workspace-1".to_string(),
            surface_id: "7:tab-a".to_string(),
            cwd: Some("/tmp/project".to_string()),
            pid: Some(1234),
            launch_command: Some(AgentLaunchCommandRecord {
                executable: "codex".to_string(),
                arguments: vec![
                    "codex".to_string(),
                    "--model".to_string(),
                    "gpt-5.5".to_string(),
                ],
                cwd: Some("/tmp/project".to_string()),
                environment: Default::default(),
                captured_at: 10.0,
            }),
            updated_at: 11.0,
        };

        store.upsert(record.clone()).expect("upsert");

        assert_eq!(
            store.lookup("codex-session-1").expect("lookup"),
            Some(record)
        );
    }

    #[test]
    fn sanitizer_drops_prompts_credentials_and_existing_resume_selectors() {
        let args = vec![
            "codex".to_string(),
            "--model".to_string(),
            "gpt-5.5".to_string(),
            "--api-key".to_string(),
            "secret".to_string(),
            "resume".to_string(),
            "old-session".to_string(),
            "--dangerously-bypass-approvals-and-sandbox".to_string(),
            "write a prompt".to_string(),
        ];

        let sanitized = sanitize_launch_arguments(AgentKind::Codex, &args);

        assert_eq!(
            sanitized,
            vec![
                "codex".to_string(),
                "--model".to_string(),
                "gpt-5.5".to_string(),
                "--dangerously-bypass-approvals-and-sandbox".to_string(),
            ]
        );
    }

    #[test]
    fn resume_command_preserves_safe_launch_flags_and_cwd() {
        let launch = AgentLaunchCommandRecord {
            executable: "codex".to_string(),
            arguments: vec![
                "codex".to_string(),
                "--model".to_string(),
                "gpt-5.5".to_string(),
                "--config".to_string(),
                "profile=work".to_string(),
            ],
            cwd: Some("/tmp/project one".to_string()),
            environment: Default::default(),
            captured_at: 20.0,
        };

        let command = build_resume_command(
            AgentKind::Codex,
            "sess-123",
            Some(&launch),
            Some("/tmp/project one"),
        )
        .expect("resume command");

        assert_eq!(
            command,
            "cd '/tmp/project one' && 'codex' 'resume' '--model' 'gpt-5.5' '--config' 'profile=work' 'sess-123'"
        );
    }

    #[test]
    fn grok_resume_command_uses_native_resume_flag() {
        let launch = AgentLaunchCommandRecord {
            executable: "grok".to_string(),
            arguments: vec![
                "grok".to_string(),
                "-r".to_string(),
                "old-session".to_string(),
                "--model".to_string(),
                "fast".to_string(),
            ],
            cwd: None,
            environment: Default::default(),
            captured_at: 30.0,
        };

        let command = build_resume_command(AgentKind::Grok, "new-session", Some(&launch), None)
            .expect("resume command");

        assert_eq!(command, "'grok' '-r' 'new-session' '--model' 'fast'");
    }

    #[test]
    fn nested_json_agents_resume_with_native_resume_flag() {
        let cases = [
            (AgentKind::Cursor, "cursor-agent"),
            (AgentKind::Copilot, "copilot"),
            (AgentKind::CodeBuddy, "codebuddy"),
            (AgentKind::Factory, "droid"),
            (AgentKind::Qoder, "qodercli"),
        ];

        for (kind, executable) in cases {
            let launch = AgentLaunchCommandRecord {
                executable: executable.to_string(),
                arguments: vec![
                    executable.to_string(),
                    "--resume".to_string(),
                    "old".to_string(),
                ],
                cwd: None,
                environment: Default::default(),
                captured_at: 40.0,
            };
            let command =
                build_resume_command(kind, "new", Some(&launch), None).expect("resume command");

            assert_eq!(command, format!("'{executable}' '--resume' 'new'"));
        }
    }

    #[test]
    fn kiro_resume_command_uses_chat_resume_id() {
        let launch = AgentLaunchCommandRecord {
            executable: "kiro-cli".to_string(),
            arguments: vec![
                "kiro-cli".to_string(),
                "chat".to_string(),
                "--resume-id".to_string(),
                "old".to_string(),
                "--agent".to_string(),
                "cmux".to_string(),
            ],
            cwd: None,
            environment: Default::default(),
            captured_at: 50.0,
        };

        let command =
            build_resume_command(AgentKind::Kiro, "new", Some(&launch), None).expect("resume");

        assert_eq!(
            command,
            "'kiro-cli' 'chat' '--resume-id' 'new' '--agent' 'cmux'"
        );
    }

    #[test]
    fn antigravity_resume_command_replays_launch_without_resume_selector() {
        let launch = AgentLaunchCommandRecord {
            executable: "agy".to_string(),
            arguments: vec!["agy".to_string(), "--model".to_string(), "fast".to_string()],
            cwd: None,
            environment: Default::default(),
            captured_at: 60.0,
        };

        let command = build_resume_command(AgentKind::Antigravity, "ignored", Some(&launch), None)
            .expect("resume");

        assert_eq!(command, "'agy' '--model' 'fast'");
    }

    #[test]
    fn rovodev_resume_command_uses_restore_subcommand() {
        let launch = AgentLaunchCommandRecord {
            executable: "acli".to_string(),
            arguments: vec![
                "acli".to_string(),
                "rovodev".to_string(),
                "run".to_string(),
                "--restore".to_string(),
                "old".to_string(),
                "--profile".to_string(),
                "work".to_string(),
            ],
            cwd: None,
            environment: Default::default(),
            captured_at: 70.0,
        };

        let command =
            build_resume_command(AgentKind::RovoDev, "new", Some(&launch), None).expect("resume");

        assert_eq!(
            command,
            "'acli' 'rovodev' 'run' '--restore' 'new' '--profile' 'work'"
        );
    }

    #[test]
    fn omp_resume_command_uses_session_flag() {
        let launch = AgentLaunchCommandRecord {
            executable: "omp".to_string(),
            arguments: vec![
                "omp".to_string(),
                "--session".to_string(),
                "old".to_string(),
                "--model".to_string(),
                "fast".to_string(),
            ],
            cwd: None,
            environment: Default::default(),
            captured_at: 80.0,
        };

        let command =
            build_resume_command(AgentKind::Omp, "new", Some(&launch), None).expect("resume");

        assert_eq!(command, "'omp' '--session' 'new' '--model' 'fast'");
    }

    #[test]
    fn pi_resume_command_uses_session_flag_and_keeps_safe_options() {
        let launch = AgentLaunchCommandRecord {
            executable: "pi".to_string(),
            arguments: vec![
                "pi".to_string(),
                "--session".to_string(),
                "old".to_string(),
                "--model".to_string(),
                "fast".to_string(),
                "--provider=anthropic".to_string(),
                "--api-key".to_string(),
                "secret".to_string(),
                "--prompt".to_string(),
                "do work".to_string(),
            ],
            cwd: None,
            environment: Default::default(),
            captured_at: 90.0,
        };

        let command =
            build_resume_command(AgentKind::Pi, "new", Some(&launch), None).expect("resume");

        assert_eq!(
            command,
            "'pi' '--session' 'new' '--model' 'fast' '--provider=anthropic'"
        );
    }

    #[test]
    fn amp_resume_command_uses_threads_continue_and_drops_old_session() {
        let launch = AgentLaunchCommandRecord {
            executable: "amp".to_string(),
            arguments: vec![
                "amp".to_string(),
                "threads".to_string(),
                "continue".to_string(),
                "old-thread".to_string(),
                "--model".to_string(),
                "fast".to_string(),
                "--api-key".to_string(),
                "secret".to_string(),
            ],
            cwd: None,
            environment: Default::default(),
            captured_at: 100.0,
        };

        let command = build_resume_command(AgentKind::Amp, "new-thread", Some(&launch), None)
            .expect("resume");

        assert_eq!(
            command,
            "'amp' 'threads' 'continue' 'new-thread' '--model' 'fast'"
        );
    }

    #[test]
    fn hermes_agent_resume_command_uses_resume_flag_and_preserves_home() {
        let launch = AgentLaunchCommandRecord {
            executable: "hermes".to_string(),
            arguments: vec![
                "hermes".to_string(),
                "--resume".to_string(),
                "old-session".to_string(),
                "--model".to_string(),
                "gpt-5.4".to_string(),
            ],
            cwd: Some("/tmp/hermes repo".to_string()),
            environment: BTreeMap::from([(
                "HERMES_HOME".to_string(),
                "/tmp/hermes home".to_string(),
            )]),
            captured_at: 110.0,
        };

        let command = build_resume_command(
            AgentKind::HermesAgent,
            "new-session",
            Some(&launch),
            Some("/tmp/hermes repo"),
        )
        .expect("resume");

        assert_eq!(
            command,
            "cd '/tmp/hermes repo' && 'hermes' '--resume' 'new-session' '--model' 'gpt-5.4'"
        );
    }

    #[test]
    fn launch_argv_decodes_cmux_base64_nul_payload() {
        let decoded = decode_base64_nul_separated("b21wAC0tbW9kZWwAZmFzdAA=").expect("decoded");

        assert_eq!(decoded, vec!["omp", "--model", "fast"]);
    }
}
