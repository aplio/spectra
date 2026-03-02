use std::io::stdout;
use std::panic;

use crossterm::{
    cursor::{self, SetCursorStyle},
    event::{DisableBracketedPaste, DisableMouseCapture, EnableBracketedPaste, EnableMouseCapture},
    execute,
    terminal::{self, ClearType},
};

/// Enter raw mode, alternate screen, and install panic hook.
pub fn setup() -> std::io::Stdout {
    let mut stdout = stdout();
    terminal::enable_raw_mode().expect("Failed to enable raw mode");
    execute!(
        stdout,
        terminal::EnterAlternateScreen,
        terminal::Clear(ClearType::All),
        EnableBracketedPaste,
        EnableMouseCapture,
        cursor::Show,
    )
    .expect("Failed to setup terminal");

    let default_hook = panic::take_hook();
    panic::set_hook(Box::new(move |info| {
        let _ = terminal::disable_raw_mode();
        let _ = execute!(
            std::io::stdout(),
            SetCursorStyle::DefaultUserShape,
            DisableBracketedPaste,
            DisableMouseCapture,
            terminal::LeaveAlternateScreen
        );
        default_hook(info);
    }));

    stdout
}

/// Restore terminal to normal state.
pub fn teardown(mut stdout: std::io::Stdout) {
    let _ = terminal::disable_raw_mode();
    let _ = execute!(
        stdout,
        SetCursorStyle::DefaultUserShape,
        DisableBracketedPaste,
        DisableMouseCapture,
        terminal::LeaveAlternateScreen
    );
}
