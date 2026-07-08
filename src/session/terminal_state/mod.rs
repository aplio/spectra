mod grid;
mod modes;
mod passthrough;
mod reflow;
#[cfg(test)]
mod tests;

pub use modes::MouseProtocol;

use modes::KittyKeyboardStack;
use passthrough::TmuxPassthroughState;

use std::collections::VecDeque;
use std::sync::Arc;

use crossterm::style::Color;
use unicode_width::UnicodeWidthChar;
use vte::{Params, Parser, Perform};

use crate::io::host_colors::HostColors;
pub use crate::ui::style::CellStyle;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StyledCell {
    pub ch: char,
    pub style: CellStyle,
    /// Explicit hyperlink target set by the guest via OSC 8, if any.
    pub link: Option<Arc<str>>,
}

impl Default for StyledCell {
    fn default() -> Self {
        Self {
            ch: ' ',
            style: CellStyle::default(),
            link: None,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
enum RowBoundary {
    #[default]
    None,
    SoftWrap,
    HardLf,
}

#[derive(Debug, Clone)]
struct HistoryLine {
    text: String,
    cells: Vec<StyledCell>,
    boundary_to_next: RowBoundary,
}

struct LogicalLine {
    cells: Vec<StyledCell>,
    trailing_boundary: RowBoundary,
}

fn trim_trailing_default_cells(cells: &mut Vec<StyledCell>) {
    while let Some(last) = cells.last() {
        if *last == StyledCell::default() {
            cells.pop();
        } else {
            break;
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TerminalEvent {
    TitleChanged {
        title: Option<String>,
    },
    CwdChanged {
        cwd: String,
    },
    /// The guest program set the clipboard via OSC 52.
    ClipboardSet {
        text: String,
    },
    /// The guest requested a desktop notification (OSC 9 iTerm2-style or
    /// OSC 777;notify).
    Notification {
        message: String,
    },
    /// The guest reported command progress (ConEmu OSC 9;4). `None` removes
    /// a previously shown progress indicator.
    ProgressChanged {
        progress: Option<ProgressReport>,
    },
}

/// Semantic prompt / shell integration state reported via OSC 133. Rows are
/// absolute (scrollback + viewport).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct SemanticPrompt {
    /// Row of the most recent prompt start (133;A).
    pub prompt_abs_row: Option<usize>,
    /// Row of the most recent input start (133;B).
    pub input_abs_row: Option<usize>,
    /// Row of the most recent command-output start (133;C).
    pub output_abs_row: Option<usize>,
    /// A 133;C was seen without a matching 133;D yet.
    pub command_running: bool,
    /// Exit code carried by the most recent 133;D, when present.
    pub last_exit_code: Option<i32>,
}

/// Kind of a ConEmu OSC 9;4 progress report.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProgressState {
    /// `9;4;1;pr` — determinate progress.
    Normal,
    /// `9;4;2[;pr]` — error state.
    Error,
    /// `9;4;3` — indeterminate (busy spinner).
    Indeterminate,
    /// `9;4;4[;pr]` — paused / warning.
    Paused,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ProgressReport {
    pub state: ProgressState,
    /// 0-100. `None` for states reported without a percentage.
    pub percent: Option<u8>,
}

/// Upper bound for an inbound OSC 52 base64 payload (~256 KiB of decoded
/// clipboard text, matching common terminal caps). vte's std parser
/// accumulates OSC payloads in an unbounded Vec, so the cap is enforced at
/// dispatch time.
const MAX_OSC52_BASE64_LEN: usize = 256 * 1024 * 4 / 3 + 4;

/// Upper bound for an OSC 8 hyperlink URI (the conventional maximum URL
/// length). Longer URIs are dropped rather than truncated.
const MAX_OSC8_URI_LEN: usize = 2083;

pub struct TerminalState {
    parser: Parser,
    grid: TerminalGrid,
    tmux_passthrough_state: TmuxPassthroughState,
}

impl TerminalState {
    pub fn new(width: usize, height: usize) -> Self {
        Self::new_with_passthrough(width, height, true)
    }

    pub fn new_with_passthrough(width: usize, height: usize, allow_passthrough: bool) -> Self {
        Self {
            parser: Parser::new(),
            grid: TerminalGrid::new(width, height, allow_passthrough),
            tmux_passthrough_state: TmuxPassthroughState::default(),
        }
    }

    pub fn resize(&mut self, width: usize, height: usize) {
        self.grid.resize(width, height);
    }

    pub fn feed(&mut self, bytes: &[u8]) {
        if !self.grid.allow_passthrough {
            self.parser.advance(&mut self.grid, bytes);
            return;
        }
        let filtered = self.filter_tmux_passthrough(bytes);
        if !filtered.is_empty() {
            self.parser.advance(&mut self.grid, &filtered);
        }
    }

    pub fn set_allow_passthrough(&mut self, allow_passthrough: bool) {
        self.grid.set_allow_passthrough(allow_passthrough);
        if !allow_passthrough {
            self.tmux_passthrough_state = TmuxPassthroughState::Ground;
        }
    }

    /// Cap scrollback retention (visual rows). Lowering the cap trims the
    /// oldest history immediately.
    pub fn set_max_scrollback(&mut self, lines: usize) {
        self.grid.set_max_scrollback(lines);
    }

    pub fn allow_passthrough(&self) -> bool {
        self.grid.allow_passthrough
    }

    pub fn drain_passthrough(&mut self) -> Vec<Vec<u8>> {
        std::mem::take(&mut self.grid.passthrough_queue)
    }

    pub fn drain_events(&mut self) -> Vec<TerminalEvent> {
        std::mem::take(&mut self.grid.terminal_events)
    }

    /// Drain any pending response bytes (e.g. cursor position reports).
    pub fn drain_responses(&mut self) -> Vec<Vec<u8>> {
        std::mem::take(&mut self.grid.response_queue)
    }

    /// Whether the guest program enabled bracketed paste (DECSET 2004).
    pub fn bracketed_paste(&self) -> bool {
        self.grid.bracketed_paste
    }

    /// Update the host terminal default colors used to answer guest
    /// OSC 10/11 queries (reported by the most recently attached client).
    pub fn set_host_colors(&mut self, colors: HostColors) {
        self.grid.host_colors = colors;
    }

    /// Host terminal default colors currently cached for OSC 10/11.
    pub fn host_colors(&self) -> HostColors {
        self.grid.host_colors
    }

    /// Whether the guest asked to hold frame output (DECSET 2026) and the
    /// hold has not yet exceeded [`SYNC_OUTPUT_TIMEOUT`].
    pub fn synchronized_output_active(&self) -> bool {
        self.grid
            .sync_output_since
            .is_some_and(|since| since.elapsed() < SYNC_OUTPUT_TIMEOUT)
    }

    /// Instant at which the currently active synchronized-output hold times
    /// out; `None` when no hold is active (never requested, released, or
    /// already past [`SYNC_OUTPUT_TIMEOUT`]).
    pub fn sync_output_deadline(&self) -> Option<std::time::Instant> {
        let deadline = self.grid.sync_output_since? + SYNC_OUTPUT_TIMEOUT;
        (std::time::Instant::now() < deadline).then_some(deadline)
    }

    /// Mouse reporting level requested by the guest program.
    pub fn mouse_protocol(&self) -> MouseProtocol {
        self.grid.mouse_protocol
    }

    /// Kitty keyboard protocol flags currently in effect for the active
    /// screen (`0` when the guest never enabled the protocol).
    pub fn kitty_keyboard_flags(&self) -> u8 {
        self.grid.kitty_keyboard_flags()
    }

    /// Absolute row (scrollback + viewport) of the most recent OSC 133;A
    /// shell prompt mark, if the guest shell reports semantic prompts.
    pub fn last_prompt_abs_row(&self) -> Option<usize> {
        self.grid.semantic_prompt.prompt_abs_row
    }

    /// Full semantic prompt state (OSC 133 A/B/C/D marks).
    pub fn semantic_prompt(&self) -> SemanticPrompt {
        self.grid.semantic_prompt
    }

    /// Progress reported by the guest via ConEmu OSC 9;4, if active.
    pub fn progress(&self) -> Option<ProgressReport> {
        self.grid.progress
    }

    /// Cursor color set by the guest via OSC 12, if any.
    pub fn cursor_color(&self) -> Option<(u8, u8, u8)> {
        self.grid.cursor_color_override
    }

    pub fn row_text(&self, row: usize) -> String {
        self.grid.row_text(row)
    }

    pub fn row_cells(&self, row: usize) -> Vec<StyledCell> {
        self.grid.resolve_cell_colors(self.grid.row_cells(row))
    }

    pub fn absolute_row_cells(&self, absolute_row: usize) -> Vec<StyledCell> {
        self.grid
            .resolve_cell_colors(self.grid.absolute_row_cells(absolute_row))
    }

    pub fn history_len(&self) -> usize {
        self.grid.history_len()
    }

    pub fn total_lines(&self) -> usize {
        self.grid.total_lines()
    }

    pub fn width(&self) -> usize {
        self.grid.width
    }

    pub fn height(&self) -> usize {
        self.grid.height
    }

    pub fn cursor(&self) -> (usize, usize) {
        (self.grid.cursor_x, self.grid.cursor_y)
    }

    pub fn cursor_style(&self) -> crossterm::cursor::SetCursorStyle {
        self.grid.cursor_style
    }

    /// Whether the guest wants the cursor shown (DECTCEM, CSI ?25 h/l).
    pub fn cursor_visible(&self) -> bool {
        self.grid.cursor_visible
    }

    pub fn scrollback_text(&self) -> String {
        self.grid.scrollback_text()
    }

    pub fn history_lines(&self) -> Vec<String> {
        self.grid.history_lines()
    }

    pub fn history_cells(&self) -> Vec<Vec<StyledCell>> {
        self.grid
            .history_cells()
            .into_iter()
            .map(|cells| self.grid.resolve_cell_colors(cells))
            .collect()
    }

    /// Per-row soft-wrap flags matching [`Self::history_lines`]: `true` when
    /// that row soft-wraps into the next one (no real LF between them).
    pub fn history_soft_wraps(&self) -> Vec<bool> {
        self.grid.history_soft_wraps()
    }

    /// Whether the row at `absolute_row` continues onto the next row via a
    /// soft wrap.
    pub fn absolute_row_soft_wrapped(&self, absolute_row: usize) -> bool {
        self.grid.absolute_row_soft_wrapped(absolute_row)
    }

    pub fn history_tail_lines(&self, max_lines: usize) -> Vec<String> {
        self.grid.history_tail_lines(max_lines)
    }

    pub fn export_text_hard_lf(&self) -> String {
        self.grid.export_text_hard_lf()
    }
}

struct SavedScreen {
    cells: Vec<StyledCell>,
    scrollback: VecDeque<HistoryLine>,
    row_boundaries: Vec<RowBoundary>,
    cursor_x: usize,
    cursor_y: usize,
    active_style: CellStyle,
    scroll_top: usize,
    scroll_bottom: usize,
}

struct TerminalGrid {
    width: usize,
    height: usize,
    cells: Vec<StyledCell>,
    /// Physical row (within `cells`) that visual row 0 maps to. The visible
    /// screen is a ring: full-screen scrolls advance this origin instead of
    /// shifting every cell, so bulk output scrolls in O(width) per line
    /// rather than O(width * height). Row helpers translate visual rows
    /// through it; whole-buffer operations call `normalize_ring` first.
    /// `row_boundaries` stays visually indexed (rotated on scroll).
    row0: usize,
    /// Scrollback is a deque because the hot path once it is full is
    /// push-one-line + evict-one-line per scrolled row: with a Vec the
    /// eviction (`drain(0..1)`) memmoves the entire history on every
    /// newline, which dominated flood-ingest profiles.
    scrollback: VecDeque<HistoryLine>,
    row_boundaries: Vec<RowBoundary>,
    scroll_top: usize,
    scroll_bottom: usize,
    cursor_x: usize,
    cursor_y: usize,
    active_style: CellStyle,
    saved_cursor_x: usize,
    saved_cursor_y: usize,
    saved_style: CellStyle,
    cursor_style: crossterm::cursor::SetCursorStyle,
    /// Cursor visibility requested via DECTCEM (CSI ?25 h/l). Like real
    /// terminals this is global, not per-screen: entering or leaving the
    /// alternate screen does not change it.
    cursor_visible: bool,
    saved_screen: Option<SavedScreen>,
    /// Bytes to send back to the child process (e.g. cursor position reports).
    response_queue: Vec<Vec<u8>>,
    /// Insert Replacement Mode (IRM, CSI 4 h/l). When true, printing shifts
    /// existing characters to the right instead of overwriting.
    insert_mode: bool,
    /// Bracketed paste (DECSET 2004) requested by the guest program. When
    /// true, pasted text must be wrapped in ESC[200~ / ESC[201~ markers.
    bracketed_paste: bool,
    /// Synchronized output (DECSET 2026): while set, frames should not be
    /// flushed to clients. The timestamp bounds the hold so a misbehaving
    /// guest cannot freeze rendering.
    sync_output_since: Option<std::time::Instant>,
    /// Semantic prompt marks reported via OSC 133 (shell integration).
    semantic_prompt: SemanticPrompt,
    /// Progress reported via ConEmu OSC 9;4, if any is active.
    progress: Option<ProgressReport>,
    /// Palette entries redefined by the guest via OSC 4 (index → RGB).
    /// Resolved when cells are read for rendering, so overrides stay
    /// pane-local and never touch the host terminal's palette.
    palette_overrides: std::collections::HashMap<u8, (u8, u8, u8)>,
    /// Default foreground set by the guest via OSC 10, applied at render
    /// time to cells without an explicit foreground.
    default_fg_override: Option<(u8, u8, u8)>,
    /// Default background set by the guest via OSC 11.
    default_bg_override: Option<(u8, u8, u8)>,
    /// Cursor color set by the guest via OSC 12; forwarded to the host
    /// terminal while this pane is focused.
    cursor_color_override: Option<(u8, u8, u8)>,
    /// Mouse reporting level requested via DECSET 9/1000/1002/1003.
    mouse_protocol: MouseProtocol,
    /// SGR mouse encoding (DECSET 1006). Without it the legacy X10 byte
    /// encoding is used, which caps coordinates at 223.
    mouse_sgr: bool,
    allow_passthrough: bool,
    passthrough_queue: Vec<Vec<u8>>,
    terminal_events: Vec<TerminalEvent>,
    /// Default fg/bg colors mirrored from the most recently attached
    /// client's host terminal; used to answer guest OSC 10/11 queries.
    host_colors: HostColors,
    /// Hyperlink target opened by OSC 8 and not yet closed. Newly printed
    /// cells are stamped with this link.
    active_link: Option<Arc<str>>,
    /// Kitty keyboard protocol flag stack for the main screen.
    kitty_kbd_main: KittyKeyboardStack,
    /// Kitty keyboard protocol flag stack for the alternate screen. The
    /// kitty spec keeps separate stacks for the main and alternate screens
    /// so a fullscreen app enabling the protocol cannot leak flags back to
    /// the shell when it exits.
    kitty_kbd_alt: KittyKeyboardStack,
    /// Scrollback retention cap in visual rows (`[terminal] scrollback_lines`).
    max_scrollback: usize,
}

/// Default scrollback retention (`[terminal] scrollback_lines`).
pub const DEFAULT_SCROLLBACK_LINES: usize = 10_000;

/// Upper bound on how long a synchronized-output hold (DECSET 2026) may
/// suppress rendering. Mirrors the ~150 ms cap used by other terminals so a
/// guest that never sends the reset cannot freeze the UI.
pub const SYNC_OUTPUT_TIMEOUT: std::time::Duration = std::time::Duration::from_millis(150);
