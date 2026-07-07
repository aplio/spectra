//! End-to-end checks for CJK rendering and IME cursor anchoring.
//!
//! Drives a real `SessionManager` + frame renderer with synthetic PTY output
//! and asserts on the ANSI bytes sent to the host terminal:
//!
//! - double-width text reaches the host intact (no split wide chars, no
//!   leaked `'\0'` continuation cells);
//! - a guest hiding its cursor (DECTCEM, like Claude Code does) leaves the
//!   host cursor hidden but parked at the pane cursor cell, so IME candidate
//!   windows anchor correctly once a client policy re-shows it.

use std::io;
use std::sync::{Arc, Mutex};

use spectra::session::manager::{SessionManager, SessionOptions};
use spectra::session::pane::PaneBackend;
use spectra::session::pty_backend::{PaneFactory, PaneSpawnConfig};

// Only `ansi_bytes_to_rows` is used from the shared support module.
#[allow(dead_code)]
mod support;

const COLS: u16 = 40;
const ROWS: u16 = 8;

struct StaticBackend {
    chunks: Vec<Vec<u8>>,
}

impl PaneBackend for StaticBackend {
    fn write(&mut self, _bytes: &[u8]) -> io::Result<()> {
        Ok(())
    }

    fn resize(&mut self, _cols: u16, _rows: u16) -> io::Result<()> {
        Ok(())
    }

    fn poll_output(&mut self) -> Vec<Vec<u8>> {
        std::mem::take(&mut self.chunks)
    }
}

struct FixedFactory {
    chunks: Mutex<Vec<Vec<u8>>>,
}

impl FixedFactory {
    fn new(chunks: Vec<Vec<u8>>) -> Self {
        Self {
            chunks: Mutex::new(chunks),
        }
    }
}

impl PaneFactory for FixedFactory {
    fn spawn(&self, _config: &PaneSpawnConfig) -> io::Result<Box<dyn PaneBackend>> {
        Ok(Box::new(StaticBackend {
            chunks: std::mem::take(&mut *self.chunks.lock().expect("chunks lock")),
        }))
    }
}

fn session_with_output(chunks: Vec<Vec<u8>>) -> SessionManager {
    let options = SessionOptions::from_cli(Some("/bin/sh".to_string()), None, vec![]);
    let mut session =
        SessionManager::with_factory(options, Arc::new(FixedFactory::new(chunks)), COLS, ROWS)
            .expect("create session");
    session.poll_output();
    session
}

#[test]
fn japanese_text_renders_to_host_without_split_chars() {
    let mut session = session_with_output(vec!["$ echo 日本語のテスト".as_bytes().to_vec()]);
    session.poll_output();

    let frame = session.frame(COLS, ROWS);
    let mut out = Vec::new();
    spectra::ui::render::render_to_writer(&mut out, &frame, "status", COLS, ROWS, true, None, None)
        .expect("render CJK frame");

    let rows = support::render_snapshot::ansi_bytes_to_rows(&out, COLS as usize, ROWS as usize);
    let row0 = rows[0].as_str();
    assert!(
        row0.contains("$ echo 日本語のテスト"),
        "wide chars must reach the host intact: {row0:?}"
    );
    assert!(
        !row0.contains('\0'),
        "continuation cells must never be emitted"
    );

    // The pane cursor sits after 21 display cells ("$ echo " = 7, seven
    // double-width chars = 14) and the frame reports the cell column.
    assert_eq!(frame.focused_cursor, Some((21, 0)));
    assert!(!frame.focused_cursor_hidden);
    let text = String::from_utf8_lossy(&out);
    assert!(
        text.contains("\x1b[?25h"),
        "visible guest cursor shows the host cursor"
    );
}

#[test]
fn guest_hiding_cursor_parks_hidden_host_cursor_at_pane_cell() {
    let mut session = session_with_output(vec!["typing: あい\x1b[?25l".as_bytes().to_vec()]);
    session.poll_output();

    let frame = session.frame(COLS, ROWS);
    assert!(
        frame.focused_cursor_hidden,
        "DECTCEM hide must be tracked through the frame"
    );
    // "typing: " = 8 cells + あい = 4 cells → cursor cell column 12.
    assert_eq!(frame.focused_cursor, Some((12, 0)));

    let mut out = Vec::new();
    spectra::ui::render::render_to_writer(&mut out, &frame, "status", COLS, ROWS, true, None, None)
        .expect("render hidden-cursor frame");
    let text = String::from_utf8_lossy(&out);
    assert!(
        !text.contains("\x1b[?25h"),
        "host cursor must stay hidden while the guest hides it: {text:?}"
    );
    assert!(
        text.ends_with("\x1b[1;13H"),
        "host cursor must be parked at the pane cursor cell for IME anchoring: {text:?}"
    );
}

#[test]
fn guest_reshowing_cursor_restores_host_cursor() {
    let mut session = session_with_output(vec![
        "x\x1b[?25l".as_bytes().to_vec(),
        b"\x1b[?25h".to_vec(),
    ]);
    session.poll_output();
    session.poll_output();

    let frame = session.frame(COLS, ROWS);
    assert!(!frame.focused_cursor_hidden);

    let mut out = Vec::new();
    spectra::ui::render::render_to_writer(&mut out, &frame, "status", COLS, ROWS, true, None, None)
        .expect("render reshown-cursor frame");
    let text = String::from_utf8_lossy(&out);
    assert!(text.contains("\x1b[?25h"));
}
