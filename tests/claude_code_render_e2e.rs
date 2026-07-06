//! End-to-end regression for stray underlines when running Claude Code.
//!
//! `tests/fixtures/claude_code_startup.ansi` is a sanitized PTY capture of a
//! real Claude Code v2.1 startup. Two properties of that stream make it a
//! good adversarial input:
//!
//! - it opens with XTMODKEYS `CSI > 4;2 m`, which shares the `m` final byte
//!   with SGR: parsed without checking the `>` private marker it reads as
//!   SGR `4;2` = underline + dim;
//! - it never sends a full SGR 0 reset (only targeted resets like 22/23/39),
//!   so any spuriously enabled attribute sticks for the entire screen.
//!
//! The capture contains no legitimate SGR underline at all, so the invariant
//! is simple: after feeding the whole startup through the grid and rendering
//! a frame, the bytes sent to the host terminal must never enable underline.

use std::io;
use std::sync::{Arc, Mutex};

use spectra::session::manager::{SessionManager, SessionOptions};
use spectra::session::pane::PaneBackend;
use spectra::session::pty_backend::{PaneFactory, PaneSpawnConfig};

// Only `ansi_bytes_to_rows` is used from the shared support module.
#[allow(dead_code)]
mod support;

// The capture was taken on a 200x60 PTY; give the pane at least that much.
const COLS: u16 = 200;
const ROWS: u16 = 62;

/// Split the capture into PTY-read-sized bursts so escape sequences are
/// routinely cut in half across feeds, like real `poll_output` chunks.
const CHUNK_LEN: usize = 700;

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

struct FixtureFactory {
    chunks: Mutex<Vec<Vec<u8>>>,
}

impl PaneFactory for FixtureFactory {
    fn spawn(&self, _config: &PaneSpawnConfig) -> io::Result<Box<dyn PaneBackend>> {
        Ok(Box::new(StaticBackend {
            chunks: std::mem::take(&mut self.chunks.lock().expect("lock chunks")),
        }))
    }
}

fn claude_startup_chunks() -> Vec<Vec<u8>> {
    let bytes = std::fs::read(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/tests/fixtures/claude_code_startup.ansi"
    ))
    .expect("read claude_code_startup.ansi fixture");
    bytes.chunks(CHUNK_LEN).map(<[u8]>::to_vec).collect()
}

/// True if any non-private CSI `m` sequence in `bytes` enables underline
/// (SGR 4, styled `4:1`..`4:5`, or 21 = doubly underlined).
fn output_enables_underline(bytes: &[u8]) -> bool {
    let mut i = 0;
    while i + 1 < bytes.len() {
        if bytes[i] != 0x1b || bytes[i + 1] != b'[' {
            i += 1;
            continue;
        }
        let start = i + 2;
        let mut end = start;
        while end < bytes.len() && !(0x40..=0x7e).contains(&bytes[end]) {
            end += 1;
        }
        if end >= bytes.len() {
            break;
        }
        let body = &bytes[start..end];
        i = end + 1;
        if bytes[end] != b'm' || body.first().is_some_and(|b| matches!(b, b'<' | b'=' | b'>' | b'?')) {
            continue;
        }
        let params = std::str::from_utf8(body).expect("CSI params are ASCII");
        for group in params.split(';') {
            let mut sub = group.split(':');
            let code = sub.next().unwrap_or("");
            match code {
                "21" => return true,
                "4" => {
                    if sub.next().is_none_or(|style| style != "0") {
                        return true;
                    }
                }
                _ => {}
            }
        }
    }
    false
}

#[test]
fn claude_code_startup_never_underlines_host_output() {
    let chunks = claude_startup_chunks();
    assert!(
        chunks
            .concat()
            .windows(7)
            .any(|window| window == b"\x1b[>4;2m"),
        "fixture must contain the XTMODKEYS sequence this test guards against"
    );

    let options = SessionOptions::from_cli(Some("/bin/sh".to_string()), None, vec![]);
    let factory = FixtureFactory {
        chunks: Mutex::new(chunks),
    };
    let mut session = SessionManager::with_factory(options, Arc::new(factory), COLS, ROWS)
        .expect("create session");
    session.poll_output();

    let frame = session.frame(COLS, ROWS);
    let mut out = Vec::new();
    spectra::ui::render::render_to_writer(&mut out, &frame, "status", COLS, ROWS, true, None, None)
        .expect("render claude startup frame");

    // Sanity: the startup screen actually rendered.
    let rows = support::render_snapshot::ansi_bytes_to_rows(&out, COLS as usize, ROWS as usize);
    let screen = rows.join("\n");
    assert!(
        screen.contains("Tips for getting started"),
        "expected Claude Code startup content in the rendered frame:\n{screen}"
    );

    assert!(
        !output_enables_underline(&out),
        "host output must not enable underline anywhere: the guest never \
         underlines, so any underline is XTMODKEYS/attribute leakage"
    );
}
