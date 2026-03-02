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

fn rendered_rows(bytes: &[u8]) -> Vec<String> {
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
        .expect("render output");
    ansi_bytes_to_rows(&out, COLS as usize, ROWS as usize)
}

#[test]
fn lf_moves_down_without_cr() {
    let rows = rendered_rows(b"~$\n~$");
    assert_eq!(rows[0], "~$");
    // \n only moves down, cursor stays at column 2
    assert_eq!(rows[1], "  ~$");
}

#[test]
fn crlf_moves_prompt_to_column_zero() {
    let rows = rendered_rows(b"~$\r\n~$");
    assert_eq!(rows[0], "~$");
    assert_eq!(rows[1], "~$");
}

#[test]
fn crlf_keeps_expected_prompt_alignment() {
    let rows = rendered_rows(b"~$\r\n~$");
    assert_eq!(rows[0], "~$");
    assert_eq!(rows[1], "~$");
}

#[test]
fn prompt_repaint_sequence_clears_stale_marker_bytes() {
    // Use \r\n to get CR+LF behavior (as a real PTY with onlcr would send)
    let rows = rendered_rows(b"abc%\r\x1b[2K~$\r\n~$");
    assert_eq!(rows[0], "~$");
    assert_eq!(rows[1], "~$");
}

fn ansi_bytes_to_rows(bytes: &[u8], cols: usize, rows: usize) -> Vec<String> {
    let mut screen = vec![vec![' '; cols]; rows];
    let mut cursor_x = 0usize;
    let mut cursor_y = 0usize;
    let mut i = 0usize;

    while i < bytes.len() {
        match bytes[i] {
            b'\x1b' => {
                if i + 1 < bytes.len() && bytes[i + 1] == b'[' {
                    if let Some((next, final_byte, params)) = parse_csi(bytes, i + 2) {
                        if final_byte == b'H' {
                            let (row, col) = parse_cursor_position(params);
                            cursor_y = row.min(rows.saturating_sub(1));
                            cursor_x = col.min(cols.saturating_sub(1));
                        }
                        if final_byte == b'J' && params == "2" {
                            for row in &mut screen {
                                for cell in row {
                                    *cell = ' ';
                                }
                            }
                        }
                        i = next;
                    } else {
                        i += 1;
                    }
                } else {
                    i += 1;
                }
            }
            b'\r' => {
                cursor_x = 0;
                i += 1;
            }
            b'\n' => {
                cursor_y = (cursor_y + 1).min(rows.saturating_sub(1));
                i += 1;
            }
            _ => {
                let s = std::str::from_utf8(&bytes[i..]).expect("valid utf-8 render output");
                let ch = s.chars().next().expect("char exists");
                if cursor_y < rows && cursor_x < cols {
                    screen[cursor_y][cursor_x] = ch;
                }
                cursor_x = (cursor_x + 1).min(cols.saturating_sub(1));
                i += ch.len_utf8();
            }
        }
    }

    screen
        .into_iter()
        .map(|row| {
            row.into_iter()
                .collect::<String>()
                .trim_end_matches(' ')
                .to_string()
        })
        .collect()
}

fn parse_csi(bytes: &[u8], start: usize) -> Option<(usize, u8, &str)> {
    let mut idx = start;
    while idx < bytes.len() {
        let byte = bytes[idx];
        if (0x40..=0x7e).contains(&byte) {
            let params = std::str::from_utf8(&bytes[start..idx]).ok()?;
            return Some((idx + 1, byte, params));
        }
        idx += 1;
    }
    None
}

fn parse_cursor_position(params: &str) -> (usize, usize) {
    let mut parts = params.split(';');
    let row_1_based = parts
        .next()
        .and_then(|s| s.parse::<usize>().ok())
        .unwrap_or(1);
    let col_1_based = parts
        .next()
        .and_then(|s| s.parse::<usize>().ok())
        .unwrap_or(1);
    (row_1_based.saturating_sub(1), col_1_based.saturating_sub(1))
}
