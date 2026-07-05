//! Host terminal default foreground/background color discovery.
//!
//! spectra answers guest `OSC 10;?` / `OSC 11;?` queries from a server-side
//! cache instead of inventing fixed defaults (which would break dark/light
//! detection in guests). The cache is filled by the attaching client: right
//! after entering raw mode — and before crossterm's event stream starts
//! consuming terminal input — the client sends the same queries to its own
//! host terminal once, scans the replies with a bounded deadline, and
//! reports the result in the `Hello` handshake.

use serde::{Deserialize, Serialize};

/// Default foreground/background colors reported by a client's host
/// terminal. `None` components mean the host terminal never answered (or
/// the client could not ask), in which case guest queries stay unanswered.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct HostColors {
    #[serde(default)]
    pub fg: Option<(u8, u8, u8)>,
    #[serde(default)]
    pub bg: Option<(u8, u8, u8)>,
}

/// Total time budget for the startup OSC 10/11 reply window. Short enough
/// to be imperceptible at attach, long enough for a terminal on the other
/// end of an ssh hop to answer.
const REPLY_TIMEOUT: std::time::Duration = std::time::Duration::from_millis(150);

/// Sleep between non-blocking reads while waiting for the replies.
const REPLY_POLL_INTERVAL: std::time::Duration = std::time::Duration::from_millis(5);

/// Longest accepted reply payload. Real replies are tiny
/// (`rgb:ffff/ffff/ffff` is 19 bytes); anything larger is not a color
/// reply and gets dropped.
const MAX_REPLY_PAYLOAD_LEN: usize = 64;

/// Longest accepted OSC number in a reply (`10`/`11` in practice).
const MAX_REPLY_NUMBER_DIGITS: usize = 4;

#[derive(Debug, Default)]
enum ScanState {
    #[default]
    Ground,
    Escape,
    OscNumber {
        number: Vec<u8>,
    },
    OscPayload {
        number: Vec<u8>,
        payload: Vec<u8>,
    },
    OscPayloadEscape {
        number: Vec<u8>,
        payload: Vec<u8>,
    },
}

/// Tolerant scanner for host terminal OSC 10/11 color replies.
///
/// Replies may be BEL- or ST-terminated, arrive interleaved with other
/// bytes, and be split across arbitrary read chunk boundaries. Bytes that
/// are not part of an OSC reply are preserved in [`pending_input`] so the
/// caller can decide what to do with potential user keystrokes; malformed
/// or foreign OSC sequences are dropped.
///
/// [`pending_input`]: HostColorScanner::pending_input
#[derive(Debug, Default)]
pub struct HostColorScanner {
    state: ScanState,
    colors: HostColors,
    pending_input: Vec<u8>,
}

impl HostColorScanner {
    pub fn feed(&mut self, bytes: &[u8]) {
        for &byte in bytes {
            self.feed_byte(byte);
        }
    }

    /// Both colors were seen; the caller can stop reading early.
    pub fn complete(&self) -> bool {
        self.colors.fg.is_some() && self.colors.bg.is_some()
    }

    pub fn colors(&self) -> HostColors {
        self.colors
    }

    /// Bytes read during the reply window that were not part of an OSC
    /// sequence — potentially genuine user keystrokes.
    pub fn pending_input(&self) -> &[u8] {
        &self.pending_input
    }

    fn feed_byte(&mut self, byte: u8) {
        self.state = match std::mem::take(&mut self.state) {
            ScanState::Ground => {
                if byte == 0x1b {
                    ScanState::Escape
                } else {
                    self.pending_input.push(byte);
                    ScanState::Ground
                }
            }
            ScanState::Escape => match byte {
                b']' => ScanState::OscNumber { number: Vec::new() },
                0x1b => {
                    self.pending_input.push(0x1b);
                    ScanState::Escape
                }
                _ => {
                    self.pending_input.push(0x1b);
                    self.pending_input.push(byte);
                    ScanState::Ground
                }
            },
            ScanState::OscNumber { mut number } => match byte {
                b'0'..=b'9' if number.len() < MAX_REPLY_NUMBER_DIGITS => {
                    number.push(byte);
                    ScanState::OscNumber { number }
                }
                b';' => ScanState::OscPayload {
                    number,
                    payload: Vec::new(),
                },
                // Malformed OSC: drop it. These bytes are part of an escape
                // sequence, not keystrokes, so dropping is safe.
                _ => ScanState::Ground,
            },
            ScanState::OscPayload {
                number,
                mut payload,
            } => match byte {
                0x07 => {
                    self.finish_reply(&number, &payload);
                    ScanState::Ground
                }
                0x1b => ScanState::OscPayloadEscape { number, payload },
                _ if payload.len() < MAX_REPLY_PAYLOAD_LEN => {
                    payload.push(byte);
                    ScanState::OscPayload { number, payload }
                }
                // Oversized: cannot be a color reply, drop the sequence.
                _ => ScanState::Ground,
            },
            ScanState::OscPayloadEscape { number, payload } => match byte {
                b'\\' => {
                    self.finish_reply(&number, &payload);
                    ScanState::Ground
                }
                // Malformed terminator: drop the sequence.
                _ => ScanState::Ground,
            },
        };
    }

    fn finish_reply(&mut self, number: &[u8], payload: &[u8]) {
        let Ok(payload) = std::str::from_utf8(payload) else {
            return;
        };
        let Some(color) = parse_color_spec(payload) else {
            return;
        };
        match number {
            b"10" => self.colors.fg = Some(color),
            b"11" => self.colors.bg = Some(color),
            _ => {}
        }
    }
}

/// Parse a terminal color specification into an 8-bit-per-channel triple.
///
/// Accepts the X11 `rgb:R/G/B` form with 1–4 hex digits per component
/// (values are scaled, so `rgb:ffff/0000/8080` and `rgb:ff/00/80` are
/// equivalent) and the `#RRGGBB` shorthand some terminals reply with.
pub fn parse_color_spec(spec: &str) -> Option<(u8, u8, u8)> {
    let spec = spec.trim();
    if let Some(rest) = spec.strip_prefix("rgb:") {
        let mut components = rest.split('/');
        let r = parse_scaled_component(components.next()?)?;
        let g = parse_scaled_component(components.next()?)?;
        let b = parse_scaled_component(components.next()?)?;
        if components.next().is_some() {
            return None;
        }
        return Some((r, g, b));
    }
    if let Some(hex) = spec.strip_prefix('#') {
        if hex.len() != 6 || !hex.bytes().all(|byte| byte.is_ascii_hexdigit()) {
            return None;
        }
        let r = u8::from_str_radix(&hex[0..2], 16).ok()?;
        let g = u8::from_str_radix(&hex[2..4], 16).ok()?;
        let b = u8::from_str_radix(&hex[4..6], 16).ok()?;
        return Some((r, g, b));
    }
    None
}

/// Scale an X11 hex component of 1–4 digits to 8 bits (`ffff` → `0xff`,
/// `12` → `0x12`, `f` → `0xff`), rounding to nearest.
fn parse_scaled_component(component: &str) -> Option<u8> {
    if component.is_empty()
        || component.len() > 4
        || !component.bytes().all(|byte| byte.is_ascii_hexdigit())
    {
        return None;
    }
    let value = u32::from_str_radix(component, 16).ok()?;
    let max = (1u32 << (4 * component.len() as u32)) - 1;
    Some(((value * 255 + max / 2) / max) as u8)
}

/// Ask the host terminal for its default foreground/background colors
/// (OSC 10/11 queries) and wait briefly for the replies.
///
/// Must run right after entering raw mode and BEFORE crossterm's event
/// stream starts polling, so the replies are not swallowed by the input
/// machinery. Returns `None` when stdin is not a tty (plain pipes, CI
/// PTYs without a controlling terminal) or `/dev/tty` cannot be opened.
/// A terminal that never answers simply yields empty colors once the
/// bounded deadline passes — startup can never hang here.
#[cfg(unix)]
pub fn query_host_terminal_colors() -> Option<HostColors> {
    use std::io::{IsTerminal, Read, Write};
    use std::time::Instant;

    if !std::io::stdin().is_terminal() {
        return None;
    }
    let mut tty = open_tty_nonblocking()?;

    let mut stdout = std::io::stdout();
    stdout.write_all(b"\x1b]10;?\x1b\\\x1b]11;?\x1b\\").ok()?;
    stdout.flush().ok()?;

    let mut scanner = HostColorScanner::default();
    let deadline = Instant::now() + REPLY_TIMEOUT;
    let mut chunk = [0u8; 256];
    while !scanner.complete() && Instant::now() < deadline {
        match tty.read(&mut chunk) {
            Ok(0) => break,
            Ok(n) => scanner.feed(&chunk[..n]),
            Err(err) if err.kind() == std::io::ErrorKind::WouldBlock => {
                std::thread::sleep(REPLY_POLL_INTERVAL);
            }
            Err(err) if err.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(_) => break,
        }
    }

    // Non-reply bytes read during the ~150 ms window could be user
    // keystrokes typed at the instant of attach. They cannot be replayed
    // into crossterm's event stream (it reads the tty itself and exposes
    // no injection path), so they are dropped — the same tradeoff tmux
    // makes with its startup terminal capability queries.
    let _ = scanner.pending_input();

    Some(scanner.colors())
}

/// `O_NONBLOCK` for the unix platforms spectra builds on; hardcoded to
/// avoid pulling in a libc dependency for a single flag.
#[cfg(all(unix, target_os = "linux"))]
const O_NONBLOCK: i32 = 0o4000;
#[cfg(all(unix, not(target_os = "linux")))]
const O_NONBLOCK: i32 = 0x0004;

/// Open the controlling terminal in non-blocking mode so the reply loop
/// can poll without ever blocking past its deadline. A separate fd is used
/// instead of toggling flags on stdin so crossterm later finds stdin
/// exactly as it expects it.
#[cfg(unix)]
fn open_tty_nonblocking() -> Option<std::fs::File> {
    use std::os::unix::fs::OpenOptionsExt;

    std::fs::OpenOptions::new()
        .read(true)
        .custom_flags(O_NONBLOCK)
        .open("/dev/tty")
        .ok()
}

#[cfg(test)]
mod tests {
    use super::{HostColorScanner, HostColors, parse_color_spec};

    #[test]
    fn parse_color_spec_accepts_four_digit_rgb() {
        assert_eq!(
            parse_color_spec("rgb:ffff/0000/8080"),
            Some((0xff, 0x00, 0x80))
        );
        assert_eq!(
            parse_color_spec("rgb:1e1e/2a2a/3c3c"),
            Some((0x1e, 0x2a, 0x3c))
        );
    }

    #[test]
    fn parse_color_spec_accepts_two_digit_rgb() {
        assert_eq!(parse_color_spec("rgb:ff/00/80"), Some((0xff, 0x00, 0x80)));
    }

    #[test]
    fn parse_color_spec_scales_odd_widths() {
        // 1-digit components scale like X11: f → 0xff.
        assert_eq!(parse_color_spec("rgb:f/0/8"), Some((0xff, 0x00, 0x88)));
        // 3-digit components round to nearest 8-bit value.
        assert_eq!(
            parse_color_spec("rgb:fff/000/800"),
            Some((0xff, 0x00, 0x80))
        );
    }

    #[test]
    fn parse_color_spec_accepts_hash_hex() {
        assert_eq!(parse_color_spec("#1e2a3c"), Some((0x1e, 0x2a, 0x3c)));
    }

    #[test]
    fn parse_color_spec_rejects_garbage() {
        for garbage in [
            "",
            "?",
            "rgb:",
            "rgb:ff/00",
            "rgb:ff/00/80/00",
            "rgb:gg/00/00",
            "rgb:+f/00/00",
            "#12345",
            "#12345g",
            "#1234567",
            "cmyk:0/0/0/0",
        ] {
            assert_eq!(parse_color_spec(garbage), None, "accepted {garbage:?}");
        }
    }

    #[test]
    fn scanner_parses_st_terminated_replies() {
        let mut scanner = HostColorScanner::default();
        scanner.feed(b"\x1b]10;rgb:ffff/ffff/ffff\x1b\\\x1b]11;rgb:1e1e/2a2a/3c3c\x1b\\");
        assert!(scanner.complete());
        assert_eq!(
            scanner.colors(),
            HostColors {
                fg: Some((0xff, 0xff, 0xff)),
                bg: Some((0x1e, 0x2a, 0x3c)),
            }
        );
        assert!(scanner.pending_input().is_empty());
    }

    #[test]
    fn scanner_parses_bel_terminated_replies() {
        let mut scanner = HostColorScanner::default();
        scanner.feed(b"\x1b]10;#ffffff\x07\x1b]11;rgb:00/00/00\x07");
        assert!(scanner.complete());
        assert_eq!(scanner.colors().fg, Some((0xff, 0xff, 0xff)));
        assert_eq!(scanner.colors().bg, Some((0x00, 0x00, 0x00)));
    }

    #[test]
    fn scanner_handles_interleaved_partial_chunks() {
        let mut scanner = HostColorScanner::default();
        // Keystrokes interleaved with replies, split at awkward boundaries.
        scanner.feed(b"ab\x1b]10;rgb:ff");
        assert!(!scanner.complete());
        scanner.feed(b"ff/0000/0000\x1b");
        assert!(!scanner.complete());
        scanner.feed(b"\\c\x1b]11;rgb:0000/ffff/0000\x07d");
        assert!(scanner.complete());
        assert_eq!(scanner.colors().fg, Some((0xff, 0x00, 0x00)));
        assert_eq!(scanner.colors().bg, Some((0x00, 0xff, 0x00)));
        assert_eq!(scanner.pending_input(), b"abcd");
    }

    #[test]
    fn scanner_without_replies_reports_nothing() {
        let mut scanner = HostColorScanner::default();
        scanner.feed(b"plain user input, no escape sequences");
        assert!(!scanner.complete());
        assert_eq!(scanner.colors(), HostColors::default());
        assert_eq!(
            scanner.pending_input(),
            b"plain user input, no escape sequences"
        );
    }

    #[test]
    fn scanner_drops_malformed_and_foreign_sequences() {
        let mut scanner = HostColorScanner::default();
        // Foreign OSC number, garbage payload, and a malformed terminator
        // are all dropped without poisoning later replies.
        scanner.feed(b"\x1b]52;c;aGk=\x07\x1b]10;not-a-color\x07\x1b]11;rgb:11/11/11\x1bX");
        assert_eq!(scanner.colors(), HostColors::default());
        scanner.feed(b"\x1b]11;rgb:2222/2222/2222\x1b\\");
        assert_eq!(scanner.colors().bg, Some((0x22, 0x22, 0x22)));
        assert_eq!(scanner.colors().fg, None);
    }

    #[test]
    fn host_colors_serde_roundtrips_and_defaults() {
        let colors = HostColors {
            fg: Some((1, 2, 3)),
            bg: None,
        };
        let json = serde_json::to_string(&colors).expect("encode host colors");
        let decoded: HostColors = serde_json::from_str(&json).expect("decode host colors");
        assert_eq!(decoded, colors);

        let decoded: HostColors = serde_json::from_str("{}").expect("decode empty host colors");
        assert_eq!(decoded, HostColors::default());
    }
}
