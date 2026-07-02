// summary: Provide a CMUX-compatible retained host event stream.
// purpose: Publish, replay, filter, and stream local host events for CLI and automation observers.
// inputs: Event publications from host subsystems and events.stream socket request params.
// returns/effects: Maintains a bounded in-memory replay ring and writes JSONL stream frames.

use std::collections::VecDeque;
use std::io::{self, Write};
use std::os::unix::net::UnixStream;
use std::sync::{Condvar, Mutex, OnceLock};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use serde_json::{json, Value};

const MAX_EVENTS: usize = 4_096;
const HEARTBEAT_INTERVAL: Duration = Duration::from_secs(15);

static EVENT_BUS: OnceLock<EventBus> = OnceLock::new();
static BOOT_ID: OnceLock<String> = OnceLock::new();

// purpose: Return the process-wide retained event bus.
// inputs: No caller input.
// returns/effects: Lazily initializes the bus once and returns a static handle.
pub fn bus() -> &'static EventBus {
    EVENT_BUS.get_or_init(EventBus::new)
}

fn boot_id() -> &'static str {
    BOOT_ID.get_or_init(|| format!("limux-{}-{}", std::process::id(), now_millis()))
}

// purpose: Read the current wall clock as milliseconds since the Unix epoch.
// inputs: System clock.
// returns/effects: Returns zero if system time is before the epoch.
fn now_millis() -> u128 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_millis())
        .unwrap_or(0)
}

fn occurred_at() -> String {
    format!("unix_ms:{}", now_millis())
}

#[derive(Clone, Debug)]
struct EventRecord {
    frame: Value,
    seq: u64,
    name: String,
    category: String,
}

#[derive(Clone, Debug)]
struct EventFilters {
    after_seq: u64,
    names: Vec<String>,
    categories: Vec<String>,
    include_heartbeats: bool,
}

struct EventState {
    next_seq: u64,
    events: VecDeque<EventRecord>,
}

pub struct EventBus {
    state: Mutex<EventState>,
    changed: Condvar,
}

pub struct EventPublish<'a> {
    pub name: &'a str,
    pub category: &'a str,
    pub source: &'a str,
    pub workspace_id: Option<Value>,
    pub surface_id: Option<Value>,
    pub pane_id: Option<Value>,
    pub payload: Value,
}

impl EventBus {
    /// purpose: Build an empty event bus.
    /// inputs: No caller input.
    /// returns/effects: Initializes sequence one and an empty replay ring.
    fn new() -> Self {
        Self {
            state: Mutex::new(EventState {
                next_seq: 1,
                events: VecDeque::new(),
            }),
            changed: Condvar::new(),
        }
    }

    // purpose: Publish one retained CMUX event frame.
    // inputs: Event identity, optional object ids, and event-specific payload.
    // returns/effects: Appends to the bounded replay ring and wakes stream subscribers.
    pub fn publish(&self, event: EventPublish<'_>) -> u64 {
        let mut state = self.state.lock().expect("event bus lock");
        let seq = state.next_seq;
        state.next_seq = state.next_seq.saturating_add(1);
        let frame = json!({
            "type": "event",
            "protocol": "cmux-events",
            "version": 1,
            "boot_id": boot_id(),
            "seq": seq,
            "id": format!("{}-{}", boot_id(), seq),
            "name": event.name,
            "category": event.category,
            "source": event.source,
            "occurred_at": occurred_at(),
            "workspace_id": event.workspace_id.unwrap_or(Value::Null),
            "surface_id": event.surface_id.unwrap_or(Value::Null),
            "pane_id": event.pane_id.unwrap_or(Value::Null),
            "window_id": Value::Null,
            "payload": event.payload,
        });
        state.events.push_back(EventRecord {
            frame,
            seq,
            name: event.name.to_string(),
            category: event.category.to_string(),
        });
        while state.events.len() > MAX_EVENTS {
            state.events.pop_front();
        }
        self.changed.notify_all();
        seq
    }

    // purpose: Stream replay, live events, and optional heartbeats over a takeover socket.
    // inputs: Event stream params and a Unix socket writer.
    // returns/effects: Writes JSONL frames until the client disconnects or a write fails.
    pub fn stream(&self, params: &Value, writer: &mut UnixStream) -> io::Result<()> {
        let filters = EventFilters::from_params(params);
        let (ack, replay) = self.ack_and_replay(&filters);
        write_frame(writer, &ack)?;
        for event in &replay {
            write_frame(writer, &event.frame)?;
        }

        let mut cursor = replay
            .last()
            .map(|event| event.seq)
            .unwrap_or(filters.after_seq);
        loop {
            let (events, latest_seq) = self.wait_for_events(cursor, &filters)?;
            if events.is_empty() && filters.include_heartbeats {
                write_frame(writer, &heartbeat_frame(latest_seq))?;
            }
            if events.is_empty() {
                continue;
            }
            for event in &events {
                cursor = event.seq;
                write_frame(writer, &event.frame)?;
            }
        }
    }

    // purpose: Build stream ack metadata and retained replay frames for a subscription.
    // inputs: Event filters parsed from events.stream params.
    // returns/effects: Returns ack JSON plus matching retained events without mutating state.
    fn ack_and_replay(&self, filters: &EventFilters) -> (Value, Vec<EventRecord>) {
        let state = self.state.lock().expect("event bus lock");
        let oldest_seq = state.events.front().map(|event| event.seq).unwrap_or(0);
        let latest_seq = state.events.back().map(|event| event.seq).unwrap_or(0);
        let replay = state
            .events
            .iter()
            .filter(|event| event.seq > filters.after_seq && filters.matches(event))
            .cloned()
            .collect::<Vec<_>>();
        let gap = filters.after_seq > 0
            && (filters.after_seq < oldest_seq || filters.after_seq > latest_seq);
        let ack = json!({
            "type": "ack",
            "protocol": "cmux-events",
            "version": 1,
            "boot_id": boot_id(),
            "subscription_id": subscription_id(),
            "heartbeat_interval_seconds": HEARTBEAT_INTERVAL.as_secs(),
            "replay_count": replay.len(),
            "resume": {
                "after_seq": filters.after_seq,
                "requested_after_seq": filters.after_seq,
                "oldest_seq": oldest_seq,
                "latest_seq": latest_seq,
                "next_seq": state.next_seq,
                "gap": gap
            },
            "filters": {
                "names": filters.names,
                "categories": filters.categories
            }
        });
        (ack, replay)
    }

    // purpose: Wait until matching live events arrive or the heartbeat interval expires.
    // inputs: Last delivered sequence and active subscription filters.
    // returns/effects: Returns matching events and the latest known sequence.
    fn wait_for_events(
        &self,
        cursor: u64,
        filters: &EventFilters,
    ) -> io::Result<(Vec<EventRecord>, u64)> {
        let mut state = self
            .state
            .lock()
            .map_err(|_| io::Error::other("event bus lock poisoned"))?;
        let deadline = Instant::now() + HEARTBEAT_INTERVAL;
        loop {
            let events = state
                .events
                .iter()
                .filter(|event| event.seq > cursor && filters.matches(event))
                .cloned()
                .collect::<Vec<_>>();
            let latest_seq = state.events.back().map(|event| event.seq).unwrap_or(0);
            if !events.is_empty() || Instant::now() >= deadline {
                return Ok((events, latest_seq));
            }
            let remaining = deadline.saturating_duration_since(Instant::now());
            let (next_state, _) = self
                .changed
                .wait_timeout(state, remaining)
                .map_err(|_| io::Error::other("event bus lock poisoned"))?;
            state = next_state;
        }
    }
}

impl EventFilters {
    /// purpose: Normalize CMUX event stream filter fields from a v2 request.
    /// inputs: Request params that may contain singular or array filter aliases.
    /// returns/effects: Returns strict filters and heartbeat preference.
    fn from_params(params: &Value) -> Self {
        Self {
            after_seq: params
                .get("after_seq")
                .or_else(|| params.get("after"))
                .and_then(Value::as_u64)
                .unwrap_or(0),
            names: filter_strings(params, "name", "names"),
            categories: filter_strings(params, "category", "categories"),
            include_heartbeats: params
                .get("include_heartbeats")
                .and_then(Value::as_bool)
                .unwrap_or(true),
        }
    }

    // purpose: Check whether an event satisfies this subscription.
    // inputs: Retained event record.
    // returns/effects: Returns true when name and category filters match.
    fn matches(&self, event: &EventRecord) -> bool {
        (self.names.is_empty() || self.names.iter().any(|name| name == &event.name))
            && (self.categories.is_empty()
                || self
                    .categories
                    .iter()
                    .any(|category| category == &event.category))
    }
}

// purpose: Normalize one singular/plural filter pair into a flat string list.
// inputs: JSON params plus singular and plural field names.
// returns/effects: Ignores invalid values and returns only non-empty strings.
fn filter_strings(params: &Value, singular: &str, plural: &str) -> Vec<String> {
    /// purpose: Append valid filter strings from a scalar or array JSON value.
    /// inputs: Mutable output list and one JSON filter value.
    /// returns/effects: Mutates out with non-empty string leaves.
    fn push_value(out: &mut Vec<String>, value: &Value) {
        match value {
            Value::String(text) if !text.is_empty() => out.push(text.clone()),
            Value::Array(items) => {
                for item in items {
                    push_value(out, item);
                }
            }
            _ => {}
        }
    }

    let mut values = Vec::new();
    if let Some(value) = params.get(plural) {
        push_value(&mut values, value);
    }
    if let Some(value) = params.get(singular) {
        push_value(&mut values, value);
    }
    values
}

// purpose: Build one CMUX heartbeat frame for a subscription.
// inputs: Latest retained event sequence known to the bus.
// returns/effects: Returns JSON without mutating event state.
fn heartbeat_frame(latest_seq: u64) -> Value {
    json!({
        "type": "heartbeat",
        "protocol": "cmux-events",
        "version": 1,
        "boot_id": boot_id(),
        "subscription_id": subscription_id(),
        "latest_seq": latest_seq,
        "occurred_at": occurred_at(),
    })
}

fn subscription_id() -> String {
    format!("{}-sub-{}", boot_id(), now_millis())
}

// purpose: Write one JSON event-stream frame to the takeover socket.
// inputs: Unix socket writer and JSON frame.
// returns/effects: Serializes as one newline-terminated JSON object.
fn write_frame(writer: &mut UnixStream, frame: &Value) -> io::Result<()> {
    let mut payload = serde_json::to_string(frame)
        .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error.to_string()))?;
    payload.push('\n');
    writer.write_all(payload.as_bytes())?;
    writer.flush()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    /// purpose: Verify retained replay honors category filters.
    /// inputs: Test event bus with one notification event and one Feed event.
    /// returns/effects: Asserts only the matching Feed event is replayed.
    fn ack_replays_matching_events_after_cursor() {
        let bus = EventBus::new();
        bus.publish(EventPublish {
            name: "notification.created",
            category: "notification",
            source: "test",
            workspace_id: Some(Value::String("workspace-a".to_string())),
            surface_id: None,
            pane_id: None,
            payload: json!({ "notification_id": 1 }),
        });
        bus.publish(EventPublish {
            name: "feed.item.received",
            category: "feed",
            source: "test",
            workspace_id: None,
            surface_id: None,
            pane_id: None,
            payload: json!({ "request_id": "req" }),
        });

        let filters = EventFilters::from_params(&json!({ "category": "feed" }));
        let (ack, replay) = bus.ack_and_replay(&filters);

        assert_eq!(ack["replay_count"], 1);
        assert_eq!(replay[0].name, "feed.item.received");
    }

    #[test]
    /// purpose: Verify replay gap metadata detects cursors older than the ring.
    /// inputs: Test event bus filled beyond the retained event limit.
    /// returns/effects: Asserts the ack reports a resume gap.
    fn ack_marks_old_cursor_gap() {
        let bus = EventBus::new();
        for idx in 0..(MAX_EVENTS + 2) {
            bus.publish(EventPublish {
                name: "workspace.selected",
                category: "workspace",
                source: "test",
                workspace_id: Some(Value::String(format!("workspace-{idx}"))),
                surface_id: None,
                pane_id: None,
                payload: json!({}),
            });
        }

        let filters = EventFilters::from_params(&json!({ "after_seq": 1 }));
        let (ack, _) = bus.ack_and_replay(&filters);

        assert_eq!(ack["resume"]["gap"], true);
    }
}
