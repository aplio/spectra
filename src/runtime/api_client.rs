//! One-shot client for the read-only JSON-RPC API socket
//! (`spectra api <METHOD> [PARAMS_JSON]`).
//!
//! This deliberately bypasses the interactive client protocol: it connects
//! straight to `spectra-api.sock`, sends one newline-delimited JSON-RPC
//! request, and prints the raw result. It is the generic scripting/agent
//! escape hatch; pretty per-method wrappers are intentionally not provided.

#![cfg(unix)]

use std::io::{self, BufRead, BufReader, Write};
use std::os::unix::net::UnixStream;
use std::time::Duration;

use serde_json::{Value, json};

use crate::cli::{Cli, CliCommand};
use crate::ipc::socket_path;

const RESPONSE_TIMEOUT: Duration = Duration::from_secs(5);

pub fn run(cli: Cli) -> io::Result<()> {
    let Some(CliCommand::Api { method, params }) = &cli.subcommand else {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "missing api subcommand",
        ));
    };

    // Reject malformed params before touching the socket.
    let params = parse_params(params.as_deref())?;

    let socket = socket_path::api_socket_path();
    let stream = UnixStream::connect(&socket)
        .map_err(|err| io::Error::new(err.kind(), "no spectra server is running"))?;
    stream.set_read_timeout(Some(RESPONSE_TIMEOUT))?;
    stream.set_write_timeout(Some(RESPONSE_TIMEOUT))?;

    let mut request = json!({ "id": 1, "method": method });
    if let Some(params) = params {
        request["params"] = params;
    }

    let mut reader = BufReader::new(stream);
    {
        let stream = reader.get_mut();
        stream.write_all(request.to_string().as_bytes())?;
        stream.write_all(b"\n")?;
    }

    let mut line = String::new();
    reader.read_line(&mut line)?;
    if line.trim().is_empty() {
        return Err(io::Error::new(
            io::ErrorKind::UnexpectedEof,
            "empty response from spectra api socket",
        ));
    }
    let response: Value = serde_json::from_str(line.trim_end()).map_err(|err| {
        io::Error::new(
            io::ErrorKind::InvalidData,
            format!("invalid api response line {line:?}: {err}"),
        )
    })?;

    if let Some(error) = response.get("error") {
        let message = error
            .get("message")
            .and_then(Value::as_str)
            .map(str::to_string)
            .unwrap_or_else(|| error.to_string());
        return Err(io::Error::other(message));
    }
    match response.get("result") {
        Some(result) => {
            println!("{result}");
            Ok(())
        }
        None => Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("api response has neither result nor error: {response}"),
        )),
    }
}

fn parse_params(raw: Option<&str>) -> io::Result<Option<Value>> {
    match raw {
        None => Ok(None),
        Some(raw) => serde_json::from_str(raw).map(Some).map_err(|err| {
            io::Error::new(
                io::ErrorKind::InvalidInput,
                format!("invalid PARAMS_JSON: {err}"),
            )
        }),
    }
}

#[cfg(test)]
mod tests {
    use super::parse_params;

    #[test]
    fn parse_params_accepts_missing_params() {
        assert_eq!(parse_params(None).expect("no params"), None);
    }

    #[test]
    fn parse_params_accepts_json_object() {
        let parsed = parse_params(Some(r#"{"pane_id":1,"lines":50}"#))
            .expect("valid params")
            .expect("some value");
        assert_eq!(parsed["pane_id"], 1);
        assert_eq!(parsed["lines"], 50);
    }

    #[test]
    fn parse_params_rejects_invalid_json() {
        let err = parse_params(Some("{not json")).expect_err("invalid params");
        assert_eq!(err.kind(), std::io::ErrorKind::InvalidInput);
        assert!(err.to_string().contains("invalid PARAMS_JSON"));
    }
}
