// summary: Parse Linux procfs process and socket metadata for sidebar port discovery.
// purpose: Keep low-level /proc parsing separate from CMUX sidebar row construction.
// inputs: /proc/<pid>/stat, /proc/<pid>/environ, /proc/<pid>/fd, and /proc/net/tcp* rows.
// returns/effects: Returns parsed process/socket metadata or explicit read/parse errors.

use std::collections::BTreeSet;
use std::fs;
use std::io::ErrorKind;

use crate::port_discovery::ListeningSocket;

/// purpose: Parse one /proc/net/tcp* row when it represents a listening socket.
/// inputs: Raw table line and public protocol label.
/// returns/effects: Returns None for non-LISTEN states and errors for malformed LISTEN rows.
pub(crate) fn parse_listening_socket_line(
    line: &str,
    protocol: &str,
) -> Result<Option<(u64, ListeningSocket)>, String> {
    let fields = line.split_whitespace().collect::<Vec<_>>();
    if fields.len() < 10 {
        return Err(format!("malformed /proc/net/{protocol} row: {line}"));
    }
    if fields[3] != "0A" {
        return Ok(None);
    }
    let (address, port) = parse_local_address(fields[1], protocol)?;
    let inode = fields[9]
        .parse::<u64>()
        .map_err(|error| format!("malformed /proc/net/{protocol} inode: {error}"))?;
    Ok(Some((
        inode,
        ListeningSocket {
            protocol: protocol.to_string(),
            address,
            port,
        },
    )))
}

/// purpose: Read a process parent pid from /proc/<pid>/stat.
/// inputs: Process id.
/// returns/effects: Returns None only when the process exits during the scan.
pub(crate) fn read_process_parent_if_alive(pid: u32) -> Result<Option<u32>, String> {
    let path = format!("/proc/{pid}/stat");
    match fs::read_to_string(&path) {
        Ok(stat) => parse_parent_pid(&stat).map(Some),
        Err(error) if error.kind() == ErrorKind::NotFound => Ok(None),
        Err(error) => Err(format!("failed to read {path}: {error}")),
    }
}

/// purpose: Read Limux workspace attribution from process environment.
/// inputs: Process id.
/// returns/effects: Returns None when no Limux workspace id exists or procfs denies/exits during scan.
pub(crate) fn process_workspace_id(pid: u32) -> Result<Option<String>, String> {
    let path = format!("/proc/{pid}/environ");
    let raw = match fs::read(&path) {
        Ok(raw) => raw,
        Err(error)
            if matches!(
                error.kind(),
                ErrorKind::NotFound | ErrorKind::PermissionDenied
            ) =>
        {
            return Ok(None);
        }
        Err(error) => return Err(format!("failed to read {path}: {error}")),
    };
    for entry in raw
        .split(|byte| *byte == 0)
        .filter(|entry| !entry.is_empty())
    {
        if let Some(value) = entry.strip_prefix(b"LIMUX_WORKSPACE_ID=") {
            return Ok(Some(String::from_utf8_lossy(value).to_string()));
        }
    }
    Ok(None)
}

/// purpose: Read socket inode references from one process fd directory.
/// inputs: Process id.
/// returns/effects: Returns socket inodes while tolerating processes/fds that exit mid-scan.
pub(crate) fn process_socket_inodes(pid: u32) -> Result<BTreeSet<u64>, String> {
    let mut inodes = BTreeSet::new();
    let fd_dir = format!("/proc/{pid}/fd");
    let entries = match fs::read_dir(&fd_dir) {
        Ok(entries) => entries,
        Err(error) if error.kind() == ErrorKind::NotFound => return Ok(inodes),
        Err(error) => return Err(format!("failed to read {fd_dir}: {error}")),
    };
    for entry in entries {
        let entry = entry.map_err(|error| format!("failed to read {fd_dir} entry: {error}"))?;
        match fs::read_link(entry.path()) {
            Ok(target) => {
                if let Some(inode) = parse_socket_inode(&target.to_string_lossy())? {
                    inodes.insert(inode);
                }
            }
            Err(error) if error.kind() == ErrorKind::NotFound => {}
            Err(error) => return Err(format!("failed to read fd link for pid {pid}: {error}")),
        }
    }
    Ok(inodes)
}

/// purpose: Read a compact command label for a process.
/// inputs: Process id.
/// returns/effects: Returns cmdline, comm, or pid label if process details disappeared.
pub(crate) fn read_command(pid: u32) -> String {
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

/// purpose: Parse local address and port from a procfs socket endpoint.
/// inputs: Hex endpoint and protocol label for diagnostics.
/// returns/effects: Returns a display address and port or a malformed-endpoint error.
fn parse_local_address(endpoint: &str, protocol: &str) -> Result<(String, u16), String> {
    let (address_hex, port_hex) = endpoint
        .split_once(':')
        .ok_or_else(|| format!("malformed /proc/net/{protocol} local_address"))?;
    let port = u16::from_str_radix(port_hex, 16)
        .map_err(|error| format!("malformed /proc/net/{protocol} port: {error}"))?;
    let address = if address_hex.len() == 8 {
        parse_ipv4_hex(address_hex, protocol)?
    } else if address_hex.len() == 32 {
        parse_ipv6_hex(address_hex, protocol)?
    } else {
        return Err(format!("malformed /proc/net/{protocol} address width"));
    };
    Ok((address, port))
}

/// purpose: Convert little-endian procfs IPv4 hex into dotted decimal text.
/// inputs: Eight hex characters from /proc/net/tcp.
/// returns/effects: Returns an IPv4 address string or a parse error.
fn parse_ipv4_hex(raw: &str, protocol: &str) -> Result<String, String> {
    let value = u32::from_str_radix(raw, 16)
        .map_err(|error| format!("malformed /proc/net/{protocol} ipv4: {error}"))?;
    let bytes = value.to_le_bytes();
    Ok(format!(
        "{}.{}.{}.{}",
        bytes[0], bytes[1], bytes[2], bytes[3]
    ))
}

/// purpose: Convert procfs IPv6 hex into display text without guessing hostnames.
/// inputs: Thirty-two hex characters from /proc/net/tcp6.
/// returns/effects: Returns an IPv6 address string or a parse error.
fn parse_ipv6_hex(raw: &str, protocol: &str) -> Result<String, String> {
    let mut groups = Vec::new();
    for chunk in raw.as_bytes().chunks(8) {
        let chunk_text = std::str::from_utf8(chunk)
            .map_err(|error| format!("malformed /proc/net/{protocol} ipv6 utf8: {error}"))?;
        let value = u32::from_str_radix(chunk_text, 16)
            .map_err(|error| format!("malformed /proc/net/{protocol} ipv6: {error}"))?;
        for segment in value.to_le_bytes().chunks(2) {
            groups.push(format!("{:02x}{:02x}", segment[0], segment[1]));
        }
    }
    Ok(groups.join(":"))
}

/// purpose: Extract ppid from a procfs stat row with command names in parentheses.
/// inputs: Raw /proc/<pid>/stat text.
/// returns/effects: Returns parent pid or a malformed-stat error.
fn parse_parent_pid(raw: &str) -> Result<u32, String> {
    let close = raw.rfind(')').ok_or("malformed stat: missing name end")?;
    raw.get(close + 2..)
        .ok_or("malformed stat: missing fields")?
        .split_whitespace()
        .nth(1)
        .ok_or("malformed stat: missing ppid")?
        .parse::<u32>()
        .map_err(|error| format!("malformed stat ppid: {error}"))
}

/// purpose: Parse a Linux fd symlink target for socket inode values.
/// inputs: Symlink target text such as socket:[123].
/// returns/effects: Returns socket inode, None for non-socket fds, or a malformed error.
fn parse_socket_inode(target: &str) -> Result<Option<u64>, String> {
    let Some(raw) = target
        .strip_prefix("socket:[")
        .and_then(|value| value.strip_suffix(']'))
    else {
        return Ok(None);
    };
    raw.parse::<u64>()
        .map(Some)
        .map_err(|error| format!("malformed socket inode {target}: {error}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    // purpose: Verify procfs TCP parsing keeps only LISTEN socket rows.
    // inputs: Synthetic /proc/net/tcp lines for listen and established states.
    // returns/effects: Asserts address, port, protocol, and inode parsing.
    #[test]
    fn parse_listening_socket_line_reads_tcp_listeners() {
        let listen =
            "0: 0100007F:1F90 00000000:0000 0A 00000000:00000000 00:00000000 00000000 1000 0 42 1";
        let parsed = parse_listening_socket_line(listen, "tcp")
            .expect("parse")
            .expect("listen row");
        assert_eq!(parsed.0, 42);
        assert_eq!(
            parsed.1,
            ListeningSocket {
                protocol: "tcp".to_string(),
                address: "127.0.0.1".to_string(),
                port: 8080,
            }
        );

        let established =
            "0: 0100007F:1F90 0100007F:0035 01 00000000:00000000 00:00000000 00000000 1000 0 43 1";
        assert_eq!(
            parse_listening_socket_line(established, "tcp").expect("parse"),
            None
        );
    }

    // purpose: Verify procfs helper parsers handle process and fd formats.
    // inputs: Stat row with spaced process name plus socket/non-socket fd links.
    // returns/effects: Asserts pid and socket inode extraction.
    #[test]
    fn process_and_socket_parsers_handle_procfs_formats() {
        assert_eq!(
            parse_parent_pid("123 (name with spaces) S 77 1 1 0").expect("ppid"),
            77
        );
        assert_eq!(
            parse_socket_inode("socket:[987]").expect("socket"),
            Some(987)
        );
        assert_eq!(parse_socket_inode("/dev/null").expect("non-socket"), None);
    }
}
