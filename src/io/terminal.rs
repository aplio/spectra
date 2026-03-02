pub use crate::core_lib::terminal::lifecycle::{setup, teardown};

/// Build an OSC 2 escape sequence for setting the host window title.
pub fn osc2_title_sequence(title: &str) -> String {
    format!("\x1b]2;{title}\x07")
}

#[cfg(test)]
mod tests {
    use super::osc2_title_sequence;

    #[test]
    fn osc2_title_sequence_uses_bell_terminator() {
        assert_eq!(osc2_title_sequence("build"), "\x1b]2;build\x07");
    }
}
