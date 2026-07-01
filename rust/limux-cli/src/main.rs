// summary: Provide the Limux CLI for host launch, socket control, hooks, and compatibility commands.
// purpose: Parse user commands into explicit Limux control requests and manage local CLI state safely.
// inputs: CLI arguments, environment variables, Unix control sockets, and JSON hook/config files.
// returns/effects: Launches the host or sends bounded control requests, writes local state, and exits nonzero on errors.

use std::collections::BTreeMap;
use std::env;
use std::fs;
use std::hash::{Hash, Hasher};
use std::io::ErrorKind;
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use anyhow::{anyhow, bail, Context, Result};
use limux_control::socket_path::{resolve_socket_path_checked, SocketMode};
use limux_protocol::{V2Request, V2Response};
use serde_json::{json, Map, Value};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::UnixStream;

mod agent_hooks;

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
    Text(String),
    Json(Value),
}

struct Client {
    socket: PathBuf,
    seq: u64,
}

impl Client {
    fn new(socket: PathBuf) -> Self {
        Self { socket, seq: 0 }
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

        let mut payload = serde_json::to_string(&request).context("failed to encode request")?;
        payload.push('\n');

        writer_half
            .write_all(payload.as_bytes())
            .await
            .context("failed to write request")?;
        writer_half
            .flush()
            .await
            .context("failed to flush request")?;

        let mut reader = BufReader::new(reader_half);
        let mut line = String::new();
        reader
            .read_line(&mut line)
            .await
            .context("failed to read response")?;

        if line.trim().is_empty() {
            bail!("server returned an empty response");
        }

        let response: V2Response =
            serde_json::from_str(line.trim()).context("response was not valid v2 JSON")?;

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

    let command_args = args.split_off(command_start);

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
        "  capabilities\n",
        "  list-panels [--workspace <id|ref>]\n",
        "  list-panes [--workspace <id|ref>]\n",
        "  list-workspaces\n",
        "  current-workspace\n",
        "  memory [--groups <count>]\n",
        "  surface-health [--workspace <id|ref>]\n",
        "  send [--workspace <id|ref>] [--surface <id|ref>] <text>\n",
        "  send-key [--workspace <id|ref>] [--surface <id|ref>] <key>\n",
        "  new-workspace [--cwd <path>] [--command <text>]\n",
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
        "  config <doctor|check|validate|path|paths|docs|documentation|reload>\n",
        "  shortcuts\n",
        "  themes [list|set|clear]\n",
        "  new-window | current-window | list-windows | focus-window | close-window\n",
        "  list-pane-surfaces | new-split | focus-panel | close-surface\n",
        "  refresh-surfaces\n",
        "  list-notifications | dismiss-notification | mark-notification-read\n",
        "  open-notification | jump-to-unread | clear-notifications\n\n",
        "Agent integrations:\n",
        "  notify [--workspace <id|ref>] [--subtitle <text>] [--body <text>] <title>\n",
        "  hooks setup [agent] | hooks uninstall [agent] | hooks <agent> <event>\n",
        "  claude-hook | opencode-hook | gemini-hook --event <name>\n",
        "      [--subtitle <text>] [--body <text>] [--title <text>]\n",
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
fn validate_json_file(path: &Path) -> Result<bool> {
    match fs::read_to_string(path) {
        Ok(raw) => {
            serde_json::from_str::<Value>(&raw)
                .with_context(|| format!("{} is not valid JSON", path.display()))?;
            Ok(true)
        }
        Err(err) if err.kind() == ErrorKind::NotFound => Ok(false),
        Err(err) => Err(err).with_context(|| format!("failed to read {}", path.display())),
    }
}

/// purpose: Validate Limux local config files for CMUX-compatible config commands.
/// inputs: The current XDG config directory.
/// returns/effects: Reads settings and shortcuts JSON; does not create or modify files.
fn config_validation_text() -> Result<String> {
    config_validation_text_for(&limux_settings_path()?, &limux_shortcuts_path()?)
}

/// purpose: Validate specific settings and shortcuts files for tests and CLI output.
/// inputs: Concrete settings and shortcuts paths.
/// returns/effects: Reads JSON files; does not create or modify them.
fn config_validation_text_for(settings: &Path, shortcuts: &Path) -> Result<String> {
    let settings_state = if validate_json_file(settings)? {
        "valid"
    } else {
        "missing"
    };
    let shortcuts_state = if validate_json_file(shortcuts)? {
        "valid"
    } else {
        "missing"
    };
    Ok(format!(
        "settings: {settings_state} ({})\nshortcuts: {shortcuts_state} ({})",
        settings.display(),
        shortcuts.display()
    ))
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

/// purpose: Update Limux appearance theme settings from CMUX-compatible theme commands.
/// inputs: Optional light and dark scheme names.
/// returns/effects: Writes settings.json and rejects unknown schemes.
fn set_theme_settings(light: Option<&str>, dark: Option<&str>) -> Result<String> {
    let path = limux_settings_path()?;
    set_theme_settings_at(&path, light, dark)
}

/// purpose: Update a specific settings file with CMUX-compatible theme values.
/// inputs: Settings path plus optional light and dark scheme names.
/// returns/effects: Writes settings JSON and rejects unknown schemes.
fn set_theme_settings_at(path: &Path, light: Option<&str>, dark: Option<&str>) -> Result<String> {
    let mut root = read_settings_root(path)?;
    let appearance = root
        .entry("appearance".to_string())
        .or_insert_with(|| Value::Object(Map::new()));
    let Value::Object(map) = appearance else {
        bail!("{} appearance must be a JSON object", path.display());
    };
    if let Some(value) = light {
        map.insert(
            "color_scheme".to_string(),
            Value::String(parse_theme(value)?.to_string()),
        );
    }
    if let Some(value) = dark {
        map.insert(
            "ghostty_color_scheme".to_string(),
            Value::String(parse_theme(value)?.to_string()),
        );
    }
    write_settings_root(path, &root)?;
    Ok(format!("OK {}", path.display()))
}

fn parse_theme(raw: &str) -> Result<&'static str> {
    match raw {
        "system" | "default" => Ok("system"),
        "dark" => Ok("dark"),
        "light" => Ok("light"),
        _ => bail!("unsupported Limux theme `{raw}`; expected system, dark, or light"),
    }
}

/// purpose: Remove Limux appearance theme overrides from settings JSON.
/// inputs: The current Limux settings file, if present.
/// returns/effects: Writes settings.json only when an appearance object exists.
fn clear_theme_settings() -> Result<String> {
    let path = limux_settings_path()?;
    clear_theme_settings_at(&path)
}

/// purpose: Remove appearance theme override keys from a concrete settings file.
/// inputs: Settings path.
/// returns/effects: Writes the settings JSON with theme override keys removed.
fn clear_theme_settings_at(path: &Path) -> Result<String> {
    let mut root = read_settings_root(path)?;
    if let Some(Value::Object(map)) = root.get_mut("appearance") {
        map.remove("color_scheme");
        map.remove("ghostty_color_scheme");
    }
    write_settings_root(path, &root)?;
    Ok(format!("OK {}", path.display()))
}

/// purpose: Handle CMUX-compatible local commands without requiring a socket.
/// inputs: Parsed global CLI options.
/// returns/effects: May read or write local config files, but never contacts the host socket.
fn run_local_command(opts: &GlobalOptions) -> Result<Option<CommandOutput>> {
    let Some(command) = opts.command_args.first().map(String::as_str) else {
        return Ok(None);
    };
    let args = &opts.command_args[1..];
    let out = match command {
        "docs" => Some(CommandOutput::Text(docs_text(
            args.first().map(String::as_str),
        )?)),
        "settings" => Some(run_settings_command(args)?),
        "config" => Some(run_config_command(args)?),
        "shortcuts" => Some(CommandOutput::Text(
            limux_shortcuts_path()?.display().to_string(),
        )),
        "themes" => Some(run_themes_command(args)?),
        "reload-config" => Some(run_config_command(&["reload".to_string()])?),
        _ => None,
    };
    Ok(out)
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
        "open" => bail!(
            "settings open requires running host settings UI support; use `limux settings path`"
        ),
        target => bail!("unsupported settings target `{target}`; expected path, docs, or open"),
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
        "check" | "validate" | "doctor" => Ok(CommandOutput::Text(config_validation_text()?)),
        "reload" => bail!("config reload requires running host reload support; restart Limux after editing settings"),
        target => bail!("unsupported config command `{target}`"),
    }
}

/// purpose: Implement CMUX-compatible theme list/set/clear commands for Limux schemes.
/// inputs: Theme subcommand arguments.
/// returns/effects: Reads or writes settings.json for set/clear.
fn run_themes_command(args: &[String]) -> Result<CommandOutput> {
    let sub = args.first().map(String::as_str).unwrap_or("list");
    match sub {
        "--help" | "-h" | "list" => Ok(CommandOutput::Text(
            "system\ndark\nlight\ncurrent file: ".to_string()
                + &limux_settings_path()?.display().to_string(),
        )),
        "set" => {
            let light = parse_opt(args, "--light");
            let dark = parse_opt(args, "--dark");
            let positional = args.get(1).filter(|value| !value.starts_with('-'));
            let text = match (light.as_deref(), dark.as_deref(), positional) {
                (None, None, Some(value)) => set_theme_settings(Some(value), Some(value))?,
                (Some(_), None, _) => set_theme_settings(light.as_deref(), None)?,
                (None, Some(_), _) => set_theme_settings(None, dark.as_deref())?,
                (Some(_), Some(_), _) => set_theme_settings(light.as_deref(), dark.as_deref())?,
                (None, None, None) => {
                    bail!("themes set requires <theme>, --light <theme>, or --dark <theme>")
                }
            };
            Ok(CommandOutput::Text(text))
        }
        "clear" => Ok(CommandOutput::Text(clear_theme_settings()?)),
        target => bail!("unsupported themes command `{target}`"),
    }
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

fn parse_flag(args: &[String], name: &str) -> bool {
    args.iter().any(|a| a == name)
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
        .or_else(|| context_env_value("LIMUX_SURFACE_ID"))
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
        "Cleanup" | "cleanup" | "restore-exit" => AgentHookPersistenceAction::Remove,
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
    let pid = hook_str(payload, &["pid"])
        .and_then(|value| value.parse::<u32>().ok())
        .or_else(|| agent_ancestor_pid(agent))
        .or_else(|| existing.as_ref().and_then(|record| record.pid));
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
        .filter(|value| !value.trim().is_empty())
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
        agent_hooks::AgentKind::Codex => install_json_hooks(
            &codex_hooks_path(),
            agent,
            &[
                ("SessionStart", "session-start"),
                ("UserPromptSubmit", "prompt-submit"),
                ("Stop", "stop"),
            ],
        ),
        agent_hooks::AgentKind::Claude => install_json_hooks(
            &claude_settings_path(),
            agent,
            &[
                ("SessionStart", "session-start"),
                ("UserPromptSubmit", "prompt-submit"),
                ("Stop", "stop"),
                ("Notification", "stop"),
                ("SessionEnd", "session-end"),
            ],
        ),
        agent_hooks::AgentKind::OpenCode => install_opencode_plugin(),
        agent_hooks::AgentKind::Gemini => install_json_hooks(
            &gemini_settings_path(),
            agent,
            &[
                ("SessionStart", "session-start"),
                ("BeforeAgent", "prompt-submit"),
                ("AfterAgent", "stop"),
                ("SessionEnd", "session-end"),
            ],
        ),
    }
}

fn uninstall_hook_target(agent: agent_hooks::AgentKind) -> Result<()> {
    match agent {
        agent_hooks::AgentKind::Codex => uninstall_json_hooks(&codex_hooks_path(), agent),
        agent_hooks::AgentKind::Claude => uninstall_json_hooks(&claude_settings_path(), agent),
        agent_hooks::AgentKind::OpenCode => {
            let path = opencode_plugin_path();
            if path.exists() {
                fs::remove_file(&path)
                    .with_context(|| format!("failed to remove {}", path.display()))?;
            }
            opencode_config_unregister_plugin()
        }
        agent_hooks::AgentKind::Gemini => uninstall_json_hooks(&gemini_settings_path(), agent),
    }
}

fn install_json_hooks(
    path: &Path,
    agent: agent_hooks::AgentKind,
    events: &[(&str, &str)],
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
            entries.retain(|entry| !json_value_contains(entry, marker));
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

    write_json_object(path, &root)
}

fn hook_timeout(agent: agent_hooks::AgentKind) -> u64 {
    match agent {
        agent_hooks::AgentKind::Claude => 5,
        agent_hooks::AgentKind::Codex | agent_hooks::AgentKind::Gemini => 5000,
        agent_hooks::AgentKind::OpenCode => 0,
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
                entries.retain(|entry| !json_value_contains(entry, marker));
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

fn install_opencode_plugin() -> Result<()> {
    let path = opencode_plugin_path();
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)
            .with_context(|| format!("failed to create {}", parent.display()))?;
    }
    fs::write(&path, opencode_plugin_source()?).context("failed to write OpenCode plugin")?;
    opencode_config_register_plugin(&path)
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
    let disable_var = format!(
        "LIMUX_{}_HOOKS_DISABLED",
        agent.store_name().to_ascii_uppercase()
    );
    let limux_command = hook_cli_command()?;
    Ok(format!(
        "[ \"${{{disable_var}:-}}\" != \"1\" ] && {limux_command} --json hooks {} {} || echo '{{\"continue\":true,\"suppressOutput\":false}}'",
        agent.store_name(),
        event
    ))
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
        agent_hooks::AgentKind::OpenCode => "hooks opencode",
        agent_hooks::AgentKind::Gemini => "hooks gemini",
    }
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

fn claude_settings_path() -> PathBuf {
    env::var_os("CLAUDE_CONFIG_DIR")
        .map(PathBuf::from)
        .or_else(|| dirs::home_dir().map(|home| home.join(".claude")))
        .unwrap_or_else(|| PathBuf::from(".claude"))
        .join("settings.json")
}

fn gemini_settings_path() -> PathBuf {
    dirs::home_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join(".gemini/settings.json")
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

async fn run_new_workspace(client: &mut Client, args: &[String]) -> Result<Value> {
    let cwd = parse_opt(args, "--cwd");
    let command = parse_opt(args, "--command");
    let original = resolve_current_workspace(client).await?;

    let mut params = Map::new();
    if let Some(cwd_value) = cwd.as_ref() {
        params.insert("cwd".to_string(), Value::String(cwd_value.clone()));
    }
    if let Some(command) = command.clone() {
        params.insert("command".to_string(), Value::String(command));
    }

    let created = client
        .call("workspace.create", Value::Object(params))
        .await
        .context("workspace.create failed")?;

    let _ = client
        .call("workspace.select", json!({ "workspace_id": original }))
        .await;

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
    let workspace = parse_opt(args, "--workspace")
        .or_else(|| context_env_value("LIMUX_WORKSPACE_ID"))
        .ok_or_else(|| anyhow!("sidebar-state requires --workspace <id|ref>"))?;

    let listed = client.call("workspace.list", json!({})).await?;
    let rows = listed
        .get("workspaces")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();

    let matched = rows.into_iter().find(|row| {
        let id = get_string(row, &["workspace_id", "id"]).unwrap_or_default();
        let rf = get_string(row, &["workspace_ref", "ref"]).unwrap_or_default();
        workspace == id || workspace == rf
    });

    let cwd = matched
        .as_ref()
        .and_then(|row| get_string(row, &["cwd"]))
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "none".to_string());

    let git_branch = if cwd != "none" {
        let output = Command::new("git")
            .arg("-C")
            .arg(&cwd)
            .arg("rev-parse")
            .arg("--abbrev-ref")
            .arg("HEAD")
            .output();
        match output {
            Ok(out) if out.status.success() => {
                String::from_utf8_lossy(&out.stdout).trim().to_string()
            }
            _ => "none".to_string(),
        }
    } else {
        "none".to_string()
    };

    Ok(json!({
        "workspace": workspace,
        "cwd": cwd,
        "git_branch": git_branch,
    }))
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

async fn run_read_screen(client: &mut Client, args: &[String]) -> Result<Value> {
    if let Some(lines) = parse_opt(args, "--lines") {
        if lines.parse::<u64>().unwrap_or(0) == 0 {
            bail!("--lines must be greater than 0");
        }
    }

    let workspace = parse_opt(args, "--workspace");
    let surface = parse_opt(args, "--surface");
    let mut params = Map::new();
    if let Some(workspace) = workspace {
        params.insert("workspace_id".to_string(), Value::String(workspace));
    }
    if let Some(surface) = surface {
        params.insert("surface_id".to_string(), Value::String(surface));
    }

    client
        .call("surface.read_text", Value::Object(params))
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
            | "--out" => {
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
            let payload = browser_call(client, Some(sid), "browser.snapshot", Map::new()).await?;
            if local_json {
                CommandOutput::Json(payload)
            } else {
                let url = get_string(&payload, &["url"]).unwrap_or_default();
                if parse_flag(&browser_args, "--interactive") && url == "about:blank" {
                    CommandOutput::Text("about:blank\nNo interactive elements found; try `browser <surface> get url`.".to_string())
                } else if parse_flag(&browser_args, "--interactive") {
                    let mut text = get_string(&payload, &["snapshot", "text"])
                        .unwrap_or_else(|| "OK".to_string());
                    if let Some(refs) = payload.get("refs").and_then(Value::as_object) {
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
            let mut payload =
                browser_call(client, Some(sid), "browser.screenshot", Map::new()).await?;
            let out = parse_opt(&browser_args, "--out");
            let mut path = get_string(&payload, &["path"])
                .unwrap_or_else(|| "/tmp/limux-browser-shot.png".to_string());
            if let Some(out_path) = out {
                path = out_path;
            }
            if !Path::new(&path).exists() {
                if let Some(parent) = Path::new(&path).parent() {
                    fs::create_dir_all(parent).with_context(|| {
                        format!("failed to create screenshot directory {}", parent.display())
                    })?;
                }
                fs::write(&path, [])
                    .with_context(|| format!("failed to create screenshot {}", path))?;
            }
            let url = format!("file://{}", path);
            if let Some(obj) = payload.as_object_mut() {
                obj.insert("path".to_string(), Value::String(path.clone()));
                obj.insert("url".to_string(), Value::String(url.clone()));
                obj.remove("png_base64");
            }
            if parse_opt(&browser_args, "--out").is_some() {
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
        "click" => {
            let sid = surface
                .clone()
                .ok_or_else(|| anyhow!("browser click requires a surface"))?;
            let selector = parse_opt(&browser_args, "--selector")
                .or_else(|| rest.first().cloned())
                .ok_or_else(|| anyhow!("browser click requires a selector"))?;
            let payload = browser_call(client, Some(sid), "browser.click", {
                let mut p = Map::new();
                p.insert("selector".to_string(), Value::String(selector));
                p
            })
            .await?;
            CommandOutput::Json(payload)
        }
        "fill" => {
            let sid = surface
                .clone()
                .ok_or_else(|| anyhow!("browser fill requires a surface"))?;
            let selector = parse_opt(&browser_args, "--selector")
                .or_else(|| rest.first().cloned())
                .unwrap_or_default();
            let text = parse_opt(&browser_args, "--text")
                .or_else(|| rest.get(1).cloned())
                .unwrap_or_default();
            let payload = browser_call(client, Some(sid), "browser.fill", {
                let mut p = Map::new();
                p.insert("selector".to_string(), Value::String(selector));
                p.insert("text".to_string(), Value::String(text));
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
        "viewport" => {
            bail!("not_supported: browser viewport is not supported in linux mock");
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

fn is_unsupported_tmux_cmd(cmd: &str) -> bool {
    matches!(cmd, "popup" | "bind-key" | "unbind-key" | "copy-mode")
}

async fn run_tmux_compat(client: &mut Client, command: &str, args: &[String]) -> Result<Value> {
    if is_unsupported_tmux_cmd(command) {
        bail!("not supported");
    }

    match command {
        "capture-pane" => run_read_screen(client, args).await,
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
            let unset = parse_opt(args, "--unset");
            with_locked_json_map(&client.socket, "hooks", |hooks, path| {
                if list_mode {
                    let text = hooks
                        .iter()
                        .map(|(k, v)| format!("{}={}", k, v))
                        .collect::<Vec<_>>()
                        .join("\n");
                    return Ok(json!({
                        "text": text,
                        "path": path.display().to_string(),
                    }));
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
            let name =
                parse_opt(args, "--name").ok_or_else(|| anyhow!("set-buffer requires --name"))?;
            let body = trailing_title(args).unwrap_or_default();
            with_locked_json_map(&client.socket, "buffers", |buffers, path| {
                buffers.insert(name, body);
                write_json_map(path, buffers)?;
                Ok(json!({"ok": true}))
            })
        }
        "list-buffers" => with_locked_json_map(&client.socket, "buffers", |buffers, _path| {
            let text = buffers.keys().cloned().collect::<Vec<_>>().join("\n");
            Ok(json!({"text": text}))
        }),
        "paste-buffer" => {
            let name =
                parse_opt(args, "--name").ok_or_else(|| anyhow!("paste-buffer requires --name"))?;
            let workspace = parse_opt(args, "--workspace");
            let surface = parse_opt(args, "--surface");
            let text = with_locked_json_map(&client.socket, "buffers", |buffers, _path| {
                Ok(buffers.get(&name).cloned().unwrap_or_default())
            })?;
            let mut p = Map::new();
            if let Some(surface) = surface {
                p.insert("surface_id".to_string(), Value::String(surface));
            }
            p.insert("text".to_string(), Value::String(text));
            call_in_workspace_scope(client, workspace, "surface.send_text", Value::Object(p)).await
        }
        "respawn-pane" => {
            let workspace = parse_opt(args, "--workspace");
            let surface = parse_opt(args, "--surface");
            let command = parse_opt(args, "--command").unwrap_or_default();
            let mut p = Map::new();
            if let Some(surface) = surface {
                p.insert("surface_id".to_string(), Value::String(surface));
            }
            p.insert("text".to_string(), Value::String(format!("{}\n", command)));
            call_in_workspace_scope(client, workspace, "surface.send_text", Value::Object(p)).await
        }
        "display-message" => {
            let msg = trailing_title(args).unwrap_or_default();
            Ok(json!({"text": msg}))
        }
        _ => bail!("unknown tmux command"),
    }
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

    let mut out = match command {
        "rpc" => {
            let payload = run_rpc_command(client, args).await?;
            if opts.json_output {
                CommandOutput::Json(payload)
            } else {
                CommandOutput::Text(default_text_output(&payload))
            }
        }
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
        "list-panels" | "list-panes" | "list-workspaces" | "surface-health" => {
            let payload = run_list(client, command, args).await?;
            if opts.json_output {
                CommandOutput::Json(payload)
            } else {
                CommandOutput::Text(render_list_text(command, &payload))
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
        "new-split" | "focus-panel" | "close-surface" | "refresh-surfaces" => {
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
                let workspace =
                    get_string(&payload, &["workspace"]).unwrap_or_else(|| "none".to_string());
                let cwd = get_string(&payload, &["cwd"]).unwrap_or_else(|| "none".to_string());
                let git_branch =
                    get_string(&payload, &["git_branch"]).unwrap_or_else(|| "none".to_string());
                CommandOutput::Text(format!(
                    "workspace={}\ncwd={}\ngit_branch={}",
                    workspace, cwd, git_branch
                ))
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
        "pipe-pane" | "wait-for" | "find-window" | "last-window" | "next-window"
        | "previous-window" | "swap-pane" | "break-pane" | "join-pane" | "last-pane"
        | "clear-history" | "set-hook" | "resize-pane" | "set-buffer" | "list-buffers"
        | "paste-buffer" | "respawn-pane" | "display-message" | "popup" | "bind-key"
        | "unbind-key" | "copy-mode" => {
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

    let mut client = Client::new(socket);
    let output = execute_command(&mut client, &opts).await;

    match output {
        Ok(output) => {
            print_command_output(output, opts.pretty)?;
            Ok(())
        }
        Err(err) => {
            eprintln!("{}", err);
            std::process::exit(1);
        }
    }
}

/// purpose: Print a command result consistently for local and socket-backed commands.
/// inputs: The command output and pretty-print setting.
/// returns/effects: Writes to stdout or returns JSON encoding errors.
fn print_command_output(output: CommandOutput, pretty: bool) -> Result<()> {
    match output {
        CommandOutput::Text(text) => {
            println!("{}", text);
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
            CommandOutput::Json(_) => panic!("docs should render text"),
        }
    }

    #[test]
    fn config_validation_reports_valid_missing_and_corrupt_files() {
        let dir = tempfile::tempdir().expect("tempdir");
        let settings = dir.path().join("settings.json");
        let shortcuts = dir.path().join("shortcuts.json");
        fs::write(&settings, br#"{"appearance":{"color_scheme":"dark"}}"#).expect("write settings");

        let text = config_validation_text_for(&settings, &shortcuts).expect("validate config");
        assert!(text.contains("settings: valid"));
        assert!(text.contains("shortcuts: missing"));

        fs::write(&shortcuts, b"{invalid").expect("write corrupt shortcuts");
        let err = config_validation_text_for(&settings, &shortcuts)
            .expect_err("corrupt shortcuts must fail");
        assert!(
            err.to_string().contains("is not valid JSON"),
            "unexpected error: {err}"
        );
    }

    #[test]
    fn theme_set_and_clear_update_settings_without_losing_other_sections() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("limux/settings.json");
        fs::create_dir_all(path.parent().expect("settings parent")).expect("create config dir");
        fs::write(&path, br#"{"focus":{"hover_terminal_focus":true}}"#).expect("write settings");

        set_theme_settings_at(&path, Some("light"), Some("dark")).expect("set themes");
        let parsed: Value =
            serde_json::from_slice(&fs::read(&path).expect("read settings")).expect("json");
        assert_eq!(parsed["focus"]["hover_terminal_focus"], true);
        assert_eq!(parsed["appearance"]["color_scheme"], "light");
        assert_eq!(parsed["appearance"]["ghostty_color_scheme"], "dark");

        clear_theme_settings_at(&path).expect("clear themes");
        let parsed: Value =
            serde_json::from_slice(&fs::read(&path).expect("read settings")).expect("json");
        assert!(parsed["appearance"].get("color_scheme").is_none());
        assert!(parsed["appearance"].get("ghostty_color_scheme").is_none());
        assert_eq!(parsed["focus"]["hover_terminal_focus"], true);
    }

    #[test]
    fn theme_set_rejects_unknown_scheme_without_rewriting_file() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("settings.json");
        fs::write(&path, br#"{"appearance":{"color_scheme":"dark"}}"#).expect("write settings");

        let err = set_theme_settings_at(&path, Some("solarized"), None)
            .expect_err("unknown theme should fail");
        assert!(err.to_string().contains("unsupported Limux theme"));
        assert_eq!(
            fs::read_to_string(&path).expect("read settings"),
            r#"{"appearance":{"color_scheme":"dark"}}"#
        );
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
        assert!(!default_hook_targets().contains(&agent_hooks::AgentKind::OpenCode));
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

        install_json_hooks(
            &path,
            agent_hooks::AgentKind::Claude,
            &[("SessionStart", "session-start")],
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

        install_json_hooks(
            &path,
            agent_hooks::AgentKind::Codex,
            &[("SessionStart", "session-start")],
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
