//! Read-only JSON-RPC API served over the secondary Unix socket
//! (`spectra-api.sock`).
//!
//! The wire format is newline-delimited JSON: one request object per line,
//! one response object per line.
//!
//! - Request: `{"id": <number|string>, "method": "<name>", "params": {...}?}`
//! - Success: `{"id": ..., "result": ...}`
//! - Error: `{"id": ..., "error": {"code": <int>, "message": "..."}}`
//!
//! Dispatch takes `&App`, so the API cannot mutate server state.

use serde::Serialize;
use serde_json::{Map, Value, json};

use crate::app::App;

pub const PARSE_ERROR: i64 = -32700;
pub const METHOD_NOT_FOUND: i64 = -32601;
pub const INVALID_PARAMS: i64 = -32602;
pub const INTERNAL_ERROR: i64 = -32603;
pub const PANE_NOT_FOUND: i64 = -32000;

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
}

type MethodError = (i64, String);

/// Handle one JSON-RPC request line and return the response line
/// (without the trailing newline).
pub fn dispatch(app: &App, request: &str) -> String {
    let parsed: Value = match serde_json::from_str(request) {
        Ok(value) => value,
        Err(err) => {
            return error_response(Value::Null, PARSE_ERROR, &format!("parse error: {err}"));
        }
    };
    let id = parsed.get("id").cloned().unwrap_or(Value::Null);
    let Some(method) = parsed.get("method").and_then(Value::as_str) else {
        return error_response(id, INVALID_PARAMS, "request has no string \"method\" field");
    };

    match handle_method(app, method, parsed.get("params")) {
        Ok(result) => json!({ "id": id, "result": result }).to_string(),
        Err((code, message)) => error_response(id, code, &message),
    }
}

fn handle_method(app: &App, method: &str, params: Option<&Value>) -> Result<Value, MethodError> {
    match method {
        "session.list" => session_list(app),
        "pane.list" => pane_list(app, params),
        "pane.read" => pane_read(app, params),
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
