// summary: Store and resolve CMUX-compatible Feed workstream items.
// purpose: Provide feed.push and feed.*.reply backend parity for agent approval workflows.
// inputs: V2 feed socket params containing WorkstreamEvent frames and decision replies.
// returns/effects: Maintains a bounded in-memory feed ring and blocks feed.push until reply or timeout.

use std::collections::VecDeque;
use std::fs::{self, OpenOptions};
use std::io::{self, ErrorKind, Write};
use std::path::{Path, PathBuf};
use std::sync::{Condvar, Mutex, OnceLock};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use serde_json::{json, Map, Value};

use crate::control_bridge::BridgeError;

const MAX_FEED_ITEMS: usize = 2_000;
const MAX_WAIT_SECONDS: f64 = 120.0;
const MAX_WORKSTREAM_LOG_BYTES: u64 = 16 * 1024 * 1024;

static FEED: OnceLock<FeedCoordinator> = OnceLock::new();

pub(crate) fn coordinator() -> &'static FeedCoordinator {
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

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct FeedNotificationRequest {
    pub(crate) workspace_id: Option<String>,
    pub(crate) surface_id: Option<String>,
    pub(crate) title: String,
    pub(crate) body: String,
    pub(crate) actions: Vec<FeedNotificationAction>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct FeedNotificationAction {
    pub(crate) label: String,
    pub(crate) decision: FeedNotificationDecision,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) enum FeedNotificationDecision {
    Permission {
        request_id: String,
        mode: String,
    },
    ExitPlan {
        request_id: String,
        mode: String,
    },
    Question {
        request_id: String,
        selections: Vec<String>,
    },
}

struct FeedPushReceipt {
    item_id: String,
    request_id: Option<String>,
    wait: Duration,
    should_wait: bool,
}

struct FeedIncomingItem {
    event: Value,
    wait: Duration,
    request_id: Option<String>,
    kind: String,
    source: String,
    status: FeedStatus,
    should_wait: bool,
}

pub(crate) struct FeedCoordinator {
    state: Mutex<FeedState>,
    changed: Condvar,
    audit_log_path: Option<PathBuf>,
}

impl FeedCoordinator {
    fn new() -> Self {
        Self::with_audit_log_path(Some(workstream_log_path()))
    }

    // purpose: Build a Feed coordinator with caller-selected audit persistence.
    // inputs: Optional JSONL path for production or isolated tests.
    // returns/effects: Initializes an empty feed ring and decision wakeup condition.
    fn with_audit_log_path(audit_log_path: Option<PathBuf>) -> Self {
        Self {
            state: Mutex::new(FeedState {
                next_id: 1,
                items: VecDeque::new(),
            }),
            changed: Condvar::new(),
            audit_log_path,
        }
    }

    // purpose: Ingest one Feed event and notify the host when it needs attention.
    // inputs: Feed push params plus a synchronous callback for pending actionable rows.
    // returns/effects: Stores the item, invokes callback before waiting, then resolves as normal.
    pub(crate) fn push_with_received_hook<F>(
        &self,
        params: &Map<String, Value>,
        mut on_received: F,
    ) -> Result<Value, BridgeError>
    where
        F: FnMut(&FeedNotificationRequest),
    {
        let receipt = self.store_received_item(params, &mut on_received)?;
        if !receipt.should_wait {
            return self.complete_unblocked_push(&receipt);
        }
        self.wait_for_decision(receipt)
    }

    // purpose: Store one inbound item and emit received/audit/native-notification side effects.
    // inputs: Ingestion params and callback for pending actionable native notifications.
    // returns/effects: Mutates retained Feed state and returns wait metadata for the caller.
    fn store_received_item<F>(
        &self,
        params: &Map<String, Value>,
        on_received: &mut F,
    ) -> Result<FeedPushReceipt, BridgeError>
    where
        F: FnMut(&FeedNotificationRequest),
    {
        let incoming = parse_incoming_item(params)?;
        let (receipt, payload, notification) = self.insert_received_item(incoming)?;
        publish_feed_bus_event(
            "feed.item.received",
            "feed",
            "feed.coordinator",
            payload.clone(),
        );
        self.write_audit_record("feed.item.received", &payload)?;
        publish_agent_hook_event(&payload);
        if let Some(notification) = notification {
            on_received(&notification);
        }
        Ok(receipt)
    }

    // purpose: Insert one parsed Feed item into the retained ring.
    // inputs: Parsed Feed item metadata.
    // returns/effects: Mutates Feed state and returns received event/notification metadata.
    fn insert_received_item(
        &self,
        incoming: FeedIncomingItem,
    ) -> Result<(FeedPushReceipt, Value, Option<FeedNotificationRequest>), BridgeError> {
        let mut state = self.lock_state()?;
        let item_id = format!("feed-{}", state.next_id);
        state.next_id += 1;
        state.items.push_back(FeedItem {
            id: item_id.clone(),
            request_id: incoming.request_id.clone(),
            event: incoming.event,
            source: incoming.source,
            kind: incoming.kind,
            status: incoming.status,
            created_at_ms: now_millis(),
            decision: None,
        });
        while state.items.len() > MAX_FEED_ITEMS {
            state.items.pop_front();
        }
        let event_payload = feed_event_payload(
            &item_id,
            incoming.request_id.as_deref(),
            state.items.back().expect("just pushed feed item"),
            "received",
            None,
        );
        let notification = state.items.back().and_then(feed_notification_request);
        let receipt = FeedPushReceipt {
            item_id,
            request_id: incoming.request_id,
            wait: incoming.wait,
            should_wait: incoming.should_wait,
        };
        Ok((receipt, event_payload, notification))
    }

    // purpose: Finish ingestion that does not block for a user decision.
    // inputs: Stored receipt for telemetry or zero timeout pending rows.
    // returns/effects: Publishes completion and returns acknowledged status.
    fn complete_unblocked_push(&self, receipt: &FeedPushReceipt) -> Result<Value, BridgeError> {
        let state = self.lock_state()?;
        let Some(item) = state
            .items
            .iter()
            .rev()
            .find(|item| item.id == receipt.item_id)
        else {
            return Err(BridgeError::not_found("feed item not found"));
        };
        let completed_payload = feed_event_payload(
            &receipt.item_id,
            receipt.request_id.as_deref(),
            item,
            "acknowledged",
            None,
        );
        publish_feed_bus_event(
            "feed.item.completed",
            "feed",
            "feed.coordinator",
            completed_payload.clone(),
        );
        self.write_audit_record("feed.item.completed", &completed_payload)?;
        Ok(json!({ "status": "acknowledged", "item_id": receipt.item_id }))
    }

    // purpose: Wait for a Feed decision until the request resolves or times out.
    // inputs: Stored Feed receipt with a required request id and timeout duration.
    // returns/effects: Blocks current socket worker and publishes completion on resolve/timeout.
    fn wait_for_decision(&self, receipt: FeedPushReceipt) -> Result<Value, BridgeError> {
        let request_id = receipt.request_id.expect("checked should_wait");
        let deadline = Instant::now() + receipt.wait;
        let mut state = self.lock_state()?;
        loop {
            if let Some(result) =
                self.resolved_push_result(&state, &receipt.item_id, &request_id)?
            {
                return Ok(result);
            }
            let now = Instant::now();
            if now >= deadline {
                return self.expire_pending_push(state, &receipt.item_id, &request_id);
            }
            let remaining = deadline.saturating_duration_since(now);
            let (next_state, _) = self
                .changed
                .wait_timeout(state, remaining)
                .map_err(|_| BridgeError::internal("feed coordinator lock poisoned"))?;
            state = next_state;
        }
    }

    // purpose: Convert resolved/expired Feed state into a socket response when available.
    // inputs: Locked Feed state, item id, and request id.
    // returns/effects: Publishes resolved completion once; otherwise leaves state untouched.
    fn resolved_push_result(
        &self,
        state: &FeedState,
        item_id: &str,
        request_id: &str,
    ) -> Result<Option<Value>, BridgeError> {
        let Some(item) = state.items.iter().rev().find(|item| item.id == item_id) else {
            return Ok(None);
        };
        if item.status == FeedStatus::Expired {
            return Ok(Some(json!({ "status": "timed_out", "item_id": item_id })));
        }
        if item.status != FeedStatus::Resolved {
            return Ok(None);
        }
        let payload = feed_event_payload(
            item_id,
            Some(request_id),
            item,
            "resolved",
            item.decision.as_ref(),
        );
        publish_feed_bus_event(
            "feed.item.completed",
            "feed",
            "feed.coordinator",
            payload.clone(),
        );
        self.write_audit_record("feed.item.completed", &payload)?;
        Ok(Some(json!({
            "status": "resolved",
            "item_id": item_id,
            "decision": item.decision.clone().unwrap_or_else(|| json!({})),
        })))
    }

    // purpose: Mark a pending Feed push expired and return timed-out socket status.
    // inputs: Locked Feed state plus item/request identifiers.
    // returns/effects: Mutates pending item status, publishes completion, and wakes waiters.
    fn expire_pending_push(
        &self,
        mut state: std::sync::MutexGuard<'_, FeedState>,
        item_id: &str,
        request_id: &str,
    ) -> Result<Value, BridgeError> {
        if let Some(item) = state
            .items
            .iter_mut()
            .rev()
            .find(|item| item.request_id.as_deref() == Some(request_id))
        {
            if item.status == FeedStatus::Pending {
                item.status = FeedStatus::Expired;
            }
        }
        if let Some(item) = state.items.iter().rev().find(|item| item.id == item_id) {
            let payload = feed_event_payload(item_id, Some(request_id), item, "timed_out", None);
            publish_feed_bus_event(
                "feed.item.completed",
                "feed",
                "feed.coordinator",
                payload.clone(),
            );
            self.write_audit_record("feed.item.completed", &payload)?;
        }
        self.changed.notify_all();
        Ok(json!({ "status": "timed_out", "item_id": item_id }))
    }

    pub(crate) fn permission_reply(
        &self,
        params: &Map<String, Value>,
    ) -> Result<Value, BridgeError> {
        let request_id = required_string(params, &["request_id", "requestId"])?;
        let mode = required_string(params, &["mode"])?;
        if !matches!(mode, "once" | "always" | "all" | "bypass" | "deny") {
            return Err(BridgeError::invalid_params(
                "feed.permission.reply mode must be once, always, all, bypass, or deny",
            ));
        }
        self.resolve(request_id, json!({ "kind": "permission", "mode": mode }))
    }

    pub(crate) fn question_reply(&self, params: &Map<String, Value>) -> Result<Value, BridgeError> {
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

    pub(crate) fn exit_plan_reply(
        &self,
        params: &Map<String, Value>,
    ) -> Result<Value, BridgeError> {
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

    pub(crate) fn list(&self, params: &Map<String, Value>) -> Result<Value, BridgeError> {
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
        let resolved_payload = feed_event_payload(
            &item.id,
            item.request_id.as_deref(),
            item,
            "resolved",
            item.decision.as_ref(),
        );
        publish_feed_bus_event(
            "feed.item.resolved",
            "feed",
            "feed.coordinator",
            resolved_payload.clone(),
        );
        self.write_audit_record("feed.item.resolved", &resolved_payload)?;
        self.changed.notify_all();
        Ok(json!({ "delivered": true }))
    }

    // purpose: Clear retained Feed items and CMUX-compatible persisted workstream history.
    // inputs: No caller params; the coordinator-owned audit path selects persistent storage.
    // returns/effects: Empties memory, removes workstream JSONL files, and publishes clear metadata.
    pub(crate) fn clear(&self) -> Result<Value, BridgeError> {
        let mut state = self.lock_state()?;
        let cleared_items = state.items.len();
        state.items.clear();
        self.changed.notify_all();
        drop(state);

        let removed_paths = match &self.audit_log_path {
            Some(path) => remove_workstream_logs(path).map_err(|error| {
                BridgeError::internal(format!("feed workstream clear failed: {error}"))
            })?,
            None => Vec::new(),
        };
        let payload = json!({
            "cleared_items": cleared_items,
            "removed_paths": removed_paths,
        });
        publish_feed_bus_event("feed.cleared", "feed", "feed.coordinator", payload.clone());
        Ok(payload)
    }

    fn lock_state(&self) -> Result<std::sync::MutexGuard<'_, FeedState>, BridgeError> {
        self.state
            .lock()
            .map_err(|_| BridgeError::internal("feed coordinator lock poisoned"))
    }

    // purpose: Persist one Feed audit record to the CMUX-compatible JSONL log.
    // inputs: Event name and redacted event-stream payload.
    // returns/effects: Appends to workstream.jsonl when audit persistence is enabled.
    fn write_audit_record(&self, name: &str, payload: &Value) -> Result<(), BridgeError> {
        let Some(path) = &self.audit_log_path else {
            return Ok(());
        };
        let record = json!({
            "name": name,
            "category": "feed",
            "source": "feed.coordinator",
            "occurred_at": now_millis(),
            "payload": payload,
        });
        write_workstream_record(path, &record).map_err(|error| {
            BridgeError::internal(format!("feed workstream audit write failed: {error}"))
        })
    }

    #[cfg(test)]
    pub(crate) fn reset_for_tests(&self) {
        let mut state = self.state.lock().expect("feed lock");
        state.next_id = 1;
        state.items.clear();
        self.changed.notify_all();
    }
}

// purpose: Resolve CMUX-compatible Feed audit log location.
// inputs: Current user home directory from the OS.
// returns/effects: Panics loudly if no home directory is available.
fn workstream_log_path() -> PathBuf {
    dirs::home_dir()
        .map(|home| home.join(".cmuxterm/workstream.jsonl"))
        .expect("home directory is required for CMUX Feed workstream log")
}

// purpose: Append one Feed audit record to a bounded JSONL log.
// inputs: Destination path and JSON record.
// returns/effects: Creates parent directories, rotates at 16 MiB, and writes one line.
fn write_workstream_record(path: &Path, record: &Value) -> io::Result<()> {
    let line = serialize_jsonl_line(record)?;
    rotate_workstream_log_if_needed(path, line.len() as u64)?;
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    let mut file = OpenOptions::new().create(true).append(true).open(path)?;
    file.write_all(line.as_bytes())
}

// purpose: Rotate the Feed workstream log before it exceeds the size cap.
// inputs: Log path and next append length in bytes.
// returns/effects: Renames current log to workstream.jsonl.1, replacing prior rotation.
fn rotate_workstream_log_if_needed(path: &Path, next_len: u64) -> io::Result<()> {
    let Ok(metadata) = fs::metadata(path) else {
        return Ok(());
    };
    if metadata.len().saturating_add(next_len) <= MAX_WORKSTREAM_LOG_BYTES {
        return Ok(());
    }
    let rotated = path.with_file_name("workstream.jsonl.1");
    match fs::remove_file(&rotated) {
        Ok(()) => {}
        Err(error) if error.kind() == ErrorKind::NotFound => {}
        Err(error) => return Err(error),
    }
    fs::rename(path, rotated)
}

// purpose: Remove CMUX Feed persistent history files for a clear operation.
// inputs: Primary workstream JSONL path.
// returns/effects: Deletes the primary log and one bounded rotation, ignoring only missing files.
fn remove_workstream_logs(path: &Path) -> io::Result<Vec<String>> {
    let mut removed = Vec::new();
    for candidate in [
        path.to_path_buf(),
        path.with_file_name("workstream.jsonl.1"),
    ] {
        match fs::remove_file(&candidate) {
            Ok(()) => removed.push(candidate.display().to_string()),
            Err(error) if error.kind() == ErrorKind::NotFound => {}
            Err(error) => return Err(error),
        }
    }
    Ok(removed)
}

// purpose: Serialize a JSON value as one newline-terminated JSONL record.
// inputs: Record value.
// returns/effects: Returns an io error if JSON serialization fails.
fn serialize_jsonl_line(record: &Value) -> io::Result<String> {
    let mut line = serde_json::to_string(record)
        .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error.to_string()))?;
    line.push('\n');
    Ok(line)
}

// purpose: Parse Feed push params into normalized retained item metadata.
// inputs: Feed push params containing an event object or flattened event fields.
// returns/effects: Returns parsed metadata and wait behavior without mutating state.
fn parse_incoming_item(params: &Map<String, Value>) -> Result<FeedIncomingItem, BridgeError> {
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
    Ok(FeedIncomingItem {
        event,
        wait,
        request_id,
        kind,
        source,
        status,
        should_wait,
    })
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

// purpose: Build a host notification request for pending actionable Feed rows.
// inputs: One retained Feed item.
// returns/effects: Returns None for telemetry/resolved rows that do not need user attention.
fn feed_notification_request(item: &FeedItem) -> Option<FeedNotificationRequest> {
    if item.status != FeedStatus::Pending {
        return None;
    }
    Some(FeedNotificationRequest {
        workspace_id: string_field(&item.event, &["workspace_id", "workspaceId", "workspace"])
            .map(str::to_string),
        surface_id: string_field(&item.event, &["surface_id", "surfaceId", "surface"])
            .map(str::to_string),
        title: feed_notification_title(item),
        body: feed_notification_body(item),
        actions: feed_notification_actions(item),
    })
}

// purpose: Build inline native actions for pending Feed decision rows.
// inputs: Pending Feed item with request id and kind-specific payload.
// returns/effects: Returns valid decision actions or an empty list when no action is possible.
fn feed_notification_actions(item: &FeedItem) -> Vec<FeedNotificationAction> {
    let Some(request_id) = &item.request_id else {
        return Vec::new();
    };
    match item.kind.as_str() {
        "permissionRequest" | "PermissionRequest" => feed_notification_mode_actions(
            request_id,
            &crate::feed_actions::permission_action_specs(&item.source, &item.event),
            |request_id, mode| FeedNotificationDecision::Permission { request_id, mode },
        ),
        "exitPlan" | "ExitPlanMode" => feed_notification_mode_actions(
            request_id,
            &[
                ("Manual", "manual"),
                ("Auto", "autoAccept"),
                ("Bypass", "bypassPermissions"),
                ("Ultraplan", "ultraplan"),
                ("Deny", "deny"),
            ],
            |request_id, mode| FeedNotificationDecision::ExitPlan { request_id, mode },
        ),
        "question" | "AskUserQuestion" => {
            feed_notification_question_actions(request_id, &item.event)
        }
        _ => Vec::new(),
    }
}

// purpose: Build inline actions for permission-like mode decisions.
// inputs: Request id, label/mode pairs, and a decision constructor.
// returns/effects: Returns notification actions without mutating Feed state.
fn feed_notification_mode_actions<F>(
    request_id: &str,
    modes: &[(&str, &str)],
    decision: F,
) -> Vec<FeedNotificationAction>
where
    F: Fn(String, String) -> FeedNotificationDecision,
{
    modes
        .iter()
        .map(|(label, mode)| FeedNotificationAction {
            label: (*label).to_string(),
            decision: decision(request_id.to_string(), (*mode).to_string()),
        })
        .collect()
}

// purpose: Build inline actions for CMUX/Claude question options.
// inputs: Request id and Feed event with optional question metadata.
// returns/effects: Returns up to six direct choices or a multi-question default action.
fn feed_notification_question_actions(
    request_id: &str,
    event: &Value,
) -> Vec<FeedNotificationAction> {
    let questions = feed_question_option_groups(event);
    if questions.len() > 1 {
        let selections = questions
            .iter()
            .map(|question| question.first().cloned().unwrap_or_default())
            .collect::<Vec<_>>();
        return vec![FeedNotificationAction {
            label: "Default".to_string(),
            decision: FeedNotificationDecision::Question {
                request_id: request_id.to_string(),
                selections,
            },
        }];
    }
    questions
        .first()
        .into_iter()
        .flat_map(|options| options.iter())
        .take(6)
        .filter(|option| !option.trim().is_empty())
        .map(|option| FeedNotificationAction {
            label: option.clone(),
            decision: FeedNotificationDecision::Question {
                request_id: request_id.to_string(),
                selections: vec![option.clone()],
            },
        })
        .collect()
}

// purpose: Format the native notification title for one pending Feed row.
// inputs: Pending Feed item with source and kind metadata.
// returns/effects: Returns a short user-facing title.
fn feed_notification_title(item: &FeedItem) -> String {
    let source = title_case_source(&item.source);
    match item.kind.as_str() {
        "permissionRequest" | "PermissionRequest" => format!("{source} needs approval"),
        "exitPlan" | "ExitPlanMode" => format!("{source} wants plan approval"),
        "question" | "AskUserQuestion" => format!("{source} has a question"),
        _ => format!("{source} needs attention"),
    }
}

// purpose: Format native notification body text for one pending Feed row.
// inputs: Pending Feed item with optional tool name and question prompt.
// returns/effects: Returns compact non-empty body text.
fn feed_notification_body(item: &FeedItem) -> String {
    if matches!(item.kind.as_str(), "question" | "AskUserQuestion") {
        if let Some(question) = first_question_prompt(&item.event) {
            return question;
        }
    }
    let tool = string_field(&item.event, &["tool_name", "toolName", "name"]);
    match tool {
        Some(tool) => format!("{}: {tool}", feed_notification_kind_label(&item.kind)),
        None => feed_notification_kind_label(&item.kind).to_string(),
    }
}

// purpose: Convert normalized Feed kinds into stable notification labels.
// inputs: Feed item kind.
// returns/effects: Returns a display label without mutating state.
fn feed_notification_kind_label(kind: &str) -> &'static str {
    match kind {
        "permissionRequest" | "PermissionRequest" => "PermissionRequest",
        "exitPlan" | "ExitPlanMode" => "ExitPlanMode",
        "question" | "AskUserQuestion" => "AskUserQuestion",
        _ => "Feed",
    }
}

// purpose: Extract the first question prompt from CMUX/Claude question payloads.
// inputs: Feed event with possible tool_input.questions array.
// returns/effects: Returns a trimmed question string when present.
fn first_question_prompt(event: &Value) -> Option<String> {
    let input = event.get("tool_input").or_else(|| event.get("toolInput"))?;
    let question = input
        .get("questions")
        .and_then(Value::as_array)
        .and_then(|questions| questions.first())
        .and_then(|question| string_field(question, &["question", "prompt", "text"]))?;
    let question = question.trim();
    (!question.is_empty()).then(|| question.to_string())
}

// purpose: Parse question option groups from CMUX/Claude question payloads.
// inputs: Feed event with possible top-level options or tool_input.questions arrays.
// returns/effects: Returns user-visible option labels without mutating state.
fn feed_question_option_groups(event: &Value) -> Vec<Vec<String>> {
    let Some(input) = event.get("tool_input").or_else(|| event.get("toolInput")) else {
        let options = feed_question_options(event);
        return (!options.is_empty())
            .then_some(vec![options])
            .unwrap_or_default();
    };
    if let Some(questions) = input.get("questions").and_then(Value::as_array) {
        return questions
            .iter()
            .map(feed_question_options)
            .filter(|options| !options.is_empty())
            .collect();
    }
    let options = feed_question_options(input);
    (!options.is_empty())
        .then_some(vec![options])
        .unwrap_or_default()
}

// purpose: Parse one set of question option labels from a JSON object.
// inputs: JSON object with options or choices array.
// returns/effects: Returns string labels in source order.
fn feed_question_options(value: &Value) -> Vec<String> {
    value
        .get("options")
        .or_else(|| value.get("choices"))
        .and_then(Value::as_array)
        .map(|options| {
            options
                .iter()
                .filter_map(Value::as_str)
                .map(str::to_string)
                .collect()
        })
        .unwrap_or_default()
}

// purpose: Convert an agent source token into concise title-case display text.
// inputs: Lowercase or kebab-case Feed source.
// returns/effects: Returns stable labels for known sources and generic title case otherwise.
fn title_case_source(source: &str) -> String {
    match source {
        "codex" => "Codex".to_string(),
        "claude" => "Claude".to_string(),
        "opencode" => "OpenCode".to_string(),
        "hermes-agent" => "Hermes Agent".to_string(),
        _ => source
            .split(['-', '_'])
            .filter(|part| !part.is_empty())
            .map(|part| {
                let mut chars = part.chars();
                match chars.next() {
                    Some(first) => format!("{}{}", first.to_ascii_uppercase(), chars.as_str()),
                    None => String::new(),
                }
            })
            .collect::<Vec<_>>()
            .join(" "),
    }
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

#[cfg(test)]
mod tests {
    use super::*;

    fn feed_push_params() -> Map<String, Value> {
        let mut params = Map::new();
        params.insert(
            "event".to_string(),
            json!({
                "session_id": "session-a",
                "hook_event_name": "PostToolUse",
                "_source": "codex",
                "tool_name": "shell"
            }),
        );
        params
    }

    #[test]
    fn push_writes_workstream_jsonl_audit_records() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("workstream.jsonl");
        let feed = FeedCoordinator::with_audit_log_path(Some(path.clone()));

        let result = feed
            .push_with_received_hook(&feed_push_params(), |_| {})
            .expect("push feed item");

        assert_eq!(result["status"], "acknowledged");
        let text = fs::read_to_string(path).expect("read workstream log");
        assert!(text.contains("\"name\":\"feed.item.received\""));
        assert!(text.contains("\"name\":\"feed.item.completed\""));
    }

    #[test]
    fn push_notifies_for_pending_actionable_feed_items() {
        let feed = FeedCoordinator::with_audit_log_path(None);
        let mut params = Map::new();
        params.insert(
            "event".to_string(),
            json!({
                "session_id": "session-a",
                "hook_event_name": "PermissionRequest",
                "_source": "codex",
                "_opencode_request_id": "request-a",
                "workspace_id": "workspace-a",
                "surface_id": "7:tab-a",
                "tool_name": "shell"
            }),
        );

        let mut notifications = Vec::new();
        let result = feed
            .push_with_received_hook(&params, |notification| {
                notifications.push(notification.clone());
            })
            .expect("push pending feed item");

        assert_eq!(result["status"], "acknowledged");
        assert_eq!(
            notifications,
            vec![FeedNotificationRequest {
                workspace_id: Some("workspace-a".to_string()),
                surface_id: Some("7:tab-a".to_string()),
                title: "Codex needs approval".to_string(),
                body: "PermissionRequest: shell".to_string(),
                actions: vec![
                    FeedNotificationAction {
                        label: "Once".to_string(),
                        decision: FeedNotificationDecision::Permission {
                            request_id: "request-a".to_string(),
                            mode: "once".to_string(),
                        },
                    },
                    FeedNotificationAction {
                        label: "Always".to_string(),
                        decision: FeedNotificationDecision::Permission {
                            request_id: "request-a".to_string(),
                            mode: "always".to_string(),
                        },
                    },
                    FeedNotificationAction {
                        label: "Bypass".to_string(),
                        decision: FeedNotificationDecision::Permission {
                            request_id: "request-a".to_string(),
                            mode: "bypass".to_string(),
                        },
                    },
                    FeedNotificationAction {
                        label: "Deny".to_string(),
                        decision: FeedNotificationDecision::Permission {
                            request_id: "request-a".to_string(),
                            mode: "deny".to_string(),
                        },
                    },
                ],
            }]
        );
    }

    // purpose: Verify native notification actions honor Codex app-server approval capabilities.
    // inputs: Pending Codex app-server Feed permission row with only amendment and decline decisions.
    // returns/effects: Asserts notification buttons expose `all` and `deny`, not unsupported modes.
    #[test]
    fn push_notifies_codex_app_server_permission_with_supported_actions() {
        let feed = FeedCoordinator::with_audit_log_path(None);
        let mut params = Map::new();
        params.insert(
            "event".to_string(),
            json!({
                "session_id": "codex-thread-1",
                "hook_event_name": "PermissionRequest",
                "_source": "codex",
                "_opencode_request_id": "codex-app-server-approval-1",
                "workspace_id": "workspace-a",
                "tool_name": "Bash",
                "tool_input": {
                    "app_server_method": "item/commandExecution/requestApproval",
                    "available_decisions": [{"acceptWithExecpolicyAmendment": {}}, "decline"],
                    "proposed_execpolicy_amendment": [{"kind": "prefix", "value": "cargo test"}]
                }
            }),
        );

        let mut notifications = Vec::new();
        feed.push_with_received_hook(&params, |notification| {
            notifications.push(notification.clone());
        })
        .expect("push pending app-server feed item");

        assert_eq!(notifications.len(), 1);
        assert_eq!(
            notifications[0].actions,
            vec![
                FeedNotificationAction {
                    label: "All".to_string(),
                    decision: FeedNotificationDecision::Permission {
                        request_id: "codex-app-server-approval-1".to_string(),
                        mode: "all".to_string(),
                    },
                },
                FeedNotificationAction {
                    label: "Deny".to_string(),
                    decision: FeedNotificationDecision::Permission {
                        request_id: "codex-app-server-approval-1".to_string(),
                        mode: "deny".to_string(),
                    },
                },
            ]
        );
    }

    #[test]
    fn push_does_not_notify_for_telemetry_feed_items() {
        let feed = FeedCoordinator::with_audit_log_path(None);
        let mut notifications = Vec::new();

        let result = feed
            .push_with_received_hook(&feed_push_params(), |notification| {
                notifications.push(notification.clone());
            })
            .expect("push telemetry feed item");

        assert_eq!(result["status"], "acknowledged");
        assert!(notifications.is_empty());
    }

    #[test]
    fn workstream_jsonl_rotates_at_size_cap() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("workstream.jsonl");
        fs::write(&path, "x".repeat(MAX_WORKSTREAM_LOG_BYTES as usize)).expect("write log");

        write_workstream_record(&path, &json!({ "name": "feed.item.received" }))
            .expect("append record");

        assert!(dir.path().join("workstream.jsonl.1").exists());
        let text = fs::read_to_string(path).expect("read new log");
        assert!(text.contains("\"name\":\"feed.item.received\""));
    }

    #[test]
    fn clear_removes_retained_items_and_workstream_logs() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("workstream.jsonl");
        let rotated = dir.path().join("workstream.jsonl.1");
        fs::write(&path, "{}\n").expect("seed log");
        fs::write(&rotated, "{}\n").expect("seed rotated log");

        let feed = FeedCoordinator::with_audit_log_path(Some(path.clone()));
        feed.push_with_received_hook(&feed_push_params(), |_| {})
            .expect("push feed item");
        let result = feed.clear().expect("clear feed");

        assert_eq!(result["cleared_items"], 1);
        assert!(!path.exists());
        assert!(!rotated.exists());
        let listed = feed.list(&Map::new()).expect("list cleared feed");
        assert_eq!(listed["items"].as_array().expect("items").len(), 0);
    }
}
