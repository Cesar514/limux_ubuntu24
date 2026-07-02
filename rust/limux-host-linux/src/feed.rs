// summary: Store and resolve CMUX-compatible Feed workstream items.
// purpose: Provide feed.push and feed.*.reply backend parity for agent approval workflows.
// inputs: V2 feed socket params containing WorkstreamEvent frames and decision replies.
// returns/effects: Maintains a bounded in-memory feed ring and blocks feed.push until reply or timeout.

use std::collections::VecDeque;
use std::sync::{Condvar, Mutex, OnceLock};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use serde_json::{json, Map, Value};

use crate::control_bridge::BridgeError;

const MAX_FEED_ITEMS: usize = 2_000;
const MAX_WAIT_SECONDS: f64 = 120.0;

static FEED: OnceLock<FeedCoordinator> = OnceLock::new();

pub fn coordinator() -> &'static FeedCoordinator {
    FEED.get_or_init(FeedCoordinator::new)
}

#[derive(Clone, Debug)]
struct FeedItem {
    id: String,
    request_id: Option<String>,
    event: Value,
    source: String,
    kind: String,
    status: FeedStatus,
    created_at_ms: u128,
    decision: Option<Value>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
enum FeedStatus {
    Pending,
    Resolved,
    Expired,
    Telemetry,
}

struct FeedState {
    next_id: u64,
    items: VecDeque<FeedItem>,
}

pub struct FeedCoordinator {
    state: Mutex<FeedState>,
    changed: Condvar,
}

impl FeedCoordinator {
    fn new() -> Self {
        Self {
            state: Mutex::new(FeedState {
                next_id: 1,
                items: VecDeque::new(),
            }),
            changed: Condvar::new(),
        }
    }

    // purpose: Ingest one CMUX workstream event and optionally wait for a decision.
    // inputs: feed.push params with event or flattened event fields and wait_timeout_seconds.
    // returns/effects: Stores a bounded feed item and returns acknowledged/resolved/timed_out.
    pub fn push(&self, params: &Map<String, Value>) -> Result<Value, BridgeError> {
        let event = event_from_params(params)?;
        let wait = wait_timeout(params)?;
        let request_id = request_id_from_event(&event);
        let kind = kind_from_event(&event);
        let source = string_field(&event, &["_source", "source"])
            .unwrap_or("unknown")
            .to_string();
        let actionable = matches!(kind.as_str(), "permissionRequest" | "exitPlan" | "question");
        let should_wait = wait > Duration::ZERO && request_id.is_some();
        let status = if actionable && request_id.is_some() {
            FeedStatus::Pending
        } else {
            FeedStatus::Telemetry
        };

        let mut state = self.lock_state()?;
        let item_id = format!("feed-{}", state.next_id);
        state.next_id += 1;
        state.items.push_back(FeedItem {
            id: item_id.clone(),
            request_id: request_id.clone(),
            event,
            source,
            kind,
            status,
            created_at_ms: now_millis(),
            decision: None,
        });
        while state.items.len() > MAX_FEED_ITEMS {
            state.items.pop_front();
        }
        let event_payload = feed_event_payload(
            &item_id,
            request_id.as_deref(),
            state.items.back().expect("just pushed feed item"),
            "received",
            None,
        );
        publish_feed_bus_event(
            "feed.item.received",
            "feed",
            "feed.coordinator",
            event_payload.clone(),
        );
        publish_agent_hook_event(&event_payload);

        if !should_wait {
            publish_feed_bus_event(
                "feed.item.completed",
                "feed",
                "feed.coordinator",
                feed_event_payload(
                    &item_id,
                    request_id.as_deref(),
                    state.items.back().expect("just pushed feed item"),
                    "acknowledged",
                    None,
                ),
            );
            return Ok(json!({ "status": "acknowledged", "item_id": item_id }));
        }

        let request_id = request_id.expect("checked should_wait");
        let deadline = Instant::now() + wait;
        loop {
            if let Some(item) = state.items.iter().rev().find(|item| item.id == item_id) {
                if item.status == FeedStatus::Resolved {
                    publish_feed_bus_event(
                        "feed.item.completed",
                        "feed",
                        "feed.coordinator",
                        feed_event_payload(
                            &item_id,
                            Some(&request_id),
                            item,
                            "resolved",
                            item.decision.as_ref(),
                        ),
                    );
                    return Ok(json!({
                        "status": "resolved",
                        "item_id": item_id,
                        "decision": item.decision.clone().unwrap_or_else(|| json!({})),
                    }));
                }
                if item.status == FeedStatus::Expired {
                    return Ok(json!({ "status": "timed_out", "item_id": item_id }));
                }
            }

            let now = Instant::now();
            if now >= deadline {
                if let Some(item) = state
                    .items
                    .iter_mut()
                    .rev()
                    .find(|item| item.request_id.as_deref() == Some(request_id.as_str()))
                {
                    if item.status == FeedStatus::Pending {
                        item.status = FeedStatus::Expired;
                    }
                }
                if let Some(item) = state.items.iter().rev().find(|item| item.id == item_id) {
                    publish_feed_bus_event(
                        "feed.item.completed",
                        "feed",
                        "feed.coordinator",
                        feed_event_payload(&item_id, Some(&request_id), item, "timed_out", None),
                    );
                }
                self.changed.notify_all();
                return Ok(json!({ "status": "timed_out", "item_id": item_id }));
            }

            let remaining = deadline.saturating_duration_since(now);
            let (next_state, _) = self
                .changed
                .wait_timeout(state, remaining)
                .map_err(|_| BridgeError::internal("feed coordinator lock poisoned"))?;
            state = next_state;
        }
    }

    pub fn permission_reply(&self, params: &Map<String, Value>) -> Result<Value, BridgeError> {
        let request_id = required_string(params, &["request_id", "requestId"])?;
        let mode = required_string(params, &["mode"])?;
        if !matches!(mode, "once" | "always" | "all" | "bypass" | "deny") {
            return Err(BridgeError::invalid_params(
                "feed.permission.reply mode must be once, always, all, bypass, or deny",
            ));
        }
        self.resolve(request_id, json!({ "kind": "permission", "mode": mode }))
    }

    pub fn question_reply(&self, params: &Map<String, Value>) -> Result<Value, BridgeError> {
        let request_id = required_string(params, &["request_id", "requestId"])?;
        let selections = params
            .get("selections")
            .and_then(Value::as_array)
            .ok_or_else(|| BridgeError::invalid_params("feed.question.reply requires selections"))?
            .iter()
            .map(|value| {
                value.as_str().map(str::to_string).ok_or_else(|| {
                    BridgeError::invalid_params("feed.question.reply selections must be strings")
                })
            })
            .collect::<Result<Vec<_>, _>>()?;
        self.resolve(
            request_id,
            json!({ "kind": "question", "selections": selections }),
        )
    }

    pub fn exit_plan_reply(&self, params: &Map<String, Value>) -> Result<Value, BridgeError> {
        let request_id = required_string(params, &["request_id", "requestId"])?;
        let raw_mode = required_string(params, &["mode"])?;
        let mode = match raw_mode {
            "auto" => "autoAccept",
            "bypass" => "bypassPermissions",
            "ultraplan" | "bypassPermissions" | "autoAccept" | "manual" | "deny" => raw_mode,
            _ => {
                return Err(BridgeError::invalid_params(
                    "feed.exit_plan.reply mode must be ultraplan, bypassPermissions, autoAccept, manual, or deny",
                ))
            }
        };
        let mut decision = json!({ "kind": "exit_plan", "mode": mode });
        if let Some(feedback) = string_field_from_map(params, &["feedback"]) {
            decision["feedback"] = Value::String(feedback.to_string());
        }
        self.resolve(request_id, decision)
    }

    pub fn list(&self, params: &Map<String, Value>) -> Result<Value, BridgeError> {
        let pending_only = params
            .get("pending_only")
            .or_else(|| params.get("pendingOnly"))
            .and_then(Value::as_bool)
            .unwrap_or(false);
        let state = self.lock_state()?;
        let items = state
            .items
            .iter()
            .filter(|item| !pending_only || item.status == FeedStatus::Pending)
            .map(feed_item_row)
            .collect::<Vec<_>>();
        Ok(json!({ "items": items }))
    }

    fn resolve(&self, request_id: &str, decision: Value) -> Result<Value, BridgeError> {
        let mut state = self.lock_state()?;
        let Some(item) = state
            .items
            .iter_mut()
            .rev()
            .find(|item| item.request_id.as_deref() == Some(request_id))
        else {
            return Err(BridgeError::not_found("feed request not found"));
        };
        item.status = FeedStatus::Resolved;
        item.decision = Some(decision);
        publish_feed_bus_event(
            "feed.item.resolved",
            "feed",
            "feed.coordinator",
            feed_event_payload(
                &item.id,
                item.request_id.as_deref(),
                item,
                "resolved",
                item.decision.as_ref(),
            ),
        );
        self.changed.notify_all();
        Ok(json!({ "delivered": true }))
    }

    fn lock_state(&self) -> Result<std::sync::MutexGuard<'_, FeedState>, BridgeError> {
        self.state
            .lock()
            .map_err(|_| BridgeError::internal("feed coordinator lock poisoned"))
    }

    #[cfg(test)]
    pub fn reset_for_tests(&self) {
        let mut state = self.state.lock().expect("feed lock");
        state.next_id = 1;
        state.items.clear();
        self.changed.notify_all();
    }
}

fn event_from_params(params: &Map<String, Value>) -> Result<Value, BridgeError> {
    if let Some(event) = params.get("event") {
        if event.is_object() {
            return Ok(event.clone());
        }
        return Err(BridgeError::invalid_params(
            "feed.push event must be an object",
        ));
    }
    for key in ["session_id", "hook_event_name", "_source"] {
        if !params.contains_key(key) {
            return Err(BridgeError::invalid_params(
                "feed.push requires event or flattened session_id, hook_event_name, and _source",
            ));
        }
    }
    Ok(Value::Object(params.clone()))
}

fn wait_timeout(params: &Map<String, Value>) -> Result<Duration, BridgeError> {
    let Some(value) = params
        .get("wait_timeout_seconds")
        .or_else(|| params.get("waitTimeoutSeconds"))
    else {
        return Ok(Duration::ZERO);
    };
    let seconds = value
        .as_f64()
        .ok_or_else(|| BridgeError::invalid_params("wait_timeout_seconds must be numeric"))?;
    if !seconds.is_finite() || !(0.0..=MAX_WAIT_SECONDS).contains(&seconds) {
        return Err(BridgeError::invalid_params(
            "wait_timeout_seconds must be finite and between 0 and 120",
        ));
    }
    Ok(Duration::from_secs_f64(seconds))
}

fn request_id_from_event(event: &Value) -> Option<String> {
    string_field(event, &["_opencode_request_id", "request_id", "requestId"]).map(str::to_string)
}

fn kind_from_event(event: &Value) -> String {
    match string_field(event, &["hook_event_name"]).unwrap_or("event") {
        "PermissionRequest" => "permissionRequest".to_string(),
        "ExitPlanMode" => "exitPlan".to_string(),
        "AskUserQuestion" => "question".to_string(),
        other => other.to_string(),
    }
}

fn feed_item_row(item: &FeedItem) -> Value {
    let status = status_text(&item.status);
    let mut row = json!({
        "id": item.id,
        "workstream_id": format!("{}-{}", item.source, session_id(&item.event)),
        "source": item.source,
        "kind": item.kind,
        "status": status,
        "created_at": item.created_at_ms,
        "updated_at": item.created_at_ms,
    });
    if let Some(request_id) = &item.request_id {
        row["request_id"] = Value::String(request_id.clone());
    }
    if let Some(decision) = &item.decision {
        row["decision"] = decision.clone();
    }
    if let Some(cwd) = string_field(&item.event, &["cwd"]) {
        row["cwd"] = Value::String(cwd.to_string());
    }
    if let Some(tool_name) = string_field(&item.event, &["tool_name"]) {
        row["tool_name"] = Value::String(tool_name.to_string());
    }
    if let Some(tool_input) = item.event.get("tool_input") {
        row["tool_input"] = tool_input.clone();
    }
    row
}

fn feed_event_payload(
    item_id: &str,
    request_id: Option<&str>,
    item: &FeedItem,
    phase: &str,
    decision: Option<&Value>,
) -> Value {
    let mut payload = json!({
        "item_id": item_id,
        "request_id": request_id,
        "source": item.source,
        "_source": item.source,
        "kind": item.kind,
        "hook_event_name": hook_event_name(&item.event),
        "phase": phase,
        "status": status_text(&item.status),
    });
    if let Some(tool_name) = string_field(&item.event, &["tool_name", "toolName", "name"]) {
        payload["tool_name"] = Value::String(tool_name.to_string());
    }
    if let Some(decision) = decision {
        payload["result"] = decision.clone();
    }
    payload
}

fn publish_agent_hook_event(payload: &Value) {
    let source = payload
        .get("_source")
        .and_then(Value::as_str)
        .unwrap_or("unknown");
    let hook_event_name = payload
        .get("hook_event_name")
        .and_then(Value::as_str)
        .unwrap_or("Unknown");
    publish_feed_bus_event(
        &format!("agent.hook.{hook_event_name}"),
        "agent",
        source,
        payload.clone(),
    );
}

fn hook_event_name(event: &Value) -> Option<String> {
    string_field(event, &["hook_event_name", "hookEventName"]).map(str::to_string)
}

fn publish_feed_bus_event(name: &str, category: &str, source: &str, payload: Value) {
    crate::event_bus::bus().publish(crate::event_bus::EventPublish {
        name,
        category,
        source,
        workspace_id: None,
        surface_id: None,
        pane_id: None,
        payload,
    });
}

fn status_text(status: &FeedStatus) -> &'static str {
    match status {
        FeedStatus::Pending => "pending",
        FeedStatus::Resolved => "resolved",
        FeedStatus::Expired => "expired",
        FeedStatus::Telemetry => "telemetry",
    }
}

fn session_id(event: &Value) -> String {
    string_field(event, &["session_id"])
        .unwrap_or("session")
        .to_string()
}

fn required_string<'a>(
    params: &'a Map<String, Value>,
    keys: &[&str],
) -> Result<&'a str, BridgeError> {
    string_field_from_map(params, keys).ok_or_else(|| {
        BridgeError::invalid_params(format!("required field missing: {}", keys.join("/")))
    })
}

fn string_field_from_map<'a>(params: &'a Map<String, Value>, keys: &[&str]) -> Option<&'a str> {
    keys.iter()
        .filter_map(|key| params.get(*key))
        .find_map(Value::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty())
}

fn string_field<'a>(value: &'a Value, keys: &[&str]) -> Option<&'a str> {
    let map = value.as_object()?;
    string_field_from_map(map, keys)
}

fn now_millis() -> u128 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_millis())
        .unwrap_or(0)
}
