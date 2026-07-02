// summary: Build Linux process-memory diagnostics for the live Limux host.
// purpose: Report host RSS plus descendant process groups for active terminal and agent workloads.
// inputs: Linux /proc process metadata for the current Limux process and descendants.
// returns/effects: Returns JSON diagnostics or explicit errors when required host process data is unavailable.

use std::collections::{BTreeMap, VecDeque};
use std::fs;
use std::io::ErrorKind;

use serde_json::{json, Value};

#[derive(Clone, Debug, Default, Eq, PartialEq)]
struct ProcStats {
    pid: u32,
    ppid: u32,
    name: String,
    resident_bytes: u64,
    command: String,
    attribution: ProcessAttribution,
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
struct ProcessAttribution {
    workspace_id: Option<String>,
    pane_id: Option<String>,
    surface_id: Option<String>,
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
struct ProcessGroup {
    root: ProcStats,
    process_count: usize,
    rss_bytes: u64,
}

/// purpose: Build the CMUX-style memory diagnostic payload for the current host.
/// inputs: top_group_limit controls the number of child groups returned.
/// returns/effects: Returns JSON with app RSS and descendant group summaries.
pub fn memory_diagnostic_payload(top_group_limit: usize) -> Result<Value, String> {
    diagnostic_payload(top_group_limit, None, "memory_diagnostic")
}

/// purpose: Build the CMUX-style top diagnostic payload for the current host.
/// inputs: top_group_limit controls rows and workspace_filter optionally scopes by workspace.
/// returns/effects: Returns JSON with app RSS and descendant process groups.
pub fn top_diagnostic_payload(
    top_group_limit: usize,
    workspace_filter: Option<&str>,
) -> Result<Value, String> {
    diagnostic_payload(top_group_limit, workspace_filter, "top_diagnostic")
}

/// purpose: Build a process diagnostic payload from one /proc scan.
/// inputs: Row limit, optional workspace scope, and public payload key.
/// returns/effects: Returns aggregate app/child process memory diagnostics.
fn diagnostic_payload(
    top_group_limit: usize,
    workspace_filter: Option<&str>,
    payload_key: &str,
) -> Result<Value, String> {
    let root_pid = std::process::id();
    let root = read_process_stats(root_pid)?;
    let groups = collect_child_groups(root_pid)?;
    let mut groups = group_processes_by_direct_child(groups)
        .into_iter()
        .filter(|group| group_matches_workspace(group, workspace_filter))
        .collect::<Vec<_>>();
    groups.sort_by(|left, right| right.rss_bytes.cmp(&left.rss_bytes));

    let total_child_rss = groups.iter().map(|group| group.rss_bytes).sum::<u64>();
    let total_child_count = groups
        .iter()
        .map(|group| group.process_count)
        .sum::<usize>();
    let group_values = groups
        .into_iter()
        .take(top_group_limit)
        .map(group_to_json)
        .collect::<Vec<_>>();

    let mut diagnostic = json!({
        "summary": format!(
            "Limux memory: app RSS {}, child RSS {} across {}",
            format_bytes(root.resident_bytes),
            format_bytes(total_child_rss),
            process_count_text(total_child_count)
        ),
        "app": process_to_json(&root),
        "children": {
            "recursive_rss_bytes": total_child_rss,
            "process_count": total_child_count,
            "groups": group_values,
        }
    });
    if let Some(workspace_id) = workspace_filter {
        diagnostic["scope"] = json!({
            "workspace_id": normalize_workspace_filter(workspace_id),
            "workspace_ref": format!("workspace:{}", normalize_workspace_filter(workspace_id)),
        });
    }
    Ok(json!({ payload_key: diagnostic }))
}

/// purpose: Check whether a process group belongs to an optional workspace scope.
/// inputs: Process group plus optional raw/ref workspace id.
/// returns/effects: Returns true for unscoped diagnostics or matching attribution.
fn group_matches_workspace(group: &ProcessGroup, workspace_filter: Option<&str>) -> bool {
    let Some(workspace_filter) = workspace_filter else {
        return true;
    };
    let expected = normalize_workspace_filter(workspace_filter);
    group
        .root
        .attribution
        .workspace_id
        .as_deref()
        .is_some_and(|workspace_id| workspace_id == expected)
}

/// purpose: Normalize CMUX workspace refs for process attribution matching.
/// inputs: Raw workspace id or `workspace:<id>` ref.
/// returns/effects: Returns the attribution id stored in child environments.
fn normalize_workspace_filter(raw: &str) -> &str {
    raw.strip_prefix("workspace:").unwrap_or(raw)
}

/// purpose: Read descendants grouped under each direct child of the host process.
/// inputs: root_pid is the Limux host process id.
/// returns/effects: Returns direct-child groups, ignoring descendants that exit during the scan.
fn collect_child_groups(root_pid: u32) -> Result<Vec<(u32, ProcStats)>, String> {
    let child_map = process_child_map()?;
    let mut queue = VecDeque::new();
    let mut results = Vec::new();

    for child in child_map.get(&root_pid).into_iter().flatten() {
        queue.push_back((*child, *child));
    }

    while let Some((group_pid, pid)) = queue.pop_front() {
        if let Some(stats) = read_process_stats_if_alive(pid)? {
            results.push((group_pid, stats));
            for child in child_map.get(&pid).into_iter().flatten() {
                queue.push_back((group_pid, *child));
            }
        }
    }
    Ok(results)
}

/// purpose: Aggregate descendant process stats by direct child process id.
/// inputs: pairs of direct child pid and descendant process stats.
/// returns/effects: Returns RSS/count totals plus each group's root process stats.
fn group_processes_by_direct_child(pairs: Vec<(u32, ProcStats)>) -> Vec<ProcessGroup> {
    let mut groups: BTreeMap<u32, ProcessGroup> = BTreeMap::new();
    for (group_pid, stats) in pairs {
        let group = groups.entry(group_pid).or_insert_with(|| ProcessGroup {
            root: stats.clone(),
            process_count: 0,
            rss_bytes: 0,
        });
        group.process_count += 1;
        group.rss_bytes += stats.resident_bytes;
    }
    groups.into_values().collect()
}

/// purpose: Build a parent-to-children map from Linux /proc entries.
/// inputs: Current /proc filesystem.
/// returns/effects: Returns a child map or an explicit /proc read error.
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

/// purpose: Read only the parent pid needed for process-tree traversal.
/// inputs: pid identifies the process to inspect.
/// returns/effects: Returns None only when the process exits during the scan.
fn read_process_parent_if_alive(pid: u32) -> Result<Option<u32>, String> {
    let stat_path = format!("/proc/{pid}/stat");
    match fs::read_to_string(&stat_path) {
        Ok(stat) => parse_stat(&stat).map(|(_, ppid)| Some(ppid)),
        Err(error) if error.kind() == ErrorKind::NotFound => Ok(None),
        Err(error) => Err(format!("failed to read {stat_path}: {error}")),
    }
}

/// purpose: Read process stats while tolerating processes that exit mid-scan.
/// inputs: pid identifies the process to inspect.
/// returns/effects: Returns None only when the process no longer exists.
fn read_process_stats_if_alive(pid: u32) -> Result<Option<ProcStats>, String> {
    match read_process_stats(pid) {
        Ok(stats) => Ok(Some(stats)),
        Err(error) if error.contains("not found") => Ok(None),
        Err(error) => Err(error),
    }
}

/// purpose: Read stable process metadata from /proc for one pid.
/// inputs: pid identifies the process to inspect.
/// returns/effects: Returns process stats or an explicit parse/read error.
fn read_process_stats(pid: u32) -> Result<ProcStats, String> {
    let stat_path = format!("/proc/{pid}/stat");
    let status_path = format!("/proc/{pid}/status");
    let stat = read_required(&stat_path)?;
    let status = read_required(&status_path)?;

    let (name, ppid) = parse_stat(&stat)?;
    Ok(ProcStats {
        pid,
        ppid,
        name,
        resident_bytes: parse_rss_bytes(&status)?,
        command: read_command(pid),
        attribution: read_attribution(pid),
    })
}

/// purpose: Read a required /proc text file.
/// inputs: path points to a required process metadata file.
/// returns/effects: Returns contents or an explicit not-found/read error.
fn read_required(path: &str) -> Result<String, String> {
    fs::read_to_string(path).map_err(|error| {
        if error.kind() == ErrorKind::NotFound {
            format!("{path} not found")
        } else {
            format!("failed to read {path}: {error}")
        }
    })
}

/// purpose: Parse process name and parent pid from /proc/<pid>/stat.
/// inputs: raw stat file contents.
/// returns/effects: Returns name and ppid or an explicit malformed-stat error.
fn parse_stat(raw: &str) -> Result<(String, u32), String> {
    let open = raw.find('(').ok_or("malformed stat: missing name start")?;
    let close = raw.rfind(')').ok_or("malformed stat: missing name end")?;
    let name = raw[open + 1..close].to_string();
    let rest = raw
        .get(close + 2..)
        .ok_or("malformed stat: missing fields")?;
    let ppid = rest
        .split_whitespace()
        .nth(1)
        .ok_or("malformed stat: missing ppid")?
        .parse::<u32>()
        .map_err(|error| format!("malformed stat ppid: {error}"))?;
    Ok((name, ppid))
}

/// purpose: Parse resident memory from /proc/<pid>/status.
/// inputs: raw status file contents.
/// returns/effects: Returns RSS bytes or an explicit missing/malformed error.
fn parse_rss_bytes(raw: &str) -> Result<u64, String> {
    let line = raw
        .lines()
        .find(|line| line.starts_with("VmRSS:"))
        .ok_or("status missing VmRSS")?;
    let kb = line
        .split_whitespace()
        .nth(1)
        .ok_or("status VmRSS missing value")?
        .parse::<u64>()
        .map_err(|error| format!("status VmRSS malformed: {error}"))?;
    Ok(kb * 1024)
}

/// purpose: Read a displayable command from cmdline, falling back to comm only when cmdline is empty.
/// inputs: pid identifies the process.
/// returns/effects: Returns a command label or process name when details are unavailable.
fn read_command(pid: u32) -> String {
    let cmdline = fs::read(format!("/proc/{pid}/cmdline")).unwrap_or_default();
    let command = cmdline
        .split(|byte| *byte == 0)
        .filter(|part| !part.is_empty())
        .map(|part| String::from_utf8_lossy(part).to_string())
        .collect::<Vec<_>>()
        .join(" ");
    if !command.is_empty() {
        return command;
    }
    fs::read_to_string(format!("/proc/{pid}/comm"))
        .map(|value| value.trim().to_string())
        .unwrap_or_else(|_| format!("pid:{pid}"))
}

/// purpose: Read Limux attribution from a process environment when available.
/// inputs: pid identifies the process.
/// returns/effects: Returns optional workspace/pane/surface attribution.
fn read_attribution(pid: u32) -> ProcessAttribution {
    let environ = fs::read(format!("/proc/{pid}/environ")).unwrap_or_default();
    let mut attribution = ProcessAttribution::default();
    for entry in environ
        .split(|byte| *byte == 0)
        .filter(|entry| !entry.is_empty())
    {
        if let Some((key, value)) = split_env_entry(entry) {
            match key {
                "LIMUX_WORKSPACE_ID" => attribution.workspace_id = Some(value.to_string()),
                "LIMUX_PANE_ID" => attribution.pane_id = Some(value.to_string()),
                "LIMUX_SURFACE_ID" => attribution.surface_id = Some(value.to_string()),
                _ => {}
            }
        }
    }
    attribution
}

/// purpose: Split one null-delimited environment entry into key/value strings.
/// inputs: raw entry bytes from /proc/<pid>/environ.
/// returns/effects: Returns UTF-8-ish key/value text when an equals sign exists.
fn split_env_entry(entry: &[u8]) -> Option<(&str, String)> {
    let equals = entry.iter().position(|byte| *byte == b'=')?;
    let key = std::str::from_utf8(&entry[..equals]).ok()?;
    let value = String::from_utf8_lossy(&entry[equals + 1..]).to_string();
    Some((key, value))
}

/// purpose: Convert one process group into the public JSON shape.
/// inputs: group contains aggregate RSS, process count, and root metadata.
/// returns/effects: Returns a JSON object for CLI rendering.
fn group_to_json(group: ProcessGroup) -> Value {
    json!({
        "pid": group.root.pid,
        "name": group.root.name,
        "command": group.root.command,
        "rss_bytes": group.rss_bytes,
        "process_count": group.process_count,
        "top_attribution": attribution_to_json(&group.root.attribution),
    })
}

/// purpose: Convert one process into the public JSON shape.
/// inputs: stats contains process memory and identity metadata.
/// returns/effects: Returns a JSON object for CLI rendering.
fn process_to_json(stats: &ProcStats) -> Value {
    json!({
        "pid": stats.pid,
        "name": stats.name,
        "command": stats.command,
        "resident_bytes": stats.resident_bytes,
    })
}

/// purpose: Convert optional Limux process attribution into JSON.
/// inputs: attribution values may be absent when /proc environ is unavailable.
/// returns/effects: Returns an object with any known Limux identifiers.
fn attribution_to_json(attribution: &ProcessAttribution) -> Value {
    let mut value = serde_json::Map::new();
    if let Some(workspace_id) = &attribution.workspace_id {
        value.insert("workspace_id".to_string(), json!(workspace_id));
        value.insert(
            "workspace_ref".to_string(),
            json!(format!("workspace:{workspace_id}")),
        );
    }
    if let Some(pane_id) = &attribution.pane_id {
        value.insert("pane_id".to_string(), json!(pane_id));
        value.insert("pane_ref".to_string(), json!(format!("pane:{pane_id}")));
    }
    if let Some(surface_id) = &attribution.surface_id {
        value.insert("surface_id".to_string(), json!(surface_id));
        value.insert(
            "surface_ref".to_string(),
            json!(format!("surface:{surface_id}")),
        );
    }
    Value::Object(value)
}

/// purpose: Format a byte count for a concise human-readable summary.
/// inputs: bytes is a raw byte count.
/// returns/effects: Returns a binary-unit formatted string.
fn format_bytes(bytes: u64) -> String {
    let units = ["B", "KiB", "MiB", "GiB"];
    let mut value = bytes as f64;
    let mut unit = 0;
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

/// purpose: Format a process count for a concise human-readable summary.
/// inputs: count is the number of descendant processes.
/// returns/effects: Returns singular/plural display text.
fn process_count_text(count: usize) -> String {
    if count == 1 {
        "1 process".to_string()
    } else {
        format!("{count} processes")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_stat_handles_process_names_with_spaces() {
        let parsed = parse_stat("42 (name with spaces) S 7 8 9").expect("stat parses");
        assert_eq!(parsed, ("name with spaces".to_string(), 7));
    }

    #[test]
    fn parse_rss_bytes_reads_kilobytes() {
        let rss = parse_rss_bytes("Name:\ttest\nVmRSS:\t  123 kB\n").expect("rss parses");
        assert_eq!(rss, 123 * 1024);
    }

    #[test]
    fn split_env_entry_preserves_values_with_equals() {
        let parsed = split_env_entry(b"LIMUX_SURFACE_ID=1:terminal=extra").expect("env parses");
        assert_eq!(parsed, ("LIMUX_SURFACE_ID", "1:terminal=extra".to_string()));
    }

    #[test]
    fn group_workspace_filter_accepts_raw_and_ref_ids() {
        let mut group = ProcessGroup::default();
        group.root.attribution.workspace_id = Some("workspace-a".to_string());

        assert!(group_matches_workspace(&group, Some("workspace-a")));
        assert!(group_matches_workspace(
            &group,
            Some("workspace:workspace-a")
        ));
        assert!(!group_matches_workspace(&group, Some("workspace-b")));
    }
}
