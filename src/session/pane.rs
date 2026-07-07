use std::io;
use std::path::{Path, PathBuf};

use crate::io::host_colors::HostColors;
use crate::session::terminal_state::{StyledCell, TerminalEvent, TerminalState};

/// Raw output retained per pane for replay across a live server handoff.
/// Kept small on purpose: enough to repaint the visible screen, not the
/// scrollback (herdr uses the same 8 KiB budget).
pub const MAX_REPLAY_BYTES_PER_PANE: usize = 8 * 1024;

pub trait PaneBackend: Send {
    fn write(&mut self, bytes: &[u8]) -> io::Result<()>;
    fn resize(&mut self, cols: u16, rows: u16) -> io::Result<()>;
    fn poll_output(&mut self) -> Vec<Vec<u8>>;
    fn is_closed(&mut self) -> bool {
        false
    }
    /// Pid of the process spawned for this pane, when known (real PTY
    /// backends only). Used for best-effort agent detection.
    fn child_pid(&self) -> Option<u32> {
        None
    }
    /// Raw PTY master fd for a live server handoff; `None` when the backend
    /// has no transferable descriptor (fake/test backends).
    #[cfg(unix)]
    fn handoff_master_fd(&self) -> Option<std::os::fd::RawFd> {
        None
    }
    /// Stop killing the pane child when this backend drops. Called by the
    /// outgoing server once its successor has acked receipt of the PTY fds,
    /// so process exit leaves the children running.
    fn disarm_child_kill(&mut self) {}
}

pub struct Pane {
    terminal: TerminalState,
    backend: Box<dyn PaneBackend>,
    view_scroll_offset: usize,
    pending_passthrough: Vec<Vec<u8>>,
    pending_terminal_events: Vec<TerminalEvent>,
    /// Last ≤[`MAX_REPLAY_BYTES_PER_PANE`] raw output bytes, kept so a live
    /// server handoff can repaint the pane in the successor process.
    replay_tail: Vec<u8>,
    /// Working directory reported by the guest via OSC 7 (or seeded from the
    /// spawn cwd). New splits/windows spawn here so they inherit the focused
    /// pane's directory. `None` until the shell emits its first OSC 7 and no
    /// spawn cwd was set.
    cwd: Option<PathBuf>,
}

impl Pane {
    pub fn new(
        width: usize,
        height: usize,
        allow_passthrough: bool,
        backend: Box<dyn PaneBackend>,
    ) -> Self {
        Self {
            terminal: TerminalState::new_with_passthrough(width, height, allow_passthrough),
            backend,
            view_scroll_offset: 0,
            pending_passthrough: Vec::new(),
            pending_terminal_events: Vec::new(),
            replay_tail: Vec::new(),
            cwd: None,
        }
    }

    /// Working directory last reported by the guest (OSC 7) or seeded at spawn.
    pub fn cwd(&self) -> Option<&Path> {
        self.cwd.as_deref()
    }

    /// Seed/override the tracked cwd. Called by `spawn_pane` with the spawn
    /// cwd, and by the handoff path with the transferred cwd metadata.
    pub fn set_cwd(&mut self, cwd: Option<PathBuf>) {
        self.cwd = cwd;
    }

    pub fn write(&mut self, bytes: &[u8]) -> io::Result<()> {
        self.backend.write(bytes)
    }

    pub fn resize(&mut self, width: usize, height: usize) -> io::Result<()> {
        self.terminal.resize(width, height);
        self.backend.resize(width as u16, height as u16)
    }

    pub fn scroll_view(&mut self, lines: isize, view_rows: usize) {
        let max_offset = self.max_view_scroll_offset(view_rows);
        if max_offset == 0 || lines == 0 {
            self.view_scroll_offset = 0;
            return;
        }

        if lines.is_negative() {
            self.view_scroll_offset = self.view_scroll_offset.saturating_sub(lines.unsigned_abs());
        } else {
            self.view_scroll_offset = self
                .view_scroll_offset
                .saturating_add(lines as usize)
                .min(max_offset);
        }
    }

    pub fn reset_view_scroll(&mut self) -> bool {
        if self.view_scroll_offset == 0 {
            return false;
        }
        self.view_scroll_offset = 0;
        true
    }

    pub fn poll_output(&mut self) -> bool {
        let mut changed = false;
        let preserve_view_origin = if self.view_scroll_offset > 0 {
            let view_rows = self.terminal.height().max(1);
            Some(self.view_row_origin(view_rows))
        } else {
            None
        };
        for chunk in self.backend.poll_output() {
            self.terminal.feed(&chunk);
            self.push_replay_tail(&chunk);
            self.pending_passthrough
                .extend(self.terminal.drain_passthrough());
            let events = self.terminal.drain_events();
            for event in &events {
                if let TerminalEvent::CwdChanged { cwd } = event {
                    self.cwd = Some(PathBuf::from(cwd));
                }
            }
            self.pending_terminal_events.extend(events);
            changed = true;
        }
        if changed && let Some(target_origin) = preserve_view_origin {
            let view_rows = self.terminal.height().max(1);
            let follow_origin = self.follow_row_origin(view_rows);
            let clamped_origin = target_origin.min(follow_origin);
            self.view_scroll_offset = follow_origin.saturating_sub(clamped_origin);
        }
        // Send any terminal responses (e.g. cursor position reports) back
        for response in self.terminal.drain_responses() {
            let _ = self.backend.write(&response);
        }
        changed
    }

    pub fn set_allow_passthrough(&mut self, allow_passthrough: bool) {
        self.terminal.set_allow_passthrough(allow_passthrough);
        if !allow_passthrough {
            self.pending_passthrough.clear();
        }
    }

    pub fn allow_passthrough(&self) -> bool {
        self.terminal.allow_passthrough()
    }

    pub fn take_passthrough(&mut self) -> Vec<Vec<u8>> {
        std::mem::take(&mut self.pending_passthrough)
    }

    pub fn bracketed_paste(&self) -> bool {
        self.terminal.bracketed_paste()
    }

    /// Update the host terminal default colors used to answer guest
    /// OSC 10/11 queries.
    pub fn set_host_colors(&mut self, colors: HostColors) {
        self.terminal.set_host_colors(colors);
    }

    /// Host terminal default colors currently cached for OSC 10/11.
    pub fn host_colors(&self) -> HostColors {
        self.terminal.host_colors()
    }

    /// Kitty keyboard protocol flags enabled by the guest for the active
    /// screen (0 when the protocol is not in use).
    pub fn kitty_keyboard_flags(&self) -> u8 {
        self.terminal.kitty_keyboard_flags()
    }

    pub fn wants_mouse_reporting(&self) -> bool {
        self.terminal.mouse_protocol() != crate::session::terminal_state::MouseProtocol::None
    }

    pub fn encode_mouse_event(
        &self,
        kind: crossterm::event::MouseEventKind,
        modifiers: crossterm::event::KeyModifiers,
        col: usize,
        row: usize,
    ) -> Option<Vec<u8>> {
        self.terminal.encode_mouse_event(kind, modifiers, col, row)
    }

    pub fn synchronized_output_active(&self) -> bool {
        self.terminal.synchronized_output_active()
    }

    /// See [`crate::session::terminal_state::TerminalState::sync_output_deadline`].
    pub fn sync_output_deadline(&self) -> Option<std::time::Instant> {
        self.terminal.sync_output_deadline()
    }

    pub fn take_terminal_events(&mut self) -> Vec<TerminalEvent> {
        std::mem::take(&mut self.pending_terminal_events)
    }

    pub fn row_text(&self, row: usize) -> String {
        self.terminal.row_text(row)
    }

    pub fn row_cells(&self, row: usize) -> Vec<StyledCell> {
        self.terminal.row_cells(row)
    }

    pub fn absolute_row_cells(&self, absolute_row: usize) -> Vec<StyledCell> {
        self.terminal.absolute_row_cells(absolute_row)
    }

    pub fn total_lines(&self) -> usize {
        self.terminal.total_lines()
    }

    pub fn screen_rows(&self) -> usize {
        self.terminal.height()
    }

    pub fn cursor(&self) -> (usize, usize) {
        self.terminal.cursor()
    }

    pub fn cursor_style(&self) -> crossterm::cursor::SetCursorStyle {
        self.terminal.cursor_style()
    }

    pub fn scrollback_text(&self) -> String {
        self.terminal.scrollback_text()
    }

    pub fn history_lines(&self) -> Vec<String> {
        self.terminal.history_lines()
    }

    pub fn history_cells(&self) -> Vec<Vec<StyledCell>> {
        self.terminal.history_cells()
    }

    pub fn history_tail_lines(&self, max_lines: usize) -> Vec<String> {
        self.terminal.history_tail_lines(max_lines)
    }

    pub fn row_cells_for_view(&self, view_rows: usize) -> Vec<Vec<StyledCell>> {
        if view_rows == 0 {
            return Vec::new();
        }
        let row_origin = self.view_row_origin(view_rows);
        (0..view_rows)
            .map(|row| self.terminal.absolute_row_cells(row_origin + row))
            .collect()
    }

    pub fn cursor_row_in_view(&self, view_rows: usize) -> Option<usize> {
        if view_rows == 0 {
            return None;
        }
        let cursor_absolute_row = self.terminal.history_len() + self.terminal.cursor().1;
        let row_origin = self.view_row_origin(view_rows);

        if cursor_absolute_row < row_origin {
            return None;
        }

        let cursor_view_row = cursor_absolute_row - row_origin;
        (cursor_view_row < view_rows).then_some(cursor_view_row)
    }

    pub fn view_row_origin_for(&self, view_rows: usize) -> usize {
        self.view_row_origin(view_rows)
    }

    pub fn cursor_absolute_position(&self) -> (usize, usize) {
        let (col, row) = self.terminal.cursor();
        (col, self.terminal.history_len() + row)
    }

    fn max_view_scroll_offset(&self, view_rows: usize) -> usize {
        self.follow_row_origin(view_rows)
    }

    fn follow_row_origin(&self, view_rows: usize) -> usize {
        if view_rows == 0 {
            return 0;
        }
        let history_len = self.terminal.history_len();
        let cursor_absolute_row = history_len + self.terminal.cursor().1;
        let max_origin = self.terminal.total_lines().saturating_sub(view_rows);
        cursor_absolute_row
            .saturating_add(1)
            .saturating_sub(view_rows)
            .max(history_len)
            .min(max_origin)
    }

    fn view_row_origin(&self, view_rows: usize) -> usize {
        if view_rows == 0 {
            return 0;
        }
        let follow_origin = self.follow_row_origin(view_rows);
        let offset = self.view_scroll_offset.min(follow_origin);
        follow_origin.saturating_sub(offset)
    }

    pub fn export_text_hard_lf(&self) -> String {
        self.terminal.export_text_hard_lf()
    }

    pub fn is_closed(&mut self) -> bool {
        self.backend.is_closed()
    }

    pub fn child_pid(&self) -> Option<u32> {
        self.backend.child_pid()
    }

    /// Raw PTY master fd for a live server handoff, when the backend has one.
    #[cfg(unix)]
    pub fn handoff_master_fd(&self) -> Option<std::os::fd::RawFd> {
        self.backend.handoff_master_fd()
    }

    /// See [`PaneBackend::disarm_child_kill`].
    pub fn disarm_child_kill(&mut self) {
        self.backend.disarm_child_kill();
    }

    /// Last ≤[`MAX_REPLAY_BYTES_PER_PANE`] raw output bytes seen by this pane.
    pub fn replay_tail(&self) -> &[u8] {
        &self.replay_tail
    }

    /// Feed transferred replay bytes straight into the terminal state after
    /// a live handoff. Side effects are discarded: passthrough frames and
    /// terminal responses were already delivered by the previous server, so
    /// re-emitting them would duplicate output or inject stray bytes.
    pub fn feed_replay(&mut self, bytes: &[u8]) {
        self.terminal.feed(bytes);
        let _ = self.terminal.drain_passthrough();
        let _ = self.terminal.drain_events();
        let _ = self.terminal.drain_responses();
        self.push_replay_tail(bytes);
    }

    fn push_replay_tail(&mut self, bytes: &[u8]) {
        if bytes.len() >= MAX_REPLAY_BYTES_PER_PANE {
            self.replay_tail.clear();
            self.replay_tail
                .extend_from_slice(&bytes[bytes.len() - MAX_REPLAY_BYTES_PER_PANE..]);
            return;
        }
        let overflow =
            (self.replay_tail.len() + bytes.len()).saturating_sub(MAX_REPLAY_BYTES_PER_PANE);
        if overflow > 0 {
            self.replay_tail.drain(..overflow);
        }
        self.replay_tail.extend_from_slice(bytes);
    }
}

pub struct FakeBackend {
    output: Vec<Vec<u8>>,
    pub writes: Vec<Vec<u8>>,
    pub last_size: Option<(u16, u16)>,
}

impl FakeBackend {
    pub fn new(output: Vec<Vec<u8>>) -> Self {
        Self {
            output,
            writes: Vec::new(),
            last_size: None,
        }
    }
}

impl PaneBackend for FakeBackend {
    fn write(&mut self, bytes: &[u8]) -> io::Result<()> {
        self.writes.push(bytes.to_vec());
        Ok(())
    }

    fn resize(&mut self, cols: u16, rows: u16) -> io::Result<()> {
        self.last_size = Some((cols, rows));
        Ok(())
    }

    fn poll_output(&mut self) -> Vec<Vec<u8>> {
        std::mem::take(&mut self.output)
    }
}

#[cfg(test)]
mod tests {
    use super::{FakeBackend, MAX_REPLAY_BYTES_PER_PANE, Pane};

    fn pane_with_output(chunks: Vec<Vec<u8>>) -> Pane {
        Pane::new(80, 24, false, Box::new(FakeBackend::new(chunks)))
    }

    #[test]
    fn replay_tail_records_polled_output() {
        let mut pane = pane_with_output(vec![b"hello ".to_vec(), b"world".to_vec()]);
        assert!(pane.poll_output());
        assert_eq!(pane.replay_tail(), b"hello world");
    }

    #[test]
    fn poll_output_tracks_osc7_cwd() {
        let osc7 = b"\x1b]7;file://localhost/tmp/some/dir\x07".to_vec();
        let mut pane = pane_with_output(vec![osc7]);
        assert_eq!(pane.cwd(), None);
        assert!(pane.poll_output());
        assert_eq!(pane.cwd(), Some(std::path::Path::new("/tmp/some/dir")));
    }

    #[test]
    fn replay_tail_is_capped_at_the_replay_budget() {
        let big = vec![b'x'; MAX_REPLAY_BYTES_PER_PANE + 100];
        let mut pane = pane_with_output(vec![big]);
        assert!(pane.poll_output());
        assert_eq!(pane.replay_tail().len(), MAX_REPLAY_BYTES_PER_PANE);

        // Small chunks after a full tail keep only the newest bytes.
        let mut pane = pane_with_output(vec![
            vec![b'a'; MAX_REPLAY_BYTES_PER_PANE],
            b"tail-marker".to_vec(),
        ]);
        assert!(pane.poll_output());
        let tail = pane.replay_tail();
        assert_eq!(tail.len(), MAX_REPLAY_BYTES_PER_PANE);
        assert!(tail.ends_with(b"tail-marker"));
    }

    #[test]
    fn feed_replay_populates_grid_without_response_side_effects() {
        let mut pane = pane_with_output(vec![]);
        // Include a cursor-position query: the reply must be discarded, not
        // written back to the (already answered) guest.
        pane.feed_replay(b"restored-line\x1b[6n");
        assert!(pane.row_text(0).contains("restored-line"));
        assert_eq!(pane.replay_tail(), b"restored-line\x1b[6n");
    }
}
