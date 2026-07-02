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
use std::process::{Command, Stdio};
use std::thread;
use std::time::{Duration, Instant};

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

struct CodexPidTranscript {
    session_id: String,
    path: String,
}

struct SessionForkDiagnostics {
    transcript_path: Option<String>,
    hook_record_restorable: bool,
    fork_command: Option<String>,
    fork_supported: bool,
    fork_unavailable_reason: &'static str,
    pid_exists: Option<bool>,
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
    let mut opencode_probe_cache = BTreeMap::new();
    for agent in selected_agents(options.agent) {
        let snapshot =
            agent_hooks::AgentHookSessionStore::snapshot_for_dir(agent, &options.state_dir)?;
        stores.push(render_store_payload(&snapshot));
        collect_session_entries(
            options,
            &snapshot,
            &mut codex_index,
            &mut opencode_probe_cache,
            &mut entries,
        )?;
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
    opencode_probe_cache: &mut BTreeMap<String, bool>,
    entries: &mut Vec<SessionEntry>,
) -> Result<()> {
    for record in &snapshot.records {
        if !record_matches(options, record) {
            continue;
        }
        let resolved_record = resolved_session_record(snapshot.agent, record);
        let mut payload = base_session_payload(snapshot, &resolved_record, opencode_probe_cache);
        if snapshot.agent == AgentKind::Codex {
            let index =
                codex_index.get_or_insert_with(|| build_codex_debug_index(&options.codex_home));
            add_codex_payload(&mut payload, &options.codex_home, &resolved_record, index);
        }
        entries.push(SessionEntry {
            updated_at: resolved_record.updated_at,
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

// purpose: Resolve agent-specific diagnostic records before payload rendering.
// inputs: Agent kind and raw saved hook record.
// returns/effects: Returns a cloned record with trusted workflow transcript metadata when found.
fn resolved_session_record(
    agent: AgentKind,
    record: &AgentHookSessionRecord,
) -> AgentHookSessionRecord {
    if agent == AgentKind::Claude {
        return resolved_claude_workflow_record(record);
    }
    record.clone()
}

// purpose: Render a CMUX-shaped JSON payload for one saved session.
// inputs: Store metadata and one hook session record.
// returns/effects: Returns diagnostic JSON including stale-PID and fork-command flags.
fn base_session_payload(
    snapshot: &AgentHookSessionSnapshot,
    record: &AgentHookSessionRecord,
    opencode_probe_cache: &mut BTreeMap<String, bool>,
) -> Value {
    let launch = record.launch_command.as_ref();
    let fork = session_fork_diagnostics(snapshot.agent, record, opencode_probe_cache);
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
        "transcript_path": fork.transcript_path,
        "pid": record.pid,
        "stored_pid_exists": fork.pid_exists,
        "runtime_status": Value::Null,
        "agent_lifecycle": Value::Null,
        "last_prompt_turn_id": Value::Null,
        "active_prompt_turn_id": Value::Null,
        "launch_working_directory": launch.and_then(|launch| launch.cwd.clone()),
        "launch_arguments": launch.map(|launch| launch.arguments.clone()).unwrap_or_default(),
        "fork_command": fork.fork_command,
        "fork_command_available": fork.fork_command.is_some(),
        "fork_supported": fork.fork_supported,
        "fork_unavailable_reason": fork.fork_unavailable_reason,
        "fork_startup_input_available": false,
        "hook_record_restorable": fork.hook_record_restorable,
        "stale_pid_blocks_restore_in_0_64_17": fork.pid_exists == Some(false) && fork.hook_record_restorable,
        "fork_risk": fork.pid_exists == Some(false),
        "active_for_workspace": false, "active_for_surface": false,
        "active_workspace_session_id": Value::Null,
        "active_surface_session_id": Value::Null,
    })
}

// purpose: Compute CMUX-compatible fork and restorable diagnostics for one session.
// inputs: Agent kind and saved hook record.
// returns/effects: Returns diagnostic values without mutating state.
fn session_fork_diagnostics(
    agent: AgentKind,
    record: &AgentHookSessionRecord,
    opencode_probe_cache: &mut BTreeMap<String, bool>,
) -> SessionForkDiagnostics {
    let transcript_path = session_transcript_path(agent, record);
    let hook_record_restorable = hook_record_restorable(agent, record, transcript_path.as_deref());
    let launch = record.launch_command.as_ref();
    let fork_command = hook_record_restorable
        .then(|| agent_hooks::build_fork_command(agent, &record.session_id, launch, None))
        .flatten();
    let fork_support = fork_support(agent, record, &fork_command, opencode_probe_cache);
    let fork_unavailable_reason =
        fork_unavailable_reason(hook_record_restorable, &fork_command, fork_support);
    SessionForkDiagnostics {
        transcript_path,
        hook_record_restorable,
        fork_command,
        fork_supported: fork_support.0,
        fork_unavailable_reason,
        pid_exists: record.pid.map(stored_pid_exists),
    }
}

// purpose: Compute CMUX-compatible fork support separate from command rendering.
// inputs: Agent kind, saved record, optional rendered fork command, and OpenCode probe cache.
// returns/effects: Returns supported flag and diagnostic reason without mutating session state.
fn fork_support(
    agent: AgentKind,
    record: &AgentHookSessionRecord,
    fork_command: &Option<String>,
    opencode_probe_cache: &mut BTreeMap<String, bool>,
) -> (bool, &'static str) {
    if fork_command.is_none() {
        return (false, "agent_has_no_fork_command");
    }
    if agent != AgentKind::OpenCode {
        return (true, "available");
    }
    opencode_fork_support(record, opencode_probe_cache)
}

// purpose: Resolve the transcript path used by session diagnostics.
// inputs: Agent kind and saved hook record.
// returns/effects: Returns a known nonempty transcript path without creating files.
fn session_transcript_path(agent: AgentKind, record: &AgentHookSessionRecord) -> Option<String> {
    let recorded = record
        .transcript_path
        .as_deref()
        .and_then(normalized)
        .map(expand_tilde);
    if let Some(path) = recorded.as_ref().filter(|path| regular_nonempty_file(path)) {
        return Some(path.display().to_string());
    }
    if agent == AgentKind::Claude {
        return claude_transcript_path(record);
    }
    recorded.map(|path| path.display().to_string())
}

// purpose: Apply CMUX's restorable-record trust rule for session list diagnostics.
// inputs: Agent kind, record metadata, and resolved transcript evidence.
// returns/effects: Returns false for untrusted Claude records without transcript evidence.
fn hook_record_restorable(
    agent: AgentKind,
    record: &AgentHookSessionRecord,
    transcript_path: Option<&str>,
) -> bool {
    if agent != AgentKind::Claude {
        return record.is_restorable != Some(false);
    }
    if transcript_path.is_some() {
        return true;
    }
    claude_transcript_path(record).is_some()
}

// purpose: Render CMUX-compatible fork unavailable reason metadata.
// inputs: Trusted restorable flag and rendered fork command.
// returns/effects: Returns the diagnostic reason string.
fn fork_unavailable_reason(
    hook_record_restorable: bool,
    fork_command: &Option<String>,
    fork_support: (bool, &'static str),
) -> &'static str {
    if fork_support.0 {
        return "available";
    }
    if !hook_record_restorable {
        return "record_marked_non_restorable";
    }
    if fork_command.is_none() {
        return "agent_has_no_fork_command";
    }
    fork_support.1
}

// purpose: Gate local OpenCode fork support on CMUX's minimum fixed version.
// inputs: Saved OpenCode record and per-command probe cache.
// returns/effects: Runs at most one bounded local `--version` probe per cache key.
fn opencode_fork_support(
    record: &AgentHookSessionRecord,
    cache: &mut BTreeMap<String, bool>,
) -> (bool, &'static str) {
    let Some(launch) = record.launch_command.as_ref() else {
        return (false, "opencode_version_unverified");
    };
    if opencode_launcher_is_omo(launch) {
        return (true, "available");
    }
    let working_directory = launch
        .cwd
        .as_deref()
        .and_then(normalized)
        .or_else(|| record.cwd.as_deref().and_then(normalized));
    if working_directory
        .as_deref()
        .is_some_and(|cwd| !Path::new(cwd).is_dir())
    {
        return (true, "available");
    }
    let Some(executable) = opencode_probe_executable(launch) else {
        return (false, "opencode_version_unverified");
    };
    if executable.starts_with('/') && !Path::new(&executable).is_file() {
        return (false, "opencode_executable_missing");
    }
    opencode_local_probe_support(&executable, launch, working_directory.as_deref(), cache)
}

// purpose: Run or reuse a local OpenCode version probe decision.
// inputs: Probe executable, launch metadata, optional cwd, and cache.
// returns/effects: Returns CMUX fork support and reason for local OpenCode records.
fn opencode_local_probe_support(
    executable: &str,
    launch: &agent_hooks::AgentLaunchCommandRecord,
    working_directory: Option<&str>,
    cache: &mut BTreeMap<String, bool>,
) -> (bool, &'static str) {
    let key = opencode_probe_cache_key(executable, launch, working_directory);
    let supported = match cache.get(&key) {
        Some(supported) => *supported,
        None => {
            let supported = opencode_version_output(executable, launch, working_directory)
                .is_some_and(|output| opencode_version_supports_fork(&output));
            cache.insert(key, supported);
            supported
        }
    };
    if supported {
        (true, "available")
    } else {
        (false, "opencode_version_unsupported")
    }
}

// purpose: Detect OpenCode wrapper launchers that CMUX treats as fork-capable.
// inputs: Saved launch metadata.
// returns/effects: Returns true for `omo` launch forms without filesystem probing.
fn opencode_launcher_is_omo(launch: &agent_hooks::AgentLaunchCommandRecord) -> bool {
    opencode_probe_executable(launch)
        .as_deref()
        .map(Path::new)
        .and_then(Path::file_name)
        .and_then(|name| name.to_str())
        .is_some_and(|name| name == "omo")
}

// purpose: Resolve the executable used for OpenCode `--version` probing.
// inputs: Saved launch metadata.
// returns/effects: Returns normalized executable or first launch arg.
fn opencode_probe_executable(launch: &agent_hooks::AgentLaunchCommandRecord) -> Option<String> {
    normalized(&launch.executable)
        .or_else(|| launch.arguments.first().and_then(|arg| normalized(arg)))
}

// purpose: Build a stable OpenCode probe cache key.
// inputs: Probe executable, saved launch metadata, and working directory.
// returns/effects: Returns a key covering command, cwd, and relevant safe environment.
fn opencode_probe_cache_key(
    executable: &str,
    launch: &agent_hooks::AgentLaunchCommandRecord,
    working_directory: Option<&str>,
) -> String {
    let environment = opencode_probe_environment(&launch.environment);
    [
        executable.to_string(),
        "--version".to_string(),
        format!(
            "PATH={}",
            environment.get("PATH").cloned().unwrap_or_default()
        ),
        format!(
            "OPENCODE_CONFIG_DIR={}",
            environment
                .get("OPENCODE_CONFIG_DIR")
                .cloned()
                .unwrap_or_default()
        ),
        format!("cwd={}", working_directory.unwrap_or_default()),
    ]
    .join("\u{1f}")
}

// purpose: Run a bounded OpenCode `--version` command.
// inputs: Executable, saved launch metadata, and optional local cwd.
// returns/effects: Captures stdout/stderr or kills the process on timeout.
fn opencode_version_output(
    executable: &str,
    launch: &agent_hooks::AgentLaunchCommandRecord,
    working_directory: Option<&str>,
) -> Option<String> {
    let mut command = Command::new(executable);
    command
        .arg("--version")
        .env_clear()
        .envs(opencode_probe_environment(&launch.environment))
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    if let Some(cwd) = working_directory {
        command.current_dir(cwd);
    }
    command_output_with_timeout(command, Duration::from_secs(3))
}

// purpose: Capture command output without allowing a diagnostics probe to hang.
// inputs: Configured command and timeout duration.
// returns/effects: Returns combined output, terminating the child on timeout.
fn command_output_with_timeout(mut command: Command, timeout: Duration) -> Option<String> {
    let mut child = command.spawn().ok()?;
    let started = Instant::now();
    loop {
        let Some(status) = child.try_wait().ok()? else {
            if started.elapsed() >= timeout {
                let _ = child.kill();
                let _ = child.wait();
                return None;
            }
            thread::sleep(Duration::from_millis(25));
            continue;
        };
        let output = child.wait_with_output().ok()?;
        if !status.success() {
            return None;
        }
        let mut combined = output.stdout;
        combined.extend(output.stderr);
        return String::from_utf8(combined).ok();
    }
}

// purpose: Return a nonempty process environment value.
// inputs: Environment key name.
// returns/effects: Reads the current process environment without mutation.
fn nonempty_process_env(key: &str) -> Option<(String, String)> {
    let value = std::env::var(key).ok()?;
    (!value.trim().is_empty()).then(|| (key.to_string(), value))
}

// purpose: Return a nonempty launch environment value.
// inputs: Launch environment and key name.
// returns/effects: Clones a safe value when present.
fn nonempty_launch_env(
    launch_environment: &BTreeMap<String, String>,
    key: &str,
) -> Option<(String, String)> {
    launch_environment
        .get(key)
        .filter(|value| !value.trim().is_empty())
        .map(|value| (key.to_string(), value.clone()))
}

// purpose: Insert a default PATH when the probe environment lacks one.
// inputs: Mutable environment map.
// returns/effects: Mutates only when PATH is absent.
fn ensure_probe_path(environment: &mut BTreeMap<String, String>) {
    if environment.contains_key("PATH") {
        return;
    }
    environment.insert(
        "PATH".to_string(),
        "/usr/local/bin:/usr/bin:/bin:/usr/sbin:/sbin".to_string(),
    );
}

// purpose: Build a sanitized environment for OpenCode version probing.
// inputs: Saved launch environment.
// returns/effects: Preserves safe process keys plus CMUX-selected launch keys.
fn opencode_probe_environment(
    launch_environment: &BTreeMap<String, String>,
) -> BTreeMap<String, String> {
    let mut environment = BTreeMap::new();
    for key in [
        "HOME", "LANG", "LC_ALL", "LC_CTYPE", "LOGNAME", "PATH", "TMPDIR", "USER",
    ] {
        if let Some((key, value)) = nonempty_process_env(key) {
            environment.insert(key, value);
        }
    }
    for key in ["OPENCODE_CONFIG_DIR", "PATH"] {
        if let Some((key, value)) = nonempty_launch_env(launch_environment, key) {
            environment.insert(key, value);
        }
    }
    ensure_probe_path(&mut environment);
    environment
}

// purpose: Parse OpenCode version output using CMUX's minimum fork-fixed version.
// inputs: Raw `opencode --version` output.
// returns/effects: Returns true for versions >= 1.14.50.
fn opencode_version_supports_fork(output: &str) -> bool {
    let Some((major, minor, patch)) = first_semver(output) else {
        return false;
    };
    (major, minor, patch) >= (1, 14, 50)
}

// purpose: Find the first semantic version triple in command output.
// inputs: Arbitrary process output.
// returns/effects: Returns the first major/minor/patch tuple when parseable.
fn first_semver(output: &str) -> Option<(u64, u64, u64)> {
    output.split_whitespace().find_map(|token| {
        let token = token.trim_matches(|character: char| !character.is_ascii_alphanumeric());
        let mut parts = token.split('.');
        let major = parts.next()?.parse().ok()?;
        let minor = parts.next()?.parse().ok()?;
        let patch = parts.next()?.parse().ok()?;
        Some((major, minor, patch))
    })
}

// purpose: Find a Claude transcript in known Claude config roots.
// inputs: Session options and saved Claude hook record.
// returns/effects: Searches config roots read-only and returns one direct transcript path.
fn claude_transcript_path(record: &AgentHookSessionRecord) -> Option<String> {
    if !safe_session_filename(&record.session_id) {
        return None;
    }
    let cwd = record
        .launch_command
        .as_ref()
        .and_then(|launch| launch.cwd.as_deref())
        .or(record.cwd.as_deref())
        .and_then(normalized);
    claude_config_roots(record)
        .into_iter()
        .find_map(|root| claude_transcript_in_root(&root, record, cwd.as_deref()))
}

// purpose: Resolve Claude workflow-container records to their single sibling transcript.
// inputs: Raw Claude hook record.
// returns/effects: Returns a cloned record with session id/path changed only on an unambiguous match.
fn resolved_claude_workflow_record(record: &AgentHookSessionRecord) -> AgentHookSessionRecord {
    if !safe_session_filename(&record.session_id) {
        return record.clone();
    }
    if record
        .transcript_path
        .as_deref()
        .and_then(normalized)
        .map(expand_tilde)
        .is_some_and(|path| regular_nonempty_file(&path))
    {
        return record.clone();
    }
    let Some((session_id, transcript_path)) = single_claude_workflow_sibling(record) else {
        return record.clone();
    };
    let mut resolved = record.clone();
    resolved.session_id = session_id;
    resolved.transcript_path = Some(transcript_path.display().to_string());
    resolved
}

// purpose: Find the sole sibling transcript for a Claude workflow-container record.
// inputs: Raw Claude hook record.
// returns/effects: Returns None when zero or multiple transcript candidates exist.
fn single_claude_workflow_sibling(record: &AgentHookSessionRecord) -> Option<(String, PathBuf)> {
    let matches = claude_workflow_project_dirs(record)
        .into_iter()
        .flat_map(|project| collect_claude_sibling_transcripts(&project, &record.session_id, 4))
        .collect::<Vec<_>>();
    (matches.len() == 1).then(|| matches[0].clone())
}

// purpose: Locate Claude project dirs that contain the workflow container.
// inputs: Raw Claude hook record.
// returns/effects: Returns deduplicated project roots.
fn claude_workflow_project_dirs(record: &AgentHookSessionRecord) -> Vec<PathBuf> {
    let cwd_candidates = claude_cwd_candidates(record);
    let mut dirs = Vec::new();
    for root in claude_config_roots(record) {
        let projects_root = root.join("projects");
        for cwd in &cwd_candidates {
            push_workflow_project_dir(
                &mut dirs,
                projects_root.join(encode_claude_project_dir(cwd)),
                &record.session_id,
            );
        }
        for project in read_dir_paths(&projects_root) {
            push_workflow_project_dir(&mut dirs, project, &record.session_id);
        }
    }
    dirs
}

// purpose: Get cwd candidates used for Claude project-dir lookup.
// inputs: Raw Claude hook record.
// returns/effects: Returns normalized cwd strings.
fn claude_cwd_candidates(record: &AgentHookSessionRecord) -> Vec<String> {
    [
        record
            .launch_command
            .as_ref()
            .and_then(|launch| launch.cwd.as_deref()),
        record.cwd.as_deref(),
    ]
    .into_iter()
    .flatten()
    .filter_map(normalized)
    .collect()
}

// purpose: Add a project dir only when it has the workflow-container child.
// inputs: Mutable dir list, candidate project dir, and workflow container id.
// returns/effects: Mutates the list on unique directory matches.
fn push_workflow_project_dir(dirs: &mut Vec<PathBuf>, project: PathBuf, session_id: &str) {
    if project.join(session_id).is_dir() && !dirs.iter().any(|existing| existing == &project) {
        dirs.push(project);
    }
}

// purpose: Recursively collect Claude sibling transcript files.
// inputs: Search dir, excluded container session id, and remaining recursion depth.
// returns/effects: Returns safe nonempty transcript ids and paths.
fn collect_claude_sibling_transcripts(
    directory: &Path,
    excluded_session_id: &str,
    remaining_depth: usize,
) -> Vec<(String, PathBuf)> {
    let mut matches = Vec::new();
    collect_claude_sibling_transcripts_into(
        directory,
        excluded_session_id,
        remaining_depth,
        &mut matches,
    );
    matches
}

// purpose: Recursive implementation for Claude sibling transcript discovery.
// inputs: Search dir, excluded id, remaining depth, and mutable results.
// returns/effects: Appends safe nonempty transcript candidates.
fn collect_claude_sibling_transcripts_into(
    directory: &Path,
    excluded_session_id: &str,
    remaining_depth: usize,
    matches: &mut Vec<(String, PathBuf)>,
) {
    for path in read_dir_paths(directory) {
        if path.extension().and_then(|ext| ext.to_str()) == Some("jsonl") {
            push_claude_sibling_transcript(matches, &path, excluded_session_id);
            continue;
        }
        if remaining_depth == 0 || !path.is_dir() {
            continue;
        }
        collect_claude_sibling_transcripts_into(
            &path,
            excluded_session_id,
            remaining_depth - 1,
            matches,
        );
    }
}

// purpose: Add one Claude sibling transcript candidate when safe and nonempty.
// inputs: Mutable result list, candidate path, and excluded id.
// returns/effects: Mutates results only for usable transcript files.
fn push_claude_sibling_transcript(
    matches: &mut Vec<(String, PathBuf)>,
    path: &Path,
    excluded_session_id: &str,
) {
    let Some(file_name) = path.file_name().and_then(|name| name.to_str()) else {
        return;
    };
    let Some(session_id) = file_name.strip_suffix(".jsonl") else {
        return;
    };
    if session_id != excluded_session_id
        && safe_session_filename(session_id)
        && regular_nonempty_file(path)
    {
        matches.push((session_id.to_string(), path.to_path_buf()));
    }
}

// purpose: Search one Claude config root for a session transcript.
// inputs: Config root, saved hook record, and optional cwd.
// returns/effects: Returns a nonempty transcript path when present.
fn claude_transcript_in_root(
    root: &Path,
    record: &AgentHookSessionRecord,
    cwd: Option<&str>,
) -> Option<String> {
    if let Some(cwd) = cwd {
        let project = root.join("projects").join(encode_claude_project_dir(cwd));
        if let Some(path) = claude_transcript_in_project(&project, &record.session_id) {
            return Some(path.display().to_string());
        }
    }
    read_dir_paths(&root.join("projects"))
        .into_iter()
        .find_map(|project| claude_transcript_in_project(&project, &record.session_id))
        .map(|path| path.display().to_string())
}

// purpose: Resolve Claude config roots from launch metadata and standard locations.
// inputs: Session-list options and one hook record.
// returns/effects: Returns deduplicated candidate roots.
fn claude_config_roots(record: &AgentHookSessionRecord) -> Vec<PathBuf> {
    let mut roots = Vec::new();
    push_unique_path(
        &mut roots,
        record
            .launch_command
            .as_ref()
            .and_then(|launch| launch.environment.get("CLAUDE_CONFIG_DIR"))
            .filter(|value| !value.trim().is_empty())
            .map(|value| expand_tilde(value.clone())),
    );
    if roots.is_empty() {
        if let Some(home) = dirs::home_dir() {
            push_unique_path(&mut roots, Some(home.join(".claude")));
            push_unique_path(&mut roots, Some(home.join(".subrouter/codex/claude")));
        }
    }
    roots
}

// purpose: Add a path to a list once after lightweight normalization.
// inputs: Mutable path list and optional candidate.
// returns/effects: Mutates the list only when the path is new.
fn push_unique_path(paths: &mut Vec<PathBuf>, path: Option<PathBuf>) {
    let Some(path) = path else {
        return;
    };
    if !paths.iter().any(|existing| existing == &path) {
        paths.push(path);
    }
}

// purpose: Search one Claude project directory for a direct or nested session transcript.
// inputs: Project directory and safe session id.
// returns/effects: Returns a nonempty transcript path when present.
fn claude_transcript_in_project(project: &Path, session_id: &str) -> Option<PathBuf> {
    let direct = project.join(format!("{session_id}.jsonl"));
    if regular_nonempty_file(&direct) {
        return Some(direct);
    }
    let nested = project
        .join(session_id)
        .join("messages")
        .join(format!("{session_id}.jsonl"));
    regular_nonempty_file(&nested).then_some(nested)
}

// purpose: List direct child paths from a directory.
// inputs: Directory path.
// returns/effects: Missing or unreadable directories return an empty list.
fn read_dir_paths(path: &Path) -> Vec<PathBuf> {
    fs::read_dir(path)
        .ok()
        .into_iter()
        .flat_map(|entries| entries.flatten().map(|entry| entry.path()))
        .collect()
}

// purpose: Check whether a path is a nonempty regular file.
// inputs: Candidate transcript path.
// returns/effects: Reads metadata only and returns false on missing/unreadable paths.
fn regular_nonempty_file(path: &Path) -> bool {
    fs::metadata(path)
        .map(|metadata| metadata.is_file() && metadata.len() > 0)
        .unwrap_or(false)
}

// purpose: Validate session ids before using them as local file names.
// inputs: Raw session id.
// returns/effects: Returns false for empty, dot, parent, or path-bearing ids.
fn safe_session_filename(session_id: &str) -> bool {
    !session_id.is_empty()
        && session_id != "."
        && session_id != ".."
        && !session_id.contains('/')
        && !session_id.contains('\\')
}

// purpose: Encode Claude's project directory naming scheme.
// inputs: Absolute project path.
// returns/effects: Replaces path separators and dots with dashes.
fn encode_claude_project_dir(path: &str) -> String {
    path.replace(['/', '.'], "-")
}

// purpose: Normalize optional freeform stored text.
// inputs: Raw string slice.
// returns/effects: Returns trimmed nonempty text.
fn normalized(value: &str) -> Option<String> {
    let trimmed = value.trim();
    (!trimmed.is_empty()).then(|| trimmed.to_string())
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
    record: &AgentHookSessionRecord,
    index: &CodexDebugIndex,
) {
    let session_id = &record.session_id;
    let pid_transcript = record
        .pid
        .and_then(|pid| codex_transcript_for_pid(pid, codex_home));
    let indexed_id = codex_indexed_session_id(session_id, pid_transcript.as_ref());
    let indexed_path = codex_indexed_transcript_path(session_id, pid_transcript.as_ref(), index);
    let session_dir = codex_home.join("sessions");
    payload["session_home"] = json!(codex_home);
    payload["session_dir"] = json!(session_dir);
    payload["codex_indexed"] = json!(index.indexed_session_ids.contains(indexed_id));
    set_codex_pid_payload_fields(payload, pid_transcript.as_ref());
    payload["codex_transcript_found"] = json!(indexed_path.is_some());
    payload["codex_transcript_path"] = indexed_path.map(Value::String).unwrap_or(Value::Null);
    if payload["transcript_path"].is_null() {
        payload["transcript_path"] = payload["codex_transcript_path"].clone();
    }
}

// purpose: Choose the Codex session id used for index membership checks.
// inputs: Saved wrapper session id and optional pid-resolved native Codex transcript.
// returns/effects: Returns the native Codex id when available, otherwise the saved id.
fn codex_indexed_session_id<'a>(
    saved_session_id: &'a str,
    pid_transcript: Option<&'a CodexPidTranscript>,
) -> &'a str {
    pid_transcript
        .map(|transcript| transcript.session_id.as_str())
        .unwrap_or(saved_session_id)
}

// purpose: Resolve the best Codex transcript path for one saved session.
// inputs: Saved session id, optional pid-resolved transcript, and Codex debug index.
// returns/effects: Prefers indexed saved id, then indexed native id, then live pid fd path.
fn codex_indexed_transcript_path(
    saved_session_id: &str,
    pid_transcript: Option<&CodexPidTranscript>,
    index: &CodexDebugIndex,
) -> Option<String> {
    index
        .transcript_path_by_session_id
        .get(saved_session_id)
        .cloned()
        .or_else(|| {
            pid_transcript
                .and_then(|transcript| {
                    index
                        .transcript_path_by_session_id
                        .get(&transcript.session_id)
                })
                .cloned()
        })
        .or_else(|| pid_transcript.map(|transcript| transcript.path.clone()))
}

// purpose: Add pid-anchored Codex transcript diagnostics to a JSON payload.
// inputs: Mutable session payload and optional pid-resolved native Codex transcript.
// returns/effects: Mutates payload with native Codex id/path presence fields.
fn set_codex_pid_payload_fields(payload: &mut Value, pid_transcript: Option<&CodexPidTranscript>) {
    payload["codex_native_session_id"] = pid_transcript
        .map(|transcript| json!(transcript.session_id))
        .unwrap_or(Value::Null);
    payload["codex_pid_transcript_found"] = json!(pid_transcript.is_some());
    payload["codex_pid_transcript_path"] = pid_transcript
        .map(|transcript| json!(transcript.path))
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

// purpose: Resolve a live Codex transcript from a saved Linux process id.
// inputs: Process id and configured Codex home directory.
// returns/effects: Reads /proc/<pid>/fd symlinks on Linux; returns None elsewhere.
fn codex_transcript_for_pid(pid: u32, codex_home: &Path) -> Option<CodexPidTranscript> {
    if cfg!(target_os = "linux") {
        codex_transcript_from_fd_dir(
            &Path::new("/proc").join(pid.to_string()).join("fd"),
            codex_home,
        )
    } else {
        None
    }
}

// purpose: Resolve a Codex transcript from a directory of process fd symlinks.
// inputs: fd directory and configured Codex home.
// returns/effects: Returns the first JSONL fd target under Codex sessions roots with UUID token.
fn codex_transcript_from_fd_dir(fd_dir: &Path, codex_home: &Path) -> Option<CodexPidTranscript> {
    let mut candidates = fs::read_dir(fd_dir)
        .ok()?
        .flatten()
        .filter_map(|entry| fs::read_link(entry.path()).ok())
        .filter_map(|target| codex_transcript_from_path(&target, codex_home))
        .collect::<Vec<_>>();
    candidates.sort_by(|left, right| {
        left.path
            .cmp(&right.path)
            .then_with(|| left.session_id.cmp(&right.session_id))
    });
    candidates.into_iter().next()
}

// purpose: Validate and decode a Codex transcript path.
// inputs: Candidate fd target and configured Codex home.
// returns/effects: Returns session id/path only for Codex JSONL transcript files.
fn codex_transcript_from_path(path: &Path, codex_home: &Path) -> Option<CodexPidTranscript> {
    if path.extension().and_then(|ext| ext.to_str()) != Some("jsonl") {
        return None;
    }
    if path.is_absolute() && !path.starts_with(codex_home) {
        return None;
    }
    if !path.components().any(|component| {
        matches!(
            component.as_os_str().to_str(),
            Some("sessions" | "archived_sessions")
        )
    }) {
        return None;
    }
    let name = path.file_name()?.to_str()?;
    let session_id = uuid_like_tokens(name).into_iter().next()?;
    Some(CodexPidTranscript {
        session_id,
        path: path.display().to_string(),
    })
}

// purpose: Extract UUID-shaped tokens from transcript file names.
// inputs: A file name or path component.
// returns/effects: Returns lowercased UUID-shaped strings.
fn uuid_like_tokens(value: &str) -> Vec<String> {
    let chars = value.chars().collect::<Vec<_>>();
    if chars.len() < 36 {
        return Vec::new();
    }
    let mut tokens = Vec::new();
    for start in 0..=(chars.len() - 36) {
        let token = chars[start..start + 36].iter().collect::<String>();
        if is_uuid_like_token(&token) && !tokens.contains(&token) {
            tokens.push(token.to_ascii_lowercase());
        }
    }
    tokens
}

// purpose: Validate UUID-shaped transcript ids embedded in filenames.
// inputs: Candidate 36-character string.
// returns/effects: Returns true only for 8-4-4-4-12 hexadecimal UUID shape.
fn is_uuid_like_token(value: &str) -> bool {
    if value.len() != 36 {
        return false;
    }
    value.chars().enumerate().all(|(index, ch)| {
        if matches!(index, 8 | 13 | 18 | 23) {
            ch == '-'
        } else {
            ch.is_ascii_hexdigit()
        }
    })
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
        yes_no(
            payload
                .get("fork_command")
                .and_then(Value::as_str)
                .is_some_and(|value| !value.is_empty())
        )
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
        if let Some(native) = payload
            .get("codex_native_session_id")
            .and_then(Value::as_str)
            .filter(|value| !value.is_empty())
        {
            parts.push(format!("codex_native_session={native}"));
        }
        parts.push(format!(
            "codex_indexed={}",
            yes_no(payload_bool(payload, "codex_indexed"))
        ));
        parts.push(format!(
            "codex_transcript={}",
            yes_no(payload_bool(payload, "codex_transcript_found"))
        ));
        parts.push(format!(
            "codex_pid_transcript={}",
            yes_no(payload_bool(payload, "codex_pid_transcript_found"))
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
    use std::os::unix::fs::symlink;
    use std::os::unix::fs::PermissionsExt;
    use tempfile::tempdir;

    struct ClaudeSeed {
        is_restorable: Option<bool>,
        transcript_path: Option<String>,
        environment: BTreeMap<String, String>,
    }

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
                is_restorable: None,
                transcript_path: None,
                launch_command: Some(AgentLaunchCommandRecord {
                    executable: "codex".to_string(),
                    arguments: vec![
                        "codex".to_string(),
                        "resume".to_string(),
                        "old-session".to_string(),
                        "--model".to_string(),
                        "gpt-5".to_string(),
                        "--api-key".to_string(),
                        "SECRET".to_string(),
                    ],
                    cwd: Some("/tmp/project".to_string()),
                    environment: BTreeMap::from([(
                        "CODEX_HOME".to_string(),
                        "/tmp/codex home".to_string(),
                    )]),
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

    // purpose: Exercise JSON session-list output for CMUX-compatible fork command diagnostics.
    // inputs: Temporary hook state directory with one Codex launch record.
    // returns/effects: Asserts JSON exposes the concrete fork command and strips secrets.
    fn assert_sessions_list_json_includes_fork_command() {
        let dir = tempdir().expect("tempdir");
        seed_stale_codex_record(dir.path());
        let output = SessionCommandResult::from(SessionCommandInput {
            args: vec![
                "list".to_string(),
                "--agent".to_string(),
                "codex".to_string(),
                "--state-dir".to_string(),
                dir.path().display().to_string(),
                "--json".to_string(),
            ],
            global_json: false,
        });

        let SessionCommandResult::Output(SessionCommandOutput::Json(value)) = output else {
            panic!("expected json");
        };
        let command = value["sessions"][0]["fork_command"]
            .as_str()
            .expect("fork command");
        assert_eq!(
            command,
            concat!(
                "cd -- '/tmp/project' 2>/dev/null || [ ! -d '/tmp/project' ] && ",
                "'env' 'CODEX_HOME=/tmp/codex home' 'codex' 'fork' ",
                "'session-a' '--model' 'gpt-5'"
            )
        );
        assert_eq!(value["sessions"][0]["fork_command_available"], true);
        assert_eq!(value["sessions"][0]["fork_supported"], true);
        assert!(!command.contains("SECRET"));
    }

    #[test]
    fn sessions_list_json_includes_fork_command() {
        assert_sessions_list_json_includes_fork_command();
    }

    // purpose: Exercise CMUX OpenCode minimum version parsing for fork support.
    // inputs: Representative OpenCode version command outputs.
    // returns/effects: Asserts only versions at or above 1.14.50 are accepted.
    fn assert_opencode_version_supports_fork_threshold() {
        assert!(!opencode_version_supports_fork("opencode 1.14.48"));
        assert!(opencode_version_supports_fork("opencode 1.14.50"));
        assert!(opencode_version_supports_fork("opencode version 1.15.0"));
        assert!(!opencode_version_supports_fork("not a version"));
    }

    #[test]
    fn opencode_version_supports_fork_threshold() {
        assert_opencode_version_supports_fork_threshold();
    }

    // purpose: Exercise local OpenCode version probe support diagnostics.
    // inputs: Temporary executable reporting a fork-capable OpenCode version.
    // returns/effects: Asserts fork command remains available and fork is supported.
    fn assert_opencode_supported_version_enables_fork() {
        let dir =
            seed_versioned_opencode_record("opencode-session-supported", "opencode 1.14.50\n");
        let value = sessions_json_for(dir.path(), "opencode", "opencode-session-supported");
        assert_opencode_session_fork_state(&value, true, true, "available");
    }

    #[test]
    fn opencode_supported_version_enables_fork() {
        assert_opencode_supported_version_enables_fork();
    }

    // purpose: Exercise local OpenCode version probe rejection diagnostics.
    // inputs: Temporary executable reporting an OpenCode version before the fork fix.
    // returns/effects: Asserts command is renderable but fork support is false.
    fn assert_opencode_unsupported_version_disables_fork_support() {
        let dir =
            seed_versioned_opencode_record("opencode-session-unsupported", "opencode 1.14.48\n");
        let value = sessions_json_for(dir.path(), "opencode", "opencode-session-unsupported");
        assert_opencode_session_fork_state(&value, true, false, "opencode_version_unsupported");
    }

    #[test]
    fn opencode_unsupported_version_disables_fork_support() {
        assert_opencode_unsupported_version_disables_fork_support();
    }

    // purpose: Exercise CMUX remote-like OpenCode bypass behavior.
    // inputs: Missing local working directory with an otherwise normal launch record.
    // returns/effects: Asserts fork support is trusted without a local version probe.
    fn assert_opencode_remote_like_context_bypasses_local_probe() {
        let dir = tempdir().expect("tempdir");
        let remote_cwd = dir.path().join("remote-project-does-not-exist");
        seed_opencode_record(
            dir.path(),
            "opencode-session-remote",
            &remote_cwd,
            "/remote/bin/opencode",
            BTreeMap::from([("PATH".to_string(), "/remote/bin:/usr/bin".to_string())]),
        );

        let value = sessions_json_for(dir.path(), "opencode", "opencode-session-remote");
        assert_opencode_session_fork_state(&value, true, true, "available");
    }

    #[test]
    fn opencode_remote_like_context_bypasses_local_probe() {
        assert_opencode_remote_like_context_bypasses_local_probe();
    }

    // purpose: Exercise CMUX missing absolute OpenCode executable rejection.
    // inputs: Local cwd and absent absolute executable path.
    // returns/effects: Asserts fork command is renderable but marked unsupported.
    fn assert_opencode_missing_absolute_executable_disables_fork_support() {
        let dir = tempdir().expect("tempdir");
        let missing = dir.path().join("missing-opencode");
        seed_opencode_record(
            dir.path(),
            "opencode-session-missing",
            dir.path(),
            &missing,
            BTreeMap::new(),
        );

        let value = sessions_json_for(dir.path(), "opencode", "opencode-session-missing");
        assert_opencode_session_fork_state(&value, true, false, "opencode_executable_missing");
    }

    // purpose: Seed an OpenCode record backed by a local version-printing executable.
    // inputs: Session id and version probe output.
    // returns/effects: Returns a tempdir containing the hook store and probe executable.
    fn seed_versioned_opencode_record(session_id: &str, version_output: &str) -> tempfile::TempDir {
        let dir = tempdir().expect("tempdir");
        let executable = write_opencode_probe(dir.path(), version_output);
        seed_opencode_record(
            dir.path(),
            session_id,
            dir.path(),
            &executable,
            BTreeMap::from([("PATH".to_string(), dir.path().display().to_string())]),
        );
        dir
    }

    // purpose: Assert OpenCode fork support diagnostics for the first session row.
    // inputs: Rendered sessions JSON and expected diagnostic values.
    // returns/effects: Panics if any fork diagnostic diverges.
    fn assert_opencode_session_fork_state(
        value: &Value,
        command_available: bool,
        supported: bool,
        reason: &str,
    ) {
        let session = &value["sessions"][0];
        assert_eq!(session["fork_command_available"], command_available);
        assert_eq!(session["fork_supported"], supported);
        assert_eq!(session["fork_unavailable_reason"], reason);
    }

    #[test]
    fn opencode_missing_absolute_executable_disables_fork_support() {
        assert_opencode_missing_absolute_executable_disables_fork_support();
    }

    // purpose: Exercise CMUX Claude transcript trust behavior for session diagnostics.
    // inputs: Temporary Claude hook store with an existing transcript path.
    // returns/effects: Asserts transcript evidence overrides a false stored restorable flag.
    fn assert_claude_transcript_backed_record_is_restorable() {
        let dir = tempdir().expect("tempdir");
        let repo = dir.path().join("repo");
        let transcript = dir.path().join("claude-session.jsonl");
        fs::create_dir_all(&repo).expect("repo");
        fs::write(&transcript, "{}\n").expect("transcript");
        seed_claude_record(
            dir.path(),
            "claude-session",
            &repo,
            ClaudeSeed {
                is_restorable: Some(false),
                transcript_path: Some(transcript.display().to_string()),
                environment: BTreeMap::new(),
            },
        );

        let value = sessions_json_for(dir.path(), "claude", "claude-session");
        let session = &value["sessions"][0];
        assert_eq!(session["hook_record_restorable"], true);
        assert_eq!(session["fork_command_available"], true);
        assert_eq!(session["fork_supported"], true);
        assert_eq!(session["fork_unavailable_reason"], "available");
    }

    #[test]
    fn claude_transcript_backed_record_is_restorable() {
        assert_claude_transcript_backed_record_is_restorable();
    }

    // purpose: Exercise CMUX Claude distrust behavior when no transcript evidence exists.
    // inputs: Temporary Claude hook store with no transcript file.
    // returns/effects: Asserts session diagnostics mark fork unavailable.
    fn assert_claude_without_transcript_is_not_restorable() {
        let dir = tempdir().expect("tempdir");
        let repo = dir.path().join("repo");
        fs::create_dir_all(&repo).expect("repo");
        seed_claude_record(
            dir.path(),
            "claude-no-transcript",
            &repo,
            ClaudeSeed {
                is_restorable: Some(true),
                transcript_path: None,
                environment: BTreeMap::new(),
            },
        );

        let value = sessions_json_for(dir.path(), "claude", "claude-no-transcript");
        let session = &value["sessions"][0];
        assert_eq!(session["hook_record_restorable"], false);
        assert_eq!(session["fork_command_available"], false);
        assert_eq!(session["fork_supported"], false);
        assert_eq!(
            session["fork_unavailable_reason"],
            "record_marked_non_restorable"
        );
    }

    #[test]
    fn claude_without_transcript_is_not_restorable() {
        assert_claude_without_transcript_is_not_restorable();
    }

    // purpose: Exercise CMUX-compatible Claude transcript lookup in CLAUDE_CONFIG_DIR.
    // inputs: Hook record without transcript path and a Claude projects transcript file.
    // returns/effects: Asserts diagnostics trust the located transcript.
    fn assert_claude_transcript_lookup_uses_launch_config_dir() {
        let dir = tempdir().expect("tempdir");
        let repo = dir.path().join("repo.with.dot");
        let config = dir.path().join("claude-config");
        let session_id = "claude-config-lookup";
        let project = config
            .join("projects")
            .join(encode_claude_project_dir(repo.to_str().expect("repo path")));
        fs::create_dir_all(&project).expect("project");
        fs::create_dir_all(&repo).expect("repo");
        fs::write(project.join(format!("{session_id}.jsonl")), "{}\n").expect("transcript");
        let env = BTreeMap::from([(
            "CLAUDE_CONFIG_DIR".to_string(),
            config.display().to_string(),
        )]);
        seed_claude_record(
            dir.path(),
            session_id,
            &repo,
            ClaudeSeed {
                is_restorable: Some(false),
                transcript_path: None,
                environment: env,
            },
        );

        let value = sessions_json_for(dir.path(), "claude", session_id);
        let session = &value["sessions"][0];
        assert_eq!(session["hook_record_restorable"], true);
        assert_eq!(session["fork_unavailable_reason"], "available");
        assert_eq!(session["fork_command_available"], true);
    }

    #[test]
    fn claude_transcript_lookup_uses_launch_config_dir() {
        assert_claude_transcript_lookup_uses_launch_config_dir();
    }

    // purpose: Exercise CMUX Claude workflow-container transcript resolution.
    // inputs: Hook record whose transcriptPath is a workflow directory with one sibling JSONL.
    // returns/effects: Asserts diagnostics use the sibling transcript session id.
    fn assert_claude_workflow_container_resolves_single_sibling_transcript() {
        let dir = tempdir().expect("tempdir");
        let repo = dir.path().join("repo");
        let config = dir.path().join("claude-config");
        let container_id = "aaaaaaaa-1111-1111-1111-aaaaaaaaaaaa";
        let sibling_id = "bbbbbbbb-2222-2222-2222-bbbbbbbbbbbb";
        let project = config
            .join("projects")
            .join(encode_claude_project_dir(repo.to_str().expect("repo path")));
        let container = project.join(container_id);
        fs::create_dir_all(&container).expect("container");
        fs::create_dir_all(&repo).expect("repo");
        fs::write(project.join(format!("{sibling_id}.jsonl")), "{}\n").expect("transcript");
        seed_claude_record(
            dir.path(),
            container_id,
            &repo,
            ClaudeSeed {
                is_restorable: Some(false),
                transcript_path: Some(container.display().to_string()),
                environment: BTreeMap::from([(
                    "CLAUDE_CONFIG_DIR".to_string(),
                    config.display().to_string(),
                )]),
            },
        );

        let value = sessions_json_for(dir.path(), "claude", container_id);
        let session = &value["sessions"][0];
        assert_eq!(session["session_id"], sibling_id);
        assert_eq!(session["hook_record_restorable"], true);
        assert_eq!(session["fork_command_available"], true);
        assert!(session["transcript_path"]
            .as_str()
            .expect("transcript path")
            .ends_with(&format!("{sibling_id}.jsonl")));
    }

    #[test]
    fn claude_workflow_container_resolves_single_sibling_transcript() {
        assert_claude_workflow_container_resolves_single_sibling_transcript();
    }

    // purpose: Create a temporary OpenCode probe executable for session diagnostics tests.
    // inputs: Directory where the executable should live and output to print.
    // returns/effects: Writes an executable shell script and returns its path.
    fn write_opencode_probe(dir: &Path, output: &str) -> PathBuf {
        let executable = dir.join("opencode");
        fs::write(
            &executable,
            format!(
                "#!/bin/sh\nprintf '%s' '{}'\n",
                output.replace('\'', "'\\''")
            ),
        )
        .expect("probe executable");
        let mut permissions = fs::metadata(&executable).expect("metadata").permissions();
        permissions.set_mode(0o755);
        fs::set_permissions(&executable, permissions).expect("permissions");
        executable
    }

    // purpose: Seed one OpenCode session record for fork-support diagnostics tests.
    // inputs: Hook state dir, session id, cwd, executable, and preserved environment.
    // returns/effects: Writes one OpenCode hook store record.
    fn seed_opencode_record(
        dir: &Path,
        session_id: &str,
        cwd: &Path,
        executable: impl AsRef<Path>,
        environment: BTreeMap<String, String>,
    ) {
        let executable = executable.as_ref().display().to_string();
        let store = AgentHookSessionStore::new_for_dir("opencode", dir);
        store
            .upsert(AgentHookSessionRecord {
                session_id: session_id.to_string(),
                workspace_id: "workspace-opencode".to_string(),
                surface_id: "surface-opencode".to_string(),
                cwd: Some(cwd.display().to_string()),
                pid: None,
                is_restorable: Some(true),
                transcript_path: None,
                launch_command: Some(AgentLaunchCommandRecord {
                    executable: executable.clone(),
                    arguments: vec![executable],
                    cwd: Some(cwd.display().to_string()),
                    environment,
                    captured_at: 1.0,
                }),
                updated_at: 10.0,
            })
            .expect("seed opencode record");
    }

    // purpose: Seed one Claude session record for diagnostics tests.
    // inputs: Hook state dir, session metadata, optional transcript, and launch environment.
    // returns/effects: Writes one Claude hook store record.
    fn seed_claude_record(dir: &Path, session_id: &str, repo: &Path, seed: ClaudeSeed) {
        let store = AgentHookSessionStore::new_for_dir("claude", dir);
        store
            .upsert(AgentHookSessionRecord {
                session_id: session_id.to_string(),
                workspace_id: "workspace-a".to_string(),
                surface_id: "surface-a".to_string(),
                cwd: Some(repo.display().to_string()),
                pid: None,
                is_restorable: seed.is_restorable,
                transcript_path: seed.transcript_path,
                launch_command: Some(AgentLaunchCommandRecord {
                    executable: "claude".to_string(),
                    arguments: vec!["claude".to_string()],
                    cwd: Some(repo.display().to_string()),
                    environment: seed.environment,
                    captured_at: 1.0,
                }),
                updated_at: 20.0,
            })
            .expect("store record");
    }

    // purpose: Exercise CMUX-compatible tolerance for out-of-range stored PID values.
    // inputs: Manually-written hook store containing a PID larger than u32.
    // returns/effects: Asserts listing succeeds and reports null PID existence.
    fn assert_sessions_list_ignores_out_of_range_pid() {
        let dir = tempdir().expect("tempdir");
        let store = dir.path().join("codex-hook-sessions.json");
        fs::write(
            &store,
            r#"{
              "version": 1,
              "sessions": {
                "session-a": {
                  "session_id": "session-a",
                  "workspace_id": "workspace-a",
                  "surface_id": "surface-a",
                  "pid": 999999999999,
                  "launch_command": {
                    "executable": "codex",
                    "arguments": ["codex"],
                    "captured_at": 1.0
                  },
                  "updated_at": 20.0
                }
              }
            }"#,
        )
        .expect("store");

        let value = sessions_json_for(dir.path(), "codex", "session-a");
        assert_eq!(value["sessions"][0]["stored_pid_exists"], Value::Null);
    }

    #[test]
    fn sessions_list_ignores_out_of_range_pid() {
        assert_sessions_list_ignores_out_of_range_pid();
    }

    // purpose: Run sessions list JSON for a single test record.
    // inputs: Hook state directory, agent name, and session id.
    // returns/effects: Returns parsed JSON output or panics in tests.
    fn sessions_json_for(dir: &Path, agent: &str, session: &str) -> Value {
        let output = sessions_json_command(vec![
            "list".to_string(),
            "--agent".to_string(),
            agent.to_string(),
            "--session".to_string(),
            session.to_string(),
            "--state-dir".to_string(),
            dir.display().to_string(),
            "--json".to_string(),
        ]);
        expect_sessions_json(output)
    }

    // purpose: Run a sessions JSON command for tests.
    // inputs: Raw sessions command args.
    // returns/effects: Returns the typed command result.
    fn sessions_json_command(args: Vec<String>) -> SessionCommandResult {
        SessionCommandResult::from(SessionCommandInput {
            args,
            global_json: false,
        })
    }

    // purpose: Extract JSON from a sessions command result.
    // inputs: Command result.
    // returns/effects: Returns parsed JSON or panics in tests.
    fn expect_sessions_json(output: SessionCommandResult) -> Value {
        let SessionCommandResult::Output(SessionCommandOutput::Json(value)) = output else {
            panic!("expected json output");
        };
        value
    }

    // purpose: Exercise JSON output for empty session-list stores.
    // inputs: Empty temporary hook state directory.
    // returns/effects: Asserts JSON shape and missing-store metadata.
    fn assert_sessions_list_json_includes_store_metadata() {
        let dir = tempdir().expect("tempdir");
        let value = expect_sessions_json(sessions_json_command(vec![
            "debug".to_string(),
            "--agent".to_string(),
            "claude".to_string(),
            "--state-dir".to_string(),
            dir.path().display().to_string(),
            "--json".to_string(),
        ]));
        assert_eq!(value["total_matches"], 0);
        assert_eq!(value["stores"][0]["agent"], "claude");
        assert_eq!(value["stores"][0]["exists"], false);
    }

    #[test]
    fn sessions_list_json_includes_store_metadata() {
        assert_sessions_list_json_includes_store_metadata();
    }

    // purpose: Verify fd-based Codex transcript resolution rejects unrelated open files.
    // inputs: Synthetic fd directory with Codex-home and outside-home JSONL symlinks.
    // returns/effects: Asserts only the Codex sessions transcript path and UUID are returned.
    fn assert_codex_fd_resolver_accepts_only_codex_home_jsonl_transcripts() {
        let dir = tempdir().expect("tempdir");
        let codex_home = dir.path().join(".codex");
        let session_dir = codex_home
            .join("sessions")
            .join("2026")
            .join("07")
            .join("02");
        fs::create_dir_all(&session_dir).expect("create sessions dir");
        let transcript = session_dir.join("rollout-11111111-2222-3333-4444-555555555555.jsonl");
        fs::write(&transcript, "{}\n").expect("write transcript");
        let outside = dir
            .path()
            .join("other")
            .join("99999999-2222-3333-4444-555555555555.jsonl");
        fs::create_dir_all(outside.parent().expect("outside parent")).expect("outside dir");
        fs::write(&outside, "{}\n").expect("write outside transcript");

        let fd_dir = dir.path().join("fd");
        fs::create_dir_all(&fd_dir).expect("create fd dir");
        symlink(&outside, fd_dir.join("3")).expect("outside symlink");
        symlink(&transcript, fd_dir.join("4")).expect("transcript symlink");

        let resolved =
            codex_transcript_from_fd_dir(&fd_dir, &codex_home).expect("resolve transcript");

        assert_eq!(resolved.session_id, "11111111-2222-3333-4444-555555555555");
        assert_eq!(resolved.path, transcript.display().to_string());
    }

    #[test]
    fn codex_fd_resolver_accepts_only_codex_home_jsonl_transcripts() {
        assert_codex_fd_resolver_accepts_only_codex_home_jsonl_transcripts();
    }

    // purpose: Verify static Codex transcript path validation is strict.
    // inputs: Candidate transcript paths with valid and invalid homes, roots, and extensions.
    // returns/effects: Asserts only Codex-home sessions JSONL files with UUID tokens are accepted.
    fn assert_codex_transcript_path_requires_codex_sessions_jsonl_with_uuid() {
        let codex_home = PathBuf::from("/home/user/.codex");
        assert!(codex_transcript_from_path(
            Path::new(
                "/home/user/.codex/sessions/rollout-11111111-2222-3333-4444-555555555555.jsonl"
            ),
            &codex_home,
        )
        .is_some());
        assert!(codex_transcript_from_path(
            Path::new("/home/user/.codex/not-sessions/11111111-2222-3333-4444-555555555555.jsonl"),
            &codex_home,
        )
        .is_none());
        assert!(codex_transcript_from_path(
            Path::new("/tmp/sessions/11111111-2222-3333-4444-555555555555.jsonl"),
            &codex_home,
        )
        .is_none());
        assert!(codex_transcript_from_path(
            Path::new("/home/user/.codex/sessions/not-a-session.txt"),
            &codex_home,
        )
        .is_none());
    }

    #[test]
    fn codex_transcript_path_requires_codex_sessions_jsonl_with_uuid() {
        assert_codex_transcript_path_requires_codex_sessions_jsonl_with_uuid();
    }
}
