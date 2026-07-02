// summary: Discover workspace-owned listening ports from Linux procfs.
// purpose: Back CMUX-compatible sidebar port metadata without spawning netstat/lsof watchers.
// inputs: /proc process tree, /proc/<pid>/environ, /proc/<pid>/fd, and /proc/net/tcp*.
// returns/effects: Returns JSON port rows for Limux-attributed child processes or explicit errors.

use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::fs;
use std::io::ErrorKind;
use std::path::Path;

use serde_json::{json, Value};

use crate::port_discovery_procfs::{
    parse_listening_socket_line, process_socket_inodes, process_workspace_id, read_command,
    read_process_parent_if_alive,
};

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct ListeningSocket {
    pub(crate) protocol: String,
    pub(crate) address: String,
    pub(crate) port: u16,
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct WorkspaceProcess {
    pid: u32,
    command: String,
}

/// purpose: Discover CMUX-shaped local port rows for one Limux workspace.
/// inputs: Raw workspace id/ref and maximum row count.
/// returns/effects: Scans procfs once, sorted by port, without spawning helpers or writing files.
pub fn workspace_port_rows(
    workspace_id: &str,
    max_rows: usize,
    open_in_cmux_browser: bool,
) -> Result<Vec<Value>, String> {
    let expected_workspace = normalize_workspace_id(workspace_id);
    let sockets = listening_sockets()?;
    if sockets.is_empty() {
        return Ok(Vec::new());
    }

    let processes = workspace_descendant_processes(std::process::id(), expected_workspace)?;
    let mut rows = Vec::new();
    let mut seen = BTreeSet::new();
    for process in processes {
        for inode in process_socket_inodes(process.pid)? {
            if let Some(socket) = sockets.get(&inode) {
                if seen.insert((socket.protocol.clone(), socket.address.clone(), socket.port)) {
                    rows.push(port_row(socket, &process, open_in_cmux_browser));
                }
            }
        }
    }
    rows.sort_by_key(port_row_sort_key);
    rows.truncate(max_rows);
    Ok(rows)
}

/// purpose: Read listening TCP socket metadata keyed by kernel socket inode.
/// inputs: Linux procfs TCP tables.
/// returns/effects: Returns only LISTEN rows from IPv4 and IPv6 tables.
fn listening_sockets() -> Result<BTreeMap<u64, ListeningSocket>, String> {
    let mut sockets = BTreeMap::new();
    read_listening_socket_table(Path::new("/proc/net/tcp"), "tcp", &mut sockets)?;
    read_listening_socket_table(Path::new("/proc/net/tcp6"), "tcp6", &mut sockets)?;
    Ok(sockets)
}

/// purpose: Add listening socket rows from one procfs network table.
/// inputs: Table path, public protocol label, and socket map to extend.
/// returns/effects: Mutates the socket map or returns explicit read/parse errors.
fn read_listening_socket_table(
    path: &Path,
    protocol: &str,
    sockets: &mut BTreeMap<u64, ListeningSocket>,
) -> Result<(), String> {
    let raw = fs::read_to_string(path).map_err(|error| {
        if error.kind() == ErrorKind::NotFound {
            format!("{} not found", path.display())
        } else {
            format!("failed to read {}: {error}", path.display())
        }
    })?;
    for line in raw.lines().skip(1).filter(|line| !line.trim().is_empty()) {
        if let Some((inode, socket)) = parse_listening_socket_line(line, protocol)? {
            sockets.insert(inode, socket);
        }
    }
    Ok(())
}

/// purpose: Find descendant processes attributed to one Limux workspace.
/// inputs: Host root pid and workspace id.
/// returns/effects: Traverses current descendants once and reads their environments.
fn workspace_descendant_processes(
    root_pid: u32,
    workspace_id: &str,
) -> Result<Vec<WorkspaceProcess>, String> {
    let child_map = process_child_map()?;
    let mut queue = VecDeque::new();
    let mut processes = Vec::new();
    for child in child_map.get(&root_pid).into_iter().flatten() {
        queue.push_back(*child);
    }
    while let Some(pid) = queue.pop_front() {
        if process_workspace_id(pid)?.as_deref() == Some(workspace_id) {
            processes.push(WorkspaceProcess {
                pid,
                command: read_command(pid),
            });
        }
        for child in child_map.get(&pid).into_iter().flatten() {
            queue.push_back(*child);
        }
    }
    Ok(processes)
}

/// purpose: Build a parent-to-children process map from procfs.
/// inputs: Current /proc entries.
/// returns/effects: Returns a map while tolerating processes that exit mid-scan.
fn process_child_map() -> Result<BTreeMap<u32, Vec<u32>>, String> {
    let mut map: BTreeMap<u32, Vec<u32>> = BTreeMap::new();
    let entries =
        fs::read_dir("/proc").map_err(|error| format!("failed to read /proc: {error}"))?;
    for entry in entries {
        let entry = entry.map_err(|error| format!("failed to read /proc entry: {error}"))?;
        let Some(pid) = entry.file_name().to_string_lossy().parse::<u32>().ok() else {
            continue;
        };
        if let Some(ppid) = read_process_parent_if_alive(pid)? {
            map.entry(ppid).or_default().push(pid);
        }
    }
    Ok(map)
}

/// purpose: Build one CMUX-compatible sidebar port row.
/// inputs: Socket metadata, owning workspace process, and link open policy.
/// returns/effects: Returns JSON without mutating state.
fn port_row(
    socket: &ListeningSocket,
    process: &WorkspaceProcess,
    open_in_cmux_browser: bool,
) -> Value {
    let url_host = if socket.address == "0.0.0.0" || socket.address == "::" {
        "127.0.0.1"
    } else {
        socket.address.as_str()
    };
    json!({
        "port": socket.port,
        "protocol": socket.protocol,
        "address": socket.address,
        "url": format!("http://{url_host}:{}", socket.port),
        "pid": process.pid,
        "command": process.command,
        "open_in_cmux_browser": open_in_cmux_browser,
        "openInCmuxBrowser": open_in_cmux_browser,
    })
}

/// purpose: Sort sidebar port rows by port then protocol/address for stable output.
/// inputs: Port row JSON.
/// returns/effects: Returns a tuple used only for ordering.
fn port_row_sort_key(row: &Value) -> (u64, String, String) {
    (
        row.get("port").and_then(Value::as_u64).unwrap_or_default(),
        row.get("protocol")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string(),
        row.get("address")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string(),
    )
}

/// purpose: Normalize CMUX workspace refs to Limux environment ids.
/// inputs: Raw workspace id or workspace:<id> ref.
/// returns/effects: Returns the id value used in LIMUX_WORKSPACE_ID.
fn normalize_workspace_id(raw: &str) -> &str {
    raw.strip_prefix("workspace:").unwrap_or(raw)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{BufRead, BufReader};
    use std::process::{Command, Stdio};
    use std::thread;
    use std::time::{Duration, Instant};

    // purpose: Verify generated port rows match CMUX-style sidebar metadata.
    // inputs: Synthetic socket and owning process.
    // returns/effects: Asserts stable fields and localhost URL normalization.
    #[test]
    fn port_row_includes_process_and_local_url() {
        let socket = ListeningSocket {
            protocol: "tcp".to_string(),
            address: "0.0.0.0".to_string(),
            port: 3000,
        };
        let process = WorkspaceProcess {
            pid: 22,
            command: "python -m http.server".to_string(),
        };
        let row = port_row(&socket, &process, true);
        assert_eq!(row["port"], json!(3000));
        assert_eq!(row["url"], json!("http://127.0.0.1:3000"));
        assert_eq!(row["pid"], json!(22));
        assert_eq!(row["command"], json!("python -m http.server"));
        assert_eq!(row["openInCmuxBrowser"], json!(true));
        assert_eq!(row["open_in_cmux_browser"], json!(true));
    }

    // purpose: Verify real workspace-attributed child listeners are discovered from procfs.
    // inputs: Python child process with LIMUX_WORKSPACE_ID and one listening TCP socket.
    // returns/effects: Starts and kills a child process, asserting its port appears in rows.
    #[test]
    fn workspace_port_rows_discovers_child_listener() {
        let workspace_id = format!("port-test-{}", std::process::id());
        let mut child = Command::new("python3")
            .arg("-c")
            .arg(
                "import socket, time\n\
                 s = socket.socket()\n\
                 s.bind(('127.0.0.1', 0))\n\
                 s.listen()\n\
                 print(s.getsockname()[1], flush=True)\n\
                 time.sleep(10)\n",
            )
            .env("LIMUX_WORKSPACE_ID", &workspace_id)
            .stdout(Stdio::piped())
            .spawn()
            .expect("spawn python3 listener");
        let stdout = child.stdout.take().expect("child stdout");
        let mut line = String::new();
        BufReader::new(stdout)
            .read_line(&mut line)
            .expect("read listener port");
        let port = line.trim().parse::<u64>().expect("listener port");

        let deadline = Instant::now() + Duration::from_secs(3);
        let mut rows = Vec::new();
        while Instant::now() < deadline {
            rows = workspace_port_rows(&workspace_id, 20, true).expect("discover ports");
            if rows.iter().any(|row| row["port"] == json!(port)) {
                break;
            }
            thread::sleep(Duration::from_millis(50));
        }
        child.kill().expect("kill listener");
        child.wait().expect("wait listener");
        assert!(rows.iter().any(|row| row["port"] == json!(port)));
    }
}
