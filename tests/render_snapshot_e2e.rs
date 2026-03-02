use std::io;
use std::sync::{Arc, Mutex};

use spectra::session::manager::{SessionManager, SessionOptions};
use spectra::session::pane::PaneBackend;
use spectra::session::pty_backend::{PaneFactory, PaneSpawnConfig};
use spectra::ui::window_manager::SplitAxis;

mod support;

const COLS: u16 = 50;
const ROWS: u16 = 10;

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
    count: Mutex<usize>,
}

impl StaticFactory {
    fn new() -> Self {
        Self {
            count: Mutex::new(0),
        }
    }
}

impl PaneFactory for StaticFactory {
    fn spawn(&self, _config: &PaneSpawnConfig) -> io::Result<Box<dyn PaneBackend>> {
        let mut count = self.count.lock().expect("lock count");
        *count += 1;
        let text = format!("pane-{} ready\r\n$ ", *count);
        Ok(Box::new(StaticBackend {
            chunks: vec![text.into_bytes()],
        }))
    }
}

#[test]
fn single_pane_snapshot() {
    let options = SessionOptions::from_cli(Some("/bin/sh".to_string()), None, vec![]);
    let mut session =
        SessionManager::with_factory(options, Arc::new(StaticFactory::new()), COLS, ROWS)
            .expect("create session");
    session.poll_output();

    let frame = session.frame(COLS, ROWS);
    let mut out = Vec::new();
    spectra::ui::render::render_to_writer(&mut out, &frame, "status", COLS, ROWS, true, None, None)
        .expect("render single pane");
    let rows = support::render_snapshot::ansi_bytes_to_rows(&out, COLS as usize, ROWS as usize);
    support::render_snapshot::assert_rows_match_fixture("single_pane", &rows);
}

#[test]
fn vertical_split_snapshot() {
    let options = SessionOptions::from_cli(Some("/bin/sh".to_string()), None, vec![]);
    let mut session =
        SessionManager::with_factory(options, Arc::new(StaticFactory::new()), COLS, ROWS)
            .expect("create session");
    session
        .split_focused(SplitAxis::Vertical, COLS, ROWS)
        .expect("split vertical");
    session.poll_output();

    let frame = session.frame(COLS, ROWS);
    let mut out = Vec::new();
    spectra::ui::render::render_to_writer(&mut out, &frame, "status", COLS, ROWS, true, None, None)
        .expect("render vertical split");
    let rows = support::render_snapshot::ansi_bytes_to_rows(&out, COLS as usize, ROWS as usize);
    support::render_snapshot::assert_rows_match_fixture("split_vertical", &rows);
}

#[test]
fn horizontal_split_snapshot() {
    let options = SessionOptions::from_cli(Some("/bin/sh".to_string()), None, vec![]);
    let mut session =
        SessionManager::with_factory(options, Arc::new(StaticFactory::new()), COLS, ROWS)
            .expect("create session");
    session
        .split_focused(SplitAxis::Horizontal, COLS, ROWS)
        .expect("split horizontal");
    session.poll_output();

    let frame = session.frame(COLS, ROWS);
    let mut out = Vec::new();
    spectra::ui::render::render_to_writer(&mut out, &frame, "status", COLS, ROWS, true, None, None)
        .expect("render horizontal split");
    let rows = support::render_snapshot::ansi_bytes_to_rows(&out, COLS as usize, ROWS as usize);
    support::render_snapshot::assert_rows_match_fixture("split_horizontal", &rows);
}
