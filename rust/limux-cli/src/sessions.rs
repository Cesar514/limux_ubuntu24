// summary: Inspect saved Limux and CMUX-compatible agent session records.
// purpose: Provide local no-socket `sessions list/debug` parity over hook session stores.
// inputs: CLI session-list flags, hook-session JSON files, optional Codex transcript indexes, and /proc PID state.
// returns/effects: Returns text or JSON session diagnostics and fails loudly on malformed arguments or stores.

use crate::agent_hooks::{self, AgentHookSessionRecord, AgentHookSessionSnapshot, AgentKind};
use anyhow::{anyhow, bail, Result};
use serde_json::{json, Value};
use std::collections::{BTreeMap, BTreeSet};
use std::fmt;
use std::fs;
use std::path::{Path, PathBuf};

pub(super) enum SessionCommandOutput {
    Text(String),
    Json(Value),
}

pub(super) struct SessionCommandInput {
    pub(super) args: Vec<String>,
    pub(super) global_json: bool,
}

pub(super) enum SessionCommandResult {
    Output(SessionCommandOutput),
    Error(SessionCommandError),
}

#[derive(Debug)]
pub(super) struct SessionCommandError(anyhow::Error);

impl fmt::Display for SessionCommandError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "{}", self.0)
    }
}

impl std::error::Error for SessionCommandError {}

impl From<anyhow::Error> for SessionCommandError {
    fn from(error: anyhow::Error) -> Self {
        Self(error)
    }
}

struct SessionListOptions {
    agent: Option<AgentKind>,
    session: Option<String>,
    workspace: Option<String>,
    surface: Option<String>,
    cwd: Option<String>,
    state_dir: PathBuf,
    codex_home: PathBuf,
    limit: Option<usize>,
    json_output: bool,
}

struct SessionEntry {
    updated_at: f64,
    payload: Value,
}

struct CodexDebugIndex {
    indexed_session_ids: BTreeSet<String>,
    transcript_path_by_session_id: BTreeMap<String, String>,
}

impl From<SessionCommandInput> for SessionCommandResult {
    fn from(input: SessionCommandInput) -> Self {
        session_command_result_from_input(input)
    }
}

// purpose: Execute a sessions command from a typed input envelope.
// inputs: Session command input with args and JSON preference.
// returns/effects: Converts internal errors into a typed command result.
fn session_command_result_from_input(input: SessionCommandInput) -> SessionCommandResult {
    match run_sessions_command_inner(&input.args, input.global_json) {
        Ok(output) => SessionCommandResult::Output(output),
        Err(error) => SessionCommandResult::Error(SessionCommandError::from(error)),
    }
}

// purpose: Run CMUX-compatible local session inspection with internal error context.
// inputs: Raw args after `sessions` or `session-debug`, plus global JSON preference.
// returns/effects: Reads hook stores and returns text or JSON without connecting to Limux.
fn run_sessions_command_inner(args: &[String], global_json: bool) -> Result<SessionCommandOutput> {
    if matches!(
        args.first().map(String::as_str),
        Some("help" | "--help" | "-h")
    ) {
        return Ok(SessionCommandOutput::Text(sessions_usage().to_string()));
    }
    let options = parse_session_list_options(args, global_json)?;
    let (stores, entries) = load_session_entries(&options)?;
    Ok(render_session_output(options, stores, entries))
}

// purpose: Load store metadata and matching session entries for selected agents.
// inputs: Parsed session-list options.
// returns/effects: Reads hook session stores and optional Codex diagnostics.
fn load_session_entries(options: &SessionListOptions) -> Result<(Vec<Value>, Vec<SessionEntry>)> {
    let mut stores = Vec::new();
    let mut entries = Vec::new();
    let mut codex_index = None;
    for agent in selected_agents(options.agent) {
        let snapshot =
            agent_hooks::AgentHookSessionStore::snapshot_for_dir(agent, &options.state_dir)?;
        stores.push(render_store_payload(&snapshot));
        collect_session_entries(options, &snapshot, &mut codex_index, &mut entries)?;
    }
    entries.sort_by(|left, right| {
        right
            .updated_at
            .total_cmp(&left.updated_at)
            .then_with(|| session_id(&left.payload).cmp(&session_id(&right.payload)))
    });
    Ok((stores, entries))
}

// purpose: Convert loaded session data into text or JSON command output.
// inputs: Parsed options, store payloads, and sorted matching entries.
// returns/effects: Applies limits and returns the requested presentation.
fn render_session_output(
    options: SessionListOptions,
    stores: Vec<Value>,
    entries: Vec<SessionEntry>,
) -> SessionCommandOutput {
    let total_matches = entries.len();
    let limit = options.limit.unwrap_or(total_matches);
    let limited = entries.into_iter().take(limit).collect::<Vec<_>>();
    if options.json_output {
        return SessionCommandOutput::Json(json!({
            "state_dir": options.state_dir,
            "default_codex_home": options.codex_home,
            "total_matches": total_matches,
            "limit": options.limit,
            "stores": stores,
            "sessions": limited.into_iter().map(|entry| entry.payload).collect::<Vec<_>>(),
        }));
    }
    SessionCommandOutput::Text(render_sessions_text(
        &options.state_dir,
        total_matches,
        limit,
        &limited,
    ))
}

// purpose: Parse `sessions list` and `sessions debug` flags.
// inputs: Raw CLI tokens and inherited JSON preference.
// returns/effects: Produces validated options or explicit usage errors.
fn parse_session_list_options(
    raw_args: &[String],
    global_json: bool,
) -> Result<SessionListOptions> {
    let mut args = raw_args.to_vec();
    strip_sessions_subcommand(&mut args)?;
    let (agent, session, workspace, surface, cwd) = parse_session_filters(&mut args)?;
    let (state_dir, codex_home) = parse_session_paths(&mut args)?;
    let (limit, json_output) = parse_session_presentation(&mut args, global_json)?;
    Ok(SessionListOptions {
        agent,
        session,
        workspace,
        surface,
        cwd,
        state_dir,
        codex_home,
        limit,
        json_output,
    })
}

// purpose: Remove optional sessions subcommand markers.
// inputs: Mutable raw argument vector.
// returns/effects: Removes list/debug or fails on unknown subcommands.
fn strip_sessions_subcommand(args: &mut Vec<String>) -> Result<()> {
    match args.first().map(|value| value.as_str()) {
        Some("list" | "debug") => {
            args.remove(0);
        }
        Some("help" | "--help" | "-h") => {}
        Some(value) if !value.starts_with('-') => {
            bail!("Unknown sessions subcommand: {value}. Usage: limux sessions list [options]");
        }
        _ => {}
    }
    Ok(())
}

type SessionFilters = (
    Option<AgentKind>,
    Option<String>,
    Option<String>,
    Option<String>,
    Option<String>,
);

// purpose: Parse filter flags for sessions list.
// inputs: Mutable raw argument vector after subcommand removal.
// returns/effects: Removes filter flags and returns normalized filter values.
fn parse_session_filters(args: &mut Vec<String>) -> Result<SessionFilters> {
    let agent = optional_arg(args, "--agent")?
        .map(|value| parse_agent(&value))
        .transpose()?;
    let session = optional_arg(args, "--session")?.and_then(normalized_lower);
    let workspace = optional_arg(args, "--workspace")?.and_then(normalized_lower);
    let surface = optional_arg(args, "--surface")?.and_then(normalized_lower);
    let cwd = optional_arg(args, "--cwd")?.and_then(normalized_lower);
    Ok((agent, session, workspace, surface, cwd))
}

// purpose: Parse path flags for sessions list.
// inputs: Mutable raw argument vector after filter parsing.
// returns/effects: Removes path flags and resolves defaults.
fn parse_session_paths(args: &mut Vec<String>) -> Result<(PathBuf, PathBuf)> {
    let state_dir = optional_arg(args, "--state-dir")?
        .map(expand_tilde)
        .unwrap_or_else(agent_hooks::default_state_dir);
    let codex_home = optional_arg(args, "--codex-home")?
        .map(expand_tilde)
        .or_else(|| std::env::var_os("CODEX_HOME").map(PathBuf::from))
        .or_else(|| dirs::home_dir().map(|home| home.join(".codex")))
        .ok_or_else(|| anyhow!("sessions list requires HOME or --codex-home"))?;
    Ok((state_dir, codex_home))
}

// purpose: Parse output and limiting flags for sessions list.
// inputs: Remaining mutable args and inherited JSON preference.
// returns/effects: Removes presentation flags or fails on unknown leftover args.
fn parse_session_presentation(
    args: &mut Vec<String>,
    global_json: bool,
) -> Result<(Option<usize>, bool)> {
    let limit = parse_limit(optional_arg(args, "--limit")?)?;
    let mut include_all = false;
    let mut json_output = global_json;
    for arg in args.drain(..) {
        match arg.as_str() {
            "--all" => include_all = true,
            "--json" => json_output = true,
            value if value.starts_with('-') => bail!("sessions list: unknown flag '{value}'"),
            value => bail!("sessions list: unexpected argument '{value}'"),
        }
    }
    let limit = if include_all {
        None
    } else {
        Some(limit.unwrap_or(100))
    };
    Ok((limit, json_output))
}

// purpose: Remove a single option and its required value from an argument vector.
// inputs: Mutable token vector and option name.
// returns/effects: Returns the value if present, failing on missing values.
fn optional_arg(args: &mut Vec<String>, name: &str) -> Result<Option<String>> {
    let Some(index) = args.iter().position(|arg| arg == name) else {
        return Ok(None);
    };
    args.remove(index);
    if index >= args.len() {
        bail!("{name} requires a value");
    }
    Ok(Some(args.remove(index)))
}

// purpose: Validate a positive optional limit.
// inputs: Optional raw --limit token.
// returns/effects: Returns a positive usize or an explicit parse error.
fn parse_limit(raw: Option<String>) -> Result<Option<usize>> {
    raw.map(|value| {
        value
            .parse::<usize>()
            .ok()
            .filter(|limit| *limit > 0)
            .ok_or_else(|| anyhow!("sessions list: --limit must be a positive integer"))
    })
    .transpose()
}

// purpose: Parse a CMUX agent filter into a supported hook-store kind.
// inputs: Raw --agent value.
// returns/effects: Returns an agent kind or a clear unknown-agent error.
fn parse_agent(raw: &str) -> Result<AgentKind> {
    let value = raw.trim();
    if value.is_empty() {
        bail!("sessions list: --agent requires a value");
    }
    agent_hooks::AgentKind::from_hook_name(value)
        .ok_or_else(|| anyhow!("sessions list: unknown agent '{value}'"))
}

// purpose: Resolve the agents to inspect.
// inputs: Optional parsed --agent filter.
// returns/effects: Returns either one agent or every supported hook-store agent.
fn selected_agents(agent: Option<AgentKind>) -> Vec<AgentKind> {
    agent
        .map(|agent| vec![agent])
        .unwrap_or_else(|| AgentKind::all().to_vec())
}

// purpose: Add matching records from one agent store to the output list.
// inputs: Parsed options, one store snapshot, optional Codex index cache, and output list.
// returns/effects: Appends JSON payloads for records that satisfy filters.
fn collect_session_entries(
    options: &SessionListOptions,
    snapshot: &AgentHookSessionSnapshot,
    codex_index: &mut Option<CodexDebugIndex>,
    entries: &mut Vec<SessionEntry>,
) -> Result<()> {
    for record in &snapshot.records {
        if !record_matches(options, record) {
            continue;
        }
        let mut payload = base_session_payload(snapshot, record);
        if snapshot.agent == AgentKind::Codex {
            let index =
                codex_index.get_or_insert_with(|| build_codex_debug_index(&options.codex_home));
            add_codex_payload(&mut payload, &options.codex_home, &record.session_id, index);
        }
        entries.push(SessionEntry {
            updated_at: record.updated_at,
            payload,
        });
    }
    Ok(())
}

// purpose: Apply all session-list filters to one record.
// inputs: Parsed options and one saved hook session record.
// returns/effects: Returns true when the record should be included.
fn record_matches(options: &SessionListOptions, record: &AgentHookSessionRecord) -> bool {
    string_filter_matches(&options.session, &record.session_id)
        && string_filter_matches(&options.workspace, &record.workspace_id)
        && string_filter_matches(&options.surface, &record.surface_id)
        && cwd_filter_matches(&options.cwd, record)
}

// purpose: Compare exact string filters in CMUX session-list style.
// inputs: Optional lowercased filter and raw record value.
// returns/effects: Returns true when no filter is present or the lowercased value matches.
fn string_filter_matches(filter: &Option<String>, value: &str) -> bool {
    filter
        .as_ref()
        .is_none_or(|filter| value.to_ascii_lowercase() == *filter)
}

// purpose: Apply the cwd substring filter to saved cwd and launch cwd.
// inputs: Optional lowercased cwd filter and one record.
// returns/effects: Returns true when the filter is absent or either saved path contains it.
fn cwd_filter_matches(filter: &Option<String>, record: &AgentHookSessionRecord) -> bool {
    let Some(filter) = filter else {
        return true;
    };
    record
        .cwd
        .as_deref()
        .into_iter()
        .chain(
            record
                .launch_command
                .as_ref()
                .and_then(|launch| launch.cwd.as_deref()),
        )
        .any(|value| value.to_ascii_lowercase().contains(filter))
}

// purpose: Render a CMUX-shaped JSON payload for one saved session.
// inputs: Store metadata and one hook session record.
// returns/effects: Returns diagnostic JSON including stale-PID and fork-command flags.
fn base_session_payload(
    snapshot: &AgentHookSessionSnapshot,
    record: &AgentHookSessionRecord,
) -> Value {
    let pid_exists = record.pid.map(stored_pid_exists);
    let launch = record.launch_command.as_ref();
    json!({
        "agent": snapshot.agent.store_name(),
        "agent_display_name": snapshot.agent.label(),
        "session_id": record.session_id,
        "workspace_id": record.workspace_id,
        "surface_id": record.surface_id,
        "store_path": snapshot.path,
        "started_at": Value::Null,
        "updated_at": format!("{:.3}", record.updated_at),
        "updated_at_unix": record.updated_at,
        "cwd": record.cwd,
        "transcript_path": Value::Null,
        "pid": record.pid,
        "stored_pid_exists": pid_exists,
        "runtime_status": Value::Null,
        "agent_lifecycle": Value::Null,
        "last_prompt_turn_id": Value::Null,
        "active_prompt_turn_id": Value::Null,
        "launch_working_directory": launch.and_then(|launch| launch.cwd.clone()),
        "launch_arguments": launch.map(|launch| launch.arguments.clone()).unwrap_or_default(),
        "fork_command_available": launch.is_some(),
        "fork_supported": launch.is_some(),
        "fork_risk": pid_exists == Some(false),
        "active_for_workspace": false,
        "active_for_surface": false,
        "active_workspace_session_id": Value::Null,
        "active_surface_session_id": Value::Null,
    })
}

// purpose: Render metadata for one inspected hook store.
// inputs: Store snapshot.
// returns/effects: Returns CMUX-shaped JSON store metadata.
fn render_store_payload(snapshot: &AgentHookSessionSnapshot) -> Value {
    json!({
        "agent": snapshot.agent.store_name(),
        "path": snapshot.path,
        "exists": snapshot.exists,
        "session_count": snapshot.session_count,
    })
}

// purpose: Add Codex transcript/index diagnostics to one session payload.
// inputs: Mutable payload, Codex home, session id, and prebuilt Codex index.
// returns/effects: Mutates the payload with Codex-specific diagnostic fields.
fn add_codex_payload(
    payload: &mut Value,
    codex_home: &Path,
    session_id: &str,
    index: &CodexDebugIndex,
) {
    let session_dir = codex_home.join("sessions");
    payload["session_home"] = json!(codex_home);
    payload["session_dir"] = json!(session_dir);
    payload["codex_indexed"] = json!(index.indexed_session_ids.contains(session_id));
    payload["codex_transcript_found"] =
        json!(index.transcript_path_by_session_id.contains_key(session_id));
    payload["codex_transcript_path"] = index
        .transcript_path_by_session_id
        .get(session_id)
        .map(|path| json!(path))
        .unwrap_or(Value::Null);
}

// purpose: Build Codex session lookup metadata from local Codex state.
// inputs: Codex home directory.
// returns/effects: Best-effort transcript/index diagnostics; unreadable optional files count as absent.
fn build_codex_debug_index(codex_home: &Path) -> CodexDebugIndex {
    let indexed_session_ids = read_codex_session_index(&codex_home.join("session_index.jsonl"));
    let mut transcript_path_by_session_id = BTreeMap::new();
    for root in [
        codex_home.join("sessions"),
        codex_home.join("archived_sessions"),
    ] {
        collect_codex_transcripts(&root, &mut transcript_path_by_session_id);
    }
    CodexDebugIndex {
        indexed_session_ids,
        transcript_path_by_session_id,
    }
}

// purpose: Read Codex session_index.jsonl ids.
// inputs: Path to session_index.jsonl.
// returns/effects: Returns indexed ids; missing optional files are treated as absent diagnostics.
fn read_codex_session_index(path: &Path) -> BTreeSet<String> {
    let Ok(contents) = fs::read_to_string(path) else {
        return BTreeSet::new();
    };
    contents
        .lines()
        .filter_map(|line| serde_json::from_str::<Value>(line).ok())
        .filter_map(|value| value.get("id").and_then(Value::as_str).map(str::to_string))
        .collect()
}

// purpose: Recursively collect Codex transcript paths keyed by UUID-like session id.
// inputs: Root directory and mutable output map.
// returns/effects: Adds first-seen transcript paths; missing roots are treated as absent optional diagnostics.
fn collect_codex_transcripts(root: &Path, output: &mut BTreeMap<String, String>) {
    let Ok(entries) = fs::read_dir(root) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            collect_codex_transcripts(&path, output);
            continue;
        }
        if path.extension().and_then(|ext| ext.to_str()) != Some("jsonl") {
            continue;
        }
        let Some(name) = path.file_name().and_then(|name| name.to_str()) else {
            continue;
        };
        for id in uuid_like_tokens(name) {
            output
                .entry(id)
                .or_insert_with(|| path.display().to_string());
        }
    }
}

// purpose: Extract UUID-shaped tokens from transcript file names.
// inputs: A file name or path component.
// returns/effects: Returns lowercased UUID-shaped strings.
fn uuid_like_tokens(value: &str) -> Vec<String> {
    value
        .split(|ch: char| !(ch.is_ascii_hexdigit() || ch == '-'))
        .filter(|part| part.len() == 36 && part.chars().filter(|ch| *ch == '-').count() == 4)
        .map(str::to_ascii_lowercase)
        .collect()
}

// purpose: Render CMUX-like text output for session rows.
// inputs: State directory, total match count, applied limit, and limited session rows.
// returns/effects: Returns user-facing text with no-socket empty-state diagnostics.
fn render_sessions_text(
    state_dir: &Path,
    total_matches: usize,
    limit: usize,
    entries: &[SessionEntry],
) -> String {
    if entries.is_empty() {
        return format!(
            "No saved agent sessions matched.\nstate_dir={}",
            state_dir.display()
        );
    }
    let mut lines = entries
        .iter()
        .map(|entry| render_session_line(&entry.payload))
        .collect::<Vec<_>>();
    if total_matches > limit {
        lines.push(format!(
            "... {} more. Pass --all or --limit <n>.",
            total_matches - limit
        ));
    }
    lines.join("\n")
}

// purpose: Render one session row in CMUX-compatible key-value text form.
// inputs: One JSON session payload.
// returns/effects: Returns a single line including fork and stale-PID diagnostics.
fn render_session_line(payload: &Value) -> String {
    let mut parts = base_session_line_parts(payload);
    push_codex_line_parts(payload, &mut parts);
    parts.push(format!(
        "fork_command={}",
        yes_no(payload_bool(payload, "fork_command_available"))
    ));
    parts.push(format!(
        "fork={}",
        yes_no(payload_bool(payload, "fork_supported"))
    ));
    if let Some(value) = payload.get("stored_pid_exists").and_then(Value::as_bool) {
        parts.push(format!("pid_exists={}", yes_no(value)));
    }
    parts.join("  ")
}

// purpose: Build common session text columns.
// inputs: One JSON session payload.
// returns/effects: Returns base key-value text columns.
fn base_session_line_parts(payload: &Value) -> Vec<String> {
    vec![
        format!(
            "{} {}",
            payload_str(payload, "agent"),
            payload_str(payload, "session_id")
        ),
        format!("workspace={}", payload_str(payload, "workspace_id")),
        format!("surface={}", payload_str(payload, "surface_id")),
        format!("cwd={}", payload_str(payload, "cwd")),
        format!("session_dir={}", payload_str(payload, "session_dir")),
        format!(
            "active_ws={}",
            yes_no(payload_bool(payload, "active_for_workspace"))
        ),
        format!(
            "active_surface={}",
            yes_no(payload_bool(payload, "active_for_surface"))
        ),
        format!("updated={}", payload_str(payload, "updated_at")),
    ]
}

// purpose: Add Codex-only text columns when the row is for Codex.
// inputs: One JSON session payload and mutable text columns.
// returns/effects: Appends Codex index/transcript diagnostics.
fn push_codex_line_parts(payload: &Value, parts: &mut Vec<String>) {
    if payload.get("agent").and_then(Value::as_str) == Some("codex") {
        parts.push(format!(
            "codex_indexed={}",
            yes_no(payload_bool(payload, "codex_indexed"))
        ));
        parts.push(format!(
            "codex_transcript={}",
            yes_no(payload_bool(payload, "codex_transcript_found"))
        ));
    }
}

// purpose: Read a string field for text output.
// inputs: JSON payload and key.
// returns/effects: Returns "-" for missing, null, or empty strings.
fn payload_str(payload: &Value, key: &str) -> String {
    payload
        .get(key)
        .and_then(Value::as_str)
        .filter(|value| !value.is_empty())
        .unwrap_or("-")
        .to_string()
}

// purpose: Read a boolean field for text output.
// inputs: JSON payload and key.
// returns/effects: Returns false when the field is missing or not boolean.
fn payload_bool(payload: &Value, key: &str) -> bool {
    payload.get(key).and_then(Value::as_bool).unwrap_or(false)
}

// purpose: Render booleans in CMUX text output style.
// inputs: Boolean value.
// returns/effects: Returns yes or no.
fn yes_no(value: bool) -> &'static str {
    if value {
        "yes"
    } else {
        "no"
    }
}

// purpose: Read a stable session id sorting key.
// inputs: JSON payload.
// returns/effects: Returns the session id text or "-".
fn session_id(payload: &Value) -> String {
    payload_str(payload, "session_id")
}

// purpose: Check whether a saved Linux PID still exists.
// inputs: Saved process id.
// returns/effects: Returns /proc presence for stale-PID diagnostics.
fn stored_pid_exists(pid: u32) -> bool {
    Path::new("/proc").join(pid.to_string()).exists()
}

// purpose: Expand leading tilde syntax for CLI path flags.
// inputs: Raw path string.
// returns/effects: Returns a PathBuf, preserving the raw path if HOME is unavailable.
fn expand_tilde(value: String) -> PathBuf {
    if value == "~" {
        return dirs::home_dir().unwrap_or_else(|| PathBuf::from(value));
    }
    if let Some(rest) = value.strip_prefix("~/") {
        return dirs::home_dir()
            .map(|home| home.join(rest))
            .unwrap_or_else(|| PathBuf::from(value));
    }
    PathBuf::from(value)
}

// purpose: Normalize optional filter text.
// inputs: Raw string value.
// returns/effects: Returns lowercased nonempty text.
fn normalized_lower(value: String) -> Option<String> {
    let value = value.trim();
    (!value.is_empty()).then(|| value.to_ascii_lowercase())
}

// purpose: Provide no-socket sessions usage text.
// inputs: None.
// returns/effects: Returns static help text.
fn sessions_usage() -> &'static str {
    "Usage: limux sessions list [options]\n\
     Options: --agent <name> --session <id> --workspace <id> --surface <id> --cwd <text>\n\
     Options: --state-dir <path> --codex-home <path> --limit <n> --all --json"
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent_hooks::{AgentHookSessionStore, AgentLaunchCommandRecord};
    use tempfile::tempdir;

    // purpose: Seed one Codex hook-store record with a stale PID.
    // inputs: Hook state directory path.
    // returns/effects: Writes a test store record.
    fn seed_stale_codex_record(dir: &Path) {
        let store = AgentHookSessionStore::new_for_dir("codex", dir);
        store
            .upsert(AgentHookSessionRecord {
                session_id: "session-a".to_string(),
                workspace_id: "workspace-a".to_string(),
                surface_id: "surface-a".to_string(),
                cwd: Some("/tmp/project".to_string()),
                pid: Some(999_999),
                launch_command: Some(AgentLaunchCommandRecord {
                    executable: "codex".to_string(),
                    arguments: vec![
                        "codex".to_string(),
                        "--model".to_string(),
                        "gpt-5".to_string(),
                    ],
                    cwd: Some("/tmp/project".to_string()),
                    environment: Default::default(),
                    captured_at: 1.0,
                }),
                updated_at: 20.0,
            })
            .expect("store record");
    }

    // purpose: Exercise text session-list output for filters and stale-PID diagnostics.
    // inputs: Temporary hook state directory with one Codex session.
    // returns/effects: Asserts output includes the matching session and diagnostics.
    fn assert_sessions_list_filters_and_reports_stale_pid() {
        let dir = tempdir().expect("tempdir");
        seed_stale_codex_record(dir.path());
        let output = SessionCommandResult::from(SessionCommandInput {
            args: vec![
                "list".to_string(),
                "--agent".to_string(),
                "codex".to_string(),
                "--workspace".to_string(),
                "workspace-a".to_string(),
                "--state-dir".to_string(),
                dir.path().display().to_string(),
            ],
            global_json: false,
        });

        let SessionCommandResult::Output(SessionCommandOutput::Text(text)) = output else {
            panic!("expected text");
        };
        assert!(text.contains("codex session-a"));
        assert!(text.contains("fork_command=yes"));
        assert!(text.contains("pid_exists=no"));
    }

    #[test]
    fn sessions_list_filters_and_reports_stale_pid() {
        assert_sessions_list_filters_and_reports_stale_pid();
    }

    // purpose: Exercise JSON output for empty session-list stores.
    // inputs: Empty temporary hook state directory.
    // returns/effects: Asserts JSON shape and missing-store metadata.
    fn assert_sessions_list_json_includes_store_metadata() {
        let dir = tempdir().expect("tempdir");
        let output = SessionCommandResult::from(SessionCommandInput {
            args: vec![
                "debug".to_string(),
                "--agent".to_string(),
                "claude".to_string(),
                "--state-dir".to_string(),
                dir.path().display().to_string(),
                "--json".to_string(),
            ],
            global_json: false,
        });

        let SessionCommandResult::Output(SessionCommandOutput::Json(value)) = output else {
            panic!("expected json");
        };
        assert_eq!(value["total_matches"], 0);
        assert_eq!(value["stores"][0]["agent"], "claude");
        assert_eq!(value["stores"][0]["exists"], false);
    }

    #[test]
    fn sessions_list_json_includes_store_metadata() {
        assert_sessions_list_json_includes_store_metadata();
    }
}
