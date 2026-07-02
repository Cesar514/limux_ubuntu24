// summary: Persist CMUX-compatible remote device registry metadata.
// purpose: Handle `remotes.list/add/remove` without requiring the unavailable CMUX cloud service.
// inputs: Control socket JSON params and the Limux config directory.
// returns/effects: Reads or atomically writes a local remotes registry file, rejecting corrupt data loudly.

use std::fs;
use std::io::ErrorKind;
use std::net::{Ipv4Addr, Ipv6Addr};
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};
use serde_json::{json, Map, Value};
use uuid::Uuid;

use crate::control_bridge::BridgeError;

const REGISTRY_FILE_NAME: &str = "remotes.json";

#[derive(Clone, Debug, Default, Deserialize, Serialize)]
struct RemoteRegistryFile {
    #[serde(default)]
    remotes: Vec<RemoteDevice>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
struct RemoteDevice {
    #[serde(rename = "deviceId")]
    device_id: String,
    #[serde(rename = "displayName")]
    display_name: String,
    #[serde(default)]
    routes: Vec<RemoteRoute>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    tag: Option<String>,
    #[serde(rename = "lastSeenAt")]
    last_seen_at: u64,
    platform: String,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
struct RemoteRoute {
    host: String,
    port: u16,
}

impl RemoteDevice {
    // purpose: Convert one persisted remote into the CMUX registry response shape.
    // inputs: Persisted device metadata.
    // returns/effects: Returns a JSON row without mutating registry state.
    fn to_json(&self) -> Value {
        json!({
            "deviceId": self.device_id,
            "displayName": self.display_name,
            "platform": self.platform,
            "routes": self.routes,
            "tag": self.tag,
            "lastSeenAt": self.last_seen_at,
            "online": false,
            "source": "limux_local_registry"
        })
    }
}

// purpose: Route a CMUX `remotes.*` method to the local registry backend.
// inputs: Method name and validated JSON object params from the control bridge.
// returns/effects: Reads or writes `remotes.json` and returns CMUX-shaped JSON.
pub fn handle(method: &str, params: &Map<String, Value>) -> Result<Value, BridgeError> {
    let path = registry_path()?;
    match method {
        "remotes.list" => list_remotes_at(&path),
        "remotes.add" => add_remote_at(&path, params),
        "remotes.remove" => remove_remote_at(&path, params),
        _ => Err(BridgeError::invalid_params(format!(
            "unsupported remotes method: {method}"
        ))),
    }
}

// purpose: Resolve the Limux remotes registry path.
// inputs: Process XDG config environment through shared shortcut config path logic.
// returns/effects: Returns an error if the config directory cannot be resolved.
fn registry_path() -> Result<PathBuf, BridgeError> {
    crate::shortcut_config::config_dir_path()
        .map(|dir| dir.join(REGISTRY_FILE_NAME))
        .ok_or_else(|| {
            BridgeError::internal("config_dir unavailable; cannot load remotes registry")
        })
}

// purpose: List all persisted local remote registry rows.
// inputs: Registry file path.
// returns/effects: Reads and validates the registry if present.
fn list_remotes_at(path: &Path) -> Result<Value, BridgeError> {
    let registry = read_registry(path)?;
    let remotes = registry
        .remotes
        .iter()
        .map(RemoteDevice::to_json)
        .collect::<Vec<_>>();
    Ok(json!({ "remotes": remotes, "source": "limux_local_registry" }))
}

// purpose: Add or replace one local remote registry entry.
// inputs: Registry file path and CMUX `remotes.add` params.
// returns/effects: Atomically persists the updated registry and returns the stored row.
fn add_remote_at(path: &Path, params: &Map<String, Value>) -> Result<Value, BridgeError> {
    let name = required_non_empty_string(params, "name")?;
    let routes = required_route_array(params)?;
    let tag = optional_non_empty_string(params, "tag")?;
    let mut registry = read_registry(path)?;
    let device_id = registry
        .remotes
        .iter()
        .find(|remote| remote.display_name == name)
        .map(|remote| remote.device_id.clone())
        .unwrap_or_else(|| Uuid::new_v4().to_string());
    let remote = RemoteDevice {
        device_id,
        display_name: name,
        routes,
        tag,
        last_seen_at: unix_seconds()?,
        platform: "linux".to_string(),
    };
    registry.remotes.retain(|existing| {
        existing.device_id != remote.device_id && existing.display_name != remote.display_name
    });
    registry.remotes.push(remote.clone());
    registry
        .remotes
        .sort_by(|left, right| left.display_name.cmp(&right.display_name));
    write_registry(path, &registry)?;
    Ok(remote.to_json())
}

// purpose: Remove one local remote registry entry by display name or device id.
// inputs: Registry file path and CMUX `remotes.remove` params.
// returns/effects: Atomically persists the registry if a row was removed.
fn remove_remote_at(path: &Path, params: &Map<String, Value>) -> Result<Value, BridgeError> {
    let target = required_non_empty_string(params, "target")?;
    let mut registry = read_registry(path)?;
    let before = registry.remotes.len();
    registry
        .remotes
        .retain(|remote| remote.device_id != target && remote.display_name != target);
    if registry.remotes.len() == before {
        return Err(BridgeError::not_found(format!(
            "remote '{target}' was not found"
        )));
    }
    write_registry(path, &registry)?;
    Ok(json!({ "removed": target, "source": "limux_local_registry" }))
}

// purpose: Read and validate the local remote registry file.
// inputs: Registry file path.
// returns/effects: Missing files produce an empty registry; corrupt files fail loudly.
fn read_registry(path: &Path) -> Result<RemoteRegistryFile, BridgeError> {
    let raw = match fs::read_to_string(path) {
        Ok(raw) => raw,
        Err(err) if err.kind() == ErrorKind::NotFound => return Ok(RemoteRegistryFile::default()),
        Err(err) => {
            return Err(BridgeError::internal(format!(
                "failed to read remotes registry `{}`: {err}",
                path.display()
            )))
        }
    };
    let registry: RemoteRegistryFile = serde_json::from_str(&raw).map_err(|err| {
        BridgeError::invalid_params(format!(
            "invalid remotes registry `{}`: {err}",
            path.display()
        ))
    })?;
    validate_registry(&registry)?;
    Ok(registry)
}

// purpose: Atomically persist a local remote registry file with private permissions.
// inputs: Registry file path and parsed registry.
// returns/effects: Creates parent directories and renames a temporary file into place.
fn write_registry(path: &Path, registry: &RemoteRegistryFile) -> Result<(), BridgeError> {
    let parent = path.parent().ok_or_else(|| {
        BridgeError::internal(format!(
            "remotes registry path `{}` has no parent",
            path.display()
        ))
    })?;
    fs::create_dir_all(parent).map_err(|err| {
        BridgeError::internal(format!(
            "failed to create remotes registry directory `{}`: {err}",
            parent.display()
        ))
    })?;
    let tmp_path = path.with_extension("json.tmp");
    let bytes = serde_json::to_vec_pretty(registry).map_err(|err| {
        BridgeError::internal(format!("failed to serialize remotes registry: {err}"))
    })?;
    fs::write(&tmp_path, bytes).map_err(|err| {
        BridgeError::internal(format!(
            "failed to write temporary remotes registry `{}`: {err}",
            tmp_path.display()
        ))
    })?;
    fs::rename(&tmp_path, path).map_err(|err| {
        BridgeError::internal(format!(
            "failed to persist remotes registry `{}`: {err}",
            path.display()
        ))
    })
}

// purpose: Validate loaded registry rows before using them.
// inputs: Parsed registry file contents.
// returns/effects: Returns invalid_params for corrupt local registry data.
fn validate_registry(registry: &RemoteRegistryFile) -> Result<(), BridgeError> {
    for remote in &registry.remotes {
        if remote.device_id.trim().is_empty() || remote.display_name.trim().is_empty() {
            return Err(BridgeError::invalid_params(
                "invalid remotes registry: deviceId and displayName are required",
            ));
        }
        for route in &remote.routes {
            validate_host(&route.host)?;
            if route.port == 0 {
                return Err(BridgeError::invalid_params(
                    "invalid remotes registry: route port must be 1-65535",
                ));
            }
        }
    }
    Ok(())
}

// purpose: Extract a required non-empty string field from socket params.
// inputs: JSON params and required field name.
// returns/effects: Returns invalid_params for missing, non-string, or empty values.
fn required_non_empty_string(
    params: &Map<String, Value>,
    key: &str,
) -> Result<String, BridgeError> {
    let value = params
        .get(key)
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .ok_or_else(|| BridgeError::invalid_params(format!("remotes requires non-empty {key}")))?;
    Ok(value.to_string())
}

// purpose: Extract an optional non-empty string field from socket params.
// inputs: JSON params and optional field name.
// returns/effects: Rejects non-string values and normalizes blank strings to None.
fn optional_non_empty_string(
    params: &Map<String, Value>,
    key: &str,
) -> Result<Option<String>, BridgeError> {
    let Some(value) = params.get(key) else {
        return Ok(None);
    };
    let Some(raw) = value.as_str() else {
        return Err(BridgeError::invalid_params(format!(
            "remotes {key} must be a string"
        )));
    };
    let trimmed = raw.trim();
    Ok((!trimmed.is_empty()).then(|| trimmed.to_string()))
}

// purpose: Parse and validate CMUX `routes` params.
// inputs: JSON params with routes as host:port strings.
// returns/effects: Returns route structs or invalid_params for malformed routes.
fn required_route_array(params: &Map<String, Value>) -> Result<Vec<RemoteRoute>, BridgeError> {
    let rows = params
        .get("routes")
        .and_then(Value::as_array)
        .ok_or_else(|| BridgeError::invalid_params("remotes.add requires routes array"))?;
    if rows.is_empty() {
        return Err(BridgeError::invalid_params(
            "remotes.add requires at least one route",
        ));
    }
    rows.iter()
        .map(|row| {
            let raw = row.as_str().ok_or_else(|| {
                BridgeError::invalid_params("remotes.add routes must be host:port strings")
            })?;
            parse_route(raw)
        })
        .collect()
}

// purpose: Parse a host:port or [ipv6]:port route.
// inputs: Raw CMUX route token.
// returns/effects: Returns a normalized route or invalid_params.
fn parse_route(raw: &str) -> Result<RemoteRoute, BridgeError> {
    let (host, port) = split_route(raw)?;
    validate_host(&host)?;
    let port = port.parse::<u16>().map_err(|_| {
        BridgeError::invalid_params(format!(
            "invalid remote route '{raw}': port must be 1-65535"
        ))
    })?;
    if port == 0 {
        return Err(BridgeError::invalid_params(format!(
            "invalid remote route '{raw}': port must be 1-65535"
        )));
    }
    Ok(RemoteRoute { host, port })
}

// purpose: Split route text while preserving bracketed IPv6 host support.
// inputs: Raw route token.
// returns/effects: Returns host and port substrings or invalid_params.
fn split_route(raw: &str) -> Result<(String, String), BridgeError> {
    let trimmed = raw.trim();
    if let Some(rest) = trimmed.strip_prefix('[') {
        let Some(close) = rest.find(']') else {
            return Err(BridgeError::invalid_params(format!(
                "invalid remote route '{raw}': unterminated IPv6 bracket"
            )));
        };
        let host = rest[..close].trim();
        let after = &rest[close + 1..];
        if !after.starts_with(':') || host.is_empty() {
            return Err(BridgeError::invalid_params(format!(
                "invalid remote route '{raw}': use [ipv6]:port"
            )));
        }
        return Ok((host.to_string(), after[1..].to_string()));
    }
    let Some(colon) = trimmed.rfind(':') else {
        return Err(BridgeError::invalid_params(format!(
            "invalid remote route '{raw}': missing :port"
        )));
    };
    let host = trimmed[..colon].trim();
    if host.is_empty() || host.contains(':') {
        return Err(BridgeError::invalid_params(format!(
            "invalid remote route '{raw}': bracket IPv6 addresses as [ipv6]:port"
        )));
    }
    Ok((host.to_string(), trimmed[colon + 1..].to_string()))
}

// purpose: Reject local-only hosts that cannot be used as remote registry routes.
// inputs: Parsed route host.
// returns/effects: Returns invalid_params for localhost, loopback, or unspecified addresses.
fn validate_host(host: &str) -> Result<(), BridgeError> {
    let normalized = host.trim().trim_end_matches('.').to_ascii_lowercase();
    if normalized == "localhost" || normalized.ends_with(".localhost") {
        return Err(BridgeError::invalid_params(
            "remote route host must not be localhost",
        ));
    }
    if let Ok(addr) = normalized.parse::<Ipv4Addr>() {
        if addr.is_loopback() || addr.is_unspecified() {
            return Err(BridgeError::invalid_params(
                "remote route host must not be loopback or unspecified",
            ));
        }
    }
    if let Ok(addr) = normalized.parse::<Ipv6Addr>() {
        let mapped_loopback = addr
            .to_ipv4_mapped()
            .is_some_and(|v4| v4.is_loopback() || v4.is_unspecified());
        if addr.is_loopback() || addr.is_unspecified() || mapped_loopback {
            return Err(BridgeError::invalid_params(
                "remote route host must not be loopback or unspecified",
            ));
        }
    }
    Ok(())
}

// purpose: Generate a coarse update timestamp for registry rows.
// inputs: System clock.
// returns/effects: Returns internal error if the clock is before the Unix epoch.
fn unix_seconds() -> Result<u64, BridgeError> {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_secs())
        .map_err(|err| BridgeError::internal(format!("system clock before Unix epoch: {err}")))
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    // purpose: Verify add/list/remove behavior without requiring the GTK host.
    // inputs: Temporary registry path and CMUX-shaped params.
    // returns/effects: Asserts persisted rows and remove responses are CMUX-shaped.
    #[test]
    fn registry_add_list_remove_round_trips_remote_rows() {
        let dir = tempdir().expect("tempdir");
        let path = dir.path().join("remotes.json");
        let params = Map::from_iter([
            ("name".to_string(), json!("studio")),
            (
                "routes".to_string(),
                json!(["100.64.1.2:51001", "[fd7a:115c:a1e0::1]:443"]),
            ),
            ("tag".to_string(), json!("stable")),
        ]);

        let added = add_remote_at(&path, &params).expect("add remote");
        assert_eq!(added["displayName"], "studio");
        assert_eq!(added["routes"][0]["host"], "100.64.1.2");
        assert_eq!(added["routes"][1]["port"], 443);

        let listed = list_remotes_at(&path).expect("list remotes");
        assert_eq!(listed["remotes"].as_array().expect("rows").len(), 1);
        let target = added["deviceId"].as_str().expect("device id");
        let removed = remove_remote_at(
            &path,
            &Map::from_iter([("target".to_string(), json!(target))]),
        )
        .expect("remove remote");
        assert_eq!(removed["removed"], target);
        let listed = list_remotes_at(&path).expect("list after remove");
        assert!(listed["remotes"].as_array().expect("rows").is_empty());
    }

    // purpose: Verify socket callers cannot persist unusable local-only remote routes.
    // inputs: Route parser examples.
    // returns/effects: Asserts loopback/unspecified routes fail loudly.
    #[test]
    fn registry_rejects_loopback_and_malformed_routes() {
        assert!(parse_route("127.0.0.1:51001").is_err());
        assert!(parse_route("[::1]:51001").is_err());
        assert!(parse_route("100.64.1.2:0").is_err());
        assert!(parse_route("fd7a:115c:a1e0::1:443").is_err());
    }
}
