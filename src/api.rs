//! JSON-RPC API served over the secondary Unix socket (`spectra-api.sock`).
//!
//! The wire format is newline-delimited JSON: one request object per line,
//! one response object per line.
//!
//! - Request: `{"id": <number|string>, "method": "<name>", "params": {...}?}`
//! - Success: `{"id": ..., "result": ...}`
//! - Error: `{"id": ..., "error": {"code": <int>, "message": "..."}}`
//! - Event push (after `events.subscribe`): `{"event": "<name>", "params": {...}}`
//!
//! This module stays a thin JSON adapter: it translates requests into small,
//! purposeful [`App`] methods and never contains business logic itself.

use serde::Serialize;
use serde_json::{Map, Value, json};

use crate::app::App;

pub const PARSE_ERROR: i64 = -32700;
pub const METHOD_NOT_FOUND: i64 = -32601;
pub const INVALID_PARAMS: i64 = -32602;
pub const INTERNAL_ERROR: i64 = -32603;
pub const PANE_NOT_FOUND: i64 = -32000;

/// Event names a connection can subscribe to via `events.subscribe`.
pub const EVENT_NAMES: [&str; 7] = [
    "session.created",
    "session.killed",
    "window.created",
    "pane.split",
    "pane.closed",
    "config.reloaded",
    "agent.changed",
];

/// Maximum length of an externally reported agent kind after sanitization.
const AGENT_KIND_MAX_CHARS: usize = 32;

/// One entry of the `session.list` result.
#[derive(Debug, Clone, Serialize)]
pub struct SessionInfo {
    pub session_id: String,
    pub name: String,
    pub ordinal: usize,
    pub active: bool,
    pub windows: usize,
}

/// One entry of the `pane.list` result.
#[derive(Debug, Clone, Serialize)]
pub struct PaneInfo {
    pub pane_id: usize,
    pub session_id: String,
    pub window: usize,
    pub focused: bool,
    pub title: Option<String>,
    /// Detected AI agent in the pane, `null` when none.
    pub agent: Option<AgentInfo>,
}

/// Detected AI-agent status of a pane (`pane.list` `agent` field).
#[derive(Debug, Clone, Serialize)]
pub struct AgentInfo {
    /// Agent kind, e.g. `"claude"`.
    pub kind: String,
    /// Derived display state:
    /// `"unknown" | "idle" | "done" | "working" | "blocked"`.
    /// `"done"` = idle and the pane has not been viewed since it went idle.
    pub state: String,
}

/// One server-pushed event line for subscribed API connections.
#[derive(Debug, Clone)]
pub struct ApiEvent {
    /// Event name, one of [`EVENT_NAMES`].
    pub name: String,
    /// Event payload, serialized as the `params` field.
    pub params: Value,
}

impl ApiEvent {
    /// Wire line for subscribed connections (without the trailing newline).
    pub fn event_line(&self) -> String {
        json!({ "event": self.name, "params": self.params }).to_string()
    }
}

/// Per-connection event filter created by `events.subscribe`.
#[derive(Debug, Clone, Default)]
pub struct EventSubscription {
    /// Requested event names; `None` = all events. Unknown names are kept
    /// for forward compatibility but never match anything today.
    events: Option<Vec<String>>,
}

impl EventSubscription {
    pub fn matches(&self, name: &str) -> bool {
        self.events
            .as_ref()
            .is_none_or(|events| events.iter().any(|event| event == name))
    }
}

/// Result of handling one request line.
pub struct DispatchOutcome {
    /// Response line (without the trailing newline).
    pub response: String,
    /// Set when this request subscribed the connection to event pushes.
    pub subscription: Option<EventSubscription>,
}

type MethodError = (i64, String);

/// Handle one JSON-RPC request line.
///
/// Dispatch takes `&mut App` because the API deliberately includes write
/// methods (`pane.send_keys`, `pane.split`, `agent.report`); the original
/// read-only-by-type guarantee (`&App`) was superseded by that decision.
/// The mutations happen inside dedicated `App` methods that reuse the same
/// internal paths as the interactive/CLI surfaces.
pub fn dispatch(app: &mut App, request: &str) -> DispatchOutcome {
    let parsed: Value = match serde_json::from_str(request) {
        Ok(value) => value,
        Err(err) => {
            return DispatchOutcome {
                response: error_response(Value::Null, PARSE_ERROR, &format!("parse error: {err}")),
                subscription: None,
            };
        }
    };
    let id = parsed.get("id").cloned().unwrap_or(Value::Null);
    let Some(method) = parsed.get("method").and_then(Value::as_str) else {
        return DispatchOutcome {
            response: error_response(id, INVALID_PARAMS, "request has no string \"method\" field"),
            subscription: None,
        };
    };

    let mut subscription = None;
    let response = match handle_method(app, method, parsed.get("params"), &mut subscription) {
        Ok(result) => json!({ "id": id, "result": result }).to_string(),
        Err((code, message)) => error_response(id, code, &message),
    };
    DispatchOutcome {
        response,
        subscription,
    }
}

fn handle_method(
    app: &mut App,
    method: &str,
    params: Option<&Value>,
    subscription: &mut Option<EventSubscription>,
) -> Result<Value, MethodError> {
    match method {
        "session.list" => session_list(app),
        "pane.list" => pane_list(app, params),
        "pane.read" => pane_read(app, params),
        "pane.send_keys" => pane_send_keys(app, params),
        "pane.split" => pane_split(app, params),
        "agent.report" => agent_report(app, params),
        "events.subscribe" => {
            let (result, accepted) = events_subscribe(params)?;
            *subscription = Some(accepted);
            Ok(result)
        }
        _ => Err((METHOD_NOT_FOUND, format!("method not found: {method}"))),
    }
}

fn session_list(app: &App) -> Result<Value, MethodError> {
    serde_json::to_value(app.api_sessions()).map_err(internal_error)
}

fn pane_list(app: &App, params: Option<&Value>) -> Result<Value, MethodError> {
    let params = params_object(params)?;
    let session_id = optional_str_param(params, "session_id")?;
    serde_json::to_value(app.api_panes(session_id)).map_err(internal_error)
}

fn pane_read(app: &App, params: Option<&Value>) -> Result<Value, MethodError> {
    let params = params_object(params)?;
    let Some(pane_id) = optional_usize_param(params, "pane_id")? else {
        return Err(invalid_params("pane_id is required"));
    };
    let session_id = optional_str_param(params, "session_id")?;
    let lines = optional_usize_param(params, "lines")?;

    match app.api_pane_read(pane_id, session_id, lines) {
        Some(text) => Ok(json!({ "text": text })),
        None => Err((PANE_NOT_FOUND, "pane not found".to_string())),
    }
}

/// `pane.send_keys`: write `text` bytes verbatim to one pane's PTY (raw,
/// no key encoding — same semantics as the CLI `send-keys` command).
fn pane_send_keys(app: &mut App, params: Option<&Value>) -> Result<Value, MethodError> {
    let params = params_object(params)?;
    let Some(pane_id) = optional_usize_param(params, "pane_id")? else {
        return Err(invalid_params("pane_id is required"));
    };
    let session_id = optional_str_param(params, "session_id")?;
    let Some(text) = optional_str_param(params, "text")? else {
        return Err(invalid_params("text is required"));
    };
    if text.is_empty() {
        return Err(invalid_params("text cannot be empty"));
    }

    app.api_send_keys(pane_id, session_id, text)
        .map_err(|message| (PANE_NOT_FOUND, message))?;
    Ok(json!({ "ok": true }))
}

/// `pane.split`: focus the target pane (default: the currently focused
/// pane), split it, and return the new pane id.
fn pane_split(app: &mut App, params: Option<&Value>) -> Result<Value, MethodError> {
    let params = params_object(params)?;
    let pane_id = optional_usize_param(params, "pane_id")?;
    let session_id = optional_str_param(params, "session_id")?;
    let axis = match optional_str_param(params, "axis")? {
        Some("horizontal") => crate::ui::window_manager::SplitAxis::Horizontal,
        Some("vertical") => crate::ui::window_manager::SplitAxis::Vertical,
        Some(other) => {
            return Err(invalid_params(&format!(
                "axis must be \"horizontal\" or \"vertical\", got {other:?}"
            )));
        }
        None => return Err(invalid_params("axis is required")),
    };

    let new_pane_id = app
        .api_split_pane(pane_id, session_id, axis)
        .map_err(|message| (PANE_NOT_FOUND, message))?;
    Ok(json!({ "pane_id": new_pane_id }))
}

/// `agent.report`: externally reported agent state for one pane, overriding
/// screen-based detection for a bounded validity window.
fn agent_report(app: &mut App, params: Option<&Value>) -> Result<Value, MethodError> {
    let params = params_object(params)?;
    let Some(pane_id) = optional_usize_param(params, "pane_id")? else {
        return Err(invalid_params("pane_id is required"));
    };
    let session_id = optional_str_param(params, "session_id")?;
    let Some(kind) = optional_str_param(params, "kind")? else {
        return Err(invalid_params("kind is required"));
    };
    let kind = sanitize_agent_kind(kind);
    if kind.is_empty() {
        return Err(invalid_params(
            "kind must contain at least one alphanumeric or dash character",
        ));
    }
    let state = match optional_str_param(params, "state")? {
        Some("idle") => crate::agent::AgentState::Idle,
        Some("working") => crate::agent::AgentState::Working,
        Some("blocked") => crate::agent::AgentState::Blocked,
        Some("unknown") => crate::agent::AgentState::Unknown,
        Some(other) => {
            return Err(invalid_params(&format!(
                "state must be one of \"idle\"|\"working\"|\"blocked\"|\"unknown\", got {other:?}"
            )));
        }
        None => return Err(invalid_params("state is required")),
    };

    app.api_report_agent(pane_id, session_id, kind, state)
        .map_err(|message| (PANE_NOT_FOUND, message))?;
    Ok(json!({ "ok": true }))
}

/// `events.subscribe`: build the connection's event filter. Unknown event
/// names are accepted silently (forward compatibility) but only known names
/// are echoed back in `subscribed`.
fn events_subscribe(params: Option<&Value>) -> Result<(Value, EventSubscription), MethodError> {
    let params = params_object(params)?;
    let events = match params.and_then(|map| map.get("events")) {
        None | Some(Value::Null) => None,
        Some(Value::Array(entries)) => {
            let mut names = Vec::new();
            for entry in entries {
                let Some(name) = entry.as_str() else {
                    return Err(invalid_params("events must be an array of strings"));
                };
                names.push(name.to_string());
            }
            Some(names)
        }
        Some(_) => return Err(invalid_params("events must be an array of strings")),
    };

    let subscribed: Vec<&str> = match &events {
        None => EVENT_NAMES.to_vec(),
        Some(names) => EVENT_NAMES
            .iter()
            .copied()
            .filter(|known| names.iter().any(|name| name == known))
            .collect(),
    };
    Ok((
        json!({ "subscribed": subscribed }),
        EventSubscription { events },
    ))
}

/// Normalize an externally reported agent kind: lowercase, keep only ASCII
/// alphanumerics and dashes, cap at [`AGENT_KIND_MAX_CHARS`] characters.
fn sanitize_agent_kind(raw: &str) -> String {
    raw.chars()
        .filter_map(|character| {
            let character = character.to_ascii_lowercase();
            (character.is_ascii_alphanumeric() || character == '-').then_some(character)
        })
        .take(AGENT_KIND_MAX_CHARS)
        .collect()
}

fn params_object(params: Option<&Value>) -> Result<Option<&Map<String, Value>>, MethodError> {
    match params {
        None | Some(Value::Null) => Ok(None),
        Some(Value::Object(map)) => Ok(Some(map)),
        Some(_) => Err(invalid_params("params must be an object")),
    }
}

fn optional_str_param<'a>(
    params: Option<&'a Map<String, Value>>,
    key: &str,
) -> Result<Option<&'a str>, MethodError> {
    match params.and_then(|map| map.get(key)) {
        None | Some(Value::Null) => Ok(None),
        Some(Value::String(value)) => Ok(Some(value.as_str())),
        Some(_) => Err(invalid_params(&format!("{key} must be a string"))),
    }
}

fn optional_usize_param(
    params: Option<&Map<String, Value>>,
    key: &str,
) -> Result<Option<usize>, MethodError> {
    match params.and_then(|map| map.get(key)) {
        None | Some(Value::Null) => Ok(None),
        Some(value) => value
            .as_u64()
            .and_then(|value| usize::try_from(value).ok())
            .map(Some)
            .ok_or_else(|| invalid_params(&format!("{key} must be a non-negative integer"))),
    }
}

fn invalid_params(message: &str) -> MethodError {
    (INVALID_PARAMS, message.to_string())
}

fn internal_error(err: serde_json::Error) -> MethodError {
    (INTERNAL_ERROR, format!("internal error: {err}"))
}

fn error_response(id: Value, code: i64, message: &str) -> String {
    json!({ "id": id, "error": { "code": code, "message": message } }).to_string()
}
