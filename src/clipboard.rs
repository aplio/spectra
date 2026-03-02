use std::io::Write;
use std::process::{Command, Stdio};

use base64::Engine;
use base64::engine::general_purpose::STANDARD as BASE64;

/// Copy text to the system clipboard.
///
/// First tries native clipboard commands (pbcopy, wl-copy, xclip).
/// If all fail, falls back to OSC 52 which works over SSH by asking
/// the host terminal emulator to set the clipboard.
pub fn copy_text(text: &str) -> Result<(), String> {
    let native_result = copy_text_native(text);
    if native_result.is_ok() {
        return native_result;
    }

    // Fall back to OSC 52 (works over SSH)
    copy_text_osc52(text)
}

fn copy_text_native(text: &str) -> Result<(), String> {
    copy_text_with_runner(text, |program, args, payload| {
        let mut child = Command::new(program)
            .args(args)
            .stdin(Stdio::piped())
            .spawn()
            .map_err(|err| err.to_string())?;
        child
            .stdin
            .as_mut()
            .ok_or_else(|| "failed to open clipboard stdin".to_string())?
            .write_all(payload.as_bytes())
            .map_err(|err| err.to_string())?;
        let status = child.wait().map_err(|err| err.to_string())?;
        if status.success() {
            Ok(())
        } else {
            Err(format!(
                "clipboard command `{program}` exited with {status}"
            ))
        }
    })
}

fn copy_text_with_runner<F>(text: &str, mut run: F) -> Result<(), String>
where
    F: FnMut(&str, &[&str], &str) -> Result<(), String>,
{
    if cfg!(target_os = "macos") {
        return run("pbcopy", &[], text);
    }

    let mut last_error = None;
    for (program, args) in [
        ("wl-copy", vec![]),
        ("xclip", vec!["-selection", "clipboard"]),
    ] {
        match run(program, &args, text) {
            Ok(()) => return Ok(()),
            Err(err) => last_error = Some(format!("{program}: {err}")),
        }
    }

    Err(last_error.unwrap_or_else(|| "no clipboard backend available".to_string()))
}

/// Copy text using OSC 52 escape sequence.
///
/// Writes `ESC ] 52 ; c ; <base64> ST` directly to stdout.
/// The host terminal emulator intercepts this and sets the system clipboard.
/// Works over SSH because the sequence travels through the terminal stream.
pub fn copy_text_osc52(text: &str) -> Result<(), String> {
    copy_text_osc52_to(&mut std::io::stdout(), text)
}

/// Build an OSC 52 escape sequence for clipboard copy.
pub fn osc52_sequence(text: &str) -> String {
    let encoded = BASE64.encode(text.as_bytes());
    format!("\x1b]52;c;{encoded}\x1b\\")
}

fn copy_text_osc52_to<W: Write>(writer: &mut W, text: &str) -> Result<(), String> {
    write!(writer, "{}", osc52_sequence(text)).map_err(|err| err.to_string())?;
    writer.flush().map_err(|err| err.to_string())?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::{copy_text_osc52_to, copy_text_with_runner, osc52_sequence};

    use base64::Engine;
    use base64::engine::general_purpose::STANDARD as BASE64;

    #[test]
    fn linux_path_falls_back_to_xclip() {
        let mut calls = Vec::new();
        let result = copy_text_with_runner("hello", |program, _args, payload| {
            calls.push(program.to_string());
            assert_eq!(payload, "hello");
            if program == "wl-copy" {
                Err("missing".to_string())
            } else {
                Ok(())
            }
        });
        assert!(result.is_ok());
        if !cfg!(target_os = "macos") {
            assert_eq!(calls, vec!["wl-copy", "xclip"]);
        }
    }

    #[test]
    fn reports_backend_error_when_all_fail() {
        let err = copy_text_with_runner("hello", |program, _args, _payload| {
            Err(format!("{program} failed"))
        })
        .expect_err("expected failure");

        if cfg!(target_os = "macos") {
            assert!(err.contains("pbcopy"));
        } else {
            assert!(err.contains("xclip"));
        }
    }

    #[test]
    fn osc52_emits_correct_escape_sequence() {
        let mut buf = Vec::new();
        copy_text_osc52_to(&mut buf, "hello").expect("osc52 write");
        let output = String::from_utf8(buf).expect("valid utf8");
        let expected_b64 = BASE64.encode(b"hello");
        assert_eq!(output, format!("\x1b]52;c;{expected_b64}\x1b\\"));
    }

    #[test]
    fn osc52_sequence_encodes_payload() {
        let sequence = osc52_sequence("hello");
        let expected_b64 = BASE64.encode(b"hello");
        assert_eq!(sequence, format!("\x1b]52;c;{expected_b64}\x1b\\"));
    }
}
