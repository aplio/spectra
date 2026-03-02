use std::io;
use std::sync::Arc;

use spectra::session::manager::{SessionManager, SessionOptions};
use spectra::session::pane::PaneBackend;
use spectra::session::pty_backend::{PaneFactory, PaneSpawnConfig};

const COLS: u16 = 20;
const ROWS: u16 = 4;

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

struct StaticFactory {
    bytes: Vec<u8>,
}

impl PaneFactory for StaticFactory {
    fn spawn(&self, _config: &PaneSpawnConfig) -> io::Result<Box<dyn PaneBackend>> {
        Ok(Box::new(StaticBackend {
            chunks: vec![self.bytes.clone()],
        }))
    }
}

#[test]
fn render_emits_style_control_sequences_for_colored_cells() {
    let styled = render_output(b"\x1b[38;2;12;34;56m\x1b[48;2;3;4;5m\x1b[1;4;9mHi\x1b[0m\r\n$ ");
    let plain = render_output(b"Hi\r\n$ ");

    assert!(
        styled.windows(2).any(|window| window == b"Hi"),
        "expected rendered text in styled output"
    );

    let styled_sgr = count_sgr_sequences(&styled);
    let plain_sgr = count_sgr_sequences(&plain);
    assert!(
        styled_sgr > plain_sgr,
        "expected styled output to emit more SGR sequences than plain output (styled={styled_sgr}, plain={plain_sgr})"
    );
}

fn render_output(bytes: &[u8]) -> Vec<u8> {
    let options = SessionOptions::from_cli(Some("/bin/sh".to_string()), None, vec![]);
    let factory = StaticFactory {
        bytes: bytes.to_vec(),
    };
    let mut session = SessionManager::with_factory(options, Arc::new(factory), COLS, ROWS)
        .expect("create session");
    session.poll_output();

    let frame = session.frame(COLS, ROWS);
    let mut out = Vec::new();
    spectra::ui::render::render_to_writer(&mut out, &frame, "status", COLS, ROWS, true, None, None)
        .expect("render pane");
    out
}

fn count_sgr_sequences(bytes: &[u8]) -> usize {
    let mut count = 0usize;
    let mut i = 0usize;

    while i < bytes.len() {
        if bytes[i] == b'\x1b' && i + 1 < bytes.len() && bytes[i + 1] == b'[' {
            let mut idx = i + 2;
            while idx < bytes.len() {
                let byte = bytes[idx];
                if (0x40..=0x7e).contains(&byte) {
                    if byte == b'm' {
                        count += 1;
                    }
                    idx += 1;
                    break;
                }
                idx += 1;
            }
            i = idx;
            continue;
        }
        i += 1;
    }

    count
}
