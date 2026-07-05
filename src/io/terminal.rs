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

/// Build an OSC 2 escape sequence for setting the host window title.
pub fn osc2_title_sequence(title: &str) -> String {
    format!("\x1b]2;{title}\x07")
}

/// Build an OSC 9 desktop-notification escape sequence, stripping control
/// characters from the message so it cannot break out of the sequence.
pub fn osc9_notification_sequence(message: &str) -> String {
    let sanitized: String = message.chars().filter(|ch| !ch.is_control()).collect();
    format!("\x1b]9;{sanitized}\x07")
}

#[cfg(test)]
mod tests {
    use super::{osc2_title_sequence, osc9_notification_sequence};

    #[test]
    fn osc2_title_sequence_uses_bell_terminator() {
        assert_eq!(osc2_title_sequence("build"), "\x1b]2;build\x07");
    }

    #[test]
    fn osc9_notification_sequence_strips_control_characters() {
        assert_eq!(
            osc9_notification_sequence("spectra: claude blocked (pane 3)"),
            "\x1b]9;spectra: claude blocked (pane 3)\x07"
        );
        assert_eq!(
            osc9_notification_sequence("bad\x07\x1b]2;title\rmessage"),
            "\x1b]9;bad]2;titlemessage\x07"
        );
    }
}
