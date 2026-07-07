use std::io;
use std::sync::{Arc, Mutex};

use spectra::session::manager::{SessionManager, SessionOptions};
use spectra::session::pane::PaneBackend;
use spectra::session::pty_backend::{PaneFactory, PaneSpawnConfig};
use spectra::session::terminal_state::{ProgressReport, ProgressState, TerminalEvent};

const COLS: u16 = 20;
const ROWS: u16 = 4;

/// Backend that replays canned guest output and records everything the
/// server writes back (terminal query replies).
struct RecordingBackend {
    chunks: Vec<Vec<u8>>,
    writes: Arc<Mutex<Vec<Vec<u8>>>>,
}

impl PaneBackend for RecordingBackend {
    fn write(&mut self, bytes: &[u8]) -> io::Result<()> {
        self.writes.lock().unwrap().push(bytes.to_vec());
        Ok(())
    }

    fn resize(&mut self, _cols: u16, _rows: u16) -> io::Result<()> {
        Ok(())
    }

    fn poll_output(&mut self) -> Vec<Vec<u8>> {
        std::mem::take(&mut self.chunks)
    }
}

struct RecordingFactory {
    bytes: Vec<u8>,
    writes: Arc<Mutex<Vec<Vec<u8>>>>,
}

impl PaneFactory for RecordingFactory {
    fn spawn(&self, _config: &PaneSpawnConfig) -> io::Result<Box<dyn PaneBackend>> {
        Ok(Box::new(RecordingBackend {
            chunks: vec![self.bytes.clone()],
            writes: Arc::clone(&self.writes),
        }))
    }
}

struct Harness {
    session: SessionManager,
    writes: Arc<Mutex<Vec<Vec<u8>>>>,
}

fn feed(bytes: &[u8]) -> Harness {
    let options = SessionOptions::from_cli(Some("/bin/sh".to_string()), None, vec![]);
    let writes = Arc::new(Mutex::new(Vec::new()));
    let factory = RecordingFactory {
        bytes: bytes.to_vec(),
        writes: Arc::clone(&writes),
    };
    let mut session = SessionManager::with_factory(options, Arc::new(factory), COLS, ROWS)
        .expect("create session");
    session.poll_output();
    Harness { session, writes }
}

fn render(session: &SessionManager) -> Vec<u8> {
    let frame = session.frame(COLS, ROWS);
    let mut out = Vec::new();
    spectra::ui::render::render_to_writer(&mut out, &frame, "status", COLS, ROWS, true, None, None)
        .expect("render pane");
    out
}

fn contains(haystack: &[u8], needle: &[u8]) -> bool {
    haystack
        .windows(needle.len())
        .any(|window| window == needle)
}

fn replies(harness: &Harness) -> Vec<u8> {
    harness.writes.lock().unwrap().concat()
}

#[test]
fn osc4_palette_override_recolors_indexed_cells_in_rendered_output() {
    // Redefine palette index 1, then draw with SGR 31 (fg = index 1).
    let harness = feed(b"\x1b]4;1;#ff8000\x07\x1b[31mHi\x1b[0m");
    let output = render(&harness.session);
    assert!(contains(&output, b"Hi"), "expected pane text in output");
    assert!(
        contains(&output, b"38;2;255;128;0"),
        "expected the palette override as a truecolor SGR, got {:?}",
        String::from_utf8_lossy(&output)
    );

    // Without the override the same text renders as an indexed color.
    let plain = feed(b"\x1b[31mHi\x1b[0m");
    let output = render(&plain.session);
    assert!(!contains(&output, b"38;2;255;128;0"));
}

#[test]
fn osc10_11_overrides_recolor_default_cells_in_rendered_output() {
    let harness = feed(b"\x1b]10;#123456\x07\x1b]11;#654321\x07Hi");
    let output = render(&harness.session);
    assert!(
        contains(&output, b"38;2;18;52;86"),
        "expected OSC 10 default-fg override in output, got {:?}",
        String::from_utf8_lossy(&output)
    );
    assert!(
        contains(&output, b"48;2;101;67;33"),
        "expected OSC 11 default-bg override in output, got {:?}",
        String::from_utf8_lossy(&output)
    );
}

#[test]
fn osc_color_queries_reply_to_the_guest() {
    let harness =
        feed(b"\x1b]4;1;?\x07\x1b]10;#101010\x07\x1b]10;?\x07\x1b]12;#aabbcc\x07\x1b]12;?\x07");
    let replies = replies(&harness);
    assert!(
        contains(&replies, b"\x1b]4;1;rgb:cdcd/0000/0000\x07"),
        "expected default-palette OSC 4 reply, got {:?}",
        String::from_utf8_lossy(&replies)
    );
    assert!(
        contains(&replies, b"\x1b]10;rgb:1010/1010/1010\x07"),
        "expected OSC 10 override reply, got {:?}",
        String::from_utf8_lossy(&replies)
    );
    assert!(
        contains(&replies, b"\x1b]12;rgb:aaaa/bbbb/cccc\x07"),
        "expected OSC 12 override reply, got {:?}",
        String::from_utf8_lossy(&replies)
    );
}

#[test]
fn osc12_cursor_color_is_forwarded_to_the_host_and_reset() {
    let harness = feed(b"\x1b]12;#ff0000\x07");
    let frame = harness.session.frame(COLS, ROWS);
    assert_eq!(frame.cursor_color, Some((0xff, 0x00, 0x00)));

    let mut renderer = spectra::ui::render::FrameRenderer::new();
    let mut out = Vec::new();
    renderer
        .render_to_writer(&mut out, &frame, "status", COLS, ROWS, true, None, None)
        .expect("render pane");
    assert!(
        contains(&out, b"\x1b]12;rgb:ffff/0000/0000\x07"),
        "expected OSC 12 forwarded to the host, got {:?}",
        String::from_utf8_lossy(&out)
    );

    // A frame without a cursor color (guest reset it or focus moved to a
    // pane without one) restores the host default exactly once.
    let mut frame = frame;
    frame.cursor_color = None;
    let mut renderer_out = Vec::new();
    renderer
        .render_to_writer(
            &mut renderer_out,
            &frame,
            "status",
            COLS,
            ROWS,
            true,
            None,
            None,
        )
        .expect("render pane");
    assert!(
        contains(&renderer_out, b"\x1b]112\x07"),
        "expected OSC 112 reset, got {:?}",
        String::from_utf8_lossy(&renderer_out)
    );

    // Unchanged cursor color is not re-emitted.
    let mut third = Vec::new();
    renderer
        .render_to_writer(&mut third, &frame, "status", COLS, ROWS, true, None, None)
        .expect("render pane");
    assert!(!contains(&third, b"\x1b]112"));
    assert!(!contains(&third, b"\x1b]12;"));
}

#[test]
fn osc9_and_777_notifications_surface_as_events() {
    let mut harness = feed(b"\x1b]9;build finished\x07\x1b]777;notify;CI;job done\x07");
    let events: Vec<TerminalEvent> = harness
        .session
        .take_terminal_events()
        .into_iter()
        .map(|pane_event| pane_event.event)
        .collect();
    assert!(events.contains(&TerminalEvent::Notification {
        message: "build finished".to_string()
    }));
    assert!(events.contains(&TerminalEvent::Notification {
        message: "CI: job done".to_string()
    }));
}

#[test]
fn osc9_4_progress_surfaces_as_events() {
    let mut harness = feed(b"\x1b]9;4;1;42\x07\x1b]9;4;0\x07");
    let events: Vec<TerminalEvent> = harness
        .session
        .take_terminal_events()
        .into_iter()
        .map(|pane_event| pane_event.event)
        .collect();
    assert_eq!(
        events,
        vec![
            TerminalEvent::ProgressChanged {
                progress: Some(ProgressReport {
                    state: ProgressState::Normal,
                    percent: Some(42),
                }),
            },
            TerminalEvent::ProgressChanged { progress: None },
        ]
    );
}

#[test]
fn osc133_marks_survive_the_pane_pipeline() {
    use spectra::session::pane::{FakeBackend, Pane};

    let output =
        b"\x1b]133;A\x07$ \x1b]133;B\x07true\r\n\x1b]133;C\x07out\r\n\x1b]133;D;3\x07".to_vec();
    let mut pane = Pane::new(
        usize::from(COLS),
        usize::from(ROWS),
        false,
        Box::new(FakeBackend::new(vec![output])),
    );
    assert!(pane.poll_output());

    let prompt = pane.semantic_prompt();
    assert_eq!(prompt.prompt_abs_row, Some(0));
    assert_eq!(prompt.input_abs_row, Some(0));
    assert_eq!(prompt.output_abs_row, Some(1));
    assert!(!prompt.command_running);
    assert_eq!(prompt.last_exit_code, Some(3));
}
