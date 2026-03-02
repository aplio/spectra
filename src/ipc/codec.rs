use std::io;

use serde::Serialize;
use serde::de::DeserializeOwned;

#[derive(Debug)]
pub struct DecodeResult<T> {
    pub messages: Vec<T>,
    pub errors: Vec<String>,
}

impl<T> Default for DecodeResult<T> {
    fn default() -> Self {
        Self {
            messages: Vec::new(),
            errors: Vec::new(),
        }
    }
}

pub fn encode_message<T: Serialize>(message: &T) -> io::Result<Vec<u8>> {
    let mut bytes =
        serde_json::to_vec(message).map_err(|err| io::Error::other(format!("encode: {err}")))?;
    bytes.push(b'\n');
    Ok(bytes)
}

pub fn decode_messages<T: DeserializeOwned>(buffer: &mut Vec<u8>) -> DecodeResult<T> {
    let mut result = DecodeResult::default();

    while let Some(newline) = buffer.iter().position(|byte| *byte == b'\n') {
        let mut line = buffer.drain(..=newline).collect::<Vec<_>>();
        while matches!(line.last(), Some(b'\n' | b'\r')) {
            line.pop();
        }
        if line.is_empty() {
            continue;
        }
        match serde_json::from_slice::<T>(&line) {
            Ok(message) => result.messages.push(message),
            Err(err) => result.errors.push(err.to_string()),
        }
    }

    result
}

#[cfg(test)]
mod tests {
    use serde::{Deserialize, Serialize};

    use super::{decode_messages, encode_message};

    #[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
    struct Msg {
        value: String,
    }

    #[test]
    fn decodes_partial_frames_incrementally() {
        let mut buffer = br#"{"value":"one"}"#.to_vec();
        let first = decode_messages::<Msg>(&mut buffer);
        assert!(first.messages.is_empty());
        assert!(first.errors.is_empty());

        buffer.extend_from_slice(b"\n");
        buffer.extend_from_slice(br#"{"value":"two"}"#);
        let second = decode_messages::<Msg>(&mut buffer);
        assert_eq!(
            second.messages,
            vec![Msg {
                value: "one".to_string()
            }]
        );
        assert!(second.errors.is_empty());
        assert_eq!(buffer, br#"{"value":"two"}"#);
    }

    #[test]
    fn malformed_frame_reports_error_and_continues() {
        let mut buffer = br#"{"value":"ok"}
{"value"
{"value":"next"}
"#
        .to_vec();
        let decoded = decode_messages::<Msg>(&mut buffer);
        assert_eq!(
            decoded.messages,
            vec![
                Msg {
                    value: "ok".to_string()
                },
                Msg {
                    value: "next".to_string()
                }
            ]
        );
        assert_eq!(decoded.errors.len(), 1);
        assert!(buffer.is_empty());
    }

    #[test]
    fn encode_appends_newline() {
        let bytes = encode_message(&Msg {
            value: "test".to_string(),
        })
        .expect("encode");
        assert_eq!(*bytes.last().expect("trailing newline"), b'\n');
    }
}
