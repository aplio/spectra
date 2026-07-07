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
use crate::ui::window_manager::{Direction, LayoutTree, SplitAxis};

pub const PARSE_ERROR: i64 = -32700;
pub const METHOD_NOT_FOUND: i64 = -32601;
pub const INVALID_PARAMS: i64 = -32602;
pub const INTERNAL_ERROR: i64 = -32603;
pub const PANE_NOT_FOUND: i64 = -32000;
/// `server.handoff` pre-flight refused (clients attached / too many panes).
pub const HANDOFF_REFUSED: i64 = -32001;

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

/// One entry of the `plugin.list` result.
#[derive(Debug, Clone, Serialize)]
pub struct PluginInfo {
    pub name: String,
    pub description: String,
    /// Whether the plugin declares a supervised `[service]`.
    pub has_service: bool,
    /// Number of `[[on_event]]` command entries.
    pub on_event_commands: usize,
    /// Sorted, de-duplicated union of the subscribed event names.
    pub events: Vec<String>,
    /// Agent kind provided by a bundled agent manifest, `null` when none.
    pub agent_manifest: Option<String>,
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
    /// Set when this request was an accepted `server.handoff`: the server
    /// loop must flush the response and run the fd transfer.
    pub handoff_requested: bool,
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
                handoff_requested: false,
            };
        }
    };
    let id = parsed.get("id").cloned().unwrap_or(Value::Null);
    let Some(method) = parsed.get("method").and_then(Value::as_str) else {
        return DispatchOutcome {
            response: error_response(id, INVALID_PARAMS, "request has no string \"method\" field"),
            subscription: None,
            handoff_requested: false,
        };
    };

    let mut subscription = None;
    let mut handoff_requested = false;
    let response = match handle_method(
        app,
        method,
        parsed.get("params"),
        &mut subscription,
        &mut handoff_requested,
    ) {
        Ok(result) => json!({ "id": id, "result": result }).to_string(),
        Err((code, message)) => error_response(id, code, &message),
    };
    DispatchOutcome {
        response,
        subscription,
        handoff_requested,
    }
}

#[cfg_attr(not(unix), allow(unused_variables))]
fn handle_method(
    app: &mut App,
    method: &str,
    params: Option<&Value>,
    subscription: &mut Option<EventSubscription>,
    handoff_requested: &mut bool,
) -> Result<Value, MethodError> {
    match method {
        "session.list" => session_list(app),
        "pane.list" => pane_list(app, params),
        "pane.read" => pane_read(app, params),
        "pane.send_keys" => pane_send_keys(app, params),
        "pane.split" => pane_split(app, params),
        "pane.swap" => pane_swap(app, params),
        "pane.move" => pane_move(app, params),
        "layout.export" => layout_export(app, params),
        "layout.apply" => layout_apply(app, params),
        "layout.set_split_ratio" => layout_set_split_ratio(app, params),
        "agent.report" => agent_report(app, params),
        "plugin.list" => plugin_list(app),
        #[cfg(unix)]
        "server.handoff" => {
            let result = app
                .api_server_handoff()
                .map_err(|message| (HANDOFF_REFUSED, message))?;
            *handoff_requested = true;
            Ok(result)
        }
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

/// `plugin.list`: loaded plugins with their declared capabilities (empty
/// outside the server, where plugins are never loaded).
fn plugin_list(app: &App) -> Result<Value, MethodError> {
    serde_json::to_value(app.api_plugins()).map_err(internal_error)
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

/// `pane.swap`: focus the target pane (default: the currently focused pane)
/// and swap it with its nearest neighbor in `direction`, keeping the split
/// shape and both panes' PTYs.
fn pane_swap(app: &mut App, params: Option<&Value>) -> Result<Value, MethodError> {
    let params = params_object(params)?;
    let pane_id = optional_usize_param(params, "pane_id")?;
    let session_id = optional_str_param(params, "session_id")?;
    let direction = match optional_str_param(params, "direction")? {
        Some("left") => Direction::Left,
        Some("down") => Direction::Down,
        Some("up") => Direction::Up,
        Some("right") => Direction::Right,
        Some(other) => {
            return Err(invalid_params(&format!(
                "direction must be \"left\"|\"down\"|\"up\"|\"right\", got {other:?}"
            )));
        }
        None => return Err(invalid_params("direction is required")),
    };

    app.api_swap_pane(pane_id, session_id, direction)
        .map_err(|message| (PANE_NOT_FOUND, message))?;
    Ok(json!({ "ok": true }))
}

/// `pane.move`: relocate the target pane (default: the currently focused
/// pane) with its PTY intact. Exactly one destination is required:
/// `to_window: N` grafts it into window N, `new_window: true` breaks it out
/// into a new window, `to_session: "<id>"` adopts it into another session
/// (where it gets that session's next pane id).
fn pane_move(app: &mut App, params: Option<&Value>) -> Result<Value, MethodError> {
    let params = params_object(params)?;
    let pane_id = optional_usize_param(params, "pane_id")?;
    let session_id = optional_str_param(params, "session_id")?;
    let to_window = optional_usize_param(params, "to_window")?;
    let new_window = match params.and_then(|map| map.get("new_window")) {
        None | Some(Value::Null) => false,
        Some(Value::Bool(value)) => *value,
        Some(_) => return Err(invalid_params("new_window must be a boolean")),
    };
    let to_session = optional_str_param(params, "to_session")?;

    let destinations = usize::from(to_window.is_some())
        + usize::from(new_window)
        + usize::from(to_session.is_some());
    if destinations != 1 {
        return Err(invalid_params(
            "exactly one of to_window, new_window, to_session is required",
        ));
    }

    if let Some(target_session) = to_session {
        let Some(pane_id) = pane_id else {
            return Err(invalid_params("pane_id is required with to_session"));
        };
        let new_pane_id = app
            .api_move_pane_to_session(pane_id, session_id, target_session)
            .map_err(|message| (PANE_NOT_FOUND, message))?;
        return Ok(json!({ "pane_id": new_pane_id, "session_id": target_session }));
    }

    let (moved_pane_id, window) = app
        .api_move_pane_in_session(pane_id, session_id, to_window)
        .map_err(|message| (PANE_NOT_FOUND, message))?;
    Ok(json!({ "pane_id": moved_pane_id, "window": window }))
}

/// `layout.export`: portable split tree of one window (default: the
/// session's focused window).
fn layout_export(app: &App, params: Option<&Value>) -> Result<Value, MethodError> {
    let params = params_object(params)?;
    let session_id = optional_str_param(params, "session_id")?;
    let window = optional_usize_param(params, "window")?;

    let (session_id, window, tree) = app
        .api_layout_export(session_id, window)
        .map_err(|message| (PANE_NOT_FOUND, message))?;
    Ok(json!({
        "session_id": session_id,
        "window": window,
        "layout": layout_tree_to_json(&tree),
    }))
}

/// `layout.apply`: rearrange one window (default: the session's focused
/// window) into the given layout. The layout's leaves must reference exactly
/// the panes currently in that window.
fn layout_apply(app: &mut App, params: Option<&Value>) -> Result<Value, MethodError> {
    let params = params_object(params)?;
    let session_id = optional_str_param(params, "session_id")?;
    let window = optional_usize_param(params, "window")?;
    let Some(layout) = params.and_then(|map| map.get("layout")) else {
        return Err(invalid_params("layout is required"));
    };
    let tree = layout_tree_from_json(layout).map_err(|message| invalid_params(&message))?;

    app.api_layout_apply(session_id, window, &tree)
        .map_err(|message| (PANE_NOT_FOUND, message))?;
    Ok(json!({ "ok": true }))
}

/// `layout.set_split_ratio`: set the first-child share (percent, clamped
/// 10..=90) of the split directly containing the pane.
fn layout_set_split_ratio(app: &mut App, params: Option<&Value>) -> Result<Value, MethodError> {
    let params = params_object(params)?;
    let Some(pane_id) = optional_usize_param(params, "pane_id")? else {
        return Err(invalid_params("pane_id is required"));
    };
    let session_id = optional_str_param(params, "session_id")?;
    let Some(ratio) = optional_usize_param(params, "ratio")? else {
        return Err(invalid_params("ratio is required"));
    };
    if ratio > 100 {
        return Err(invalid_params("ratio must be 0..=100"));
    }

    app.api_layout_set_split_ratio(pane_id, session_id, ratio as u8)
        .map_err(|message| (PANE_NOT_FOUND, message))?;
    Ok(json!({ "ok": true }))
}

/// Wire form of a layout tree:
/// `{"type": "leaf", "pane_id": N}` or
/// `{"type": "split", "axis": "vertical"|"horizontal", "ratio_percent": N,
///   "first": ..., "second": ...}`.
fn layout_tree_to_json(tree: &LayoutTree) -> Value {
    match tree {
        LayoutTree::Leaf { item } => json!({ "type": "leaf", "pane_id": item }),
        LayoutTree::Split {
            axis,
            ratio_percent,
            first,
            second,
        } => json!({
            "type": "split",
            "axis": match axis {
                SplitAxis::Vertical => "vertical",
                SplitAxis::Horizontal => "horizontal",
            },
            "ratio_percent": ratio_percent,
            "first": layout_tree_to_json(first),
            "second": layout_tree_to_json(second),
        }),
    }
}

fn layout_tree_from_json(value: &Value) -> Result<LayoutTree, String> {
    let Some(object) = value.as_object() else {
        return Err("layout node must be an object".to_string());
    };
    match object.get("type").and_then(Value::as_str) {
        Some("leaf") => {
            let pane_id = object
                .get("pane_id")
                .and_then(Value::as_u64)
                .and_then(|value| usize::try_from(value).ok())
                .ok_or_else(|| "leaf node needs a numeric pane_id".to_string())?;
            Ok(LayoutTree::Leaf { item: pane_id })
        }
        Some("split") => {
            let axis = match object.get("axis").and_then(Value::as_str) {
                Some("vertical") => SplitAxis::Vertical,
                Some("horizontal") => SplitAxis::Horizontal,
                _ => {
                    return Err("split node needs axis \"vertical\" or \"horizontal\"".to_string());
                }
            };
            let ratio_percent = match object.get("ratio_percent") {
                None | Some(Value::Null) => 50,
                Some(value) => value
                    .as_u64()
                    .filter(|ratio| *ratio <= 100)
                    .ok_or_else(|| "ratio_percent must be an integer 0..=100".to_string())?
                    as u8,
            };
            let first = object
                .get("first")
                .ok_or_else(|| "split node needs first".to_string())?;
            let second = object
                .get("second")
                .ok_or_else(|| "split node needs second".to_string())?;
            Ok(LayoutTree::Split {
                axis,
                ratio_percent,
                first: Box::new(layout_tree_from_json(first)?),
                second: Box::new(layout_tree_from_json(second)?),
            })
        }
        Some(other) => Err(format!("unknown layout node type {other:?}")),
        None => Err("layout node needs a string \"type\" field".to_string()),
    }
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
