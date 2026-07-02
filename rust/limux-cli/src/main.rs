// summary: Provide the Limux CLI for host launch, socket control, hooks, and compatibility commands.
// purpose: Parse user commands into explicit Limux control requests and manage local CLI state safely.
// inputs: CLI arguments, environment variables, Unix control sockets, and JSON hook/config files.
// returns/effects: Launches the host or sends bounded control requests, writes local state, and exits nonzero on errors.

use std::collections::{BTreeMap, BTreeSet};
use std::env;
use std::fs::{self, OpenOptions};
use std::hash::{Hash, Hasher};
use std::io::{self, ErrorKind, Write};
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use anyhow::{anyhow, bail, Context, Result};
use limux_control::auth;
use limux_control::socket_path::{resolve_socket_path_checked, SocketMode};
use limux_protocol::{V2Request, V2Response};
use serde_json::{json, Map, Value};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::UnixStream;

mod agent_hooks;
mod codex_teams;
mod sessions;

const CLI_STATE_LOCK_TIMEOUT: Duration = Duration::from_secs(2);
const CLI_STATE_LOCK_RETRY: Duration = Duration::from_millis(25);
const PRIVATE_CLI_DIR_MODE: u32 = 0o700;
const WAIT_MARKER_MODE: u32 = 0o600;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum IdFormat {
    Refs,
    Both,
    Uuids,
}

impl IdFormat {
    fn parse(raw: &str) -> Result<Self> {
        match raw {
            "refs" => Ok(Self::Refs),
            "both" => Ok(Self::Both),
            "uuids" => Ok(Self::Uuids),
            _ => bail!("--id-format must be one of refs|both|uuids"),
        }
    }
}

#[derive(Debug, Clone)]
struct GlobalOptions {
    socket: Option<PathBuf>,
    socket_mode: SocketMode,
    password: Option<String>,
    window: Option<String>,
    json_output: bool,
    id_format: IdFormat,
    request: Option<String>,
    pretty: bool,
    command_args: Vec<String>,
}

#[derive(Debug)]
enum CommandOutput {
    Silent,
    Text(String),
    Exit {
        stdout: String,
        stderr: String,
        status: i32,
    },
    Json(Value),
}

struct Client {
    socket: PathBuf,
    password: Option<String>,
    seq: u64,
}

impl Client {
    fn new(socket: PathBuf, password: Option<String>) -> Self {
        Self {
            socket,
            password,
            seq: 0,
        }
    }

    async fn call(&mut self, method: &str, params: Value) -> Result<Value> {
        self.seq = self.seq.saturating_add(1);
        let request = V2Request {
            id: Some(Value::String(format!("cli-{}", self.seq))),
            method: method.to_string(),
            params,
        };
        self.send_request(request).await
    }

    async fn send_request(&self, request: V2Request) -> Result<Value> {
        let stream = UnixStream::connect(&self.socket)
            .await
            .with_context(|| format!("failed to connect to socket {}", self.socket.display()))?;
        let (reader_half, mut writer_half) = stream.into_split();
        let mut reader = BufReader::new(reader_half);

        if let Some(password) = &self.password {
            let auth_request = V2Request {
                id: Some(Value::String("cli-auth".to_string())),
                method: "auth.login".to_string(),
                params: json!({ "password": password }),
            };
            Self::write_request(&mut writer_half, &auth_request).await?;
            let auth_response = Self::read_response(&mut reader).await?;
            if !auth_response.ok {
                let err = auth_response
                    .error
                    .ok_or_else(|| anyhow!("server returned !ok without error payload"))?;
                bail!("{}: {}", err.code, err.message);
            }
        }

        Self::write_request(&mut writer_half, &request).await?;
        Self::read_success_response(&mut reader).await
    }

    async fn write_request(
        writer: &mut tokio::net::unix::OwnedWriteHalf,
        request: &V2Request,
    ) -> Result<()> {
        let mut payload = serde_json::to_string(request).context("failed to encode request")?;
        payload.push('\n');

        writer
            .write_all(payload.as_bytes())
            .await
            .context("failed to write request")?;
        writer.flush().await.context("failed to flush request")?;
        Ok(())
    }

    async fn read_response(
        reader: &mut BufReader<tokio::net::unix::OwnedReadHalf>,
    ) -> Result<V2Response> {
        let mut line = String::new();
        reader
            .read_line(&mut line)
            .await
            .context("failed to read response")?;

        if line.trim().is_empty() {
            bail!("server returned an empty response");
        }

        serde_json::from_str(line.trim()).context("response was not valid v2 JSON")
    }

    async fn read_success_response(
        reader: &mut BufReader<tokio::net::unix::OwnedReadHalf>,
    ) -> Result<Value> {
        let response = Self::read_response(reader).await?;
        if response.ok {
            Ok(response.result.unwrap_or_else(|| json!({})))
        } else {
            let err = response
                .error
                .ok_or_else(|| anyhow!("server returned !ok without error payload"))?;
            if err.code == -32004 {
                bail!("not_found: {}", err.message);
            }
            bail!("{}: {}", err.code, err.message);
        }
    }
}

fn parse_global_args() -> Result<GlobalOptions> {
    parse_global_args_from(env::args().skip(1).collect())
}

/// purpose: Parse Limux and CMUX-compatible global CLI flags.
/// inputs: args are raw process arguments after argv[0].
/// returns/effects: Returns structured options, failing loudly on malformed globals.
fn parse_global_args_from(mut args: Vec<String>) -> Result<GlobalOptions> {
    let mut socket: Option<PathBuf> = None;
    let mut socket_mode = SocketMode::Runtime;
    let mut password: Option<String> = None;
    let mut window: Option<String> = None;
    let mut json_output = false;
    let mut id_format = IdFormat::Refs;
    let mut request: Option<String> = None;
    let mut pretty = false;

    let mut command_start = 0usize;
    while command_start < args.len() {
        let arg = args[command_start].clone();
        if !arg.starts_with('-') {
            break;
        }
        match arg.as_str() {
            "--socket" => {
                let value = args
                    .get(command_start + 1)
                    .ok_or_else(|| anyhow!("--socket requires a value"))?;
                socket = Some(PathBuf::from(value));
                command_start += 2;
            }
            "--password" => {
                let value = args
                    .get(command_start + 1)
                    .ok_or_else(|| anyhow!("--password requires a value"))?;
                password = Some(value.clone());
                command_start += 2;
            }
            "--window" => {
                let value = args
                    .get(command_start + 1)
                    .ok_or_else(|| anyhow!("--window requires a value"))?;
                window = Some(value.clone());
                command_start += 2;
            }
            "--socket-mode" => {
                let value = args
                    .get(command_start + 1)
                    .ok_or_else(|| anyhow!("--socket-mode requires runtime|debug"))?;
                socket_mode = match value.as_str() {
                    "runtime" => SocketMode::Runtime,
                    "debug" => SocketMode::Debug,
                    _ => bail!("--socket-mode must be runtime or debug"),
                };
                command_start += 2;
            }
            "--json" => {
                json_output = true;
                command_start += 1;
            }
            "--id-format" => {
                let value = args
                    .get(command_start + 1)
                    .ok_or_else(|| anyhow!("--id-format requires refs|both|uuids"))?;
                id_format = IdFormat::parse(value)?;
                command_start += 2;
            }
            "--request" => {
                let value = args
                    .get(command_start + 1)
                    .ok_or_else(|| anyhow!("--request requires a JSON value"))?;
                request = Some(value.clone());
                command_start += 2;
            }
            "--pretty" => {
                pretty = true;
                command_start += 1;
            }
            "--help" | "-h" => {
                print_help();
                std::process::exit(0);
            }
            "--version" | "-v" => {
                args = vec!["version".to_string()];
                command_start = 0;
                break;
            }
            _ => break,
        }
    }

    let mut command_args = args.split_off(command_start);
    parse_command_presentation_flags(&mut command_args, &mut json_output, &mut id_format)?;

    Ok(GlobalOptions {
        socket,
        socket_mode,
        password,
        window,
        json_output,
        id_format,
        request,
        pretty,
        command_args,
    })
}

/// purpose: Accept CMUX presentation flags before or after the command name.
/// inputs: Mutable command args plus global output-format state.
/// returns/effects: Removes consumed presentation flags or fails on malformed values.
fn parse_command_presentation_flags(
    command_args: &mut Vec<String>,
    json_output: &mut bool,
    id_format: &mut IdFormat,
) -> Result<()> {
    let mut normalized = Vec::with_capacity(command_args.len());
    let mut idx = 0usize;
    while idx < command_args.len() {
        match command_args[idx].as_str() {
            "--" => {
                normalized.extend_from_slice(&command_args[idx..]);
                break;
            }
            "--json" => {
                *json_output = true;
                idx += 1;
            }
            "--id-format" => {
                let value = command_args
                    .get(idx + 1)
                    .ok_or_else(|| anyhow!("--id-format requires refs|both|uuids"))?;
                *id_format = IdFormat::parse(value)?;
                idx += 2;
            }
            _ => {
                normalized.push(command_args[idx].clone());
                idx += 1;
            }
        }
    }
    *command_args = normalized;
    Ok(())
}

fn print_help() {
    println!("{}", full_help_text());
}

fn help_text() -> &'static str {
    concat!(
        "limux CLI\n\n",
        "Usage: limux [--socket <path>] [--json] [--id-format refs|both|uuids] ",
        "<command> [args...]\n",
        "Use `limux --help` for the full command list."
    )
}

fn version_text() -> String {
    format!("limux {}", env!("CARGO_PKG_VERSION"))
}

fn full_help_text() -> &'static str {
    concat!(
        "limux CLI\n\n",
        "Usage: limux [--socket <path>] [--json] [--id-format refs|both|uuids] ",
        "<command> [args...]\n",
        "       limux\n\n",
        "Running `limux` with no arguments launches the GTK app.\n\n",
        "Common commands:\n",
        "  identify [--workspace <id|ref>] [--surface <id|ref>]\n",
        "  rpc <method> [json-params]\n",
        "  events [--after <seq>] [--category <name>] [--name <event>] [--limit <n>]\n",
        "  capabilities\n",
        "  list-panels [--workspace <id|ref>]\n",
        "  list-panes [--workspace <id|ref>]\n",
        "  list-workspaces\n",
        "  list-workspace-groups\n",
        "  workspace-group list\n",
        "  current-workspace\n",
        "  memory [--groups <count>]\n",
        "  surface-health [--workspace <id|ref>]\n",
        "  send [--workspace <id|ref>] [--surface <id|ref>] <text>\n",
        "  send-key [--workspace <id|ref>] [--surface <id|ref>] <key>\n",
        "  new-workspace [--name <title>] [--cwd <path>] [--command <text>] [--layout <json>]\n",
        "      [--focus <true|false>]\n",
        "      [--env KEY=VALUE] [--env-file <path>]\n",
        "  workspace env [<workspace>] [--workspace <id|ref|name>] [--mask]\n",
        "  select-workspace --workspace <id|ref>\n",
        "  close-workspace --workspace <id|ref>\n",
        "  sidebar-state --workspace <id|ref>\n",
        "  new-surface [--workspace <id|ref>]\n",
        "  new-pane [--workspace <id|ref>] [--pane <id|ref>] [--surface <id|ref>]\n",
        "      [--direction <left|right|up|down>] [--type <terminal|browser>]\n",
        "      [--command <text>] [--url <url>]\n",
        "  new-split [--workspace <id|ref>] [--surface <id|ref>]\n",
        "      [--direction <left|right|up|down>]\n",
        "  focus-panel --panel <id|ref> [--workspace <id|ref>]\n",
        "  close-surface --surface <id|ref>\n",
        "  split-off --surface <id|ref> [--direction <left|right|up|down>]\n",
        "  drag-surface-to-split --surface <id|ref> [--direction <left|right|up|down>]\n",
        "  reorder-surface --surface <id|ref>\n",
        "      (--index <n>|--before-surface <id|ref>|--after-surface <id|ref>)\n",
        "  refresh-surfaces [--surface <id|ref>]\n",
        "  rename-workspace [--workspace <id|ref>] <title>\n",
        "  rename-window [--workspace <id|ref>] <title>\n",
        "  rename-tab [--workspace <id|ref>] [--tab <id|ref>] <title>\n",
        "  read-screen [--workspace <id|ref>] [--surface <id|ref>]\n",
        "      [--scrollback] [--lines <n>]\n",
        "  capture-pane (alias of read-screen)\n",
        "  tab-action --action <name> [--workspace <id|ref>] [--tab <id|ref>]\n",
        "      [--title <text>] [--url <url>]\n",
        "  browser [--surface <id|ref>|<surface>] <subcommand> ...\n\n",
        "CMUX compatibility aliases:\n",
        "  docs [settings|shortcuts|api|browser|agents]\n",
        "  settings [path|docs|open]\n",
        "  config <doctor|check|validate|path|paths|docs|documentation|reload|get|set>\n",
        "  config sidebar-font-size [points]\n",
        "  config surface-tab-bar-font-size [points]\n",
        "  shortcuts\n",
        "  themes [list|set|clear]\n",
        "  sessions list [--agent <name>] [--state-dir <path>] [--json]\n",
        "  new-window | current-window | list-windows | focus-window | close-window\n",
        "  list-pane-surfaces | new-split | focus-panel | close-surface\n",
        "  move-surface | split-off | drag-surface-to-split | reorder-surface\n",
        "  refresh-surfaces\n",
        "  list-notifications | dismiss-notification | mark-notification-read\n",
        "  open-notification | jump-to-unread | clear-notifications\n\n",
        "Agent integrations:\n",
        "  notify [--workspace <id|ref>] [--subtitle <text>] [--body <text>] <title>\n",
        "  hooks setup [agent] | hooks uninstall [agent] | hooks <agent> <event>\n",
        "  claude-hook | opencode-hook | gemini-hook --event <name>\n",
        "      [--subtitle <text>] [--body <text>] [--title <text>]\n",
        "  codex-teams [codex-args...]\n",
        "  agent-team [--agents codex,claude[,opencode,gemini]] [--cwd <path>]\n",
        "      [--no-launch] [--dry-run]\n",
        "      Splits the active workspace into one pane per agent and writes AGENTS.md.\n"
    )
}

/// purpose: Resolve Limux's XDG config directory without creating it.
/// inputs: The process environment observed by dirs::config_dir.
/// returns/effects: Returns the config directory path or a fatal configuration error.
fn limux_config_dir() -> Result<PathBuf> {
    dirs::config_dir()
        .map(|base| base.join("limux"))
        .ok_or_else(|| anyhow!("XDG config directory is unavailable"))
}

fn limux_settings_path() -> Result<PathBuf> {
    Ok(limux_config_dir()?.join("settings.json"))
}

fn limux_shortcuts_path() -> Result<PathBuf> {
    Ok(limux_config_dir()?.join("shortcuts.json"))
}

// purpose: Resolve the Ghostty config path used by CMUX-compatible theme commands.
// inputs: The process XDG config directory environment.
// returns/effects: Returns ~/.config/ghostty/config or a fatal config-dir error.
fn ghostty_config_path() -> Result<PathBuf> {
    dirs::config_dir()
        .map(|base| base.join("ghostty/config"))
        .ok_or_else(|| anyhow!("XDG config directory is unavailable"))
}

/// purpose: Produce CMUX-compatible docs pointers for local command families.
/// inputs: Optional docs topic from the CLI.
/// returns/effects: Returns text only; never contacts the Limux socket.
fn docs_text(topic: Option<&str>) -> Result<String> {
    let topics = "settings, shortcuts, api, browser, agents";
    let Some(topic) = topic else {
        return Ok(format!(
            "Limux docs topics: {topics}\nUse `limux docs <topic>`."
        ));
    };
    match topic {
        "settings" => Ok(format!(
            "Settings docs\nsettings path: {}\nvalidate: limux config validate\nreload: restart Limux or use host reload support when available",
            limux_settings_path()?.display()
        )),
        "shortcuts" => Ok(format!(
            "Shortcuts docs\nshortcuts path: {}\nvalidate: limux config validate",
            limux_shortcuts_path()?.display()
        )),
        "api" => Ok("API docs\nUse `limux capabilities` and `limux rpc <method> [json-params]` against a running host.".to_string()),
        "browser" => Ok(concat!(
            "Browser docs\n",
            "Use `limux browser open|navigate|url|back|forward|reload`; ",
            "DOM automation remains tracked in the CMUX parity matrix."
        )
        .to_string()),
        "agents" => Ok(concat!(
            "Agent docs\n",
            "Use `limux hooks setup`, `limux agent-team`, and agent hook commands ",
            "for Codex, Claude, Gemini, and OpenCode."
        )
        .to_string()),
        _ => bail!("unknown docs topic `{topic}`; expected one of {topics}"),
    }
}

/// purpose: Parse a JSON config file if it exists and fail loudly if it is corrupt.
/// inputs: A settings or shortcuts path.
/// returns/effects: Returns true when the file existed and parsed; false when absent.
struct ConfigDoctorTarget {
    label: String,
    path: PathBuf,
    missing_is_error: bool,
}

struct ConfigDoctorFinding {
    label: String,
    path: PathBuf,
    status: &'static str,
    message: String,
    keys: Vec<String>,
    byte_count: Option<usize>,
}

#[derive(Default)]
struct JsoncStringState {
    in_string: bool,
    escaped: bool,
}

impl JsoncStringState {
    // purpose: Track JSON string state for JSONC preprocessing.
    // inputs: One source character.
    // returns/effects: Returns true when the caller should copy the character as string content.
    fn consume(&mut self, ch: char) -> bool {
        if self.in_string {
            self.consume_inside_string(ch);
            return true;
        }
        if ch == '"' {
            self.in_string = true;
            return true;
        }
        false
    }

    // purpose: Advance state while inside a JSON string.
    // inputs: One character already known to be in a string.
    // returns/effects: Updates escape and closing-quote state.
    fn consume_inside_string(&mut self, ch: char) {
        if self.escaped {
            self.escaped = false;
            return;
        }
        if ch == '\\' {
            self.escaped = true;
            return;
        }
        if ch == '"' {
            self.in_string = false;
        }
    }
}

/// purpose: Validate Limux local config files for CMUX-compatible config commands.
/// inputs: The current XDG config directory.
/// returns/effects: Reads settings and shortcuts JSON; does not create or modify files.
fn config_validation_text() -> Result<String> {
    config_validation_text_for_targets(default_config_doctor_targets()?)
}

/// purpose: Validate specific config targets for tests and CLI output.
/// inputs: Concrete doctor targets.
/// returns/effects: Reads JSON files; does not create or modify them.
fn config_validation_text_for_targets(targets: Vec<ConfigDoctorTarget>) -> Result<String> {
    let findings = targets
        .iter()
        .map(config_doctor_finding)
        .collect::<Result<Vec<_>>>()?;
    Ok(render_config_doctor_findings(&findings))
}

/// purpose: Build default Limux config doctor targets.
/// inputs: Current config paths and nearest project cmux.json, if present.
/// returns/effects: Includes project config once when it differs from settings.
fn default_config_doctor_targets() -> Result<Vec<ConfigDoctorTarget>> {
    let settings = limux_settings_path()?;
    let mut targets = vec![
        ConfigDoctorTarget {
            label: "settings".to_string(),
            path: settings.clone(),
            missing_is_error: false,
        },
        ConfigDoctorTarget {
            label: "shortcuts".to_string(),
            path: limux_shortcuts_path()?,
            missing_is_error: false,
        },
    ];
    if let Some(project) = find_project_cmux_config_path()? {
        if project != settings {
            targets.push(ConfigDoctorTarget {
                label: "project".to_string(),
                path: project,
                missing_is_error: false,
            });
        }
    }
    Ok(targets)
}

/// purpose: Parse CMUX-compatible config doctor --path options.
/// inputs: Args after the `doctor` subcommand.
/// returns/effects: Returns explicit custom targets or None for default targets.
fn parse_config_doctor_targets(args: &[String]) -> Result<Option<Vec<ConfigDoctorTarget>>> {
    let mut paths = Vec::new();
    let mut index = 0usize;
    while index < args.len() {
        let argument = &args[index];
        if argument == "--path" {
            let Some(path) = args.get(index + 1) else {
                bail!("limux config doctor --path requires a path");
            };
            paths.push(path.clone());
            index += 2;
            continue;
        }
        if let Some(path) = argument.strip_prefix("--path=") {
            if path.is_empty() {
                bail!("limux config doctor --path requires a path");
            }
            paths.push(path.to_string());
            index += 1;
            continue;
        }
        if argument.starts_with('-') {
            bail!("unknown config doctor option `{argument}`");
        }
        bail!("unknown config doctor argument `{argument}`. Use --path <path>.");
    }
    if paths.is_empty() {
        return Ok(None);
    }
    paths
        .into_iter()
        .enumerate()
        .map(|(index, raw)| {
            Ok(ConfigDoctorTarget {
                label: format!("custom {}", index + 1),
                path: absolute_cli_path(&raw)?,
                missing_is_error: true,
            })
        })
        .collect::<Result<Vec<_>>>()
        .map(Some)
}

/// purpose: Resolve a CLI path like CMUX config doctor does.
/// inputs: Raw absolute, relative, or tilde path.
/// returns/effects: Expands `~` using HOME and anchors relative paths at cwd.
fn absolute_cli_path(raw: &str) -> Result<PathBuf> {
    let expanded = if raw == "~" {
        env::var_os("HOME")
            .map(PathBuf::from)
            .ok_or_else(|| anyhow!("HOME is required to expand ~"))?
    } else if let Some(rest) = raw.strip_prefix("~/") {
        env::var_os("HOME")
            .map(PathBuf::from)
            .ok_or_else(|| anyhow!("HOME is required to expand ~/ paths"))?
            .join(rest)
    } else {
        PathBuf::from(raw)
    };
    if expanded.is_absolute() {
        Ok(expanded)
    } else {
        Ok(env::current_dir()
            .context("failed to read current directory")?
            .join(expanded))
    }
}

/// purpose: Validate one config doctor target.
/// inputs: Target label, path, and missing-file policy.
/// returns/effects: Reports status, message, JSON object keys, and byte count.
fn config_doctor_finding(target: &ConfigDoctorTarget) -> Result<ConfigDoctorFinding> {
    let raw = match fs::read(&target.path) {
        Ok(raw) => raw,
        Err(err) if err.kind() == ErrorKind::NotFound => {
            return Ok(ConfigDoctorFinding {
                label: target.label.clone(),
                path: target.path.clone(),
                status: if target.missing_is_error {
                    "error"
                } else {
                    "missing"
                },
                message: if target.missing_is_error {
                    "file not found".to_string()
                } else {
                    "not found; Limux will use defaults until this file exists".to_string()
                },
                keys: Vec::new(),
                byte_count: None,
            });
        }
        Err(err) => {
            return Err(err).with_context(|| format!("failed to read {}", target.path.display()))
        }
    };
    if raw.is_empty() {
        return Ok(ConfigDoctorFinding {
            label: target.label.clone(),
            path: target.path.clone(),
            status: "error",
            message: "file is empty".to_string(),
            keys: Vec::new(),
            byte_count: Some(0),
        });
    }
    let sanitized = jsonc_preprocess(&raw)
        .with_context(|| format!("failed to preprocess JSONC {}", target.path.display()))?;
    let value: Value = serde_json::from_slice(&sanitized)
        .with_context(|| format!("{} is not valid JSON", target.path.display()))?;
    let Some(object) = value.as_object() else {
        return Ok(ConfigDoctorFinding {
            label: target.label.clone(),
            path: target.path.clone(),
            status: "error",
            message: "top-level value must be a JSON object".to_string(),
            keys: Vec::new(),
            byte_count: Some(raw.len()),
        });
    };
    Ok(ConfigDoctorFinding {
        label: target.label.clone(),
        path: target.path.clone(),
        status: "ok",
        message: "JSONC syntax is valid".to_string(),
        keys: object.keys().cloned().collect(),
        byte_count: Some(raw.len()),
    })
}

// purpose: Strip JSONC comments and trailing commas before strict JSON parsing.
// inputs: UTF-8 JSONC bytes from a config file.
// returns/effects: Preserves string contents and fails loudly on invalid UTF-8.
fn jsonc_preprocess(raw: &[u8]) -> Result<Vec<u8>> {
    let source = std::str::from_utf8(raw).context("config file must be UTF-8")?;
    let without_comments = jsonc_strip_comments(source)?;
    Ok(jsonc_strip_trailing_commas(&without_comments).into_bytes())
}

// purpose: Remove JSONC line and block comments outside strings.
// inputs: UTF-8 JSONC source.
// returns/effects: Preserves newlines for useful parse locations and errors on unclosed blocks.
fn jsonc_strip_comments(source: &str) -> Result<String> {
    let mut output = String::with_capacity(source.len());
    let mut chars = source.chars().peekable();
    let mut string_state = JsoncStringState::default();
    while let Some(ch) = chars.next() {
        if string_state.consume(ch) {
            output.push(ch);
            continue;
        }
        match ch {
            '/' if chars.peek() == Some(&'/') => {
                chars.next();
                skip_jsonc_line_comment(&mut chars, &mut output);
            }
            '/' if chars.peek() == Some(&'*') => {
                chars.next();
                skip_jsonc_block_comment(&mut chars, &mut output)?;
            }
            _ => output.push(ch),
        }
    }
    Ok(output)
}

// purpose: Skip a JSONC line annotation.
// inputs: Comment iterator positioned after a line-comment marker.
// returns/effects: Emits the line break that ends the comment, if any.
fn skip_jsonc_line_comment<I>(chars: &mut std::iter::Peekable<I>, output: &mut String)
where
    I: Iterator<Item = char>,
{
    while let Some(ch) = chars.next() {
        if push_jsonc_line_break(ch, chars, output) {
            break;
        }
    }
}

// purpose: Preserve a line break found while skipping ignored JSONC text.
// inputs: Current character plus the remaining iterator for CRLF detection.
// returns/effects: Writes the line break and returns true when the skipped text ended.
fn push_jsonc_line_break<I>(
    ch: char,
    chars: &mut std::iter::Peekable<I>,
    output: &mut String,
) -> bool
where
    I: Iterator<Item = char>,
{
    match ch {
        '\n' => {
            output.push('\n');
            true
        }
        '\r' => {
            output.push('\r');
            if chars.peek() == Some(&'\n') {
                chars.next();
                output.push('\n');
            }
            true
        }
        _ => false,
    }
}

// purpose: Skip a JSONC block annotation.
// inputs: Comment iterator positioned after `/*`.
// returns/effects: Emits contained newlines and errors on unclosed comments.
fn skip_jsonc_block_comment<I>(
    chars: &mut std::iter::Peekable<I>,
    output: &mut String,
) -> Result<()>
where
    I: Iterator<Item = char>,
{
    let mut previous = '\0';
    for ch in chars.by_ref() {
        if ch == '\n' || ch == '\r' {
            output.push(ch);
        }
        if previous == '*' && ch == '/' {
            return Ok(());
        }
        previous = ch;
    }
    bail!("unterminated JSONC block comment")
}

// purpose: Remove commas before object and array closing delimiters outside strings.
// inputs: Comment-free JSONC source.
// returns/effects: Returns strict JSON-compatible source.
fn jsonc_strip_trailing_commas(source: &str) -> String {
    let mut output = String::with_capacity(source.len());
    let mut chars = source.chars().peekable();
    let mut string_state = JsoncStringState::default();
    while let Some(ch) = chars.next() {
        if string_state.consume(ch) {
            output.push(ch);
            continue;
        }
        if ch == ',' && next_non_ws_is_closing(&chars) {
            continue;
        }
        output.push(ch);
    }
    output
}

// purpose: Check whether a comma is followed only by whitespace before `]` or `}`.
// inputs: The remaining source iterator after a comma.
// returns/effects: Does not consume the iterator.
fn next_non_ws_is_closing<I>(chars: &std::iter::Peekable<I>) -> bool
where
    I: Iterator<Item = char> + Clone,
{
    let mut clone = chars.clone();
    clone
        .find(|ch| !ch.is_whitespace())
        .is_some_and(|ch| ch == ']' || ch == '}')
}

/// purpose: Render text config doctor output.
/// inputs: Config doctor findings.
/// returns/effects: Produces deterministic CMUX-style text for terminal output.
fn render_config_doctor_findings(findings: &[ConfigDoctorFinding]) -> String {
    let mut lines = vec!["limux config doctor".to_string()];
    for finding in findings {
        lines.push(format!(
            "{} {}: {}",
            finding.status.to_ascii_uppercase(),
            finding.label,
            finding.path.display()
        ));
        lines.push(format!("  path: {}", finding.path.display()));
        if let Some(byte_count) = finding.byte_count {
            lines.push(format!("  bytes: {byte_count}"));
        }
        if !finding.keys.is_empty() {
            lines.push(format!("  keys: {}", finding.keys.join(", ")));
        }
        lines.push(format!("  {}", finding.message));
    }
    lines.push("Docs: limux docs settings".to_string());
    lines.push("Reload: limux reload-config".to_string());
    lines.join("\n")
}

/// purpose: Read the settings JSON object while rejecting corrupt or non-object config.
/// inputs: The Limux settings path.
/// returns/effects: Returns an editable JSON object; creates no files by itself.
fn read_settings_root(path: &Path) -> Result<Map<String, Value>> {
    match fs::read_to_string(path) {
        Ok(raw) => match serde_json::from_str::<Value>(&raw)
            .with_context(|| format!("{} is not valid JSON", path.display()))?
        {
            Value::Object(root) => Ok(root),
            _ => bail!("{} root must be a JSON object", path.display()),
        },
        Err(err) if err.kind() == ErrorKind::NotFound => Ok(Map::new()),
        Err(err) => Err(err).with_context(|| format!("failed to read {}", path.display())),
    }
}

/// purpose: Persist the settings JSON through a same-directory temporary file.
/// inputs: Target path and the full JSON object to write.
/// returns/effects: Creates the config directory and atomically replaces settings.json.
fn write_settings_root(path: &Path, root: &Map<String, Value>) -> Result<()> {
    let parent = path
        .parent()
        .ok_or_else(|| anyhow!("settings path has no parent directory"))?;
    fs::create_dir_all(parent)
        .with_context(|| format!("failed to create config directory {}", parent.display()))?;
    let temp = path.with_extension("json.tmp");
    let encoded = serde_json::to_vec_pretty(&Value::Object(root.clone()))
        .context("failed to encode settings JSON")?;
    fs::write(&temp, encoded).with_context(|| format!("failed to write {}", temp.display()))?;
    fs::rename(&temp, path).with_context(|| {
        format!(
            "failed to replace {} with {}",
            path.display(),
            temp.display()
        )
    })
}

const CMUX_THEMES_BLOCK_START: &str = "# cmux themes start";
const CMUX_THEMES_BLOCK_END: &str = "# cmux themes end";

#[derive(Debug, Clone, PartialEq, Eq)]
struct ThemeSelection {
    raw_value: Option<String>,
    light: Option<String>,
    dark: Option<String>,
    source_path: Option<String>,
}

// purpose: Reject malformed Ghostty theme names before writing a managed config block.
// inputs: Raw CLI theme argument.
// returns/effects: Returns a trimmed theme name or a fatal user-facing error.
fn validated_theme_name(raw: &str) -> Result<String> {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        bail!("themes set requires a non-empty theme name");
    }
    if trimmed
        .chars()
        .any(|ch| ch == ',' || ch == '\n' || ch == '\r' || ch == '\0')
    {
        bail!("theme name contains unsupported control or separator characters");
    }
    Ok(trimmed.to_string())
}

// purpose: Encode CMUX/Ghostty conditional theme values for light and dark appearances.
// inputs: Optional validated light and dark theme names.
// returns/effects: Returns Ghostty config value syntax or None when both sides are absent.
fn encoded_theme_value(light: Option<&str>, dark: Option<&str>) -> Option<String> {
    match (light, dark) {
        (Some(light), Some(dark)) => Some(format!("light:{light},dark:{dark}")),
        (Some(light), None) => Some(format!("light:{light}")),
        (None, Some(dark)) => Some(format!("dark:{dark}")),
        (None, None) => None,
    }
}

// purpose: Build inherited theme selection state for absent Ghostty theme directives.
// inputs: Optional source path for the checked Ghostty config.
// returns/effects: Returns a selection with no active theme names.
fn empty_theme_selection(source_path: Option<String>) -> ThemeSelection {
    ThemeSelection {
        raw_value: None,
        light: None,
        dark: None,
        source_path,
    }
}

// purpose: Fold one Ghostty theme token into CMUX light/dark/fallback slots.
// inputs: One comma-separated token plus mutable theme slots.
// returns/effects: Updates the first matching slot and ignores empty tokens.
fn apply_theme_selection_token(
    entry: &str,
    fallback_theme: &mut Option<String>,
    light_theme: &mut Option<String>,
    dark_theme: &mut Option<String>,
) {
    if entry.is_empty() {
        return;
    }
    let Some((key, value)) = entry.split_once(':') else {
        fallback_theme.get_or_insert_with(|| entry.to_string());
        return;
    };
    let value = value.trim();
    if value.is_empty() {
        return;
    }
    match key.trim().to_ascii_lowercase().as_str() {
        "light" => light_theme.get_or_insert_with(|| value.to_string()),
        "dark" => dark_theme.get_or_insert_with(|| value.to_string()),
        _ => fallback_theme.get_or_insert_with(|| value.to_string()),
    };
}

// purpose: Parse a Ghostty theme directive into CMUX light/dark selection fields.
// inputs: Raw theme directive value plus optional source path.
// returns/effects: Returns current light/dark selections without touching files.
fn parse_theme_selection(raw_value: Option<String>, source_path: Option<String>) -> ThemeSelection {
    let Some(raw) = raw_value
        .as_deref()
        .map(str::trim)
        .filter(|raw| !raw.is_empty())
    else {
        return empty_theme_selection(source_path);
    };

    let mut fallback_theme = None;
    let mut light_theme = None;
    let mut dark_theme = None;
    for token in raw.split(',') {
        apply_theme_selection_token(
            token.trim(),
            &mut fallback_theme,
            &mut light_theme,
            &mut dark_theme,
        );
    }

    ThemeSelection {
        raw_value: Some(raw.to_string()),
        light: light_theme.or_else(|| fallback_theme.clone()),
        dark: dark_theme.or(fallback_theme),
        source_path,
    }
}

// purpose: Find the last Ghostty theme directive in a config file.
// inputs: Full Ghostty config contents.
// returns/effects: Returns the raw theme value after `=` when present.
fn last_theme_directive(contents: &str) -> Option<String> {
    contents.lines().rev().find_map(parse_theme_directive)
}

// purpose: Parse one Ghostty config line as a theme directive.
// inputs: One config line.
// returns/effects: Returns the trimmed theme value when the line declares `theme`.
fn parse_theme_directive(line: &str) -> Option<String> {
    let trimmed = line.trim();
    if trimmed.is_empty() || trimmed.starts_with('#') {
        return None;
    }
    let (key, value) = trimmed.split_once('=')?;
    if key.trim() != "theme" {
        return None;
    }
    Some(value.trim().to_string())
}

// purpose: Read the current CMUX/Ghostty theme selection from a config path.
// inputs: Ghostty config path.
// returns/effects: Reads the file when present and returns inherited state when absent.
fn current_theme_selection_at(path: &Path) -> Result<ThemeSelection> {
    match fs::read_to_string(path) {
        Ok(contents) => Ok(parse_theme_selection(
            last_theme_directive(&contents),
            Some(path.display().to_string()),
        )),
        Err(err) if err.kind() == ErrorKind::NotFound => Ok(parse_theme_selection(
            None,
            Some(path.display().to_string()),
        )),
        Err(err) => Err(err).with_context(|| format!("failed to read {}", path.display())),
    }
}

// purpose: Remove the CMUX-managed theme block while preserving user Ghostty config lines.
// inputs: Full Ghostty config contents.
// returns/effects: Returns config contents without the managed block or errors on malformed markers.
fn remove_managed_theme_block(contents: &str) -> Result<String> {
    let mut output = Vec::new();
    let mut in_block = false;
    for line in contents.lines() {
        match line.trim() {
            CMUX_THEMES_BLOCK_START if in_block => {
                bail!("nested cmux themes block in Ghostty config")
            }
            CMUX_THEMES_BLOCK_START => in_block = true,
            CMUX_THEMES_BLOCK_END if in_block => in_block = false,
            CMUX_THEMES_BLOCK_END => bail!("cmux themes block end without start in Ghostty config"),
            _ if !in_block => output.push(line.to_string()),
            _ => {}
        }
    }
    if in_block {
        bail!("unterminated cmux themes block in Ghostty config");
    }
    Ok(output.join("\n").trim_end().to_string())
}

// purpose: Write or replace CMUX's managed Ghostty theme block.
// inputs: Ghostty config path and encoded theme directive value.
// returns/effects: Creates the parent directory and atomically writes the config.
fn write_managed_theme_override_at(path: &Path, raw_theme_value: &str) -> Result<()> {
    let existing = match fs::read_to_string(path) {
        Ok(contents) => contents,
        Err(err) if err.kind() == ErrorKind::NotFound => String::new(),
        Err(err) => return Err(err).with_context(|| format!("failed to read {}", path.display())),
    };
    let mut next = remove_managed_theme_block(&existing)?;
    if !next.is_empty() {
        next.push_str("\n\n");
    }
    next.push_str(CMUX_THEMES_BLOCK_START);
    next.push('\n');
    next.push_str(&format!("theme = {raw_theme_value}\n"));
    next.push_str(CMUX_THEMES_BLOCK_END);
    next.push('\n');
    write_text_atomically(path, &next)
}

// purpose: Clear CMUX's managed Ghostty theme override block.
// inputs: Ghostty config path.
// returns/effects: Rewrites the config with the managed block removed.
fn clear_managed_theme_override_at(path: &Path) -> Result<()> {
    let existing = match fs::read_to_string(path) {
        Ok(contents) => contents,
        Err(err) if err.kind() == ErrorKind::NotFound => String::new(),
        Err(err) => return Err(err).with_context(|| format!("failed to read {}", path.display())),
    };
    let next = remove_managed_theme_block(&existing)?;
    let serialized = if next.is_empty() {
        String::new()
    } else {
        format!("{next}\n")
    };
    write_text_atomically(path, &serialized)
}

// purpose: Persist text through a same-directory temporary file.
// inputs: Target path and complete serialized contents.
// returns/effects: Creates the parent directory and atomically replaces the target file.
fn write_text_atomically(path: &Path, contents: &str) -> Result<()> {
    let parent = path
        .parent()
        .ok_or_else(|| anyhow!("path has no parent directory: {}", path.display()))?;
    fs::create_dir_all(parent)
        .with_context(|| format!("failed to create config directory {}", parent.display()))?;
    let temp = path.with_extension("tmp");
    fs::write(&temp, contents).with_context(|| format!("failed to write {}", temp.display()))?;
    fs::rename(&temp, path).with_context(|| {
        format!(
            "failed to replace {} with {}",
            path.display(),
            temp.display()
        )
    })
}

#[derive(Default)]
struct ThemeSetParts {
    light: Option<String>,
    dark: Option<String>,
    positional: Vec<String>,
}

// purpose: Consume one CLI token from `themes set` parsing.
// inputs: Argument slice, current index, and mutable parsed parts.
// returns/effects: Updates parsed parts and returns the next index to inspect.
fn consume_theme_set_arg(
    args: &[String],
    index: usize,
    parts: &mut ThemeSetParts,
) -> Result<usize> {
    match args[index].as_str() {
        "--light" | "--dark" => {
            let flag = args[index].as_str();
            let value = args
                .get(index + 1)
                .ok_or_else(|| anyhow!("themes set: {flag} requires a theme name"))?;
            if flag == "--light" {
                parts.light = Some(validated_theme_name(value)?);
            } else {
                parts.dark = Some(validated_theme_name(value)?);
            }
            Ok(index + 2)
        }
        value if value.starts_with("--") => {
            bail!(
                "themes set: unknown flag `{value}`. Known flags: --light <theme>, --dark <theme>"
            );
        }
        value => {
            parts.positional.push(value.to_string());
            Ok(index + 1)
        }
    }
}

// purpose: Parse raw `themes set` arguments into explicit flags and positional tokens.
// inputs: Theme set arguments after the subcommand.
// returns/effects: Returns parsed parts or a fatal usage error.
fn parse_theme_set_parts(args: &[String]) -> Result<ThemeSetParts> {
    let mut parts = ThemeSetParts::default();
    let mut index = 0;
    while index < args.len() {
        index = consume_theme_set_arg(args, index, &mut parts)?;
    }
    Ok(parts)
}

// purpose: Resolve parsed `themes set` parts against the current theme selection.
// inputs: Parsed CLI parts and current CMUX/Ghostty selection.
// returns/effects: Returns validated light/dark names or a fatal usage error.
fn resolve_theme_set_parts(
    parts: ThemeSetParts,
    current: &ThemeSelection,
) -> Result<(Option<String>, Option<String>)> {
    if parts.light.is_none() && parts.dark.is_none() {
        let joined = parts.positional.join(" ");
        let theme = validated_theme_name(&joined)?;
        return Ok((Some(theme.clone()), Some(theme)));
    }
    if !parts.positional.is_empty() {
        bail!(
            "themes set: unexpected argument `{}`",
            parts.positional.join(" ")
        );
    }
    Ok((
        parts.light.or_else(|| current.light.clone()),
        parts.dark.or_else(|| current.dark.clone()),
    ))
}

// purpose: Parse `themes set` arguments into CMUX light and dark theme selections.
// inputs: Theme set arguments after the subcommand and the current selection.
// returns/effects: Returns validated light/dark names or a fatal usage error.
fn parse_theme_set_args(
    args: &[String],
    current: &ThemeSelection,
) -> Result<(Option<String>, Option<String>)> {
    resolve_theme_set_parts(parse_theme_set_parts(args)?, current)
}

#[derive(Clone, Copy)]
struct FontSizeSetting {
    key: &'static str,
    default: f64,
    min: f64,
    max: f64,
}

const SIDEBAR_FONT_SIZE: FontSizeSetting = FontSizeSetting {
    key: "sidebar-font-size",
    default: 12.5,
    min: 10.0,
    max: 20.0,
};

const SURFACE_TAB_BAR_FONT_SIZE: FontSizeSetting = FontSizeSetting {
    key: "surface-tab-bar-font-size",
    default: 11.0,
    min: 8.0,
    max: 14.0,
};

/// purpose: Map CMUX config font-size keys to their supported ranges.
/// inputs: Raw config key from CLI arguments.
/// returns/effects: Returns the supported descriptor or None for unknown keys.
fn font_size_setting(raw: &str) -> Option<FontSizeSetting> {
    match raw {
        "sidebar-font-size" => Some(SIDEBAR_FONT_SIZE),
        "surface-tab-bar-font-size" => Some(SURFACE_TAB_BAR_FONT_SIZE),
        _ => None,
    }
}

/// purpose: Format CMUX font-size values without unnecessary trailing zeros.
/// inputs: Numeric point size.
/// returns/effects: Returns strings like 12, 12.5, or 13.75.
fn format_font_size(value: f64) -> String {
    let scaled = (value * 100.0).round() as i64;
    let whole = scaled / 100;
    let fraction = (scaled % 100).abs();
    if fraction == 0 {
        return whole.to_string();
    }
    if fraction % 10 == 0 {
        return format!("{whole}.{}", fraction / 10);
    }
    format!("{whole}.{fraction:02}")
}

/// purpose: Read the effective CMUX font-size setting from Limux settings JSON.
/// inputs: Settings path and supported font-size descriptor.
/// returns/effects: Returns configured value when present, otherwise CMUX default.
fn get_config_font_size_at(path: &Path, setting: FontSizeSetting) -> Result<(f64, bool)> {
    let root = read_settings_root(path)?;
    let configured = root
        .get(setting.key)
        .and_then(Value::as_f64)
        .filter(|value| value.is_finite())
        .map(|value| value.clamp(setting.min, setting.max));
    Ok((configured.unwrap_or(setting.default), configured.is_some()))
}

/// purpose: Write a CMUX font-size setting while preserving unrelated settings.
/// inputs: Settings path, supported descriptor, and raw CLI value.
/// returns/effects: Clamps numeric values and atomically writes settings JSON.
fn set_config_font_size_at(path: &Path, setting: FontSizeSetting, raw: &str) -> Result<f64> {
    let requested = raw
        .parse::<f64>()
        .with_context(|| format!("{} requires a numeric point size", setting.key))?;
    if !requested.is_finite() {
        bail!("{} requires a finite numeric point size", setting.key);
    }
    let value = requested.clamp(setting.min, setting.max);
    let mut root = read_settings_root(path)?;
    root.insert(setting.key.to_string(), json!(value));
    write_settings_root(path, &root)?;
    Ok(value)
}

/// purpose: Render CMUX-compatible config get output for one font-size setting.
/// inputs: Settings path and setting descriptor.
/// returns/effects: Returns text with effective value and backing path.
fn render_config_font_size_get(path: &Path, setting: FontSizeSetting) -> Result<String> {
    let (value, _) = get_config_font_size_at(path, setting)?;
    Ok(format!(
        "{} = {}\npath: {}",
        setting.key,
        format_font_size(value),
        path.display()
    ))
}

/// purpose: Apply a CMUX-compatible config set operation for one font-size setting.
/// inputs: Settings path, setting descriptor, and raw point-size argument.
/// returns/effects: Writes settings JSON and returns user-facing status text.
fn render_config_font_size_set(path: &Path, setting: FontSizeSetting, raw: &str) -> Result<String> {
    let value = set_config_font_size_at(path, setting, raw)?;
    Ok(format!(
        "OK {} = {} (saved)\nRun `limux config reload` to apply it.\npath: {}",
        setting.key,
        format_font_size(value),
        path.display()
    ))
}

/// purpose: Handle CMUX-compatible local commands without requiring a socket.
/// inputs: Parsed global CLI options.
/// returns/effects: May read or write local config files, but never contacts the host socket.
fn run_local_command(opts: &GlobalOptions) -> Result<Option<CommandOutput>> {
    let Some(command) = opts.command_args.first().map(String::as_str) else {
        return Ok(None);
    };
    let args = &opts.command_args[1..];
    if let Some(text) = command_help_probe_text(command, args) {
        return Ok(Some(CommandOutput::Text(text.to_string())));
    }
    if is_unsupported_remote_cli_command(command) {
        bail!(
            "not_supported: {command} requires CMUX remote daemon support, which Limux does not implement yet"
        );
    }
    let out = match command {
        "docs" => Some(CommandOutput::Text(docs_text(
            args.first().map(String::as_str),
        )?)),
        "settings" if args.first().map(String::as_str) == Some("open") => None,
        "settings" => Some(run_settings_command(args)?),
        "config" if args.first().map(String::as_str) == Some("reload") => None,
        "config" => Some(run_config_command(args)?),
        "shortcuts" => Some(CommandOutput::Text(
            limux_shortcuts_path()?.display().to_string(),
        )),
        "feed" if args.first().map(String::as_str) == Some("clear") => {
            Some(run_feed_clear_command(args, opts.json_output)?)
        }
        "themes" => Some(run_themes_command(args, opts.json_output)?),
        "sessions" => Some(run_sessions_local_command(args, opts.json_output)?),
        "session-debug" => {
            let mut debug_args = vec!["debug".to_string()];
            debug_args.extend(args.iter().cloned());
            Some(run_sessions_local_command(&debug_args, opts.json_output)?)
        }
        "reload-config" => None,
        _ => None,
    };
    Ok(out)
}

// purpose: Clear CMUX-compatible Feed persistent history from the local state file.
// inputs: `feed clear` arguments plus global JSON preference.
// returns/effects: Prompts unless --yes/-y is present, then removes workstream JSONL files.
fn run_feed_clear_command(args: &[String], json_output: bool) -> Result<CommandOutput> {
    let path = feed_workstream_log_path()?;
    run_feed_clear_at(args, json_output, &path)
}

// purpose: Clear Feed history at a caller-selected path for CLI and isolated tests.
// inputs: `feed clear` args, JSON preference, and primary workstream JSONL path.
// returns/effects: Removes primary and rotated history files after optional confirmation.
fn run_feed_clear_at(args: &[String], json_output: bool, path: &Path) -> Result<CommandOutput> {
    if args.is_empty() || args[0] != "clear" {
        bail!("Usage: limux feed clear [--yes|-y]");
    }
    let assume_yes = parse_feed_clear_args(&args[1..])?;
    if !assume_yes && !confirm_feed_clear(path)? {
        if json_output {
            return Ok(CommandOutput::Json(
                json!({ "cleared": false, "aborted": true }),
            ));
        }
        return Ok(CommandOutput::Text("Aborted.".to_string()));
    }
    let removed_paths = remove_feed_history_files(path)?;
    if json_output {
        return Ok(CommandOutput::Json(json!({
            "cleared": true,
            "path": path,
            "removed_paths": removed_paths,
        })));
    }
    if removed_paths.is_empty() {
        return Ok(CommandOutput::Text(format!(
            "No Feed history to clear ({} does not exist).",
            path.display()
        )));
    }
    Ok(CommandOutput::Text(format!("Cleared {}", path.display())))
}

// purpose: Parse CMUX Feed clear confirmation flags.
// inputs: Arguments after `feed clear`.
// returns/effects: Returns whether confirmation is bypassed, failing on unknown flags.
fn parse_feed_clear_args(args: &[String]) -> Result<bool> {
    let mut assume_yes = false;
    for arg in args {
        match arg.as_str() {
            "--yes" | "-y" => assume_yes = true,
            "--help" | "-h" => bail!("Usage: limux feed clear [--yes|-y]"),
            other => bail!("unknown feed clear argument: {other}"),
        }
    }
    Ok(assume_yes)
}

// purpose: Resolve the CMUX-compatible Feed persistent history path.
// inputs: Current user home directory.
// returns/effects: Fails loudly if the operating system does not expose a home directory.
fn feed_workstream_log_path() -> Result<PathBuf> {
    let home = dirs::home_dir().context("home directory is required for Feed history")?;
    Ok(home.join(".cmuxterm/workstream.jsonl"))
}

// purpose: Ask for destructive Feed history deletion confirmation.
// inputs: Workstream log path shown to the user.
// returns/effects: Writes a prompt, reads one line from stdin, and returns true only for y/yes.
fn confirm_feed_clear(path: &Path) -> Result<bool> {
    print!(
        "This will permanently delete {}. Proceed? [y/N] ",
        path.display()
    );
    io::stdout()
        .flush()
        .context("failed to flush Feed clear prompt")?;
    let mut response = String::new();
    io::stdin()
        .read_line(&mut response)
        .context("failed to read Feed clear confirmation")?;
    Ok(response.trim().to_ascii_lowercase().starts_with('y'))
}

// purpose: Delete the Feed persistent history files used by CMUX-compatible audit storage.
// inputs: Primary workstream JSONL path.
// returns/effects: Removes the primary file and one rotation, ignoring only missing files.
fn remove_feed_history_files(path: &Path) -> Result<Vec<String>> {
    let mut removed = Vec::new();
    for candidate in [
        path.to_path_buf(),
        path.with_file_name("workstream.jsonl.1"),
    ] {
        match fs::remove_file(&candidate) {
            Ok(()) => removed.push(candidate.display().to_string()),
            Err(error) if error.kind() == ErrorKind::NotFound => {}
            Err(error) => {
                return Err(error)
                    .with_context(|| format!("failed to remove {}", candidate.display()));
            }
        }
    }
    Ok(removed)
}

// purpose: Return CMUX-compatible no-socket help text for command probes.
// inputs: Command name and its arguments.
// returns/effects: Returns usage text only when the first command arg is --help or -h.
fn command_help_probe_text(command: &str, args: &[String]) -> Option<&'static str> {
    if !matches!(args.first().map(String::as_str), Some("--help" | "-h")) {
        return None;
    }
    cmux_command_usage(command)
}

// purpose: Map CMUX-compatible command names to stable usage text.
// inputs: Top-level command name.
// returns/effects: Returns no-socket help text for known CMUX probes.
fn cmux_command_usage(command: &str) -> Option<&'static str> {
    CMUX_HELP_USAGES
        .iter()
        .find_map(|(name, usage)| (*name == command).then_some(*usage))
}

// purpose: Identify CMUX remote/SSH commands that need the unavailable remote daemon.
// inputs: Top-level CLI command name.
// returns/effects: Returns true for commands that should fail locally with not_supported.
fn is_unsupported_remote_cli_command(command: &str) -> bool {
    matches!(
        command,
        "ssh"
            | "ssh-tmux"
            | "ssh-pty-attach"
            | "ssh-session-list"
            | "ssh-session-attach"
            | "ssh-session-cleanup"
            | "ssh-session-end"
            | "remote-daemon-status"
            | "remote"
            | "remotes"
            | "vm-ssh-attach"
    )
}

const CMUX_HELP_USAGES: &[(&str, &str)] = &[
    ("restore-session", "Usage: limux restore-session"),
    ("open", "Usage: limux open <path-or-url>..."),
    ("feedback", "Usage: limux feedback"),
    ("feed", "Usage: limux feed tui [--opentui|--legacy]"),
    (
        "hooks",
        "Usage: limux hooks setup [agent] [--agent <name>] [--yes|-y]",
    ),
    (
        "codex",
        "Usage: limux codex <install-hooks|uninstall-hooks>",
    ),
    (
        "codex-teams",
        "Usage: limux codex-teams [codex-args...]\n\n\
         Launch Codex with cmux-managed subagent panes.\n\n\
         This CMUX command starts a private Codex app-server, launches the root Codex TUI against it, \
         watches live Codex thread-spawn subagents, opens subagents up to depth 2 as native splits, \
         bridges app-server approvals through Feed, and forwards all remaining arguments to codex.",
    ),
    ("themes", "Usage: limux themes"),
    ("omo", "Usage: limux omo [opencode-args...]"),
    ("omx", "Usage: limux omx [omx-args...]"),
    ("omc", "Usage: limux omc [omc-args...]"),
    ("identify", "Usage: limux identify"),
    ("list-windows", "Usage: limux list-windows"),
    ("current-window", "Usage: limux current-window"),
    ("new-window", "Usage: limux new-window"),
    (
        "focus-window",
        "Usage: limux focus-window --window <id|ref|index>",
    ),
    (
        "close-window",
        "Usage: limux close-window --window <id|ref|index>",
    ),
    (
        "move-workspace-to-window",
        "Usage: limux move-workspace-to-window",
    ),
    ("move-surface", "Usage: limux move-surface"),
    ("split-off", "Usage: limux split-off"),
    ("reorder-surface", "Usage: limux reorder-surface"),
    ("reorder-workspace", "Usage: limux reorder-workspace"),
    ("reorder-workspaces", "Usage: limux reorder-workspaces"),
    (
        "workspace-action",
        "Usage: limux workspace-action --action <name>",
    ),
    (
        "move-tab-to-new-workspace",
        "Usage: limux move-tab-to-new-workspace",
    ),
    ("tab-action", "Usage: limux tab-action --action <name>"),
    ("rename-tab", "Usage: limux rename-tab"),
    ("new-workspace", "Usage: limux new-workspace"),
    ("list-workspaces", "Usage: limux list-workspaces"),
    ("ssh", "Usage: limux ssh <destination>\n--forward-agent"),
    ("ssh-tmux", "Usage: limux ssh-tmux <destination>"),
    (
        "ssh-pty-attach",
        "Usage: limux ssh-pty-attach --workspace <workspace> --session-id <id>",
    ),
    ("ssh-session-list", "Usage: limux ssh-session-list"),
    (
        "ssh-session-attach",
        "Usage: limux ssh-session-attach --session-id <id>",
    ),
    ("ssh-session-cleanup", "Usage: limux ssh-session-cleanup"),
    (
        "ssh-session-end",
        "Usage: limux ssh-session-end --relay-port <port> --workspace <id>",
    ),
    ("remote-daemon-status", "Usage: limux remote-daemon-status"),
    ("remote", "Usage: limux remotes <list|add|remove> [options]"),
    (
        "remotes",
        "Usage: limux remotes <list|add|remove> [options]",
    ),
    ("vm-ssh-attach", "Usage: limux vm-ssh-attach --id <vm-id>"),
    ("new-split", "Usage: limux new-split"),
    ("list-panes", "Usage: limux list-panes"),
    ("list-pane-surfaces", "Usage: limux list-pane-surfaces"),
    ("tree", "Usage: limux tree"),
    ("top", "Usage: limux top"),
    ("focus-pane", "Usage: limux focus-pane"),
    ("new-pane", "Usage: limux new-pane"),
    ("new-surface", "Usage: limux new-surface"),
    ("close-surface", "Usage: limux close-surface"),
    (
        "drag-surface-to-split",
        "Usage: limux drag-surface-to-split",
    ),
    ("refresh-surfaces", "Usage: limux refresh-surfaces"),
    ("reload-config", "Usage: limux reload-config"),
    ("surface-health", "Usage: limux surface-health"),
    ("debug-terminals", "Usage: limux debug-terminals"),
    ("trigger-flash", "Usage: limux trigger-flash"),
    ("list-panels", "Usage: limux list-panels"),
    ("focus-panel", "Usage: limux focus-panel"),
    ("close-workspace", "Usage: limux close-workspace"),
    ("select-workspace", "Usage: limux select-workspace"),
    ("rename-workspace", "Usage: limux rename-workspace"),
    ("rename-window", "Usage: limux rename-workspace"),
    ("current-workspace", "Usage: limux current-workspace"),
    ("capture-pane", "Usage: limux capture-pane"),
    ("resize-pane", "Usage: limux resize-pane"),
    ("pipe-pane", "Usage: limux pipe-pane"),
    ("wait-for", "Usage: limux wait-for"),
    ("swap-pane", "Usage: limux swap-pane"),
    ("break-pane", "Usage: limux break-pane"),
    ("join-pane", "Usage: limux join-pane"),
    ("next-window", "Usage: limux next-window"),
    ("previous-window", "Usage: limux previous-window"),
    ("last-window", "Usage: limux last-window"),
    ("last-pane", "Usage: limux last-pane"),
    ("find-window", "Usage: limux find-window"),
    ("clear-history", "Usage: limux clear-history"),
    ("set-hook", "Usage: limux set-hook"),
    ("popup", "Usage: limux popup"),
    ("bind-key", "Usage: limux bind-key"),
    ("unbind-key", "Usage: limux unbind-key"),
    ("copy-mode", "Usage: limux copy-mode"),
    ("set-buffer", "Usage: limux set-buffer"),
    ("paste-buffer", "Usage: limux paste-buffer"),
    ("list-buffers", "Usage: limux list-buffers"),
    ("respawn-pane", "Usage: limux respawn-pane"),
    ("display-message", "Usage: limux display-message"),
    ("read-screen", "Usage: limux read-screen"),
    ("send", "Usage: limux send"),
    ("send-key", "Usage: limux send-key"),
    ("send-panel", "Usage: limux send-panel"),
    ("send-key-panel", "Usage: limux send-key-panel"),
    ("notify", "Usage: limux notify"),
    ("list-notifications", "Usage: limux list-notifications"),
    ("dismiss-notification", "Usage: limux dismiss-notification"),
    (
        "mark-notification-read",
        "Usage: limux mark-notification-read",
    ),
    ("open-notification", "Usage: limux open-notification"),
    ("jump-to-unread", "Usage: limux jump-to-unread"),
    ("clear-notifications", "Usage: limux clear-notifications"),
    (
        "right-sidebar",
        "Usage: limux right-sidebar <command> [flags]",
    ),
    (
        "set-status",
        concat!(
            "Usage: limux set-status <key> <value> [--icon <name>] ",
            "[--color <#hex>] [--priority <n>] [--workspace <id|ref>]"
        ),
    ),
    (
        "clear-status",
        "Usage: limux clear-status <key> [--workspace <id|ref>]",
    ),
    (
        "list-status",
        "Usage: limux list-status [--workspace <id|ref>]",
    ),
    (
        "set-progress",
        concat!(
            "Usage: limux set-progress <0.0-1.0> [--label <text>] ",
            "[--workspace <id|ref>]"
        ),
    ),
    (
        "clear-progress",
        "Usage: limux clear-progress [--workspace <id|ref>]",
    ),
    (
        "log",
        concat!(
            "Usage: limux log [--level info|progress|success|warning|error] ",
            "[--source <name>] [--] <message>"
        ),
    ),
    ("clear-log", "Usage: limux clear-log [--workspace <id|ref>]"),
    (
        "list-log",
        "Usage: limux list-log [--limit <n>] [--workspace <id|ref>]",
    ),
    (
        "sidebar-state",
        "Usage: limux sidebar-state [--workspace <id|ref>]",
    ),
    ("set-app-focus", "Usage: limux set-app-focus"),
    ("simulate-app-active", "Usage: limux simulate-app-active"),
    ("claude-hook", "Usage: limux claude-hook"),
    ("browser", "Usage: limux browser"),
    ("open-browser", "Legacy alias for 'limux browser open'"),
    ("navigate", "Legacy alias for 'limux browser navigate'"),
    ("browser-back", "Legacy alias for 'limux browser back'"),
    (
        "browser-forward",
        "Legacy alias for 'limux browser forward'",
    ),
    ("browser-reload", "Legacy alias for 'limux browser reload'"),
    ("get-url", "Legacy alias for 'limux browser get-url'"),
    (
        "focus-webview",
        "Legacy alias for 'limux browser focus-webview'",
    ),
    (
        "is-webview-focused",
        "Legacy alias for 'limux browser is-webview-focused'",
    ),
    ("markdown", "Usage: limux markdown open <path>"),
];

// purpose: Adapt the no-socket CMUX sessions command module to CLI output.
// inputs: Session command args plus global JSON preference.
// returns/effects: Reads hook store files and returns text or JSON diagnostics.
fn run_sessions_local_command(args: &[String], json_output: bool) -> Result<CommandOutput> {
    let input = sessions::SessionCommandInput {
        args: args.to_vec(),
        global_json: json_output,
    };
    match sessions::SessionCommandResult::from(input) {
        sessions::SessionCommandResult::Output(sessions::SessionCommandOutput::Text(text)) => {
            Ok(CommandOutput::Text(text))
        }
        sessions::SessionCommandResult::Output(sessions::SessionCommandOutput::Json(value)) => {
            Ok(CommandOutput::Json(value))
        }
        sessions::SessionCommandResult::Error(error) => Err(error.into()),
    }
}

/// purpose: Implement CMUX settings command probes that can run without the app.
/// inputs: Settings subcommand arguments.
/// returns/effects: Prints paths/docs or fails explicitly for host-only actions.
fn run_settings_command(args: &[String]) -> Result<CommandOutput> {
    let sub = args.first().map(String::as_str).unwrap_or("path");
    match sub {
        "--help" | "-h" | "docs" => Ok(CommandOutput::Text(docs_text(Some("settings"))?)),
        "path" => Ok(CommandOutput::Text(
            limux_settings_path()?.display().to_string(),
        )),
        "open" => bail!("settings open requires a running Limux host"),
        target => bail!("unsupported settings target `{target}`; expected path, docs, or open"),
    }
}

// purpose: Normalize CMUX settings target aliases accepted by `settings open`.
// inputs: Raw CLI settings target.
// returns/effects: Returns the canonical CMUX target or None for unknown aliases.
fn settings_target_raw_value(raw: &str) -> Option<&'static str> {
    match raw.trim().to_ascii_lowercase().replace('_', "-").as_str() {
        "account" => Some("account"),
        "app" | "general" => Some("app"),
        "terminal" => Some("terminal"),
        "text-box" | "textbox" => Some("textBox"),
        "sleepy-mode" | "sleepymode" => Some("sleepyMode"),
        "mobile" => Some("mobile"),
        "sidebar" | "sidebar-appearance" | "sidebarappearance" => Some("sidebarAppearance"),
        "custom-sidebars" | "customsidebars" => Some("customSidebars"),
        "beta-features" | "betafeatures" => Some("betaFeatures"),
        "automation" => Some("automation"),
        "browser" => Some("browser"),
        "browser-import" | "browserimport" | "import-browser-data" => Some("browserImport"),
        "global-hotkey" | "globalhotkey" | "hotkey" => Some("globalHotkey"),
        "keyboard-shortcuts" | "keyboardshortcuts" | "shortcuts" | "keys" | "keybindings" => {
            Some("keyboardShortcuts")
        }
        "workspace-colors" | "workspacecolors" | "colors" => Some("workspaceColors"),
        "cmux-json" | "cmuxjson" | "settings-json" | "settingsjson" | "json" | "file"
        | "settings-file" => Some("settingsJSON"),
        "reset" => Some("reset"),
        _ => None,
    }
}

/// purpose: Implement CMUX config path, docs, validation, and reload probes.
/// inputs: Config subcommand arguments.
/// returns/effects: Reads local JSON config and fails on corrupt files.
fn run_config_command(args: &[String]) -> Result<CommandOutput> {
    let sub = args.first().map(String::as_str).unwrap_or("check");
    match sub {
        "--help" | "-h" | "docs" | "documentation" => Ok(CommandOutput::Text(docs_text(Some("settings"))?)),
        "path" => Ok(CommandOutput::Text(limux_settings_path()?.display().to_string())),
        "paths" => Ok(CommandOutput::Text(format!(
            "config_dir: {}\nsettings: {}\nshortcuts: {}",
            limux_config_dir()?.display(),
            limux_settings_path()?.display(),
            limux_shortcuts_path()?.display()
        ))),
        "check" | "validate" => Ok(CommandOutput::Text(config_validation_text()?)),
        "doctor" => {
            let targets =
                parse_config_doctor_targets(&args[1..])?.unwrap_or(default_config_doctor_targets()?);
            Ok(CommandOutput::Text(config_validation_text_for_targets(targets)?))
        }
        "reload" => bail!("config reload requires running host reload support; restart Limux after editing settings"),
        "get" => {
            if args.len() != 2 {
                bail!("Usage: limux config get <sidebar-font-size|surface-tab-bar-font-size>");
            }
            let key = args
                .get(1)
                .ok_or_else(|| anyhow!("Usage: limux config get <sidebar-font-size|surface-tab-bar-font-size>"))?;
            let setting = font_size_setting(key).ok_or_else(|| {
                anyhow!("Usage: limux config get <sidebar-font-size|surface-tab-bar-font-size>")
            })?;
            Ok(CommandOutput::Text(render_config_font_size_get(
                &limux_settings_path()?,
                setting,
            )?))
        }
        "set" => {
            if args.len() != 3 {
                bail!(
                    "Usage: limux config set <sidebar-font-size|surface-tab-bar-font-size> <points>"
                );
            }
            let key = args
                .get(1)
                .ok_or_else(|| anyhow!("Usage: limux config set <sidebar-font-size|surface-tab-bar-font-size> <points>"))?;
            let value = args
                .get(2)
                .ok_or_else(|| anyhow!("Usage: limux config set <sidebar-font-size|surface-tab-bar-font-size> <points>"))?;
            let setting = font_size_setting(key).ok_or_else(|| {
                anyhow!("Usage: limux config set <sidebar-font-size|surface-tab-bar-font-size> <points>")
            })?;
            Ok(CommandOutput::Text(render_config_font_size_set(
                &limux_settings_path()?,
                setting,
                value,
            )?))
        }
        "sidebar-font-size" | "surface-tab-bar-font-size" => {
            if args.len() > 2 {
                bail!("Usage: limux config {sub} [points]");
            }
            let setting = font_size_setting(sub).expect("matched supported setting");
            let path = limux_settings_path()?;
            if let Some(value) = args.get(1) {
                Ok(CommandOutput::Text(render_config_font_size_set(
                    &path, setting, value,
                )?))
            } else {
                Ok(CommandOutput::Text(render_config_font_size_get(
                    &path, setting,
                )?))
            }
        }
        target => bail!("unsupported config command `{target}`"),
    }
}

/// purpose: Implement CMUX-compatible theme list/set/clear commands for Ghostty themes.
/// inputs: Theme subcommand arguments.
/// returns/effects: Reads or writes CMUX's managed block in the Ghostty config.
fn run_themes_command(args: &[String], json_output: bool) -> Result<CommandOutput> {
    let sub = args.first().map(String::as_str).unwrap_or("list");
    match sub {
        "--help" | "-h" => Ok(CommandOutput::Text(
            "Usage: limux themes [list|set|clear]".to_string(),
        )),
        "list" => render_themes_list(&ghostty_config_path()?, json_output),
        "set" => {
            let path = ghostty_config_path()?;
            run_themes_set_at(&path, &args[1..], json_output)
        }
        "clear" => {
            if args.len() > 1 {
                bail!("themes clear does not take any positional arguments");
            }
            run_themes_clear_at(&ghostty_config_path()?, json_output)
        }
        target if !target.starts_with('-') => {
            run_themes_set_at(&ghostty_config_path()?, args, json_output)
        }
        target => bail!("unsupported themes command `{target}`"),
    }
}

/// purpose: Render CMUX-style current theme state for the Ghostty config path.
/// inputs: Ghostty config path plus JSON output preference.
/// returns/effects: Reads config and returns text or JSON output.
fn render_themes_list(path: &Path, json_output: bool) -> Result<CommandOutput> {
    let current = current_theme_selection_at(path)?;
    if json_output {
        return Ok(CommandOutput::Json(json!({
            "themes": [],
            "current": {
                "raw_value": current.raw_value,
                "light": current.light,
                "dark": current.dark,
                "source_path": current.source_path,
            },
            "config_path": path.display().to_string(),
            "catalog": "external_ghostty",
        })));
    }

    Ok(CommandOutput::Text(format!(
        concat!(
            "Current light: {}\n",
            "Current dark: {}\n",
            "Config: {}\n\n",
            "Theme catalog: external Ghostty theme names are accepted; ",
            "run `limux config reload` to validate and apply."
        ),
        current.light.as_deref().unwrap_or("inherit"),
        current.dark.as_deref().unwrap_or("inherit"),
        path.display()
    )))
}

/// purpose: Apply a CMUX-compatible theme selection to Ghostty config.
/// inputs: Ghostty config path, set arguments, and JSON output preference.
/// returns/effects: Writes managed theme block and returns status payload.
fn run_themes_set_at(path: &Path, args: &[String], json_output: bool) -> Result<CommandOutput> {
    let current = current_theme_selection_at(path)?;
    let (light, dark) = parse_theme_set_args(args, &current)?;
    let raw_theme_value = encoded_theme_value(light.as_deref(), dark.as_deref())
        .ok_or_else(|| anyhow!("themes set requires at least one theme"))?;
    write_managed_theme_override_at(path, &raw_theme_value)?;

    if json_output {
        return Ok(CommandOutput::Json(json!({
            "ok": true,
            "light": light,
            "dark": dark,
            "raw_value": raw_theme_value,
            "config_path": path.display().to_string(),
            "reload_requested": false,
            "reload_command": "limux config reload",
        })));
    }

    Ok(CommandOutput::Text(format!(
        "OK light={} dark={} config={} reload=manual",
        light.as_deref().unwrap_or("-"),
        dark.as_deref().unwrap_or("-"),
        path.display()
    )))
}

/// purpose: Clear CMUX's managed Ghostty theme selection.
/// inputs: Ghostty config path plus JSON output preference.
/// returns/effects: Removes the managed block and returns status payload.
fn run_themes_clear_at(path: &Path, json_output: bool) -> Result<CommandOutput> {
    clear_managed_theme_override_at(path)?;
    if json_output {
        return Ok(CommandOutput::Json(json!({
            "ok": true,
            "cleared": true,
            "config_path": path.display().to_string(),
            "reload_requested": false,
            "reload_command": "limux config reload",
        })));
    }
    Ok(CommandOutput::Text(format!(
        "OK cleared config={} reload=manual",
        path.display()
    )))
}

fn should_launch_host(opts: &GlobalOptions) -> bool {
    opts.command_args.is_empty()
        && opts.request.is_none()
        && opts.socket.is_none()
        && opts.socket_mode == SocketMode::Runtime
        && opts.password.is_none()
        && opts.window.is_none()
        && !opts.json_output
        && !opts.pretty
        && opts.id_format == IdFormat::Refs
}

fn host_binary_candidates(exe: &Path) -> Vec<PathBuf> {
    let mut candidates = Vec::new();

    if let Some(bin_dir) = exe.parent() {
        if let Some(prefix) = bin_dir.parent() {
            candidates.push(prefix.join("libexec/limux/limux-host"));
        }

        let sibling_host = bin_dir.join("limux-host");
        if sibling_host != exe {
            candidates.push(sibling_host);
        }

        let sibling_dev_host = bin_dir.join("limux");
        if sibling_dev_host != exe {
            candidates.push(sibling_dev_host);
        }
    }

    candidates
}

fn resolve_host_binary() -> Result<PathBuf> {
    if let Ok(raw) = env::var("LIMUX_HOST_BIN") {
        let path = PathBuf::from(raw);
        if path.is_file() {
            return Ok(path);
        }
    }

    let exe = env::current_exe().context("failed to resolve current executable")?;
    host_binary_candidates(&exe)
        .into_iter()
        .find(|path| path.is_file())
        .ok_or_else(|| {
            anyhow!(
                "could not find limux host binary; expected limux-host next to the installed CLI"
            )
        })
}

fn launch_host() -> Result<()> {
    let host = resolve_host_binary()?;
    let err = Command::new(&host)
        .spawn()
        .with_context(|| format!("failed to launch {}", host.display()))?
        .wait()
        .with_context(|| format!("failed to wait for {}", host.display()))?;
    if err.success() {
        Ok(())
    } else {
        bail!("{} exited with {}", host.display(), err)
    }
}

fn get_string(value: &Value, keys: &[&str]) -> Option<String> {
    for key in keys {
        if let Some(raw) = value.get(*key) {
            match raw {
                Value::String(s) if !s.is_empty() => return Some(s.clone()),
                Value::Number(n) => return Some(n.to_string()),
                _ => {}
            }
        }
    }
    None
}

fn handle_from_payload(value: &Value, id_key: &str, ref_key: &str) -> String {
    get_string(value, &[ref_key])
        .or_else(|| get_string(value, &[id_key]))
        .unwrap_or_default()
}

fn apply_id_format(value: &mut Value, id_format: IdFormat) {
    match value {
        Value::Object(map) => {
            let keys: Vec<String> = map.keys().cloned().collect();
            for key in &keys {
                if key.ends_with("_id") {
                    let prefix = key.trim_end_matches("_id");
                    let ref_key = format!("{}_ref", prefix);
                    match id_format {
                        IdFormat::Refs => {
                            if map.contains_key(&ref_key) {
                                map.remove(key);
                            }
                        }
                        IdFormat::Uuids => {
                            if map.contains_key(key) {
                                map.remove(&ref_key);
                            }
                        }
                        IdFormat::Both => {}
                    }
                }
            }

            match id_format {
                IdFormat::Refs => {
                    if map.contains_key("ref") {
                        map.remove("id");
                    }
                }
                IdFormat::Uuids => {
                    if map.contains_key("id") {
                        map.remove("ref");
                    }
                }
                IdFormat::Both => {}
            }

            let child_keys: Vec<String> = map.keys().cloned().collect();
            for key in child_keys {
                if let Some(child) = map.get_mut(&key) {
                    apply_id_format(child, id_format);
                }
            }
        }
        Value::Array(list) => {
            for item in list {
                apply_id_format(item, id_format);
            }
        }
        _ => {}
    }
}

fn parse_opt(args: &[String], name: &str) -> Option<String> {
    args.windows(2).find_map(|w| {
        if w[0] == name {
            Some(w[1].clone())
        } else {
            None
        }
    })
}

fn parse_opts(args: &[String], name: &str) -> Vec<String> {
    args.windows(2)
        .filter_map(|w| {
            if w[0] == name {
                Some(w[1].clone())
            } else {
                None
            }
        })
        .collect()
}

fn parse_flag(args: &[String], name: &str) -> bool {
    args.iter().any(|a| a == name)
}

// purpose: Parse CMUX boolean option values.
// inputs: Raw value from an option such as --focus.
// returns/effects: Returns a boolean for CMUX aliases or None for invalid text.
fn parse_bool_string(raw: &str) -> Option<bool> {
    match raw.to_ascii_lowercase().as_str() {
        "1" | "true" | "yes" | "on" => Some(true),
        "0" | "false" | "no" | "off" => Some(false),
        _ => None,
    }
}

// purpose: Parse an optional CMUX boolean option while failing loudly on bad values.
// inputs: CLI args and the option name to parse.
// returns/effects: Returns Some(bool) when present, None when absent, or an error.
fn parse_optional_bool_arg(args: &[String], name: &str) -> Result<Option<bool>> {
    let Some(raw) = parse_opt(args, name) else {
        return Ok(None);
    };
    parse_bool_string(&raw)
        .map(Some)
        .ok_or_else(|| anyhow!("{name} must be one of true, false, 1, 0, yes, no, on, or off"))
}

// purpose: Validate CMUX workspace environment variable names.
// inputs: Raw key from --env or --env-file.
// returns/effects: Rejects malformed and managed CMUX/LIMUX keys.
fn validate_workspace_env_key(key: &str) -> Result<()> {
    let mut chars = key.chars();
    let Some(first) = chars.next() else {
        bail!("workspace env keys must not be empty");
    };
    if !(first == '_' || first.is_ascii_alphabetic()) {
        bail!("invalid workspace env key `{key}`");
    }
    if !chars.all(|ch| ch == '_' || ch.is_ascii_alphanumeric()) {
        bail!("invalid workspace env key `{key}`");
    }
    if key.starts_with("CMUX_") || key.starts_with("LIMUX_") {
        bail!("workspace env cannot override managed key `{key}`");
    }
    Ok(())
}

// purpose: Parse one KEY=VALUE workspace environment assignment.
// inputs: Raw assignment from CLI or env-file.
// returns/effects: Returns validated key/value or an explicit error.
fn parse_workspace_env_assignment(raw: &str) -> Result<(String, String)> {
    let (key, value) = raw
        .split_once('=')
        .ok_or_else(|| anyhow!("workspace env assignment must be KEY=VALUE"))?;
    validate_workspace_env_key(key)?;
    Ok((key.to_string(), value.to_string()))
}

// purpose: Read CMUX-compatible KEY=VALUE workspace env-file entries.
// inputs: Env-file path.
// returns/effects: Ignores blank/comment lines and returns validated assignments.
fn read_workspace_env_file(path: &Path) -> Result<BTreeMap<String, String>> {
    let text = fs::read_to_string(path)
        .with_context(|| format!("failed to read env file {}", path.display()))?;
    let mut values = BTreeMap::new();
    for (index, line) in text.lines().enumerate() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let line = line.strip_prefix("export ").unwrap_or(line).trim();
        let (key, value) = parse_workspace_env_assignment(line)
            .with_context(|| format!("invalid env file line {}", index + 1))?;
        values.insert(key, value);
    }
    Ok(values)
}

// purpose: Merge workspace env sources using CMUX precedence.
// inputs: Base project config env plus CLI args with repeated --env-file and --env.
// returns/effects: Returns sorted workspace_env map where explicit CLI values win.
fn parse_workspace_env_args_with_base(
    args: &[String],
    mut values: BTreeMap<String, String>,
) -> Result<BTreeMap<String, String>> {
    for path in parse_opts(args, "--env-file") {
        values.extend(read_workspace_env_file(Path::new(&path))?);
    }
    for raw in parse_opts(args, "--env") {
        let (key, value) = parse_workspace_env_assignment(&raw)?;
        values.insert(key, value);
    }
    Ok(values)
}

// purpose: Locate CMUX project config from the current working directory.
// inputs: Process cwd and optional HOME boundary from the environment.
// returns/effects: Returns the nearest `.cmux/cmux.json` or `cmux.json`, if any.
fn find_project_cmux_config_path() -> Result<Option<PathBuf>> {
    let cwd = env::current_dir().context("failed to read current directory")?;
    let home = env::var_os("HOME").map(PathBuf::from);
    Ok(find_project_cmux_config_path_from(&cwd, home.as_deref()))
}

// purpose: Implement CMUX's upward project config search order.
// inputs: Starting directory and optional home boundary.
// returns/effects: Stops before HOME, checking `.cmux/cmux.json` then `cmux.json`.
fn find_project_cmux_config_path_from(start: &Path, home: Option<&Path>) -> Option<PathBuf> {
    let mut current = start.to_path_buf();
    loop {
        if home.is_some_and(|home| current == home) {
            return None;
        }
        if let Some(candidate) = project_cmux_config_candidate(&current) {
            return Some(candidate);
        }
        if !current.pop() {
            return None;
        }
    }
}

// purpose: Find the CMUX config candidate at one directory level.
// inputs: Directory to inspect.
// returns/effects: Prefers `.cmux/cmux.json` before sibling `cmux.json`.
fn project_cmux_config_candidate(directory: &Path) -> Option<PathBuf> {
    [
        directory.join(".cmux").join("cmux.json"),
        directory.join("cmux.json"),
    ]
    .into_iter()
    .find(|candidate| candidate.is_file())
}

// purpose: Load CMUX project workspace env for workspace creation parity.
// inputs: Optional project cmux.json discovered from the current directory.
// returns/effects: Fails loudly on malformed JSON or malformed workspace env keys.
fn read_project_workspace_env() -> Result<BTreeMap<String, String>> {
    let Some(path) = find_project_cmux_config_path()? else {
        return Ok(BTreeMap::new());
    };
    read_project_workspace_env_at(&path)
}

// purpose: Decode workspace env from CMUX project config workspace definitions.
// inputs: A project cmux.json path.
// returns/effects: Reads top-level workspace.env or newWorkspaceCommand's workspace.env.
fn read_project_workspace_env_at(path: &Path) -> Result<BTreeMap<String, String>> {
    let text = fs::read_to_string(path)
        .with_context(|| format!("failed to read project config {}", path.display()))?;
    let root: Value = serde_json::from_str(&text)
        .with_context(|| format!("failed to parse project config {}", path.display()))?;
    if let Some(workspace) = root.get("workspace") {
        return workspace_env_from_config_value(workspace, "workspace.env");
    }
    let Some(command_name) = root.get("newWorkspaceCommand").and_then(Value::as_str) else {
        return Ok(BTreeMap::new());
    };
    let Some(commands) = root.get("commands").and_then(Value::as_array) else {
        return Ok(BTreeMap::new());
    };
    for command in commands {
        if command.get("name").and_then(Value::as_str) != Some(command_name) {
            continue;
        }
        if let Some(workspace) = command.get("workspace") {
            return workspace_env_from_config_value(workspace, "commands[].workspace.env");
        }
        return Ok(BTreeMap::new());
    }
    Ok(BTreeMap::new())
}

// purpose: Validate a CMUX workspace definition env object.
// inputs: JSON object containing optional env.
// returns/effects: Returns sorted env values or explicit schema/key errors.
fn workspace_env_from_config_value(value: &Value, label: &str) -> Result<BTreeMap<String, String>> {
    let Some(env_value) = value.get("env") else {
        return Ok(BTreeMap::new());
    };
    let object = env_value
        .as_object()
        .ok_or_else(|| anyhow!("{label} must be an object of string values"))?;
    let mut values = BTreeMap::new();
    for (key, raw_value) in object {
        validate_workspace_env_key(key).with_context(|| format!("invalid {label} key"))?;
        let value = raw_value
            .as_str()
            .ok_or_else(|| anyhow!("{label}.{key} must be a string"))?;
        values.insert(key.to_string(), value.to_string());
    }
    Ok(values)
}

fn positional_arg(args: &[String], index: usize) -> Option<String> {
    let mut position = 0usize;
    let mut skip = false;
    for arg in args {
        if skip {
            skip = false;
            continue;
        }
        if arg == "--agent" {
            skip = true;
            continue;
        }
        if arg.starts_with('-') {
            continue;
        }
        if position == index {
            return Some(arg.clone());
        }
        position += 1;
    }
    None
}

fn trailing_title(args: &[String]) -> Option<String> {
    let mut filtered: Vec<String> = Vec::new();
    let mut skip = false;
    for arg in args {
        if skip {
            skip = false;
            continue;
        }
        if arg == "--workspace"
            || arg == "--tab"
            || arg == "--surface"
            || arg == "--pane"
            || arg == "--target-pane"
            || arg == "--action"
            || arg == "--title"
            || arg == "--url"
            || arg == "--cwd"
            || arg == "--command"
            || arg == "--direction"
            || arg == "--type"
            || arg == "--lines"
            || arg == "--timeout"
            || arg == "--timeout-ms"
            || arg == "--name"
            || arg == "--out"
            || arg == "--subtitle"
            || arg == "--body"
            || arg == "--message"
            || arg == "--event"
            || arg == "--agents"
            || arg == "--selector"
            || arg == "--text"
            || arg == "--attr"
            || arg == "--property"
            || arg == "--value"
            || arg == "--amount"
            || arg == "--unset"
            || arg == "-b"
        {
            skip = true;
            continue;
        }
        if arg.starts_with('-') {
            continue;
        }
        filtered.push(arg.clone());
    }
    if filtered.is_empty() {
        None
    } else {
        Some(filtered.join(" "))
    }
}

fn wait_signal_path(socket: &Path, name: &str) -> PathBuf {
    let sanitized: String = name
        .chars()
        .map(|ch| {
            if ch.is_ascii_alphanumeric() || ch == '-' || ch == '_' {
                ch
            } else {
                '_'
            }
        })
        .collect();
    cli_state_dir(socket)
        .join("wait")
        .join(format!("{sanitized}.sig"))
}

fn ensure_private_cli_dir(path: &Path) -> Result<()> {
    fs::create_dir_all(path).with_context(|| format!("failed to create {}", path.display()))?;
    fs::set_permissions(path, fs::Permissions::from_mode(PRIVATE_CLI_DIR_MODE))
        .with_context(|| format!("failed to lock down {}", path.display()))
}

fn create_wait_signal(path: &Path) -> Result<()> {
    let parent = path
        .parent()
        .ok_or_else(|| anyhow!("wait-for signal path has no parent: {}", path.display()))?;
    ensure_private_cli_dir(parent)?;
    fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(WAIT_MARKER_MODE)
        .open(path)
        .with_context(|| format!("failed to create wait-for signal {}", path.display()))?;
    Ok(())
}

fn remove_wait_signal(path: &Path) -> Result<()> {
    let metadata = fs::symlink_metadata(path)
        .with_context(|| format!("failed to inspect wait-for signal {}", path.display()))?;
    if !metadata.file_type().is_file() {
        bail!(
            "refusing to remove non-file wait-for signal {}",
            path.display()
        );
    }
    fs::remove_file(path)
        .with_context(|| format!("failed to remove wait-for signal {}", path.display()))
}

fn read_json_map(path: &str) -> BTreeMap<String, String> {
    let raw = fs::read_to_string(path).unwrap_or_default();
    serde_json::from_str::<BTreeMap<String, String>>(&raw).unwrap_or_default()
}

fn write_json_map(path: &Path, map: &BTreeMap<String, String>) -> Result<()> {
    let encoded = serde_json::to_string_pretty(map).context("failed to encode json map")?;
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)
            .with_context(|| format!("failed to create {}", parent.display()))?;
    }
    let nonce = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    let tmp = path.with_extension(format!("tmp-{}-{}", std::process::id(), nonce));
    fs::write(&tmp, encoded).with_context(|| format!("failed to write {}", tmp.display()))?;
    fs::rename(&tmp, path).with_context(|| format!("failed to replace {}", path.display()))?;
    Ok(())
}

fn socket_state_namespace(socket: &Path) -> String {
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    socket.hash(&mut hasher);
    format!("{:016x}", hasher.finish())
}

fn cli_state_dir(socket: &Path) -> PathBuf {
    env::temp_dir()
        .join("limux-cli")
        .join(socket_state_namespace(socket))
}

fn cli_state_path(socket: &Path, kind: &str) -> PathBuf {
    cli_state_dir(socket).join(format!("{kind}.json"))
}

fn cli_state_lock_path(socket: &Path, kind: &str) -> PathBuf {
    cli_state_dir(socket).join(format!("{kind}.lock"))
}

struct CliStateLock {
    path: PathBuf,
}

impl Drop for CliStateLock {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.path);
    }
}

fn acquire_cli_state_lock(socket: &Path, kind: &str) -> Result<CliStateLock> {
    let dir = cli_state_dir(socket);
    fs::create_dir_all(&dir).with_context(|| format!("failed to create {}", dir.display()))?;
    let lock_path = cli_state_lock_path(socket, kind);
    let deadline = Instant::now() + CLI_STATE_LOCK_TIMEOUT;
    loop {
        match fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&lock_path)
        {
            Ok(_) => return Ok(CliStateLock { path: lock_path }),
            Err(err) if err.kind() == ErrorKind::AlreadyExists => {
                if Instant::now() >= deadline {
                    bail!("timed out acquiring CLI state lock {}", lock_path.display());
                }
                std::thread::sleep(CLI_STATE_LOCK_RETRY);
            }
            Err(err) => {
                return Err(err).with_context(|| {
                    format!("failed to create CLI state lock {}", lock_path.display())
                });
            }
        }
    }
}

fn with_locked_json_map<T, F>(socket: &Path, kind: &str, update: F) -> Result<T>
where
    F: FnOnce(&mut BTreeMap<String, String>, &Path) -> Result<T>,
{
    let _lock = acquire_cli_state_lock(socket, kind)?;
    let path = cli_state_path(socket, kind);
    let path_str = path.to_string_lossy().to_string();
    let mut map = read_json_map(&path_str);
    update(&mut map, &path)
}

// purpose: Resolve named tmux-compat buffer content without silent empty paste.
// inputs: Buffer map and requested buffer name.
// returns/effects: Returns cloned text or fails when the buffer is absent.
fn tmux_buffer_text(buffers: &BTreeMap<String, String>, name: &str) -> Result<String> {
    buffers
        .get(name)
        .cloned()
        .ok_or_else(|| anyhow!("Buffer not found: {name}"))
}

// purpose: Convert a stable text id into tmux-style numeric handle material.
// inputs: A workspace, pane, or surface identifier.
// returns/effects: Returns a deterministic hash string for user-facing tmux refs.
fn tmux_stable_numeric_id(value: &str) -> String {
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    value.hash(&mut hasher);
    (hasher.finish() % 1_000_000_000).to_string()
}

// purpose: Parse CMUX/tmux display-message arguments without affecting shared parsers.
// inputs: Raw display-message argv after the command name.
// returns/effects: Returns print mode, target, and chosen format string.
fn parse_tmux_display_message_args(args: &[String]) -> (bool, Option<String>, Option<String>) {
    let mut print = false;
    let mut target = None;
    let mut flag_format = None;
    let mut positional = Vec::new();
    let mut index = 0usize;
    while index < args.len() {
        match args[index].as_str() {
            "-p" | "--print" => print = true,
            "-F" | "--format" => {
                index += 1;
                flag_format = args.get(index).cloned();
            }
            "-t" | "--target" => {
                index += 1;
                target = args.get(index).cloned();
            }
            value if value.starts_with('-') => {}
            value => positional.push(value.to_string()),
        }
        index += 1;
    }
    let format = if positional.is_empty() {
        flag_format
    } else {
        Some(positional.join(" "))
    };
    (print, target, format)
}

// purpose: Render tmux format keys with CMUX-compatible unknown-key stripping.
// inputs: Optional tmux format, context values, and fallback text.
// returns/effects: Returns trimmed rendered text or the fallback when empty.
fn tmux_render_format(
    format: Option<&str>,
    context: &BTreeMap<String, String>,
    fallback: &str,
) -> String {
    let Some(format) = format.filter(|raw| !raw.is_empty()) else {
        return fallback.to_string();
    };
    let mut rendered = String::new();
    let mut rest = format;
    while let Some(start) = rest.find("#{") {
        rendered.push_str(&rest[..start]);
        let after_start = &rest[start + 2..];
        let Some(end) = after_start.find('}') else {
            rendered.push_str(&rest[start..]);
            rest = "";
            break;
        };
        let key = &after_start[..end];
        if let Some(value) = context.get(key) {
            rendered.push_str(value);
        }
        rest = &after_start[end + 1..];
    }
    rendered.push_str(rest);
    let trimmed = rendered.trim();
    if trimmed.is_empty() {
        fallback.to_string()
    } else {
        trimmed.to_string()
    }
}

// purpose: Build the default CMUX/tmux format fields available before host lookup.
// inputs: Environment and cwd available to the CLI process.
// returns/effects: Returns a context map for display-message expansion.
fn base_tmux_format_context() -> BTreeMap<String, String> {
    let cwd = env::current_dir()
        .ok()
        .and_then(|path| path.to_str().map(str::to_string))
        .unwrap_or_default();
    let mut context = BTreeMap::from([
        ("session_name".to_string(), "cmux".to_string()),
        ("session_attached".to_string(), "1".to_string()),
        ("window_active".to_string(), "1".to_string()),
        ("window_flags".to_string(), "*".to_string()),
        ("window_width".to_string(), "80".to_string()),
        ("window_height".to_string(), "24".to_string()),
        ("pane_active".to_string(), "1".to_string()),
        ("pane_width".to_string(), "80".to_string()),
        ("pane_height".to_string(), "24".to_string()),
        ("pane_current_path".to_string(), cwd),
    ]);
    for (limux_key, cmux_key) in [
        ("LIMUX_WORKSPACE_ID", "CMUX_WORKSPACE_ID"),
        ("LIMUX_SURFACE_ID", "CMUX_SURFACE_ID"),
        ("LIMUX_PANE_ID", "CMUX_PANE_ID"),
        ("LIMUX_TAB_ID", "CMUX_TAB_ID"),
    ] {
        if let Some(value) = context_env_value(limux_key) {
            context.insert(limux_key.to_ascii_lowercase(), value.clone());
            context.insert(cmux_key.to_ascii_lowercase(), value);
        }
    }
    context
}

// purpose: Return a cloned JSON array from a control payload key.
// inputs: JSON object payload and the expected array key.
// returns/effects: Returns rows or an empty list when the host omits the key.
fn payload_array(payload: &Value, key: &str) -> Vec<Value> {
    payload
        .get(key)
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default()
}

// purpose: Apply a tmux target string to the current pane/surface handles.
// inputs: Optional -t/--target value plus mutable pane and surface handles.
// returns/effects: Updates the handle kind inferred from the target syntax.
fn apply_tmux_target(
    target: Option<&str>,
    pane_id: &mut Option<String>,
    surface_id: &mut Option<String>,
) {
    let Some(target) = target else {
        return;
    };
    if target.starts_with('%') || target.starts_with("pane:") {
        *pane_id = Some(target.trim_start_matches('%').to_string());
    } else {
        *surface_id = Some(target.trim_start_matches('$').to_string());
    }
}

// purpose: Select a row by id, then focused flag, then first row.
// inputs: Candidate rows, an optional id, and accepted id key names.
// returns/effects: Returns the best matching row for tmux format context.
fn selected_tmux_row<'a>(
    rows: &'a [Value],
    id: Option<&str>,
    id_keys: &[&str],
) -> Option<&'a Value> {
    id.and_then(|id| {
        rows.iter()
            .find(|row| get_string(row, id_keys).as_deref() == Some(id))
    })
    .or_else(|| {
        rows.iter()
            .find(|row| row.get("focused").and_then(Value::as_bool) == Some(true))
    })
    .or_else(|| rows.first())
}

// purpose: Return a trimmed non-empty title-like field from a payload.
// inputs: JSON row with possible title or name keys.
// returns/effects: Returns None for empty or whitespace-only names.
fn nonempty_row_title(row: &Value) -> Option<String> {
    get_string(row, &["title", "name"]).filter(|value| !value.trim().is_empty())
}

struct TmuxListContextSpec<'a> {
    method: &'a str,
    rows_key: &'a str,
    selected_id: Option<&'a str>,
    id_keys: &'a [&'a str],
    insert: fn(&mut BTreeMap<String, String>, &Value),
}

struct TmuxTargetContext {
    workspace_id: Option<String>,
    pane_id: Option<String>,
    surface_id: Option<String>,
}

// purpose: Build the current-surface request params from optional workspace context.
// inputs: Optional workspace id from environment.
// returns/effects: Returns an empty or workspace-scoped JSON request object.
fn current_surface_params(workspace: Option<&str>) -> Value {
    workspace
        .map(|id| json!({"workspace_id": id}))
        .unwrap_or_else(|| json!({}))
}

// purpose: Resolve current workspace, pane, and surface handles for tmux formats.
// inputs: Live client and optional tmux target string.
// returns/effects: Performs surface.current and applies target override syntax.
async fn tmux_target_context(
    client: &mut Client,
    target: Option<&str>,
) -> Result<TmuxTargetContext> {
    let workspace = context_env_value("LIMUX_WORKSPACE_ID");
    let current = client
        .call(
            "surface.current",
            current_surface_params(workspace.as_deref()),
        )
        .await?;
    let workspace_id = get_string(&current, &["workspace_id", "workspace_ref"]).or(workspace);
    let mut pane_id = get_string(&current, &["pane_id", "pane_ref"]);
    let mut surface_id = get_string(&current, &["surface_id", "surface_ref"]);
    apply_tmux_target(target, &mut pane_id, &mut surface_id);
    Ok(TmuxTargetContext {
        workspace_id,
        pane_id,
        surface_id,
    })
}

// purpose: Add host workspace, pane, and surface values to a tmux format context.
// inputs: A live client plus optional tmux target string from -t/--target.
// returns/effects: Performs bounded RPC lookups and returns CMUX-style fields.
async fn tmux_format_context(
    client: &mut Client,
    target: Option<&str>,
) -> Result<BTreeMap<String, String>> {
    let mut context = base_tmux_format_context();
    let target_context = tmux_target_context(client, target).await?;
    enrich_tmux_workspace_context(client, &mut context, target_context.workspace_id.as_deref())
        .await?;
    enrich_tmux_list_context(
        client,
        &mut context,
        target_context.workspace_id.as_deref(),
        TmuxListContextSpec {
            method: "pane.list",
            rows_key: "panes",
            selected_id: target_context.pane_id.as_deref(),
            id_keys: &["pane_id", "id"],
            insert: insert_tmux_pane_row,
        },
    )
    .await?;
    enrich_tmux_list_context(
        client,
        &mut context,
        target_context.workspace_id.as_deref(),
        TmuxListContextSpec {
            method: "surface.list",
            rows_key: "surfaces",
            selected_id: target_context.surface_id.as_deref(),
            id_keys: &["surface_id", "id"],
            insert: insert_tmux_surface_row,
        },
    )
    .await?;
    Ok(context)
}

// purpose: Add workspace/session fields from workspace.list to a tmux context.
// inputs: Client, mutable context, and optional active workspace id.
// returns/effects: Updates context from host data when available.
async fn enrich_tmux_workspace_context(
    client: &mut Client,
    context: &mut BTreeMap<String, String>,
    workspace_id: Option<&str>,
) -> Result<()> {
    let Some(workspace_id) = workspace_id else {
        return Ok(());
    };
    context.insert(
        "session_id".to_string(),
        format!("${}", tmux_stable_numeric_id(workspace_id)),
    );
    context.insert(
        "window_id".to_string(),
        format!("@{}", tmux_stable_numeric_id(workspace_id)),
    );
    context.insert("window_uuid".to_string(), workspace_id.to_string());
    let payload = client.call("workspace.list", json!({})).await?;
    let workspaces = payload_array(&payload, "workspaces");
    if let Some(row) = workspaces
        .iter()
        .find(|row| get_string(row, &["workspace_id", "id"]).as_deref() == Some(workspace_id))
    {
        if let Some(index) = row.get("index").and_then(Value::as_u64) {
            context.insert("window_index".to_string(), index.to_string());
        }
        if let Some(title) = nonempty_row_title(row) {
            context.insert("window_name".to_string(), title);
        }
    }
    Ok(())
}

// purpose: Add the chosen pane row fields to a tmux context.
// inputs: Mutable context and the selected pane row.
// returns/effects: Updates pane handle, index, and active state.
fn insert_tmux_pane_row(context: &mut BTreeMap<String, String>, pane: &Value) {
    if let Some(id) = get_string(pane, &["pane_id", "id"]) {
        context.insert(
            "pane_id".to_string(),
            format!("%{}", tmux_stable_numeric_id(&id)),
        );
        context.insert("pane_uuid".to_string(), id);
    }
    if let Some(index) = pane.get("index").and_then(Value::as_u64) {
        context.insert("pane_index".to_string(), index.to_string());
    }
    if let Some(focused) = pane.get("focused").and_then(Value::as_bool) {
        context.insert(
            "pane_active".to_string(),
            if focused { "1" } else { "0" }.to_string(),
        );
    }
}

// purpose: Add selected rows from a host list route to a tmux context.
// inputs: Client, context, workspace id, list route, selection keys, and row inserter.
// returns/effects: Calls the host list route and inserts the selected row when present.
async fn enrich_tmux_list_context(
    client: &mut Client,
    context: &mut BTreeMap<String, String>,
    workspace_id: Option<&str>,
    spec: TmuxListContextSpec<'_>,
) -> Result<()> {
    let Some(workspace_id) = workspace_id else {
        return Ok(());
    };
    let payload = client
        .call(spec.method, json!({"workspace_id": workspace_id}))
        .await?;
    let rows = payload_array(&payload, spec.rows_key);
    if let Some(row) = selected_tmux_row(&rows, spec.selected_id, spec.id_keys) {
        (spec.insert)(context, row);
    }
    Ok(())
}

// purpose: Add the chosen surface row fields to a tmux context.
// inputs: Mutable context and the selected surface row.
// returns/effects: Updates surface id, pane title, and window fallback name.
fn insert_tmux_surface_row(context: &mut BTreeMap<String, String>, surface: &Value) {
    if let Some(id) = get_string(surface, &["surface_id", "id"]) {
        context.insert("surface_id".to_string(), id);
    }
    if let Some(title) = nonempty_row_title(surface) {
        context.insert("pane_title".to_string(), title.clone());
        context.entry("window_name".to_string()).or_insert(title);
    }
}

// purpose: Build the CMUX-compatible respawn payload for a terminal surface.
// inputs: Raw CLI args after `respawn-pane`.
// returns/effects: Returns optional workspace scope plus surface.respawn params.
fn build_respawn_pane_request(args: &[String]) -> Result<(Option<String>, Value)> {
    let workspace = parse_opt(args, "--workspace");
    let surface = parse_opt(args, "--surface");
    let command = parse_opt(args, "--command")
        .or_else(|| trailing_title(args))
        .unwrap_or_else(|| "exec ${SHELL:-/bin/sh} -l".to_string());
    let command = command.trim().to_string();
    if command.is_empty() {
        bail!("respawn-pane requires non-empty command text");
    }

    let mut p = Map::new();
    if let Some(surface) = surface {
        p.insert("surface_id".to_string(), Value::String(surface));
    }
    p.insert("command".to_string(), Value::String(command.clone()));
    p.insert("tmux_start_command".to_string(), Value::String(command));
    Ok((workspace, Value::Object(p)))
}

async fn resolve_current_workspace(client: &mut Client) -> Result<String> {
    let current = client.call("workspace.current", json!({})).await?;
    get_string(&current, &["workspace_id", "workspace_ref"])
        .ok_or_else(|| anyhow!("workspace.current returned no workspace handle"))
}

async fn call_in_workspace_scope(
    client: &mut Client,
    workspace: Option<String>,
    method: &str,
    params: Value,
) -> Result<Value> {
    if let Some(target) = workspace {
        let mut map = match params {
            Value::Object(map) => map,
            Value::Null => Map::new(),
            _ => bail!("{method} requires object params for workspace-scoped calls"),
        };
        map.entry("workspace_id".to_string())
            .or_insert(Value::String(target));
        return client.call(method, Value::Object(map)).await;
    }
    client.call(method, params).await
}

async fn browser_call(
    client: &mut Client,
    surface: Option<String>,
    method: &str,
    mut params: Map<String, Value>,
) -> Result<Value> {
    if let Some(surface) = surface {
        params.insert("surface_id".to_string(), Value::String(surface));
    }
    client.call(method, Value::Object(params)).await
}

async fn selected_surface_for_pane(
    client: &mut Client,
    workspace: Option<String>,
    pane_id: &str,
) -> Result<String> {
    let payload = call_in_workspace_scope(
        client,
        workspace,
        "pane.surfaces",
        json!({ "pane_id": pane_id }),
    )
    .await?;
    let rows = payload
        .get("surfaces")
        .and_then(Value::as_array)
        .ok_or_else(|| anyhow!("pane.surfaces returned no surfaces"))?;

    for row in rows {
        let focused = row.get("focused").and_then(Value::as_bool).unwrap_or(false)
            || row
                .get("selected")
                .and_then(Value::as_bool)
                .unwrap_or(false);
        if focused {
            let handle = handle_from_payload(row, "surface_id", "surface_ref");
            if !handle.is_empty() {
                return Ok(handle);
            }
        }
    }

    let first = rows
        .first()
        .ok_or_else(|| anyhow!("pane has no surfaces"))?;
    let handle = handle_from_payload(first, "surface_id", "surface_ref");
    if handle.is_empty() {
        bail!("pane.surfaces returned an empty surface handle");
    }
    Ok(handle)
}

async fn run_identify(client: &mut Client, args: &[String]) -> Result<Value> {
    let workspace = parse_opt(args, "--workspace");
    let surface = parse_opt(args, "--surface");
    let no_caller = parse_flag(args, "--no-caller");

    let mut params = Map::new();
    if workspace.is_some() || surface.is_some() {
        let mut caller = Map::new();
        if let Some(workspace) = workspace {
            caller.insert("workspace_id".to_string(), Value::String(workspace));
        }
        if let Some(surface) = surface {
            caller.insert("surface_id".to_string(), Value::String(surface));
        }
        params.insert("caller".to_string(), Value::Object(caller));
    }

    let mut payload = client
        .call("system.identify", Value::Object(params))
        .await?;
    if no_caller {
        if let Some(map) = payload.as_object_mut() {
            map.remove("caller");
        }
    }
    Ok(payload)
}

async fn run_list(client: &mut Client, command: &str, args: &[String]) -> Result<Value> {
    let workspace = parse_opt(args, "--workspace")
        .or_else(|| context_env_value("LIMUX_WORKSPACE_ID"))
        .filter(|value| !value.trim().is_empty());
    let params = if let Some(workspace) = workspace.as_ref() {
        json!({ "workspace_id": workspace })
    } else {
        json!({})
    };
    let method = match command {
        "list-panels" => "surface.list",
        "list-panes" => "pane.list",
        "list-workspaces" => "workspace.list",
        "list-workspace-groups" => "workspace.group.list",
        "surface-health" => "surface.health",
        _ => bail!("unsupported list command"),
    };
    let mut payload = client.call(method, params).await?;
    if let Some(workspace) = workspace.as_ref() {
        if let Some(map) = payload.as_object_mut() {
            if workspace.contains(':') {
                map.entry("workspace_ref".to_string())
                    .or_insert_with(|| Value::String(workspace.clone()));
            } else {
                map.entry("workspace_id".to_string())
                    .or_insert_with(|| Value::String(workspace.clone()));
            }
        }
    }
    Ok(payload)
}

// purpose: Run CMUX-compatible workspace-group subcommands through the host socket.
// inputs: Subcommand args, currently supporting the read-only `list` slice.
// returns/effects: Sends one control request or fails explicitly on unsupported mutations.
async fn run_workspace_group_command(client: &mut Client, args: &[String]) -> Result<Value> {
    let subcommand = args.first().map(String::as_str).unwrap_or("list");
    let rest = args.get(1..).unwrap_or(&[]);
    match subcommand {
        "list" | "ls" => client.call("workspace.group.list", json!({})).await,
        "create" => {
            let mut params = Map::new();
            if let Some(name) = parse_opt(rest, "--name").or_else(|| first_positional(rest)) {
                params.insert("name".to_string(), Value::String(name));
            }
            if let Some(cwd) = parse_opt(rest, "--cwd") {
                params.insert("cwd".to_string(), Value::String(cwd));
            }
            if let Some(from) = parse_opt(rest, "--from") {
                params.insert("from".to_string(), Value::String(from));
            }
            client
                .call("workspace.group.create", Value::Object(params))
                .await
        }
        "ungroup" | "delete" | "collapse" | "expand" | "pin" | "unpin" | "focus" => {
            let group = workspace_group_arg(subcommand, rest)?;
            let method = format!("workspace.group.{}", subcommand.replace('-', "_"));
            client.call(&method, json!({ "group_id": group })).await
        }
        "rename" => {
            let group = workspace_group_arg(subcommand, rest)?;
            let name = parse_opt(rest, "--name")
                .or_else(|| positional_arg(rest, 1))
                .ok_or_else(|| anyhow!("workspace-group rename requires --name or a new name"))?;
            client
                .call(
                    "workspace.group.rename",
                    json!({ "group_id": group, "name": name }),
                )
                .await
        }
        "add" => {
            let group = workspace_group_arg(subcommand, rest)?;
            let workspace = parse_opt(rest, "--workspace")
                .or_else(|| positional_arg(rest, 1))
                .ok_or_else(|| anyhow!("workspace-group add requires --workspace"))?;
            client
                .call(
                    "workspace.group.add",
                    json!({ "group_id": group, "workspace_id": workspace }),
                )
                .await
        }
        "remove" => {
            let workspace = parse_opt(rest, "--workspace")
                .or_else(|| first_positional(rest))
                .ok_or_else(|| anyhow!("workspace-group remove requires --workspace"))?;
            client
                .call(
                    "workspace.group.remove",
                    json!({ "workspace_id": workspace }),
                )
                .await
        }
        "set-anchor" => {
            let group = workspace_group_arg(subcommand, rest)?;
            let workspace = parse_opt(rest, "--workspace")
                .or_else(|| positional_arg(rest, 1))
                .ok_or_else(|| anyhow!("workspace-group set-anchor requires --workspace"))?;
            client
                .call(
                    "workspace.group.set_anchor",
                    json!({ "group_id": group, "workspace_id": workspace }),
                )
                .await
        }
        "new-workspace" => {
            let group = workspace_group_arg(subcommand, rest)?;
            let mut params = Map::new();
            params.insert("group_id".to_string(), Value::String(group));
            if let Some(placement) = parse_opt(rest, "--placement") {
                params.insert("placement".to_string(), Value::String(placement));
            }
            client
                .call("workspace.group.new_workspace", Value::Object(params))
                .await
        }
        "set-color" => {
            let group = workspace_group_arg(subcommand, rest)?;
            let color = parse_opt(rest, "--hex")
                .or_else(|| parse_opt(rest, "--color"))
                .or_else(|| positional_arg(rest, 1));
            client
                .call(
                    "workspace.group.set_color",
                    json!({ "group_id": group, "hex": color }),
                )
                .await
        }
        "set-icon" => {
            let group = workspace_group_arg(subcommand, rest)?;
            let symbol = parse_opt(rest, "--symbol")
                .or_else(|| parse_opt(rest, "--icon"))
                .or_else(|| positional_arg(rest, 1));
            client
                .call(
                    "workspace.group.set_icon",
                    json!({ "group_id": group, "symbol": symbol }),
                )
                .await
        }
        "move" => {
            let group = workspace_group_arg(subcommand, rest)?;
            let raw_index = parse_opt(rest, "--index")
                .or_else(|| positional_arg(rest, 1))
                .ok_or_else(|| anyhow!("workspace-group move requires --index"))?;
            let index = raw_index
                .parse::<usize>()
                .with_context(|| format!("invalid workspace-group move index `{raw_index}`"))?;
            client
                .call(
                    "workspace.group.move",
                    json!({ "group_id": group, "index": index }),
                )
                .await
        }
        other => bail!("unsupported workspace-group command `{other}`"),
    }
}

// purpose: Extract the group id accepted by CMUX workspace-group commands.
// inputs: Subcommand name plus raw subcommand args after the subcommand.
// returns/effects: Returns a group id/ref or fails loudly with command context.
fn workspace_group_arg(subcommand: &str, args: &[String]) -> Result<String> {
    parse_opt(args, "--group")
        .or_else(|| first_positional(args))
        .ok_or_else(|| anyhow!("workspace-group {subcommand} requires a group id or --group"))
}

// purpose: Render list subcommands as rows and mutations as explicit payload text.
// inputs: Original workspace-group args plus the returned payload.
// returns/effects: Returns user-facing CLI text without contacting the host.
fn render_workspace_group_text(args: &[String], payload: &Value) -> String {
    let subcommand = args.first().map(String::as_str).unwrap_or("list");
    if matches!(subcommand, "list" | "ls") {
        return render_list_text("list-workspace-groups", payload);
    }
    default_text_output(payload)
}

fn render_list_text(command: &str, payload: &Value) -> String {
    match command {
        "list-panels" => {
            let rows = payload
                .get("surfaces")
                .and_then(Value::as_array)
                .cloned()
                .unwrap_or_default();
            if rows.is_empty() {
                return "No surfaces".to_string();
            }
            rows.iter()
                .map(|row| {
                    let handle = handle_from_payload(row, "surface_id", "surface_ref");
                    let title = get_string(row, &["title"]).unwrap_or_default();
                    format!("{} {}", handle, title)
                })
                .collect::<Vec<_>>()
                .join("\n")
        }
        "list-panes" => {
            let rows = payload
                .get("panes")
                .and_then(Value::as_array)
                .cloned()
                .unwrap_or_default();
            if rows.is_empty() {
                return "No panes".to_string();
            }
            rows.iter()
                .map(|row| {
                    let handle = handle_from_payload(row, "pane_id", "pane_ref");
                    let count = row
                        .get("surface_count")
                        .and_then(Value::as_u64)
                        .unwrap_or(0);
                    format!("{} surfaces={}", handle, count)
                })
                .collect::<Vec<_>>()
                .join("\n")
        }
        "list-workspaces" => {
            let rows = payload
                .get("workspaces")
                .and_then(Value::as_array)
                .cloned()
                .unwrap_or_default();
            if rows.is_empty() {
                return "No workspaces".to_string();
            }
            rows.iter()
                .map(|row| {
                    let handle = handle_from_payload(row, "workspace_id", "workspace_ref");
                    let title = get_string(row, &["title", "name"]).unwrap_or_default();
                    let selected = row
                        .get("selected")
                        .and_then(Value::as_bool)
                        .unwrap_or(false);
                    if selected {
                        format!("* {} {}", handle, title)
                    } else {
                        format!("  {} {}", handle, title)
                    }
                })
                .collect::<Vec<_>>()
                .join("\n")
        }
        "list-workspace-groups" => {
            let rows = payload
                .get("groups")
                .and_then(Value::as_array)
                .cloned()
                .unwrap_or_default();
            if rows.is_empty() {
                return "No workspace groups".to_string();
            }
            rows.iter()
                .map(|row| {
                    let handle = handle_from_payload(row, "group_id", "group_ref");
                    let title = get_string(row, &["title", "name"]).unwrap_or_default();
                    let pinned = row
                        .get("isPinned")
                        .and_then(Value::as_bool)
                        .unwrap_or(false);
                    if pinned {
                        format!("* {} {}", handle, title)
                    } else {
                        format!("  {} {}", handle, title)
                    }
                })
                .collect::<Vec<_>>()
                .join("\n")
        }
        "surface-health" => {
            let rows = payload
                .get("surfaces")
                .and_then(Value::as_array)
                .cloned()
                .unwrap_or_default();
            if rows.is_empty() {
                return "No surfaces".to_string();
            }
            rows.iter()
                .map(|row| {
                    let handle = handle_from_payload(row, "surface_id", "surface_ref");
                    let healthy = row.get("healthy").and_then(Value::as_bool).unwrap_or(true);
                    format!("{} healthy={}", handle, healthy)
                })
                .collect::<Vec<_>>()
                .join("\n")
        }
        _ => "".to_string(),
    }
}

async fn run_memory(client: &mut Client, args: &[String]) -> Result<Value> {
    let group_limit = parse_opt(args, "--groups")
        .map(|raw| {
            raw.parse::<u64>()
                .ok()
                .filter(|value| (1..=100).contains(value))
                .ok_or_else(|| anyhow!("memory --groups must be an integer from 1 to 100"))
        })
        .transpose()?
        .unwrap_or(12);

    client
        .call("system.memory", json!({ "top_group_limit": group_limit }))
        .await
}

fn render_memory_text(payload: &Value, id_format: IdFormat) -> String {
    let Some(diagnostic) = payload.get("memory_diagnostic").and_then(Value::as_object) else {
        return "No memory diagnostic available".to_string();
    };
    let app = diagnostic.get("app").and_then(Value::as_object);
    let children = diagnostic.get("children").and_then(Value::as_object);
    let summary = diagnostic
        .get("summary")
        .and_then(Value::as_str)
        .unwrap_or_default();

    let mut lines = Vec::new();
    if !summary.is_empty() {
        lines.push(summary.to_string());
        lines.push(String::new());
    }

    let app_name = app
        .and_then(|value| value.get("name"))
        .and_then(Value::as_str)
        .unwrap_or("limux");
    let app_pid = app
        .and_then(|value| value.get("pid"))
        .and_then(Value::as_u64)
        .unwrap_or(0);
    let app_rss = app
        .and_then(|value| value.get("resident_bytes"))
        .and_then(Value::as_u64)
        .unwrap_or(0);

    lines.push("APP".to_string());
    lines.push(format!("  {app_name} pid={app_pid}"));
    lines.push(format!("  rss       {}", format_bytes(app_rss)));
    lines.push(String::new());

    let child_rss = children
        .and_then(|value| value.get("recursive_rss_bytes"))
        .and_then(Value::as_u64)
        .unwrap_or(0);
    let child_count = children
        .and_then(|value| value.get("process_count"))
        .and_then(Value::as_u64)
        .unwrap_or(0);
    lines.push("CHILD PROCESSES".to_string());
    lines.push(format!(
        "  recursive RSS {} across {}",
        format_bytes(child_rss),
        process_count_text(child_count)
    ));

    let groups = children
        .and_then(|value| value.get("groups"))
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    if groups.is_empty() {
        lines.push("  no child process groups".to_string());
        return lines.join("\n");
    }

    lines.push(String::new());
    lines.push("TOP CHILD GROUPS".to_string());
    lines.push("      RSS  PROC  COMMAND                    ATTRIBUTION".to_string());
    for group in groups {
        let rss = format_bytes(group.get("rss_bytes").and_then(Value::as_u64).unwrap_or(0));
        let process_count = group
            .get("process_count")
            .and_then(Value::as_u64)
            .unwrap_or(0);
        let command = group
            .get("name")
            .and_then(Value::as_str)
            .unwrap_or("process");
        let attribution = memory_attribution_text(group.get("top_attribution"), id_format);
        lines.push(format!(
            "{rss:>9} {process_count:>5}  {command:<26} {attribution}"
        ));
    }
    lines.join("\n")
}

fn memory_attribution_text(raw: Option<&Value>, id_format: IdFormat) -> String {
    let Some(attribution) = raw.and_then(Value::as_object) else {
        return "unattributed".to_string();
    };
    let mut parts = Vec::new();
    for prefix in ["workspace", "pane", "surface"] {
        if let Some(handle) = memory_attribution_handle(attribution, prefix, id_format) {
            parts.push(format!("{prefix} {handle}"));
        }
    }
    if parts.is_empty() {
        "unattributed".to_string()
    } else {
        parts.join(" / ")
    }
}

fn memory_attribution_handle(
    attribution: &Map<String, Value>,
    prefix: &str,
    id_format: IdFormat,
) -> Option<String> {
    let id_key = format!("{prefix}_id");
    let ref_key = format!("{prefix}_ref");
    let id = attribution
        .get(&id_key)
        .and_then(Value::as_str)
        .unwrap_or("");
    let reference = attribution
        .get(&ref_key)
        .and_then(Value::as_str)
        .unwrap_or("");
    match id_format {
        IdFormat::Refs => (!reference.is_empty())
            .then(|| reference.to_string())
            .or_else(|| (!id.is_empty()).then(|| id.to_string())),
        IdFormat::Uuids => (!id.is_empty())
            .then(|| id.to_string())
            .or_else(|| (!reference.is_empty()).then(|| reference.to_string())),
        IdFormat::Both => {
            let values = [reference, id]
                .into_iter()
                .filter(|value| !value.is_empty())
                .collect::<Vec<_>>();
            (!values.is_empty()).then(|| values.join(" "))
        }
    }
}

fn format_bytes(bytes: u64) -> String {
    let units = ["B", "KiB", "MiB", "GiB"];
    let mut value = bytes as f64;
    let mut unit = 0usize;
    while value >= 1024.0 && unit + 1 < units.len() {
        value /= 1024.0;
        unit += 1;
    }
    if unit == 0 {
        format!("{bytes} {}", units[unit])
    } else {
        format!("{value:.1} {}", units[unit])
    }
}

fn process_count_text(count: u64) -> String {
    if count == 1 {
        "1 process".to_string()
    } else {
        format!("{count} processes")
    }
}

/// purpose: Render generic compatibility command output when no custom text view exists.
/// inputs: payload is the JSON-RPC result returned by the server.
/// returns/effects: Returns OK for empty payloads, plain strings as-is, or compact JSON text.
fn default_text_output(payload: &Value) -> String {
    match payload {
        Value::Null => "OK".to_string(),
        Value::String(value) if value.is_empty() => "OK".to_string(),
        Value::String(value) => value.clone(),
        Value::Object(map) if map.is_empty() => "OK".to_string(),
        Value::Array(values) if values.is_empty() => "OK".to_string(),
        _ => serde_json::to_string_pretty(payload).unwrap_or_else(|_| "OK".to_string()),
    }
}

async fn run_send(client: &mut Client, args: &[String]) -> Result<Value> {
    let workspace = parse_opt(args, "--workspace")
        .or_else(|| context_env_value("LIMUX_WORKSPACE_ID"))
        .filter(|s| !s.is_empty());
    let surface = parse_opt(args, "--surface")
        .or_else(|| parse_opt(args, "--surface-id"))
        .or_else(|| parse_opt(args, "--tab"))
        .or_else(|| parse_opt(args, "--tab-id"))
        .or_else(|| context_env_value("LIMUX_SURFACE_ID"))
        .or_else(|| context_env_value("CMUX_SURFACE_ID"))
        .filter(|s| !s.is_empty());

    let text = trailing_title(args).ok_or_else(|| anyhow!("send requires text"))?;

    let mut params = Map::new();
    params.insert("text".to_string(), Value::String(text));
    if let Some(surface) = surface {
        params.insert("surface_id".to_string(), Value::String(surface));
    }

    call_in_workspace_scope(
        client,
        workspace,
        "surface.send_text",
        Value::Object(params),
    )
    .await
}

async fn run_send_key(client: &mut Client, args: &[String]) -> Result<Value> {
    let workspace = parse_opt(args, "--workspace")
        .or_else(|| context_env_value("LIMUX_WORKSPACE_ID"))
        .filter(|s| !s.is_empty());
    let surface = parse_opt(args, "--surface")
        .or_else(|| context_env_value("LIMUX_SURFACE_ID"))
        .filter(|s| !s.is_empty());
    let key = trailing_title(args).ok_or_else(|| anyhow!("send-key requires key"))?;

    let mut params = Map::new();
    params.insert("key".to_string(), Value::String(key));
    if let Some(surface) = surface {
        params.insert("surface_id".to_string(), Value::String(surface));
    }

    call_in_workspace_scope(client, workspace, "surface.send_key", Value::Object(params)).await
}

/// `limux notify` — post a notification into the sidebar + toast overlay.
///
/// Usage:
///   limux notify [--workspace <id|ref>] [--subtitle <text>] [--body <text>] <title>
///   limux notify --title "..." --subtitle "..." --body "..."
///
/// Mirrors the `cmux notify` shape (title / subtitle / body). Title is
/// required; subtitle and body are optional. Falls back to the current
/// workspace via LIMUX_WORKSPACE_ID when --workspace isn't given.
async fn run_notify(client: &mut Client, args: &[String]) -> Result<Value> {
    let workspace = parse_opt(args, "--workspace")
        .or_else(|| context_env_value("LIMUX_WORKSPACE_ID"))
        .filter(|s| !s.is_empty());
    let surface = parse_opt(args, "--surface")
        .or_else(|| parse_opt(args, "--surface-id"))
        .or_else(|| parse_opt(args, "--tab"))
        .or_else(|| parse_opt(args, "--tab-id"))
        .or_else(|| context_env_value("LIMUX_SURFACE_ID"))
        .or_else(|| context_env_value("CMUX_SURFACE_ID"))
        .filter(|s| !s.is_empty());

    // Title can be provided either via --title or as the trailing positional
    // (matching `limux send`'s ergonomics).
    let title = parse_opt(args, "--title")
        .or_else(|| trailing_title(args))
        .ok_or_else(|| anyhow!("notify requires a title"))?;

    let subtitle = parse_opt(args, "--subtitle").unwrap_or_default();
    let body = parse_opt(args, "--body")
        .or_else(|| parse_opt(args, "--message"))
        .unwrap_or_default();

    let mut params = Map::new();
    params.insert("title".to_string(), Value::String(title));
    if !subtitle.is_empty() {
        params.insert("subtitle".to_string(), Value::String(subtitle));
    }
    if !body.is_empty() {
        params.insert("body".to_string(), Value::String(body));
    }
    if let Some(surface) = surface {
        params.insert("surface_id".to_string(), Value::String(surface));
    }

    call_in_workspace_scope(
        client,
        workspace,
        "notification.create",
        Value::Object(params),
    )
    .await
}

// ---------------------------------------------------------------------------
// Agent hooks (claude-hook / opencode-hook / gemini-hook)
// ---------------------------------------------------------------------------
//
// These subcommands read a JSON hook event from stdin and translate it into
// a `notify` (and, eventually, log / progress) call so the GUI reflects
// agent activity in real time. Designed for direct wiring into Claude Code,
// OpenCode, and Gemini CLI's hook settings.
//
// Claude Code stdin schema (what we rely on):
//   {
//     "session_id": "...",
//     "transcript_path": "...",
//     "cwd": "...",
//     "hook_event_name": "Notification" | "Stop" | "SessionStart" | ...,
//     "message": "agent is waiting for input",     // Notification only
//     "tool_name": "...", "tool_input": {...},     // PreToolUse/PostToolUse
//     "tool_response": {...},                       // PostToolUse
//     "prompt": "..."                               // UserPromptSubmit
//   }
//
// OpenCode and Gemini use slightly different names; we fall back gracefully
// when fields are missing.

/// Pull a string field from the hook JSON, trying multiple keys.
fn hook_str<'a>(payload: &'a Value, keys: &[&str]) -> Option<&'a str> {
    keys.iter()
        .find_map(|k| payload.get(*k).and_then(Value::as_str))
        .map(str::trim)
        .filter(|s| !s.is_empty())
}

fn parse_hook_event(args: &[String], payload: &Value) -> String {
    parse_opt(args, "--event")
        .or_else(|| trailing_title(args))
        .or_else(|| hook_str(payload, &["hook_event_name", "event"]).map(str::to_owned))
        .unwrap_or_else(|| "event".to_string())
}

fn parse_feed_hook_event(args: &[String], payload: &Value) -> String {
    parse_opt(args, "--event")
        .or_else(|| feed_hook_positional_event(args))
        .or_else(|| hook_str(payload, &["hook_event_name", "event"]).map(str::to_owned))
        .unwrap_or_else(|| "event".to_string())
}

fn feed_hook_positional_event(args: &[String]) -> Option<String> {
    let mut skip_next = false;
    for arg in args {
        if skip_next {
            skip_next = false;
            continue;
        }
        if matches!(
            arg.as_str(),
            "--source" | "--workspace" | "--surface" | "--event"
        ) {
            skip_next = true;
            continue;
        }
        if arg.starts_with('-') {
            continue;
        }
        return Some(arg.clone());
    }
    None
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum FeedEventSemantic {
    ApprovalRequest,
    ToolStart,
    ToolStartMaybeApproval,
    ToolEnd,
    PreCompact,
    PostCompact,
    PromptSubmit,
    SubagentStart,
    Response,
    SubagentResponse,
    SessionStart,
    SessionEnd,
    StatusNotification,
    Unknown,
}

// purpose: Classify raw agent hook events into CMUX Feed wire events.
// inputs: Source agent name, raw hook event name, and optional tool name.
// returns/effects: Returns hook_event_name plus whether feed.push should wait for a user decision.
fn classify_feed_event(source: &str, event: &str, tool_name: &str) -> (String, bool) {
    let semantic = feed_event_semantic(source, event);
    let (name, actionable) = match semantic {
        FeedEventSemantic::ApprovalRequest => {
            dedicated_feed_approval(tool_name).unwrap_or(("PermissionRequest", true))
        }
        FeedEventSemantic::ToolStartMaybeApproval => map_maybe_approval_tool(source, tool_name),
        FeedEventSemantic::ToolStart => ("PreToolUse", false),
        FeedEventSemantic::ToolEnd => ("PostToolUse", false),
        FeedEventSemantic::PreCompact => ("PreCompact", false),
        FeedEventSemantic::PostCompact => ("PostCompact", false),
        FeedEventSemantic::PromptSubmit => ("UserPromptSubmit", false),
        FeedEventSemantic::SubagentStart => ("SubagentStart", false),
        FeedEventSemantic::Response => ("Stop", false),
        FeedEventSemantic::SubagentResponse => ("SubagentStop", false),
        FeedEventSemantic::SessionStart => ("SessionStart", false),
        FeedEventSemantic::SessionEnd => ("SessionEnd", false),
        FeedEventSemantic::StatusNotification => ("Notification", false),
        FeedEventSemantic::Unknown => ("PreToolUse", false),
    };
    (name.to_string(), actionable)
}

fn map_maybe_approval_tool(source: &str, tool_name: &str) -> (&'static str, bool) {
    if let Some(dedicated) = dedicated_feed_approval(tool_name) {
        return dedicated;
    }
    if is_side_effecting_feed_tool(source, tool_name) {
        ("PermissionRequest", true)
    } else {
        ("PreToolUse", false)
    }
}

// purpose: Resolve a source-specific hook event into its user-attention semantic.
// inputs: Agent source and raw event string from hook stdin or --event.
// returns/effects: Returns explicit telemetry/approval semantics with unknown events non-actionable.
fn feed_event_semantic(source: &str, event: &str) -> FeedEventSemantic {
    match source {
        "claude" => claude_feed_event_semantic(event),
        "codex" => codex_feed_event_semantic(event),
        "hermes-agent" => hermes_feed_event_semantic(event),
        "kiro" => kiro_feed_event_semantic(event),
        _ => generic_feed_event_semantic(event),
    }
}

fn claude_feed_event_semantic(event: &str) -> FeedEventSemantic {
    match event {
        "PermissionRequest" => FeedEventSemantic::ApprovalRequest,
        "PreToolUse" => FeedEventSemantic::ToolStart,
        other => generic_feed_event_semantic(other),
    }
}

fn codex_feed_event_semantic(event: &str) -> FeedEventSemantic {
    match event {
        "PermissionRequest"
        | "permission_request"
        | "PreToolUse"
        | "pre_tool_use"
        | "beforeShellExecution" => FeedEventSemantic::ToolStart,
        "PostToolUse" | "post_tool_use" => FeedEventSemantic::ToolEnd,
        "PreCompact" | "pre_compact" => FeedEventSemantic::PreCompact,
        "PostCompact" | "post_compact" => FeedEventSemantic::PostCompact,
        "UserPromptSubmit" | "user_prompt_submit" => FeedEventSemantic::PromptSubmit,
        "SessionStart" | "session_start" => FeedEventSemantic::SessionStart,
        "SessionEnd" | "session_end" => FeedEventSemantic::SessionEnd,
        "Stop" | "stop" => FeedEventSemantic::Response,
        "SubagentStart" | "subagent_start" => FeedEventSemantic::SubagentStart,
        "SubagentStop" | "subagent_stop" => FeedEventSemantic::SubagentResponse,
        "Notification" | "notification" => FeedEventSemantic::StatusNotification,
        _ => FeedEventSemantic::Unknown,
    }
}

fn hermes_feed_event_semantic(event: &str) -> FeedEventSemantic {
    match event {
        "pre_tool_call" => FeedEventSemantic::ToolStart,
        "post_tool_call" => FeedEventSemantic::ToolEnd,
        "pre_approval_request" | "post_approval_response" => FeedEventSemantic::StatusNotification,
        "pre_llm_call" => FeedEventSemantic::PromptSubmit,
        "post_llm_call" => FeedEventSemantic::Response,
        "on_session_start" | "on_session_reset" => FeedEventSemantic::SessionStart,
        "on_session_end" | "on_session_finalize" => FeedEventSemantic::SessionEnd,
        _ => FeedEventSemantic::Unknown,
    }
}

fn kiro_feed_event_semantic(event: &str) -> FeedEventSemantic {
    match event {
        "preToolUse" => FeedEventSemantic::ToolStartMaybeApproval,
        "postToolUse" => FeedEventSemantic::ToolEnd,
        "userPromptSubmit" => FeedEventSemantic::PromptSubmit,
        "agentSpawn" => FeedEventSemantic::SessionStart,
        "stop" => FeedEventSemantic::Response,
        _ => FeedEventSemantic::Unknown,
    }
}

// purpose: Classify hooks for agents without source-specific Feed tables.
// inputs: Raw event string from generic agent hook integrations.
// returns/effects: Returns the CMUX generic semantic, escalating side-effecting pre-tool hooks later.
fn generic_feed_event_semantic(event: &str) -> FeedEventSemantic {
    match event {
        "PreToolUse" | "beforeShellExecution" => FeedEventSemantic::ToolStartMaybeApproval,
        "PermissionRequest" => FeedEventSemantic::ApprovalRequest,
        "PostToolUse" => FeedEventSemantic::ToolEnd,
        "PreCompact" => FeedEventSemantic::PreCompact,
        "PostCompact" => FeedEventSemantic::PostCompact,
        "UserPromptSubmit" => FeedEventSemantic::PromptSubmit,
        "SessionStart" => FeedEventSemantic::SessionStart,
        "SessionEnd" => FeedEventSemantic::SessionEnd,
        "Stop" => FeedEventSemantic::Response,
        "SubagentStart" => FeedEventSemantic::SubagentStart,
        "SubagentStop" => FeedEventSemantic::SubagentResponse,
        "Notification" => FeedEventSemantic::StatusNotification,
        _ => FeedEventSemantic::Unknown,
    }
}

fn dedicated_feed_approval(tool_name: &str) -> Option<(&'static str, bool)> {
    match tool_name {
        "ExitPlanMode" => Some(("ExitPlanMode", true)),
        "AskUserQuestion" => Some(("AskUserQuestion", true)),
        _ => None,
    }
}

// purpose: Decide whether a pre-tool event should become a Feed permission card.
// inputs: Agent source and raw tool name.
// returns/effects: Returns true only for tools CMUX treats as state-mutating.
fn is_side_effecting_feed_tool(source: &str, tool_name: &str) -> bool {
    let normalized = tool_name.to_ascii_lowercase();
    let canonical = [
        "bash",
        "write",
        "edit",
        "multiedit",
        "notebookedit",
        "apply_patch",
        "shell",
        "terminal",
        "run_command",
        "write_to_file",
        "replace_file_content",
        "multi_replace_file_content",
        "manage_task",
        "schedule",
        "ask_permission",
        "invoke_subagent",
        "define_subagent",
        "manage_subagents",
        "generate_image",
    ];
    canonical.contains(&normalized.as_str())
        || source == "kiro" && is_kiro_side_effecting_tool(&normalized)
}

fn is_kiro_side_effecting_tool(normalized_tool_name: &str) -> bool {
    matches!(
        normalized_tool_name,
        "execute_bash"
            | "fs_write"
            | "use_aws"
            | "bash"
            | "write"
            | "edit"
            | "multiedit"
            | "apply_patch"
            | "shell"
    )
}

fn feed_tool_name(payload: &Value) -> Option<String> {
    hook_str(payload, &["tool_name", "toolName"])
        .map(str::to_string)
        .or_else(|| {
            payload
                .get("toolCall")
                .and_then(|value| value.get("name"))
                .and_then(Value::as_str)
                .map(str::to_string)
        })
}

fn feed_tool_input(payload: &Value, hook_event_name: &str) -> Option<Value> {
    if hook_event_name == "PostToolUse" {
        if let Some(value) = first_payload_value(
            payload,
            &["tool_response", "toolResponse", "tool_result", "toolResult"],
        ) {
            return Some(value);
        }
    }
    first_payload_value(payload, &["tool_input", "toolInput"]).or_else(|| {
        payload
            .get("toolCall")
            .and_then(|value| value.get("args"))
            .cloned()
    })
}

fn first_payload_value(payload: &Value, keys: &[&str]) -> Option<Value> {
    keys.iter().find_map(|key| payload.get(*key).cloned())
}

fn feed_session_id(source: &str, payload: &Value) -> String {
    let session = hook_session_id(payload)
        .or_else(|| hook_str(payload, &["conversation_id", "conversationId"]).map(str::to_string))
        .unwrap_or_else(|| format!("pid-{}", std::process::id()));
    if session.starts_with(&format!("{source}-")) {
        session
    } else {
        format!("{source}-{session}")
    }
}

// purpose: Build the CMUX-shaped feed.push request body from hook stdin.
// inputs: Hook CLI args and decoded agent hook JSON.
// returns/effects: Returns params, actionable flag, event name, tool name, and tool input.
fn build_feed_hook_push(
    args: &[String],
    payload: &Value,
) -> Result<(Value, bool, String, String, Option<Value>)> {
    let source = parse_opt(args, "--source")
        .ok_or_else(|| anyhow!("limux hooks feed requires --source <agent-name>"))?;
    let raw_event = parse_feed_hook_event(args, payload);
    let tool_name = feed_tool_name(payload).unwrap_or_default();
    let (hook_event_name, actionable) = classify_feed_event(&source, &raw_event, &tool_name);
    let tool_input = feed_tool_input(payload, &hook_event_name);
    let mut event = payload.as_object().cloned().unwrap_or_default();

    event.insert(
        "session_id".to_string(),
        Value::String(feed_session_id(&source, payload)),
    );
    event.insert(
        "hook_event_name".to_string(),
        Value::String(hook_event_name.clone()),
    );
    event.insert("_source".to_string(), Value::String(source.clone()));
    if !tool_name.is_empty() {
        event.insert("tool_name".to_string(), Value::String(tool_name.clone()));
    }
    if let Some(input) = tool_input.clone() {
        event.insert("tool_input".to_string(), input);
    }
    enrich_feed_hook_context(args, payload, &source, &mut event);
    ensure_feed_request_id(&source, &raw_event, &tool_name, payload, &mut event);

    Ok((
        json!({
            "event": Value::Object(event),
            "wait_timeout_seconds": if actionable { 120.0 } else { 0.0 },
        }),
        actionable,
        hook_event_name,
        tool_name,
        tool_input,
    ))
}

// purpose: Add workspace, surface, cwd, and process context to Feed hook events.
// inputs: Hook args, raw payload, agent source, and mutable event object.
// returns/effects: Mutates event with available non-empty context fields.
fn enrich_feed_hook_context(
    args: &[String],
    payload: &Value,
    source: &str,
    event: &mut Map<String, Value>,
) {
    if let Some(workspace) =
        parse_opt(args, "--workspace").or_else(|| context_env_value("LIMUX_WORKSPACE_ID"))
    {
        event.insert("workspace_id".to_string(), Value::String(workspace));
    }
    if let Some(surface) =
        parse_opt(args, "--surface").or_else(|| context_env_value("LIMUX_SURFACE_ID"))
    {
        event.insert("surface_id".to_string(), Value::String(surface));
    }
    if let Some(cwd) = hook_str(payload, &["cwd", "working_directory", "workingDirectory"]) {
        event.insert("cwd".to_string(), Value::String(cwd.to_string()));
    }
    if let Some(agent) = agent_hooks::AgentKind::from_hook_name(source) {
        if let Some(pid) = agent_ancestor_pid(agent) {
            event.insert("_ppid".to_string(), json!(pid));
        }
    }
}

// purpose: Ensure feed.push can correlate later feed.*.reply calls.
// inputs: Source, raw event name, tool name, payload, and mutable event object.
// returns/effects: Preserves supplied request ids or inserts a generated non-empty id.
fn ensure_feed_request_id(
    source: &str,
    raw_event: &str,
    tool_name: &str,
    payload: &Value,
    event: &mut Map<String, Value>,
) {
    let request_id = hook_str(
        payload,
        &[
            "_opencode_request_id",
            "request_id",
            "tool_use_id",
            "toolUseID",
        ],
    )
    .map(str::to_string)
    .unwrap_or_else(|| {
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|duration| duration.as_millis())
            .unwrap_or(0);
        format!("{source}-{raw_event}-{tool_name}-{now}")
    });
    event.insert(
        "_opencode_request_id".to_string(),
        Value::String(request_id),
    );
}

// purpose: Run CMUX-compatible `limux hooks feed --source <agent>`.
// inputs: Hook CLI args, stdin hook JSON, socket client, and JSON-output flag.
// returns/effects: Pushes to feed.push and emits agent-native decision JSON when resolved.
async fn run_feed_hook(
    client: &mut Client,
    args: &[String],
    json_output: bool,
) -> Result<CommandOutput> {
    use std::io::Read;

    let mut raw = String::new();
    std::io::stdin()
        .read_to_string(&mut raw)
        .context("failed to read hook JSON from stdin")?;
    let payload: Value = if raw.trim().is_empty() {
        Value::Object(Map::new())
    } else {
        serde_json::from_str(raw.trim()).context("hook stdin was not valid JSON")?
    };
    let (params, _actionable, hook_event_name, tool_name, tool_input) =
        build_feed_hook_push(args, &payload)?;
    let result = client.call("feed.push", params).await?;
    if json_output {
        return Ok(CommandOutput::Json(result));
    }
    let decision = result.get("decision").cloned();
    let source = parse_opt(args, "--source")
        .ok_or_else(|| anyhow!("limux hooks feed requires --source <agent-name>"))?;
    if source == "kiro" {
        if let Some(output) = decision
            .as_ref()
            .and_then(render_kiro_permission_decision_output)
        {
            return Ok(output);
        }
    }
    let output = decision
        .as_ref()
        .map(|value| render_feed_decision(args, tool_input.as_ref(), &payload, value))
        .transpose()?
        .unwrap_or_else(|| "{}".to_string());
    let _ = hook_event_name;
    let _ = tool_name;
    Ok(CommandOutput::Text(output))
}

// purpose: Enforce Kiro's preToolUse permission contract from resolved Feed decisions.
// inputs: Feed decision JSON returned by feed.push.
// returns/effects: Returns stdout/exit behavior; deny and malformed modes fail closed with exit 2.
fn render_kiro_permission_decision_output(decision: &Value) -> Option<CommandOutput> {
    if hook_str(decision, &["kind"]) != Some("permission") {
        return None;
    }
    let mode = hook_str(decision, &["mode"])
        .map(|value| value.trim().to_ascii_lowercase())
        .unwrap_or_default();
    if matches!(mode.as_str(), "once" | "always" | "all" | "bypass") {
        return Some(CommandOutput::Text("{}".to_string()));
    }
    let stderr = if mode == "deny" {
        "User denied permission via Limux Feed."
    } else {
        "Limux Feed returned an unrecognized Kiro permission decision; denying for safety."
    };
    Some(CommandOutput::Exit {
        stdout: String::new(),
        stderr: stderr.to_string(),
        status: 2,
    })
}

// purpose: Convert resolved Feed decisions into the source agent's hook stdout JSON.
// inputs: Hook args, optional tool input, raw hook payload, and feed decision object.
// returns/effects: Returns compact JSON text for stdout; unknown decisions emit an empty object.
fn render_feed_decision(
    args: &[String],
    tool_input: Option<&Value>,
    raw_payload: &Value,
    decision: &Value,
) -> Result<String> {
    let source = parse_opt(args, "--source")
        .ok_or_else(|| anyhow!("limux hooks feed requires --source <agent-name>"))?;
    let kind = hook_str(decision, &["kind"]).unwrap_or("");
    let rendered = match kind {
        "permission" => render_feed_permission_decision(&source, raw_payload, decision),
        "exit_plan" => render_feed_exit_plan_decision(&source, tool_input, decision),
        "question" => render_feed_question_decision(&source, tool_input, decision),
        _ => json!({}),
    };
    serde_json::to_string(&rendered).context("failed to encode feed decision")
}

// purpose: Render permission allow/deny decisions for Claude, Codex, and generic hooks.
// inputs: Agent source, raw hook payload, and Feed permission decision.
// returns/effects: Produces agent-native hookSpecificOutput JSON.
fn render_feed_permission_decision(source: &str, raw_payload: &Value, decision: &Value) -> Value {
    let mode = hook_str(decision, &["mode"]).unwrap_or("deny");
    if matches!(source, "claude" | "codex") {
        return render_claude_like_permission_decision(mode, raw_payload);
    }
    if source == "hermes-agent" {
        return render_hermes_permission_decision(mode);
    }
    if source == "antigravity" {
        return render_antigravity_permission_decision(mode);
    }
    render_generic_permission_decision(mode)
}

fn render_claude_like_permission_decision(mode: &str, raw_payload: &Value) -> Value {
    if mode == "deny" {
        return permission_request_hook_decision(
            "deny",
            Some("User denied permission via Limux Feed."),
            None,
            None,
        );
    }
    let permissions = if mode == "always" || mode == "all" {
        raw_payload
            .get("permission_suggestions")
            .and_then(Value::as_array)
            .cloned()
    } else {
        None
    };
    permission_request_hook_decision("allow", None, None, permissions)
}

fn render_hermes_permission_decision(mode: &str) -> Value {
    if mode == "deny" {
        json!({ "action": "block", "message": "User denied permission via Limux Feed." })
    } else {
        json!({})
    }
}

fn render_antigravity_permission_decision(mode: &str) -> Value {
    let reason = if mode == "deny" {
        "User denied permission via Limux Feed."
    } else {
        "User approved via Limux Feed."
    };
    json!({ "decision": if mode == "deny" { "deny" } else { "allow" }, "reason": reason })
}

fn render_generic_permission_decision(mode: &str) -> Value {
    if mode == "deny" {
        return non_claude_pre_tool_decision(
            "deny",
            "User denied permission via Limux Feed.",
            None,
            None,
        );
    }
    let reason = generic_permission_allow_reason(mode);
    non_claude_pre_tool_decision("allow", &reason, None, None)
}

fn generic_permission_allow_reason(mode: &str) -> String {
    if matches!(mode, "always" | "all" | "bypass") {
        return format!(
            "User granted {mode} permission via Limux Feed. Reduce subsequent approval prompts for similar calls."
        );
    }
    "User approved via Limux Feed.".to_string()
}

// purpose: Render ExitPlanMode decisions in agent-native hook stdout shape.
// inputs: Agent source, original tool input, and Feed exit-plan decision.
// returns/effects: Produces allow/deny or context JSON matching CMUX semantics.
fn render_feed_exit_plan_decision(
    source: &str,
    tool_input: Option<&Value>,
    decision: &Value,
) -> Value {
    let mode = hook_str(decision, &["mode"]).unwrap_or("manual");
    let feedback = hook_str(decision, &["feedback"]);
    if source == "claude" {
        return render_claude_exit_plan_decision(mode, feedback, tool_input);
    }
    if source == "hermes-agent" {
        if let Some(feedback) = feedback {
            return json!({ "action": "block", "message": format!("User rejected the plan via Limux Feed and wants this change: {feedback}") });
        }
        return if mode == "deny" {
            json!({ "action": "block", "message": "User rejected the plan via Limux Feed." })
        } else {
            json!({})
        };
    }
    let context = generic_exit_plan_context(mode, feedback);
    non_claude_pre_tool_decision("deny", &context, Some(&context), None)
}

// purpose: Render Claude-specific ExitPlanMode updates.
// inputs: Feed mode, optional feedback, and original tool input.
// returns/effects: Produces PermissionRequest hookSpecificOutput for Claude.
fn render_claude_exit_plan_decision(
    mode: &str,
    feedback: Option<&str>,
    tool_input: Option<&Value>,
) -> Value {
    if let Some(feedback) = feedback {
        return claude_exit_plan_deny(&format!(
            "User rejected the plan via Limux Feed and wants this change: {feedback}"
        ));
    }
    if mode == "deny" {
        return claude_exit_plan_deny("User rejected the plan via Limux Feed.");
    }
    if mode == "ultraplan" {
        return claude_exit_plan_deny(
            "User chose Ultraplan via Limux Feed. Refine this plan with Ultraplan on Claude Code on the web.",
        );
    }
    permission_request_hook_decision(
        "allow",
        None,
        tool_input.and_then(json_dictionary),
        claude_exit_plan_permissions(mode),
    )
}

fn claude_exit_plan_deny(message: &str) -> Value {
    permission_request_hook_decision("deny", Some(message), None, None)
}

fn claude_exit_plan_permissions(mode: &str) -> Option<Vec<Value>> {
    if mode == "autoAccept" {
        Some(vec![
            json!({ "type": "setMode", "mode": "auto", "destination": "session" }),
        ])
    } else {
        None
    }
}

fn generic_exit_plan_context(mode: &str, feedback: Option<&str>) -> String {
    if let Some(feedback) = feedback {
        return format!("User rejected the plan via Limux Feed and wants this change: {feedback}");
    }
    match mode {
        "deny" => "User rejected the plan via Limux Feed.".to_string(),
        "ultraplan" => "User chose Ultraplan via Limux Feed. Refine this plan with Ultraplan if available.".to_string(),
        "bypassPermissions" => {
            "User accepted this plan via Limux Feed with bypass-permissions mode. Exit plan mode now and proceed.".to_string()
        }
        "autoAccept" => "User accepted this plan via Limux Feed with auto mode. Exit plan mode now and proceed.".to_string(),
        _ => "User accepted this plan via Limux Feed with manual-approval mode. Exit plan mode now and proceed.".to_string(),
    }
}

// purpose: Render AskUserQuestion decisions in agent-native hook stdout shape.
// inputs: Agent source, original tool input, and Feed question decision.
// returns/effects: Produces updated Claude input or generic context response.
fn render_feed_question_decision(
    source: &str,
    tool_input: Option<&Value>,
    decision: &Value,
) -> Value {
    let selections = decision
        .get("selections")
        .and_then(Value::as_array)
        .map(|values| {
            values
                .iter()
                .filter_map(Value::as_str)
                .map(str::to_string)
                .collect::<Vec<_>>()
        })
        .unwrap_or_default();
    if source == "hermes-agent" {
        return json!({ "context": question_answer_body(&selections) });
    }
    if source == "claude" {
        return permission_request_hook_decision(
            "allow",
            None,
            Some(claude_question_input(tool_input, &selections)),
            None,
        );
    }
    let body = question_answer_body(&selections);
    let context = format!(
        "[Limux Feed] {body}. Treat this as the user's response; do not ask again for the same question."
    );
    non_claude_pre_tool_decision("deny", &context, Some(&context), None)
}

fn question_answer_body(selections: &[String]) -> String {
    match selections {
        [] => "The user submitted an empty answer.".to_string(),
        [one] => format!("The user answered: {one}"),
        many => format!("The user answered: {}", many.join(", ")),
    }
}

fn permission_request_hook_decision(
    behavior: &str,
    message: Option<&str>,
    updated_input: Option<Map<String, Value>>,
    updated_permissions: Option<Vec<Value>>,
) -> Value {
    let mut inner = Map::new();
    inner.insert("behavior".to_string(), Value::String(behavior.to_string()));
    if behavior == "deny" {
        inner.insert(
            "message".to_string(),
            Value::String(
                message
                    .unwrap_or("User denied permission via Limux Feed.")
                    .to_string(),
            ),
        );
    }
    if let Some(updated_input) = updated_input.filter(|value| !value.is_empty()) {
        inner.insert("updatedInput".to_string(), Value::Object(updated_input));
    }
    if let Some(updated_permissions) = updated_permissions.filter(|value| !value.is_empty()) {
        inner.insert(
            "updatedPermissions".to_string(),
            Value::Array(updated_permissions),
        );
    }
    json!({ "hookSpecificOutput": { "hookEventName": "PermissionRequest", "decision": Value::Object(inner) } })
}

fn non_claude_pre_tool_decision(
    permission: &str,
    reason: &str,
    additional_context: Option<&str>,
    updated_input: Option<Value>,
) -> Value {
    let mut specific = json!({ "hookEventName": "PreToolUse", "permissionDecision": permission });
    specific["permissionDecisionReason"] = Value::String(reason.to_string());
    if let Some(context) = additional_context {
        specific["additionalContext"] = Value::String(context.to_string());
    }
    if let Some(input) = updated_input {
        specific["updatedInput"] = input;
    }
    let mut out = json!({ "hookSpecificOutput": specific });
    out["decision"] = Value::String(
        if permission == "deny" {
            "block"
        } else {
            "approve"
        }
        .to_string(),
    );
    if permission == "deny" {
        out["reason"] = Value::String(reason.to_string());
    } else {
        out["systemMessage"] = Value::String(additional_context.unwrap_or(reason).to_string());
    }
    out
}

fn json_dictionary(value: &Value) -> Option<Map<String, Value>> {
    if let Some(object) = value.as_object() {
        return Some(object.clone());
    }
    value
        .as_str()
        .and_then(|raw| serde_json::from_str::<Value>(raw).ok())
        .and_then(|parsed| parsed.as_object().cloned())
}

fn claude_question_input(tool_input: Option<&Value>, selections: &[String]) -> Map<String, Value> {
    let mut input = tool_input.and_then(json_dictionary).unwrap_or_default();
    let questions = input
        .get("questions")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    let answers = selections
        .iter()
        .enumerate()
        .map(|(index, selection)| {
            let key = questions
                .get(index)
                .and_then(|question| hook_str(question, &["question"]))
                .map(str::to_string)
                .unwrap_or_else(|| format!("Answer {}", index + 1));
            (key, Value::String(selection.clone()))
        })
        .collect::<Map<_, _>>();
    input.insert("answers".to_string(), Value::Object(answers));
    input
}

/// Run an agent hook: read JSON from stdin, synthesize a notification.
///
/// Args:
///   [event_name] — optional positional, e.g. "Notification", "Stop".
///                  If omitted, we read `hook_event_name` from the JSON.
async fn run_agent_hook(
    client: &mut Client,
    agent: agent_hooks::AgentKind,
    args: &[String],
) -> Result<Value> {
    use std::io::Read;

    // Read stdin (hook JSON). If stdin is empty or not JSON, treat as
    // minimal event so we still post *something*.
    let mut raw = String::new();
    let _ = std::io::stdin().read_to_string(&mut raw);
    let raw = raw.trim();
    let payload: Value = if raw.is_empty() {
        Value::Object(Map::new())
    } else {
        serde_json::from_str(raw).unwrap_or_else(|_| json!({ "raw": raw }))
    };

    // Explicit --event or positional event beats the JSON field.
    let event = parse_hook_event(args, &payload);

    // Build a human-friendly title + body depending on event + agent.
    let agent_label = agent.label();
    persist_agent_hook_session(agent, args, &payload, &event)?;
    let (title, body) = match event.as_str() {
        "Notification" => (
            format!("{agent_label} needs you"),
            hook_str(&payload, &["message", "notification"])
                .unwrap_or("waiting for input")
                .to_owned(),
        ),
        "Stop" | "SubagentStop" => (
            format!("{agent_label} finished"),
            hook_str(&payload, &["message", "reason"])
                .unwrap_or("task complete")
                .to_owned(),
        ),
        "SessionStart" => (
            format!("{agent_label} session started"),
            hook_str(&payload, &["cwd", "source"])
                .unwrap_or("")
                .to_owned(),
        ),
        "SessionEnd" => (
            format!("{agent_label} session ended"),
            hook_str(&payload, &["reason"]).unwrap_or("").to_owned(),
        ),
        "PreToolUse" | "PostToolUse" => (
            format!(
                "{agent_label}: {}",
                hook_str(&payload, &["tool_name"]).unwrap_or("tool")
            ),
            hook_str(&payload, &["tool_input", "summary"])
                .unwrap_or("")
                .to_owned(),
        ),
        "UserPromptSubmit" => (
            format!("{agent_label}: new prompt"),
            hook_str(&payload, &["prompt"])
                .unwrap_or("")
                .chars()
                .take(120)
                .collect(),
        ),
        other => (
            format!("{agent_label}: {other}"),
            hook_str(&payload, &["message", "summary"])
                .unwrap_or("")
                .to_owned(),
        ),
    };

    let subtitle = hook_str(&payload, &["session_id"])
        .map(|s| {
            // Show only a short prefix of the session id to keep sidebar tidy.
            s.chars().take(8).collect::<String>()
        })
        .unwrap_or_default();

    let workspace = parse_opt(args, "--workspace")
        .or_else(|| context_env_value("LIMUX_WORKSPACE_ID"))
        .filter(|s| !s.is_empty());

    let mut params = Map::new();
    params.insert("title".to_string(), Value::String(title));
    if !subtitle.is_empty() {
        params.insert("subtitle".to_string(), Value::String(subtitle));
    }
    if !body.is_empty() {
        params.insert("body".to_string(), Value::String(body));
    }

    let _ = call_in_workspace_scope(
        client,
        workspace,
        "notification.create",
        Value::Object(params),
    )
    .await;

    Ok(agent_hook_output(&event, &payload))
}

fn agent_hook_output(event: &str, payload: &Value) -> Value {
    let canonical_event = canonical_hook_event_name(event);
    let mut output = Map::new();
    output.insert("continue".to_string(), Value::Bool(true));
    output.insert("suppressOutput".to_string(), Value::Bool(false));

    if matches!(canonical_event, Some("SessionStart" | "UserPromptSubmit")) {
        let mut specific = Map::new();
        specific.insert(
            "hookEventName".to_string(),
            Value::String(
                canonical_event
                    .expect("matched canonical event")
                    .to_string(),
            ),
        );
        if let Some(context) = hook_additional_context(payload) {
            specific.insert("additionalContext".to_string(), Value::String(context));
        }
        output.insert("hookSpecificOutput".to_string(), Value::Object(specific));
    }

    Value::Object(output)
}

fn canonical_hook_event_name(event: &str) -> Option<&'static str> {
    match event {
        "SessionStart" | "session-start" => Some("SessionStart"),
        "UserPromptSubmit" | "prompt-submit" => Some("UserPromptSubmit"),
        "Stop" | "stop" | "Notification" => Some("Stop"),
        "SessionEnd" | "session-end" => None,
        "Cleanup" | "cleanup" | "restore-exit" => None,
        _ => None,
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum AgentHookPersistenceAction {
    Upsert,
    Preserve,
    Remove,
}

fn agent_hook_persistence_action(event: &str) -> AgentHookPersistenceAction {
    match event {
        "Cleanup" | "cleanup" | "restore-exit" | "session-finalize" => {
            AgentHookPersistenceAction::Remove
        }
        "SessionEnd" | "session-end" => AgentHookPersistenceAction::Preserve,
        _ => AgentHookPersistenceAction::Upsert,
    }
}

fn hook_additional_context(payload: &Value) -> Option<String> {
    hook_str(payload, &["additional_context", "additionalContext"])
        .map(str::to_owned)
        .filter(|value| !value.trim().is_empty())
}

fn persist_agent_hook_session(
    agent: agent_hooks::AgentKind,
    args: &[String],
    payload: &Value,
    event: &str,
) -> Result<()> {
    let Some(session_id) = hook_session_id(payload) else {
        write_agent_hook_debug(
            agent,
            event,
            "skip_missing_session_id",
            &json!({
                "payload_keys": payload_keys(payload),
                "has_claude_code_session_env": limux_env_value("CLAUDE_CODE_SESSION_ID").is_some(),
                "has_claude_session_env": limux_env_value("CLAUDE_SESSION_ID").is_some(),
            }),
        );
        return Ok(());
    };

    let store = agent_hooks::AgentHookSessionStore::new(agent);
    match agent_hook_persistence_action(event) {
        AgentHookPersistenceAction::Remove => {
            let result = store.remove(&session_id);
            if result.is_ok() {
                write_agent_hook_debug(
                    agent,
                    event,
                    "removed",
                    &json!({
                        "session_id": session_id,
                        "payload_keys": payload_keys(payload),
                    }),
                );
            }
            return result;
        }
        AgentHookPersistenceAction::Preserve => {
            write_agent_hook_debug(
                agent,
                event,
                "preserved",
                &json!({
                    "session_id": session_id,
                    "payload_keys": payload_keys(payload),
                }),
            );
            return Ok(());
        }
        AgentHookPersistenceAction::Upsert => {}
    }

    let workspace_id = parse_opt(args, "--workspace")
        .or_else(|| context_env_value("LIMUX_WORKSPACE_ID"))
        .filter(|value| !value.trim().is_empty());
    let surface_id = parse_opt(args, "--surface")
        .or_else(|| context_env_value("LIMUX_SURFACE_ID"))
        .filter(|value| !value.trim().is_empty());
    let (Some(workspace_id), Some(surface_id)) = (workspace_id, surface_id) else {
        write_agent_hook_debug(
            agent,
            event,
            "skip_missing_limux_target",
            &json!({
                "session_id": session_id,
                "has_workspace_arg": parse_opt(args, "--workspace").is_some(),
                "has_surface_arg": parse_opt(args, "--surface").is_some(),
                "has_workspace_env": context_env_value("LIMUX_WORKSPACE_ID").is_some(),
                "has_surface_env": context_env_value("LIMUX_SURFACE_ID").is_some(),
                "payload_keys": payload_keys(payload),
            }),
        );
        return Ok(());
    };

    let existing = store.lookup(&session_id)?;
    let cwd = hook_str(payload, &["cwd", "working_directory", "directory"])
        .map(str::to_string)
        .or_else(|| existing.as_ref().and_then(|record| record.cwd.clone()));
    let pid =
        hook_agent_pid(payload, agent).or_else(|| existing.as_ref().and_then(|record| record.pid));
    let launch_command = agent_hooks::launch_record_from_env(agent, cwd.as_deref()).or_else(|| {
        existing
            .as_ref()
            .and_then(|record| record.launch_command.clone())
    });

    let record = agent_hooks::AgentHookSessionRecord {
        session_id,
        workspace_id,
        surface_id,
        cwd,
        pid,
        launch_command,
        updated_at: agent_hooks::now_seconds(),
    };
    let result = store.upsert(record);
    if result.is_ok() {
        write_agent_hook_debug(
            agent,
            event,
            "upserted",
            &json!({
                "payload_keys": payload_keys(payload),
            }),
        );
    }
    result
}

fn hook_session_id(payload: &Value) -> Option<String> {
    hook_str(payload, &["session_id", "sessionId", "sessionID"])
        .map(str::to_string)
        .or_else(|| limux_env_value("CLAUDE_CODE_SESSION_ID"))
        .or_else(|| limux_env_value("CLAUDE_SESSION_ID"))
        .or_else(|| hook_session_id_from_transcript(payload))
        .or_else(|| limux_env_value("LIMUX_AGENT_SESSION_ID"))
        .or_else(|| limux_env_value("CMUX_AGENT_SESSION_ID"))
        .filter(|value| !value.trim().is_empty())
}

fn hook_agent_pid(payload: &Value, agent: agent_hooks::AgentKind) -> Option<u32> {
    hook_str(payload, &["pid"])
        .and_then(|value| value.parse::<u32>().ok())
        .or_else(|| limux_env_value("LIMUX_AGENT_PID").and_then(|value| value.parse().ok()))
        .or_else(|| limux_env_value("CMUX_AGENT_PID").and_then(|value| value.parse().ok()))
        .or_else(|| agent_ancestor_pid(agent))
}

fn hook_session_id_from_transcript(payload: &Value) -> Option<String> {
    let transcript = hook_str(
        payload,
        &["transcript_path", "transcriptPath", "transcript"],
    )?;
    Path::new(transcript)
        .file_stem()
        .and_then(|stem| stem.to_str())
        .map(str::to_string)
        .filter(|value| !value.trim().is_empty())
}

fn payload_keys(payload: &Value) -> Vec<String> {
    payload
        .as_object()
        .map(|object| object.keys().cloned().collect())
        .unwrap_or_default()
}

fn write_agent_hook_debug(
    agent: agent_hooks::AgentKind,
    event: &str,
    outcome: &str,
    details: &Value,
) {
    let Some(dir) = agent_hook_debug_dir() else {
        return;
    };
    if fs::create_dir_all(&dir).is_err() {
        return;
    }
    let path = dir.join("agent-hook-debug.jsonl");
    let line = json!({
        "time": agent_hooks::now_seconds(),
        "agent": agent.store_name(),
        "event": event,
        "outcome": outcome,
        "details": details,
    });
    if let Ok(mut encoded) = serde_json::to_vec(&line) {
        encoded.push(b'\n');
        let _ = append_debug_line(&path, &encoded);
    }
}

fn agent_hook_debug_dir() -> Option<PathBuf> {
    if let Some(dir) = env::var_os("LIMUX_AGENT_HOOK_STATE_DIR") {
        return Some(PathBuf::from(dir));
    }
    dirs::state_dir()
        .map(|dir| dir.join("limux"))
        .or_else(|| dirs::home_dir().map(|home| home.join(".local/state/limux")))
}

fn append_debug_line(path: &Path, bytes: &[u8]) -> Result<()> {
    use std::io::Write;
    let mut file = fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
        .with_context(|| format!("failed to open {}", path.display()))?;
    file.write_all(bytes)
        .with_context(|| format!("failed to append {}", path.display()))
}

fn limux_env_value(name: &str) -> Option<String> {
    env::var(name)
        .ok()
        .filter(|value| !value.trim().is_empty())
        .or_else(|| ancestor_env_value(name))
}

/// purpose: Read Limux context env with CMUX-compatible aliases.
/// inputs: name is a Limux context variable such as LIMUX_WORKSPACE_ID.
/// returns/effects: Returns the first non-empty Limux or matching CMUX value.
fn context_env_value(name: &str) -> Option<String> {
    limux_env_value(name).or_else(|| {
        let cmux_name = match name {
            "LIMUX_WORKSPACE_ID" => "CMUX_WORKSPACE_ID",
            "LIMUX_SURFACE_ID" => "CMUX_SURFACE_ID",
            "LIMUX_TAB_ID" => "CMUX_TAB_ID",
            "LIMUX_SOCKET" => "CMUX_SOCKET_PATH",
            _ => return None,
        };
        limux_env_value(cmux_name)
    })
}

#[cfg(target_os = "linux")]
fn agent_ancestor_pid(agent: agent_hooks::AgentKind) -> Option<u32> {
    let needle = agent.store_name();
    let mut pid = std::process::id();
    for _ in 0..8 {
        let parent = proc_parent_pid(pid)?;
        if parent <= 1 || parent == pid {
            return None;
        }
        if proc_identity_contains(parent, needle) {
            return Some(parent);
        }
        pid = parent;
    }
    None
}

#[cfg(not(target_os = "linux"))]
fn agent_ancestor_pid(_agent: agent_hooks::AgentKind) -> Option<u32> {
    None
}

#[cfg(target_os = "linux")]
fn proc_identity_contains(pid: u32, needle: &str) -> bool {
    let needle = needle.to_ascii_lowercase();
    proc_cmdline(pid)
        .or_else(|| fs::read_to_string(format!("/proc/{pid}/comm")).ok())
        .map(|value| value.to_ascii_lowercase().contains(&needle))
        .unwrap_or(false)
}

#[cfg(target_os = "linux")]
fn proc_cmdline(pid: u32) -> Option<String> {
    let raw = fs::read(format!("/proc/{pid}/cmdline")).ok()?;
    let parts = raw
        .split(|byte| *byte == 0)
        .filter(|part| !part.is_empty())
        .filter_map(|part| std::str::from_utf8(part).ok())
        .collect::<Vec<_>>();
    (!parts.is_empty()).then(|| parts.join(" "))
}

#[cfg(target_os = "linux")]
fn ancestor_env_value(name: &str) -> Option<String> {
    let mut pid = std::process::id();
    for _ in 0..8 {
        let parent = proc_parent_pid(pid)?;
        if parent <= 1 || parent == pid {
            return None;
        }
        if let Some(value) = proc_env_value(parent, name) {
            return Some(value);
        }
        pid = parent;
    }
    None
}

#[cfg(not(target_os = "linux"))]
fn ancestor_env_value(_name: &str) -> Option<String> {
    None
}

#[cfg(target_os = "linux")]
fn proc_parent_pid(pid: u32) -> Option<u32> {
    let stat = fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
    parse_proc_stat_parent_pid(&stat)
}

#[cfg(target_os = "linux")]
fn parse_proc_stat_parent_pid(stat: &str) -> Option<u32> {
    let close = stat.rfind(')')?;
    let mut fields = stat.get(close + 2..)?.split_whitespace();
    fields.next()?;
    fields.next()?.parse().ok()
}

#[cfg(target_os = "linux")]
fn proc_env_value(pid: u32, name: &str) -> Option<String> {
    let environ = fs::read(format!("/proc/{pid}/environ")).ok()?;
    env_value_from_environ(&environ, name)
}

fn env_value_from_environ(environ: &[u8], name: &str) -> Option<String> {
    let prefix = format!("{name}=");
    environ
        .split(|byte| *byte == 0)
        .filter_map(|part| std::str::from_utf8(part).ok())
        .find_map(|entry| entry.strip_prefix(&prefix).map(str::to_string))
        .filter(|value| !value.trim().is_empty())
}

async fn run_hooks_command(
    client: &mut Client,
    args: &[String],
    json_output: bool,
) -> Result<CommandOutput> {
    let Some(first) = args.first().map(String::as_str) else {
        bail!(
            "Usage: limux hooks setup [agent]|uninstall [agent]|<agent> install|uninstall|<event>"
        );
    };

    match first {
        "feed" => return run_feed_hook(client, &args[1..], json_output).await,
        "setup" | "install" => {
            let target = parse_opt(args, "--agent").or_else(|| positional_arg(args, 1));
            let installed = install_hook_targets(target.as_deref())?;
            return hooks_summary_output("installed", installed, json_output);
        }
        "uninstall" => {
            let target = parse_opt(args, "--agent").or_else(|| positional_arg(args, 1));
            let changed = uninstall_hook_targets(target.as_deref())?;
            return hooks_summary_output("uninstalled", changed, json_output);
        }
        _ => {}
    }

    let agent = agent_hooks::AgentKind::from_hook_name(first)
        .ok_or_else(|| anyhow!("unknown hooks target: {first}"))?;
    let rest = &args[1..];
    match rest.first().map(String::as_str) {
        Some("install") => {
            install_hook_target(agent)?;
            hooks_summary_output(
                "installed",
                vec![agent.store_name().to_string()],
                json_output,
            )
        }
        Some("uninstall") => {
            uninstall_hook_target(agent)?;
            hooks_summary_output(
                "uninstalled",
                vec![agent.store_name().to_string()],
                json_output,
            )
        }
        _ => {
            let payload = run_agent_hook(client, agent, rest).await?;
            if json_output {
                Ok(CommandOutput::Json(payload))
            } else {
                Ok(CommandOutput::Text("OK".to_string()))
            }
        }
    }
}

fn hooks_summary_output(
    action: &str,
    agents: Vec<String>,
    json_output: bool,
) -> Result<CommandOutput> {
    if json_output {
        Ok(CommandOutput::Json(json!({
            "action": action,
            "agents": agents,
        })))
    } else {
        Ok(CommandOutput::Text(format!(
            "OK {action}: {}",
            if agents.is_empty() {
                "none".to_string()
            } else {
                agents.join(", ")
            }
        )))
    }
}

fn install_hook_targets(target: Option<&str>) -> Result<Vec<String>> {
    let agents = target
        .map(|name| {
            agent_hooks::AgentKind::from_hook_name(name)
                .ok_or_else(|| anyhow!("unknown hooks target: {name}"))
                .map(|agent| vec![agent])
        })
        .transpose()?
        .unwrap_or_else(default_hook_targets);

    let mut installed = Vec::new();
    for agent in agents {
        install_hook_target(agent)?;
        installed.push(agent.store_name().to_string());
    }
    Ok(installed)
}

fn uninstall_hook_targets(target: Option<&str>) -> Result<Vec<String>> {
    let agents = target
        .map(|name| {
            agent_hooks::AgentKind::from_hook_name(name)
                .ok_or_else(|| anyhow!("unknown hooks target: {name}"))
                .map(|agent| vec![agent])
        })
        .transpose()?
        .unwrap_or_else(default_hook_targets);

    let mut changed = Vec::new();
    for agent in agents {
        uninstall_hook_target(agent)?;
        changed.push(agent.store_name().to_string());
    }
    Ok(changed)
}

fn default_hook_targets() -> Vec<agent_hooks::AgentKind> {
    vec![
        agent_hooks::AgentKind::Codex,
        agent_hooks::AgentKind::Claude,
        agent_hooks::AgentKind::Gemini,
    ]
}

fn install_hook_target(agent: agent_hooks::AgentKind) -> Result<()> {
    match agent {
        agent_hooks::AgentKind::Codex => install_json_hooks_with_feed(
            &codex_hooks_path(),
            agent,
            &[
                ("SessionStart", "session-start"),
                ("UserPromptSubmit", "prompt-submit"),
                ("Stop", "stop"),
            ],
            codex_feed_hook_events(),
        ),
        agent_hooks::AgentKind::Grok => install_json_hooks_with_feed(
            &grok_hooks_path(),
            agent,
            &[
                ("SessionStart", "session-start"),
                ("UserPromptSubmit", "prompt-submit"),
                ("Stop", "stop"),
                ("Notification", "notification"),
                ("SessionEnd", "session-end"),
            ],
            grok_feed_hook_events(),
        ),
        agent_hooks::AgentKind::Claude => install_json_hooks_with_feed(
            &claude_settings_path(),
            agent,
            &[
                ("SessionStart", "session-start"),
                ("UserPromptSubmit", "prompt-submit"),
                ("Stop", "stop"),
                ("Notification", "stop"),
                ("SessionEnd", "session-end"),
            ],
            claude_feed_hook_events(),
        ),
        agent_hooks::AgentKind::OpenCode => install_opencode_plugin(),
        agent_hooks::AgentKind::Cursor => install_flat_json_hooks_with_feed(
            &cursor_hooks_path(),
            agent,
            &[
                ("beforeSubmitPrompt", "prompt-submit"),
                ("stop", "stop"),
                ("afterAgentResponse", "agent-response"),
                ("beforeShellExecution", "shell-exec"),
                ("afterShellExecution", "shell-done"),
            ],
            cursor_feed_hook_events(),
        ),
        agent_hooks::AgentKind::Kiro => install_kiro_agent_hooks_with_feed(
            &kiro_agent_path(),
            agent,
            &[
                ("agentSpawn", "session-start"),
                ("userPromptSubmit", "prompt-submit"),
                ("stop", "stop"),
            ],
            kiro_feed_hook_events(),
        ),
        agent_hooks::AgentKind::Antigravity => install_antigravity_hooks_with_feed(
            &antigravity_hooks_path(),
            agent,
            &[
                ("SessionStart", "session-start"),
                ("PreInvocation", "prompt-submit"),
                ("Stop", "stop"),
                ("turn-completion", "stop"),
                ("Notification", "notification"),
                ("SessionEnd", "session-end"),
            ],
            antigravity_feed_hook_events(),
        ),
        agent_hooks::AgentKind::RovoDev => install_rovodev_hooks(
            &rovodev_config_path(),
            agent,
            &[
                ("on_complete", "stop"),
                ("on_error", "stop"),
                ("on_tool_permission", "prompt-submit"),
            ],
        ),
        agent_hooks::AgentKind::Pi => install_pi_extension(),
        agent_hooks::AgentKind::Omp => install_omp_extension(),
        agent_hooks::AgentKind::Amp => install_amp_extension(),
        agent_hooks::AgentKind::HermesAgent => install_hermes_agent_hooks(),
        agent_hooks::AgentKind::Gemini => install_json_hooks_with_feed(
            &gemini_settings_path(),
            agent,
            &[
                ("SessionStart", "session-start"),
                ("BeforeAgent", "prompt-submit"),
                ("AfterAgent", "stop"),
                ("SessionEnd", "session-end"),
            ],
            gemini_feed_hook_events(),
        ),
        agent_hooks::AgentKind::Copilot => install_json_hooks_with_feed(
            &copilot_config_path(),
            agent,
            &[
                ("SessionStart", "session-start"),
                ("Stop", "stop"),
                ("Notification", "stop"),
                ("SessionEnd", "session-end"),
            ],
            pre_tool_use_feed_hook_events(),
        ),
        agent_hooks::AgentKind::CodeBuddy => install_json_hooks_with_feed(
            &codebuddy_settings_path(),
            agent,
            &[
                ("SessionStart", "session-start"),
                ("Stop", "stop"),
                ("Notification", "stop"),
                ("SessionEnd", "session-end"),
            ],
            pre_tool_use_feed_hook_events(),
        ),
        agent_hooks::AgentKind::Factory => install_json_hooks_with_feed(
            &factory_settings_path(),
            agent,
            &[
                ("SessionStart", "session-start"),
                ("Stop", "stop"),
                ("Notification", "stop"),
                ("SessionEnd", "session-end"),
            ],
            pre_tool_use_feed_hook_events(),
        ),
        agent_hooks::AgentKind::Qoder => install_json_hooks_with_feed(
            &qoder_settings_path(),
            agent,
            &[
                ("SessionStart", "session-start"),
                ("Stop", "stop"),
                ("SessionEnd", "session-end"),
            ],
            pre_tool_use_feed_hook_events(),
        ),
    }
}

fn uninstall_hook_target(agent: agent_hooks::AgentKind) -> Result<()> {
    match agent {
        agent_hooks::AgentKind::Codex => uninstall_json_hooks(&codex_hooks_path(), agent),
        agent_hooks::AgentKind::Grok => uninstall_json_hooks(&grok_hooks_path(), agent),
        agent_hooks::AgentKind::Claude => uninstall_json_hooks(&claude_settings_path(), agent),
        agent_hooks::AgentKind::OpenCode => {
            let path = opencode_plugin_path();
            if path.exists() {
                fs::remove_file(&path)
                    .with_context(|| format!("failed to remove {}", path.display()))?;
            }
            opencode_config_unregister_plugin()
        }
        agent_hooks::AgentKind::Cursor => uninstall_json_hooks(&cursor_hooks_path(), agent),
        agent_hooks::AgentKind::Kiro => uninstall_json_hooks(&kiro_agent_path(), agent),
        agent_hooks::AgentKind::Antigravity => {
            uninstall_antigravity_hooks(&antigravity_hooks_path(), agent)
        }
        agent_hooks::AgentKind::RovoDev => uninstall_rovodev_hooks(&rovodev_config_path()),
        agent_hooks::AgentKind::Pi => uninstall_pi_extension(),
        agent_hooks::AgentKind::Omp => uninstall_omp_extension(),
        agent_hooks::AgentKind::Amp => uninstall_amp_extension(),
        agent_hooks::AgentKind::HermesAgent => uninstall_hermes_agent_hooks(),
        agent_hooks::AgentKind::Gemini => uninstall_json_hooks(&gemini_settings_path(), agent),
        agent_hooks::AgentKind::Copilot => uninstall_json_hooks(&copilot_config_path(), agent),
        agent_hooks::AgentKind::CodeBuddy => {
            uninstall_json_hooks(&codebuddy_settings_path(), agent)
        }
        agent_hooks::AgentKind::Factory => uninstall_json_hooks(&factory_settings_path(), agent),
        agent_hooks::AgentKind::Qoder => uninstall_json_hooks(&qoder_settings_path(), agent),
    }
}

// purpose: Install session lifecycle hooks and optional Feed hooks into an agent JSON hook file.
// inputs: Target JSON path, agent kind, lifecycle event mappings, and Feed hook event names.
// returns/effects: Rewrites the hook file after removing stale Limux entries for that agent.
fn install_json_hooks_with_feed(
    path: &Path,
    agent: agent_hooks::AgentKind,
    events: &[(&str, &str)],
    feed_events: &[&str],
) -> Result<()> {
    let mut root = read_json_object(path)?;
    let hooks = root
        .entry("hooks".to_string())
        .or_insert_with(|| Value::Object(Map::new()));
    let hooks = hooks
        .as_object_mut()
        .ok_or_else(|| anyhow!("{} has non-object hooks field", path.display()))?;
    let marker = hook_marker(agent);
    for value in hooks.values_mut() {
        if let Some(entries) = value.as_array_mut() {
            entries.retain(|entry| !hook_entry_matches_agent(agent, entry, marker));
        }
    }
    hooks.retain(|_, value| {
        value
            .as_array()
            .map(|entries| !entries.is_empty())
            .unwrap_or(true)
    });

    for (agent_event, limux_event) in events {
        let entries = hooks
            .entry((*agent_event).to_string())
            .or_insert_with(|| Value::Array(Vec::new()));
        let entries = entries
            .as_array_mut()
            .ok_or_else(|| anyhow!("{} hook {agent_event} is not an array", path.display()))?;
        entries.retain(|entry| !json_value_contains(entry, marker));
        let mut entry = json!({
            "hooks": [{
                "type": "command",
                "command": hook_command(agent, limux_event)?,
                "statusMessage": format!("Limux {} session restore", agent.label()),
                "timeout": hook_timeout(agent)
            }]
        });
        if matches!(agent, agent_hooks::AgentKind::Claude) {
            entry["matcher"] = Value::String("*".to_string());
        }
        entries.push(entry);
    }
    for agent_event in feed_events {
        let entries = hooks
            .entry((*agent_event).to_string())
            .or_insert_with(|| Value::Array(Vec::new()));
        let entries = entries
            .as_array_mut()
            .ok_or_else(|| anyhow!("{} hook {agent_event} is not an array", path.display()))?;
        entries.retain(|entry| !hook_entry_matches_agent(agent, entry, marker));
        entries.push(json!({
            "hooks": [{
                "type": "command",
                "command": feed_hook_command(agent, agent_event)?,
                "statusMessage": format!("Limux {} Feed", agent.label()),
                "timeout": feed_hook_timeout(agent)
            }]
        }));
        if matches!(agent, agent_hooks::AgentKind::Claude) {
            let Some(entry) = entries.last_mut() else {
                bail!("failed to append {} Feed hook {agent_event}", agent.label());
            };
            entry["matcher"] = Value::String("*".to_string());
        }
    }

    write_json_object(path, &root)
}

// purpose: Install flat JSON hooks and optional Feed hooks into an agent hook file.
// inputs: Target JSON path, agent kind, lifecycle event mappings, and Feed hook event names.
// returns/effects: Rewrites flat hook arrays after removing stale Limux entries for that agent.
fn install_flat_json_hooks_with_feed(
    path: &Path,
    agent: agent_hooks::AgentKind,
    events: &[(&str, &str)],
    feed_events: &[&str],
) -> Result<()> {
    let mut root = read_json_object(path)?;
    root.insert("version".to_string(), json!(1));
    let hooks = root
        .entry("hooks".to_string())
        .or_insert_with(|| Value::Object(Map::new()));
    let hooks = hooks
        .as_object_mut()
        .ok_or_else(|| anyhow!("{} has non-object hooks field", path.display()))?;
    let marker = hook_marker(agent);
    for value in hooks.values_mut() {
        if let Some(entries) = value.as_array_mut() {
            entries.retain(|entry| !hook_entry_matches_agent(agent, entry, marker));
        }
    }
    hooks.retain(|_, value| {
        value
            .as_array()
            .map(|entries| !entries.is_empty())
            .unwrap_or(true)
    });

    for (agent_event, limux_event) in events {
        append_flat_hook_entry(path, hooks, agent_event, hook_command(agent, limux_event)?)?;
    }
    for agent_event in feed_events {
        append_flat_hook_entry(
            path,
            hooks,
            agent_event,
            feed_hook_command(agent, agent_event)?,
        )?;
    }

    write_json_object(path, &root)
}

// purpose: Append one command entry to a flat JSON hook event array.
// inputs: Config path for diagnostics, hooks map, agent event, and shell command.
// returns/effects: Mutates the hooks map or fails if the existing hook is not an array.
fn append_flat_hook_entry(
    path: &Path,
    hooks: &mut Map<String, Value>,
    agent_event: &str,
    command: String,
) -> Result<()> {
    let entries = hooks
        .entry(agent_event.to_string())
        .or_insert_with(|| Value::Array(Vec::new()));
    let entries = entries
        .as_array_mut()
        .ok_or_else(|| anyhow!("{} hook {agent_event} is not an array", path.display()))?;
    entries.push(json!({ "command": command }));
    Ok(())
}

// purpose: Install Kiro's CMUX agent JSON with lifecycle and Feed bridge hooks.
// inputs: Kiro agent JSON path, agent kind, lifecycle event mappings, and Feed event names.
// returns/effects: Rewrites owned hook entries and preserves user-provided agent metadata/tools.
fn install_kiro_agent_hooks_with_feed(
    path: &Path,
    agent: agent_hooks::AgentKind,
    events: &[(&str, &str)],
    feed_events: &[&str],
) -> Result<()> {
    let mut root = read_json_object(path)?;
    let hooks = root
        .entry("hooks".to_string())
        .or_insert_with(|| Value::Object(Map::new()));
    let hooks = hooks
        .as_object_mut()
        .ok_or_else(|| anyhow!("{} has non-object hooks field", path.display()))?;
    let marker = hook_marker(agent);
    remove_owned_hook_entries(hooks, agent, marker);

    for (agent_event, limux_event) in events {
        append_kiro_hook_entry(
            path,
            hooks,
            agent_event,
            hook_command(agent, limux_event)?,
            hook_timeout(agent),
        )?;
    }
    for agent_event in feed_events {
        append_kiro_hook_entry(
            path,
            hooks,
            agent_event,
            feed_hook_command(agent, agent_event)?,
            feed_hook_timeout(agent),
        )?;
    }

    root.entry("name".to_string())
        .or_insert_with(|| Value::String("cmux".to_string()));
    root.entry("description".to_string()).or_insert_with(|| {
        Value::String("CMUX notification and Feed bridge hooks for Kiro CLI.".to_string())
    });
    root.entry("tools".to_string())
        .or_insert_with(|| json!(["*"]));

    write_json_object(path, &root)
}

// purpose: Append one Kiro flat command hook entry with millisecond timeout.
// inputs: Config path for diagnostics, hooks map, event name, command, and timeout_ms.
// returns/effects: Mutates the hook list or fails if the existing event value is not an array.
fn append_kiro_hook_entry(
    path: &Path,
    hooks: &mut Map<String, Value>,
    agent_event: &str,
    command: String,
    timeout_ms: u64,
) -> Result<()> {
    let entries = hooks
        .entry(agent_event.to_string())
        .or_insert_with(|| Value::Array(Vec::new()));
    let entries = entries
        .as_array_mut()
        .ok_or_else(|| anyhow!("{} hook {agent_event} is not an array", path.display()))?;
    entries.push(json!({ "command": command, "timeout_ms": timeout_ms.max(1) }));
    Ok(())
}

// purpose: Remove all hook entries owned by a Limux agent from a hook map.
// inputs: Hook map, agent kind, and lifecycle marker string.
// returns/effects: Mutates hook arrays and drops empty hook event arrays.
fn remove_owned_hook_entries(
    hooks: &mut Map<String, Value>,
    agent: agent_hooks::AgentKind,
    marker: &str,
) {
    for value in hooks.values_mut() {
        if let Some(entries) = value.as_array_mut() {
            entries.retain(|entry| !hook_entry_matches_agent(agent, entry, marker));
        }
    }
    hooks.retain(|_, value| {
        value
            .as_array()
            .map(|entries| !entries.is_empty())
            .unwrap_or(true)
    });
}

// purpose: Install Antigravity hooks into the top-level CMUX hook group.
// inputs: Antigravity hooks path, agent kind, lifecycle event mappings, and Feed event names.
// returns/effects: Replaces the owned `cmux` group while preserving other hook groups.
fn install_antigravity_hooks_with_feed(
    path: &Path,
    agent: agent_hooks::AgentKind,
    events: &[(&str, &str)],
    feed_events: &[&str],
) -> Result<()> {
    let mut root = read_json_object(path)?;
    let mut group = Map::new();
    for (agent_event, limux_event) in events {
        let entry = antigravity_hook_entry(
            hook_command(agent, limux_event)?,
            hook_timeout(agent),
            agent_event,
        );
        append_antigravity_group_entry(&mut group, agent_event, entry);
    }
    for agent_event in feed_events {
        let entry = antigravity_hook_entry(
            feed_hook_command(agent, agent_event)?,
            feed_hook_timeout(agent),
            agent_event,
        );
        append_antigravity_group_entry(&mut group, agent_event, entry);
    }
    root.insert("cmux".to_string(), Value::Object(group));
    write_json_object(path, &root)
}

// purpose: Append one Antigravity hook entry to an event list inside a group.
// inputs: Mutable group map, event name, and hook entry value.
// returns/effects: Mutates the event array, creating it when absent.
fn append_antigravity_group_entry(group: &mut Map<String, Value>, event: &str, entry: Value) {
    let entries = group
        .entry(event.to_string())
        .or_insert_with(|| Value::Array(Vec::new()));
    let Some(entries) = entries.as_array_mut() else {
        panic!("new Antigravity hook group event must be an array");
    };
    entries.push(entry);
}

// purpose: Build an Antigravity hook entry matching CMUX's lifecycle and Feed schemas.
// inputs: Shell command, second-based timeout value, and Antigravity event name.
// returns/effects: Returns a plain command entry or matcher-wrapped Feed hook entry.
fn antigravity_hook_entry(command: String, timeout_seconds: u64, event_name: &str) -> Value {
    let hook = json!({
        "type": "command",
        "command": command,
        "timeout": timeout_seconds.max(1)
    });
    if matches!(event_name, "PreToolUse" | "PostToolUse") {
        return json!({ "matcher": "*", "hooks": [hook] });
    }
    hook
}

// purpose: Remove Limux's Antigravity hook group when it contains owned commands.
// inputs: Antigravity hooks path and agent kind.
// returns/effects: Rewrites the JSON file with the `cmux` group removed when present.
fn uninstall_antigravity_hooks(path: &Path, agent: agent_hooks::AgentKind) -> Result<()> {
    if !path.exists() {
        return Ok(());
    }
    let mut root = read_json_object(path)?;
    let marker = hook_marker(agent);
    let should_remove = root
        .get("cmux")
        .is_some_and(|group| hook_entry_matches_agent(agent, group, marker));
    if should_remove {
        root.remove("cmux");
    }
    write_json_object(path, &root)
}

// purpose: Install Rovo Dev lifecycle hooks into config.yml using CMUX's marked YAML block.
// inputs: Rovo config path, agent kind, and event-to-Limux hook mappings.
// returns/effects: Preserves unrelated YAML text and rewrites only the owned marked block.
fn install_rovodev_hooks(
    path: &Path,
    agent: agent_hooks::AgentKind,
    events: &[(&str, &str)],
) -> Result<()> {
    let existing = read_optional_text(path)?;
    let entries = events
        .iter()
        .map(|(event_name, limux_event)| Ok((*event_name, hook_command(agent, limux_event)?)))
        .collect::<Result<Vec<_>>>()?;
    let updated = rovodev_installing(&existing, &entries);
    write_text_file(path, &updated)
}

// purpose: Remove Limux's Rovo Dev marked block from config.yml.
// inputs: Rovo config path.
// returns/effects: Leaves missing files and dangling begin markers untouched.
fn uninstall_rovodev_hooks(path: &Path) -> Result<()> {
    if !path.exists() {
        return Ok(());
    }
    let existing = read_optional_text(path)?;
    let updated = rovodev_uninstalling(&existing);
    write_text_file(path, &updated)
}

// purpose: Build Rovo Dev config text with an idempotent CMUX-compatible marked hook block.
// inputs: Existing config text and event command pairs.
// returns/effects: Returns complete config text without filesystem changes.
fn rovodev_installing(existing: &str, events: &[(&str, String)]) -> String {
    let mut lines = normalized_text_lines(existing);
    lines = rovodev_removing_marked_block(lines);

    if let Some(events_index) = rovodev_events_line_index(&lines) {
        let item_indent = format!("{}  ", leading_whitespace(&lines[events_index]));
        let block = rovodev_event_hooks_block(events, &item_indent, true);
        lines.splice(events_index + 1..events_index + 1, block);
    } else if let Some(event_hooks_index) = rovodev_event_hooks_line_index(&lines) {
        let child_indent = format!("{}  ", leading_whitespace(&lines[event_hooks_index]));
        let mut block = vec![
            format!("{child_indent}# cmux hooks rovodev begin"),
            format!("{child_indent}events:"),
        ];
        block.extend(rovodev_event_hooks_block(
            events,
            &format!("{child_indent}  "),
            false,
        ));
        block.push(format!("{child_indent}# cmux hooks rovodev end"));
        lines.splice(event_hooks_index + 1..event_hooks_index + 1, block);
    } else {
        if !lines.is_empty() && !lines.last().is_some_and(|line| line.trim().is_empty()) {
            lines.push(String::new());
        }
        lines.push("# cmux hooks rovodev begin".to_string());
        lines.push("eventHooks:".to_string());
        lines.push("  events:".to_string());
        lines.extend(rovodev_event_hooks_block(events, "    ", false));
        lines.push("# cmux hooks rovodev end".to_string());
    }

    serialize_text_lines(&lines)
}

// purpose: Remove the complete Rovo Dev marked block if both markers are present.
// inputs: Existing config text.
// returns/effects: Returns config text without a complete owned block.
fn rovodev_uninstalling(existing: &str) -> String {
    serialize_text_lines(&rovodev_removing_marked_block(normalized_text_lines(
        existing,
    )))
}

// purpose: Render Rovo Dev event hook YAML lines with optional markers.
// inputs: Event command pairs, item indentation, and marker flag.
// returns/effects: Returns YAML text lines without filesystem changes.
fn rovodev_event_hooks_block(
    events: &[(&str, String)],
    item_indent: &str,
    include_markers: bool,
) -> Vec<String> {
    let mut lines = Vec::new();
    if include_markers {
        lines.push(format!("{item_indent}# cmux hooks rovodev begin"));
    }
    for (event_name, command) in events {
        lines.push(format!("{item_indent}- name: {event_name}"));
        lines.push(format!("{item_indent}  commands:"));
        lines.push(format!(
            "{item_indent}    - command: {}",
            yaml_double_quoted(command)
        ));
    }
    if include_markers {
        lines.push(format!("{item_indent}# cmux hooks rovodev end"));
    }
    lines
}

// purpose: Remove complete Rovo Dev marker blocks from normalized lines.
// inputs: Config lines without a final empty line for trailing newline.
// returns/effects: Returns remaining lines; dangling begin markers are preserved.
fn rovodev_removing_marked_block(mut lines: Vec<String>) -> Vec<String> {
    let mut index = 0;
    while index < lines.len() {
        if lines[index].trim() != "# cmux hooks rovodev begin" {
            index += 1;
            continue;
        }
        let Some(relative_end) = lines[index + 1..]
            .iter()
            .position(|line| line.trim() == "# cmux hooks rovodev end")
        else {
            index += 1;
            continue;
        };
        let end_index = index + 1 + relative_end;
        let start_index = if index > 0 && lines[index - 1].trim().is_empty() {
            index - 1
        } else {
            index
        };
        lines.drain(start_index..=end_index);
        index = start_index;
    }
    lines
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct HermesAgentHookEvent {
    name: &'static str,
    command: String,
    timeout: u64,
}

// purpose: Build CMUX-compatible Hermes Agent lifecycle and Feed hook entries.
// inputs: Current Limux executable path.
// returns/effects: Returns shell-wrapped commands without filesystem changes.
fn hermes_agent_events() -> Result<Vec<HermesAgentHookEvent>> {
    let lifecycle = [
        ("on_session_start", "session-start"),
        ("pre_llm_call", "prompt-submit"),
        ("post_llm_call", "agent-response"),
        ("pre_approval_request", "notification"),
        ("post_approval_response", "approval-response"),
        ("on_session_end", "session-end"),
        ("on_session_finalize", "session-finalize"),
        ("on_session_reset", "session-start"),
    ];
    let feed = [
        "pre_tool_call",
        "post_tool_call",
        "pre_approval_request",
        "post_approval_response",
    ];
    let mut events = Vec::new();
    for (agent_event, limux_event) in lifecycle {
        events.push(HermesAgentHookEvent {
            name: agent_event,
            command: hermes_agent_shell_command(hook_command(
                agent_hooks::AgentKind::HermesAgent,
                limux_event,
            )?),
            timeout: 5,
        });
    }
    for agent_event in feed {
        events.push(HermesAgentHookEvent {
            name: agent_event,
            command: hermes_agent_shell_command(feed_hook_command(
                agent_hooks::AgentKind::HermesAgent,
                agent_event,
            )?),
            timeout: 120,
        });
    }
    Ok(events)
}

fn hermes_agent_shell_command(script: String) -> String {
    format!("sh -c {}", shell_single_quote(&script))
}

// purpose: Install marked Hermes Agent hook blocks into config YAML text.
// inputs: Existing YAML text and desired hook events.
// returns/effects: Returns rewritten text without touching files.
fn hermes_agent_hooks_installing(existing: &str, events: &[HermesAgentHookEvent]) -> String {
    let mut lines = hermes_agent_removing_marked_blocks(normalized_text_lines(existing));
    if events.is_empty() {
        return serialize_text_lines(&lines);
    }
    let mut block = Vec::new();
    block.push("# cmux hooks hermes-agent begin".to_string());
    block.push("hooks:".to_string());
    for (name, grouped) in hermes_agent_grouped_events(events) {
        block.push(format!("  {name}:"));
        block.extend(hermes_agent_hook_entries(&grouped, "    "));
    }
    block.push("# cmux hooks hermes-agent end".to_string());
    if !lines.is_empty() && !lines.last().is_some_and(|line| line.trim().is_empty()) {
        lines.push(String::new());
    }
    lines.extend(block);
    serialize_text_lines(&lines)
}

fn hermes_agent_hooks_uninstalling(existing: &str) -> String {
    serialize_text_lines(&hermes_agent_removing_marked_blocks(normalized_text_lines(
        existing,
    )))
}

fn hermes_agent_grouped_events(
    events: &[HermesAgentHookEvent],
) -> Vec<(&'static str, Vec<&HermesAgentHookEvent>)> {
    let mut grouped: Vec<(&'static str, Vec<&HermesAgentHookEvent>)> = Vec::new();
    for event in events {
        if let Some((_, entries)) = grouped.iter_mut().find(|(name, _)| *name == event.name) {
            entries.push(event);
        } else {
            grouped.push((event.name, vec![event]));
        }
    }
    grouped
}

fn hermes_agent_hook_entries(events: &[&HermesAgentHookEvent], item_indent: &str) -> Vec<String> {
    let mut lines = Vec::new();
    for event in events {
        lines.push(format!(
            "{item_indent}- command: {}",
            yaml_double_quoted(&event.command)
        ));
        lines.push(format!("{item_indent}  timeout: {}", event.timeout));
    }
    lines
}

// purpose: Remove complete Hermes Agent marked blocks from normalized lines.
// inputs: Config lines without a synthetic final trailing newline.
// returns/effects: Returns remaining lines and leaves dangling begin markers untouched.
fn hermes_agent_removing_marked_blocks(mut lines: Vec<String>) -> Vec<String> {
    let mut index = 0;
    while index < lines.len() {
        if lines[index].trim() != "# cmux hooks hermes-agent begin" {
            index += 1;
            continue;
        }
        let Some(relative_end) = lines[index + 1..]
            .iter()
            .position(|line| line.trim() == "# cmux hooks hermes-agent end")
        else {
            index += 1;
            continue;
        };
        let end_index = index + 1 + relative_end;
        let start_index = if index > 0 && lines[index - 1].trim().is_empty() {
            index - 1
        } else {
            index
        };
        lines.drain(start_index..=end_index);
        index = start_index;
    }
    lines
}

fn hermes_agent_allowlist_installing(
    existing: Option<&[u8]>,
    events: &[HermesAgentHookEvent],
) -> Result<Vec<u8>> {
    let mut root = hermes_agent_allowlist_root(existing)?;
    let mut approvals = root
        .remove("approvals")
        .and_then(|value| value.as_array().cloned())
        .unwrap_or_default();
    for event in events {
        approvals.retain(|approval| !hermes_agent_approval_matches(approval, event));
        approvals.push(json!({
            "event": event.name,
            "command": event.command,
            "approved_at": format!("{:.3}", agent_hooks::now_seconds()),
        }));
    }
    root.insert("approvals".to_string(), Value::Array(approvals));
    serde_json::to_vec_pretty(&Value::Object(root)).context("failed to encode Hermes allowlist")
}

fn hermes_agent_allowlist_uninstalling(
    existing: &[u8],
    events: &[HermesAgentHookEvent],
) -> Result<Vec<u8>> {
    let mut root = hermes_agent_allowlist_root(Some(existing))?;
    let mut approvals = root
        .remove("approvals")
        .and_then(|value| value.as_array().cloned())
        .unwrap_or_default();
    approvals.retain(|approval| {
        !events
            .iter()
            .any(|event| hermes_agent_approval_matches(approval, event))
    });
    root.insert("approvals".to_string(), Value::Array(approvals));
    serde_json::to_vec_pretty(&Value::Object(root)).context("failed to encode Hermes allowlist")
}

fn hermes_agent_allowlist_root(existing: Option<&[u8]>) -> Result<Map<String, Value>> {
    let Some(existing) = existing.filter(|bytes| !bytes.is_empty()) else {
        return Ok(Map::new());
    };
    let value: Value =
        serde_json::from_slice(existing).context("failed to parse Hermes allowlist JSON")?;
    value
        .as_object()
        .cloned()
        .ok_or_else(|| anyhow!("Hermes allowlist must contain a JSON object"))
}

fn hermes_agent_approval_matches(approval: &Value, event: &HermesAgentHookEvent) -> bool {
    approval
        .get("event")
        .and_then(Value::as_str)
        .is_some_and(|value| value == event.name)
        && approval
            .get("command")
            .and_then(Value::as_str)
            .is_some_and(|value| value == event.command)
}

// purpose: Find the top-level Rovo Dev eventHooks line.
// inputs: Normalized YAML lines.
// returns/effects: Returns the line index when present.
fn rovodev_event_hooks_line_index(lines: &[String]) -> Option<usize> {
    lines.iter().position(|line| {
        let trimmed = line.trim_start();
        trimmed == "eventHooks:" || trimmed.starts_with("eventHooks: #")
    })
}

// purpose: Find the direct events child under eventHooks, ignoring nested events keys.
// inputs: Normalized YAML lines.
// returns/effects: Returns the direct child line index when present.
fn rovodev_events_line_index(lines: &[String]) -> Option<usize> {
    let event_hooks_index = rovodev_event_hooks_line_index(lines)?;
    let child_indent = format!("{}  ", leading_whitespace(&lines[event_hooks_index]));
    for (index, line) in lines.iter().enumerate().skip(event_hooks_index + 1) {
        if !line.trim().is_empty() && !line.starts_with(&child_indent) {
            return None;
        }
        if !line.starts_with(&child_indent) {
            continue;
        }
        let suffix = &line[child_indent.len()..];
        if suffix == "events:" || suffix.starts_with("events: #") {
            return Some(index);
        }
    }
    None
}

// purpose: Split optional text content into CMUX-compatible normalized lines.
// inputs: Raw text.
// returns/effects: Drops only the synthetic final empty line from a trailing newline.
fn normalized_text_lines(content: &str) -> Vec<String> {
    let mut lines = content.split('\n').map(str::to_string).collect::<Vec<_>>();
    if lines.last().is_some_and(String::is_empty) {
        lines.pop();
    }
    lines
}

// purpose: Serialize normalized text lines using CMUX's trailing newline convention.
// inputs: Normalized text lines.
// returns/effects: Returns empty text for no lines, otherwise line-joined text plus newline.
fn serialize_text_lines(lines: &[String]) -> String {
    if lines.is_empty() {
        String::new()
    } else {
        format!("{}\n", lines.join("\n"))
    }
}

// purpose: Extract leading spaces and tabs from one line.
// inputs: Text line.
// returns/effects: Returns the indentation prefix.
fn leading_whitespace(line: &str) -> &str {
    let bytes = line.as_bytes();
    let end = bytes
        .iter()
        .position(|byte| !matches!(*byte, b' ' | b'\t'))
        .unwrap_or(bytes.len());
    &line[..end]
}

// purpose: Escape a string as a YAML double-quoted scalar for Rovo Dev config.
// inputs: Raw command string.
// returns/effects: Returns an escaped scalar without validating YAML semantics elsewhere.
fn yaml_double_quoted(value: &str) -> String {
    let escaped = value
        .replace('\\', "\\\\")
        .replace('"', "\\\"")
        .replace('\n', "\\n");
    format!("\"{escaped}\"")
}

// purpose: List the Claude hook events that CMUX forwards into Feed.
// inputs: None.
// returns/effects: Returns static Claude hook event names without side effects.
fn claude_feed_hook_events() -> &'static [&'static str] {
    &["PreToolUse", "PermissionRequest"]
}

// purpose: List the Codex hook events that CMUX forwards into Feed.
// inputs: None.
// returns/effects: Returns static Codex hook event names without side effects.
fn codex_feed_hook_events() -> &'static [&'static str] {
    &[
        "PreToolUse",
        "PermissionRequest",
        "PostToolUse",
        "PreCompact",
        "PostCompact",
        "SubagentStart",
        "SubagentStop",
    ]
}

// purpose: List the Grok hook events that CMUX forwards into Feed.
// inputs: None.
// returns/effects: Returns static Grok hook event names without side effects.
fn grok_feed_hook_events() -> &'static [&'static str] {
    &["PreToolUse"]
}

// purpose: List the Cursor hook events that CMUX forwards into Feed.
// inputs: None.
// returns/effects: Returns static Cursor hook event names without side effects.
fn cursor_feed_hook_events() -> &'static [&'static str] {
    &["beforeShellExecution"]
}

// purpose: List the Kiro hook events that CMUX forwards into Feed.
// inputs: None.
// returns/effects: Returns static Kiro hook event names without side effects.
fn kiro_feed_hook_events() -> &'static [&'static str] {
    &["preToolUse", "postToolUse"]
}

// purpose: List the Antigravity hook events that CMUX forwards into Feed.
// inputs: None.
// returns/effects: Returns static Antigravity hook event names without side effects.
fn antigravity_feed_hook_events() -> &'static [&'static str] {
    &["PreToolUse", "PostToolUse"]
}

// purpose: List the Gemini hook events that CMUX forwards into Feed.
// inputs: None.
// returns/effects: Returns static Gemini hook event names without side effects.
fn gemini_feed_hook_events() -> &'static [&'static str] {
    &["PreToolUse"]
}

// purpose: List the common CMUX PreToolUse Feed event for nested JSON hook agents.
// inputs: None.
// returns/effects: Returns static hook event names without side effects.
fn pre_tool_use_feed_hook_events() -> &'static [&'static str] {
    &["PreToolUse"]
}

fn hook_timeout(agent: agent_hooks::AgentKind) -> u64 {
    match agent {
        agent_hooks::AgentKind::Claude | agent_hooks::AgentKind::Grok => 5,
        agent_hooks::AgentKind::Antigravity => 10,
        agent_hooks::AgentKind::Codex
        | agent_hooks::AgentKind::Cursor
        | agent_hooks::AgentKind::Kiro
        | agent_hooks::AgentKind::Gemini
        | agent_hooks::AgentKind::Copilot
        | agent_hooks::AgentKind::CodeBuddy
        | agent_hooks::AgentKind::Factory
        | agent_hooks::AgentKind::Qoder => 5000,
        agent_hooks::AgentKind::OpenCode | agent_hooks::AgentKind::RovoDev => 0,
        agent_hooks::AgentKind::Pi => 0,
        agent_hooks::AgentKind::Omp => 0,
        agent_hooks::AgentKind::Amp => 0,
        agent_hooks::AgentKind::HermesAgent => 5,
    }
}

// purpose: Match CMUX's Feed hook timeout units for each supported agent schema.
// inputs: Agent kind whose hook schema is being written.
// returns/effects: Returns the timeout value stored in that agent's hook JSON.
fn feed_hook_timeout(agent: agent_hooks::AgentKind) -> u64 {
    match agent {
        agent_hooks::AgentKind::Codex => 5,
        agent_hooks::AgentKind::Grok => 120,
        agent_hooks::AgentKind::Cursor => 0,
        agent_hooks::AgentKind::Kiro => 120_000,
        agent_hooks::AgentKind::Antigravity => 120,
        agent_hooks::AgentKind::Claude
        | agent_hooks::AgentKind::Gemini
        | agent_hooks::AgentKind::Copilot
        | agent_hooks::AgentKind::CodeBuddy
        | agent_hooks::AgentKind::Factory
        | agent_hooks::AgentKind::Qoder => 120_000,
        agent_hooks::AgentKind::OpenCode | agent_hooks::AgentKind::RovoDev => 0,
        agent_hooks::AgentKind::Pi => 0,
        agent_hooks::AgentKind::Omp => 0,
        agent_hooks::AgentKind::Amp => 0,
        agent_hooks::AgentKind::HermesAgent => 120,
    }
}

fn uninstall_json_hooks(path: &Path, agent: agent_hooks::AgentKind) -> Result<()> {
    if !path.exists() {
        return Ok(());
    }
    let mut root = read_json_object(path)?;
    if let Some(hooks) = root.get_mut("hooks").and_then(Value::as_object_mut) {
        let marker = hook_marker(agent);
        for value in hooks.values_mut() {
            if let Some(entries) = value.as_array_mut() {
                entries.retain(|entry| !hook_entry_matches_agent(agent, entry, marker));
            }
        }
        hooks.retain(|_, value| {
            value
                .as_array()
                .map(|entries| !entries.is_empty())
                .unwrap_or(true)
        });
    }
    write_json_object(path, &root)
}

// purpose: Detect any lifecycle or Feed hook entry owned by a specific Limux agent integration.
// inputs: Agent kind, candidate JSON hook entry, and lifecycle command marker.
// returns/effects: Returns true when the entry should be replaced or removed.
fn hook_entry_matches_agent(agent: agent_hooks::AgentKind, entry: &Value, marker: &str) -> bool {
    json_value_contains(entry, marker)
        || feed_hook_marker(agent)
            .is_some_and(|feed_marker| json_value_contains(entry, &feed_marker))
}

fn install_opencode_plugin() -> Result<()> {
    let path = opencode_plugin_path();
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)
            .with_context(|| format!("failed to create {}", parent.display()))?;
    }
    fs::write(&path, opencode_plugin_source()?).context("failed to write OpenCode plugin")?;
    opencode_config_register_plugin(&path)
}

// purpose: Install CMUX-compatible Pi extension hooks.
// inputs: None; path is resolved from PI_CODING_AGENT_DIR, PI_CONFIG_DIR, HOME, or dirs.
// returns/effects: Writes the generated extension, refusing to replace non-Limux files.
fn install_pi_extension() -> Result<()> {
    let path = pi_extension_path();
    let source = pi_extension_source()?;
    install_marked_extension_file(&path, &source, "cmux-pi-session-extension-marker")
}

// purpose: Remove the installed Pi extension only when it carries the owned marker.
// inputs: None.
// returns/effects: Deletes the generated extension or leaves missing files untouched.
fn uninstall_pi_extension() -> Result<()> {
    let path = pi_extension_path();
    uninstall_marked_extension_file(&path, "cmux-pi-session-extension-marker")
}

// purpose: Install CMUX-compatible OMP extension hooks.
// inputs: None; path is resolved from PI_CODING_AGENT_DIR, PI_CONFIG_DIR, HOME, or dirs.
// returns/effects: Writes the generated extension, refusing to replace non-Limux files.
fn install_omp_extension() -> Result<()> {
    let path = omp_extension_path();
    let source = omp_extension_source()?;
    install_marked_extension_file(&path, &source, "cmux-omp-session-extension-marker")
}

// purpose: Remove the installed OMP extension only when it carries the owned marker.
// inputs: None.
// returns/effects: Deletes the generated extension or leaves missing files untouched.
fn uninstall_omp_extension() -> Result<()> {
    let path = omp_extension_path();
    uninstall_marked_extension_file(&path, "cmux-omp-session-extension-marker")
}

// purpose: Install CMUX-compatible Amp plugin hooks.
// inputs: None; path is resolved from AMP_CONFIG_DIR, HOME, or dirs.
// returns/effects: Writes the generated plugin, refusing to replace non-Limux files.
fn install_amp_extension() -> Result<()> {
    let path = amp_extension_path();
    let source = amp_extension_source()?;
    install_marked_extension_file(&path, &source, "cmux-amp-session-extension-marker")
}

// purpose: Remove the installed Amp plugin only when it carries the owned marker.
// inputs: None.
// returns/effects: Deletes the generated plugin or leaves missing files untouched.
fn uninstall_amp_extension() -> Result<()> {
    let path = amp_extension_path();
    uninstall_marked_extension_file(&path, "cmux-amp-session-extension-marker")
}

// purpose: Install CMUX-compatible Hermes Agent YAML hooks and shell allowlist entries.
// inputs: None; config path is resolved from HERMES_HOME or HOME.
// returns/effects: Rewrites marked YAML hook blocks and approved command JSON.
fn install_hermes_agent_hooks() -> Result<()> {
    let config_dir = hermes_agent_config_dir();
    if !config_dir.exists() {
        bail!(
            "{} does not exist. Install Hermes Agent first.",
            config_dir.display()
        );
    }
    let events = hermes_agent_events()?;
    let config_path = config_dir.join("config.yaml");
    let existing = read_optional_text(&config_path)?;
    let updated = hermes_agent_hooks_installing(&existing, &events);
    if updated != existing {
        write_text_file(&config_path, &updated)?;
    }
    let allowlist_path = config_dir.join("shell-hooks-allowlist.json");
    let existing_allowlist = if allowlist_path.exists() {
        Some(
            fs::read(&allowlist_path)
                .with_context(|| format!("failed to read {}", allowlist_path.display()))?,
        )
    } else {
        None
    };
    let updated_allowlist =
        hermes_agent_allowlist_installing(existing_allowlist.as_deref(), &events)?;
    if existing_allowlist.as_deref() != Some(updated_allowlist.as_slice()) {
        if let Some(parent) = allowlist_path.parent() {
            fs::create_dir_all(parent)
                .with_context(|| format!("failed to create {}", parent.display()))?;
        }
        fs::write(&allowlist_path, updated_allowlist)
            .with_context(|| format!("failed to write {}", allowlist_path.display()))?;
    }
    Ok(())
}

// purpose: Remove Limux-owned Hermes Agent YAML hooks and shell allowlist entries.
// inputs: None; config path is resolved from HERMES_HOME or HOME.
// returns/effects: Preserves unrelated config and approvals.
fn uninstall_hermes_agent_hooks() -> Result<()> {
    let config_dir = hermes_agent_config_dir();
    let config_path = config_dir.join("config.yaml");
    if config_path.exists() {
        let existing = read_optional_text(&config_path)?;
        let updated = hermes_agent_hooks_uninstalling(&existing);
        if updated != existing {
            write_text_file(&config_path, &updated)?;
        }
    }
    let allowlist_path = config_dir.join("shell-hooks-allowlist.json");
    if allowlist_path.exists() {
        let existing = fs::read(&allowlist_path)
            .with_context(|| format!("failed to read {}", allowlist_path.display()))?;
        let events = hermes_agent_events()?;
        let updated = hermes_agent_allowlist_uninstalling(&existing, &events)?;
        if updated.as_slice() != existing.as_slice() {
            fs::write(&allowlist_path, updated)
                .with_context(|| format!("failed to write {}", allowlist_path.display()))?;
        }
    }
    Ok(())
}

// purpose: Write a generated extension file while protecting unrelated user files.
// inputs: Destination path, new source, and required ownership marker.
// returns/effects: Creates parent directories and writes source, or fails on unowned existing file.
fn install_marked_extension_file(path: &Path, source: &str, marker: &str) -> Result<()> {
    if path.exists() {
        let existing = fs::read_to_string(path)
            .with_context(|| format!("failed to read {}", path.display()))?;
        if !existing.is_empty() && !existing.contains(marker) {
            bail!(
                "{} exists and is not a Limux/CMUX extension",
                path.display()
            );
        }
        if existing == source {
            return Ok(());
        }
    }
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)
            .with_context(|| format!("failed to create {}", parent.display()))?;
    }
    fs::write(path, source).with_context(|| format!("failed to write {}", path.display()))
}

// purpose: Delete an owned generated extension file.
// inputs: Destination path and ownership marker.
// returns/effects: Removes the file only when the marker is present.
fn uninstall_marked_extension_file(path: &Path, marker: &str) -> Result<()> {
    if !path.exists() {
        return Ok(());
    }
    let existing =
        fs::read_to_string(path).with_context(|| format!("failed to read {}", path.display()))?;
    if !existing.contains(marker) {
        return Ok(());
    }
    fs::remove_file(path).with_context(|| format!("failed to remove {}", path.display()))
}

fn opencode_config_register_plugin(plugin_path: &Path) -> Result<()> {
    let config_path = opencode_config_path();
    let mut root = read_json_object(&config_path)?;
    let plugins = root
        .entry("plugin".to_string())
        .or_insert_with(|| Value::Array(Vec::new()));
    let plugins = plugins
        .as_array_mut()
        .ok_or_else(|| anyhow!("{} has non-array plugin field", config_path.display()))?;
    let plugin_str = plugin_path.to_string_lossy().into_owned();
    if !plugins.iter().any(|v| v.as_str() == Some(&plugin_str)) {
        plugins.push(Value::String(plugin_str));
    }
    write_json_object(&config_path, &root)
}

fn opencode_config_unregister_plugin() -> Result<()> {
    let config_path = opencode_config_path();
    if !config_path.exists() {
        return Ok(());
    }
    let plugin_path = opencode_plugin_path();
    let plugin_str = plugin_path.to_string_lossy().into_owned();
    let mut root = read_json_object(&config_path)?;
    if let Some(plugins) = root.get_mut("plugin").and_then(Value::as_array_mut) {
        plugins.retain(|v| v.as_str() != Some(&plugin_str));
    }
    write_json_object(&config_path, &root)
}

fn hook_command(agent: agent_hooks::AgentKind, event: &str) -> Result<String> {
    let limux_disable_var = hook_disable_var("LIMUX", agent);
    let cmux_disable_var = hook_disable_var("CMUX", agent);
    let limux_command = hook_cli_command()?;
    Ok(format!(
        "[ \"${{{limux_disable_var}:-}}\" != \"1\" ] && [ \"${{{cmux_disable_var}:-}}\" != \"1\" ] && {limux_command} --json hooks {} {} || echo '{{\"continue\":true,\"suppressOutput\":false}}'",
        agent.store_name(),
        event
    ))
}

// purpose: Build the shell command installed for a Feed hook event.
// inputs: Agent kind and raw agent hook event name.
// returns/effects: Returns the command string or fails if the Limux CLI path cannot be resolved.
fn feed_hook_command(agent: agent_hooks::AgentKind, event: &str) -> Result<String> {
    let limux_disable_var = hook_disable_var("LIMUX", agent);
    let cmux_disable_var = hook_disable_var("CMUX", agent);
    let limux_command = hook_cli_command()?;
    if agent == agent_hooks::AgentKind::Kiro {
        return Ok(format!(
            concat!(
                "if [ \"${{{limux_disable_var}:-}}\" != \"1\" ] && ",
                "[ \"${{{cmux_disable_var}:-}}\" != \"1\" ]; then ",
                "{{ {limux_command} hooks feed --source {source} --event {event}; ",
                "status=$?; [ \"$status\" -eq 2 ] && exit 2; ",
                "if [ \"$status\" -ne 0 ]; then echo '{{}}'; fi; }}; else echo '{{}}'; fi"
            ),
            limux_disable_var = limux_disable_var,
            cmux_disable_var = cmux_disable_var,
            limux_command = limux_command,
            source = agent.store_name(),
            event = shell_single_quote(event)
        ));
    }
    Ok(format!(
        "[ \"${{{limux_disable_var}:-}}\" != \"1\" ] && [ \"${{{cmux_disable_var}:-}}\" != \"1\" ] && {limux_command} hooks feed --source {} --event {} || echo '{{}}'",
        agent.store_name(),
        shell_single_quote(event)
    ))
}

fn hook_disable_var(prefix: &str, agent: agent_hooks::AgentKind) -> String {
    let name = agent.store_name().replace('-', "_").to_ascii_uppercase();
    format!("{prefix}_{name}_HOOKS_DISABLED")
}

fn hook_cli_command() -> Result<String> {
    let exe = env::current_exe().context("failed to resolve current executable")?;
    let file_name = exe
        .file_name()
        .and_then(|value| value.to_str())
        .unwrap_or("");
    if file_name == "limux-cli" {
        return Ok(shell_single_quote(&exe.to_string_lossy()));
    }
    Ok("limux".to_string())
}

fn opencode_plugin_cli_command() -> Result<String> {
    let exe = env::current_exe().context("failed to resolve current executable")?;
    let file_name = exe
        .file_name()
        .and_then(|value| value.to_str())
        .unwrap_or("");
    if file_name == "limux-cli" {
        return Ok(exe.to_string_lossy().to_string());
    }
    Ok("limux".to_string())
}

fn shell_single_quote(value: &str) -> String {
    format!("'{}'", value.replace('\'', "'\\''"))
}

fn hook_marker(agent: agent_hooks::AgentKind) -> &'static str {
    match agent {
        agent_hooks::AgentKind::Claude => "hooks claude",
        agent_hooks::AgentKind::Codex => "hooks codex",
        agent_hooks::AgentKind::Grok => "hooks grok",
        agent_hooks::AgentKind::OpenCode => "hooks opencode",
        agent_hooks::AgentKind::Cursor => "hooks cursor",
        agent_hooks::AgentKind::Kiro => "hooks kiro",
        agent_hooks::AgentKind::Antigravity => "hooks antigravity",
        agent_hooks::AgentKind::RovoDev => "hooks rovodev",
        agent_hooks::AgentKind::Pi => "hooks pi",
        agent_hooks::AgentKind::Omp => "hooks omp",
        agent_hooks::AgentKind::Amp => "hooks amp",
        agent_hooks::AgentKind::HermesAgent => "hooks hermes-agent",
        agent_hooks::AgentKind::Gemini => "hooks gemini",
        agent_hooks::AgentKind::Copilot => "hooks copilot",
        agent_hooks::AgentKind::CodeBuddy => "hooks codebuddy",
        agent_hooks::AgentKind::Factory => "hooks factory",
        agent_hooks::AgentKind::Qoder => "hooks qoder",
    }
}

// purpose: Build the stable marker used to identify installed Feed hook commands.
// inputs: Agent kind whose Feed hook command should be matched.
// returns/effects: Returns the marker substring without filesystem changes.
fn feed_hook_marker(agent: agent_hooks::AgentKind) -> Option<String> {
    Some(format!("hooks feed --source {}", agent.store_name()))
}

fn read_json_object(path: &Path) -> Result<Map<String, Value>> {
    if !path.exists() {
        return Ok(Map::new());
    }
    let raw =
        fs::read_to_string(path).with_context(|| format!("failed to read {}", path.display()))?;
    if raw.trim().is_empty() {
        return Ok(Map::new());
    }
    let value: Value = serde_json::from_str(&raw)
        .with_context(|| format!("failed to parse {}", path.display()))?;
    value
        .as_object()
        .cloned()
        .ok_or_else(|| anyhow!("{} must contain a JSON object", path.display()))
}

fn read_optional_text(path: &Path) -> Result<String> {
    if !path.exists() {
        return Ok(String::new());
    }
    fs::read_to_string(path).with_context(|| format!("failed to read {}", path.display()))
}

fn write_text_file(path: &Path, text: &str) -> Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)
            .with_context(|| format!("failed to create {}", parent.display()))?;
    }
    fs::write(path, text).with_context(|| format!("failed to write {}", path.display()))
}

fn write_json_object(path: &Path, object: &Map<String, Value>) -> Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)
            .with_context(|| format!("failed to create {}", parent.display()))?;
    }
    let temp = path.with_extension(format!("json.{}.tmp", std::process::id()));
    let encoded = serde_json::to_vec_pretty(object).context("failed to encode hook config")?;
    fs::write(&temp, encoded).with_context(|| format!("failed to write {}", temp.display()))?;
    fs::rename(&temp, path).with_context(|| format!("failed to replace {}", path.display()))
}

fn json_value_contains(value: &Value, needle: &str) -> bool {
    match value {
        Value::String(value) => value.contains(needle),
        Value::Array(values) => values
            .iter()
            .any(|value| json_value_contains(value, needle)),
        Value::Object(map) => map.values().any(|value| json_value_contains(value, needle)),
        _ => false,
    }
}

fn codex_hooks_path() -> PathBuf {
    env::var_os("CODEX_HOME")
        .map(PathBuf::from)
        .or_else(|| dirs::home_dir().map(|home| home.join(".codex")))
        .unwrap_or_else(|| PathBuf::from(".codex"))
        .join("hooks.json")
}

fn grok_hooks_path() -> PathBuf {
    env::var_os("GROK_HOME")
        .map(PathBuf::from)
        .or_else(|| dirs::home_dir().map(|home| home.join(".grok")))
        .unwrap_or_else(|| PathBuf::from(".grok"))
        .join("hooks/cmux-session.json")
}

fn claude_settings_path() -> PathBuf {
    env::var_os("CLAUDE_CONFIG_DIR")
        .map(PathBuf::from)
        .or_else(|| dirs::home_dir().map(|home| home.join(".claude")))
        .unwrap_or_else(|| PathBuf::from(".claude"))
        .join("settings.json")
}

fn cursor_hooks_path() -> PathBuf {
    dirs::home_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join(".cursor/hooks.json")
}

fn kiro_agent_path() -> PathBuf {
    if let Some(home) = env::var_os("KIRO_HOME") {
        return PathBuf::from(home).join("agents/cmux.json");
    }
    dirs::home_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join(".kiro/agents/cmux.json")
}

fn antigravity_hooks_path() -> PathBuf {
    dirs::home_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join(".gemini/config/hooks.json")
}

fn rovodev_config_path() -> PathBuf {
    dirs::home_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join(".rovodev/config.yml")
}

fn pi_extension_path() -> PathBuf {
    resolved_pi_agent_directory().join("extensions/cmux-session.ts")
}

fn resolved_pi_agent_directory() -> PathBuf {
    if let Some(root) = env::var_os("PI_CODING_AGENT_DIR") {
        return expand_tilde_path(PathBuf::from(root));
    }
    let config_dir = env::var_os("PI_CONFIG_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(".pi"));
    let expanded = expand_tilde_path(config_dir);
    if expanded.is_absolute() {
        return expanded.join("agent");
    }
    dirs::home_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join(expanded)
        .join("agent")
}

fn omp_extension_path() -> PathBuf {
    resolved_omp_agent_directory().join("extensions/cmux-omp-session.ts")
}

fn amp_extension_path() -> PathBuf {
    amp_config_dir().join("plugins/cmux-session.ts")
}

fn amp_config_dir() -> PathBuf {
    env::var_os("AMP_CONFIG_DIR")
        .map(PathBuf::from)
        .or_else(|| dirs::home_dir().map(|home| home.join(".config/amp")))
        .unwrap_or_else(|| PathBuf::from(".config/amp"))
}

fn hermes_agent_config_dir() -> PathBuf {
    env::var_os("HERMES_HOME")
        .map(PathBuf::from)
        .map(expand_tilde_path)
        .or_else(|| dirs::home_dir().map(|home| home.join(".hermes")))
        .unwrap_or_else(|| PathBuf::from(".hermes"))
}

fn resolved_omp_agent_directory() -> PathBuf {
    if let Some(root) = env::var_os("PI_CODING_AGENT_DIR") {
        return expand_tilde_path(PathBuf::from(root));
    }
    let config_dir = env::var_os("PI_CONFIG_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(".omp"));
    let expanded = expand_tilde_path(config_dir);
    if expanded.is_absolute() {
        return expanded.join("agent");
    }
    dirs::home_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join(expanded)
        .join("agent")
}

fn expand_tilde_path(path: PathBuf) -> PathBuf {
    let Some(raw) = path.to_str() else {
        return path;
    };
    if raw == "~" {
        return dirs::home_dir().unwrap_or(path);
    }
    if let Some(rest) = raw.strip_prefix("~/") {
        if let Some(home) = dirs::home_dir() {
            return home.join(rest);
        }
    }
    path
}

fn gemini_settings_path() -> PathBuf {
    dirs::home_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join(".gemini/settings.json")
}

fn copilot_config_path() -> PathBuf {
    env::var_os("COPILOT_HOME")
        .map(PathBuf::from)
        .or_else(|| dirs::home_dir().map(|home| home.join(".copilot")))
        .unwrap_or_else(|| PathBuf::from(".copilot"))
        .join("config.json")
}

fn codebuddy_settings_path() -> PathBuf {
    env::var_os("CODEBUDDY_CONFIG_DIR")
        .map(PathBuf::from)
        .or_else(|| dirs::home_dir().map(|home| home.join(".codebuddy")))
        .unwrap_or_else(|| PathBuf::from(".codebuddy"))
        .join("settings.json")
}

fn factory_settings_path() -> PathBuf {
    dirs::home_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join(".factory/settings.json")
}

fn qoder_settings_path() -> PathBuf {
    env::var_os("QODER_CONFIG_DIR")
        .map(PathBuf::from)
        .or_else(|| dirs::home_dir().map(|home| home.join(".qoder")))
        .unwrap_or_else(|| PathBuf::from(".qoder"))
        .join("settings.json")
}

fn opencode_config_dir() -> PathBuf {
    env::var_os("OPENCODE_CONFIG_DIR")
        .map(PathBuf::from)
        .or_else(|| dirs::home_dir().map(|home| home.join(".config/opencode")))
        .unwrap_or_else(|| PathBuf::from(".config/opencode"))
}

fn opencode_plugin_path() -> PathBuf {
    opencode_config_dir().join("plugins/limux-session.js")
}

fn opencode_config_path() -> PathBuf {
    opencode_config_dir().join("config.json")
}

fn opencode_plugin_source() -> Result<String> {
    opencode_plugin_source_with_command(&opencode_plugin_cli_command()?)
}

fn opencode_plugin_source_with_command(limux_command: &str) -> Result<String> {
    let limux_command_json =
        serde_json::to_string(limux_command).context("failed to encode OpenCode hook command")?;
    Ok(
        r#"// limux-opencode-session-plugin v2
// Installed by `limux hooks opencode install`. Do not edit manually.

import { spawnSync } from "node:child_process";
import { appendFileSync, mkdirSync } from "node:fs";
import { join } from "node:path";

const LIMUX_COMMAND = __LIMUX_COMMAND__;

function debug(outcome, details = {}) {
  if (process.env.LIMUX_OPENCODE_HOOK_DEBUG !== "1" && outcome !== "spawn_failed") return;
  try {
    const dir = process.env.LIMUX_AGENT_HOOK_STATE_DIR || (process.env.XDG_STATE_HOME ? join(process.env.XDG_STATE_HOME, "limux") : join(process.env.HOME || ".", ".local/state/limux"));
    mkdirSync(dir, { recursive: true });
    appendFileSync(join(dir, "opencode-plugin-debug.jsonl"), JSON.stringify({
      time: Date.now() / 1000,
      outcome,
      details
    }) + "\n");
  } catch (_) {}
}

function firstString(...values) {
  for (const value of values) {
    if (typeof value === "string" && value.trim().length > 0) return value.trim();
  }
  return null;
}

function props(event) {
  return (event && typeof event === "object" && event.properties) || {};
}

function data(event) {
  return (event && typeof event === "object" && event.data) || {};
}

function info(event) {
  const p = props(event);
  const d = data(event);
  return (p.info && typeof p.info === "object" && p.info) || (d.info && typeof d.info === "object" && d.info) || {};
}

function eventType(event) {
  const raw = firstString(event && event.type, event && event.name);
  if (!raw) return null;
  if (raw === "sync") return firstString(event && event.name);
  return raw.endsWith(".1") ? raw.slice(0, -2) : raw;
}

function sessionId(event) {
  const p = props(event);
  const d = data(event);
  const i = info(event);
  return firstString(p.sessionID, p.sessionId, p.session_id, d.sessionID, d.sessionId, d.session_id, i.id, event && event.sessionID, event && event.sessionId);
}

function cwd(ctx, event) {
  const p = props(event);
  const d = data(event);
  const i = info(event);
  return firstString(p.cwd, p.directory, d.cwd, d.directory, i.directory, i.path, ctx && ctx.directory, process.cwd());
}

function launchExecutable() {
  return firstString(process.env.LIMUX_OPENCODE_EXECUTABLE, "opencode");
}

function send(kind, ctx, event) {
  if (process.env.LIMUX_OPENCODE_HOOKS_DISABLED === "1") {
    debug("skip_disabled", { kind });
    return;
  }
  if (!process.env.LIMUX_SURFACE_ID) {
    debug("skip_missing_surface", { kind, type: eventType(event), hasWorkspace: !!process.env.LIMUX_WORKSPACE_ID });
    return;
  }
  const sid = sessionId(event);
  if (!sid) {
    debug("skip_missing_session", { kind, type: eventType(event), keys: Object.keys(event || {}) });
    return;
  }
  const type = eventType(event);
  const payload = {
    session_id: sid,
    cwd: cwd(ctx, event),
    hook_event_name: type,
    event: type
  };
  try {
    const command = process.env.LIMUX_BIN || LIMUX_COMMAND;
    const result = spawnSync(command, ["hooks", "opencode", kind], {
      input: JSON.stringify(payload),
      encoding: "utf8",
      stdio: ["pipe", "ignore", "ignore"],
      timeout: 5000,
      env: {
        ...process.env,
        LIMUX_AGENT_LAUNCH_ARGV: launchExecutable(),
        LIMUX_AGENT_LAUNCH_EXECUTABLE: launchExecutable(),
        LIMUX_AGENT_LAUNCH_CWD: cwd(ctx, event)
      }
    });
    debug("spawned", { kind, type, status: result.status, error: result.error && String(result.error), command });
  } catch (error) {
    debug("spawn_failed", { kind, type, error: String(error) });
  }
}

const limuxSessionRestore = async (ctx) => {
  debug("plugin_started", { directory: ctx && ctx.directory, hasSurface: !!process.env.LIMUX_SURFACE_ID, hasWorkspace: !!process.env.LIMUX_WORKSPACE_ID });
  return {
    event: async ({ event }) => {
    const type = eventType(event);
    debug("event", { type, rawType: event && event.type, rawName: event && event.name });
    if (!type) return;
    if (type === "session.created") send("session-start", ctx, event);
    if (type === "session.idle" || type === "session.updated" || type === "session.status" || type === "session.compacted") send("prompt-submit", ctx, event);
    if (type === "session.error") send("session-end", ctx, event);
    if (type === "session.deleted") send("cleanup", ctx, event);
    }
  };
};

export const LimuxSessionRestore = limuxSessionRestore;
export default limuxSessionRestore;
"#
        .replace("__LIMUX_COMMAND__", &limux_command_json),
    )
}

fn pi_extension_source() -> Result<String> {
    pi_extension_source_with_command(&opencode_plugin_cli_command()?)
}

fn pi_extension_source_with_command(limux_command: &str) -> Result<String> {
    let limux_command_json =
        serde_json::to_string(limux_command).context("failed to encode Pi hook command")?;
    Ok(
        r#"// cmux-pi-session-extension-marker v1
// Bridges Pi lifecycle, tool telemetry, notifications, and resume bindings into Limux.
// Installed by `limux hooks pi install` or `limux hooks setup`.
// DO NOT EDIT MANUALLY. Limux upgrades this file in place.

import { spawn, spawnSync } from "node:child_process";
import * as fs from "node:fs";
import * as path from "node:path";
import type { ExtensionAPI, ExtensionContext } from "@earendil-works/pi-coding-agent";

const LIMUX_COMMAND = __LIMUX_COMMAND__;
const states = new Map();

function firstString(...values) {
  for (const value of values) {
    if (typeof value === "string" && value.trim().length > 0) return value.trim();
  }
  return null;
}

function objectValue(value, keys) {
  if (!value || typeof value !== "object") return undefined;
  for (const key of keys) {
    if (value[key] !== undefined && value[key] !== null) return value[key];
  }
  return undefined;
}

function warn(message, details = {}) {
  console.warn(JSON.stringify({ source: "limux-pi-extension", level: "warning", message, ...details }));
}

function resolveExecutable(name) {
  const pathEnv = process.env.PATH || "";
  for (const dir of pathEnv.split(path.delimiter)) {
    if (!dir) continue;
    const candidate = path.join(dir, name);
    try {
      fs.accessSync(candidate, fs.constants.X_OK);
      if (fs.statSync(candidate).isFile()) return candidate;
    } catch (_) {
      continue;
    }
  }
  return name;
}

function looksLikePiScript(value) {
  const normalized = String(value).replaceAll("\\", "/").toLowerCase();
  const base = path.basename(normalized);
  return normalized.includes("/@earendil-works/pi-coding-agent/") ||
    normalized.includes("/@mariozechner/pi-coding-agent/") ||
    normalized.includes("/packages/coding-agent/") ||
    ((base === "cli.js" || base === "cli.ts") && normalized.includes("coding-agent"));
}

function normalizedLaunchArgv() {
  const raw = Array.isArray(process.argv) ? process.argv.map((value) => String(value)) : [];
  if (raw.length === 0) return [resolveExecutable("pi")];
  const first = path.basename(raw[0]).toLowerCase();
  if (first === "pi" || first === "pi-coding-agent") return raw;
  if (raw.length > 1 && looksLikePiScript(raw[1])) return [resolveExecutable("pi"), ...raw.slice(2)];
  return [resolveExecutable("pi"), ...raw.slice(1)];
}

function base64NulSeparated(values) {
  const bytes = [];
  for (const value of values) {
    bytes.push(Buffer.from(String(value), "utf8"));
    bytes.push(Buffer.from([0]));
  }
  return Buffer.concat(bytes).toString("base64");
}

function secretLikeEnvKey(key) {
  return /(TOKEN|SECRET|PASSWORD|PASSWD|API[_-]?KEY|ACCESS[_-]?KEY|PRIVATE[_-]?KEY|CREDENTIAL|AUTHORIZATION|COOKIE)/i.test(key);
}

function shouldPreserveEnvKey(key) {
  if (key.startsWith("LIMUX_AGENT_LAUNCH_") || key.startsWith("CMUX_AGENT_LAUNCH_")) return !secretLikeEnvKey(key);
  if (key === "LIMUX_SURFACE_ID" || key === "LIMUX_WORKSPACE_ID" || key === "LIMUX_SOCKET") return true;
  if (key === "CMUX_SURFACE_ID" || key === "CMUX_WORKSPACE_ID" || key === "CMUX_SOCKET_PATH") return true;
  if (key === "LIMUX_BIN" || key === "CMUX_PI_CMUX_BIN") return true;
  if (key === "LIMUX_PI_HOOKS_DISABLED" || key === "CMUX_PI_HOOKS_DISABLED") return true;
  if (key === "LIMUX_AGENT_HOOK_STATE_DIR" || key === "CMUX_AGENT_HOOK_STATE_DIR") return true;
  if (key === "PI_CODING_AGENT_DIR" || key === "PI_CONFIG_DIR") return true;
  if (key.startsWith("PI_CODING_AGENT_") || key.startsWith("PI_")) return !secretLikeEnvKey(key);
  if (key === "PATH" || key === "HOME" || key === "PWD" || key === "SHELL") return true;
  if (key === "USER" || key === "LOGNAME" || key === "TMPDIR" || key === "TZ") return true;
  if (key === "LANG" || key.startsWith("LC_")) return true;
  if (key === "TERM" || key === "COLORTERM" || key === "SSH_AUTH_SOCK") return true;
  if (key === "NODE_ENV" || key === "NODE_OPTIONS" || key === "NODE_PATH") return true;
  return false;
}

function hookEnvironment(cwd, includeSocketPassword = false) {
  const env = {};
  for (const [key, value] of Object.entries(process.env)) {
    if (value !== undefined && shouldPreserveEnvKey(key)) env[key] = value;
  }
  if (includeSocketPassword && process.env.LIMUX_SOCKET_PASSWORD) {
    env.LIMUX_SOCKET_PASSWORD = process.env.LIMUX_SOCKET_PASSWORD;
  }
  if (includeSocketPassword && process.env.CMUX_SOCKET_PASSWORD) {
    env.CMUX_SOCKET_PASSWORD = process.env.CMUX_SOCKET_PASSWORD;
  }
  if (!env.LIMUX_AGENT_LAUNCH_ARGV_B64 && !env.CMUX_AGENT_LAUNCH_ARGV_B64) {
    const argv = normalizedLaunchArgv();
    const executable = argv[0] || resolveExecutable("pi");
    env.LIMUX_AGENT_LAUNCH_EXECUTABLE = executable;
    env.LIMUX_AGENT_LAUNCH_ARGV_B64 = base64NulSeparated(argv);
    env.LIMUX_AGENT_LAUNCH_CWD = cwd || process.cwd();
    env.CMUX_AGENT_LAUNCH_KIND = "pi";
    env.CMUX_AGENT_LAUNCH_EXECUTABLE = executable;
    env.CMUX_AGENT_LAUNCH_ARGV_B64 = env.LIMUX_AGENT_LAUNCH_ARGV_B64;
    env.CMUX_AGENT_LAUNCH_CWD = env.LIMUX_AGENT_LAUNCH_CWD;
  }
  return env;
}

function limuxExecutable() {
  return process.env.LIMUX_BIN || process.env.CMUX_PI_CMUX_BIN || LIMUX_COMMAND;
}

function eventName(subcommand) {
  if (subcommand === "session-start") return "SessionStart";
  if (subcommand === "prompt-submit") return "UserPromptSubmit";
  if (subcommand === "stop") return "Stop";
  if (subcommand === "notification") return "Notification";
  return subcommand;
}

function sessionIdFrom(ctx) {
  return firstString(ctx && ctx.sessionManager && ctx.sessionManager.getSessionId && ctx.sessionManager.getSessionId());
}

function cwdFrom(ctx) {
  return firstString(ctx && ctx.cwd, process.cwd()) || process.cwd();
}

function stateFor(sessionId) {
  let state = states.get(sessionId);
  if (!state) {
    state = { nextTurn: 0, activeTurnId: null, stopped: false };
    states.set(sessionId, state);
  }
  return state;
}

function eventTurnId(event) {
  return firstString(objectValue(event, ["turn_id", "turnId", "turnID"]));
}

function beginTurn(sessionId, event) {
  const state = stateFor(sessionId);
  const explicit = eventTurnId(event);
  const turnId = explicit || `${sessionId}:turn-${state.nextTurn + 1}`;
  if (!explicit) state.nextTurn += 1;
  state.activeTurnId = turnId;
  state.stopped = false;
  return turnId;
}

function currentTurnId(sessionId, event) {
  const state = stateFor(sessionId);
  const explicit = eventTurnId(event);
  if (explicit) return explicit;
  if (state.activeTurnId) return state.activeTurnId;
  state.nextTurn += 1;
  return `${sessionId}:turn-${state.nextTurn}`;
}

function finishTurn(sessionId, event) {
  const state = stateFor(sessionId);
  const turnId = eventTurnId(event) || state.activeTurnId || `${sessionId}:turn-${state.nextTurn + 1}`;
  if (!eventTurnId(event) && !state.activeTurnId) state.nextTurn += 1;
  state.activeTurnId = null;
  state.stopped = true;
  return turnId;
}

function sendHook(subcommand, ctx, extra = {}) {
  if (process.env.LIMUX_PI_HOOKS_DISABLED === "1" || process.env.CMUX_PI_HOOKS_DISABLED === "1") return true;
  if (!process.env.LIMUX_SURFACE_ID && !process.env.CMUX_SURFACE_ID) return true;
  const sessionId = sessionIdFrom(ctx);
  if (!sessionId) return true;
  const cwd = cwdFrom(ctx);
  const payload = {
    session_id: sessionId,
    cwd,
    hook_event_name: eventName(subcommand),
    event: eventName(subcommand),
    ...extra,
  };
  const result = spawnSync(limuxExecutable(), ["--json", "hooks", "pi", subcommand], {
    input: JSON.stringify(payload),
    encoding: "utf8",
    stdio: ["pipe", "ignore", "pipe"],
    timeout: 5000,
    env: hookEnvironment(cwd, true),
  });
  if (result.error || result.status !== 0) {
    warn("limux hook command failed", { subcommand, status: result.status, error: String(result.error || "") });
    return false;
  }
  return true;
}

function surfaceTargetArgs() {
  const surfaceId = firstString(process.env.LIMUX_SURFACE_ID, process.env.CMUX_SURFACE_ID);
  if (!surfaceId) return null;
  const args = [];
  const workspaceId = firstString(process.env.LIMUX_WORKSPACE_ID, process.env.CMUX_WORKSPACE_ID);
  if (workspaceId) args.push("--workspace", workspaceId);
  args.push("--surface", surfaceId);
  return args;
}

function sanitizedResumeArgv(sessionId) {
  const raw = normalizedLaunchArgv();
  const out = [raw[0] || resolveExecutable("pi"), "--session", sessionId];
  const withValue = new Set([
    "--model", "-m", "--thinking", "--provider", "--extension", "-e", "--skill",
    "--mcp-config", "--permission-mode", "--session-dir", "--config", "--profile",
    "--system-prompt", "--append-system-prompt", "--cwd", "--dir", "--trust", "--sandbox",
  ]);
  const withoutValue = new Set(["--no-color", "--dangerously-skip-permissions", "--yolo"]);
  const drop = new Set(["--session", "-s", "--resume", "--fork", "--api-key", "--prompt", "--print"]);
  for (let index = 1; index < raw.length; index += 1) {
    const arg = raw[index];
    if (drop.has(arg)) {
      if (index + 1 < raw.length && !raw[index + 1].startsWith("-")) index += 1;
      continue;
    }
    if (arg.startsWith("--session=") || arg.startsWith("--resume=") || arg.startsWith("--fork=")) continue;
    if (arg.startsWith("--api-key=") || arg.startsWith("--prompt=")) continue;
    if (withValue.has(arg)) {
      out.push(arg);
      if (index + 1 < raw.length) {
        out.push(raw[index + 1]);
        index += 1;
      }
      continue;
    }
    if ([...withValue].some((option) => arg.startsWith(`${option}=`)) || withoutValue.has(arg)) out.push(arg);
  }
  return out;
}

function runLimux(args, cwd, input) {
  const result = spawnSync(limuxExecutable(), args, {
    cwd,
    input,
    encoding: "utf8",
    stdio: ["pipe", "pipe", "pipe"],
    timeout: 5000,
    env: hookEnvironment(cwd, true),
  });
  return { ok: !result.error && result.status === 0, stdout: result.stdout || "", status: result.status, error: result.error };
}

function ensureResumeBinding(ctx, sessionId, cwd) {
  const target = surfaceTargetArgs();
  if (!target) return;
  const result = runLimux([
    "--json", "surface", "resume", "set", ...target,
    "--name", "Pi", "--kind", "pi", "--checkpoint-id", sessionId,
    "--source", "agent-hook", "--cwd", cwd, "--", ...sanitizedResumeArgv(sessionId),
  ], cwd);
  if (!result.ok) warn("failed to set Pi resume binding", { status: result.status, error: String(result.error || "") });
}

function clearResumeBinding(ctx, sessionId, cwd) {
  const target = surfaceTargetArgs();
  if (!target) return true;
  const result = runLimux([
    "--json", "surface", "resume", "clear", ...target,
    "--checkpoint-id", sessionId, "--source", "agent-hook",
  ], cwd);
  if (!result.ok) warn("failed to clear Pi resume binding", { status: result.status, error: String(result.error || "") });
  return result.ok;
}

function sendFeed(eventName, ctx, event, extra = {}) {
  if (process.env.LIMUX_PI_HOOKS_DISABLED === "1" || process.env.CMUX_PI_HOOKS_DISABLED === "1") return;
  if (!process.env.LIMUX_SURFACE_ID && !process.env.CMUX_SURFACE_ID) return;
  const sessionId = sessionIdFrom(ctx);
  if (!sessionId) return;
  const cwd = cwdFrom(ctx);
  const payload = {
    session_id: sessionId,
    cwd,
    hook_event_name: eventName,
    event: eventName,
    turn_id: currentTurnId(sessionId, event),
    tool_call_id: firstString(objectValue(event, ["toolCallId", "tool_call_id", "id"])),
    tool_name: firstString(objectValue(event, ["toolName", "tool_name", "name"])),
    tool_input: objectValue(event, ["args", "input"]),
    ...extra,
  };
  try {
    const child = spawn(limuxExecutable(), ["hooks", "feed", "--source", "pi", "--event", eventName], {
      env: hookEnvironment(cwd, true),
      stdio: ["pipe", "ignore", "ignore"],
      detached: true,
    });
    child.on("error", (error) => warn("failed to spawn Pi Feed hook", { error: String(error) }));
    child.stdin.on("error", (error) => warn("failed to write Pi Feed payload", { error: String(error) }));
    child.stdin.end(JSON.stringify(payload));
    child.unref();
  } catch (error) {
    warn("failed to start Pi Feed hook", { error: String(error) });
  }
}

function textFromContent(content) {
  if (typeof content === "string") return content;
  if (!Array.isArray(content)) return null;
  const parts = [];
  for (const block of content) {
    if (block && typeof block === "object" && block.type === "text" && typeof block.text === "string") parts.push(block.text);
  }
  return parts.join("\n") || null;
}

function lastAssistantMessage(event) {
  const messages = Array.isArray(event && event.messages) ? event.messages : [];
  for (let index = messages.length - 1; index >= 0; index -= 1) {
    const message = messages[index];
    if (!message || typeof message !== "object" || message.role !== "assistant") continue;
    const text = firstString(textFromContent(message.content));
    if (text) return text;
  }
  return undefined;
}

export default function limuxPiSessionExtension(pi: ExtensionAPI) {
  pi.on("session_start", async (_event: unknown, ctx: ExtensionContext) => {
    const sessionId = sessionIdFrom(ctx);
    const cwd = cwdFrom(ctx);
    if (sessionId) stateFor(sessionId).stopped = false;
    if (sendHook("session-start", ctx) && sessionId) ensureResumeBinding(ctx, sessionId, cwd);
  });
  pi.on("before_agent_start", async (event: any, ctx: ExtensionContext) => {
    const sessionId = sessionIdFrom(ctx);
    const turnId = sessionId ? beginTurn(sessionId, event) : undefined;
    sendHook("prompt-submit", ctx, { prompt: event && event.prompt, turn_id: turnId });
  });
  pi.on("tool_execution_start", async (event: unknown, ctx: ExtensionContext) => sendFeed("PreToolUse", ctx, event));
  pi.on("tool_execution_end", async (event: unknown, ctx: ExtensionContext) => {
    sendFeed("PostToolUse", ctx, event, {
      tool_result: objectValue(event, ["result", "details", "content"]),
      is_error: objectValue(event, ["isError", "is_error"]),
    });
  });
  pi.on("agent_end", async (event: unknown, ctx: ExtensionContext) => {
    const sessionId = sessionIdFrom(ctx);
    const turnId = sessionId ? finishTurn(sessionId, event) : undefined;
    const message = lastAssistantMessage(event);
    const routed = sendHook("notification", ctx, {
      message: message || "Task completed",
      turn_id: turnId,
      notification: { type: firstString(objectValue(event, ["stopReason", "reason", "terminationReason"])) || "completed" },
    });
    sendHook("stop", ctx, { last_assistant_message: message, turn_id: turnId, limux_notification_routed: routed });
  });
  pi.on("session_shutdown", async (event: unknown, ctx: ExtensionContext) => {
    const sessionId = sessionIdFrom(ctx);
    if (!sessionId) return;
    const state = stateFor(sessionId);
    const cwd = cwdFrom(ctx);
    if (!state.stopped) {
      const turnId = finishTurn(sessionId, event);
      sendHook("stop", ctx, { turn_id: turnId, terminationReason: firstString(objectValue(event, ["reason"])) || "session_shutdown" });
    }
    if (clearResumeBinding(ctx, sessionId, cwd)) states.delete(sessionId);
  });
}
"#
        .replace("__LIMUX_COMMAND__", &limux_command_json),
    )
}

fn omp_extension_source() -> Result<String> {
    omp_extension_source_with_command(&opencode_plugin_cli_command()?)
}

fn amp_extension_source() -> Result<String> {
    amp_extension_source_with_command(&opencode_plugin_cli_command()?)
}

fn amp_extension_source_with_command(limux_command: &str) -> Result<String> {
    let limux_command_json =
        serde_json::to_string(limux_command).context("failed to encode Amp hook command")?;
    Ok(
        r##"// cmux-amp-session-extension-marker v1
// Bridges Amp lifecycle and status events into Limux's CMUX-compatible session store.
// Installed by `limux hooks amp install` or `limux hooks setup`.
// DO NOT EDIT MANUALLY. Limux upgrades this file in place.
// @i-know-the-amp-plugin-api-is-wip-and-very-experimental-right-now

import { spawn } from "node:child_process";
import * as fs from "node:fs";
import * as path from "node:path";
import type {
  PluginAPI,
  AgentEndEvent,
  AgentStartEvent,
  SessionStartEvent,
  ToolCallEvent,
  ToolResultEvent,
} from "@ampcode/plugin";

const LIMUX_COMMAND = __LIMUX_COMMAND__;
const STATUS_KEY = "amp";
const LOG_SOURCE = "amp";
const COLOR = {
  idle: "#adb5bd",
  thinking: "#ffffff",
  active: "#ffd700",
  done: "#50fa7b",
  error: "#ff5555",
  interrupted: "#ffb86c",
} as const;

function firstString(...values: unknown[]): string | null {
  for (const value of values) {
    if (typeof value === "string" && value.trim().length > 0) return value.trim();
  }
  return null;
}

function resolveExecutable(name: string): string {
  const pathEnv = process.env.PATH || "";
  for (const dir of pathEnv.split(path.delimiter)) {
    if (!dir) continue;
    const candidate = path.join(dir, name);
    try {
      fs.accessSync(candidate, fs.constants.X_OK);
      if (fs.statSync(candidate).isFile()) return candidate;
    } catch (_) {
      continue;
    }
  }
  return name;
}

function looksLikeAmpScript(value: string): boolean {
  const normalized = value.replaceAll("\\", "/").toLowerCase();
  const base = path.basename(normalized);
  return normalized.includes("/@ampcode/") || (base === "cli.js" && normalized.includes("amp"));
}

function looksLikeJavaScriptRuntime(value: string): boolean {
  const base = path.basename(value).toLowerCase();
  return base === "node" || base === "bun" || base === "deno" || base === "tsx" || base === "ts-node";
}

function normalizedLaunchArgv(): string[] {
  const raw = Array.isArray(process.argv) ? process.argv.map((value) => String(value)) : [];
  if (raw.length === 0) return [resolveExecutable("amp")];
  if (path.basename(raw[0]).toLowerCase() === "amp") return raw;
  if (raw.length > 1 && (looksLikeAmpScript(raw[1]) || looksLikeJavaScriptRuntime(raw[0]))) {
    return [resolveExecutable("amp"), ...raw.slice(2)];
  }
  return [resolveExecutable("amp")];
}

function base64NulSeparated(values: string[]): string {
  const bytes: Buffer[] = [];
  for (const value of values) {
    bytes.push(Buffer.from(String(value), "utf8"));
    bytes.push(Buffer.from([0]));
  }
  return Buffer.concat(bytes).toString("base64");
}

function sanitizedEnvironment(cwd: string, includeLaunch: boolean): NodeJS.ProcessEnv {
  const env: NodeJS.ProcessEnv = { ...process.env };
  delete env.AMP_API_KEY;
  if (includeLaunch && !env.LIMUX_AGENT_LAUNCH_ARGV_B64 && !env.CMUX_AGENT_LAUNCH_ARGV_B64) {
    const argv = normalizedLaunchArgv();
    const executable = argv[0] || resolveExecutable("amp");
    env.LIMUX_AGENT_LAUNCH_EXECUTABLE = executable;
    env.LIMUX_AGENT_LAUNCH_ARGV_B64 = base64NulSeparated(argv);
    env.LIMUX_AGENT_LAUNCH_CWD = cwd || process.cwd();
    env.CMUX_AGENT_LAUNCH_KIND = "amp";
    env.CMUX_AGENT_LAUNCH_EXECUTABLE = executable;
    env.CMUX_AGENT_LAUNCH_ARGV_B64 = env.LIMUX_AGENT_LAUNCH_ARGV_B64;
    env.CMUX_AGENT_LAUNCH_CWD = env.LIMUX_AGENT_LAUNCH_CWD;
  }
  return env;
}

function eventName(subcommand: string): string {
  if (subcommand === "session-start") return "SessionStart";
  if (subcommand === "prompt-submit") return "UserPromptSubmit";
  if (subcommand === "stop") return "Stop";
  return subcommand;
}

function limuxExecutable(): string {
  return process.env.LIMUX_AMP_LIMUX_BIN || process.env.CMUX_AMP_CMUX_BIN || process.env.LIMUX_BIN || LIMUX_COMMAND;
}

function hasSurface(): boolean {
  return !!(process.env.LIMUX_SURFACE_ID || process.env.CMUX_SURFACE_ID);
}

function threadIdFrom(event?: { thread?: { id?: string } }, ctx?: { thread?: { id?: string } }): string | null {
  return firstString(event?.thread?.id, ctx?.thread?.id);
}

function sendHook(subcommand: string, sessionId: string | null, cwd: string): void {
  if (process.env.LIMUX_AMP_HOOKS_DISABLED === "1" || process.env.CMUX_AMP_HOOKS_DISABLED === "1") return;
  if (!hasSurface() || !sessionId) return;
  const payload = {
    session_id: sessionId,
    cwd,
    hook_event_name: eventName(subcommand),
    event: eventName(subcommand),
  };
  try {
    const child = spawn(limuxExecutable(), ["hooks", "amp", subcommand], {
      env: sanitizedEnvironment(cwd, true),
      stdio: ["pipe", "ignore", "ignore"],
      detached: true,
    });
    child.on("error", () => {});
    child.stdin.on("error", () => {});
    child.stdin.end(JSON.stringify(payload));
    child.unref();
  } catch (_) {}
}

function workspaceArgs(): string[] {
  const workspace = firstString(process.env.LIMUX_WORKSPACE_ID, process.env.CMUX_WORKSPACE_ID);
  return workspace ? ["--workspace", workspace] : [];
}

function runLimux(args: string[]): void {
  if (process.env.LIMUX_AMP_HOOKS_DISABLED === "1" || process.env.CMUX_AMP_HOOKS_DISABLED === "1") return;
  if (!hasSurface()) return;
  try {
    const child = spawn(limuxExecutable(), args, {
      env: sanitizedEnvironment(process.cwd(), false),
      stdio: ["ignore", "ignore", "ignore"],
      detached: true,
    });
    child.on("error", () => {});
    child.unref();
  } catch (_) {}
}

function setStatus(label: string, icon: string, color: string): void {
  runLimux(["set-status", STATUS_KEY, label, "--icon", icon, "--color", color, ...workspaceArgs()]);
}

function clearStatus(): void {
  runLimux(["clear-status", STATUS_KEY, ...workspaceArgs()]);
}

function wsLog(message: string, level: string = "info"): void {
  runLimux(["log", "--level", level, "--source", LOG_SOURCE, ...workspaceArgs(), "--", message]);
}

function toolLabel(tool: string): string {
  switch (tool) {
    case "Read": return "reading";
    case "edit_file":
    case "create_file": return "editing";
    case "Bash": return "running";
    case "Grep":
    case "finder":
    case "glob": return "searching";
    case "Task": return "subagent";
    case "oracle": return "consulting oracle";
    case "web_search":
    case "read_web_page": return "browsing";
    case "todo_write":
    case "todo_read": return "planning";
    default: return tool;
  }
}

function toolIcon(tool: string): string {
  switch (tool) {
    case "Read": return "eye";
    case "edit_file":
    case "create_file": return "pencil";
    case "Bash": return "terminal";
    case "Grep":
    case "finder":
    case "glob": return "magnifyingglass";
    case "Task": return "person.2";
    case "oracle": return "sparkles";
    case "web_search":
    case "read_web_page": return "globe";
    case "todo_write":
    case "todo_read": return "checklist";
    default: return "hammer";
  }
}

function truncate(value: string, max: number): string {
  return value.length > max ? value.slice(0, max - 1) + "..." : value;
}

function basename(value: string): string {
  const match = value.match(/[^/]+$/);
  return match ? match[0] : value;
}

function detailedToolStatus(event: ToolCallEvent, helpers: unknown): { label: string; icon: string } {
  const baseLabel = toolLabel(event.tool);
  const icon = toolIcon(event.tool);
  const typed = helpers as {
    shellCommandFromToolCall?: (event: ToolCallEvent) => { command: string } | null;
    filesModifiedByToolCall?: (event: ToolCallEvent) => string[] | null;
    filePathFromURI?: (uri: string) => string;
  } | undefined;
  try {
    const shell = typed?.shellCommandFromToolCall?.(event);
    if (shell && typeof shell.command === "string") {
      return { label: `${baseLabel}: ${truncate(shell.command.replace(/\s+/g, " ").trim(), 32)}`, icon };
    }
  } catch (_) {}
  try {
    const files = typed?.filesModifiedByToolCall?.(event);
    if (files && files.length > 0) {
      const first = typed?.filePathFromURI ? typed.filePathFromURI(files[0]) : files[0];
      return { label: `${baseLabel}: ${truncate(basename(first), 24)}`, icon };
    }
  } catch (_) {}
  if (event.tool === "Read" && typeof (event.input as { path?: unknown }).path === "string") {
    return { label: `${baseLabel}: ${truncate(basename((event.input as { path: string }).path), 24)}`, icon };
  }
  return { label: baseLabel, icon };
}

export default function limuxAmpSessionPlugin(amp: PluginAPI) {
  const cwdFromEnv = (): string => firstString(process.env.PWD, process.cwd()) || process.cwd();
  const helpers = (amp as unknown as { helpers?: unknown }).helpers;
  let inFlightTools = 0;
  let turnActive = false;

  process.on("exit", () => {
    try { clearStatus(); } catch (_) {}
  });

  amp.on("session.start", async (event: SessionStartEvent, ctx) => {
    setStatus("idle", "circle", COLOR.idle);
    sendHook("session-start", threadIdFrom(event, ctx), cwdFromEnv());
  });
  amp.on("agent.start", async (event: AgentStartEvent, ctx) => {
    inFlightTools = 0;
    turnActive = true;
    setStatus("thinking", "brain", COLOR.thinking);
    wsLog("prompt received");
    sendHook("prompt-submit", threadIdFrom(event, ctx), cwdFromEnv());
  });
  amp.on("tool.call", async (event: ToolCallEvent) => {
    inFlightTools += 1;
    const { label, icon } = detailedToolStatus(event, helpers);
    if (turnActive) setStatus(label, icon, COLOR.active);
    return { action: "allow" as const };
  });
  amp.on("tool.result", async (event: ToolResultEvent) => {
    inFlightTools = Math.max(0, inFlightTools - 1);
    if (event.status === "error") wsLog(`${event.tool} failed`, "error");
    if (turnActive && inFlightTools === 0) setStatus("thinking", "brain", COLOR.thinking);
  });
  amp.on("agent.end", async (event: AgentEndEvent, ctx) => {
    inFlightTools = 0;
    turnActive = false;
    switch (event.status) {
      case "done":
        setStatus("done", "checkmark.circle", COLOR.done);
        wsLog("turn complete", "success");
        break;
      case "error":
        setStatus("error", "xmark.circle", COLOR.error);
        wsLog("turn errored", "error");
        break;
      case "cancelled":
        setStatus("interrupted", "pause.circle", COLOR.interrupted);
        wsLog("turn interrupted", "warning");
        break;
      default:
        setStatus(String(event.status ?? "done"), "questionmark.circle", COLOR.interrupted);
        wsLog(`turn ended with unexpected status: ${event.status}`, "warning");
        break;
    }
    sendHook("stop", threadIdFrom(event, ctx), cwdFromEnv());
  });
}
"##
        .replace("__LIMUX_COMMAND__", &limux_command_json),
    )
}

fn omp_extension_source_with_command(limux_command: &str) -> Result<String> {
    let limux_command_json =
        serde_json::to_string(limux_command).context("failed to encode OMP hook command")?;
    Ok(
        r#"// cmux-omp-session-extension-marker v1
// Bridges OMP session lifecycle events into Limux's CMUX-compatible restorable session store.
// Installed by `limux hooks omp install` or `limux hooks setup`.
// DO NOT EDIT MANUALLY. Limux upgrades this file in place.

import { spawn } from "node:child_process";
import * as fs from "node:fs";
import * as path from "node:path";

const LIMUX_COMMAND = __LIMUX_COMMAND__;

function firstString(...values) {
  for (const value of values) {
    if (typeof value === "string" && value.trim().length > 0) return value.trim();
  }
  return null;
}

function resolveExecutable(name) {
  const pathEnv = process.env.PATH || "";
  for (const dir of pathEnv.split(path.delimiter)) {
    if (!dir) continue;
    const candidate = path.join(dir, name);
    try {
      fs.accessSync(candidate, fs.constants.X_OK);
      if (fs.statSync(candidate).isFile()) return candidate;
    } catch (_) {}
  }
  return name;
}

function normalizedLaunchArgv() {
  const raw = Array.isArray(process.argv) ? process.argv.map((value) => String(value)) : [];
  if (raw.length === 0) return [resolveExecutable("omp")];
  const base = path.basename(raw[0]).toLowerCase();
  if (base === "omp") return raw;
  return [resolveExecutable("omp"), ...raw.slice(1)];
}

function base64NulSeparated(values) {
  const bytes = [];
  for (const value of values) {
    bytes.push(Buffer.from(String(value), "utf8"));
    bytes.push(Buffer.from([0]));
  }
  return Buffer.concat(bytes).toString("base64");
}

function hookEnvironment(cwd) {
  const env = { ...process.env };
  if (!env.LIMUX_AGENT_LAUNCH_ARGV_B64 && !env.CMUX_AGENT_LAUNCH_ARGV_B64) {
    const argv = normalizedLaunchArgv();
    env.LIMUX_AGENT_LAUNCH_EXECUTABLE = argv[0] || resolveExecutable("omp");
    env.LIMUX_AGENT_LAUNCH_ARGV_B64 = base64NulSeparated(argv);
    env.LIMUX_AGENT_LAUNCH_CWD = cwd || process.cwd();
    env.CMUX_AGENT_LAUNCH_KIND = "omp";
    env.CMUX_AGENT_LAUNCH_EXECUTABLE = env.LIMUX_AGENT_LAUNCH_EXECUTABLE;
    env.CMUX_AGENT_LAUNCH_ARGV_B64 = env.LIMUX_AGENT_LAUNCH_ARGV_B64;
    env.CMUX_AGENT_LAUNCH_CWD = env.LIMUX_AGENT_LAUNCH_CWD;
  }
  return env;
}

function eventName(subcommand) {
  if (subcommand === "session-start") return "SessionStart";
  if (subcommand === "prompt-submit") return "UserPromptSubmit";
  if (subcommand === "stop") return "Stop";
  return subcommand;
}

function textFromContent(content) {
  if (typeof content === "string") return content;
  if (!Array.isArray(content)) return null;
  const parts = [];
  for (const block of content) {
    if (!block || typeof block !== "object") continue;
    if (block.type === "text" && typeof block.text === "string") parts.push(block.text);
  }
  return parts.join("\n") || null;
}

function lastAssistantMessage(event) {
  const messages = Array.isArray(event && event.messages) ? event.messages : [];
  for (let index = messages.length - 1; index >= 0; index -= 1) {
    const message = messages[index];
    if (!message || typeof message !== "object" || message.role !== "assistant") continue;
    const text = firstString(textFromContent(message.content));
    if (text) return text;
  }
  return undefined;
}

function hookInvocation(subcommand, ctx, extra = {}) {
  if (process.env.LIMUX_OMP_HOOKS_DISABLED === "1" || process.env.CMUX_OMP_HOOKS_DISABLED === "1") return null;
  if (!process.env.LIMUX_SURFACE_ID && !process.env.CMUX_SURFACE_ID) return null;
  const sessionId = firstString(ctx && ctx.sessionManager && ctx.sessionManager.getSessionId && ctx.sessionManager.getSessionId());
  if (!sessionId) return null;
  const cwd = firstString(ctx && ctx.cwd, process.cwd()) || process.cwd();
  const payload = {
    session_id: sessionId,
    cwd,
    hook_event_name: eventName(subcommand),
    event: eventName(subcommand),
    ...extra,
  };
  return {
    command: process.env.LIMUX_OMP_LIMUX_BIN || process.env.CMUX_OMP_CMUX_BIN || process.env.LIMUX_BIN || LIMUX_COMMAND,
    cwd,
    payload: JSON.stringify(payload),
    env: hookEnvironment(cwd),
  };
}

async function sendHook(subcommand, ctx, extra = {}) {
  const invocation = hookInvocation(subcommand, ctx, extra);
  if (!invocation) return;
  await new Promise((resolve) => {
    let settled = false;
    const settle = () => {
      if (settled) return;
      settled = true;
      resolve();
    };
    try {
      const child = spawn(invocation.command, ["hooks", "omp", subcommand], {
        env: invocation.env,
        stdio: ["pipe", "ignore", "ignore"],
        detached: true,
      });
      child.on("error", settle);
      child.stdin.on("error", settle);
      child.stdin.on("finish", settle);
      child.unref();
      child.stdin.end(invocation.payload);
    } catch (_) {
      settle();
    }
  });
}

export default function limuxOmpSessionExtension(api) {
  api.on("session_start", async (_event, ctx) => {
    await sendHook("session-start", ctx);
  });
  api.on("before_agent_start", async (event, ctx) => {
    await sendHook("prompt-submit", ctx, { prompt: event && event.prompt });
  });
  api.on("agent_end", async (event, ctx) => {
    await sendHook("stop", ctx, { last_assistant_message: lastAssistantMessage(event) });
  });
}
"#
        .replace("__LIMUX_COMMAND__", &limux_command_json),
    )
}

// purpose: Build CMUX-compatible workspace.create params from new-workspace args.
// inputs: CLI args for new-workspace or workspace create.
// returns/effects: Serializes title, description, cwd, command, focus, group, and workspace env.
fn build_new_workspace_params(args: &[String]) -> Result<Map<String, Value>> {
    let mut params = Map::new();
    if let Some(name) = parse_opt(args, "--name").or_else(|| parse_opt(args, "--title")) {
        params.insert("title".to_string(), Value::String(name));
    }
    if let Some(description) = parse_opt(args, "--description") {
        params.insert("description".to_string(), Value::String(description));
    }
    if let Some(cwd_value) = parse_opt(args, "--cwd") {
        params.insert("cwd".to_string(), Value::String(cwd_value.clone()));
    }
    if let Some(group) = parse_opt(args, "--group") {
        params.insert("group_id".to_string(), Value::String(group));
    }
    if let Some(placement) = parse_opt(args, "--group-placement") {
        params.insert("group_placement".to_string(), Value::String(placement));
    }
    if let Some(reference) = parse_opt(args, "--group-reference") {
        params.insert(
            "group_reference_workspace_id".to_string(),
            Value::String(reference),
        );
    }
    let layout = parse_opt(args, "--layout")
        .map(|raw| {
            serde_json::from_str::<Value>(&raw)
                .with_context(|| "--layout value must be a valid JSON object".to_string())
                .and_then(|value| {
                    if value.is_object() {
                        Ok(value)
                    } else {
                        bail!("--layout value must be a valid JSON object");
                    }
                })
        })
        .transpose()?;
    if let Some(command) = parse_opt(args, "--command").filter(|_| layout.is_none()) {
        params.insert("command".to_string(), Value::String(command));
    }
    if let Some(layout) = layout {
        params.insert("layout".to_string(), layout);
    }
    if let Some(focus) = parse_optional_bool_arg(args, "--focus")? {
        params.insert("focus".to_string(), Value::Bool(focus));
    }
    let environment = parse_workspace_env_args_with_base(args, read_project_workspace_env()?)?;
    if !environment.is_empty() {
        let environment = environment
            .into_iter()
            .map(|(key, value)| (key, Value::String(value)))
            .collect::<Map<_, _>>();
        params.insert("workspace_env".to_string(), Value::Object(environment));
    }
    Ok(params)
}

// purpose: Create a CMUX-compatible workspace without stealing focus by default.
// inputs: Active socket client and new-workspace args.
// returns/effects: Calls workspace.create and restores original focus unless --focus true.
async fn run_new_workspace(client: &mut Client, args: &[String]) -> Result<Value> {
    let params = build_new_workspace_params(args)?;
    let focus = params
        .get("focus")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    let original = resolve_current_workspace(client).await?;

    let created = client
        .call("workspace.create", Value::Object(params))
        .await
        .context("workspace.create failed")?;

    if !focus {
        let _ = client
            .call("workspace.select", json!({ "workspace_id": original }))
            .await;
    }

    Ok(created)
}

// ---------------------------------------------------------------------------
// `limux agent-team` — spin up a multi-agent collaboration workspace.
// ---------------------------------------------------------------------------
//
// Creates ONE workspace and one pane per requested agent (codex / claude /
// opencode / gemini), launches each agent's CLI in its pane, captures the
// pane/surface IDs, and seeds an AGENTS.md in the shared cwd describing the
// XML-tagged message protocol and the peer directory so agents can message
// each other.
//
// The protocol (codified in AGENTS.md):
//   To send a message to a peer, run from any terminal:
//     limux send --surface <peer-surface-id> \\
//       $'<agent-msg from="<me>" to="<peer>" ts="<iso-8601>">\\n...\\n</agent-msg>\\n'
//
// Peers read their own terminals normally — the text appears at the prompt.
// Each agent should watch for <agent-msg from="..."> blocks and reply with
// the same envelope targeted back.

/// Built-in agent launcher commands. Chosen to match the CLIs the user
/// actually has installed (see README); the launch command is what gets
/// typed into the new workspace's terminal, so it also works as a fallback
/// shell command if the CLI isn't in PATH yet.
fn agent_launch_command(agent: &str) -> Option<(&'static str, String)> {
    match agent.to_lowercase().as_str() {
        "codex" => Some(("codex", "codex".to_string())),
        "claude" | "claude-code" => Some(("claude", "claude".to_string())),
        "opencode" => Some(("opencode", "opencode".to_string())),
        "gemini" | "gemini-cli" => Some(("gemini", "gemini".to_string())),
        "pi" | "pi-coding-agent" => Some(("pi", "pi".to_string())),
        "amp" => Some(("amp", "amp".to_string())),
        "hermes" | "hermes-agent" => Some(("hermes", "hermes".to_string())),
        _ => None,
    }
}

async fn run_agent_team(client: &mut Client, args: &[String]) -> Result<Value> {
    // Parse --agents codex,claude (default: codex,claude).
    let agents_raw = parse_opt(args, "--agents").unwrap_or_else(|| "codex,claude".to_string());
    let agents: Vec<String> = agents_raw
        .split(',')
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .collect();
    if agents.is_empty() {
        bail!("agent-team: --agents is empty");
    }

    let cwd = parse_opt(args, "--cwd")
        .or_else(|| {
            env::current_dir()
                .ok()
                .map(|p| p.to_string_lossy().to_string())
        })
        .ok_or_else(|| anyhow!("agent-team: could not resolve --cwd"))?;

    // Optional: skip launching the CLIs (useful when the user wants to open
    // the agents manually) — still splits the panes + writes AGENTS.md.
    let no_launch = args.iter().any(|a| a == "--no-launch");
    let dry_run = args.iter().any(|a| a == "--dry-run");

    // Resolve the agent list up front so --dry-run can build a deterministic
    // peer table without touching the host.
    let resolved: Vec<(String, &'static str, String)> = agents
        .iter()
        .filter_map(|agent| {
            agent_launch_command(agent).map(|(name, launch)| (agent.clone(), name, launch))
        })
        .collect();
    for agent in &agents {
        if agent_launch_command(agent).is_none() {
            eprintln!("agent-team: unknown agent '{agent}', skipping");
        }
    }
    if resolved.is_empty() {
        bail!("agent-team: no valid agents spawned");
    }

    let agents_md_path = std::path::Path::new(&cwd).join("AGENTS.md");

    if dry_run {
        let peers: Vec<(String, String, String, String)> = resolved
            .iter()
            .enumerate()
            .map(|(i, (_, name, launch))| {
                (
                    name.to_string(),
                    format!("<dry-run-pane-{i}>"),
                    format!("<dry-run-surface-{name}>"),
                    launch.clone(),
                )
            })
            .collect();
        let body = build_agents_md(
            &peers,
            &cwd,
            "<active-workspace>",
            "<dry-run-workspace>",
            "<dry-run-orchestrator>",
        );
        if let Err(err) = std::fs::write(&agents_md_path, body) {
            eprintln!(
                "agent-team: failed to write {}: {err}",
                agents_md_path.display()
            );
        }
        return Ok(json!({
            "ok": true,
            "cwd": cwd,
            "workspace_name": "<active-workspace>",
            "workspace_id": Value::Null,
            "orchestrator_surface_id": Value::Null,
            "agents_md": agents_md_path.to_string_lossy(),
            "dry_run": true,
            "no_launch": no_launch,
            "peers": peers
                .iter()
                .map(|(name, pane, surface, launch)| {
                    json!({
                        "agent": name,
                        "pane_id": pane,
                        "surface_id": surface,
                        "launch_command": launch,
                    })
                })
                .collect::<Vec<_>>(),
        }));
    }

    // 1. Resolve the orchestrator's workspace + pane. Prefer Limux/CMUX env
    //    (set in every Limux-spawned terminal) and fall back to the host's active
    //    focus so callers from a regular shell still work.
    let orchestrator_workspace = context_env_value("LIMUX_WORKSPACE_ID").filter(|s| !s.is_empty());
    let orchestrator_surface_env = context_env_value("LIMUX_SURFACE_ID").filter(|s| !s.is_empty());
    let orchestrator_pane_env = context_env_value("LIMUX_PANE_ID").filter(|s| !s.is_empty());

    let workspace_id = match orchestrator_workspace.clone() {
        Some(id) => id,
        None => resolve_current_workspace(client)
            .await
            .context("agent-team: could not resolve active workspace; run from inside a limux pane or pass --workspace")?,
    };

    // 2. Discover the orchestrator pane's surface_id. If env didn't tell us,
    //    use the focused/first surface in the workspace.
    let surfaces = client
        .call(
            "surface.list",
            json!({ "workspace_id": workspace_id.clone() }),
        )
        .await
        .context("surface.list failed for active workspace")?;
    let surface_rows = surfaces
        .get("surfaces")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    if surface_rows.is_empty() {
        bail!("agent-team: active workspace has no surfaces");
    }
    let orchestrator_surface = orchestrator_surface_env.clone().unwrap_or_else(|| {
        surface_rows
            .iter()
            .find(|row| row.get("focused").and_then(Value::as_bool) == Some(true))
            .and_then(|row| get_string(row, &["surface_id"]))
            .or_else(|| get_string(&surface_rows[0], &["surface_id"]))
            .unwrap_or_default()
    });
    if orchestrator_surface.is_empty() {
        bail!("agent-team: could not determine orchestrator surface");
    }
    let orchestrator_pane = orchestrator_pane_env.unwrap_or_else(|| {
        surface_rows
            .iter()
            .find(|row| {
                get_string(row, &["surface_id"]).as_deref() == Some(orchestrator_surface.as_str())
            })
            .and_then(|row| get_string(row, &["pane_id"]))
            .unwrap_or_default()
    });

    // 3. Workspace name (for AGENTS.md header) — best-effort lookup.
    let workspace_name = client
        .call("workspace.list", json!({}))
        .await
        .ok()
        .and_then(|v| v.get("workspaces").and_then(Value::as_array).cloned())
        .and_then(|rows| {
            rows.into_iter().find(|row| {
                get_string(row, &["workspace_id", "id"]).as_deref() == Some(workspace_id.as_str())
            })
        })
        .and_then(|row| get_string(&row, &["name", "title"]))
        .unwrap_or_else(|| "active workspace".to_string());

    // 4. Split a pane per agent. Layout: agent[0] splits RIGHT of orchestrator,
    //    each subsequent agent splits DOWN of the previous agent — orchestrator
    //    keeps its full height on the left, peers stack top-to-bottom on the right.
    let mut peers: Vec<(String, String, String, String)> = Vec::new();
    let mut parent_surface = orchestrator_surface.clone();

    for (i, (_alias, name, launch)) in resolved.iter().enumerate() {
        let direction = if i == 0 { "right" } else { "down" };

        let mut params = Map::new();
        params.insert(
            "workspace_id".to_string(),
            Value::String(workspace_id.clone()),
        );
        params.insert(
            "surface_id".to_string(),
            Value::String(parent_surface.clone()),
        );
        params.insert(
            "direction".to_string(),
            Value::String(direction.to_string()),
        );
        params.insert("type".to_string(), Value::String("terminal".to_string()));
        if !no_launch {
            params.insert("command".to_string(), Value::String(launch.clone()));
        }

        let created = client
            .call("pane.create", Value::Object(params))
            .await
            .with_context(|| format!("pane.create failed for agent '{name}'"))?;
        let pane_id = get_string(&created, &["pane_id"])
            .ok_or_else(|| anyhow!("agent-team: pane.create for '{name}' returned no pane_id"))?;
        let surface_id = get_string(&created, &["surface_id"]).ok_or_else(|| {
            anyhow!("agent-team: pane.create for '{name}' returned no surface_id")
        })?;

        parent_surface = surface_id.clone();
        peers.push((name.to_string(), pane_id, surface_id, launch.clone()));
    }

    // 5. Write AGENTS.md into the shared cwd, clobbering any existing file.
    let body = build_agents_md(
        &peers,
        &cwd,
        &workspace_name,
        &workspace_id,
        &orchestrator_surface,
    );
    if let Err(err) = std::fs::write(&agents_md_path, body) {
        eprintln!(
            "agent-team: failed to write {}: {err}",
            agents_md_path.display()
        );
    }

    Ok(json!({
        "ok": true,
        "cwd": cwd,
        "workspace_name": workspace_name,
        "workspace_id": workspace_id,
        "orchestrator_pane_id": orchestrator_pane,
        "orchestrator_surface_id": orchestrator_surface,
        "agents_md": agents_md_path.to_string_lossy(),
        "dry_run": false,
        "no_launch": no_launch,
        "peers": peers
            .iter()
            .map(|(name, pane, surface, launch)| {
                json!({
                    "agent": name,
                    "pane_id": pane,
                    "surface_id": surface,
                    "launch_command": launch,
                })
            })
            .collect::<Vec<_>>(),
    }))
}

fn build_agents_md(
    peers: &[(String, String, String, String)],
    cwd: &str,
    workspace_name: &str,
    workspace_id: &str,
    orchestrator_surface: &str,
) -> String {
    let mut out = String::new();
    out.push_str("# AGENTS.md — agent-to-agent message protocol\n\n");
    out.push_str(
        "This file is auto-generated by `limux agent-team`. It defines how the\n\
         agents running in this workspace team communicate with each other via\n\
         the limux control socket. Humans should feel free to edit the\n\
         'Policies' section below; everything else is mechanical.\n\n",
    );

    out.push_str(&format!(
        "## Team workspace\n\n\
         The orchestrator (the pane that ran `limux agent-team`) and all\n\
         spawned peers share one workspace:\n\n\
         - Workspace name: `{workspace_name}`\n\
         - Workspace ID: `{workspace_id}`\n\
         - Orchestrator surface: `{orchestrator_surface}`\n\
         - Shared cwd: `{cwd}`\n\n",
    ));

    out.push_str("## Peers in this team\n\n");
    out.push_str("| Agent | Pane | Surface | Launch command |\n");
    out.push_str("|-------|------|---------|----------------|\n");
    for (name, pane_id, surface_id, launch) in peers {
        out.push_str(&format!(
            "| `{name}` | `{pane_id}` | `{surface_id}` | `{launch}` |\n"
        ));
    }
    out.push('\n');
    out.push_str(
        "The orchestrator is not in the table — message it back using its\n\
         `Orchestrator surface` from the block above.\n\n",
    );

    out.push_str("## How to send a message\n\n");
    out.push_str(
        "Messages use the `<agent-msg>` XML envelope so they're easy to\n\
         extract from the terminal scrollback. To send a message to a peer,\n\
         look up their `Surface` in the peers table above and run (from any\n\
         shell, including the agent's own terminal — `limux` is on PATH):\n\n",
    );
    out.push_str("```bash\n");
    out.push_str("limux send --surface <peer-surface-id> $'<agent-msg from=\"<me>\" to=\"<peer>\" id=\"<uuid>\" ts=\"<iso8601>\">\\n<body/>\\n</agent-msg>\\n'\n");
    out.push_str("```\n\n");
    out.push_str(
        "The message appears at the peer's prompt as plain stdin, so the\n\
         peer's agent CLI picks it up like a normal user message. Trailing\n\
         newline is required so the agent's read-line actually fires.\n\n",
    );

    out.push_str("### Envelope format\n\n");
    out.push_str("```xml\n");
    out.push_str("<agent-msg from=\"codex\" to=\"claude\" id=\"<uuid>\" ts=\"2026-04-19T16:48:00Z\" reply-to=\"<parent-uuid>\">\n");
    out.push_str(
        "  <context>optional: one or two sentences about what the request is for</context>\n",
    );
    out.push_str("  <request>the actual ask, in prose or code</request>\n");
    out.push_str("  <expect>how you want the peer to reply (\"inline code diff\" / \"short summary\" / etc.)</expect>\n");
    out.push_str("</agent-msg>\n");
    out.push_str("```\n\n");

    out.push_str("Rules:\n");
    out.push_str("- `from` / `to` MUST be one of the agent names in the peers table.\n");
    out.push_str("- `id` is a fresh UUID (e.g. `uuidgen`); peers echo it in `reply-to`.\n");
    out.push_str("- `ts` is ISO-8601 UTC (`date -u +%Y-%m-%dT%H:%M:%SZ`).\n");
    out.push_str("- Inner tags are guidance, not required — `<request>` alone is fine.\n");
    out.push_str("- Keep bodies short; link to files in the shared cwd for anything long.\n\n");

    out.push_str("### Replying\n\n");
    out.push_str("Reply with the envelope reversed and `reply-to` set to the original `id`:\n\n");
    out.push_str("```bash\n");
    out.push_str("limux send --surface <orig-sender-surface-id> $'<agent-msg from=\"claude\" to=\"codex\" id=\"<new-uuid>\" reply-to=\"<orig-uuid>\" ts=\"<iso8601>\">\\n<response>...</response>\\n</agent-msg>\\n'\n");
    out.push_str("```\n\n");

    out.push_str("## Pinging the human\n\n");
    out.push_str(
        "When you need human input, use `limux notify` — it pops a toast\n\
         and lights up the workspace in the sidebar. Example:\n\n",
    );
    out.push_str("```bash\n");
    out.push_str("limux notify --subtitle 'needs review' --body 'Claude blocked on auth choice' 'Input needed'\n");
    out.push_str("```\n\n");

    out.push_str("## Environment contract\n\n");
    out.push_str(
        "Every pane spawned by limux inherits:\n\
         - `LIMUX_WORKSPACE_ID` — the team workspace's UUID\n\
         - `LIMUX_SURFACE_ID` — this pane's surface id (this is your `from`)\n\
         - `LIMUX_PANE_ID`, `LIMUX_TAB_ID`\n\
         - `LIMUX_SOCKET` — the control socket path\n\n\
         This means `limux identify`, `limux send` (with `--surface`), and\n\
         `limux notify` all auto-target the right thing with no flags needed\n\
         from inside the agent's own terminal.\n\n",
    );

    out.push_str("## Splitting your own pane\n\n");
    out.push_str("If you need a scratch terminal next to you, split your own pane:\n\n");
    out.push_str("```bash\n");
    out.push_str("limux new-pane --direction right --command bash\n");
    out.push_str("```\n\n");
    out.push_str(
        "`new-pane` reads `LIMUX_*` and CMUX-compatible context variables, so\n\
         it splits your current pane even if GTK focus has moved elsewhere.\n\n",
    );

    out.push_str("## Policies (edit these freely)\n\n");
    out.push_str(
        "- If a peer is silent for more than 60 seconds, re-send with `reply-to` = your last id.\n",
    );
    out.push_str(
        "- Never send more than 200 lines at once; write to a file and send the path instead.\n",
    );
    out.push_str("- If two agents disagree on an approach, both message the human via `limux notify` and stop.\n");
    out.push_str("- Before taking destructive actions (rm, git push, kubectl apply), ask the human via `limux notify`.\n\n");

    out.push_str("---\n");
    out.push_str(
        "_Generated by `limux agent-team`. Safe to edit the Policies\n\
         section; regenerating will overwrite everything above it._\n",
    );

    out
}

async fn run_close_workspace(client: &mut Client, args: &[String]) -> Result<Value> {
    let workspace = parse_opt(args, "--workspace")
        .or_else(|| context_env_value("LIMUX_WORKSPACE_ID"))
        .ok_or_else(|| anyhow!("close-workspace requires --workspace <id|ref>"))?;
    client
        .call("workspace.close", json!({ "workspace_id": workspace }))
        .await
}

async fn run_sidebar_state(client: &mut Client, args: &[String]) -> Result<Value> {
    let request = build_sidebar_command_request("sidebar-state", args, None)?;
    client.call("sidebar.state", Value::Object(request)).await
}

/// purpose: Build and run a CMUX-compatible sidebar metadata command.
/// inputs: Command name, CLI args, and optional global window selector.
/// returns/effects: Sends one live bridge request and returns the JSON payload.
async fn run_sidebar_command(
    client: &mut Client,
    command: &str,
    args: &[String],
    global_window: Option<&str>,
) -> Result<Value> {
    let request = build_sidebar_command_request(command, args, global_window)?;
    client
        .call(sidebar_command_method(command)?, Value::Object(request))
        .await
}

/// purpose: Map CMUX sidebar command names to live bridge method names.
/// inputs: User-facing command name.
/// returns/effects: Returns a bridge method or a usage error.
fn sidebar_command_method(command: &str) -> Result<&'static str> {
    match command {
        "set-status" => Ok("sidebar.status.set"),
        "clear-status" => Ok("sidebar.status.clear"),
        "list-status" => Ok("sidebar.status.list"),
        "set-progress" => Ok("sidebar.progress.set"),
        "clear-progress" => Ok("sidebar.progress.clear"),
        "log" => Ok("sidebar.log.append"),
        "clear-log" => Ok("sidebar.log.clear"),
        "list-log" => Ok("sidebar.log.list"),
        "sidebar-state" => Ok("sidebar.state"),
        _ => bail!("unsupported sidebar command: {command}"),
    }
}

/// purpose: Parse CMUX sidebar metadata CLI args into bridge params.
/// inputs: Command name, raw args, and optional inherited global `--window`.
/// returns/effects: Returns normalized params or fails loudly on malformed commands.
fn build_sidebar_command_request(
    command: &str,
    args: &[String],
    global_window: Option<&str>,
) -> Result<Map<String, Value>> {
    let mut parsed = SidebarCommandArgs::default();
    let mut idx = 0usize;
    while idx < args.len() {
        if consume_sidebar_option(args, &mut idx, &mut parsed)? {
            continue;
        }
        if args[idx] == "--" {
            parsed.positional.extend(args[idx + 1..].iter().cloned());
            break;
        }
        parsed.positional.push(args[idx].clone());
        idx += 1;
    }
    sidebar_command_params(command, parsed, global_window)
}

#[derive(Default)]
struct SidebarCommandArgs {
    workspace: Option<String>,
    window: Option<String>,
    icon: Option<String>,
    color: Option<String>,
    url: Option<String>,
    priority: Option<String>,
    label: Option<String>,
    level: Option<String>,
    source: Option<String>,
    limit: Option<String>,
    positional: Vec<String>,
}

/// purpose: Consume one CMUX sidebar command option.
/// inputs: CLI args, current index, and mutable parsed state.
/// returns/effects: Advances idx when an option is consumed.
fn consume_sidebar_option(
    args: &[String],
    idx: &mut usize,
    parsed: &mut SidebarCommandArgs,
) -> Result<bool> {
    let raw = args[*idx].as_str();
    let Some(flag) = raw.strip_prefix("--") else {
        return Ok(false);
    };
    let (name, inline) = flag
        .split_once('=')
        .map(|(name, value)| (format!("--{name}"), Some(value.to_string())))
        .unwrap_or_else(|| (raw.to_string(), None));
    match name.as_str() {
        "--workspace" => parsed.workspace = Some(sidebar_option_value(args, idx, &name, inline)?),
        "--window" => parsed.window = Some(sidebar_option_value(args, idx, &name, inline)?),
        "--icon" => parsed.icon = Some(sidebar_option_value(args, idx, &name, inline)?),
        "--color" => parsed.color = Some(sidebar_option_value(args, idx, &name, inline)?),
        "--url" => parsed.url = Some(sidebar_option_value(args, idx, &name, inline)?),
        "--priority" => parsed.priority = Some(sidebar_option_value(args, idx, &name, inline)?),
        "--label" => parsed.label = Some(sidebar_option_value(args, idx, &name, inline)?),
        "--level" => parsed.level = Some(sidebar_option_value(args, idx, &name, inline)?),
        "--source" => parsed.source = Some(sidebar_option_value(args, idx, &name, inline)?),
        "--limit" => parsed.limit = Some(sidebar_option_value(args, idx, &name, inline)?),
        "--" => return Ok(false),
        _ => bail!("Unknown sidebar option {name}"),
    }
    Ok(true)
}

/// purpose: Read a sidebar option value from `--flag=value` or `--flag value`.
/// inputs: CLI args, mutable current index, option name, and optional inline value.
/// returns/effects: Advances idx past the consumed option/value pair.
fn sidebar_option_value(
    args: &[String],
    idx: &mut usize,
    name: &str,
    inline: Option<String>,
) -> Result<String> {
    if let Some(value) = inline {
        *idx += 1;
        return Ok(value);
    }
    let value = args
        .get(*idx + 1)
        .ok_or_else(|| anyhow!("{name} requires a value"))?
        .clone();
    *idx += 2;
    Ok(value)
}

/// purpose: Convert parsed sidebar CLI state into bridge params.
/// inputs: Command name, parsed args, and optional global window selector.
/// returns/effects: Returns command-specific JSON params.
fn sidebar_command_params(
    command: &str,
    parsed: SidebarCommandArgs,
    global_window: Option<&str>,
) -> Result<Map<String, Value>> {
    let mut params = sidebar_target_params(&parsed, global_window);
    match command {
        "set-status" => sidebar_set_status_params(&mut params, parsed)?,
        "clear-status" => sidebar_single_key_params(&mut params, parsed, "clear-status")?,
        "list-status" | "clear-progress" | "clear-log" | "sidebar-state" => {}
        "set-progress" => sidebar_set_progress_params(&mut params, parsed)?,
        "log" => sidebar_log_params(&mut params, parsed)?,
        "list-log" => sidebar_list_log_params(&mut params, parsed)?,
        _ => bail!("unsupported sidebar command: {command}"),
    }
    Ok(params)
}

/// purpose: Build shared workspace/window target params for sidebar commands.
/// inputs: Parsed sidebar args and optional inherited global window.
/// returns/effects: Omits absent values so the host may target the active workspace.
fn sidebar_target_params(
    parsed: &SidebarCommandArgs,
    global_window: Option<&str>,
) -> Map<String, Value> {
    let mut params = Map::new();
    let workspace = parsed
        .workspace
        .clone()
        .or_else(|| context_env_value("LIMUX_WORKSPACE_ID"))
        .or_else(|| context_env_value("CMUX_WORKSPACE_ID"));
    if let Some(workspace) = workspace {
        params.insert("workspace_id".to_string(), Value::String(workspace));
    }
    let window = parsed
        .window
        .clone()
        .or_else(|| global_window.map(ToOwned::to_owned));
    if let Some(window) = window {
        params.insert("window_id".to_string(), Value::String(window));
    }
    params
}

/// purpose: Add set-status positional and presentation params.
/// inputs: Mutable param map plus parsed CLI args.
/// returns/effects: Fails when key/value or priority is malformed.
fn sidebar_set_status_params(
    params: &mut Map<String, Value>,
    parsed: SidebarCommandArgs,
) -> Result<()> {
    let key = parsed
        .positional
        .first()
        .ok_or_else(|| anyhow!("set-status requires <key> <value>"))?;
    let value = parsed
        .positional
        .get(1)
        .ok_or_else(|| anyhow!("set-status requires <key> <value>"))?;
    params.insert("key".to_string(), Value::String(key.clone()));
    params.insert("value".to_string(), Value::String(value.clone()));
    insert_optional_string(params, "icon", parsed.icon);
    insert_optional_string(params, "color", parsed.color);
    insert_optional_string(params, "url", parsed.url);
    if let Some(priority) = parsed.priority {
        let priority = priority
            .parse::<i64>()
            .map_err(|_| anyhow!("--priority must be an integer"))?;
        params.insert("priority".to_string(), json!(priority));
    }
    Ok(())
}

/// purpose: Add a required single-key positional param.
/// inputs: Mutable param map, parsed CLI args, and command name for diagnostics.
/// returns/effects: Fails when the key is missing.
fn sidebar_single_key_params(
    params: &mut Map<String, Value>,
    parsed: SidebarCommandArgs,
    command: &str,
) -> Result<()> {
    let key = parsed
        .positional
        .first()
        .ok_or_else(|| anyhow!("{command} requires <key>"))?;
    params.insert("key".to_string(), Value::String(key.clone()));
    Ok(())
}

/// purpose: Add set-progress value/label params.
/// inputs: Mutable param map plus parsed CLI args.
/// returns/effects: Fails when progress is missing or outside 0.0..=1.0.
fn sidebar_set_progress_params(
    params: &mut Map<String, Value>,
    parsed: SidebarCommandArgs,
) -> Result<()> {
    let raw = parsed
        .positional
        .first()
        .ok_or_else(|| anyhow!("set-progress requires <0.0-1.0>"))?;
    let value = raw
        .parse::<f64>()
        .map_err(|_| anyhow!("set-progress value must be a number"))?;
    if !(0.0..=1.0).contains(&value) {
        bail!("set-progress value must be between 0.0 and 1.0");
    }
    params.insert("value".to_string(), json!(value));
    insert_optional_string(params, "label", parsed.label);
    Ok(())
}

/// purpose: Add sidebar log append params.
/// inputs: Mutable param map plus parsed CLI args.
/// returns/effects: Joins positional message tokens and validates level.
fn sidebar_log_params(params: &mut Map<String, Value>, parsed: SidebarCommandArgs) -> Result<()> {
    let message = parsed.positional.join(" ");
    if message.trim().is_empty() {
        bail!("log requires <message>");
    }
    if let Some(level) = parsed.level {
        match level.as_str() {
            "info" | "progress" | "success" | "warning" | "error" => {
                params.insert("level".to_string(), Value::String(level));
            }
            _ => bail!("--level must be info, progress, success, warning, or error"),
        }
    }
    insert_optional_string(params, "source", parsed.source);
    params.insert("message".to_string(), Value::String(message));
    Ok(())
}

/// purpose: Add list-log limit params.
/// inputs: Mutable param map plus parsed CLI args.
/// returns/effects: Fails when --limit is not a non-negative integer.
fn sidebar_list_log_params(
    params: &mut Map<String, Value>,
    parsed: SidebarCommandArgs,
) -> Result<()> {
    if let Some(limit) = parsed.limit {
        let limit = limit
            .parse::<usize>()
            .map_err(|_| anyhow!("--limit must be a non-negative integer"))?;
        params.insert("limit".to_string(), json!(limit));
    }
    Ok(())
}

fn insert_optional_string(params: &mut Map<String, Value>, key: &str, value: Option<String>) {
    if let Some(value) = value {
        params.insert(key.to_string(), Value::String(value));
    }
}

/// purpose: Render CMUX sidebar list command payloads as compact text.
/// inputs: Command name and JSON payload from the live bridge.
/// returns/effects: Returns newline-delimited rows for terminal output.
fn render_sidebar_list_text(command: &str, payload: &Value) -> String {
    match command {
        "list-status" => payload
            .get("status")
            .and_then(Value::as_array)
            .map(|rows| {
                rows.iter()
                    .map(render_sidebar_status_row)
                    .collect::<Vec<_>>()
            })
            .unwrap_or_default()
            .join("\n"),
        "list-log" => payload
            .get("log")
            .and_then(Value::as_array)
            .map(|rows| rows.iter().map(render_sidebar_log_row).collect::<Vec<_>>())
            .unwrap_or_default()
            .join("\n"),
        _ => String::new(),
    }
}

/// purpose: Render aggregate sidebar-state payloads for non-JSON CLI output.
/// inputs: JSON payload from the live bridge.
/// returns/effects: Returns workspace metadata plus retained status/progress/log rows.
fn render_sidebar_state_text(payload: &Value) -> String {
    let workspace =
        get_string(payload, &["workspace", "workspace_id"]).unwrap_or_else(|| "none".to_string());
    let cwd = get_string(payload, &["cwd"]).unwrap_or_else(|| "none".to_string());
    let git_branch = get_string(payload, &["git_branch"]).unwrap_or_else(|| "none".to_string());
    let status = render_sidebar_state_section(payload, "status", render_sidebar_status_row);
    let log = render_sidebar_state_section(payload, "log", render_sidebar_log_row);
    let progress = payload
        .get("progress")
        .filter(|value| !value.is_null())
        .map(render_sidebar_progress_row)
        .unwrap_or_else(|| "none".to_string());
    format!(
        "workspace={workspace}\ncwd={cwd}\ngit_branch={git_branch}\nprogress={progress}\nstatus:\n{status}\nlog:\n{log}"
    )
}

/// purpose: Render one array section from sidebar-state.
/// inputs: Payload, section key, and row renderer.
/// returns/effects: Returns "none" for empty or missing sections.
fn render_sidebar_state_section(
    payload: &Value,
    key: &str,
    render: fn(&Value) -> String,
) -> String {
    let rows = payload
        .get(key)
        .and_then(Value::as_array)
        .map(|rows| rows.iter().map(render).collect::<Vec<_>>())
        .unwrap_or_default();
    if rows.is_empty() {
        "none".to_string()
    } else {
        rows.join("\n")
    }
}

/// purpose: Render one sidebar progress row for non-JSON CLI output.
/// inputs: JSON progress object from the live bridge.
/// returns/effects: Returns value plus label when present.
fn render_sidebar_progress_row(row: &Value) -> String {
    let value = row
        .get("value")
        .and_then(Value::as_f64)
        .map(|value| value.to_string())
        .unwrap_or_else(|| "unknown".to_string());
    let label = get_string(row, &["label"])
        .map(|value| format!(" label={value}"))
        .unwrap_or_default();
    format!("{value}{label}")
}

/// purpose: Render one sidebar status row for non-JSON CLI output.
/// inputs: JSON status row from the live bridge.
/// returns/effects: Returns key=value plus priority when present.
fn render_sidebar_status_row(row: &Value) -> String {
    let key = get_string(row, &["key"]).unwrap_or_else(|| "unknown".to_string());
    let value = get_string(row, &["value"]).unwrap_or_default();
    let priority = row
        .get("priority")
        .and_then(Value::as_i64)
        .map(|value| format!(" priority={value}"))
        .unwrap_or_default();
    format!("{key}={value}{priority}")
}

/// purpose: Render one sidebar log row for non-JSON CLI output.
/// inputs: JSON log row from the live bridge.
/// returns/effects: Returns timestamp, level/source, and message.
fn render_sidebar_log_row(row: &Value) -> String {
    let created_at = get_string(row, &["created_at"]).unwrap_or_else(|| "unknown".to_string());
    let level = get_string(row, &["level"]).unwrap_or_else(|| "info".to_string());
    let source = get_string(row, &["source"])
        .map(|value| format!(" {value}"))
        .unwrap_or_default();
    let message = get_string(row, &["message"]).unwrap_or_default();
    format!("{created_at} {level}{source}: {message}")
}

/// purpose: Build and run a CMUX-compatible right-sidebar control command.
/// inputs: Right-sidebar CLI args plus optional global window selector.
/// returns/effects: Sends one live bridge request and returns whether output is expected.
async fn run_right_sidebar(
    client: &mut Client,
    args: &[String],
    global_window: Option<&str>,
) -> Result<(Value, bool)> {
    let request = build_right_sidebar_request(args, global_window)?;
    let prints_state = request
        .get("action")
        .and_then(Value::as_str)
        .map(|action| action == "mode")
        .unwrap_or(false);
    let payload = client.call("right_sidebar", Value::Object(request)).await?;
    Ok((payload, prints_state))
}

/// purpose: Run CMUX-compatible top-level Feed UI commands through Limux UI surfaces.
/// inputs: Feed CLI args plus optional global window selector.
/// returns/effects: Opens the live Feed right-sidebar surface or fails loudly on unsupported forms.
async fn run_feed_command(
    client: &mut Client,
    args: &[String],
    global_window: Option<&str>,
) -> Result<Value> {
    let request = build_feed_tui_request(args, global_window)?;
    let mut payload = client.call("right_sidebar", Value::Object(request)).await?;
    if let Value::Object(map) = &mut payload {
        map.insert(
            "implementation".to_string(),
            Value::String("limux-right-sidebar".to_string()),
        );
    }
    Ok(payload)
}

/// purpose: Parse `feed tui` into the existing right-sidebar Feed mode request.
/// inputs: Raw Feed command args and optional inherited global `--window`.
/// returns/effects: Returns bridge params, accepting CMUX implementation selector flags explicitly.
fn build_feed_tui_request(
    args: &[String],
    global_window: Option<&str>,
) -> Result<Map<String, Value>> {
    if args.first().map(String::as_str) != Some("tui") {
        bail!("Usage: limux feed tui [--opentui|--legacy]");
    }
    for arg in &args[1..] {
        match arg.as_str() {
            "--opentui" | "--legacy" => {}
            "--help" | "-h" => bail!("Usage: limux feed tui [--opentui|--legacy]"),
            other => bail!("unknown feed tui argument: {other}"),
        }
    }
    build_right_sidebar_request(&["feed".to_string()], global_window)
}

/// purpose: Render the user-facing result for top-level Feed commands.
/// inputs: JSON payload returned by the live bridge.
/// returns/effects: Returns compact text that names the explicit Limux UI implementation.
fn render_feed_command_text(payload: &Value) -> String {
    let implementation = payload
        .get("implementation")
        .and_then(Value::as_str)
        .unwrap_or("limux-right-sidebar");
    format!("OK feed ui implementation={implementation}")
}

/// purpose: Parse CMUX right-sidebar CLI forms into live bridge params.
/// inputs: Raw command args and optional global `--window`.
/// returns/effects: Returns normalized params or fails loudly on malformed commands.
fn build_right_sidebar_request(
    args: &[String],
    global_window: Option<&str>,
) -> Result<Map<String, Value>> {
    let parsed = parse_right_sidebar_args(args)?;
    let mut params = right_sidebar_target_params(&parsed, global_window);
    params.extend(right_sidebar_action_params(&parsed)?);
    Ok(params)
}

/// purpose: Convert parsed CMUX right-sidebar targets into bridge params.
/// inputs: Parsed args and optional inherited global `--window`.
/// returns/effects: Returns only target fields with empty values omitted.
fn right_sidebar_target_params(
    parsed: &RightSidebarArgs,
    global_window: Option<&str>,
) -> Map<String, Value> {
    let mut params = Map::new();
    if let Some(workspace) = parsed.workspace.as_ref() {
        params.insert("workspace_id".to_string(), Value::String(workspace.clone()));
    }
    let window = parsed
        .window
        .clone()
        .or_else(|| global_window.map(ToOwned::to_owned))
        .filter(|value| !value.trim().is_empty());
    if let Some(window) = window {
        params.insert("window_id".to_string(), Value::String(window));
    }
    params
}

/// purpose: Convert parsed CMUX right-sidebar action syntax into bridge params.
/// inputs: Parsed right-sidebar args.
/// returns/effects: Returns action/mode/focus params or a usage error.
fn right_sidebar_action_params(parsed: &RightSidebarArgs) -> Result<Map<String, Value>> {
    let action = parsed
        .positional
        .first()
        .ok_or_else(|| anyhow!("right-sidebar requires a subcommand"))?
        .to_ascii_lowercase();
    let mut params = Map::new();
    match action.as_str() {
        "toggle" | "show" | "hide" | "focus" | "mode" => {
            if parsed.positional.len() != 1 {
                bail!("right-sidebar {action} received unexpected arguments");
            }
            if parsed.no_focus {
                bail!("right-sidebar: --no-focus is only valid with set");
            }
            params.insert("action".to_string(), Value::String(action));
        }
        "set" => {
            if parsed.positional.len() != 2 {
                bail!("right-sidebar set requires a mode: files, find, vault, sessions, feed, or dock");
            }
            let mode = normalize_right_sidebar_mode(&parsed.positional[1])?;
            params.insert("action".to_string(), Value::String("set".to_string()));
            params.insert("mode".to_string(), Value::String(mode));
            params.insert("focus".to_string(), Value::Bool(!parsed.no_focus));
        }
        "files" | "find" | "vault" | "sessions" | "feed" | "dock" => {
            if parsed.positional.len() != 1 {
                bail!("right-sidebar {action} received unexpected arguments");
            }
            if parsed.no_focus {
                bail!("right-sidebar: --no-focus is only valid with set");
            }
            params.insert("action".to_string(), Value::String("set".to_string()));
            params.insert("mode".to_string(), Value::String(action));
            params.insert("focus".to_string(), Value::Bool(true));
        }
        _ => {
            if parsed.positional.len() == 1 && !parsed.no_focus {
                let mode = normalize_right_sidebar_mode(&action)?;
                params.insert("action".to_string(), Value::String("set".to_string()));
                params.insert("mode".to_string(), Value::String(mode));
                params.insert("focus".to_string(), Value::Bool(true));
            } else {
                bail!("Unknown right-sidebar command '{}'", parsed.positional[0]);
            }
        }
    }
    Ok(params)
}

#[derive(Debug, Default)]
struct RightSidebarArgs {
    positional: Vec<String>,
    workspace: Option<String>,
    window: Option<String>,
    no_focus: bool,
}

/// purpose: Parse CMUX right-sidebar flags without consuming positional modes.
/// inputs: Raw CLI tokens after `right-sidebar`.
/// returns/effects: Returns split positional/target flags or a usage error.
fn parse_right_sidebar_args(args: &[String]) -> Result<RightSidebarArgs> {
    let mut parsed = RightSidebarArgs::default();
    let mut idx = 0usize;
    while idx < args.len() {
        if consume_right_sidebar_option(args, &mut idx, &mut parsed)? {
            continue;
        }
        parsed.positional.push(args[idx].clone());
        idx += 1;
    }
    Ok(parsed)
}

/// purpose: Consume one CMUX right-sidebar flag if the current token is a flag.
/// inputs: Full token list, mutable index, and parsed-args accumulator.
/// returns/effects: Advances the index when a flag is consumed; errors on unknown flags.
fn consume_right_sidebar_option(
    args: &[String],
    idx: &mut usize,
    parsed: &mut RightSidebarArgs,
) -> Result<bool> {
    let value = args[*idx].as_str();
    match value {
        "--workspace" | "--tab" => {
            parsed.workspace = Some(right_sidebar_required_value(args, *idx, "--workspace")?);
            *idx += 2;
            Ok(true)
        }
        "--window" => {
            parsed.window = Some(right_sidebar_required_value(args, *idx, "--window")?);
            *idx += 2;
            Ok(true)
        }
        "--no-focus" => {
            parsed.no_focus = true;
            *idx += 1;
            Ok(true)
        }
        value if value.starts_with("--workspace=") => {
            parsed.workspace = Some(value["--workspace=".len()..].to_string());
            *idx += 1;
            Ok(true)
        }
        value if value.starts_with("--tab=") => {
            parsed.workspace = Some(value["--tab=".len()..].to_string());
            *idx += 1;
            Ok(true)
        }
        value if value.starts_with("--window=") => {
            parsed.window = Some(value["--window=".len()..].to_string());
            *idx += 1;
            Ok(true)
        }
        value if value.starts_with("--") => bail!("right-sidebar: unknown flag '{value}'"),
        _ => Ok(false),
    }
}

/// purpose: Read a required value after a CMUX right-sidebar flag.
/// inputs: Full token list, flag index, and user-facing flag name.
/// returns/effects: Returns the next token or a flag-specific missing-value error.
fn right_sidebar_required_value(args: &[String], idx: usize, flag: &str) -> Result<String> {
    args.get(idx + 1)
        .cloned()
        .ok_or_else(|| anyhow!("right-sidebar: {flag} requires an id"))
}

/// purpose: Validate and normalize the CMUX right-sidebar mode vocabulary.
/// inputs: Raw CLI mode.
/// returns/effects: Returns the lower-case mode or a hard invalid-mode error.
fn normalize_right_sidebar_mode(raw: &str) -> Result<String> {
    let mode = raw.trim().to_ascii_lowercase();
    match mode.as_str() {
        "files" | "find" | "vault" | "sessions" | "feed" | "dock" => Ok(mode),
        _ => bail!("Unknown right-sidebar mode '{raw}'"),
    }
}

async fn run_new_surface(client: &mut Client, args: &[String]) -> Result<Value> {
    let workspace = parse_opt(args, "--workspace");
    let command = parse_opt(args, "--command");
    let mut params = Map::new();
    if let Some(command) = command {
        params.insert("command".to_string(), Value::String(command));
    }
    call_in_workspace_scope(client, workspace, "surface.create", Value::Object(params)).await
}

fn env_opt(name: &str) -> Option<String> {
    context_env_value(name)
}

fn nonempty(value: Option<String>) -> Option<String> {
    value.filter(|s| !s.trim().is_empty())
}

/// purpose: Read context variables from an injected environment lookup.
/// inputs: env_lookup is usually process env; name is a Limux context key.
/// returns/effects: Returns Limux value first, then CMUX-compatible alias.
fn context_lookup(env_lookup: &impl Fn(&str) -> Option<String>, name: &str) -> Option<String> {
    env_lookup(name).or_else(|| {
        let cmux_name = match name {
            "LIMUX_WORKSPACE_ID" => "CMUX_WORKSPACE_ID",
            "LIMUX_SURFACE_ID" => "CMUX_SURFACE_ID",
            "LIMUX_TAB_ID" => "CMUX_TAB_ID",
            "LIMUX_SOCKET" => "CMUX_SOCKET_PATH",
            _ => return None,
        };
        env_lookup(cmux_name)
    })
}

fn build_new_pane_request(
    args: &[String],
    env_lookup: impl Fn(&str) -> Option<String>,
) -> (Option<String>, Value) {
    let workspace = nonempty(
        parse_opt(args, "--workspace")
            .or_else(|| context_lookup(&env_lookup, "LIMUX_WORKSPACE_ID")),
    );
    let surface = nonempty(
        parse_opt(args, "--surface").or_else(|| context_lookup(&env_lookup, "LIMUX_SURFACE_ID")),
    );
    let pane = nonempty(parse_opt(args, "--pane").or_else(|| env_lookup("LIMUX_PANE_ID")));
    let direction = parse_opt(args, "--direction").unwrap_or_else(|| "right".to_string());
    let pane_type = parse_opt(args, "--type").unwrap_or_else(|| "terminal".to_string());
    let command = nonempty(parse_opt(args, "--command"));
    let url = nonempty(parse_opt(args, "--url"));

    let mut params = Map::new();
    params.insert("direction".to_string(), Value::String(direction));
    params.insert("type".to_string(), Value::String(pane_type));
    if let Some(surface) = surface {
        params.insert("surface_id".to_string(), Value::String(surface));
    }
    if let Some(pane) = pane {
        params.insert("pane_id".to_string(), Value::String(pane));
    }
    if let Some(command) = command {
        params.insert("command".to_string(), Value::String(command));
    }
    if let Some(url) = url {
        params.insert("url".to_string(), Value::String(url));
    }

    (workspace, Value::Object(params))
}

async fn run_new_pane(client: &mut Client, args: &[String]) -> Result<Value> {
    // `pane.create` contract shared with the core dispatcher and live GTK host:
    // direction/type are validated by the server, and responses keep
    // pane_id/pane_ref/surface_id/surface_ref. Inside a Limux terminal,
    // LIMUX_* defaults make `limux new-pane --command claude` split the
    // caller's pane; outside Limux, omitting workspace preserves active-focus
    // server behavior.
    let (workspace, params) = build_new_pane_request(args, env_opt);
    call_in_workspace_scope(client, workspace, "pane.create", params).await
}

// purpose: Convert CMUX read-screen/capture-pane flags into surface.read_text params.
// inputs: CLI arguments after the command name.
// returns/effects: Validates --lines and returns socket params without contacting the host.
fn build_read_screen_params(args: &[String]) -> Result<Map<String, Value>> {
    let lines = if let Some(lines) = parse_opt(args, "--lines") {
        let parsed = lines.parse::<u64>().unwrap_or(0);
        if parsed == 0 {
            bail!("--lines must be greater than 0");
        }
        Some(parsed)
    } else {
        None
    };

    let workspace = parse_opt(args, "--workspace");
    let surface = parse_opt(args, "--surface");
    let scrollback = parse_flag(args, "--scrollback") || lines.is_some();
    let mut params = Map::new();
    if let Some(workspace) = workspace {
        params.insert("workspace_id".to_string(), Value::String(workspace));
    }
    if let Some(surface) = surface {
        params.insert("surface_id".to_string(), Value::String(surface));
    }
    if let Some(lines) = lines {
        params.insert("lines".to_string(), Value::Number(lines.into()));
    }
    if scrollback {
        params.insert("scrollback".to_string(), Value::Bool(true));
    }
    Ok(params)
}

async fn run_read_screen(client: &mut Client, args: &[String]) -> Result<Value> {
    client
        .call(
            "surface.read_text",
            Value::Object(build_read_screen_params(args)?),
        )
        .await
}

/// purpose: Relay arbitrary JSON-RPC calls using the CMUX-compatible `rpc` command.
/// inputs: args contain a method name plus optional JSON params.
/// returns/effects: Sends the request to the configured Limux socket and returns the result.
async fn run_rpc_command(client: &mut Client, args: &[String]) -> Result<Value> {
    let method = args
        .first()
        .ok_or_else(|| anyhow!("rpc requires a method name"))?;
    let params = if let Some(raw) = args.get(1) {
        serde_json::from_str::<Value>(raw).context("rpc params must be valid JSON")?
    } else {
        json!({})
    };
    client.call(method, params).await
}

/// purpose: Build CMUX event stream params from CLI flags.
/// inputs: events command arguments.
/// returns/effects: Reads cursor file when provided; does not contact the socket.
fn build_events_stream_params(args: &[String]) -> Result<Value> {
    let mut params = Map::new();
    let after =
        if let Some(raw) = parse_opt(args, "--after").or_else(|| parse_opt(args, "--after-seq")) {
            Some(
                raw.parse::<u64>()
                    .with_context(|| format!("event sequence must be an integer, got {raw}"))?,
            )
        } else {
            read_cursor_file_arg(args).transpose()?
        };
    if let Some(after) = after {
        params.insert("after_seq".to_string(), Value::Number(after.into()));
    }
    let names = parse_opts(args, "--name");
    if !names.is_empty() {
        params.insert("names".to_string(), json!(names));
    }
    let categories = parse_opts(args, "--category");
    if !categories.is_empty() {
        params.insert("categories".to_string(), json!(categories));
    }
    if parse_flag(args, "--no-heartbeat") {
        params.insert("include_heartbeats".to_string(), Value::Bool(false));
    }
    Ok(Value::Object(params))
}

fn read_cursor_file_arg(args: &[String]) -> Option<Result<u64>> {
    parse_opt(args, "--cursor-file").map(|path| {
        let raw = fs::read_to_string(&path)
            .with_context(|| format!("failed to read cursor file {path}"))?;
        raw.trim()
            .parse::<u64>()
            .with_context(|| format!("cursor file {path} must contain an integer sequence"))
    })
}

fn parse_events_limit(args: &[String]) -> Result<Option<usize>> {
    parse_opt(args, "--limit")
        .map(|raw| {
            raw.parse::<usize>()
                .with_context(|| format!("--limit must be a non-negative integer, got {raw}"))
        })
        .transpose()
}

fn update_events_cursor(args: &[String], frame: &Value) -> Result<()> {
    let Some(path) = parse_opt(args, "--cursor-file") else {
        return Ok(());
    };
    let Some(seq) = frame.get("seq").and_then(Value::as_u64) else {
        return Ok(());
    };
    if let Some(parent) = Path::new(&path).parent() {
        fs::create_dir_all(parent)
            .with_context(|| format!("failed to create cursor directory {}", parent.display()))?;
    }
    fs::write(&path, seq.to_string()).with_context(|| format!("failed to write cursor file {path}"))
}

/// purpose: Run the CMUX-compatible event stream CLI command.
/// inputs: Socket client metadata and events command arguments.
/// returns/effects: Prints JSONL frames from the stream and updates cursor files for event frames.
async fn run_events(client: &Client, args: &[String]) -> Result<CommandOutput> {
    let reconnect = parse_flag(args, "--reconnect");
    let mut printed = Vec::new();
    let limit = parse_events_limit(args)?;
    let hide_ack = parse_flag(args, "--no-ack");
    let mut resume_after_seq: Option<u64> = None;

    loop {
        let mut params = build_events_stream_params(args)?;
        if let Some(seq) = resume_after_seq {
            params["after_seq"] = Value::Number(seq.into());
        }
        let request = V2Request {
            id: Some(Value::String("events-cli".to_string())),
            method: "events.stream".to_string(),
            params,
        };
        let mut payload =
            serde_json::to_string(&request).context("failed to encode events request")?;
        payload.push('\n');

        let stream = UnixStream::connect(&client.socket)
            .await
            .with_context(|| format!("failed to connect to socket {}", client.socket.display()))?;
        let (reader_half, mut writer_half) = stream.into_split();
        writer_half
            .write_all(payload.as_bytes())
            .await
            .context("failed to write events request")?;
        writer_half
            .flush()
            .await
            .context("failed to flush events request")?;

        let mut reader = BufReader::new(reader_half);
        let mut line = String::new();
        while reader
            .read_line(&mut line)
            .await
            .context("failed to read events frame")?
            > 0
        {
            let trimmed = line.trim_end_matches(['\n', '\r']);
            if !trimmed.is_empty() {
                let frame: Value =
                    serde_json::from_str(trimmed).context("event frame was not JSON")?;
                update_events_cursor(args, &frame)?;
                if let Some(seq) = frame.get("seq").and_then(Value::as_u64) {
                    resume_after_seq = Some(seq);
                }
                let is_ack = frame.get("type").and_then(Value::as_str) == Some("ack");
                if !(hide_ack && is_ack) {
                    printed.push(trimmed.to_string());
                    if limit.is_some_and(|max| printed.len() >= max) {
                        return Ok(CommandOutput::Text(printed.join("\n")));
                    }
                }
            }
            line.clear();
        }
        if !reconnect {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(250)).await;
    }
    Ok(CommandOutput::Text(printed.join("\n")))
}

/// purpose: Build a CMUX-compatible surface lifecycle request.
/// inputs: command is a CMUX alias and args are its CLI flags/positionals.
/// returns/effects: Returns the target Limux method plus JSON params.
fn build_surface_alias_request(
    command: &str,
    args: &[String],
) -> Result<Option<(&'static str, Value)>> {
    let method = match command {
        "focus-panel" => "surface.focus",
        "close-surface" => "surface.close",
        "move-surface" => "surface.move",
        "reorder-surface" => "surface.reorder",
        "split-off" | "drag-surface-to-split" => "surface.drag_to_split",
        "new-split" => "surface.split",
        "refresh-surfaces" => "surface.refresh",
        _ => return Ok(None),
    };

    let mut params = Map::new();
    if let Some(workspace) =
        parse_opt(args, "--workspace").or_else(|| context_env_value("LIMUX_WORKSPACE_ID"))
    {
        if !workspace.trim().is_empty() {
            params.insert("workspace_id".to_string(), Value::String(workspace));
        }
    }
    if let Some(surface) = surface_arg(args) {
        params.insert("surface_id".to_string(), Value::String(surface));
    }
    if command == "new-split" {
        let direction = parse_opt(args, "--direction").unwrap_or_else(|| "right".to_string());
        params.insert("direction".to_string(), Value::String(direction));
    }
    if command == "split-off" || command == "drag-surface-to-split" {
        if !params.contains_key("surface_id") {
            bail!("{command} requires --surface, --panel, or a surface positional");
        }
        let direction = parse_opt(args, "--direction").unwrap_or_else(|| "right".to_string());
        params.insert("direction".to_string(), Value::String(direction));
    }
    if command == "move-surface" {
        let target_pane = parse_opt(args, "--target-pane").or_else(|| parse_opt(args, "--pane"));
        let Some(target_pane) = target_pane.filter(|value| !value.trim().is_empty()) else {
            bail!("move-surface requires --target-pane or --pane");
        };
        params.insert("target_pane_id".to_string(), Value::String(target_pane));
        if let Some(index) = parse_opt(args, "--index") {
            let parsed = index
                .parse::<u64>()
                .with_context(|| format!("invalid move-surface --index: {}", index))?;
            params.insert("index".to_string(), Value::Number(parsed.into()));
        }
    }
    if command == "reorder-surface" {
        if !params.contains_key("surface_id") {
            bail!("reorder-surface requires --surface, --panel, or a surface positional");
        }
        let index = parse_opt(args, "--index");
        let before_surface =
            parse_opt(args, "--before-surface").or_else(|| parse_opt(args, "--before"));
        let after_surface =
            parse_opt(args, "--after-surface").or_else(|| parse_opt(args, "--after"));
        let target_count = usize::from(index.is_some())
            + usize::from(before_surface.is_some())
            + usize::from(after_surface.is_some());
        if target_count != 1 {
            bail!(
                "reorder-surface requires exactly one of --index, --before-surface, or --after-surface"
            );
        }
        if let Some(index) = index {
            let parsed = index
                .parse::<u64>()
                .with_context(|| format!("invalid reorder-surface --index: {}", index))?;
            params.insert("index".to_string(), Value::Number(parsed.into()));
        }
        if let Some(before_surface) = before_surface {
            params.insert(
                "before_surface_id".to_string(),
                Value::String(before_surface),
            );
        }
        if let Some(after_surface) = after_surface {
            params.insert("after_surface_id".to_string(), Value::String(after_surface));
        }
    }
    if command == "focus-panel" && !params.contains_key("surface_id") {
        bail!("focus-panel requires --panel, --surface, or a surface positional");
    }
    Ok(Some((method, Value::Object(params))))
}

/// purpose: Build a CMUX-compatible window request.
/// inputs: command is a CMUX window alias and args may include --window or a positional id.
/// returns/effects: Returns the target Limux method plus JSON params.
fn build_window_alias_request(
    command: &str,
    args: &[String],
) -> Result<Option<(&'static str, Value)>> {
    let method = match command {
        "new-window" => "window.create",
        "current-window" => "window.current",
        "list-windows" => "window.list",
        "focus-window" => "window.focus",
        "close-window" => "window.close",
        _ => return Ok(None),
    };
    let mut params = Map::new();
    if let Some(window) = parse_opt(args, "--window").or_else(|| first_positional(args)) {
        params.insert("window_id".to_string(), Value::String(window));
    }
    Ok(Some((method, Value::Object(params))))
}

/// purpose: Apply a global CMUX `--window` option to command-local args.
/// inputs: command args and an optional global window selector.
/// returns/effects: Returns args with `--window` appended only when absent.
fn args_with_global_window(args: &[String], window: Option<&str>) -> Vec<String> {
    let mut merged = args.to_vec();
    if let Some(window) = window.filter(|value| !value.trim().is_empty()) {
        if parse_opt(args, "--window").is_none() {
            merged.push("--window".to_string());
            merged.push(window.to_string());
        }
    }
    merged
}

/// purpose: Build a CMUX-compatible workspace request where Limux already has an API.
/// inputs: command is a CMUX workspace alias and args may include --workspace.
/// returns/effects: Returns the target Limux method plus JSON params.
fn build_workspace_alias_request(
    command: &str,
    args: &[String],
) -> Result<Option<(&'static str, Value)>> {
    let method = match command {
        "capabilities" => "system.capabilities",
        "current-workspace" => "workspace.current",
        "select-workspace" => "workspace.select",
        _ => return Ok(None),
    };
    let mut params = Map::new();
    if command == "select-workspace" {
        let workspace = parse_opt(args, "--workspace")
            .or_else(|| first_positional(args))
            .ok_or_else(|| anyhow!("select-workspace requires --workspace or a workspace id"))?;
        params.insert("workspace_id".to_string(), Value::String(workspace));
    }
    Ok(Some((method, Value::Object(params))))
}

// purpose: Build CMUX workspace namespace requests.
// inputs: Arguments after `limux workspace`.
// returns/effects: Supports workspace env plus explicit remote reconnect/disconnect parity routes.
fn build_workspace_namespace_request(args: &[String]) -> Result<Option<(&'static str, Value)>> {
    let Some(subcommand) = args.first().map(String::as_str) else {
        return Ok(None);
    };
    match subcommand {
        "env" => {
            let rest = &args[1..];
            let mut params = Map::new();
            let workspace = parse_opt(rest, "--workspace").or_else(|| first_positional(rest));
            if let Some(workspace) = workspace {
                params.insert("workspace_id".to_string(), Value::String(workspace));
            }
            if parse_flag(rest, "--mask") {
                params.insert("mask".to_string(), Value::Bool(true));
            }
            Ok(Some(("workspace.env", Value::Object(params))))
        }
        "reconnect" | "disconnect" => {
            let rest = &args[1..];
            let mut params = Map::new();
            let workspace = parse_opt(rest, "--workspace").or_else(|| first_positional(rest));
            if let Some(workspace) = workspace {
                params.insert("workspace_id".to_string(), Value::String(workspace));
            }
            let method = if subcommand == "reconnect" {
                "workspace.remote.reconnect"
            } else {
                "workspace.remote.disconnect"
            };
            Ok(Some((method, Value::Object(params))))
        }
        _ => Ok(None),
    }
}

// purpose: Render workspace env output in a shell-friendly CMUX-compatible form.
// inputs: workspace.env response payload.
// returns/effects: Returns KEY=VALUE lines sorted by key.
fn render_workspace_env_text(payload: &Value) -> String {
    let Some(environment) = payload.get("environment").and_then(Value::as_object) else {
        return default_text_output(payload);
    };
    if environment.is_empty() {
        return String::new();
    }
    environment
        .iter()
        .filter_map(|(key, value)| value.as_str().map(|value| format!("{key}={value}")))
        .collect::<Vec<_>>()
        .join("\n")
}

/// purpose: Build a CMUX-compatible pane surface-list request.
/// inputs: args may include --pane or a positional pane id.
/// returns/effects: Returns JSON params for pane.surfaces.
fn build_list_pane_surfaces_request(args: &[String]) -> Result<Value> {
    let pane = parse_opt(args, "--pane")
        .or_else(|| first_positional(args))
        .ok_or_else(|| anyhow!("list-pane-surfaces requires --pane or a pane id"))?;
    Ok(json!({ "pane_id": pane }))
}

/// purpose: Build a CMUX-compatible notification lifecycle request.
/// inputs: command is a notification alias and args are CMUX-style flags.
/// returns/effects: Returns the target Limux method plus JSON params.
fn build_notification_alias_request(
    command: &str,
    args: &[String],
) -> Result<Option<(&'static str, Value)>> {
    let mut params = Map::new();
    let method = match command {
        "list-notifications" => {
            if parse_flag(args, "--unread") || parse_flag(args, "--unread-only") {
                params.insert("unread_only".to_string(), Value::Bool(true));
            }
            "notification.list"
        }
        "dismiss-notification" => {
            if parse_flag(args, "--all-read") {
                params.insert("all_read".to_string(), Value::Bool(true));
            }
            if let Some(id) = parse_opt(args, "--id").or_else(|| first_positional(args)) {
                params.insert("id".to_string(), Value::String(id));
            }
            "notification.dismiss"
        }
        "mark-notification-read" => {
            if parse_flag(args, "--all") {
                params.insert("all".to_string(), Value::Bool(true));
            }
            if let Some(id) = parse_opt(args, "--id").or_else(|| first_positional(args)) {
                params.insert("id".to_string(), Value::String(id));
            }
            if let Some(workspace) = parse_opt(args, "--workspace") {
                params.insert("workspace_id".to_string(), Value::String(workspace));
            }
            if let Some(surface) = parse_opt(args, "--surface") {
                params.insert("surface_id".to_string(), Value::String(surface));
            }
            "notification.mark_read"
        }
        "open-notification" => {
            let id = parse_opt(args, "--id")
                .or_else(|| first_positional(args))
                .ok_or_else(|| anyhow!("open-notification requires --id or an id positional"))?;
            params.insert("id".to_string(), Value::String(id));
            "notification.open"
        }
        "jump-to-unread" => "notification.jump_to_unread",
        "clear-notifications" => {
            if let Some(id) = parse_opt(args, "--id").or_else(|| first_positional(args)) {
                params.insert("id".to_string(), Value::String(id));
            }
            "notification.clear"
        }
        _ => return Ok(None),
    };
    Ok(Some((method, Value::Object(params))))
}

/// purpose: Pick the first non-option positional argument from a small alias command.
/// inputs: raw command args with common option/value pairs.
/// returns/effects: Returns the first positional value when present.
fn first_positional(args: &[String]) -> Option<String> {
    let value_options = [
        "--workspace",
        "--surface",
        "--panel",
        "--pane",
        "--window",
        "--direction",
        "--type",
        "--url",
        "--command",
        "--title",
        "--body",
        "--id",
        "--key",
        "--text",
        "--target-pane",
        "--index",
        "--before-surface",
        "--before",
        "--after-surface",
        "--after",
    ];
    let mut skip = false;
    for arg in args {
        if skip {
            skip = false;
            continue;
        }
        if value_options.contains(&arg.as_str()) {
            skip = true;
            continue;
        }
        if arg.starts_with('-') {
            continue;
        }
        return Some(arg.clone());
    }
    None
}

/// purpose: Resolve CMUX `panel` terminology to Limux surface ids.
/// inputs: CLI args may use --panel, --surface, or a surface positional.
/// returns/effects: Returns the requested surface handle when present.
fn surface_arg(args: &[String]) -> Option<String> {
    parse_opt(args, "--surface")
        .or_else(|| parse_opt(args, "--panel"))
        .or_else(|| first_positional(args))
        .or_else(|| context_env_value("LIMUX_SURFACE_ID"))
        .filter(|value| !value.trim().is_empty())
}

async fn run_rename_workspace_like(
    client: &mut Client,
    command: &str,
    args: &[String],
) -> Result<Value> {
    let workspace =
        parse_opt(args, "--workspace").or_else(|| context_env_value("LIMUX_WORKSPACE_ID"));
    let title = trailing_title(args).ok_or_else(|| {
        if command == "rename-window" {
            anyhow!("rename-window requires a title")
        } else {
            anyhow!("rename-workspace requires a title")
        }
    })?;

    let mut params = Map::new();
    params.insert("title".to_string(), Value::String(title));
    if let Some(workspace) = workspace {
        params.insert("workspace_id".to_string(), Value::String(workspace));
    }

    client.call("workspace.rename", Value::Object(params)).await
}

async fn run_rename_tab(client: &mut Client, args: &[String]) -> Result<Value> {
    let workspace = parse_opt(args, "--workspace")
        .or_else(|| context_env_value("LIMUX_WORKSPACE_ID"))
        .unwrap_or_default();
    let tab = parse_opt(args, "--tab")
        .or_else(|| context_env_value("LIMUX_TAB_ID"))
        .unwrap_or_default();
    let title = trailing_title(args).ok_or_else(|| anyhow!("rename-tab requires a title"))?;

    let mut params = Map::new();
    params.insert("action".to_string(), Value::String("rename".to_string()));
    params.insert("title".to_string(), Value::String(title));
    if !workspace.is_empty() {
        params.insert("workspace_id".to_string(), Value::String(workspace));
    }
    if !tab.is_empty() {
        params.insert("surface_id".to_string(), Value::String(tab));
    }

    client.call("tab.action", Value::Object(params)).await
}

async fn run_tab_action(client: &mut Client, args: &[String]) -> Result<Value> {
    if parse_flag(args, "--help") {
        return Ok(json!({
            "help": "Usage: limux tab-action --action <name> [--workspace <id|ref>] [--tab <id|ref>] [--title <text>] [--url <url>]\nTarget tab:\n  --tab tab:<n>       Stable tab reference alias\n  --tab surface:<n>   Surface alias (legacy-compatible)\nExamples:\n  limux tab-action --workspace workspace:2 --tab tab:1 --action pin\n  limux tab-action --tab tab:3 --action mark-unread"
        }));
    }

    let action = parse_opt(args, "--action")
        .ok_or_else(|| anyhow!("tab-action requires --action <name>"))?;
    let workspace =
        parse_opt(args, "--workspace").or_else(|| context_env_value("LIMUX_WORKSPACE_ID"));
    let tab = parse_opt(args, "--tab").or_else(|| context_env_value("LIMUX_TAB_ID"));
    let title = parse_opt(args, "--title").or_else(|| trailing_title(args));
    let url = parse_opt(args, "--url");

    if action == "new-terminal-right" || action == "new-browser-right" {
        let pane_type = if action == "new-browser-right" {
            "browser"
        } else {
            "terminal"
        };
        let mut params = vec![
            "--direction".to_string(),
            "right".to_string(),
            "--type".to_string(),
            pane_type.to_string(),
        ];
        if let Some(workspace) = workspace.clone() {
            params.push("--workspace".to_string());
            params.push(workspace);
        }
        if let Some(url) = url {
            params.push("--url".to_string());
            params.push(url);
        }
        let created = run_new_pane(client, &params).await?;
        let tab_ref = tab.unwrap_or_else(|| "tab:1".to_string());
        return Ok(json!({
            "tab_ref": tab_ref,
            "surface_id": created.get("surface_id").cloned().unwrap_or(Value::Null),
            "surface_ref": created.get("surface_ref").cloned().unwrap_or(Value::Null),
        }));
    }

    let mut params = Map::new();
    params.insert("action".to_string(), Value::String(action.clone()));
    if let Some(workspace) = workspace {
        params.insert("workspace_id".to_string(), Value::String(workspace));
    }
    if let Some(tab) = tab.clone() {
        params.insert("surface_id".to_string(), Value::String(tab));
    }
    if let Some(title) = title {
        params.insert("title".to_string(), Value::String(title));
    }

    let mut payload = client.call("tab.action", Value::Object(params)).await?;
    if let Some(obj) = payload.as_object_mut() {
        if !obj.contains_key("tab_ref") {
            obj.insert(
                "tab_ref".to_string(),
                Value::String(tab.unwrap_or_else(|| "tab:1".to_string())),
            );
        }
        if action == "pin" {
            obj.insert("pinned".to_string(), Value::Bool(true));
        }
        if action == "unpin" {
            obj.insert("pinned".to_string(), Value::Bool(false));
        }
    }
    Ok(payload)
}

async fn run_browser(
    client: &mut Client,
    args: &[String],
    json_output: bool,
) -> Result<CommandOutput> {
    let mut browser_args = args.to_vec();
    let mut local_json = json_output;

    loop {
        if browser_args.last().map(|s| s.as_str()) == Some("--json") {
            local_json = true;
            browser_args.pop();
            continue;
        }
        break;
    }

    let workspace = parse_opt(&browser_args, "--workspace");
    let mut surface = parse_opt(&browser_args, "--surface");

    let mut positional: Vec<String> = Vec::new();
    let mut skip = false;
    for (idx, arg) in browser_args.iter().enumerate() {
        if skip {
            skip = false;
            continue;
        }
        match arg.as_str() {
            "--workspace" | "--surface" | "--id-format" | "--timeout-ms" | "--load-state"
            | "--url-contains" | "--function" | "--max-depth" | "--out" | "--path"
            | "--timeout" => {
                if idx + 1 < browser_args.len() {
                    skip = true;
                }
            }
            value if value.starts_with('-') => {}
            _ => positional.push(arg.clone()),
        }
    }

    if positional.is_empty() {
        bail!("browser requires a subcommand");
    }

    let mut pos_idx = 0usize;
    let first = positional[0].clone();
    let verbs_without_surface = ["open", "open-split", "new", "identify"];

    if !verbs_without_surface.contains(&first.as_str()) {
        if !first.contains(':') && !first.contains('-') {
            // probably still subcommand
        } else {
            surface = Some(first);
            pos_idx = 1;
        }
    }

    if pos_idx >= positional.len() {
        bail!("browser requires a subcommand");
    }
    let sub = positional[pos_idx].clone();
    let rest = positional[(pos_idx + 1)..].to_vec();
    if let Some(method) = unsupported_browser_cli_method(&sub, &rest) {
        bail!("not_supported: {method}");
    }

    let output = match sub.as_str() {
        "open" | "open-split" | "new" => {
            let url = rest
                .first()
                .cloned()
                .unwrap_or_else(|| "about:blank".to_string());
            if let Some(surface) = surface.clone() {
                let payload = browser_call(client, Some(surface), "browser.navigate", {
                    let mut p = Map::new();
                    p.insert("url".to_string(), Value::String(url));
                    p
                })
                .await?;
                CommandOutput::Json(payload)
            } else {
                let payload = call_in_workspace_scope(
                    client,
                    workspace.clone(),
                    "browser.open_split",
                    json!({ "url": url }),
                )
                .await?;
                CommandOutput::Json(payload)
            }
        }
        "url" | "get-url" => {
            let sid = surface
                .clone()
                .ok_or_else(|| anyhow!("browser url requires a surface"))?;
            let payload = browser_call(client, Some(sid), "browser.url.get", Map::new()).await?;
            if local_json {
                CommandOutput::Json(payload)
            } else {
                CommandOutput::Text(get_string(&payload, &["url"]).unwrap_or_default())
            }
        }
        "focus-webview" => {
            let sid = surface
                .clone()
                .ok_or_else(|| anyhow!("browser focus-webview requires a surface"))?;
            let payload =
                browser_call(client, Some(sid), "browser.focus_webview", Map::new()).await?;
            CommandOutput::Json(payload)
        }
        "is-webview-focused" => {
            let sid = surface
                .clone()
                .ok_or_else(|| anyhow!("browser is-webview-focused requires a surface"))?;
            let payload =
                browser_call(client, Some(sid), "browser.is_webview_focused", Map::new()).await?;
            if local_json {
                CommandOutput::Json(payload)
            } else {
                CommandOutput::Text(
                    payload
                        .get("focused")
                        .and_then(Value::as_bool)
                        .unwrap_or(false)
                        .to_string(),
                )
            }
        }
        "eval" => {
            let sid = surface
                .clone()
                .ok_or_else(|| anyhow!("browser eval requires a surface"))?;
            let script = rest
                .first()
                .cloned()
                .ok_or_else(|| anyhow!("browser eval requires a script"))?;
            let payload = browser_call(client, Some(sid), "browser.eval", {
                let mut p = Map::new();
                p.insert("script".to_string(), Value::String(script));
                p
            })
            .await?;
            if local_json {
                CommandOutput::Json(payload)
            } else {
                CommandOutput::Text(
                    payload
                        .get("value")
                        .map(|value| {
                            value
                                .as_str()
                                .map(ToOwned::to_owned)
                                .unwrap_or_else(|| value.to_string())
                        })
                        .unwrap_or_default(),
                )
            }
        }
        "goto" | "navigate" => {
            let sid = surface
                .clone()
                .ok_or_else(|| anyhow!("browser navigate requires a surface"))?;
            let url = rest
                .first()
                .cloned()
                .ok_or_else(|| anyhow!("browser navigate requires a URL"))?;
            let payload = browser_call(client, Some(sid.clone()), "browser.navigate", {
                let mut p = Map::new();
                p.insert("url".to_string(), Value::String(url));
                p
            })
            .await?;
            if parse_flag(&browser_args, "--snapshot-after") {
                let snap = browser_call(client, Some(sid), "browser.snapshot", Map::new()).await?;
                if local_json {
                    let mut merged = payload;
                    if let Some(obj) = merged.as_object_mut() {
                        obj.insert("post_action_snapshot".to_string(), snap);
                    }
                    CommandOutput::Json(merged)
                } else {
                    CommandOutput::Text(
                        get_string(&snap, &["snapshot", "text"])
                            .unwrap_or_else(|| "OK".to_string()),
                    )
                }
            } else {
                CommandOutput::Json(payload)
            }
        }
        "wait" => {
            let sid = surface
                .clone()
                .ok_or_else(|| anyhow!("browser wait requires a surface"))?;
            let mut p = Map::new();
            if let Some(selector) = parse_opt(&browser_args, "--selector") {
                p.insert("selector".to_string(), Value::String(selector));
            }
            if let Some(text) = parse_opt(&browser_args, "--text") {
                p.insert("text".to_string(), Value::String(text));
            }
            if let Some(url_contains) = parse_opt(&browser_args, "--url-contains") {
                p.insert("url_contains".to_string(), Value::String(url_contains));
            }
            if let Some(load_state) = parse_opt(&browser_args, "--load-state") {
                p.insert("load_state".to_string(), Value::String(load_state));
            }
            if let Some(function) = parse_opt(&browser_args, "--function") {
                p.insert("function".to_string(), Value::String(function));
            }
            if let Some(timeout_ms) = parse_opt(&browser_args, "--timeout-ms") {
                if let Ok(ms) = timeout_ms.parse::<u64>() {
                    p.insert("timeout_ms".to_string(), Value::Number(ms.into()));
                }
            }
            let payload = browser_call(client, Some(sid), "browser.wait", p).await?;
            if local_json {
                CommandOutput::Json(payload)
            } else {
                CommandOutput::Text("OK".to_string())
            }
        }
        "snapshot" => {
            let sid = surface
                .clone()
                .ok_or_else(|| anyhow!("browser snapshot requires a surface"))?;
            let mut p = Map::new();
            if parse_flag(&browser_args, "--interactive") {
                p.insert("interactive".to_string(), Value::Bool(true));
            }
            if parse_flag(&browser_args, "--compact") {
                p.insert("compact".to_string(), Value::Bool(true));
            }
            if let Some(max_depth) = parse_opt(&browser_args, "--max-depth") {
                if let Ok(depth) = max_depth.parse::<u64>() {
                    p.insert("max_depth".to_string(), Value::Number(depth.into()));
                }
            }
            let payload = browser_call(client, Some(sid), "browser.snapshot", p).await?;
            if local_json {
                CommandOutput::Json(payload)
            } else {
                let url = get_string(&payload, &["url"]).unwrap_or_default();
                if parse_flag(&browser_args, "--interactive") && url == "about:blank" {
                    CommandOutput::Text("about:blank\nNo interactive elements found; try `browser <surface> get url`.".to_string())
                } else if parse_flag(&browser_args, "--interactive") {
                    let mut text = get_string(&payload, &["snapshot", "text"])
                        .unwrap_or_else(|| "OK".to_string());
                    if let Some(refs) = payload
                        .get("snapshot")
                        .and_then(|snapshot| snapshot.get("refs"))
                        .and_then(Value::as_object)
                    {
                        for key in refs.keys() {
                            text.push_str(&format!("\nref={}", key));
                        }
                    }
                    CommandOutput::Text(text)
                } else {
                    CommandOutput::Text(
                        get_string(&payload, &["snapshot", "text"])
                            .unwrap_or_else(|| "OK".to_string()),
                    )
                }
            }
        }
        "screenshot" => {
            let sid = surface
                .clone()
                .ok_or_else(|| anyhow!("browser screenshot requires a surface"))?;
            let out = parse_opt(&browser_args, "--out");
            let mut params = Map::new();
            if let Some(out_path) = out.clone() {
                params.insert("path".to_string(), Value::String(out_path));
            }
            let mut payload = browser_call(client, Some(sid), "browser.screenshot", params).await?;
            let path = get_string(&payload, &["path"])
                .ok_or_else(|| anyhow!("browser screenshot response missing path"))?;
            if !Path::new(&path).exists() {
                bail!("browser screenshot response path does not exist: {path}");
            }
            let url = format!("file://{}", path);
            if let Some(obj) = payload.as_object_mut() {
                obj.insert("path".to_string(), Value::String(path.clone()));
                obj.insert("url".to_string(), Value::String(url.clone()));
                obj.remove("png_base64");
            }
            if out.is_some() {
                CommandOutput::Text(format!("OK {}", path))
            } else if local_json {
                CommandOutput::Json(payload)
            } else {
                CommandOutput::Text(path)
            }
        }
        "find" => {
            let sid = surface
                .clone()
                .ok_or_else(|| anyhow!("browser find requires a surface"))?;
            let locator = rest.first().cloned().unwrap_or_else(|| "text".to_string());
            let value = rest.get(1).cloned().unwrap_or_default();
            let method = format!("browser.find.{}", locator);
            let mut params = Map::new();
            match locator.as_str() {
                "role" => {
                    params.insert("role".to_string(), Value::String(value));
                }
                "nth" => {
                    params.insert(
                        "selector".to_string(),
                        Value::String(rest.get(1).cloned().unwrap_or_default()),
                    );
                    let index = rest.get(2).and_then(|v| v.parse::<u64>().ok()).unwrap_or(0);
                    params.insert("index".to_string(), Value::Number(index.into()));
                }
                "first" | "last" => {
                    params.insert("selector".to_string(), Value::String(value));
                }
                _ => {
                    params.insert(locator.clone(), Value::String(value));
                }
            }
            let payload = browser_call(client, Some(sid), &method, params).await?;
            if local_json {
                CommandOutput::Json(payload)
            } else {
                CommandOutput::Text(
                    get_string(&payload, &["element_ref"]).unwrap_or_else(|| "@e1".to_string()),
                )
            }
        }
        "is" => {
            let sid = surface
                .clone()
                .ok_or_else(|| anyhow!("browser is requires a surface"))?;
            let requested_state = rest
                .first()
                .cloned()
                .unwrap_or_else(|| "visible".to_string());
            let known_state = matches!(requested_state.as_str(), "visible" | "enabled" | "checked");
            let state_name = if known_state {
                requested_state.as_str()
            } else {
                "visible"
            };
            let selector = parse_opt(&browser_args, "--selector")
                .or_else(|| rest.get(usize::from(known_state)).cloned())
                .ok_or_else(|| anyhow!("browser is {} requires a selector", state_name))?;
            let method = format!("browser.is.{}", state_name);
            let payload = browser_call(client, Some(sid), &method, {
                let mut p = Map::new();
                p.insert("selector".to_string(), Value::String(selector));
                p
            })
            .await?;
            if local_json {
                CommandOutput::Json(payload)
            } else {
                let value = payload
                    .get(state_name)
                    .or_else(|| payload.get("value"))
                    .and_then(Value::as_bool)
                    .unwrap_or(false);
                CommandOutput::Text(value.to_string())
            }
        }
        "frame" => {
            let sid = surface
                .clone()
                .ok_or_else(|| anyhow!("browser frame requires a surface"))?;
            let target = rest.first().cloned().unwrap_or_else(|| "main".to_string());
            let payload = if target == "main" {
                browser_call(client, Some(sid), "browser.frame.main", Map::new()).await?
            } else {
                browser_call(client, Some(sid), "browser.frame.select", {
                    let mut p = Map::new();
                    p.insert("selector".to_string(), Value::String(target));
                    p
                })
                .await?
            };
            CommandOutput::Json(payload)
        }
        "dialog" => {
            let sid = surface
                .clone()
                .ok_or_else(|| anyhow!("browser dialog requires a surface"))?;
            let verb = rest
                .first()
                .cloned()
                .ok_or_else(|| anyhow!("browser dialog requires accept or dismiss"))?;
            let (method, p) = match verb.as_str() {
                "accept" => {
                    let text = rest[1..].join(" ");
                    let mut p = Map::new();
                    if !text.trim().is_empty() {
                        p.insert("text".to_string(), Value::String(text));
                    }
                    ("browser.dialog.accept", p)
                }
                "dismiss" => ("browser.dialog.dismiss", Map::new()),
                other => bail!("Unsupported browser dialog subcommand: {}", other),
            };
            let payload = browser_call(client, Some(sid), method, p).await?;
            CommandOutput::Json(payload)
        }
        "download" => {
            let sid = surface
                .clone()
                .ok_or_else(|| anyhow!("browser download requires a surface"))?;
            let download_args = if rest.first().is_some_and(|arg| arg == "wait") {
                rest[1..].to_vec()
            } else {
                rest.clone()
            };
            let mut p = Map::new();
            if let Some(path) = parse_opt(&browser_args, "--path").or_else(|| {
                download_args
                    .iter()
                    .find(|arg| !arg.starts_with('-'))
                    .cloned()
            }) {
                p.insert("path".to_string(), Value::String(path));
            }
            if let Some(timeout_ms) = parse_opt(&browser_args, "--timeout-ms") {
                let value = timeout_ms
                    .parse::<u64>()
                    .with_context(|| "--timeout-ms must be an integer")?;
                p.insert("timeout_ms".to_string(), Value::Number(value.into()));
            } else if let Some(timeout) = parse_opt(&browser_args, "--timeout") {
                let seconds = timeout
                    .parse::<f64>()
                    .with_context(|| "--timeout must be a number")?;
                let millis = (seconds * 1000.0).max(1.0) as u64;
                p.insert("timeout_ms".to_string(), Value::Number(millis.into()));
            }
            let payload = browser_call(client, Some(sid), "browser.download.wait", p).await?;
            CommandOutput::Json(payload)
        }
        "click" | "dblclick" | "hover" | "focus" | "check" | "uncheck" | "scroll_into_view" => {
            let sid = surface
                .clone()
                .ok_or_else(|| anyhow!("browser {} requires a surface", sub))?;
            let selector = parse_opt(&browser_args, "--selector")
                .or_else(|| rest.first().cloned())
                .ok_or_else(|| anyhow!("browser {} requires a selector", sub))?;
            let payload = browser_call(client, Some(sid), &format!("browser.{}", sub), {
                let mut p = Map::new();
                p.insert("selector".to_string(), Value::String(selector));
                p
            })
            .await?;
            CommandOutput::Json(payload)
        }
        "fill" | "type" | "select" => {
            let sid = surface
                .clone()
                .ok_or_else(|| anyhow!("browser {} requires a surface", sub))?;
            let selector = parse_opt(&browser_args, "--selector")
                .or_else(|| rest.first().cloned())
                .ok_or_else(|| anyhow!("browser {} requires a selector", sub))?;
            let value_key = if sub == "select" { "value" } else { "text" };
            let value = parse_opt(&browser_args, "--text")
                .or_else(|| parse_opt(&browser_args, "--value"))
                .or_else(|| rest.get(1).cloned())
                .unwrap_or_default();
            let payload = browser_call(client, Some(sid), &format!("browser.{}", sub), {
                let mut p = Map::new();
                p.insert("selector".to_string(), Value::String(selector));
                p.insert(value_key.to_string(), Value::String(value));
                p
            })
            .await?;
            if parse_flag(&browser_args, "--snapshot-after") {
                let snap =
                    browser_call(client, surface.clone(), "browser.snapshot", Map::new()).await?;
                if local_json {
                    let mut merged = payload;
                    if let Some(obj) = merged.as_object_mut() {
                        obj.insert("post_action_snapshot".to_string(), snap);
                    }
                    CommandOutput::Json(merged)
                } else {
                    CommandOutput::Text(
                        get_string(&snap, &["snapshot", "text"])
                            .unwrap_or_else(|| "OK".to_string()),
                    )
                }
            } else {
                CommandOutput::Json(payload)
            }
        }
        "press" | "keydown" | "keyup" => {
            let sid = surface
                .clone()
                .ok_or_else(|| anyhow!("browser {} requires a surface", sub))?;
            let key = parse_opt(&browser_args, "--key")
                .or_else(|| rest.first().cloned())
                .ok_or_else(|| anyhow!("browser {} requires a key", sub))?;
            let payload = browser_call(client, Some(sid), &format!("browser.{}", sub), {
                let mut p = Map::new();
                p.insert("key".to_string(), Value::String(key));
                p
            })
            .await?;
            CommandOutput::Json(payload)
        }
        "scroll" => {
            let sid = surface
                .clone()
                .ok_or_else(|| anyhow!("browser scroll requires a surface"))?;
            let mut p = Map::new();
            if let Some(selector) = parse_opt(&browser_args, "--selector") {
                p.insert("selector".to_string(), Value::String(selector));
            }
            for (flag, key) in [("--dx", "dx"), ("--dy", "dy")] {
                if let Some(raw) = parse_opt(&browser_args, flag) {
                    let value = raw
                        .parse::<i64>()
                        .with_context(|| format!("browser scroll {flag} must be an integer"))?;
                    p.insert(key.to_string(), Value::Number(value.into()));
                }
            }
            let payload = browser_call(client, Some(sid), "browser.scroll", p).await?;
            CommandOutput::Json(payload)
        }
        "get" => {
            let sid = surface
                .clone()
                .ok_or_else(|| anyhow!("browser get requires a surface"))?;
            let get_verb = rest.first().cloned().unwrap_or_else(|| "url".to_string());
            let method = match get_verb.as_str() {
                "url" => "browser.url.get".to_string(),
                "title" => "browser.get.title".to_string(),
                "text" => "browser.get.text".to_string(),
                "html" => "browser.get.html".to_string(),
                "value" => "browser.get.value".to_string(),
                "attr" => "browser.get.attr".to_string(),
                "count" => "browser.get.count".to_string(),
                "box" => "browser.get.box".to_string(),
                "styles" => "browser.get.styles".to_string(),
                other => bail!("Unsupported browser get subcommand: {}", other),
            };
            let selector = rest
                .get(1)
                .cloned()
                .or_else(|| parse_opt(&browser_args, "--selector"));
            let mut p = Map::new();
            if let Some(selector) = selector {
                p.insert("selector".to_string(), Value::String(selector));
            }
            if let Some(attr) = parse_opt(&browser_args, "--attr") {
                p.insert("name".to_string(), Value::String(attr));
            }
            if let Some(property) = parse_opt(&browser_args, "--property") {
                p.insert("property".to_string(), Value::String(property));
            }
            let payload = browser_call(client, Some(sid), &method, p).await?;
            if local_json {
                CommandOutput::Json(payload)
            } else {
                let text = get_string(&payload, &["url", "title", "text", "value", "html"])
                    .unwrap_or_else(|| "OK".to_string());
                CommandOutput::Text(text)
            }
        }
        "cookies" => {
            let sid = surface
                .clone()
                .ok_or_else(|| anyhow!("browser cookies requires a surface"))?;
            let op = rest.first().cloned().unwrap_or_else(|| "get".to_string());
            let method = match op.as_str() {
                "get" => "browser.cookies.get",
                "set" => "browser.cookies.set",
                "clear" => "browser.cookies.clear",
                _ => bail!("Unsupported browser cookies subcommand: {}", op),
            };
            let mut p = Map::new();
            if let Some(name) = rest
                .get(1)
                .cloned()
                .or_else(|| parse_opt(&browser_args, "--name"))
            {
                p.insert("name".to_string(), Value::String(name));
            }
            if let Some(value) = rest
                .get(2)
                .cloned()
                .or_else(|| parse_opt(&browser_args, "--value"))
            {
                p.insert("value".to_string(), Value::String(value));
            }
            let payload = browser_call(client, Some(sid), method, p).await?;
            CommandOutput::Json(payload)
        }
        "storage" => {
            let sid = surface
                .clone()
                .ok_or_else(|| anyhow!("browser storage requires a surface"))?;
            if rest.len() < 2 {
                bail!("browser storage requires <local|session> <get|set|clear>");
            }
            let storage_type = rest[0].clone();
            let op = rest[1].clone();
            let method = match op.as_str() {
                "get" => "browser.storage.get",
                "set" => "browser.storage.set",
                "clear" => "browser.storage.clear",
                _ => bail!("Unsupported browser storage subcommand: {}", op),
            };
            let mut p = Map::new();
            p.insert("type".to_string(), Value::String(storage_type));
            if let Some(key) = rest.get(2) {
                p.insert("key".to_string(), Value::String(key.clone()));
            }
            if let Some(value) = rest.get(3) {
                p.insert("value".to_string(), Value::String(value.clone()));
            }
            let payload = browser_call(client, Some(sid), method, p).await?;
            CommandOutput::Json(payload)
        }
        "tab" => {
            let sid = surface
                .clone()
                .ok_or_else(|| anyhow!("browser tab requires a surface"))?;
            let tab_verb = rest.first().cloned().unwrap_or_else(|| "list".to_string());
            let (method, p) = match tab_verb.as_str() {
                "list" => ("browser.tab.list", Map::new()),
                "new" => {
                    let mut p = Map::new();
                    if let Some(url) = rest.get(1) {
                        p.insert("url".to_string(), Value::String(url.clone()));
                    }
                    ("browser.tab.new", p)
                }
                "switch" => {
                    let mut p = Map::new();
                    if let Some(target) = rest.get(1) {
                        p.insert(
                            "target_surface_id".to_string(),
                            Value::String(target.clone()),
                        );
                    }
                    ("browser.tab.switch", p)
                }
                "close" => {
                    let mut p = Map::new();
                    if let Some(target) = rest.get(1) {
                        p.insert(
                            "target_surface_id".to_string(),
                            Value::String(target.clone()),
                        );
                    }
                    ("browser.tab.close", p)
                }
                _ => bail!("Unsupported browser tab subcommand: {}", tab_verb),
            };
            let payload = browser_call(client, Some(sid), method, p).await?;
            CommandOutput::Json(payload)
        }
        "addscript" | "addinitscript" | "addstyle" => {
            let sid = surface
                .clone()
                .ok_or_else(|| anyhow!("browser {} requires a surface", sub))?;
            let content = rest.join(" ");
            if content.trim().is_empty() {
                bail!("browser {} requires content", sub);
            }
            let field = if sub == "addstyle" { "css" } else { "script" };
            let method = format!("browser.{}", sub);
            let mut p = Map::new();
            p.insert(field.to_string(), Value::String(content));
            let payload = browser_call(client, Some(sid), &method, p).await?;
            CommandOutput::Json(payload)
        }
        "console" | "errors" => {
            let sid = surface
                .clone()
                .ok_or_else(|| anyhow!("browser {} requires a surface", sub))?;
            let op = rest.first().cloned().unwrap_or_else(|| "list".to_string());
            let method = format!("browser.{}.{}", sub, op);
            let payload = browser_call(client, Some(sid), &method, Map::new()).await?;
            CommandOutput::Json(payload)
        }
        "highlight" => {
            let sid = surface
                .clone()
                .ok_or_else(|| anyhow!("browser highlight requires a surface"))?;
            let selector = rest.first().cloned().unwrap_or_default();
            let payload = browser_call(client, Some(sid), "browser.highlight", {
                let mut p = Map::new();
                p.insert("selector".to_string(), Value::String(selector));
                p
            })
            .await?;
            CommandOutput::Json(payload)
        }
        "state" => {
            let sid = surface
                .clone()
                .ok_or_else(|| anyhow!("browser state requires a surface"))?;
            let op = rest.first().cloned().unwrap_or_else(|| "save".to_string());
            let path = rest
                .get(1)
                .cloned()
                .ok_or_else(|| anyhow!("browser state {} requires a file path", op))?;
            let method = match op.as_str() {
                "save" => "browser.state.save",
                "load" => "browser.state.load",
                _ => bail!("Unsupported browser state subcommand: {}", op),
            };
            let payload = browser_call(client, Some(sid), method, {
                let mut p = Map::new();
                p.insert("path".to_string(), Value::String(path));
                p
            })
            .await?;
            CommandOutput::Json(payload)
        }
        _ => {
            // Generic passthrough to browser.<sub>
            let sid = surface
                .clone()
                .ok_or_else(|| anyhow!("browser {} requires a surface", sub))?;
            let method = format!("browser.{}", sub);
            let payload = browser_call(client, Some(sid), &method, Map::new()).await?;
            CommandOutput::Json(payload)
        }
    };

    Ok(output)
}

// purpose: Map CMUX browser CLI forms that upstream documents as unsupported.
// inputs: Parsed browser subcommand and its remaining positional arguments.
// returns/effects: Returns the exact RPC method that should fail with not_supported.
fn unsupported_browser_cli_method(sub: &str, rest: &[String]) -> Option<String> {
    match (sub, rest.first().map(String::as_str)) {
        ("viewport", Some("set")) => Some("browser.viewport.set".to_string()),
        ("geolocation", Some("set")) => Some("browser.geolocation.set".to_string()),
        ("offline", Some("set")) => Some("browser.offline.set".to_string()),
        ("trace", Some("start")) => Some("browser.trace.start".to_string()),
        ("trace", Some("stop")) => Some("browser.trace.stop".to_string()),
        ("network", Some("route")) => Some("browser.network.route".to_string()),
        ("network", Some("unroute")) => Some("browser.network.unroute".to_string()),
        ("network", Some("requests")) => Some("browser.network.requests".to_string()),
        ("screencast", Some("start")) => Some("browser.screencast.start".to_string()),
        ("screencast", Some("stop")) => Some("browser.screencast.stop".to_string()),
        ("input_mouse" | "input-mouse", _) => Some("browser.input_mouse".to_string()),
        ("input_keyboard" | "input-keyboard", _) => Some("browser.input_keyboard".to_string()),
        ("input_touch" | "input-touch", _) => Some("browser.input_touch".to_string()),
        _ => None,
    }
}

fn is_unsupported_tmux_cmd(cmd: &str) -> bool {
    matches!(cmd, "popup" | "bind-key" | "unbind-key" | "copy-mode")
}

// purpose: Map CMUX/tmux short aliases onto the single Limux implementation path.
// inputs: Raw tmux compatibility command name.
// returns/effects: Returns the canonical command used by run_tmux_compat.
fn canonical_tmux_command(command: &str) -> &str {
    match command {
        "capturep" => "capture-pane",
        "display" | "displayp" => "display-message",
        "resizep" => "resize-pane",
        "respawnp" => "respawn-pane",
        "setb" => "set-buffer",
        "pasteb" => "paste-buffer",
        "showb" => "show-buffer",
        "lsw" => "list-windows",
        "lsp" => "list-panes",
        _ => command,
    }
}

// purpose: Resolve a tmux buffer name from CMUX and tmux spellings.
// inputs: Raw args with optional --name or -b value.
// returns/effects: Returns the requested name or the CMUX default buffer name.
fn tmux_buffer_name_arg(args: &[String]) -> String {
    parse_opt(args, "--name")
        .or_else(|| parse_opt(args, "-b"))
        .unwrap_or_else(|| "default".to_string())
}

// purpose: Resolve tmux list command format flags.
// inputs: Raw args with optional -F or --format value.
// returns/effects: Returns the requested format string.
fn tmux_list_format_arg(args: &[String]) -> Option<String> {
    parse_opt(args, "-F").or_else(|| parse_opt(args, "--format"))
}

// purpose: Add workspace fields to a tmux render context from a list row.
// inputs: Mutable render context and one workspace.list row.
// returns/effects: Updates window/session keys and returns fallback text.
fn add_tmux_workspace_row_context(context: &mut BTreeMap<String, String>, row: &Value) -> String {
    let id = get_string(row, &["workspace_id", "id"]).unwrap_or_default();
    if !id.is_empty() {
        context.insert(
            "session_id".to_string(),
            format!("${}", tmux_stable_numeric_id(&id)),
        );
        context.insert(
            "window_id".to_string(),
            format!("@{}", tmux_stable_numeric_id(&id)),
        );
        context.insert("window_uuid".to_string(), id.clone());
    }
    if let Some(index) = row.get("index").and_then(Value::as_u64) {
        context.insert("window_index".to_string(), index.to_string());
    }
    if let Some(title) = nonempty_row_title(row) {
        context.insert("window_name".to_string(), title);
    }
    let index = context
        .get("window_index")
        .map(String::as_str)
        .unwrap_or("?");
    let name = context
        .get("window_name")
        .map(String::as_str)
        .unwrap_or(id.as_str());
    format!("{index} {name}")
}

// purpose: Add pane fields to a tmux render context from a pane.list row.
// inputs: Mutable render context and one pane.list row.
// returns/effects: Updates pane id/index/active keys and returns fallback text.
fn add_tmux_pane_row_context(context: &mut BTreeMap<String, String>, row: &Value) -> String {
    insert_tmux_pane_row(context, row);
    let raw_id = get_string(row, &["pane_id", "id"]).unwrap_or_default();
    let fallback = context.get("pane_id").cloned().unwrap_or(raw_id);
    if let Some(count) = row.get("surface_count").and_then(Value::as_u64) {
        context.insert("pane_tabs".to_string(), count.to_string());
    }
    fallback
}

// purpose: Render rows with CMUX/tmux format semantics.
// inputs: Rows, optional -F format, and a row-specific context filler.
// returns/effects: Returns newline-delimited tmux-compatible list output.
fn render_tmux_rows(
    rows: &[Value],
    format: Option<&str>,
    add_context: fn(&mut BTreeMap<String, String>, &Value) -> String,
) -> String {
    rows.iter()
        .map(|row| {
            let mut context = base_tmux_format_context();
            let fallback = add_context(&mut context, row);
            tmux_render_format(format, &context, &fallback)
        })
        .collect::<Vec<_>>()
        .join("\n")
}

async fn run_tmux_compat(client: &mut Client, command: &str, args: &[String]) -> Result<Value> {
    let command = canonical_tmux_command(command);
    if is_unsupported_tmux_cmd(command) {
        bail!("not supported");
    }

    match command {
        "capture-pane" => run_read_screen(client, args).await,
        "list-windows" => {
            let payload = client.call("workspace.list", json!({})).await?;
            let rows = payload_array(&payload, "workspaces");
            let text = render_tmux_rows(
                &rows,
                tmux_list_format_arg(args).as_deref(),
                add_tmux_workspace_row_context,
            );
            Ok(json!({"text": text}))
        }
        "list-panes" => {
            let workspace = parse_opt(args, "-t")
                .or_else(|| parse_opt(args, "--workspace"))
                .or_else(|| context_env_value("LIMUX_WORKSPACE_ID"));
            let params = workspace
                .as_ref()
                .map(|id| json!({"workspace_id": id}))
                .unwrap_or_else(|| json!({}));
            let payload = client.call("pane.list", params).await?;
            let rows = payload_array(&payload, "panes");
            let text = render_tmux_rows(
                &rows,
                tmux_list_format_arg(args).as_deref(),
                add_tmux_pane_row_context,
            );
            Ok(json!({"text": text}))
        }
        "pipe-pane" => {
            let capture = run_read_screen(client, args).await?;
            let text = get_string(&capture, &["text"]).unwrap_or_default();
            let shell_cmd = parse_opt(args, "--command")
                .ok_or_else(|| anyhow!("pipe-pane requires --command"))?;
            let mut child = Command::new("bash")
                .arg("-lc")
                .arg(shell_cmd)
                .stdin(Stdio::piped())
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .spawn()
                .context("failed to spawn pipe-pane command")?;
            if let Some(stdin) = child.stdin.as_mut() {
                use std::io::Write;
                stdin
                    .write_all(text.as_bytes())
                    .context("failed to write pipe-pane stdin")?;
            }
            let status = child
                .wait()
                .context("failed waiting for pipe-pane command")?;
            if !status.success() {
                bail!("pipe-pane command failed");
            }
            Ok(json!({"ok": true}))
        }
        "wait-for" => {
            let signal = parse_flag(args, "-S") || parse_flag(args, "--signal");
            let name = trailing_title(args).ok_or_else(|| anyhow!("wait-for requires a name"))?;
            let timeout = parse_opt(args, "--timeout")
                .and_then(|v| v.parse::<u64>().ok())
                .unwrap_or(5);
            let path = wait_signal_path(&client.socket, &name);
            if signal {
                create_wait_signal(&path)?;
                Ok(json!({"ok": true, "name": name}))
            } else {
                let deadline = Instant::now() + Duration::from_secs(timeout);
                loop {
                    if path.exists() {
                        remove_wait_signal(&path)?;
                        return Ok(json!({"ok": true, "name": name}));
                    }
                    if Instant::now() >= deadline {
                        bail!("wait-for timed out waiting for '{}'", name);
                    }
                    std::thread::sleep(Duration::from_millis(50));
                }
            }
        }
        "find-window" => {
            let needle = trailing_title(args).unwrap_or_default();
            let listed = client.call("workspace.list", json!({})).await?;
            let rows = listed
                .get("workspaces")
                .and_then(Value::as_array)
                .cloned()
                .unwrap_or_default();
            let mut out = String::new();
            for row in rows {
                let title = get_string(&row, &["title", "name"]).unwrap_or_default();
                if title.contains(&needle) {
                    let handle = handle_from_payload(&row, "workspace_id", "workspace_ref");
                    out = format!("{} {}", handle, title);
                    break;
                }
            }
            Ok(json!({"text": out}))
        }
        "last-window" => client.call("workspace.last", json!({})).await,
        "next-window" => client.call("workspace.next", json!({})).await,
        "previous-window" => client.call("workspace.previous", json!({})).await,
        "swap-pane" => {
            let workspace = parse_opt(args, "--workspace");
            let pane =
                parse_opt(args, "--pane").ok_or_else(|| anyhow!("swap-pane requires --pane"))?;
            let target = parse_opt(args, "--target-pane")
                .ok_or_else(|| anyhow!("swap-pane requires --target-pane"))?;

            let source_surface =
                selected_surface_for_pane(client, workspace.clone(), &pane).await?;
            let target_surface =
                selected_surface_for_pane(client, workspace.clone(), &target).await?;

            let _ = call_in_workspace_scope(
                client,
                workspace.clone(),
                "surface.move",
                json!({"surface_id": source_surface, "target_pane_id": target, "index": 0}),
            )
            .await?;
            let _ = call_in_workspace_scope(
                client,
                workspace.clone(),
                "surface.move",
                json!({"surface_id": target_surface, "target_pane_id": pane, "index": 0}),
            )
            .await?;

            Ok(json!({"ok": true}))
        }
        "break-pane" => {
            let workspace = parse_opt(args, "--workspace");
            let pane = parse_opt(args, "--pane");
            let surface = parse_opt(args, "--surface");
            let mut p = Map::new();
            if let Some(pane) = pane {
                p.insert("pane_id".to_string(), Value::String(pane));
            }
            if let Some(surface) = surface {
                p.insert("surface_id".to_string(), Value::String(surface));
            }
            call_in_workspace_scope(client, workspace, "pane.break", Value::Object(p)).await
        }
        "join-pane" => {
            let workspace = parse_opt(args, "--workspace");
            let pane = parse_opt(args, "--pane");
            let surface = parse_opt(args, "--surface");
            let target = parse_opt(args, "--target-pane")
                .ok_or_else(|| anyhow!("join-pane requires --target-pane"))?;
            let mut p = Map::new();
            p.insert("target_pane_id".to_string(), Value::String(target));
            if let Some(pane) = pane {
                p.insert("pane_id".to_string(), Value::String(pane));
            }
            if let Some(surface) = surface {
                p.insert("surface_id".to_string(), Value::String(surface));
            }
            call_in_workspace_scope(client, workspace, "pane.join", Value::Object(p)).await
        }
        "last-pane" => {
            let workspace = parse_opt(args, "--workspace");
            call_in_workspace_scope(client, workspace, "pane.last", json!({})).await
        }
        "clear-history" => {
            let workspace = parse_opt(args, "--workspace");
            let surface = parse_opt(args, "--surface");
            let mut p = Map::new();
            if let Some(surface) = surface {
                p.insert("surface_id".to_string(), Value::String(surface));
            }
            call_in_workspace_scope(client, workspace, "surface.clear_history", Value::Object(p))
                .await
        }
        "set-hook" => {
            let list_mode = parse_flag(args, "--list");
            let unset_flag = parse_flag(args, "--unset");
            let unset = parse_opt(args, "--unset");
            with_locked_json_map(&client.socket, "hooks", |hooks, path| {
                if list_mode {
                    let text = hooks
                        .iter()
                        .map(|(k, v)| format!("{} -> {}", k, v))
                        .collect::<Vec<_>>()
                        .join("\n");
                    return Ok(json!({
                        "text": text,
                        "path": path.display().to_string(),
                    }));
                }
                if unset_flag && unset.is_none() {
                    bail!("set-hook --unset requires an event name");
                }
                if let Some(name) = unset {
                    hooks.remove(&name);
                    write_json_map(path, hooks)?;
                    return Ok(json!({"ok": true}));
                }
                let name = args
                    .iter()
                    .find(|a| !a.starts_with('-'))
                    .cloned()
                    .unwrap_or_default();
                let body = trailing_title(args).unwrap_or_default();
                if name.is_empty() || body.is_empty() {
                    bail!("set-hook requires <name> <command>");
                }
                hooks.insert(name, body);
                write_json_map(path, hooks)?;
                Ok(json!({"ok": true}))
            })
        }
        "resize-pane" => {
            let workspace = parse_opt(args, "--workspace");
            let pane =
                parse_opt(args, "--pane").ok_or_else(|| anyhow!("resize-pane requires --pane"))?;
            let direction = if parse_flag(args, "-R") {
                "right"
            } else if parse_flag(args, "-L") {
                "left"
            } else if parse_flag(args, "-D") {
                "down"
            } else if parse_flag(args, "-U") {
                "up"
            } else {
                "right"
            };
            let amount = parse_opt(args, "--amount")
                .and_then(|v| v.parse::<u64>().ok())
                .unwrap_or(1);
            call_in_workspace_scope(
                client,
                workspace,
                "pane.resize",
                json!({"pane_id": pane, "direction": direction, "amount": amount}),
            )
            .await
        }
        "set-buffer" => {
            let name = tmux_buffer_name_arg(args);
            let body = trailing_title(args).ok_or_else(|| anyhow!("set-buffer requires text"))?;
            with_locked_json_map(&client.socket, "buffers", |buffers, path| {
                buffers.insert(name, body);
                write_json_map(path, buffers)?;
                Ok(json!({"ok": true}))
            })
        }
        "list-buffers" => with_locked_json_map(&client.socket, "buffers", |buffers, _path| {
            let text = buffers
                .iter()
                .map(|(name, value)| format!("{}\t{}", name, value.len()))
                .collect::<Vec<_>>()
                .join("\n");
            Ok(json!({"text": text}))
        }),
        "show-buffer" => {
            let name = tmux_buffer_name_arg(args);
            with_locked_json_map(&client.socket, "buffers", |buffers, _path| {
                let text = buffers.get(&name).cloned().unwrap_or_default();
                Ok(json!({"text": text, "name": name}))
            })
        }
        "paste-buffer" => {
            let name = tmux_buffer_name_arg(args);
            let workspace = parse_opt(args, "--workspace");
            let surface = parse_opt(args, "--surface");
            let text = with_locked_json_map(&client.socket, "buffers", |buffers, _path| {
                tmux_buffer_text(buffers, &name)
            })?;
            let mut p = Map::new();
            if let Some(surface) = surface {
                p.insert("surface_id".to_string(), Value::String(surface));
            }
            p.insert("text".to_string(), Value::String(text));
            call_in_workspace_scope(client, workspace, "surface.send_text", Value::Object(p)).await
        }
        "respawn-pane" => {
            let (workspace, params) = build_respawn_pane_request(args)?;
            call_in_workspace_scope(client, workspace, "surface.respawn", params).await
        }
        "display-message" => {
            let (print, target, format) = parse_tmux_display_message_args(args);
            let context = tmux_format_context(client, target.as_deref()).await?;
            let text = tmux_render_format(format.as_deref(), &context, "");
            Ok(json!({"text": text, "printed": print}))
        }
        _ => bail!("unknown tmux command"),
    }
}

struct CodexTeamsWatcherState {
    workspace_id: String,
    root_surface_id: String,
    codex_path: String,
    launch_path: Option<String>,
    max_auto_depth: usize,
    known_thread_ids: BTreeSet<String>,
    parent_by_thread_id: BTreeMap<String, String>,
    depth_by_thread_id: BTreeMap<String, usize>,
    pending_by_parent_thread_id: BTreeMap<String, Vec<codex_teams::Thread>>,
    opened_thread_ids: BTreeSet<String>,
    subscribed_thread_ids: BTreeSet<String>,
    approval_item_by_id: BTreeMap<String, Value>,
    approval_item_order: Vec<String>,
    last_agent_surface_id: Option<String>,
}

impl CodexTeamsWatcherState {
    /// purpose: Create state for a single hidden Codex Teams watcher process.
    /// inputs: Required workspace/surface ids, Codex executable metadata, and max auto depth.
    /// returns/effects: Returns empty watcher state ready for app-server notifications.
    fn new(
        workspace_id: String,
        root_surface_id: String,
        codex_path: String,
        launch_path: Option<String>,
        max_auto_depth: usize,
    ) -> Self {
        Self {
            workspace_id,
            root_surface_id,
            codex_path,
            launch_path,
            max_auto_depth,
            known_thread_ids: BTreeSet::new(),
            parent_by_thread_id: BTreeMap::new(),
            depth_by_thread_id: BTreeMap::new(),
            pending_by_parent_thread_id: BTreeMap::new(),
            opened_thread_ids: BTreeSet::new(),
            subscribed_thread_ids: BTreeSet::new(),
            approval_item_by_id: BTreeMap::new(),
            approval_item_order: Vec::new(),
            last_agent_surface_id: None,
        }
    }

    /// purpose: Cache bounded Codex item metadata for later approval Feed cards.
    /// inputs: App-server notification method and params object.
    /// returns/effects: Updates the bounded approval item cache.
    fn cache_approval_item_if_present(&mut self, method: &str, params: &Map<String, Value>) {
        if matches!(method, "item/started" | "item/completed") {
            if let Some(item) = params.get("item").and_then(Value::as_object) {
                if let Some(item_id) = codex_teams::string_value(item, &["id"]) {
                    self.cache_approval_item(item_id, codex_teams::approval_item_snapshot(item));
                }
            }
            return;
        }
        if method != "item/fileChange/patchUpdated" {
            return;
        }
        let Some(item_id) = codex_teams::string_value(params, &["itemId", "item_id"]) else {
            return;
        };
        let mut item = self
            .approval_item_by_id
            .get(&item_id)
            .and_then(Value::as_object)
            .cloned()
            .unwrap_or_else(|| {
                let mut item = Map::new();
                item.insert("type".to_string(), json!("fileChange"));
                item.insert("id".to_string(), json!(item_id));
                item
            });
        if let Some(changes) = params.get("changes") {
            item.insert("changes".to_string(), changes.clone());
        }
        self.cache_approval_item(item_id, codex_teams::approval_item_snapshot(&item));
    }

    /// purpose: Observe a Codex app-server thread and create eligible subagent panes.
    /// inputs: Limux socket client, app-server URL, and parsed Codex thread.
    /// returns/effects: May call `surface.split`, `tab.action`, and layout equalization.
    async fn observe_thread(
        &mut self,
        client: &mut Client,
        app_server_url: &str,
        thread: codex_teams::Thread,
    ) -> Result<()> {
        if self.known_thread_ids.contains(&thread.id) {
            if let Some(spawn) = thread.spawn.clone() {
                if !self.parent_by_thread_id.contains_key(&thread.id) {
                    self.observe_spawn(client, app_server_url, thread, spawn)
                        .await?;
                } else if !self.opened_thread_ids.contains(&thread.id) {
                    self.open_observed_subagent(client, app_server_url, &thread, &spawn)
                        .await?;
                }
            }
            return Ok(());
        }
        self.known_thread_ids.insert(thread.id.clone());
        if let Some(spawn) = thread.spawn.clone() {
            self.observe_spawn(client, app_server_url, thread.clone(), spawn)
                .await?;
        } else {
            self.depth_by_thread_id.insert(thread.id.clone(), 0);
        }
        self.drain_pending_children(client, app_server_url, &thread.id)
            .await
    }

    async fn observe_spawn(
        &mut self,
        client: &mut Client,
        app_server_url: &str,
        thread: codex_teams::Thread,
        spawn: codex_teams::Spawn,
    ) -> Result<()> {
        self.parent_by_thread_id
            .insert(thread.id.clone(), spawn.parent_thread_id.clone());
        if !self.known_thread_ids.contains(&spawn.parent_thread_id)
            || !self
                .depth_by_thread_id
                .contains_key(&spawn.parent_thread_id)
        {
            self.pending_by_parent_thread_id
                .entry(spawn.parent_thread_id)
                .or_default()
                .push(thread);
            return Ok(());
        }
        self.open_observed_subagent(client, app_server_url, &thread, &spawn)
            .await
    }

    async fn drain_pending_children(
        &mut self,
        client: &mut Client,
        app_server_url: &str,
        parent_thread_id: &str,
    ) -> Result<()> {
        let mut stack = vec![parent_thread_id.to_string()];
        while let Some(parent_thread_id) = stack.pop() {
            let pending = self
                .pending_by_parent_thread_id
                .remove(&parent_thread_id)
                .unwrap_or_default();
            for child in pending {
                let Some(spawn) = child.spawn.clone() else {
                    continue;
                };
                self.open_observed_subagent(client, app_server_url, &child, &spawn)
                    .await?;
                stack.push(child.id);
            }
        }
        Ok(())
    }

    async fn open_observed_subagent(
        &mut self,
        client: &mut Client,
        app_server_url: &str,
        thread: &codex_teams::Thread,
        spawn: &codex_teams::Spawn,
    ) -> Result<()> {
        let depth = self
            .depth_by_thread_id
            .get(&spawn.parent_thread_id)
            .map(|depth| depth.saturating_add(1))
            .unwrap_or_else(|| spawn.source_depth.unwrap_or(1).max(1));
        self.depth_by_thread_id.insert(thread.id.clone(), depth);
        if depth > self.max_auto_depth
            || self.opened_thread_ids.contains(&thread.id)
            || !codex_teams::thread_may_be_attachable(thread)
        {
            return Ok(());
        }
        let plan = codex_teams::subagent_split_plan(
            &self.workspace_id,
            &self.root_surface_id,
            self.last_agent_surface_id.as_deref(),
            thread,
            spawn,
            &codex_teams::SubagentLaunch {
                codex_executable: &self.codex_path,
                app_server_url,
                launch_path: self.launch_path.as_deref(),
                depth,
            },
        )?;
        let created = client.call("surface.split", plan.params).await?;
        if created.get("accepted").and_then(Value::as_bool) == Some(true) {
            bail!(
                "Codex subagent panes are not supported in remote tmux mirror workspaces (surface.split was routed to the remote tmux session)"
            );
        }
        let surface_id = get_string(&created, &["surface_id"])
            .ok_or_else(|| anyhow!("surface.split did not return surface_id"))?;
        self.last_agent_surface_id = Some(surface_id.clone());
        self.opened_thread_ids.insert(thread.id.clone());
        let _ = client
            .call(
                "tab.action",
                json!({
                    "workspace_id": self.workspace_id,
                    "surface_id": surface_id,
                    "action": "rename",
                    "title": plan.title,
                }),
            )
            .await;
        let _ = client
            .call(
                "workspace.equalize_splits",
                json!({
                    "workspace_id": self.workspace_id,
                    "orientation": "vertical",
                }),
            )
            .await;
        Ok(())
    }

    fn related_item_for_params(&self, params: &Map<String, Value>) -> Option<&Map<String, Value>> {
        let item_id = codex_teams::string_value(params, &["itemId", "item_id"])?;
        self.approval_item_by_id.get(&item_id)?.as_object()
    }

    fn cache_approval_item(&mut self, item_id: String, item: Value) {
        if !self.approval_item_by_id.contains_key(&item_id) {
            self.approval_item_order.push(item_id.clone());
        }
        self.approval_item_by_id.insert(item_id, item);
        while self.approval_item_order.len() > 500 {
            if let Some(evicted) = self.approval_item_order.first().cloned() {
                self.approval_item_order.remove(0);
                self.approval_item_by_id.remove(&evicted);
            }
        }
    }
}

/// purpose: Run hidden CMUX-compatible Codex Teams app-server watcher.
/// inputs: Parsed Limux client and hidden watcher args from the root launcher.
/// returns/effects: Streams Codex app-server messages, bridges approvals, and opens subagent splits.
async fn run_codex_teams_watcher(client: &mut Client, args: &[String]) -> Result<CommandOutput> {
    let workspace_id = parse_opt(args, "--workspace-id")
        .ok_or_else(|| anyhow!("__codex-teams-watch requires --workspace-id"))?;
    let surface_id = parse_opt(args, "--surface-id")
        .ok_or_else(|| anyhow!("__codex-teams-watch requires --surface-id"))?;
    let app_server_url = parse_opt(args, "--app-server-url")
        .ok_or_else(|| anyhow!("__codex-teams-watch requires --app-server-url"))?;
    let codex_path = parse_opt(args, "--codex-path").unwrap_or_else(|| "codex".to_string());
    let launch_path = parse_opt(args, "--launch-path");
    let max_auto_depth = parse_opt(args, "--max-auto-depth")
        .map(|value| {
            value
                .parse::<usize>()
                .with_context(|| format!("invalid --max-auto-depth value `{value}`"))
        })
        .transpose()?
        .unwrap_or(codex_teams::MAX_AUTO_DEPTH);
    let mut state = CodexTeamsWatcherState::new(
        workspace_id,
        surface_id,
        codex_path,
        launch_path,
        max_auto_depth,
    );
    let mut connection = codex_teams::AppServerConnection::connect(&app_server_url).await?;
    connection
        .initialize(
            "limux-codex-teams",
            env!("CARGO_PKG_VERSION"),
            &[
                "thread/tokenUsage/updated",
                "turn/diff/updated",
                "turn/plan/updated",
                "item/agentMessage/delta",
                "item/plan/delta",
                "item/reasoning/summaryTextDelta",
                "item/reasoning/textDelta",
                "command/exec/outputDelta",
                "process/outputDelta",
                "item/fileChange/outputDelta",
                "item/mcpToolCall/progress",
                "thread/turn/delta",
                "turn/delta",
                "item/textDelta",
                "item/thinkingDelta",
                "item/reasoningDelta",
                "item/commandExecution/outputDelta",
                "item/commandExecution/stdoutDelta",
                "item/commandExecution/stderrDelta",
                "item/outputDelta",
            ],
            Duration::from_secs(10),
        )
        .await?;
    backfill_codex_teams_threads(client, &mut connection, &mut state, &app_server_url).await?;
    loop {
        let message = connection.receive_object().await?;
        handle_codex_teams_message(
            client,
            &mut connection,
            &mut state,
            &app_server_url,
            message,
        )
        .await?;
    }
}

async fn backfill_codex_teams_threads(
    client: &mut Client,
    connection: &mut codex_teams::AppServerConnection,
    state: &mut CodexTeamsWatcherState,
    app_server_url: &str,
) -> Result<()> {
    let mut notifications = Vec::new();
    let loaded = connection
        .request(
            "thread/loaded/list",
            Some(json!({"limit": 200})),
            |message| {
                notifications.push(message);
                Ok(())
            },
            Duration::from_secs(10),
        )
        .await?;
    for message in notifications {
        handle_codex_teams_message_passive(client, connection, state, app_server_url, message)
            .await?;
    }
    let thread_ids = loaded
        .get("data")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    for thread_id in thread_ids.iter().filter_map(Value::as_str) {
        subscribe_codex_teams_thread(client, connection, state, app_server_url, thread_id).await?;
    }
    Ok(())
}

async fn subscribe_codex_teams_thread(
    client: &mut Client,
    connection: &mut codex_teams::AppServerConnection,
    state: &mut CodexTeamsWatcherState,
    app_server_url: &str,
    thread_id: &str,
) -> Result<()> {
    if !state.subscribed_thread_ids.insert(thread_id.to_string()) {
        return Ok(());
    }
    let mut notifications = Vec::new();
    let response = match connection
        .request(
            "thread/resume",
            Some(json!({"threadId": thread_id, "excludeTurns": true})),
            |message| {
                notifications.push(message);
                Ok(())
            },
            Duration::from_secs(10),
        )
        .await
    {
        Ok(response) => response,
        Err(error) => {
            state.subscribed_thread_ids.remove(thread_id);
            return Err(error);
        }
    };
    for message in notifications {
        handle_codex_teams_message_passive(client, connection, state, app_server_url, message)
            .await?;
    }
    if let Some(thread) = response
        .get("thread")
        .and_then(Value::as_object)
        .and_then(codex_teams::thread_from_object)
    {
        state.observe_thread(client, app_server_url, thread).await?;
    }
    Ok(())
}

async fn handle_codex_teams_message(
    client: &mut Client,
    connection: &mut codex_teams::AppServerConnection,
    state: &mut CodexTeamsWatcherState,
    app_server_url: &str,
    message: Value,
) -> Result<()> {
    let thread_id = message
        .get("params")
        .and_then(Value::as_object)
        .and_then(|params| params.get("thread"))
        .and_then(Value::as_object)
        .and_then(|thread| thread.get("id"))
        .and_then(Value::as_str)
        .map(str::to_string);
    handle_codex_teams_message_passive(client, connection, state, app_server_url, message).await?;
    if let Some(thread_id) = thread_id {
        subscribe_codex_teams_thread(client, connection, state, app_server_url, &thread_id).await?;
    }
    Ok(())
}

async fn handle_codex_teams_message_passive(
    client: &mut Client,
    connection: &mut codex_teams::AppServerConnection,
    state: &mut CodexTeamsWatcherState,
    app_server_url: &str,
    message: Value,
) -> Result<()> {
    let Some(method) = message.get("method").and_then(Value::as_str) else {
        return Ok(());
    };
    if let Some(params) = message.get("params").and_then(Value::as_object) {
        state.cache_approval_item_if_present(method, params);
    }
    if let Some(request_id) = message.get("id").cloned() {
        if handle_codex_teams_approval_request(
            client, connection, state, method, request_id, &message,
        )
        .await?
        {
            return Ok(());
        }
    }
    let Some(params) = message.get("params").and_then(Value::as_object) else {
        return Ok(());
    };
    let Some(thread) = params
        .get("thread")
        .and_then(Value::as_object)
        .and_then(codex_teams::thread_from_object)
    else {
        return Ok(());
    };
    state
        .observe_thread(client, app_server_url, thread.clone())
        .await?;
    Ok(())
}

async fn handle_codex_teams_approval_request(
    client: &mut Client,
    connection: &mut codex_teams::AppServerConnection,
    state: &CodexTeamsWatcherState,
    method: &str,
    request_id: Value,
    message: &Value,
) -> Result<bool> {
    if !matches!(
        method,
        "item/commandExecution/requestApproval"
            | "item/fileChange/requestApproval"
            | "item/permissions/requestApproval"
    ) {
        return Ok(false);
    }
    let Some(params) = message.get("params").and_then(Value::as_object) else {
        connection
            .respond_error(request_id, -32602, "Codex approval request missing params")
            .await?;
        return Ok(true);
    };
    let related_item = state.related_item_for_params(params);
    let event = codex_teams::feed_event(
        method,
        &request_id,
        params,
        &state.workspace_id,
        related_item,
    );
    let feed_response = client
        .call(
            "feed.push",
            json!({
                "event": event,
                "wait_timeout_seconds": 120.0,
            }),
        )
        .await?;
    let Some(mode) = codex_teams::permission_mode_from_feed_push_response(&feed_response) else {
        return Ok(true);
    };
    if let Some(response) = codex_teams::app_server_approval_response(method, params, &mode) {
        connection.respond(request_id, response).await?;
    }
    Ok(true)
}

/// purpose: Run public CMUX-compatible `limux codex-teams`.
/// inputs: Socket client, forwarded Codex args, and resolved socket password.
/// returns/effects: Starts Codex app-server, root Codex TUI, hidden watcher, then exits with root status.
async fn run_codex_teams(
    client: &Client,
    command_args: &[String],
    socket_password: Option<String>,
) -> Result<CommandOutput> {
    let workspace_id = context_env_value("LIMUX_WORKSPACE_ID").ok_or_else(|| {
        anyhow!("limux codex-teams must be started from a Limux terminal surface")
    })?;
    let surface_id = context_env_value("LIMUX_SURFACE_ID").ok_or_else(|| {
        anyhow!("limux codex-teams must be started from a Limux terminal surface")
    })?;
    let cwd = env::current_dir().context("failed to resolve current directory")?;
    codex_teams::validate_working_directory(command_args, &cwd)?;
    let codex_path = resolve_executable_in_path("codex")
        .ok_or_else(|| anyhow!("Codex executable `codex` was not found in PATH"))?;
    let app_server_url = format!("ws://127.0.0.1:{}", bindable_loopback_port()?);
    let app_server_log = codex_teams_log_path(&app_server_url, "app-server");
    let watcher_log = codex_teams_log_path(&app_server_url, "watcher");
    let mut environment = env::vars().collect::<BTreeMap<_, _>>();
    environment.insert(
        "CMUX_SOCKET_PATH".to_string(),
        client.socket.display().to_string(),
    );
    environment.remove("CMUX_SOCKET");
    if let Some(password) = socket_password.as_deref().filter(|value| !value.is_empty()) {
        environment.insert("CMUX_SOCKET_PASSWORD".to_string(), password.to_string());
    }

    let mut app_server = spawn_logged_process(
        &codex_path,
        &[
            "app-server".to_string(),
            "--listen".to_string(),
            app_server_url.clone(),
        ],
        &environment,
        &app_server_log,
    )?;
    if let Err(error) = wait_for_codex_teams_app_server(&app_server_url)
        .await
        .with_context(|| {
            format!(
                "Codex app-server did not become ready; log: {}",
                app_server_log.display()
            )
        })
    {
        terminate_child(&mut app_server)
            .context("failed to stop Codex app-server after startup failure")?;
        return Err(error);
    }

    let launch_exe = env::current_exe().context("failed to resolve current executable")?;
    let launch_exe_text = launch_exe.display().to_string();
    let launch_path = environment
        .get("PATH")
        .cloned()
        .ok_or_else(|| anyhow!("PATH is required to launch Codex Teams helpers"))?;
    let mut root_environment = environment.clone();
    root_environment.insert(
        "CMUX_CODEX_TEAMS_APP_SERVER_URL".to_string(),
        app_server_url.clone(),
    );
    root_environment.insert(
        "CMUX_CODEX_TEAMS_MAX_AUTO_DEPTH".to_string(),
        codex_teams::MAX_AUTO_DEPTH.to_string(),
    );
    root_environment.insert(
        "CMUX_AGENT_LAUNCH_KIND".to_string(),
        "codexTeams".to_string(),
    );
    root_environment.insert(
        "CMUX_AGENT_LAUNCH_EXECUTABLE".to_string(),
        launch_exe_text.clone(),
    );
    root_environment.insert(
        "CMUX_AGENT_LAUNCH_ARGV".to_string(),
        format!("{} codex-teams {}", launch_exe_text, command_args.join(" ")),
    );
    root_environment.insert(
        "CMUX_AGENT_LAUNCH_CWD".to_string(),
        cwd.display().to_string(),
    );

    let root_args = codex_teams::root_codex_arguments(&app_server_url, command_args);
    let mut root_codex = Command::new(&codex_path)
        .args(root_args)
        .envs(&root_environment)
        .stdin(Stdio::inherit())
        .stdout(Stdio::inherit())
        .stderr(Stdio::inherit())
        .spawn()
        .context("failed to start root Codex process")?;
    let mut watcher_args = vec!["--socket".to_string(), client.socket.display().to_string()];
    if let Some(password) = socket_password.as_deref().filter(|value| !value.is_empty()) {
        watcher_args.extend(["--password".to_string(), password.to_string()]);
    }
    watcher_args.extend([
        "__codex-teams-watch".to_string(),
        "--workspace-id".to_string(),
        workspace_id,
        "--surface-id".to_string(),
        surface_id,
        "--app-server-url".to_string(),
        app_server_url.clone(),
        "--codex-path".to_string(),
        codex_path.clone(),
        "--launch-path".to_string(),
        launch_path,
        "--max-auto-depth".to_string(),
        codex_teams::MAX_AUTO_DEPTH.to_string(),
    ]);
    let mut watcher =
        spawn_logged_process(&launch_exe_text, &watcher_args, &environment, &watcher_log)?;
    let status = root_codex
        .wait()
        .context("failed waiting for root Codex process")?;
    terminate_child(&mut watcher).context("failed to stop Codex Teams watcher")?;
    terminate_child(&mut app_server).context("failed to stop Codex app-server")?;
    let code = status.code().unwrap_or(1);
    Ok(CommandOutput::Exit {
        stdout: String::new(),
        stderr: String::new(),
        status: code,
    })
}

/// purpose: Probe the Codex app-server websocket until it accepts initialization.
/// inputs: App-server websocket URL chosen by the public launcher.
/// returns/effects: Returns success when initialized or the last readiness error after timeout.
async fn wait_for_codex_teams_app_server(app_server_url: &str) -> Result<()> {
    let deadline = Instant::now() + Duration::from_secs(8);
    let mut last_error: Option<anyhow::Error> = None;
    while Instant::now() < deadline {
        match codex_teams::AppServerConnection::connect(app_server_url).await {
            Ok(mut connection) => {
                match connection
                    .initialize(
                        "limux-codex-teams-probe",
                        env!("CARGO_PKG_VERSION"),
                        &[],
                        Duration::from_secs(1),
                    )
                    .await
                {
                    Ok(_) => return Ok(()),
                    Err(error) => last_error = Some(error),
                }
            }
            Err(error) => last_error = Some(error),
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    if let Some(error) = last_error {
        Err(error)
    } else {
        bail!("Codex app-server did not become ready")
    }
}

/// purpose: Spawn a background helper with private stdout/stderr logs.
/// inputs: Executable path, argument vector, environment map, and target log path.
/// returns/effects: Creates or truncates the log at 0600 and returns the child process handle.
fn spawn_logged_process(
    executable: &str,
    args: &[String],
    environment: &BTreeMap<String, String>,
    log_path: &Path,
) -> Result<std::process::Child> {
    let log = OpenOptions::new()
        .create(true)
        .truncate(true)
        .write(true)
        .mode(0o600)
        .open(log_path)
        .with_context(|| format!("failed to open log {}", log_path.display()))?;
    let stderr = log.try_clone().context("failed to clone process log")?;
    Command::new(executable)
        .args(args)
        .envs(environment)
        .process_group(0)
        .stdin(Stdio::null())
        .stdout(Stdio::from(log))
        .stderr(Stdio::from(stderr))
        .spawn()
        .with_context(|| format!("failed to start {}", executable))
}

/// purpose: Stop a background helper process after the root Codex process exits.
/// inputs: Mutable child process handle.
/// returns/effects: Kills and waits for the child when it is still running.
fn terminate_child(child: &mut std::process::Child) -> Result<()> {
    let pgid = child.id() as i32;
    signal_process_group(pgid, libc::SIGTERM)
        .context("failed to terminate helper process group")?;
    let deadline = Instant::now() + Duration::from_secs(2);
    while process_group_exists(pgid)? && Instant::now() < deadline {
        let _ = child
            .try_wait()
            .context("failed to inspect helper process")?;
        std::thread::sleep(Duration::from_millis(50));
    }
    if process_group_exists(pgid)? {
        signal_process_group(pgid, libc::SIGKILL).context("failed to kill helper process group")?;
        let kill_deadline = Instant::now() + Duration::from_secs(2);
        while process_group_exists(pgid)? && Instant::now() < kill_deadline {
            let _ = child
                .try_wait()
                .context("failed to inspect helper process")?;
            std::thread::sleep(Duration::from_millis(50));
        }
        if process_group_exists(pgid)? {
            bail!("helper process group {pgid} survived SIGKILL");
        }
    }
    if child
        .try_wait()
        .context("failed to inspect helper process")?
        .is_none()
    {
        child.wait().context("failed to wait for helper process")?;
    }
    Ok(())
}

/// purpose: Send a Unix signal to a helper process group.
/// inputs: Positive process-group id and signal number.
/// returns/effects: Signals the negative process-group id; missing groups are treated as stopped.
fn signal_process_group(pgid: i32, signal: i32) -> Result<()> {
    let result = unsafe { libc::kill(-pgid, signal) };
    if result == 0 {
        return Ok(());
    }
    let error = io::Error::last_os_error();
    if error.raw_os_error() == Some(libc::ESRCH) {
        return Ok(());
    }
    Err(error).with_context(|| format!("failed to send signal {signal} to process group {pgid}"))
}

/// purpose: Check whether a helper process group still has live members.
/// inputs: Positive process-group id.
/// returns/effects: Uses signal 0; returns false when the group no longer exists.
fn process_group_exists(pgid: i32) -> Result<bool> {
    let result = unsafe { libc::kill(-pgid, 0) };
    if result == 0 {
        return Ok(true);
    }
    let error = io::Error::last_os_error();
    if error.raw_os_error() == Some(libc::ESRCH) {
        return Ok(false);
    }
    Err(error).with_context(|| format!("failed to inspect process group {pgid}"))
}

/// purpose: Reserve a currently bindable loopback port for Codex app-server startup.
/// inputs: None.
/// returns/effects: Returns an available localhost TCP port number or fails loudly.
fn bindable_loopback_port() -> Result<u16> {
    let listener = std::net::TcpListener::bind(("127.0.0.1", 0))
        .context("failed to allocate a localhost port for Codex app-server")?;
    Ok(listener.local_addr()?.port())
}

/// purpose: Build a predictable private temp log path for Codex Teams helpers.
/// inputs: App-server URL and helper log name.
/// returns/effects: Returns a temp-file path; no filesystem writes are performed here.
fn codex_teams_log_path(app_server_url: &str, name: &str) -> PathBuf {
    let port = app_server_url
        .rsplit(':')
        .next()
        .expect("rsplit always yields at least one segment")
        .replace('/', "_");
    env::temp_dir().join(format!("limux-codex-teams-{port}-{name}.log"))
}

/// purpose: Resolve an executable name against PATH without invoking a shell.
/// inputs: Program name to search for.
/// returns/effects: Returns the first matching executable-looking file path from PATH.
fn resolve_executable_in_path(name: &str) -> Option<String> {
    let path = env::var_os("PATH")?;
    env::split_paths(&path).find_map(|dir| {
        let candidate = dir.join(name);
        if candidate.is_file() {
            Some(candidate.display().to_string())
        } else {
            None
        }
    })
}

async fn execute_command(client: &mut Client, opts: &GlobalOptions) -> Result<CommandOutput> {
    if let Some(raw_request) = &opts.request {
        let request: V2Request =
            serde_json::from_str(raw_request).context("request must be a valid v2 JSON object")?;
        let mut payload = client.send_request(request).await?;
        apply_id_format(&mut payload, opts.id_format);
        return Ok(CommandOutput::Json(payload));
    }

    if opts.command_args.is_empty() {
        print_help();
        bail!("missing command");
    }

    let command = opts.command_args[0].as_str();
    let args = &opts.command_args[1..];
    if matches!(command, "help") {
        return Ok(CommandOutput::Text(help_text().to_string()));
    }
    if matches!(command, "version") {
        return Ok(CommandOutput::Text(version_text()));
    }
    let mut effective_id_format = opts.id_format;
    if command == "browser" {
        if let Some(raw) = parse_opt(args, "--id-format") {
            effective_id_format = IdFormat::parse(&raw)?;
        }
    }
    if matches!(command, "lsw" | "lsp")
        || (matches!(command, "list-windows" | "list-panes")
            && (parse_opt(args, "-F").is_some() || parse_opt(args, "--format").is_some()))
    {
        let payload = run_tmux_compat(client, command, args).await?;
        if opts.json_output {
            return Ok(CommandOutput::Json(payload));
        }
        let text = get_string(&payload, &["text"]).unwrap_or_default();
        return Ok(CommandOutput::Text(text));
    }

    let mut out = match command {
        "rpc" => {
            let payload = run_rpc_command(client, args).await?;
            if opts.json_output {
                CommandOutput::Json(payload)
            } else {
                CommandOutput::Text(default_text_output(&payload))
            }
        }
        "events" => return run_events(client, args).await,
        "codex-teams" => {
            return run_codex_teams(client, args, client.password.clone()).await;
        }
        "__codex-teams-watch" => return run_codex_teams_watcher(client, args).await,
        "capabilities" | "current-workspace" | "select-workspace" => {
            let Some((method, params)) = build_workspace_alias_request(command, args)? else {
                bail!("unsupported workspace alias: {}", command);
            };
            let payload = client.call(method, params).await?;
            if opts.json_output {
                CommandOutput::Json(payload)
            } else {
                CommandOutput::Text(default_text_output(&payload))
            }
        }
        "new-window" | "current-window" | "list-windows" | "focus-window" | "close-window" => {
            let merged_args = args_with_global_window(args, opts.window.as_deref());
            let Some((method, params)) = build_window_alias_request(command, &merged_args)? else {
                bail!("unsupported window alias: {}", command);
            };
            let payload = client.call(method, params).await?;
            if opts.json_output {
                CommandOutput::Json(payload)
            } else {
                CommandOutput::Text(default_text_output(&payload))
            }
        }
        "identify" => CommandOutput::Json(run_identify(client, args).await?),
        "list-panels"
        | "list-panes"
        | "list-workspaces"
        | "list-workspace-groups"
        | "surface-health" => {
            let payload = run_list(client, command, args).await?;
            if opts.json_output {
                CommandOutput::Json(payload)
            } else {
                CommandOutput::Text(render_list_text(command, &payload))
            }
        }
        "workspace-group" | "workspace-groups" => {
            let payload = run_workspace_group_command(client, args).await?;
            if opts.json_output {
                CommandOutput::Json(payload)
            } else {
                CommandOutput::Text(render_workspace_group_text(args, &payload))
            }
        }
        "workspace" if args.first().map(String::as_str) == Some("group") => {
            let group_args = &args[1..];
            let payload = run_workspace_group_command(client, group_args).await?;
            if opts.json_output {
                CommandOutput::Json(payload)
            } else {
                CommandOutput::Text(render_workspace_group_text(group_args, &payload))
            }
        }
        "workspace" => {
            if args.first().map(String::as_str) == Some("create") {
                let payload = run_new_workspace(client, &args[1..]).await?;
                if opts.json_output {
                    CommandOutput::Json(payload)
                } else {
                    let handle = handle_from_payload(&payload, "workspace_id", "workspace_ref");
                    CommandOutput::Text(format!("OK {}", handle))
                }
            } else if let Some((method, params)) = build_workspace_namespace_request(args)? {
                let payload = client.call(method, params).await?;
                if opts.json_output {
                    CommandOutput::Json(payload)
                } else if method == "workspace.env" {
                    CommandOutput::Text(render_workspace_env_text(&payload))
                } else {
                    CommandOutput::Text(default_text_output(&payload))
                }
            } else {
                bail!("unsupported workspace command");
            }
        }
        "memory" => {
            let payload = run_memory(client, args).await?;
            if opts.json_output {
                CommandOutput::Json(payload)
            } else {
                CommandOutput::Text(render_memory_text(&payload, opts.id_format))
            }
        }
        "settings" if args.first().map(String::as_str) == Some("open") => {
            if args.len() > 2 {
                bail!("Usage: limux settings open [target]");
            }
            let mut params = Map::new();
            params.insert("activate".to_string(), json!(true));
            if let Some(raw_target) = args.get(1) {
                let target = settings_target_raw_value(raw_target).ok_or_else(|| {
                    anyhow!("Unknown settings target `{raw_target}`. Run `limux settings --help`.")
                })?;
                params.insert("target".to_string(), json!(target));
            }
            let payload = client.call("settings.open", Value::Object(params)).await?;
            if opts.json_output {
                CommandOutput::Json(payload)
            } else {
                let target = payload
                    .get("target")
                    .and_then(Value::as_str)
                    .unwrap_or("general");
                CommandOutput::Text(format!("OK target={target}"))
            }
        }
        "config" if args.first().map(String::as_str) == Some("reload") => {
            if args.len() != 1 {
                bail!("Usage: limux config reload");
            }
            let payload = client
                .call("reload_config", Value::Object(Map::new()))
                .await?;
            if opts.json_output {
                CommandOutput::Json(payload)
            } else {
                CommandOutput::Text("OK config reloaded".to_string())
            }
        }
        "reload-config" => {
            if !args.is_empty() {
                bail!("Usage: limux reload-config");
            }
            let payload = client
                .call("reload_config", Value::Object(Map::new()))
                .await?;
            if opts.json_output {
                CommandOutput::Json(payload)
            } else {
                CommandOutput::Text("OK config reloaded".to_string())
            }
        }
        "list-pane-surfaces" => {
            let payload = client
                .call("pane.surfaces", build_list_pane_surfaces_request(args)?)
                .await?;
            if opts.json_output {
                CommandOutput::Json(payload)
            } else {
                CommandOutput::Text(render_list_text("list-panels", &payload))
            }
        }
        "new-split"
        | "focus-panel"
        | "close-surface"
        | "move-surface"
        | "split-off"
        | "drag-surface-to-split"
        | "reorder-surface"
        | "refresh-surfaces" => {
            let Some((method, params)) = build_surface_alias_request(command, args)? else {
                bail!("unsupported surface alias: {}", command);
            };
            let payload = client.call(method, params).await?;
            if opts.json_output {
                CommandOutput::Json(payload)
            } else {
                CommandOutput::Text(default_text_output(&payload))
            }
        }
        "list-notifications"
        | "dismiss-notification"
        | "mark-notification-read"
        | "open-notification"
        | "jump-to-unread"
        | "clear-notifications" => {
            let Some((method, params)) = build_notification_alias_request(command, args)? else {
                bail!("unsupported notification alias: {}", command);
            };
            let payload = client.call(method, params).await?;
            if opts.json_output {
                CommandOutput::Json(payload)
            } else {
                CommandOutput::Text(default_text_output(&payload))
            }
        }
        "send" => {
            let payload = run_send(client, args).await?;
            if opts.json_output {
                CommandOutput::Json(payload)
            } else {
                let handle = handle_from_payload(&payload, "surface_id", "surface_ref");
                CommandOutput::Text(format!("OK {}", handle.trim()))
            }
        }
        "send-key" => {
            let payload = run_send_key(client, args).await?;
            if opts.json_output {
                CommandOutput::Json(payload)
            } else {
                let handle = handle_from_payload(&payload, "surface_id", "surface_ref");
                CommandOutput::Text(format!("OK {}", handle.trim()))
            }
        }
        "notify" => {
            let payload = run_notify(client, args).await?;
            if opts.json_output {
                CommandOutput::Json(payload)
            } else {
                CommandOutput::Text("OK".to_string())
            }
        }
        "claude-hook" | "opencode-hook" | "gemini-hook" => {
            let agent = match command {
                "claude-hook" => agent_hooks::AgentKind::Claude,
                "opencode-hook" => agent_hooks::AgentKind::OpenCode,
                "gemini-hook" => agent_hooks::AgentKind::Gemini,
                _ => unreachable!(),
            };
            let payload = run_agent_hook(client, agent, args).await?;
            if opts.json_output {
                CommandOutput::Json(payload)
            } else {
                CommandOutput::Text("OK".to_string())
            }
        }
        "hooks" => return run_hooks_command(client, args, opts.json_output).await,
        "feed" => {
            let payload = run_feed_command(client, args, opts.window.as_deref()).await?;
            if opts.json_output {
                CommandOutput::Json(payload)
            } else {
                CommandOutput::Text(render_feed_command_text(&payload))
            }
        }
        "new-workspace" => {
            let payload = run_new_workspace(client, args).await?;
            if opts.json_output {
                CommandOutput::Json(payload)
            } else {
                let handle = handle_from_payload(&payload, "workspace_id", "workspace_ref");
                CommandOutput::Text(format!("OK {}", handle))
            }
        }
        "close-workspace" => {
            let payload = run_close_workspace(client, args).await?;
            if opts.json_output {
                CommandOutput::Json(payload)
            } else {
                CommandOutput::Text("OK".to_string())
            }
        }
        "agent-team" => {
            let payload = run_agent_team(client, args).await?;
            if opts.json_output {
                CommandOutput::Json(payload)
            } else {
                let agents_md = payload
                    .get("agents_md")
                    .and_then(|v| v.as_str())
                    .unwrap_or("AGENTS.md");
                let workspace = payload
                    .get("workspace_name")
                    .and_then(|v| v.as_str())
                    .unwrap_or("");
                let peers = payload
                    .get("peers")
                    .and_then(|v| v.as_array())
                    .map(|arr| {
                        arr.iter()
                            .filter_map(|p| p.get("agent").and_then(|v| v.as_str()))
                            .collect::<Vec<_>>()
                            .join(", ")
                    })
                    .unwrap_or_default();
                CommandOutput::Text(format!(
                    "OK agent-team workspace={workspace} peers=[{peers}] agents_md={agents_md}"
                ))
            }
        }
        "sidebar-state" => {
            let payload = run_sidebar_state(client, args).await?;
            if opts.json_output {
                CommandOutput::Json(payload)
            } else {
                CommandOutput::Text(render_sidebar_state_text(&payload))
            }
        }
        "set-status" | "clear-status" | "set-progress" | "clear-progress" | "log" | "clear-log" => {
            let payload =
                run_sidebar_command(client, command, args, opts.window.as_deref()).await?;
            if opts.json_output {
                CommandOutput::Json(payload)
            } else {
                CommandOutput::Text("OK".to_string())
            }
        }
        "list-status" | "list-log" => {
            let payload =
                run_sidebar_command(client, command, args, opts.window.as_deref()).await?;
            if opts.json_output {
                CommandOutput::Json(payload)
            } else {
                CommandOutput::Text(render_sidebar_list_text(command, &payload))
            }
        }
        "right-sidebar" => {
            let (payload, prints_state) =
                run_right_sidebar(client, args, opts.window.as_deref()).await?;
            if opts.json_output || prints_state {
                CommandOutput::Json(payload)
            } else {
                CommandOutput::Silent
            }
        }
        "new-surface" => {
            let payload = run_new_surface(client, args).await?;
            if opts.json_output {
                CommandOutput::Json(payload)
            } else {
                let handle = handle_from_payload(&payload, "surface_id", "surface_ref");
                CommandOutput::Text(format!("OK {}", handle))
            }
        }
        "new-pane" => {
            let payload = run_new_pane(client, args).await?;
            if opts.json_output {
                CommandOutput::Json(payload)
            } else {
                let handle = handle_from_payload(&payload, "surface_id", "surface_ref");
                CommandOutput::Text(format!("OK {}", handle))
            }
        }
        "tab-action" => {
            let payload = run_tab_action(client, args).await?;
            if opts.json_output {
                CommandOutput::Json(payload)
            } else if let Some(help) = get_string(&payload, &["help"]) {
                CommandOutput::Text(help)
            } else {
                CommandOutput::Text("OK".to_string())
            }
        }
        "rename-workspace" | "rename-window" => {
            let payload = run_rename_workspace_like(client, command, args).await?;
            if opts.json_output {
                CommandOutput::Json(payload)
            } else {
                CommandOutput::Text("OK".to_string())
            }
        }
        "rename-tab" => {
            let payload = run_rename_tab(client, args).await?;
            if opts.json_output {
                CommandOutput::Json(payload)
            } else {
                CommandOutput::Text("OK".to_string())
            }
        }
        "read-screen" | "capture-pane" => {
            let payload = run_read_screen(client, args).await?;
            if opts.json_output {
                CommandOutput::Json(payload)
            } else {
                CommandOutput::Text(get_string(&payload, &["text"]).unwrap_or_default())
            }
        }
        "browser" => return run_browser(client, args, opts.json_output).await,
        "open-browser" => {
            let mut bridged = vec!["open".to_string()];
            bridged.extend(args.iter().cloned());
            return run_browser(client, &bridged, opts.json_output).await;
        }
        "navigate-browser" => {
            let mut bridged = vec!["navigate".to_string()];
            bridged.extend(args.iter().cloned());
            return run_browser(client, &bridged, opts.json_output).await;
        }
        "browser-back" => {
            let mut bridged = vec!["back".to_string()];
            bridged.extend(args.iter().cloned());
            return run_browser(client, &bridged, opts.json_output).await;
        }
        "browser-forward" => {
            let mut bridged = vec!["forward".to_string()];
            bridged.extend(args.iter().cloned());
            return run_browser(client, &bridged, opts.json_output).await;
        }
        "browser-reload" => {
            let mut bridged = vec!["reload".to_string()];
            bridged.extend(args.iter().cloned());
            return run_browser(client, &bridged, opts.json_output).await;
        }
        "get-url" => {
            let mut bridged = vec!["get-url".to_string()];
            bridged.extend(args.iter().cloned());
            return run_browser(client, &bridged, opts.json_output).await;
        }
        "focus-webview" => {
            let mut bridged = vec!["focus-webview".to_string()];
            bridged.extend(args.iter().cloned());
            return run_browser(client, &bridged, opts.json_output).await;
        }
        "is-webview-focused" => {
            let mut bridged = vec!["is-webview-focused".to_string()];
            bridged.extend(args.iter().cloned());
            return run_browser(client, &bridged, opts.json_output).await;
        }
        "pipe-pane" | "wait-for" | "find-window" | "last-window" | "next-window"
        | "previous-window" | "swap-pane" | "break-pane" | "join-pane" | "last-pane"
        | "clear-history" | "set-hook" | "resize-pane" | "resizep" | "set-buffer" | "setb"
        | "list-buffers" | "show-buffer" | "showb" | "paste-buffer" | "pasteb" | "respawn-pane"
        | "respawnp" | "display-message" | "display" | "displayp" | "capturep" | "popup"
        | "bind-key" | "unbind-key" | "copy-mode" => {
            let payload = run_tmux_compat(client, command, args).await?;
            if opts.json_output {
                CommandOutput::Json(payload)
            } else if let Some(text) = get_string(&payload, &["text"]) {
                CommandOutput::Text(text)
            } else {
                CommandOutput::Text("OK".to_string())
            }
        }
        _ => bail!("unknown command: {}", command),
    };

    if let CommandOutput::Json(ref mut payload) = out {
        apply_id_format(payload, effective_id_format);
    }

    Ok(out)
}

#[tokio::main]
async fn main() -> Result<()> {
    let opts = parse_global_args()?;
    if should_launch_host(&opts) {
        return launch_host();
    }
    if let Some(output) = run_local_command(&opts)? {
        print_command_output(output, opts.pretty)?;
        return Ok(());
    }

    let socket = resolve_socket_path_checked(opts.socket.clone(), opts.socket_mode)
        .map_err(anyhow::Error::msg)?;
    let password = auth::socket_password_from_env_or_file(opts.password.as_deref())
        .context("failed to resolve socket password")?;

    let mut client = Client::new(socket, password);
    let output = execute_command(&mut client, &opts).await;

    match output {
        Ok(output) => {
            let status = command_output_status(&output);
            print_command_output(output, opts.pretty)?;
            if let Some(status) = status {
                std::process::exit(status);
            }
            Ok(())
        }
        Err(err) => {
            eprintln!("{}", err);
            std::process::exit(1);
        }
    }
}

// purpose: Report an explicit process exit status requested by a command output.
// inputs: Completed CLI command output.
// returns/effects: Returns the desired status when the caller must terminate nonzero.
fn command_output_status(output: &CommandOutput) -> Option<i32> {
    match output {
        CommandOutput::Exit { status, .. } => Some(*status),
        _ => None,
    }
}

/// purpose: Print a command result consistently for local and socket-backed commands.
/// inputs: The command output and pretty-print setting.
/// returns/effects: Writes to stdout or returns JSON encoding errors.
fn print_command_output(output: CommandOutput, pretty: bool) -> Result<()> {
    match output {
        CommandOutput::Silent => Ok(()),
        CommandOutput::Text(text) => {
            println!("{}", text);
            Ok(())
        }
        CommandOutput::Exit { stdout, stderr, .. } => {
            if !stdout.is_empty() {
                println!("{}", stdout);
            }
            if !stderr.is_empty() {
                eprintln!("{}", stderr);
            }
            Ok(())
        }
        CommandOutput::Json(value) => {
            if pretty {
                println!(
                    "{}",
                    serde_json::to_string_pretty(&value)
                        .context("failed to pretty print response")?
                );
            } else {
                println!(
                    "{}",
                    serde_json::to_string(&value).context("failed to encode json output")?
                );
            }
            Ok(())
        }
    }
}

#[cfg(test)]
mod cli_arg_tests {
    use super::*;

    struct EnvGuard {
        key: &'static str,
        previous: Option<String>,
    }

    impl EnvGuard {
        fn set(key: &'static str, value: &str) -> Self {
            let previous = env::var(key).ok();
            unsafe {
                env::set_var(key, value);
            }
            Self { key, previous }
        }
    }

    impl Drop for EnvGuard {
        fn drop(&mut self) {
            unsafe {
                if let Some(value) = self.previous.as_ref() {
                    env::set_var(self.key, value);
                } else {
                    env::remove_var(self.key);
                }
            }
        }
    }

    fn args(values: &[&str]) -> Vec<String> {
        values.iter().map(|value| value.to_string()).collect()
    }

    fn default_opts(command_args: Vec<String>) -> GlobalOptions {
        GlobalOptions {
            socket: None,
            socket_mode: SocketMode::Runtime,
            password: None,
            window: None,
            json_output: false,
            id_format: IdFormat::Refs,
            request: None,
            pretty: false,
            command_args,
        }
    }

    #[test]
    fn no_args_launches_host_but_cli_flags_do_not() {
        assert!(should_launch_host(&default_opts(Vec::new())));

        let mut json_only = default_opts(Vec::new());
        json_only.json_output = true;
        assert!(!should_launch_host(&json_only));

        let mut window_only = default_opts(Vec::new());
        window_only.window = Some("window:1".to_string());
        assert!(!should_launch_host(&window_only));

        assert!(!should_launch_host(&default_opts(args(&[
            "list-workspaces"
        ]))));
    }

    #[test]
    fn cmux_global_window_and_password_parse_before_command() {
        let opts = parse_global_args_from(args(&[
            "--window",
            "window:3",
            "--password",
            "secret",
            "focus-window",
        ]))
        .expect("global args parse");

        assert_eq!(opts.window.as_deref(), Some("window:3"));
        assert_eq!(opts.password.as_deref(), Some("secret"));
        assert_eq!(opts.command_args, args(&["focus-window"]));
    }

    #[tokio::test]
    async fn client_sends_auth_login_before_request_when_password_is_set() {
        let dir = tempfile::tempdir().expect("tempdir");
        let socket = dir.path().join("limux.sock");
        let listener = tokio::net::UnixListener::bind(&socket).expect("bind mock socket");
        let server = tokio::spawn(async move {
            let (stream, _) = listener.accept().await.expect("accept client");
            let (reader_half, mut writer_half) = stream.into_split();
            let mut reader = BufReader::new(reader_half);

            let mut auth_line = String::new();
            reader.read_line(&mut auth_line).await.expect("read auth");
            let auth_request: V2Request =
                serde_json::from_str(auth_line.trim()).expect("parse auth request");
            assert_eq!(auth_request.method, "auth.login");
            assert_eq!(auth_request.params["password"], "secret");
            let auth_response =
                V2Response::success(auth_request.id, json!({"ok": true, "authenticated": true}));
            let mut encoded_auth = serde_json::to_string(&auth_response).expect("encode auth");
            encoded_auth.push('\n');
            writer_half
                .write_all(encoded_auth.as_bytes())
                .await
                .expect("write auth response");

            let mut request_line = String::new();
            reader
                .read_line(&mut request_line)
                .await
                .expect("read request");
            let request: V2Request =
                serde_json::from_str(request_line.trim()).expect("parse request");
            assert_eq!(request.method, "system.ping");
            let response = V2Response::success(request.id, json!({"pong": true}));
            let mut encoded = serde_json::to_string(&response).expect("encode response");
            encoded.push('\n');
            writer_half
                .write_all(encoded.as_bytes())
                .await
                .expect("write response");
        });

        let mut client = Client::new(socket, Some("secret".to_string()));
        let result = client
            .call("system.ping", json!({}))
            .await
            .expect("client request");
        assert_eq!(result["pong"], true);
        server.await.expect("mock server");
    }

    #[test]
    fn cmux_command_position_presentation_flags_parse_before_socket() {
        let opts =
            parse_global_args_from(args(&["list-workspaces", "--json", "--id-format", "both"]))
                .expect("command-position presentation args parse");

        assert!(opts.json_output);
        assert_eq!(opts.id_format, IdFormat::Both);
        assert_eq!(opts.command_args, args(&["list-workspaces"]));
    }

    #[test]
    fn cmux_command_position_presentation_flags_preserve_command_options() {
        let opts = parse_global_args_from(args(&[
            "list-panes",
            "--workspace",
            "workspace-a",
            "--id-format",
            "uuids",
            "--json",
        ]))
        .expect("mixed command options parse");

        assert!(opts.json_output);
        assert_eq!(opts.id_format, IdFormat::Uuids);
        assert_eq!(
            opts.command_args,
            args(&["list-panes", "--workspace", "workspace-a"])
        );
    }

    #[test]
    fn cmux_command_position_id_format_errors_loudly() {
        let err = parse_global_args_from(args(&["list-workspaces", "--id-format"]))
            .expect_err("missing id-format value should fail");

        assert!(err.to_string().contains("--id-format requires"));
    }

    #[test]
    fn cmux_right_sidebar_cli_forms_map_to_bridge_params() {
        let set = build_right_sidebar_request(
            &args(&["set", "find", "--no-focus", "--workspace=workspace:2"]),
            Some("window:7"),
        )
        .expect("right-sidebar set parses");
        assert_eq!(set["action"], "set");
        assert_eq!(set["mode"], "find");
        assert_eq!(set["focus"], false);
        assert_eq!(set["workspace_id"], "workspace:2");
        assert_eq!(set["window_id"], "window:7");

        let alias = build_right_sidebar_request(&args(&["dock", "--window", "window:3"]), None)
            .expect("right-sidebar mode alias parses");
        assert_eq!(alias["action"], "set");
        assert_eq!(alias["mode"], "dock");
        assert_eq!(alias["focus"], true);
        assert_eq!(alias["window_id"], "window:3");

        let mode = build_right_sidebar_request(&args(&["mode"]), None).expect("mode parses");
        assert_eq!(mode["action"], "mode");
    }

    #[test]
    fn cmux_right_sidebar_rejects_invalid_no_focus_and_modes() {
        let no_focus = build_right_sidebar_request(&args(&["toggle", "--no-focus"]), None)
            .expect_err("no-focus outside set fails");
        assert!(no_focus.to_string().contains("--no-focus"));

        let bad_mode = build_right_sidebar_request(&args(&["set", "unknown"]), None)
            .expect_err("unknown mode fails");
        assert!(bad_mode.to_string().contains("Unknown right-sidebar mode"));
    }

    #[test]
    fn cmux_sidebar_status_cli_maps_to_bridge_params() {
        let params = build_sidebar_command_request(
            "set-status",
            &args(&[
                "build",
                "running",
                "--icon=hammer",
                "--color",
                "#ff9500",
                "--priority",
                "80",
                "--workspace",
                "workspace:2",
            ]),
            Some("window:7"),
        )
        .expect("set-status parses");

        assert_eq!(params["key"], "build");
        assert_eq!(params["value"], "running");
        assert_eq!(params["icon"], "hammer");
        assert_eq!(params["color"], "#ff9500");
        assert_eq!(params["priority"], 80);
        assert_eq!(params["workspace_id"], "workspace:2");
        assert_eq!(params["window_id"], "window:7");
    }

    #[test]
    fn cmux_sidebar_progress_and_log_cli_validate_values() {
        let progress = build_sidebar_command_request(
            "set-progress",
            &args(&["0.5", "--label", "Building"]),
            None,
        )
        .expect("set-progress parses");
        assert_eq!(progress["value"], 0.5);
        assert_eq!(progress["label"], "Building");

        let bad_progress = build_sidebar_command_request("set-progress", &args(&["1.5"]), None)
            .expect_err("out-of-range progress fails");
        assert!(bad_progress.to_string().contains("between 0.0 and 1.0"));

        let log = build_sidebar_command_request(
            "log",
            &args(&[
                "--level",
                "error",
                "--source",
                "build",
                "--",
                "Compilation failed",
            ]),
            None,
        )
        .expect("log parses");
        assert_eq!(log["level"], "error");
        assert_eq!(log["source"], "build");
        assert_eq!(log["message"], "Compilation failed");

        let bad_level =
            build_sidebar_command_request("log", &args(&["--level", "debug", "message"]), None)
                .expect_err("bad log level fails");
        assert!(bad_level.to_string().contains("--level must be"));
    }

    #[test]
    fn cmux_sidebar_state_text_includes_metadata_sections() {
        let rendered = render_sidebar_state_text(&json!({
            "workspace": "ws-1",
            "cwd": "/repo",
            "git_branch": "main",
            "progress": {"value": 0.5, "label": "Building"},
            "status": [{"key": "build", "value": "running", "priority": 80}],
            "log": [{
                "created_at": "2026-07-02T04:37:18Z",
                "level": "info",
                "source": "build",
                "message": "Started"
            }]
        }));

        assert!(rendered.contains("progress=0.5 label=Building"));
        assert!(rendered.contains("build=running priority=80"));
        assert!(rendered.contains("2026-07-02T04:37:18Z info build: Started"));
    }

    #[test]
    fn cmux_global_version_parses_without_command_socket() {
        let opts = parse_global_args_from(args(&["--version"])).expect("version parses");

        assert_eq!(opts.command_args, args(&["version"]));
        assert_eq!(
            version_text(),
            format!("limux {}", env!("CARGO_PKG_VERSION"))
        );
    }

    #[test]
    fn local_docs_command_returns_without_socket_client() {
        let opts = default_opts(args(&["docs", "agents"]));
        let output = run_local_command(&opts)
            .expect("local command runs")
            .expect("docs is local");

        match output {
            CommandOutput::Text(text) => {
                assert!(text.contains("Agent docs"));
                assert!(text.contains("limux hooks setup"));
            }
            CommandOutput::Silent => panic!("docs should render text"),
            CommandOutput::Json(_) => panic!("docs should render text"),
            CommandOutput::Exit { .. } => panic!("docs should render text"),
        }
    }

    // purpose: Verify CMUX command help probes do not require a running socket.
    // inputs: Representative socket-backed and legacy alias commands with --help.
    // returns/effects: Asserts local help output is returned before socket routing.
    #[test]
    fn cmux_command_help_probes_return_without_socket_client() {
        for (command, expected) in [
            ("list-panes", "Usage: limux list-panes"),
            ("list-windows", "Usage: limux list-windows"),
            ("resize-pane", "Usage: limux resize-pane"),
            ("ssh", "--forward-agent"),
            ("open-browser", "Legacy alias for 'limux browser open'"),
        ] {
            let opts = default_opts(args(&[command, "--help"]));
            let output = run_local_command(&opts)
                .expect("local help runs")
                .expect("help is local");
            let CommandOutput::Text(text) = output else {
                panic!("help should render text");
            };
            assert!(text.contains(expected), "{command} output: {text}");
        }
    }

    #[test]
    fn cmux_feed_tui_maps_to_right_sidebar_feed_mode() {
        let opts = default_opts(args(&["feed", "tui"]));
        assert!(run_local_command(&opts)
            .expect("local command lookup")
            .is_none());

        let request = build_feed_tui_request(&args(&["tui", "--opentui"]), Some("window:7"))
            .expect("feed tui request");
        assert_eq!(request["action"], "set");
        assert_eq!(request["mode"], "feed");
        assert_eq!(request["focus"], true);
        assert_eq!(request["window_id"], "window:7");

        let help = run_local_command(&default_opts(args(&["feed", "--help"])))
            .expect("help probe")
            .expect("help output");
        let CommandOutput::Text(text) = help else {
            panic!("help should render text");
        };
        assert!(text.contains("Usage: limux feed tui"));
    }

    #[test]
    fn cmux_codex_teams_help_is_local_and_execution_is_remote() {
        let help = run_local_command(&default_opts(args(&["codex-teams", "--help"])))
            .expect("help probe")
            .expect("help output");
        let CommandOutput::Text(text) = help else {
            panic!("help should render text");
        };
        assert!(text.contains("Usage: limux codex-teams [codex-args...]"));
        assert!(text.contains("bridges app-server approvals through Feed"));

        let opts = default_opts(args(&["codex-teams", "--model", "gpt-5.4"]));
        assert!(run_local_command(&opts)
            .expect("codex-teams should route to socket-backed execution")
            .is_none());
    }

    #[tokio::test]
    async fn cmux_codex_teams_public_launcher_requires_limux_terminal_context() {
        let client = Client::new(PathBuf::from("/tmp/limux-missing.sock"), None);
        let error = run_codex_teams(&client, &args(&["--model", "gpt-5.4"]), None)
            .await
            .expect_err("missing Limux terminal env fails before process startup");
        assert!(error
            .to_string()
            .contains("limux codex-teams must be started from a Limux terminal surface"));
    }

    #[tokio::test]
    async fn cmux_codex_teams_hidden_watcher_routes_and_validates_required_args() {
        let opts = default_opts(args(&["__codex-teams-watch"]));
        assert!(run_local_command(&opts)
            .expect("local command lookup")
            .is_none());

        let mut client = Client::new(PathBuf::from("/tmp/limux-missing.sock"), None);
        let error = run_codex_teams_watcher(&mut client, &[])
            .await
            .expect_err("missing watcher args fail before socket use");
        assert!(error
            .to_string()
            .contains("__codex-teams-watch requires --workspace-id"));
    }

    #[test]
    fn cmux_feed_clear_removes_local_workstream_history() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("workstream.jsonl");
        let rotated = dir.path().join("workstream.jsonl.1");
        fs::write(&path, "{}\n").expect("seed history");
        fs::write(&rotated, "{}\n").expect("seed rotated history");

        let output = run_feed_clear_at(&args(&["clear", "--yes"]), true, &path)
            .expect("feed clear succeeds");
        let CommandOutput::Json(payload) = output else {
            panic!("feed clear should render JSON");
        };

        assert_eq!(payload["cleared"], true);
        assert!(!path.exists());
        assert!(!rotated.exists());
        assert_eq!(
            payload["removed_paths"]
                .as_array()
                .expect("removed paths")
                .len(),
            2
        );
    }

    #[test]
    fn cmux_remote_ssh_commands_fail_locally_with_not_supported() {
        for command in [
            "ssh",
            "ssh-tmux",
            "ssh-session-list",
            "ssh-session-attach",
            "ssh-session-cleanup",
            "remote-daemon-status",
            "remote",
            "remotes",
        ] {
            assert!(is_unsupported_remote_cli_command(command));
            let opts = default_opts(args(&[command]));
            let error = run_local_command(&opts).expect_err("remote command should fail locally");
            assert!(error.to_string().contains("not_supported"), "{command}");
        }

        let help = run_local_command(&default_opts(args(&["ssh-session-list", "--help"])))
            .expect("help probe")
            .expect("help output");
        let CommandOutput::Text(text) = help else {
            panic!("help should render text");
        };
        assert!(text.contains("Usage: limux ssh-session-list"));

        let remote_help = run_local_command(&default_opts(args(&["remotes", "--help"])))
            .expect("help probe")
            .expect("help output");
        let CommandOutput::Text(text) = remote_help else {
            panic!("help should render text");
        };
        assert!(text.contains("Usage: limux remotes <list|add|remove>"));
    }

    // purpose: Verify normal command arguments still use the socket-backed path.
    // inputs: A supported socket-backed command without --help.
    // returns/effects: Asserts local command handling declines the command.
    #[test]
    fn cmux_non_help_probe_still_requires_command_dispatch() {
        let opts = default_opts(args(&["list-panes", "--workspace", "workspace-a"]));
        assert!(run_local_command(&opts)
            .expect("local command lookup")
            .is_none());
    }

    #[test]
    fn read_screen_params_include_lines_and_scrollback() {
        let params = build_read_screen_params(&args(&[
            "--workspace",
            "workspace:2",
            "--surface",
            "surface:9:tab",
            "--lines",
            "5",
        ]))
        .expect("read-screen params");

        assert_eq!(params["workspace_id"], "workspace:2");
        assert_eq!(params["surface_id"], "surface:9:tab");
        assert_eq!(params["lines"], 5);
        assert_eq!(params["scrollback"], true);

        let explicit =
            build_read_screen_params(&args(&["--scrollback"])).expect("explicit scrollback params");
        assert_eq!(explicit["scrollback"], true);

        let error =
            build_read_screen_params(&args(&["--lines", "0"])).expect_err("zero lines should fail");
        assert!(error.to_string().contains("--lines must be greater than 0"));
    }

    #[test]
    fn renders_workspace_group_list_text() {
        let payload = json!({
            "groups": [
                {
                    "group_id": "group-1",
                    "group_ref": "workspace_group:group-1",
                    "name": "Agents",
                    "isPinned": true
                }
            ]
        });

        assert_eq!(
            render_list_text("list-workspace-groups", &payload),
            "* workspace_group:group-1 Agents"
        );
        assert_eq!(
            render_list_text("list-workspace-groups", &json!({ "groups": [] })),
            "No workspace groups"
        );
    }

    #[test]
    fn workspace_group_cli_helpers_extract_group_and_render_mutations() {
        assert_eq!(
            workspace_group_arg("rename", &args(&["workspace_group:2", "--name", "New"]))
                .expect("group id"),
            "workspace_group:2"
        );
        assert_eq!(
            workspace_group_arg("pin", &args(&["--group", "group-1"])).expect("group flag"),
            "group-1"
        );
        assert!(workspace_group_arg("pin", &[]).is_err());

        let mutation = json!({ "group_id": "group-1", "ok": true });
        assert_eq!(
            render_workspace_group_text(&args(&["rename", "group-1"]), &mutation),
            "{\n  \"group_id\": \"group-1\",\n  \"ok\": true\n}"
        );
    }

    #[test]
    fn config_validation_reports_valid_missing_and_corrupt_files() {
        let dir = tempfile::tempdir().expect("tempdir");
        let settings = dir.path().join("settings.json");
        let shortcuts = dir.path().join("shortcuts.json");
        fs::write(&settings, br#"{"appearance":{"color_scheme":"dark"}}"#).expect("write settings");

        let text = config_validation_text_for_targets(vec![
            ConfigDoctorTarget {
                label: "settings".to_string(),
                path: settings.clone(),
                missing_is_error: false,
            },
            ConfigDoctorTarget {
                label: "shortcuts".to_string(),
                path: shortcuts.clone(),
                missing_is_error: false,
            },
        ])
        .expect("validate config");
        assert!(text.contains("OK settings"));
        assert!(text.contains("MISSING shortcuts"));

        fs::write(&shortcuts, b"{invalid").expect("write corrupt shortcuts");
        let err = config_validation_text_for_targets(vec![ConfigDoctorTarget {
            label: "shortcuts".to_string(),
            path: shortcuts.clone(),
            missing_is_error: false,
        }])
        .expect_err("corrupt shortcuts must fail");
        assert!(
            err.to_string().contains("is not valid JSON"),
            "unexpected error: {err}"
        );
    }

    #[test]
    fn config_doctor_accepts_custom_path_targets() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("cmux.json");
        fs::write(&path, br#"{"commands":[]}"#).expect("write custom config");

        let targets =
            parse_config_doctor_targets(&args(&["--path", path.to_str().expect("utf8 path")]))
                .expect("parse targets")
                .expect("custom targets");
        let text = config_validation_text_for_targets(targets).expect("doctor output");

        assert!(text.contains("OK custom 1"));
        assert!(text.contains("keys: commands"));
    }

    #[test]
    fn config_doctor_accepts_jsonc_comments_and_trailing_commas() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("cmux.json");
        fs::write(
            &path,
            br#"{
                // project commands
                "commands": [
                    {
                        "name": "Build",
                        "command": "echo ok", // trailing line comment
                    },
                ],
                /* grouped workspace config */
                "workspaceGroups": {
                    "newWorkspacePlacement": "afterCurrent",
                },
            }"#,
        )
        .expect("write jsonc config");

        let text = config_validation_text_for_targets(vec![ConfigDoctorTarget {
            label: "project".to_string(),
            path,
            missing_is_error: false,
        }])
        .expect("jsonc doctor output");

        assert!(text.contains("OK project"));
        assert!(text.contains("JSONC syntax is valid"));
        assert!(text.contains("commands"));
        assert!(text.contains("workspaceGroups"));
    }

    #[test]
    fn jsonc_preprocess_preserves_comment_like_text_inside_strings() {
        let raw = br#"{
            "url": "https://example.com/a//b",
            "pattern": "not /* a comment */",
        }"#;

        let sanitized = jsonc_preprocess(raw).expect("preprocess jsonc");
        let value: Value = serde_json::from_slice(&sanitized).expect("strict json");

        assert_eq!(value["url"], "https://example.com/a//b");
        assert_eq!(value["pattern"], "not /* a comment */");
    }

    #[test]
    fn config_reload_commands_use_socket_path() {
        assert!(
            run_local_command(&default_opts(args(&["config", "reload"])))
                .expect("config reload local check")
                .is_none()
        );
        assert!(run_local_command(&default_opts(args(&["reload-config"])))
            .expect("reload-config local check")
            .is_none());
    }

    #[test]
    fn settings_open_uses_socket_path() {
        assert!(
            run_local_command(&default_opts(args(&["settings", "open"])))
                .expect("settings open local check")
                .is_none()
        );
        let path_output = run_local_command(&default_opts(args(&["settings", "path"])))
            .expect("settings path local check")
            .expect("settings path stays local");
        assert!(matches!(path_output, CommandOutput::Text(_)));
        assert_eq!(settings_target_raw_value("general"), Some("app"));
        assert_eq!(
            settings_target_raw_value("shortcuts"),
            Some("keyboardShortcuts")
        );
        assert_eq!(
            settings_target_raw_value("settings-json"),
            Some("settingsJSON")
        );
        assert_eq!(settings_target_raw_value("missing"), None);
    }

    #[test]
    fn theme_set_and_clear_manage_ghostty_config_block_without_losing_user_lines() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("ghostty/config");
        fs::create_dir_all(path.parent().expect("ghostty parent")).expect("create config dir");
        fs::write(&path, "font-size = 12\n").expect("write config");

        let output =
            run_themes_set_at(&path, &args(&["Catppuccin", "Mocha"]), false).expect("set themes");
        assert!(matches!(output, CommandOutput::Text(_)));

        let contents = fs::read_to_string(&path).expect("read config");
        assert!(contents.contains("font-size = 12"));
        assert!(contents.contains(CMUX_THEMES_BLOCK_START));
        assert!(contents.contains("theme = light:Catppuccin Mocha,dark:Catppuccin Mocha"));

        let current = current_theme_selection_at(&path).expect("current theme");
        assert_eq!(current.light.as_deref(), Some("Catppuccin Mocha"));
        assert_eq!(current.dark.as_deref(), Some("Catppuccin Mocha"));

        run_themes_clear_at(&path, false).expect("clear themes");
        let contents = fs::read_to_string(&path).expect("read cleared config");
        assert!(contents.contains("font-size = 12"));
        assert!(!contents.contains(CMUX_THEMES_BLOCK_START));
        assert!(!contents.contains("Catppuccin Mocha"));
    }

    #[test]
    fn theme_set_updates_one_side_from_current_selection() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("ghostty/config");
        write_managed_theme_override_at(&path, "light:Catppuccin Latte,dark:Catppuccin Mocha")
            .expect("seed themes");

        run_themes_set_at(&path, &args(&["--dark", "Tokyo Night"]), false).expect("set dark theme");
        let current = current_theme_selection_at(&path).expect("current theme");
        assert_eq!(current.light.as_deref(), Some("Catppuccin Latte"));
        assert_eq!(current.dark.as_deref(), Some("Tokyo Night"));
    }

    #[test]
    fn theme_set_rejects_malformed_name_without_rewriting_file() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("ghostty/config");
        fs::create_dir_all(path.parent().expect("ghostty parent")).expect("create config dir");
        fs::write(&path, "font-size = 12\n").expect("write config");

        let err = run_themes_set_at(&path, &args(&["Bad,Theme"]), false)
            .expect_err("bad theme should fail");
        assert!(err.to_string().contains("unsupported control or separator"));
        assert_eq!(
            fs::read_to_string(&path).expect("read config"),
            "font-size = 12\n"
        );
    }

    #[test]
    fn themes_json_outputs_cmux_shape() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("ghostty/config");

        let output = run_themes_set_at(&path, &args(&["--light", "Gruvbox Light"]), true)
            .expect("json set themes");
        let CommandOutput::Json(payload) = output else {
            panic!("expected json output");
        };
        assert_eq!(payload["ok"], true);
        assert_eq!(payload["light"], "Gruvbox Light");
        assert_eq!(payload["raw_value"], "light:Gruvbox Light");

        let output = render_themes_list(&path, true).expect("json list themes");
        let CommandOutput::Json(payload) = output else {
            panic!("expected json list");
        };
        assert_eq!(payload["current"]["light"], "Gruvbox Light");
        assert_eq!(payload["catalog"], "external_ghostty");
    }

    #[test]
    fn config_font_size_get_uses_defaults_and_formats_values() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("settings.json");

        let text = render_config_font_size_get(&path, SIDEBAR_FONT_SIZE).expect("get default");
        assert!(text.contains("sidebar-font-size = 12.5"));

        fs::write(&path, br#"{"sidebar-font-size":13.75}"#).expect("write settings");
        let text = render_config_font_size_get(&path, SIDEBAR_FONT_SIZE).expect("get configured");
        assert!(text.contains("sidebar-font-size = 13.75"));
    }

    #[test]
    fn config_font_size_set_clamps_and_preserves_unrelated_settings() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("settings.json");
        fs::write(&path, br#"{"focus":{"hover_terminal_focus":true}}"#).expect("write settings");

        let text = render_config_font_size_set(&path, SURFACE_TAB_BAR_FONT_SIZE, "40")
            .expect("set font size");
        assert!(text.contains("surface-tab-bar-font-size = 14"));

        let parsed: Value =
            serde_json::from_slice(&fs::read(&path).expect("read settings")).expect("json");
        assert_eq!(parsed["focus"]["hover_terminal_focus"], true);
        assert_eq!(parsed["surface-tab-bar-font-size"], 14.0);
    }

    #[test]
    fn config_font_size_rejects_unknown_keys_and_non_numeric_values() {
        assert!(font_size_setting("font-size").is_none());
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("settings.json");
        let err = render_config_font_size_set(&path, SIDEBAR_FONT_SIZE, "large")
            .expect_err("invalid value");
        assert!(err.to_string().contains("requires a numeric point size"));
    }

    #[test]
    fn workspace_env_args_read_files_and_cli_overrides() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("workspace.env");
        fs::write(&path, "FOO=file\n# ignored\nexport BAR=baz\n").expect("write env file");

        let values = parse_workspace_env_args_with_base(
            &args(&[
                "--env-file",
                path.to_str().expect("utf8 path"),
                "--env",
                "FOO=cli",
            ]),
            BTreeMap::new(),
        )
        .expect("parse env args");

        assert_eq!(values.get("FOO").map(String::as_str), Some("cli"));
        assert_eq!(values.get("BAR").map(String::as_str), Some("baz"));
    }

    #[test]
    fn project_cmux_config_search_prefers_dot_cmux_and_stops_before_home() {
        let dir = tempfile::tempdir().expect("tempdir");
        let home = dir.path();
        let project = home.join("project");
        let nested = project.join("src");
        fs::create_dir_all(project.join(".cmux")).expect("create dot cmux");
        fs::create_dir_all(&nested).expect("create nested");
        fs::write(project.join("cmux.json"), "{}").expect("write sibling config");
        fs::write(project.join(".cmux/cmux.json"), "{}").expect("write dot config");

        let found =
            find_project_cmux_config_path_from(&nested, Some(home)).expect("project config");
        assert_eq!(found, project.join(".cmux/cmux.json"));

        fs::write(home.join(".cmux.json"), "{}").expect("write ignored home config");
        assert!(find_project_cmux_config_path_from(home, Some(home)).is_none());
    }

    #[test]
    fn project_workspace_env_reads_new_workspace_command_definition() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("cmux.json");
        fs::write(
            &path,
            r#"{
                "newWorkspaceCommand": "Build",
                "commands": [
                    {
                        "name": "Build",
                        "workspace": {
                            "env": {
                                "AWS_PROFILE": "project",
                                "NODE_ENV": "test"
                            }
                        }
                    }
                ]
            }"#,
        )
        .expect("write project config");

        let values = read_project_workspace_env_at(&path).expect("project env");

        assert_eq!(
            values.get("AWS_PROFILE").map(String::as_str),
            Some("project")
        );
        assert_eq!(values.get("NODE_ENV").map(String::as_str), Some("test"));
    }

    #[test]
    fn project_workspace_env_merges_below_env_files_and_cli_values() {
        let dir = tempfile::tempdir().expect("tempdir");
        let env_file = dir.path().join("workspace.env");
        fs::write(&env_file, "AWS_PROFILE=file\nFROM_FILE=yes\n").expect("write env file");
        let mut project_env = BTreeMap::new();
        project_env.insert("AWS_PROFILE".to_string(), "project".to_string());
        project_env.insert("FROM_PROJECT".to_string(), "yes".to_string());

        let values = parse_workspace_env_args_with_base(
            &args(&[
                "--env-file",
                env_file.to_str().expect("utf8 path"),
                "--env",
                "AWS_PROFILE=cli",
            ]),
            project_env,
        )
        .expect("merged env");

        assert_eq!(values.get("AWS_PROFILE").map(String::as_str), Some("cli"));
        assert_eq!(values.get("FROM_FILE").map(String::as_str), Some("yes"));
        assert_eq!(values.get("FROM_PROJECT").map(String::as_str), Some("yes"));
    }

    #[test]
    fn project_workspace_env_rejects_managed_keys() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("cmux.json");
        fs::write(
            &path,
            r#"{
                "workspace": {
                    "env": {
                        "CMUX_SOCKET": "/tmp/socket"
                    }
                }
            }"#,
        )
        .expect("write project config");

        let err = read_project_workspace_env_at(&path).expect_err("managed key error");

        assert!(err.to_string().contains("invalid workspace.env key"));
    }

    #[test]
    fn new_workspace_params_include_cmux_name_title_and_env() {
        let params = build_new_workspace_params(&args(&[
            "--name",
            "Build Server",
            "--cwd",
            "/tmp/project",
            "--command",
            "npm test",
            "--env",
            "AWS_PROFILE=prod",
        ]))
        .expect("workspace params");

        assert_eq!(params["title"], "Build Server");
        assert_eq!(params["cwd"], "/tmp/project");
        assert_eq!(params["command"], "npm test");
        assert_eq!(params["workspace_env"]["AWS_PROFILE"], "prod");
    }

    #[test]
    fn new_workspace_params_accept_title_alias() {
        let params =
            build_new_workspace_params(&args(&["--title", "Launch"])).expect("workspace params");

        assert_eq!(params["title"], "Launch");
    }

    #[test]
    fn new_workspace_params_include_description_and_group_placement() {
        let params = build_new_workspace_params(&args(&[
            "--description",
            "Ship checklist",
            "--group",
            "workspace_group:agents",
            "--group-placement",
            "afterCurrent",
            "--group-reference",
            "workspace:build",
        ]))
        .expect("workspace params");

        assert_eq!(params["description"], "Ship checklist");
        assert_eq!(params["group_id"], "workspace_group:agents");
        assert_eq!(params["group_placement"], "afterCurrent");
        assert_eq!(params["group_reference_workspace_id"], "workspace:build");
    }

    #[test]
    fn new_workspace_params_include_focus_bool_aliases() {
        let focused =
            build_new_workspace_params(&args(&["--focus", "yes"])).expect("workspace params");
        let background =
            build_new_workspace_params(&args(&["--focus", "off"])).expect("workspace params");

        assert_eq!(focused["focus"], true);
        assert_eq!(background["focus"], false);
    }

    #[test]
    fn new_workspace_params_reject_invalid_focus() {
        let err =
            build_new_workspace_params(&args(&["--focus", "maybe"])).expect_err("focus error");

        assert!(err.to_string().contains("--focus must be one of"));
    }

    #[test]
    fn new_workspace_params_include_layout_and_skip_top_level_command() {
        let params = build_new_workspace_params(&args(&[
            "--layout",
            r#"{"pane":{"surfaces":[{"type":"terminal","command":"echo hi"}]}}"#,
            "--command",
            "ignored",
        ]))
        .expect("workspace params");

        assert!(params.get("command").is_none());
        assert_eq!(
            params["layout"]["pane"]["surfaces"][0]["command"],
            "echo hi"
        );
    }

    #[test]
    fn new_workspace_params_reject_invalid_layout() {
        let err =
            build_new_workspace_params(&args(&["--layout", "[1,2]"])).expect_err("layout error");

        assert!(err
            .to_string()
            .contains("--layout value must be a valid JSON object"));
    }

    #[test]
    fn workspace_env_rejects_managed_keys() {
        let err = parse_workspace_env_args_with_base(
            &args(&["--env", "CMUX_SOCKET=/tmp/socket"]),
            BTreeMap::new(),
        )
        .expect_err("managed key rejected");
        assert!(err.to_string().contains("cannot override managed key"));
    }

    #[test]
    fn workspace_env_namespace_request_targets_workspace_and_mask() {
        let (method, params) = build_workspace_namespace_request(&args(&[
            "env",
            "--workspace",
            "workspace:abc",
            "--mask",
        ]))
        .expect("workspace env request")
        .expect("request");

        assert_eq!(method, "workspace.env");
        assert_eq!(params["workspace_id"], "workspace:abc");
        assert_eq!(params["mask"], true);
    }

    #[test]
    fn workspace_remote_namespace_requests_map_to_cmux_methods() {
        let (method, params) =
            build_workspace_namespace_request(&args(&["reconnect", "workspace:abc"]))
                .expect("workspace reconnect request")
                .expect("request");
        assert_eq!(method, "workspace.remote.reconnect");
        assert_eq!(params["workspace_id"], "workspace:abc");

        let (method, params) = build_workspace_namespace_request(&args(&[
            "disconnect",
            "--workspace",
            "workspace:def",
        ]))
        .expect("workspace disconnect request")
        .expect("request");
        assert_eq!(method, "workspace.remote.disconnect");
        assert_eq!(params["workspace_id"], "workspace:def");
    }

    #[test]
    fn events_params_include_filters_after_and_no_heartbeat() {
        let params = build_events_stream_params(&args(&[
            "--after",
            "12",
            "--name",
            "notification.created",
            "--name",
            "workspace.selected",
            "--category",
            "notification",
            "--no-heartbeat",
        ]))
        .expect("events params");

        assert_eq!(params["after_seq"], 12);
        assert_eq!(
            params["names"],
            json!(["notification.created", "workspace.selected"])
        );
        assert_eq!(params["categories"], json!(["notification"]));
        assert_eq!(params["include_heartbeats"], false);
    }

    #[test]
    fn events_params_read_after_from_cursor_file() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("events.seq");
        fs::write(&path, "41\n").expect("write cursor");

        let params = build_events_stream_params(&args(&[
            "--cursor-file",
            path.to_str().expect("utf8 path"),
        ]))
        .expect("events params");

        assert_eq!(params["after_seq"], 41);
    }

    #[test]
    fn host_binary_candidates_cover_installed_and_dev_layouts() {
        let installed = Path::new("/usr/bin/limux");
        let candidates = host_binary_candidates(installed);
        assert!(candidates.contains(&PathBuf::from("/usr/libexec/limux/limux-host")));
        assert!(!candidates.contains(&PathBuf::from("/usr/bin/limux")));

        let dev = Path::new("/repo/target/debug/limux-cli");
        let candidates = host_binary_candidates(dev);
        assert!(candidates.contains(&PathBuf::from("/repo/target/debug/limux")));
    }

    #[test]
    fn wait_signal_path_uses_socket_scoped_private_state_dir() {
        let socket = Path::new("/run/user/1000/limux/limux.sock");
        let path = wait_signal_path(socket, "../unsafe name");

        assert!(path.starts_with(env::temp_dir().join("limux-cli")));
        assert!(path.ends_with("wait/___unsafe_name.sig"));
        assert!(!path.starts_with("/tmp/limux-wait-for-"));
    }

    #[test]
    fn wait_signal_creation_rejects_existing_marker_path() {
        let temp_dir = tempfile::tempdir().expect("tempdir");
        let marker = temp_dir.path().join("marker.sig");
        fs::write(&marker, b"existing").expect("existing marker");

        let error = create_wait_signal(&marker).expect_err("existing marker should fail");

        assert!(
            error
                .to_string()
                .contains("failed to create wait-for signal"),
            "{error}"
        );
        assert_eq!(
            fs::read(&marker).expect("existing marker remains"),
            b"existing"
        );
    }

    // purpose: Verify missing tmux buffers fail instead of pasting empty text.
    // inputs: A buffer map with one named entry.
    // returns/effects: Asserts successful lookup and explicit not-found error.
    #[test]
    fn tmux_buffer_lookup_fails_for_missing_named_buffer() {
        let mut buffers = BTreeMap::new();
        buffers.insert("build".to_string(), "cargo test".to_string());

        assert_eq!(
            tmux_buffer_text(&buffers, "build").expect("buffer text"),
            "cargo test"
        );
        let error = tmux_buffer_text(&buffers, "missing").expect_err("missing buffer should fail");
        assert!(error.to_string().contains("Buffer not found: missing"));
    }

    // purpose: Verify CMUX/tmux aliases use the canonical Limux implementation.
    // inputs: Short aliases used by CMUX tmux compatibility.
    // returns/effects: Asserts each alias maps to the expected command path.
    #[test]
    fn tmux_aliases_map_to_canonical_commands() {
        assert_eq!(canonical_tmux_command("capturep"), "capture-pane");
        assert_eq!(canonical_tmux_command("display"), "display-message");
        assert_eq!(canonical_tmux_command("displayp"), "display-message");
        assert_eq!(canonical_tmux_command("resizep"), "resize-pane");
        assert_eq!(canonical_tmux_command("respawnp"), "respawn-pane");
        assert_eq!(canonical_tmux_command("setb"), "set-buffer");
        assert_eq!(canonical_tmux_command("pasteb"), "paste-buffer");
        assert_eq!(canonical_tmux_command("showb"), "show-buffer");
        assert_eq!(canonical_tmux_command("lsw"), "list-windows");
        assert_eq!(canonical_tmux_command("lsp"), "list-panes");
    }

    // purpose: Verify buffer commands accept CMUX/tmux buffer-name spellings.
    // inputs: --name, -b, and no explicit buffer option.
    // returns/effects: Asserts CMUX default-buffer behavior and explicit names.
    #[test]
    fn tmux_buffer_name_accepts_name_and_b_flags() {
        assert_eq!(tmux_buffer_name_arg(&args(&["--name", "build"])), "build");
        assert_eq!(tmux_buffer_name_arg(&args(&["-b", "logs"])), "logs");
        assert_eq!(tmux_buffer_name_arg(&args(&[])), "default");
        assert_eq!(
            trailing_title(&args(&["-b", "build", "cargo test"])).as_deref(),
            Some("cargo test")
        );
    }

    // purpose: Verify CMUX/tmux display-message flags prefer positional text.
    // inputs: Short tmux -p/-t/-F flags and positional message text.
    // returns/effects: Asserts print flag, target, and selected format.
    #[test]
    fn tmux_display_message_args_parse_print_target_and_format() {
        let (print, target, format) = parse_tmux_display_message_args(&args(&[
            "-p",
            "-t",
            "%9",
            "-F",
            "#{pane_id}",
            "hello",
            "#{pane_title}",
        ]));

        assert!(print);
        assert_eq!(target.as_deref(), Some("%9"));
        assert_eq!(format.as_deref(), Some("hello #{pane_title}"));

        let (_, _, flag_format) =
            parse_tmux_display_message_args(&args(&["--format", "#{window_name}"]));
        assert_eq!(flag_format.as_deref(), Some("#{window_name}"));
    }

    // purpose: Verify CMUX/tmux format rendering behavior.
    // inputs: Known and unknown tmux format keys plus whitespace-only output.
    // returns/effects: Asserts known substitution, unknown stripping, trim, and fallback.
    #[test]
    fn tmux_display_message_renderer_strips_unknown_keys_and_falls_back() {
        let mut context = BTreeMap::new();
        context.insert("pane_id".to_string(), "%42".to_string());
        context.insert("pane_title".to_string(), "build".to_string());

        assert_eq!(
            tmux_render_format(Some(" #{pane_id}|#{missing}|#{pane_title} "), &context, ""),
            "%42||build"
        );
        assert_eq!(
            tmux_render_format(Some("#{missing}"), &context, "fallback"),
            "fallback"
        );
        assert_eq!(tmux_render_format(None, &context, "fallback"), "fallback");
    }

    // purpose: Verify CMUX/tmux list row format rendering.
    // inputs: Workspace and pane rows plus -F-style format strings.
    // returns/effects: Asserts known keys render and unknown keys strip.
    #[test]
    fn tmux_list_rows_render_format_strings() {
        let windows = vec![json!({"id": "workspace:alpha", "index": 2, "title": "Build"})];
        let panes =
            vec![json!({"id": "pane:one", "index": 1, "focused": true, "surface_count": 3})];

        assert_eq!(
            render_tmux_rows(
                &windows,
                Some("#{window_index}:#{window_name}:#{missing}"),
                add_tmux_workspace_row_context,
            ),
            "2:Build:"
        );
        assert_eq!(
            render_tmux_rows(
                &panes,
                Some("#{pane_index}:#{pane_active}:#{pane_tabs}"),
                add_tmux_pane_row_context,
            ),
            "1:1:3"
        );
    }

    // purpose: Verify respawn-pane builds CMUX-compatible surface.respawn params.
    // inputs: Workspace, surface, command flag, and positional command forms.
    // returns/effects: Asserts command metadata and target fields are present.
    #[test]
    fn respawn_pane_request_targets_surface_respawn_payload() {
        let (workspace, params) = build_respawn_pane_request(&args(&[
            "--workspace",
            "workspace:7",
            "--surface",
            "surface:9:tab",
            "--command",
            "echo ready",
        ]))
        .expect("respawn params");

        assert_eq!(workspace.as_deref(), Some("workspace:7"));
        assert_eq!(params["surface_id"], "surface:9:tab");
        assert_eq!(params["command"], "echo ready");
        assert_eq!(params["tmux_start_command"], "echo ready");

        let (_, positional) = build_respawn_pane_request(&args(&["--", "cargo", "test"]))
            .expect("positional respawn params");
        assert_eq!(positional["command"], "cargo test");

        let (_, default_shell) =
            build_respawn_pane_request(&args(&[])).expect("default shell respawn params");
        assert_eq!(default_shell["command"], "exec ${SHELL:-/bin/sh} -l");
    }

    #[test]
    fn unsupported_tmux_placeholders_are_explicit() {
        for command in ["popup", "bind-key", "unbind-key", "copy-mode"] {
            assert!(is_unsupported_tmux_cmd(command));
        }
        assert!(!is_unsupported_tmux_cmd("display-message"));
        assert_eq!(canonical_tmux_command("displayp"), "display-message");
    }

    #[test]
    fn notify_positional_title_skips_option_values() {
        let args = args(&[
            "--subtitle",
            "needs review",
            "--body",
            "blocked",
            "Input needed",
        ]);

        assert_eq!(trailing_title(&args).as_deref(), Some("Input needed"));
    }

    #[test]
    fn cmux_surface_aliases_map_to_limux_methods() {
        let request = build_surface_alias_request(
            "focus-panel",
            &args(&["--panel", "surface:7:tab-a", "--workspace", "workspace:2"]),
        )
        .expect("focus-panel parses")
        .expect("focus-panel maps");

        assert_eq!(request.0, "surface.focus");
        assert_eq!(request.1["surface_id"], "surface:7:tab-a");
        assert_eq!(request.1["workspace_id"], "workspace:2");

        let split = build_surface_alias_request(
            "new-split",
            &args(&["surface:7:tab-a", "--direction", "down"]),
        )
        .expect("new-split parses")
        .expect("new-split maps");

        assert_eq!(split.0, "surface.split");
        assert_eq!(split.1["surface_id"], "surface:7:tab-a");
        assert_eq!(split.1["direction"], "down");

        let moved = build_surface_alias_request(
            "move-surface",
            &args(&[
                "--surface",
                "surface:7:tab-a",
                "--target-pane",
                "pane:12",
                "--index",
                "2",
            ]),
        )
        .expect("move-surface parses")
        .expect("move-surface maps");
        assert_eq!(moved.0, "surface.move");
        assert_eq!(moved.1["surface_id"], "surface:7:tab-a");
        assert_eq!(moved.1["target_pane_id"], "pane:12");
        assert_eq!(moved.1["index"], 2);

        let reordered = build_surface_alias_request(
            "reorder-surface",
            &args(&[
                "--surface",
                "surface:7:tab-a",
                "--after-surface",
                "surface:7:tab-b",
            ]),
        )
        .expect("reorder-surface parses")
        .expect("reorder-surface maps");
        assert_eq!(reordered.0, "surface.reorder");
        assert_eq!(reordered.1["surface_id"], "surface:7:tab-a");
        assert_eq!(reordered.1["after_surface_id"], "surface:7:tab-b");

        let ambiguous = build_surface_alias_request(
            "reorder-surface",
            &args(&[
                "--surface",
                "surface:7:tab-a",
                "--index",
                "1",
                "--before",
                "surface:7:tab-b",
            ]),
        );
        assert!(ambiguous.is_err());

        let refreshed =
            build_surface_alias_request("refresh-surfaces", &args(&["--panel", "surface:7:tab-a"]))
                .expect("refresh-surfaces parses")
                .expect("refresh-surfaces maps");
        assert_eq!(refreshed.0, "surface.refresh");
        assert_eq!(refreshed.1["surface_id"], "surface:7:tab-a");

        let split_off = build_surface_alias_request(
            "split-off",
            &args(&["--surface", "surface:7:tab-a", "--direction", "left"]),
        )
        .expect("split-off parses")
        .expect("split-off maps");
        assert_eq!(split_off.0, "surface.drag_to_split");
        assert_eq!(split_off.1["surface_id"], "surface:7:tab-a");
        assert_eq!(split_off.1["direction"], "left");

        let drag =
            build_surface_alias_request("drag-surface-to-split", &args(&["surface:7:tab-a"]))
                .expect("drag-surface-to-split parses")
                .expect("drag-surface-to-split maps");
        assert_eq!(drag.0, "surface.drag_to_split");
        assert_eq!(drag.1["surface_id"], "surface:7:tab-a");
        assert_eq!(drag.1["direction"], "right");
    }

    #[test]
    fn cmux_window_and_workspace_aliases_map_to_limux_methods() {
        let window = build_window_alias_request("focus-window", &args(&["--window", "window:3"]))
            .expect("window parses")
            .expect("window maps");
        assert_eq!(window.0, "window.focus");
        assert_eq!(window.1["window_id"], "window:3");

        let merged = args_with_global_window(&args(&[]), Some("window:5"));
        let global_window = build_window_alias_request("focus-window", &merged)
            .expect("global window parses")
            .expect("global window maps");
        assert_eq!(global_window.1["window_id"], "window:5");

        let workspace = build_workspace_alias_request("select-workspace", &args(&["workspace:4"]))
            .expect("workspace parses")
            .expect("workspace maps");
        assert_eq!(workspace.0, "workspace.select");
        assert_eq!(workspace.1["workspace_id"], "workspace:4");
    }

    #[test]
    fn cmux_list_pane_surfaces_requires_pane_target() {
        let params = build_list_pane_surfaces_request(&args(&["--pane", "pane:8"]))
            .expect("pane surfaces parses");

        assert_eq!(params["pane_id"], "pane:8");
        assert!(build_list_pane_surfaces_request(&args(&[])).is_err());
    }

    #[test]
    fn cmux_notification_aliases_map_to_lifecycle_methods() {
        let listed = build_notification_alias_request("list-notifications", &args(&["--unread"]))
            .expect("list parses")
            .expect("list maps");
        assert_eq!(listed.0, "notification.list");
        assert_eq!(listed.1["unread_only"], true);

        let dismissed =
            build_notification_alias_request("dismiss-notification", &args(&["--all-read"]))
                .expect("dismiss parses")
                .expect("dismiss maps");
        assert_eq!(dismissed.0, "notification.dismiss");
        assert_eq!(dismissed.1["all_read"], true);

        let marked = build_notification_alias_request(
            "mark-notification-read",
            &args(&["--workspace", "workspace:agent"]),
        )
        .expect("mark parses")
        .expect("mark maps");
        assert_eq!(marked.0, "notification.mark_read");
        assert_eq!(marked.1["workspace_id"], "workspace:agent");

        let opened = build_notification_alias_request("open-notification", &args(&["42"]))
            .expect("open parses")
            .expect("open maps");
        assert_eq!(opened.0, "notification.open");
        assert_eq!(opened.1["id"], "42");

        let jumped = build_notification_alias_request("jump-to-unread", &args(&[]))
            .expect("jump parses")
            .expect("jump maps");
        assert_eq!(jumped.0, "notification.jump_to_unread");

        let cleared = build_notification_alias_request("clear-notifications", &args(&[]))
            .expect("clear parses")
            .expect("clear maps");
        assert_eq!(cleared.0, "notification.clear");
    }

    /// purpose: Verify CLI forms for CMUX documented browser gaps map to not_supported RPC names.
    /// inputs: Browser subcommands and grouped positional verbs from the CMUX browser reference.
    /// returns/effects: Asserts the CLI parser identifies each unsupported browser API.
    #[test]
    fn cmux_browser_unsupported_cli_forms_map_to_rpc_methods() {
        let cases = [
            ("viewport", &["set"][..], "browser.viewport.set"),
            ("geolocation", &["set"], "browser.geolocation.set"),
            ("offline", &["set"], "browser.offline.set"),
            ("trace", &["start"], "browser.trace.start"),
            ("trace", &["stop"], "browser.trace.stop"),
            ("network", &["route"], "browser.network.route"),
            ("network", &["unroute"], "browser.network.unroute"),
            ("network", &["requests"], "browser.network.requests"),
            ("screencast", &["start"], "browser.screencast.start"),
            ("screencast", &["stop"], "browser.screencast.stop"),
            ("input_mouse", &[], "browser.input_mouse"),
            ("input-keyboard", &[], "browser.input_keyboard"),
            ("input_touch", &[], "browser.input_touch"),
        ];

        for (sub, rest, expected) in cases {
            let rest = rest
                .iter()
                .map(|value| (*value).to_string())
                .collect::<Vec<_>>();
            assert_eq!(
                unsupported_browser_cli_method(sub, &rest).as_deref(),
                Some(expected)
            );
        }
        assert!(unsupported_browser_cli_method("viewport", &[]).is_none());
    }

    #[test]
    fn hook_event_comes_from_json_after_option_values() {
        let args = args(&["--workspace", "codex"]);
        let payload = json!({ "hook_event_name": "Notification" });

        assert_eq!(parse_hook_event(&args, &payload), "Notification");
    }

    #[test]
    fn hook_event_prefers_explicit_event_flag() {
        let args = args(&["--workspace", "codex", "--event", "Stop"]);
        let payload = json!({ "hook_event_name": "Notification" });

        assert_eq!(parse_hook_event(&args, &payload), "Stop");
    }

    #[test]
    fn hook_event_accepts_positional_event_after_options() {
        let args = args(&["--workspace", "codex", "Stop"]);
        let payload = json!({ "hook_event_name": "Notification" });

        assert_eq!(parse_hook_event(&args, &payload), "Stop");
    }

    #[test]
    fn feed_hook_builds_blocking_claude_permission_request() {
        let payload = json!({
            "session_id": "s1",
            "hook_event_name": "PermissionRequest",
            "tool_name": "Bash",
            "tool_input": { "command": "echo ok" },
            "request_id": "req-1"
        });

        let (params, actionable, event_name, tool_name, _) =
            build_feed_hook_push(&args(&["--source", "claude"]), &payload).expect("feed push");
        let event = &params["event"];

        assert!(actionable);
        assert_eq!(event_name, "PermissionRequest");
        assert_eq!(tool_name, "Bash");
        assert_eq!(params["wait_timeout_seconds"], json!(120.0));
        assert_eq!(event["session_id"], json!("claude-s1"));
        assert_eq!(event["_source"], json!("claude"));
        assert_eq!(event["_opencode_request_id"], json!("req-1"));
    }

    #[test]
    fn feed_hook_keeps_codex_permission_request_nonblocking() {
        let payload = json!({
            "session_id": "s1",
            "hook_event_name": "PermissionRequest",
            "tool_name": "shell",
            "request_id": "req-1"
        });

        let (params, actionable, event_name, _, _) =
            build_feed_hook_push(&args(&["--source", "codex"]), &payload).expect("feed push");

        assert!(!actionable);
        assert_eq!(event_name, "PreToolUse");
        assert_eq!(params["wait_timeout_seconds"], json!(0.0));
    }

    #[test]
    fn feed_hook_escalates_generic_side_effecting_tool() {
        let payload = json!({
            "session_id": "s1",
            "hook_event_name": "PreToolUse",
            "tool_name": "Write"
        });

        let (params, actionable, event_name, _, _) =
            build_feed_hook_push(&args(&["--source", "gemini"]), &payload).expect("feed push");

        assert!(actionable);
        assert_eq!(event_name, "PermissionRequest");
        assert_eq!(params["wait_timeout_seconds"], json!(120.0));
    }

    #[test]
    fn feed_hook_escalates_kiro_side_effecting_tool() {
        let payload = json!({
            "session_id": "s1",
            "hook_event_name": "preToolUse",
            "tool_name": "fs_write"
        });

        let (params, actionable, event_name, _, _) =
            build_feed_hook_push(&args(&["--source", "kiro"]), &payload).expect("feed push");

        assert!(actionable);
        assert_eq!(event_name, "PermissionRequest");
        assert_eq!(params["wait_timeout_seconds"], json!(120.0));
    }

    #[test]
    fn feed_hook_keeps_hermes_tool_calls_nonblocking() {
        let payload = json!({
            "session_id": "s1",
            "hook_event_name": "pre_tool_call",
            "tool_name": "terminal"
        });

        let (params, actionable, event_name, _, _) =
            build_feed_hook_push(&args(&["--source", "hermes-agent"]), &payload)
                .expect("feed push");

        assert!(!actionable);
        assert_eq!(event_name, "PreToolUse");
        assert_eq!(params["wait_timeout_seconds"], json!(0.0));
    }

    #[test]
    fn feed_permission_decision_renders_antigravity_output() {
        let output = render_feed_decision(
            &args(&["--source", "antigravity"]),
            None,
            &json!({}),
            &json!({ "kind": "permission", "mode": "deny" }),
        )
        .expect("render");
        let parsed: Value = serde_json::from_str(&output).expect("json");

        assert_eq!(
            parsed,
            json!({
                "decision": "deny",
                "reason": "User denied permission via Limux Feed."
            })
        );
    }

    #[test]
    fn feed_permission_decision_renders_claude_deny_output() {
        let output = render_feed_decision(
            &args(&["--source", "claude"]),
            None,
            &json!({}),
            &json!({ "kind": "permission", "mode": "deny" }),
        )
        .expect("render");
        let parsed: Value = serde_json::from_str(&output).expect("json");

        assert_eq!(
            parsed,
            json!({
                "hookSpecificOutput": {
                    "hookEventName": "PermissionRequest",
                    "decision": {
                        "behavior": "deny",
                        "message": "User denied permission via Limux Feed."
                    }
                }
            })
        );
    }

    #[test]
    fn kiro_permission_decision_preserves_exit_code_two_for_denies() {
        let allow = render_kiro_permission_decision_output(
            &json!({ "kind": "permission", "mode": "always" }),
        )
        .expect("allow output");
        let CommandOutput::Text(stdout) = allow else {
            panic!("allow should print normal hook output");
        };
        assert_eq!(stdout, "{}");

        let deny = render_kiro_permission_decision_output(
            &json!({ "kind": "permission", "mode": "deny" }),
        )
        .expect("deny output");
        let CommandOutput::Exit { stderr, status, .. } = deny else {
            panic!("deny should exit with Kiro denial status");
        };
        assert_eq!(status, 2);
        assert!(stderr.contains("denied permission"));

        let unknown = render_kiro_permission_decision_output(
            &json!({ "kind": "permission", "mode": "totally-bogus-mode" }),
        )
        .expect("unknown output");
        let CommandOutput::Exit { stderr, status, .. } = unknown else {
            panic!("unknown modes should fail closed");
        };
        assert_eq!(status, 2);
        assert!(stderr.contains("unrecognized Kiro permission decision"));
    }

    #[test]
    fn feed_question_decision_renders_claude_answers() {
        let output = render_feed_decision(
            &args(&["--source", "claude"]),
            Some(&json!({ "questions": [{ "question": "Deploy?" }] })),
            &json!({}),
            &json!({ "kind": "question", "selections": ["Yes"] }),
        )
        .expect("render");
        let parsed: Value = serde_json::from_str(&output).expect("json");

        assert_eq!(
            parsed["hookSpecificOutput"]["decision"]["updatedInput"]["answers"]["Deploy?"],
            json!("Yes")
        );
    }

    #[test]
    fn external_session_end_preserves_restorable_hook_session() {
        assert_eq!(
            agent_hook_persistence_action("SessionEnd"),
            AgentHookPersistenceAction::Preserve
        );
        assert_eq!(
            agent_hook_persistence_action("session-end"),
            AgentHookPersistenceAction::Preserve
        );
    }

    #[test]
    fn internal_cleanup_removes_restorable_hook_session() {
        assert_eq!(
            agent_hook_persistence_action("cleanup"),
            AgentHookPersistenceAction::Remove
        );
        assert_eq!(
            agent_hook_persistence_action("restore-exit"),
            AgentHookPersistenceAction::Remove
        );
    }

    #[test]
    fn default_hook_setup_omits_opencode_until_supported() {
        assert_eq!(
            default_hook_targets(),
            vec![
                agent_hooks::AgentKind::Codex,
                agent_hooks::AgentKind::Claude,
                agent_hooks::AgentKind::Gemini,
            ]
        );
        assert_eq!(
            agent_hooks::AgentKind::from_hook_name("grok"),
            Some(agent_hooks::AgentKind::Grok)
        );
        assert_eq!(
            agent_hooks::AgentKind::from_hook_name("cursor-agent"),
            Some(agent_hooks::AgentKind::Cursor)
        );
        assert_eq!(
            agent_hooks::AgentKind::from_hook_name("kiro-cli"),
            Some(agent_hooks::AgentKind::Kiro)
        );
        assert_eq!(
            agent_hooks::AgentKind::from_hook_name("agy"),
            Some(agent_hooks::AgentKind::Antigravity)
        );
        assert_eq!(
            agent_hooks::AgentKind::from_hook_name("rovo"),
            Some(agent_hooks::AgentKind::RovoDev)
        );
        assert_eq!(
            agent_hooks::AgentKind::from_hook_name("pi-coding-agent"),
            Some(agent_hooks::AgentKind::Pi)
        );
        assert_eq!(
            agent_hooks::AgentKind::from_hook_name("omp"),
            Some(agent_hooks::AgentKind::Omp)
        );
        assert_eq!(
            agent_hooks::AgentKind::from_hook_name("amp"),
            Some(agent_hooks::AgentKind::Amp)
        );
        assert_eq!(
            agent_hooks::AgentKind::from_hook_name("hermes"),
            Some(agent_hooks::AgentKind::HermesAgent)
        );
        assert_eq!(
            agent_hooks::AgentKind::from_hook_name("code-buddy"),
            Some(agent_hooks::AgentKind::CodeBuddy)
        );
        assert_eq!(
            agent_hooks::AgentKind::from_hook_name("droid"),
            Some(agent_hooks::AgentKind::Factory)
        );
        assert_eq!(
            agent_hooks::AgentKind::from_hook_name("qodercli"),
            Some(agent_hooks::AgentKind::Qoder)
        );
        assert!(!default_hook_targets().contains(&agent_hooks::AgentKind::OpenCode));
        assert!(!default_hook_targets().contains(&agent_hooks::AgentKind::Grok));
        assert!(!default_hook_targets().contains(&agent_hooks::AgentKind::Cursor));
        assert!(!default_hook_targets().contains(&agent_hooks::AgentKind::Kiro));
        assert!(!default_hook_targets().contains(&agent_hooks::AgentKind::Antigravity));
        assert!(!default_hook_targets().contains(&agent_hooks::AgentKind::RovoDev));
        assert!(!default_hook_targets().contains(&agent_hooks::AgentKind::Pi));
        assert!(!default_hook_targets().contains(&agent_hooks::AgentKind::Omp));
        assert!(!default_hook_targets().contains(&agent_hooks::AgentKind::Amp));
        assert!(!default_hook_targets().contains(&agent_hooks::AgentKind::HermesAgent));
        assert!(!default_hook_targets().contains(&agent_hooks::AgentKind::Copilot));
    }

    #[test]
    fn opencode_plugin_embeds_installer_cli_command() {
        let source = opencode_plugin_source_with_command("/tmp/limux-cli").expect("plugin source");

        assert!(source.contains("const LIMUX_COMMAND = \"/tmp/limux-cli\";"));
        assert!(source.contains("process.env.LIMUX_BIN || LIMUX_COMMAND"));
        assert!(!source.contains("process.env.LIMUX_BIN || \"limux\""));
    }

    #[test]
    fn opencode_plugin_removes_only_deleted_sessions() {
        let source = opencode_plugin_source_with_command("/tmp/limux-cli").expect("plugin source");

        assert!(
            source.contains("if (type === \"session.error\") send(\"session-end\", ctx, event);")
        );
        assert!(source.contains("if (type === \"session.deleted\") send(\"cleanup\", ctx, event);"));
        assert!(source.contains("type === \"session.status\""));
        assert!(source.contains("type === \"session.compacted\""));
    }

    #[test]
    fn omp_extension_embeds_lifecycle_hooks_and_launch_capture() {
        let source = omp_extension_source_with_command("/tmp/limux-cli").expect("extension source");

        assert!(source.contains("cmux-omp-session-extension-marker v1"));
        assert!(source.contains("const LIMUX_COMMAND = \"/tmp/limux-cli\";"));
        assert!(source.contains("api.on(\"session_start\""));
        assert!(source.contains("api.on(\"before_agent_start\""));
        assert!(source.contains("api.on(\"agent_end\""));
        assert!(source.contains("[\"hooks\", \"omp\", subcommand]"));
        assert!(source.contains("LIMUX_AGENT_LAUNCH_ARGV_B64"));
        assert!(source.contains("CMUX_AGENT_LAUNCH_ARGV_B64"));
    }

    #[test]
    fn pi_extension_embeds_lifecycle_feed_and_resume_hooks() {
        let source = pi_extension_source_with_command("/tmp/limux-cli").expect("extension source");

        assert!(source.contains("cmux-pi-session-extension-marker v1"));
        assert!(source.contains("const LIMUX_COMMAND = \"/tmp/limux-cli\";"));
        assert!(source.contains("@earendil-works/pi-coding-agent"));
        assert!(source.contains("pi.on(\"session_start\""));
        assert!(source.contains("pi.on(\"before_agent_start\""));
        assert!(source.contains("pi.on(\"tool_execution_start\""));
        assert!(source.contains("pi.on(\"tool_execution_end\""));
        assert!(source.contains("pi.on(\"agent_end\""));
        assert!(source.contains("pi.on(\"session_shutdown\""));
        assert!(source.contains("\"hooks\", \"feed\", \"--source\", \"pi\""));
        assert!(source.contains("\"surface\", \"resume\", \"set\""));
        assert!(source.contains("\"surface\", \"resume\", \"clear\""));
        assert!(source.contains("LIMUX_AGENT_LAUNCH_ARGV_B64"));
        assert!(source.contains("CMUX_AGENT_LAUNCH_ARGV_B64"));
        assert!(source.contains("secretLikeEnvKey"));
    }

    #[test]
    fn pi_extension_install_refuses_unmarked_file_and_removes_marked_file() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("cmux-session.ts");

        fs::write(&path, "export default {};\n").expect("seed user extension");
        let refused = install_marked_extension_file(
            &path,
            "// cmux-pi-session-extension-marker v1\n",
            "cmux-pi-session-extension-marker",
        );
        assert!(refused.is_err());
        assert_eq!(
            fs::read_to_string(&path).expect("read user extension"),
            "export default {};\n"
        );

        fs::write(&path, "// cmux-pi-session-extension-marker old\n").expect("seed marker");
        let source = pi_extension_source_with_command("/tmp/limux-cli").expect("extension source");
        install_marked_extension_file(&path, &source, "cmux-pi-session-extension-marker")
            .expect("install extension");
        assert_eq!(fs::read_to_string(&path).expect("read extension"), source);

        uninstall_marked_extension_file(&path, "cmux-pi-session-extension-marker")
            .expect("uninstall extension");
        assert!(!path.exists());
    }

    #[test]
    fn amp_extension_embeds_lifecycle_status_and_launch_capture() {
        let source = amp_extension_source_with_command("/tmp/limux-cli").expect("extension source");

        assert!(source.contains("cmux-amp-session-extension-marker v1"));
        assert!(source.contains("const LIMUX_COMMAND = \"/tmp/limux-cli\";"));
        assert!(source.contains("@ampcode/plugin"));
        assert!(source.contains("amp.on(\"session.start\""));
        assert!(source.contains("amp.on(\"agent.start\""));
        assert!(source.contains("amp.on(\"tool.call\""));
        assert!(source.contains("amp.on(\"tool.result\""));
        assert!(source.contains("amp.on(\"agent.end\""));
        assert!(source.contains("[\"hooks\", \"amp\", subcommand]"));
        assert!(source.contains("\"set-status\""));
        assert!(source.contains("\"clear-status\""));
        assert!(source.contains("\"log\""));
        assert!(source.contains("delete env.AMP_API_KEY"));
        assert!(source.contains("LIMUX_AGENT_LAUNCH_ARGV_B64"));
        assert!(source.contains("CMUX_AGENT_LAUNCH_ARGV_B64"));
    }

    #[test]
    fn amp_extension_install_refuses_unmarked_file_and_removes_marked_file() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("cmux-session.ts");

        fs::write(&path, "export default {};\n").expect("seed user plugin");
        let refused = install_marked_extension_file(
            &path,
            "// cmux-amp-session-extension-marker v1\n",
            "cmux-amp-session-extension-marker",
        );
        assert!(refused.is_err());
        assert_eq!(
            fs::read_to_string(&path).expect("read user plugin"),
            "export default {};\n"
        );

        fs::write(&path, "// cmux-amp-session-extension-marker old\n").expect("seed marker");
        let source = amp_extension_source_with_command("/tmp/limux-cli").expect("extension source");
        install_marked_extension_file(&path, &source, "cmux-amp-session-extension-marker")
            .expect("install extension");
        assert_eq!(fs::read_to_string(&path).expect("read extension"), source);

        uninstall_marked_extension_file(&path, "cmux-amp-session-extension-marker")
            .expect("uninstall extension");
        assert!(!path.exists());
    }

    #[test]
    fn hermes_agent_hooks_install_and_uninstall_marked_yaml_and_allowlist() {
        let events = vec![
            HermesAgentHookEvent {
                name: "on_session_start",
                command: "sh -c 'limux hooks hermes-agent session-start'".to_string(),
                timeout: 5,
            },
            HermesAgentHookEvent {
                name: "pre_approval_request",
                command: "sh -c 'limux hooks hermes-agent notification'".to_string(),
                timeout: 5,
            },
            HermesAgentHookEvent {
                name: "pre_approval_request",
                command:
                    "sh -c 'limux hooks feed --source hermes-agent --event pre_approval_request'"
                        .to_string(),
                timeout: 120,
            },
        ];
        let existing = "model: anthropic/claude-sonnet-4.6\n";

        let installed = hermes_agent_hooks_installing(existing, &events);

        assert!(installed.contains("# cmux hooks hermes-agent begin\nhooks:"));
        assert!(installed.contains("  on_session_start:"));
        assert!(installed.contains("  pre_approval_request:"));
        assert!(
            installed.contains("    - command: \"sh -c 'limux hooks hermes-agent notification'\"")
        );
        assert!(installed.contains("      timeout: 120"));
        assert_eq!(hermes_agent_hooks_uninstalling(&installed), existing);

        let allowlist = hermes_agent_allowlist_installing(
            Some(br#"{"approvals":[{"event":"user","command":"echo user"}]}"#),
            &events,
        )
        .expect("install allowlist");
        let value: Value = serde_json::from_slice(&allowlist).expect("allowlist json");
        let approvals = value
            .get("approvals")
            .and_then(Value::as_array)
            .expect("approvals");
        assert_eq!(approvals.len(), 4);
        assert!(approvals.iter().any(|approval| {
            approval.get("command").and_then(Value::as_str) == Some("echo user")
        }));

        let uninstalled =
            hermes_agent_allowlist_uninstalling(&allowlist, &events).expect("uninstall allowlist");
        let value: Value = serde_json::from_slice(&uninstalled).expect("allowlist json");
        let approvals = value
            .get("approvals")
            .and_then(Value::as_array)
            .expect("approvals");
        assert_eq!(approvals.len(), 1);
        assert_eq!(
            approvals[0].get("command").and_then(Value::as_str),
            Some("echo user")
        );
    }

    #[test]
    fn hermes_agent_install_writes_config_and_allowlist_under_home() {
        let dir = tempfile::tempdir().expect("tempdir");
        let home = dir.path().join("hermes");
        fs::create_dir_all(&home).expect("create hermes home");
        let config = home.join("config.yaml");
        fs::write(&config, "model: test\n").expect("seed config");

        let _guard = EnvGuard::set("HERMES_HOME", home.to_string_lossy().as_ref());
        install_hermes_agent_hooks().expect("install hermes hooks");

        let installed = fs::read_to_string(&config).expect("read config");
        assert!(installed.contains("# cmux hooks hermes-agent begin"));
        assert!(installed.contains("on_session_start:"));
        assert!(installed.contains("pre_tool_call:"));
        assert!(installed.contains("hooks feed --source hermes-agent"));
        assert!(installed.contains("LIMUX_HERMES_AGENT_HOOKS_DISABLED"));
        assert!(installed.contains("CMUX_HERMES_AGENT_HOOKS_DISABLED"));

        let allowlist =
            fs::read_to_string(home.join("shell-hooks-allowlist.json")).expect("read allowlist");
        assert!(allowlist.contains("pre_tool_call"));
        assert!(allowlist.contains("hooks hermes-agent session-start"));

        uninstall_hermes_agent_hooks().expect("uninstall hermes hooks");
        assert_eq!(
            fs::read_to_string(&config).expect("read config"),
            "model: test\n"
        );
    }

    #[test]
    fn omp_extension_install_refuses_unmarked_file_and_removes_marked_file() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("cmux-omp-session.ts");

        fs::write(&path, "export default {};\n").expect("seed user extension");
        let refused = install_marked_extension_file(
            &path,
            "// cmux-omp-session-extension-marker v1\n",
            "cmux-omp-session-extension-marker",
        );
        assert!(refused.is_err());
        assert_eq!(
            fs::read_to_string(&path).expect("read user extension"),
            "export default {};\n"
        );

        fs::write(&path, "// cmux-omp-session-extension-marker old\n").expect("seed marker");
        let source = omp_extension_source_with_command("/tmp/limux-cli").expect("extension source");
        install_marked_extension_file(&path, &source, "cmux-omp-session-extension-marker")
            .expect("install extension");
        assert_eq!(fs::read_to_string(&path).expect("read extension"), source);

        uninstall_marked_extension_file(&path, "cmux-omp-session-extension-marker")
            .expect("uninstall extension");
        assert!(!path.exists());
    }

    #[test]
    fn stop_hook_output_matches_codex_schema_shape() {
        let output = agent_hook_output("stop", &json!({ "session_id": "session-a" }));

        assert_eq!(
            output,
            json!({
                "continue": true,
                "suppressOutput": false
            })
        );
    }

    #[test]
    fn session_start_hook_output_uses_camel_case_specific_output() {
        let output = agent_hook_output(
            "session-start",
            &json!({ "additionalContext": "Limux session restore tracking active." }),
        );

        assert_eq!(
            output,
            json!({
                "continue": true,
                "suppressOutput": false,
                "hookSpecificOutput": {
                    "hookEventName": "SessionStart",
                    "additionalContext": "Limux session restore tracking active."
                }
            })
        );
    }

    #[test]
    fn claude_hook_install_writes_required_matcher() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("settings.json");

        install_json_hooks_with_feed(
            &path,
            agent_hooks::AgentKind::Claude,
            &[("SessionStart", "session-start")],
            &[],
        )
        .expect("install hooks");

        let root: Value =
            serde_json::from_slice(&fs::read(&path).expect("read settings")).expect("json");
        let entry = &root["hooks"]["SessionStart"][0];
        assert_eq!(entry["matcher"], "*");
        assert_eq!(entry["hooks"][0]["timeout"], 5);
        assert!(entry["hooks"][0]["command"]
            .as_str()
            .expect("command")
            .contains("hooks claude session-start"));
    }

    #[test]
    fn codex_hook_install_keeps_codex_schema_without_matcher() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("hooks.json");

        install_json_hooks_with_feed(
            &path,
            agent_hooks::AgentKind::Codex,
            &[("SessionStart", "session-start")],
            &[],
        )
        .expect("install hooks");

        let root: Value =
            serde_json::from_slice(&fs::read(&path).expect("read hooks")).expect("json");
        let entry = &root["hooks"]["SessionStart"][0];
        assert!(entry.get("matcher").is_none());
        assert_eq!(entry["hooks"][0]["timeout"], 5000);
        assert!(entry["hooks"][0]["command"]
            .as_str()
            .expect("command")
            .contains("hooks codex session-start"));
    }

    /// purpose: Verify Codex setup writes CMUX Feed hooks beside lifecycle hooks.
    /// inputs: Temporary hook JSON file and the Codex hook installer.
    /// returns/effects: Asserts installed Feed hook shape, timeout, and command markers.
    #[test]
    fn codex_hook_install_writes_feed_hooks() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("hooks.json");

        install_json_hooks_with_feed(
            &path,
            agent_hooks::AgentKind::Codex,
            &[
                ("SessionStart", "session-start"),
                ("UserPromptSubmit", "prompt-submit"),
                ("Stop", "stop"),
            ],
            codex_feed_hook_events(),
        )
        .expect("install hooks");

        let root: Value =
            serde_json::from_slice(&fs::read(&path).expect("read hooks")).expect("json");
        let feed = &root["hooks"]["PermissionRequest"][0];

        assert!(feed.get("matcher").is_none());
        assert_eq!(feed["hooks"][0]["timeout"], 5);
        assert!(feed["hooks"][0]["command"]
            .as_str()
            .expect("command")
            .contains("hooks feed --source codex --event 'PermissionRequest'"));
        assert!(root["hooks"]["PreToolUse"][0]["hooks"][0]["command"]
            .as_str()
            .expect("command")
            .contains("hooks feed --source codex --event 'PreToolUse'"));
        assert!(root["hooks"]["SessionStart"][0]["hooks"][0]["command"]
            .as_str()
            .expect("command")
            .contains("hooks codex session-start"));
    }

    /// purpose: Verify Grok setup writes CMUX lifecycle hooks and a Feed PreToolUse hook.
    /// inputs: Temporary hook JSON file and the Grok hook installer.
    /// returns/effects: Asserts installed hook shape, timeout units, and command markers.
    #[test]
    fn grok_hook_install_writes_feed_hooks() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("cmux-session.json");

        install_json_hooks_with_feed(
            &path,
            agent_hooks::AgentKind::Grok,
            &[
                ("SessionStart", "session-start"),
                ("Notification", "notification"),
            ],
            grok_feed_hook_events(),
        )
        .expect("install hooks");

        let root: Value =
            serde_json::from_slice(&fs::read(&path).expect("read hooks")).expect("json");
        let feed = &root["hooks"]["PreToolUse"][0];

        assert!(feed.get("matcher").is_none());
        assert_eq!(feed["hooks"][0]["timeout"], 120);
        assert!(feed["hooks"][0]["command"]
            .as_str()
            .expect("command")
            .contains("hooks feed --source grok --event 'PreToolUse'"));
        assert_eq!(root["hooks"]["SessionStart"][0]["hooks"][0]["timeout"], 5);
        assert!(root["hooks"]["SessionStart"][0]["hooks"][0]["command"]
            .as_str()
            .expect("command")
            .contains("hooks grok session-start"));
        assert!(root["hooks"]["Notification"][0]["hooks"][0]["command"]
            .as_str()
            .expect("command")
            .contains("hooks grok notification"));
    }

    /// purpose: Verify Cursor setup writes CMUX flat lifecycle hooks and Feed shell hook.
    /// inputs: Temporary hook JSON file and the Cursor flat hook installer.
    /// returns/effects: Asserts flat hook shape, version, and command markers.
    #[test]
    fn cursor_hook_install_writes_feed_hooks() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("hooks.json");

        install_flat_json_hooks_with_feed(
            &path,
            agent_hooks::AgentKind::Cursor,
            &[
                ("beforeSubmitPrompt", "prompt-submit"),
                ("beforeShellExecution", "shell-exec"),
                ("afterShellExecution", "shell-done"),
            ],
            cursor_feed_hook_events(),
        )
        .expect("install hooks");

        let root: Value =
            serde_json::from_slice(&fs::read(&path).expect("read hooks")).expect("json");
        let feed = &root["hooks"]["beforeShellExecution"][1];

        assert_eq!(root["version"], 1);
        assert!(root["hooks"]["beforeSubmitPrompt"][0]["command"]
            .as_str()
            .expect("command")
            .contains("hooks cursor prompt-submit"));
        assert!(root["hooks"]["beforeShellExecution"][0]["command"]
            .as_str()
            .expect("command")
            .contains("hooks cursor shell-exec"));
        assert!(feed["command"]
            .as_str()
            .expect("command")
            .contains("hooks feed --source cursor --event 'beforeShellExecution'"));
        assert!(root["hooks"]["afterShellExecution"][0]["command"]
            .as_str()
            .expect("command")
            .contains("hooks cursor shell-done"));
        assert!(feed.get("timeout").is_none());
        assert!(feed.get("hooks").is_none());
    }

    /// purpose: Verify Kiro setup writes CMUX agent JSON lifecycle and Feed hooks.
    /// inputs: Temporary Kiro agent JSON file and the Kiro hook installer.
    /// returns/effects: Asserts Kiro-specific timeout_ms shape, metadata defaults, and command markers.
    #[test]
    fn kiro_hook_install_writes_feed_hooks() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("cmux.json");

        install_kiro_agent_hooks_with_feed(
            &path,
            agent_hooks::AgentKind::Kiro,
            &[
                ("agentSpawn", "session-start"),
                ("userPromptSubmit", "prompt-submit"),
                ("stop", "stop"),
            ],
            kiro_feed_hook_events(),
        )
        .expect("install hooks");

        let root: Value =
            serde_json::from_slice(&fs::read(&path).expect("read hooks")).expect("json");
        let pre_tool = &root["hooks"]["preToolUse"][0];
        let post_tool = &root["hooks"]["postToolUse"][0];

        assert_eq!(root["name"], "cmux");
        assert_eq!(root["tools"], json!(["*"]));
        assert!(root["description"]
            .as_str()
            .expect("description")
            .contains("Kiro CLI"));
        assert_eq!(root["hooks"]["agentSpawn"][0]["timeout_ms"], 5000);
        assert!(root["hooks"]["agentSpawn"][0]["command"]
            .as_str()
            .expect("command")
            .contains("hooks kiro session-start"));
        assert_eq!(pre_tool["timeout_ms"], 120_000);
        assert!(pre_tool["command"]
            .as_str()
            .expect("command")
            .contains("hooks feed --source kiro --event 'preToolUse'"));
        assert!(!pre_tool["command"]
            .as_str()
            .expect("command")
            .contains("|| echo '{}'"));
        assert!(pre_tool["command"]
            .as_str()
            .expect("command")
            .contains("status=$?"));
        assert!(pre_tool["command"]
            .as_str()
            .expect("command")
            .contains("exit 2"));
        assert!(post_tool["command"]
            .as_str()
            .expect("command")
            .contains("hooks feed --source kiro --event 'postToolUse'"));
        assert!(pre_tool.get("hooks").is_none());
        assert!(pre_tool.get("timeout").is_none());
    }

    /// purpose: Verify Antigravity setup writes CMUX's named hook group and Feed hooks.
    /// inputs: Temporary hooks JSON file and the Antigravity installer/uninstaller.
    /// returns/effects: Asserts group shape, second-based timeouts, matcher Feed entries, and removal.
    #[test]
    fn antigravity_hook_install_writes_feed_hooks() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("hooks.json");
        fs::write(&path, r#"{"other":{"Stop":[{"command":"keep"}]}}"#).expect("seed hooks");

        install_antigravity_hooks_with_feed(
            &path,
            agent_hooks::AgentKind::Antigravity,
            &[
                ("SessionStart", "session-start"),
                ("PreInvocation", "prompt-submit"),
                ("Stop", "stop"),
            ],
            antigravity_feed_hook_events(),
        )
        .expect("install hooks");

        let root: Value =
            serde_json::from_slice(&fs::read(&path).expect("read hooks")).expect("json");
        let group = &root["cmux"];
        let pre_tool = &group["PreToolUse"][0];

        assert_eq!(root["other"]["Stop"][0]["command"], "keep");
        assert_eq!(group["SessionStart"][0]["timeout"], 10);
        assert_eq!(group["SessionStart"][0]["type"], "command");
        assert!(group["SessionStart"][0]["command"]
            .as_str()
            .expect("command")
            .contains("hooks antigravity session-start"));
        assert_eq!(pre_tool["matcher"], "*");
        assert_eq!(pre_tool["hooks"][0]["timeout"], 120);
        assert_eq!(pre_tool["hooks"][0]["type"], "command");
        assert!(pre_tool["hooks"][0]["command"]
            .as_str()
            .expect("command")
            .contains("hooks feed --source antigravity --event 'PreToolUse'"));

        uninstall_antigravity_hooks(&path, agent_hooks::AgentKind::Antigravity)
            .expect("uninstall hooks");
        let root: Value =
            serde_json::from_slice(&fs::read(&path).expect("read hooks")).expect("json");
        assert!(root.get("cmux").is_none());
        assert_eq!(root["other"]["Stop"][0]["command"], "keep");
    }

    /// purpose: Verify Rovo Dev setup writes CMUX's marked lifecycle hook YAML block.
    /// inputs: Temporary config.yml and the Rovo Dev installer/uninstaller.
    /// returns/effects: Asserts root block shape, lifecycle commands, idempotence, and uninstall.
    #[test]
    fn rovodev_hook_install_writes_lifecycle_hooks() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("config.yml");
        fs::write(&path, "sessions:\n  persistenceDir: /tmp/rovo\n").expect("seed config");

        install_rovodev_hooks(
            &path,
            agent_hooks::AgentKind::RovoDev,
            &[
                ("on_complete", "stop"),
                ("on_error", "stop"),
                ("on_tool_permission", "prompt-submit"),
            ],
        )
        .expect("install hooks");
        let installed = fs::read_to_string(&path).expect("read config");

        assert!(installed.contains("# cmux hooks rovodev begin"));
        assert!(installed.contains("eventHooks:\n  events:"));
        assert!(installed.contains("    - name: on_complete"));
        assert!(installed.contains("hooks rovodev stop"));
        assert!(installed.contains("    - name: on_tool_permission"));
        assert!(installed.contains("hooks rovodev prompt-submit"));
        assert!(installed.contains("sessions:\n  persistenceDir: /tmp/rovo"));

        install_rovodev_hooks(
            &path,
            agent_hooks::AgentKind::RovoDev,
            &[
                ("on_complete", "stop"),
                ("on_error", "stop"),
                ("on_tool_permission", "prompt-submit"),
            ],
        )
        .expect("reinstall hooks");
        assert_eq!(fs::read_to_string(&path).expect("read config"), installed);

        uninstall_rovodev_hooks(&path).expect("uninstall hooks");
        assert_eq!(
            fs::read_to_string(&path).expect("read config"),
            "sessions:\n  persistenceDir: /tmp/rovo\n"
        );
    }

    /// purpose: Verify Rovo Dev block insertion respects existing direct eventHooks.events.
    /// inputs: Existing Rovo Dev YAML with user hooks and one command requiring escaping.
    /// returns/effects: Asserts user hooks survive and command strings are YAML-escaped.
    #[test]
    fn rovodev_hook_install_merges_and_escapes_commands() {
        let existing = "\
eventHooks:\n  events:\n    - name: user_hook\n      commands:\n        - command: \"echo user\"\n";
        let installed = rovodev_installing(
            existing,
            &[(
                "on_complete",
                "limux hooks rovodev stop --message \"done\" \\ next\nline".to_string(),
            )],
        );

        assert!(installed.contains("    # cmux hooks rovodev begin"));
        assert!(installed.contains("    - name: user_hook"));
        assert!(installed.contains("        - command: \"echo user\""));
        assert!(installed.contains(
            "command: \"limux hooks rovodev stop --message \\\"done\\\" \\\\ next\\nline\""
        ));
        assert_eq!(rovodev_uninstalling(&installed), existing);
    }

    /// purpose: Verify Rovo Dev uninstall leaves dangling begin markers untouched.
    /// inputs: Malformed YAML block missing the CMUX end marker.
    /// returns/effects: Asserts no content is dropped without a complete marker pair.
    #[test]
    fn rovodev_hook_uninstall_preserves_dangling_marker() {
        let existing = "\
eventHooks:\n  events:\n    # cmux hooks rovodev begin\nsessions:\n  persistenceDir: /tmp/rovo\n";

        assert_eq!(rovodev_uninstalling(existing), existing);
    }

    /// purpose: Verify Claude setup writes blocking Feed hooks with Claude's matcher shape.
    /// inputs: Temporary settings JSON file and the Claude hook installer.
    /// returns/effects: Asserts installed Feed hook event names, matcher, timeout, and command markers.
    #[test]
    fn claude_hook_install_writes_feed_hooks() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("settings.json");

        install_json_hooks_with_feed(
            &path,
            agent_hooks::AgentKind::Claude,
            &[("SessionStart", "session-start")],
            claude_feed_hook_events(),
        )
        .expect("install hooks");

        let root: Value =
            serde_json::from_slice(&fs::read(&path).expect("read settings")).expect("json");
        let feed = &root["hooks"]["PermissionRequest"][0];

        assert_eq!(feed["matcher"], "*");
        assert_eq!(feed["hooks"][0]["timeout"], 120_000);
        assert!(feed["hooks"][0]["command"]
            .as_str()
            .expect("command")
            .contains("hooks feed --source claude --event 'PermissionRequest'"));
        assert_eq!(root["hooks"]["PreToolUse"][0]["matcher"], "*");
        assert!(root["hooks"]["PreToolUse"][0]["hooks"][0]["command"]
            .as_str()
            .expect("command")
            .contains("hooks feed --source claude --event 'PreToolUse'"));
        assert!(root["hooks"]["SessionStart"][0]["hooks"][0]["command"]
            .as_str()
            .expect("command")
            .contains("hooks claude session-start"));
    }

    /// purpose: Verify Gemini setup writes CMUX Feed PreToolUse hooks beside lifecycle hooks.
    /// inputs: Temporary settings JSON file and the Gemini hook installer.
    /// returns/effects: Asserts installed Feed hook shape, timeout, and command markers.
    #[test]
    fn gemini_hook_install_writes_feed_hooks() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("settings.json");

        install_json_hooks_with_feed(
            &path,
            agent_hooks::AgentKind::Gemini,
            &[("SessionStart", "session-start")],
            gemini_feed_hook_events(),
        )
        .expect("install hooks");

        let root: Value =
            serde_json::from_slice(&fs::read(&path).expect("read settings")).expect("json");
        let feed = &root["hooks"]["PreToolUse"][0];

        assert!(feed.get("matcher").is_none());
        assert_eq!(feed["hooks"][0]["timeout"], 120_000);
        assert!(feed["hooks"][0]["command"]
            .as_str()
            .expect("command")
            .contains("hooks feed --source gemini --event 'PreToolUse'"));
        assert!(root["hooks"]["SessionStart"][0]["hooks"][0]["command"]
            .as_str()
            .expect("command")
            .contains("hooks gemini session-start"));
    }

    /// purpose: Verify a nested JSON hook agent writes lifecycle and Feed entries.
    /// inputs: Agent kind, temporary hook path, and expected source marker.
    /// returns/effects: Asserts installed hook shape, timeouts, and command markers.
    fn assert_nested_pretool_hook_install(
        agent: agent_hooks::AgentKind,
        path: &Path,
        source: &str,
    ) {
        install_json_hooks_with_feed(
            path,
            agent,
            &[("SessionStart", "session-start"), ("Stop", "stop")],
            pre_tool_use_feed_hook_events(),
        )
        .expect("install hooks");

        let root: Value =
            serde_json::from_slice(&fs::read(path).expect("read hooks")).expect("json");
        let feed = &root["hooks"]["PreToolUse"][0];

        assert!(feed.get("matcher").is_none());
        assert_eq!(feed["hooks"][0]["timeout"], 120_000);
        assert!(feed["hooks"][0]["command"]
            .as_str()
            .expect("command")
            .contains(&format!(
                "hooks feed --source {source} --event 'PreToolUse'"
            )));
        assert_eq!(
            root["hooks"]["SessionStart"][0]["hooks"][0]["timeout"],
            5000
        );
        assert!(root["hooks"]["SessionStart"][0]["hooks"][0]["command"]
            .as_str()
            .expect("command")
            .contains(&format!("hooks {source} session-start")));
    }

    /// purpose: Verify Copilot, CodeBuddy, Factory, and Qoder nested JSON hook setup.
    /// inputs: Temporary hook JSON files and shared nested installer helper.
    /// returns/effects: Asserts Feed PreToolUse parity for four CMUX agent integrations.
    #[test]
    fn nested_json_agents_install_feed_hooks() {
        let dir = tempfile::tempdir().expect("tempdir");
        let cases = [
            (agent_hooks::AgentKind::Copilot, "copilot", "copilot.json"),
            (
                agent_hooks::AgentKind::CodeBuddy,
                "codebuddy",
                "codebuddy.json",
            ),
            (agent_hooks::AgentKind::Factory, "factory", "factory.json"),
            (agent_hooks::AgentKind::Qoder, "qoder", "qoder.json"),
        ];

        for (agent, source, file) in cases {
            assert_nested_pretool_hook_install(agent, &dir.path().join(file), source);
        }
    }

    /// purpose: Verify Codex uninstall removes both lifecycle hooks and Feed hooks.
    /// inputs: Temporary hook JSON file containing installed Codex hook entries.
    /// returns/effects: Asserts no Codex-owned hook entries remain after uninstall.
    #[test]
    fn codex_hook_uninstall_removes_lifecycle_and_feed_hooks() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("hooks.json");

        install_json_hooks_with_feed(
            &path,
            agent_hooks::AgentKind::Codex,
            &[("SessionStart", "session-start")],
            &["PermissionRequest"],
        )
        .expect("install hooks");
        uninstall_json_hooks(&path, agent_hooks::AgentKind::Codex).expect("uninstall hooks");

        let root: Value =
            serde_json::from_slice(&fs::read(&path).expect("read hooks")).expect("json");
        assert!(root["hooks"].as_object().expect("hooks").is_empty());
    }

    #[test]
    fn environ_parser_reads_requested_limux_value() {
        let environ = b"PATH=/bin\0LIMUX_WORKSPACE_ID=ws-1\0LIMUX_SURFACE_ID=7:tab-a\0";

        assert_eq!(
            env_value_from_environ(environ, "LIMUX_WORKSPACE_ID").as_deref(),
            Some("ws-1")
        );
        assert_eq!(
            env_value_from_environ(environ, "LIMUX_SURFACE_ID").as_deref(),
            Some("7:tab-a")
        );
        assert_eq!(env_value_from_environ(environ, "LIMUX_PANE_ID"), None);
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn proc_stat_parser_handles_process_names_with_spaces() {
        let stat = "1234 (claude hook sh) S 987 1 1 0 -1 4194560";

        assert_eq!(parse_proc_stat_parent_pid(stat), Some(987));
    }

    #[test]
    fn hook_session_id_falls_back_to_transcript_stem() {
        let payload = json!({
            "transcript_path": "/home/amwill/.claude/projects/-home-amwill-Applications-limux/268746f1-5a8f-471c-85db-dc50649c2f9c.jsonl"
        });

        assert_eq!(
            hook_session_id(&payload).as_deref(),
            Some("268746f1-5a8f-471c-85db-dc50649c2f9c")
        );
    }

    #[test]
    fn hook_session_id_prefers_explicit_session_id() {
        let payload = json!({
            "session_id": "explicit-session",
            "transcript_path": "/tmp/transcript-session.jsonl"
        });

        assert_eq!(
            hook_session_id(&payload).as_deref(),
            Some("explicit-session")
        );
    }

    #[test]
    fn hook_session_id_accepts_wrapper_env_session_id() {
        let _guard = EnvGuard::set("LIMUX_AGENT_SESSION_ID", "codex-wrapper-surface-123");

        assert_eq!(
            hook_session_id(&json!({})).as_deref(),
            Some("codex-wrapper-surface-123")
        );
    }

    #[test]
    fn hook_agent_pid_accepts_wrapper_env_pid() {
        let _guard = EnvGuard::set("LIMUX_AGENT_PID", "4242");

        assert_eq!(
            hook_agent_pid(&json!({}), agent_hooks::AgentKind::Codex),
            Some(4242)
        );
    }
}

#[cfg(test)]
mod agent_team_tests {
    use super::*;

    #[test]
    fn agent_launch_known() {
        for agent in [
            "codex",
            "claude",
            "claude-code",
            "opencode",
            "gemini",
            "gemini-cli",
        ] {
            assert!(
                agent_launch_command(agent).is_some(),
                "expected '{agent}' to be a known agent"
            );
        }
    }

    #[test]
    fn agent_launch_unknown_returns_none() {
        assert!(agent_launch_command("nonsense-cli").is_none());
    }

    #[test]
    fn agents_md_contains_protocol_and_peers() {
        let peers = vec![
            (
                "codex".to_string(),
                "10".to_string(),
                "10:tab-a".to_string(),
                "codex".to_string(),
            ),
            (
                "claude".to_string(),
                "11".to_string(),
                "11:tab-a".to_string(),
                "claude".to_string(),
            ),
        ];
        let md = build_agents_md(
            &peers,
            "/tmp/team",
            "active-ws",
            "ws-uuid-123",
            "9:terminal-orch",
        );

        // Header & generation marker
        assert!(md.contains("AGENTS.md — agent-to-agent message protocol"));
        assert!(md.contains("Generated by `limux agent-team`"));

        // Team workspace block
        assert!(md.contains("Workspace name: `active-ws`"));
        assert!(md.contains("Workspace ID: `ws-uuid-123`"));
        assert!(md.contains("Orchestrator surface: `9:terminal-orch`"));
        assert!(md.contains("Shared cwd: `/tmp/team`"));

        // Peer table rows (Agent | Pane | Surface | Launch)
        assert!(md.contains("| `codex` | `10` | `10:tab-a` | `codex` |"));
        assert!(md.contains("| `claude` | `11` | `11:tab-a` | `claude` |"));

        // Protocol envelope spec uses --surface, not --workspace
        assert!(md.contains("<agent-msg from=\"codex\" to=\"claude\""));
        assert!(md.contains("limux send --surface"));
        assert!(!md.contains("limux send --workspace"));
        assert!(md.contains("reply-to"));

        // Notify + env contract
        assert!(md.contains("limux notify"));
        assert!(md.contains("LIMUX_WORKSPACE_ID"));
        assert!(md.contains("LIMUX_SURFACE_ID"));
        assert!(md.contains("limux new-pane --direction right --command bash"));
        assert!(md.contains("CMUX-compatible context variables"));
    }
}

#[cfg(test)]
mod new_pane_tests {
    use super::*;

    fn args(values: &[&str]) -> Vec<String> {
        values.iter().map(|value| value.to_string()).collect()
    }

    fn test_env(name: &str) -> Option<String> {
        match name {
            "LIMUX_WORKSPACE_ID" => Some("workspace:agent".to_string()),
            "LIMUX_SURFACE_ID" => Some("surface:11:tab-a".to_string()),
            "LIMUX_PANE_ID" => Some("pane:11".to_string()),
            _ => None,
        }
    }

    fn cmux_only_env(name: &str) -> Option<String> {
        match name {
            "CMUX_WORKSPACE_ID" => Some("workspace:cmux".to_string()),
            "CMUX_SURFACE_ID" => Some("surface:12:tab-b".to_string()),
            _ => None,
        }
    }

    #[test]
    fn new_pane_serializes_env_defaults_and_command() {
        let (workspace, params) = build_new_pane_request(&args(&["--command", "claude"]), test_env);

        assert_eq!(workspace.as_deref(), Some("workspace:agent"));
        assert_eq!(
            params,
            json!({
                "direction": "right",
                "type": "terminal",
                "surface_id": "surface:11:tab-a",
                "pane_id": "pane:11",
                "command": "claude"
            })
        );
    }

    #[test]
    fn new_pane_accepts_cmux_context_env_aliases() {
        let (workspace, params) = build_new_pane_request(&args(&[]), cmux_only_env);

        assert_eq!(workspace.as_deref(), Some("workspace:cmux"));
        assert_eq!(params["surface_id"], "surface:12:tab-b");
        assert!(params.get("pane_id").is_none());
    }

    #[test]
    fn new_pane_flags_override_env_and_preserve_raw_refs() {
        let (workspace, params) = build_new_pane_request(
            &args(&[
                "--workspace",
                "raw-workspace",
                "--surface",
                "7:tab-b",
                "--pane",
                "7",
                "--direction",
                "down",
                "--type",
                "terminal",
                "--command",
                "codex --ask-for-approval never",
            ]),
            test_env,
        );

        assert_eq!(workspace.as_deref(), Some("raw-workspace"));
        assert_eq!(
            params,
            json!({
                "direction": "down",
                "type": "terminal",
                "surface_id": "7:tab-b",
                "pane_id": "7",
                "command": "codex --ask-for-approval never"
            })
        );
    }

    #[test]
    fn new_pane_without_env_preserves_active_workspace_fallback() {
        let (workspace, params) = build_new_pane_request(&args(&[]), |_| None);

        assert_eq!(workspace, None);
        assert_eq!(
            params,
            json!({
                "direction": "right",
                "type": "terminal"
            })
        );
    }
}
