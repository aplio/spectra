use std::fs;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use base64::Engine;
use base64::engine::general_purpose::STANDARD as BASE64;

use crate::ipc::protocol::MAX_PASTE_IMAGE_BYTES;

/// Staged clipboard-image files older than this are deleted the next time
/// an image is staged, so pastes stay readable for a while without the
/// staging directory growing forever.
const STAGED_IMAGE_MAX_AGE: Duration = Duration::from_secs(24 * 60 * 60);

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

/// An image read from the OS clipboard.
pub struct ClipboardImage {
    pub bytes: Vec<u8>,
    /// File extension matching the image encoding, e.g. `png`.
    pub format: &'static str,
}

/// Read an image from the system clipboard, if one is present.
///
/// Runs on the client so the clipboard of the machine the user is typing on
/// is used, even when the server is remote. Images larger than
/// [`MAX_PASTE_IMAGE_BYTES`] or with bytes that do not match the advertised
/// encoding are treated as absent.
pub fn read_image() -> Option<ClipboardImage> {
    if cfg!(target_os = "macos") {
        return read_image_macos();
    }
    read_image_linux()
}

fn read_image_linux() -> Option<ClipboardImage> {
    for (mime, format) in [
        ("image/png", "png"),
        ("image/jpeg", "jpg"),
        ("image/jpg", "jpg"),
        ("image/gif", "gif"),
        ("image/webp", "webp"),
        ("image/bmp", "bmp"),
    ] {
        if std::env::var_os("WAYLAND_DISPLAY").is_some()
            && let Some(bytes) = read_image_command_output("wl-paste", &["--type", mime])
            && bytes_match_image_signature(format, &bytes)
        {
            return Some(ClipboardImage { bytes, format });
        }

        if std::env::var_os("DISPLAY").is_some()
            && let Some(bytes) =
                read_image_command_output("xclip", &["-selection", "clipboard", "-t", mime, "-o"])
            && bytes_match_image_signature(format, &bytes)
        {
            return Some(ClipboardImage { bytes, format });
        }
    }

    None
}

fn read_image_macos() -> Option<ClipboardImage> {
    // `osascript` writes the clipboard's PNG representation to a temp file;
    // reading binary data from its stdout is not reliable.
    let path = std::env::temp_dir().join(format!(
        "spectra-clipboard-image-{}-{}.png",
        std::process::id(),
        unix_nanos()
    ));
    let script = format!(
        "set png_data to (the clipboard as \u{ab}class PNGf\u{bb})\nset fp to open for access POSIX file \"{}\" with write permission\nwrite png_data to fp\nclose access fp",
        path.display()
    );

    let status = Command::new("osascript")
        .arg("-e")
        .arg(&script)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .ok();

    let bytes = match status {
        Some(status) if status.success() => fs::File::open(&path)
            .ok()
            .and_then(|file| read_limited(file, MAX_PASTE_IMAGE_BYTES)),
        _ => None,
    };
    let _ = fs::remove_file(&path);

    let bytes = bytes?;
    if !bytes_match_image_signature("png", &bytes) {
        return None;
    }
    Some(ClipboardImage {
        bytes,
        format: "png",
    })
}

fn read_image_command_output(program: &str, args: &[&str]) -> Option<Vec<u8>> {
    let mut child = Command::new(program)
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .ok()?;
    let stdout = child.stdout.take()?;

    let bytes = read_limited(stdout, MAX_PASTE_IMAGE_BYTES);
    if bytes.is_none() {
        let _ = child.kill();
    }
    let status = child.wait().ok()?;
    let bytes = bytes?;
    if !status.success() || bytes.is_empty() {
        return None;
    }
    Some(bytes)
}

/// Read at most `max_bytes` from `reader`; `None` when the stream holds more.
fn read_limited<R: Read>(reader: R, max_bytes: usize) -> Option<Vec<u8>> {
    let mut bytes = Vec::new();
    let mut limited = reader.take(max_bytes as u64 + 1);
    limited.read_to_end(&mut bytes).ok()?;
    if bytes.len() > max_bytes {
        return None;
    }
    Some(bytes)
}

fn bytes_match_image_signature(format: &str, bytes: &[u8]) -> bool {
    match format {
        "png" => bytes.starts_with(b"\x89PNG\r\n\x1a\n"),
        "jpg" => bytes.starts_with(&[0xFF, 0xD8, 0xFF]),
        "gif" => bytes.starts_with(b"GIF87a") || bytes.starts_with(b"GIF89a"),
        "webp" => bytes.len() >= 12 && bytes.starts_with(b"RIFF") && bytes[8..12] == *b"WEBP",
        "bmp" => bytes.starts_with(b"BM"),
        _ => false,
    }
}

fn sanitize_image_format(format: &str) -> &'static str {
    if format.eq_ignore_ascii_case("jpg") || format.eq_ignore_ascii_case("jpeg") {
        "jpg"
    } else if format.eq_ignore_ascii_case("gif") {
        "gif"
    } else if format.eq_ignore_ascii_case("webp") {
        "webp"
    } else if format.eq_ignore_ascii_case("bmp") {
        "bmp"
    } else {
        "png"
    }
}

/// Directory where bridged clipboard images are staged on the server, so
/// the path pasted into a pane is readable by processes on the server's
/// machine. Mirrors the runtime socket location.
fn image_staging_dir() -> PathBuf {
    if let Some(runtime_dir) = std::env::var_os("XDG_RUNTIME_DIR") {
        return PathBuf::from(runtime_dir)
            .join("spectra")
            .join("clipboard-images");
    }
    crate::xdg::app_data_dir()
        .join("run")
        .join("clipboard-images")
}

/// Write clipboard image bytes to a fresh, user-only-readable file under
/// the staging directory and return its path. Stale staged images are
/// cleaned up on the way.
pub fn stage_image(format: &str, bytes: &[u8]) -> std::io::Result<PathBuf> {
    stage_image_in(&image_staging_dir(), format, bytes)
}

fn stage_image_in(dir: &Path, format: &str, bytes: &[u8]) -> std::io::Result<PathBuf> {
    use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};

    let format = sanitize_image_format(format);
    fs::create_dir_all(dir)?;
    fs::set_permissions(dir, fs::Permissions::from_mode(0o700))?;
    remove_stale_staged_images(dir);

    let unique = unix_nanos();
    for attempt in 0..100u32 {
        let path = dir.join(format!("clipboard-{unique}-{attempt}.{format}"));
        let mut file = match fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&path)
        {
            Ok(file) => file,
            Err(err) if err.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(err) => return Err(err),
        };
        file.write_all(bytes)?;
        return Ok(path);
    }

    Err(std::io::Error::new(
        std::io::ErrorKind::AlreadyExists,
        "failed to allocate a unique clipboard image path",
    ))
}

fn remove_stale_staged_images(dir: &Path) {
    let Ok(entries) = fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let Ok(metadata) = entry.metadata() else {
            continue;
        };
        let Ok(modified) = metadata.modified() else {
            continue;
        };
        if modified.elapsed().unwrap_or_default() > STAGED_IMAGE_MAX_AGE {
            let _ = fs::remove_file(entry.path());
        }
    }
}

fn unix_nanos() -> u128 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_nanos())
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::{
        bytes_match_image_signature, copy_text_osc52_to, copy_text_with_runner, osc52_sequence,
        read_limited, sanitize_image_format, stage_image_in,
    };

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

    const PNG_HEADER: &[u8] = b"\x89PNG\r\n\x1a\nrest-of-image";

    #[test]
    fn image_signatures_validate_known_formats() {
        assert!(bytes_match_image_signature("png", PNG_HEADER));
        assert!(bytes_match_image_signature(
            "jpg",
            &[0xFF, 0xD8, 0xFF, 0xE0]
        ));
        assert!(bytes_match_image_signature("gif", b"GIF89a..."));
        assert!(bytes_match_image_signature("webp", b"RIFF\0\0\0\0WEBPVP8 "));
        assert!(bytes_match_image_signature("bmp", b"BMxxxx"));
        assert!(!bytes_match_image_signature("png", b"#!/bin/sh"));
        assert!(!bytes_match_image_signature("txt", b"plain text"));
    }

    #[test]
    fn sanitize_image_format_defaults_to_png() {
        assert_eq!(sanitize_image_format("PNG"), "png");
        assert_eq!(sanitize_image_format("jpeg"), "jpg");
        assert_eq!(sanitize_image_format("webp"), "webp");
        assert_eq!(sanitize_image_format("sh"), "png");
        assert_eq!(sanitize_image_format("../../etc/passwd"), "png");
    }

    #[test]
    fn read_limited_rejects_oversized_streams() {
        assert_eq!(
            read_limited(&b"under"[..], 16),
            Some(b"under".to_vec()),
            "small stream is read fully"
        );
        assert_eq!(
            read_limited(&b"exactly-8"[..], 9),
            Some(b"exactly-8".to_vec())
        );
        assert_eq!(read_limited(&b"way too large"[..], 4), None);
    }

    #[test]
    fn stage_image_writes_restricted_unique_files() {
        use std::os::unix::fs::PermissionsExt;

        let dir = tempfile::tempdir().expect("tempdir");
        let staging = dir.path().join("clipboard-images");

        let first = stage_image_in(&staging, "png", PNG_HEADER).expect("stage first");
        let second = stage_image_in(&staging, "PNG", PNG_HEADER).expect("stage second");
        assert_ne!(first, second);
        assert_eq!(first.extension().and_then(|ext| ext.to_str()), Some("png"));
        assert_eq!(std::fs::read(&first).expect("read staged"), PNG_HEADER);

        let file_mode = std::fs::metadata(&first)
            .expect("staged metadata")
            .permissions()
            .mode();
        assert_eq!(file_mode & 0o777, 0o600, "staged file must be user-only");
        let dir_mode = std::fs::metadata(&staging)
            .expect("staging dir metadata")
            .permissions()
            .mode();
        assert_eq!(dir_mode & 0o777, 0o700, "staging dir must be user-only");

        // Hostile extensions collapse to png instead of escaping the dir.
        let sanitized = stage_image_in(&staging, "../evil", PNG_HEADER).expect("stage sanitized");
        assert!(sanitized.starts_with(&staging));
        assert_eq!(
            sanitized.extension().and_then(|ext| ext.to_str()),
            Some("png")
        );
    }
}
