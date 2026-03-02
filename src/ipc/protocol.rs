use crossterm::event::{
    KeyCode, KeyEvent, KeyEventKind, KeyModifiers, MouseButton, MouseEvent, MouseEventKind,
};
use serde::{Deserialize, Serialize};

use crate::attach_target::AttachTarget;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum CommandSplitAxis {
    Vertical,
    Horizontal,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "command", rename_all = "snake_case")]
pub enum CommandRequest {
    NewSession,
    Ls,
    KillSession {
        target: Option<String>,
    },
    NewWindow {
        target: Option<AttachTarget>,
    },
    SplitWindow {
        target: Option<AttachTarget>,
        axis: CommandSplitAxis,
    },
    SelectSession {
        target: Option<String>,
    },
    SelectWindow {
        target: Option<String>,
        window: usize,
    },
    SelectPane {
        target: Option<String>,
        pane: usize,
    },
    SendKeys {
        target: Option<AttachTarget>,
        all: bool,
        text: String,
    },
    SourceFile {
        path: Option<String>,
    },
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct SessionListEntry {
    pub alias: String,
    pub session_id: String,
    pub session_name: String,
    pub window_count: usize,
    pub pane_count: usize,
    pub focused_window: Option<usize>,
    pub focused_pane: Option<usize>,
    pub active: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum CommandResult {
    Message { message: String },
    SessionList { sessions: Vec<SessionListEntry> },
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ClientMessage {
    Hello {
        cols: u16,
        rows: u16,
        attach_target: Option<AttachTarget>,
        #[serde(default)]
        client_identity: Option<String>,
    },
    Key {
        key: NetKeyEvent,
    },
    Paste {
        text: String,
    },
    Mouse {
        mouse: NetMouseEvent,
    },
    Resize {
        cols: u16,
        rows: u16,
    },
    Command {
        request: CommandRequest,
    },
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ServerMessage {
    Render { ansi: String },
    Clipboard { ansi: String },
    Passthrough { ansi: String },
    Detached { reason: String },
    Shutdown { reason: String },
    Error { message: String },
    CommandResult { result: CommandResult },
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct NetKeyEvent {
    pub code: NetKeyCode,
    pub modifiers: u8,
    pub kind: NetKeyKind,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct NetMouseEvent {
    pub kind: NetMouseEventKind,
    pub column: u16,
    pub row: u16,
    pub modifiers: u8,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum NetMouseEventKind {
    Down { button: NetMouseButton },
    Up { button: NetMouseButton },
    Drag { button: NetMouseButton },
    Moved,
    ScrollUp,
    ScrollDown,
    ScrollLeft,
    ScrollRight,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum NetMouseButton {
    Left,
    Right,
    Middle,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum NetKeyCode {
    Char(char),
    Enter,
    Tab,
    BackTab,
    Backspace,
    Esc,
    Left,
    Right,
    Up,
    Down,
    Home,
    End,
    Delete,
    Insert,
    PageUp,
    PageDown,
    F(u8),
    Null,
    Other,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum NetKeyKind {
    Press,
    Repeat,
    Release,
}

impl From<KeyEvent> for NetKeyEvent {
    fn from(value: KeyEvent) -> Self {
        Self {
            code: NetKeyCode::from(value.code),
            modifiers: value.modifiers.bits(),
            kind: NetKeyKind::from(value.kind),
        }
    }
}

impl TryFrom<NetKeyEvent> for KeyEvent {
    type Error = String;

    fn try_from(value: NetKeyEvent) -> Result<Self, Self::Error> {
        let code = KeyCode::try_from(value.code)?;
        Ok(KeyEvent::new_with_kind(
            code,
            KeyModifiers::from_bits_truncate(value.modifiers),
            KeyEventKind::from(value.kind),
        ))
    }
}

impl From<MouseEvent> for NetMouseEvent {
    fn from(value: MouseEvent) -> Self {
        Self {
            kind: NetMouseEventKind::from(value.kind),
            column: value.column,
            row: value.row,
            modifiers: value.modifiers.bits(),
        }
    }
}

impl TryFrom<NetMouseEvent> for MouseEvent {
    type Error = String;

    fn try_from(value: NetMouseEvent) -> Result<Self, Self::Error> {
        Ok(MouseEvent {
            kind: MouseEventKind::try_from(value.kind)?,
            column: value.column,
            row: value.row,
            modifiers: KeyModifiers::from_bits_truncate(value.modifiers),
        })
    }
}

impl From<KeyCode> for NetKeyCode {
    fn from(value: KeyCode) -> Self {
        match value {
            KeyCode::Char(ch) => Self::Char(ch),
            KeyCode::Enter => Self::Enter,
            KeyCode::Tab => Self::Tab,
            KeyCode::BackTab => Self::BackTab,
            KeyCode::Backspace => Self::Backspace,
            KeyCode::Esc => Self::Esc,
            KeyCode::Left => Self::Left,
            KeyCode::Right => Self::Right,
            KeyCode::Up => Self::Up,
            KeyCode::Down => Self::Down,
            KeyCode::Home => Self::Home,
            KeyCode::End => Self::End,
            KeyCode::Delete => Self::Delete,
            KeyCode::Insert => Self::Insert,
            KeyCode::PageUp => Self::PageUp,
            KeyCode::PageDown => Self::PageDown,
            KeyCode::F(number) => Self::F(number),
            KeyCode::Null => Self::Null,
            _ => Self::Other,
        }
    }
}

impl TryFrom<NetKeyCode> for KeyCode {
    type Error = String;

    fn try_from(value: NetKeyCode) -> Result<Self, Self::Error> {
        match value {
            NetKeyCode::Char(ch) => Ok(KeyCode::Char(ch)),
            NetKeyCode::Enter => Ok(KeyCode::Enter),
            NetKeyCode::Tab => Ok(KeyCode::Tab),
            NetKeyCode::BackTab => Ok(KeyCode::BackTab),
            NetKeyCode::Backspace => Ok(KeyCode::Backspace),
            NetKeyCode::Esc => Ok(KeyCode::Esc),
            NetKeyCode::Left => Ok(KeyCode::Left),
            NetKeyCode::Right => Ok(KeyCode::Right),
            NetKeyCode::Up => Ok(KeyCode::Up),
            NetKeyCode::Down => Ok(KeyCode::Down),
            NetKeyCode::Home => Ok(KeyCode::Home),
            NetKeyCode::End => Ok(KeyCode::End),
            NetKeyCode::Delete => Ok(KeyCode::Delete),
            NetKeyCode::Insert => Ok(KeyCode::Insert),
            NetKeyCode::PageUp => Ok(KeyCode::PageUp),
            NetKeyCode::PageDown => Ok(KeyCode::PageDown),
            NetKeyCode::F(number) => Ok(KeyCode::F(number)),
            NetKeyCode::Null => Ok(KeyCode::Null),
            NetKeyCode::Other => Err("unsupported key code".to_string()),
        }
    }
}

impl From<KeyEventKind> for NetKeyKind {
    fn from(value: KeyEventKind) -> Self {
        match value {
            KeyEventKind::Press => Self::Press,
            KeyEventKind::Repeat => Self::Repeat,
            KeyEventKind::Release => Self::Release,
        }
    }
}

impl From<NetKeyKind> for KeyEventKind {
    fn from(value: NetKeyKind) -> Self {
        match value {
            NetKeyKind::Press => KeyEventKind::Press,
            NetKeyKind::Repeat => KeyEventKind::Repeat,
            NetKeyKind::Release => KeyEventKind::Release,
        }
    }
}

impl From<MouseButton> for NetMouseButton {
    fn from(value: MouseButton) -> Self {
        match value {
            MouseButton::Left => Self::Left,
            MouseButton::Right => Self::Right,
            MouseButton::Middle => Self::Middle,
        }
    }
}

impl From<NetMouseButton> for MouseButton {
    fn from(value: NetMouseButton) -> Self {
        match value {
            NetMouseButton::Left => MouseButton::Left,
            NetMouseButton::Right => MouseButton::Right,
            NetMouseButton::Middle => MouseButton::Middle,
        }
    }
}

impl From<MouseEventKind> for NetMouseEventKind {
    fn from(value: MouseEventKind) -> Self {
        match value {
            MouseEventKind::Down(button) => Self::Down {
                button: NetMouseButton::from(button),
            },
            MouseEventKind::Up(button) => Self::Up {
                button: NetMouseButton::from(button),
            },
            MouseEventKind::Drag(button) => Self::Drag {
                button: NetMouseButton::from(button),
            },
            MouseEventKind::Moved => Self::Moved,
            MouseEventKind::ScrollUp => Self::ScrollUp,
            MouseEventKind::ScrollDown => Self::ScrollDown,
            MouseEventKind::ScrollLeft => Self::ScrollLeft,
            MouseEventKind::ScrollRight => Self::ScrollRight,
        }
    }
}

impl TryFrom<NetMouseEventKind> for MouseEventKind {
    type Error = String;

    fn try_from(value: NetMouseEventKind) -> Result<Self, Self::Error> {
        Ok(match value {
            NetMouseEventKind::Down { button } => MouseEventKind::Down(MouseButton::from(button)),
            NetMouseEventKind::Up { button } => MouseEventKind::Up(MouseButton::from(button)),
            NetMouseEventKind::Drag { button } => MouseEventKind::Drag(MouseButton::from(button)),
            NetMouseEventKind::Moved => MouseEventKind::Moved,
            NetMouseEventKind::ScrollUp => MouseEventKind::ScrollUp,
            NetMouseEventKind::ScrollDown => MouseEventKind::ScrollDown,
            NetMouseEventKind::ScrollLeft => MouseEventKind::ScrollLeft,
            NetMouseEventKind::ScrollRight => MouseEventKind::ScrollRight,
        })
    }
}

#[cfg(test)]
mod tests {
    use crossterm::event::{
        KeyCode, KeyEvent, KeyEventKind, KeyModifiers, MouseButton, MouseEvent, MouseEventKind,
    };

    use crate::attach_target::AttachTarget;

    use super::{
        ClientMessage, CommandRequest, CommandResult, CommandSplitAxis, NetKeyEvent, NetMouseEvent,
        ServerMessage, SessionListEntry,
    };

    #[test]
    fn roundtrip_key_event() {
        let key = KeyEvent::new_with_kind(
            KeyCode::Char('D'),
            KeyModifiers::CONTROL | KeyModifiers::SHIFT,
            KeyEventKind::Repeat,
        );

        let net = NetKeyEvent::from(key);
        let roundtrip = KeyEvent::try_from(net).expect("decode key");
        assert_eq!(roundtrip.code, KeyCode::Char('D'));
        assert!(roundtrip.modifiers.contains(KeyModifiers::CONTROL));
        assert!(roundtrip.modifiers.contains(KeyModifiers::SHIFT));
        assert_eq!(roundtrip.kind, KeyEventKind::Repeat);
    }

    #[test]
    fn unsupported_code_fails_decode() {
        let net = NetKeyEvent {
            code: super::NetKeyCode::Other,
            modifiers: 0,
            kind: super::NetKeyKind::Press,
        };
        let err = KeyEvent::try_from(net).expect_err("unsupported key");
        assert!(err.contains("unsupported"));
    }

    #[test]
    fn roundtrip_mouse_event() {
        let mouse = MouseEvent {
            kind: MouseEventKind::Drag(MouseButton::Left),
            column: 18,
            row: 7,
            modifiers: KeyModifiers::SHIFT,
        };

        let net = NetMouseEvent::from(mouse);
        let roundtrip = MouseEvent::try_from(net).expect("decode mouse");
        assert_eq!(roundtrip.kind, MouseEventKind::Drag(MouseButton::Left));
        assert_eq!(roundtrip.column, 18);
        assert_eq!(roundtrip.row, 7);
        assert!(roundtrip.modifiers.contains(KeyModifiers::SHIFT));
    }

    #[test]
    fn command_request_roundtrips_json() {
        let request = CommandRequest::SplitWindow {
            target: Some(AttachTarget::parse("main:w2.p3").expect("parse target")),
            axis: CommandSplitAxis::Horizontal,
        };
        let json = serde_json::to_string(&request).expect("encode request");
        let decoded: CommandRequest = serde_json::from_str(&json).expect("decode request");
        assert_eq!(decoded, request);
    }

    #[test]
    fn send_keys_request_roundtrips_json() {
        let request = CommandRequest::SendKeys {
            target: Some(AttachTarget::parse("s2:w1.p3").expect("parse target")),
            all: false,
            text: "echo hi".to_string(),
        };
        let json = serde_json::to_string(&request).expect("encode request");
        let decoded: CommandRequest = serde_json::from_str(&json).expect("decode request");
        assert_eq!(decoded, request);
    }

    #[test]
    fn source_file_request_roundtrips_json() {
        let request = CommandRequest::SourceFile {
            path: Some("/tmp/spectra.toml".to_string()),
        };
        let json = serde_json::to_string(&request).expect("encode request");
        let decoded: CommandRequest = serde_json::from_str(&json).expect("decode request");
        assert_eq!(decoded, request);
    }

    #[test]
    fn command_result_roundtrips_json() {
        let result = CommandResult::SessionList {
            sessions: vec![SessionListEntry {
                alias: "s1".to_string(),
                session_id: "main-1".to_string(),
                session_name: "main".to_string(),
                window_count: 3,
                pane_count: 3,
                focused_window: Some(2),
                focused_pane: Some(4),
                active: true,
            }],
        };
        let json = serde_json::to_string(&result).expect("encode result");
        let decoded: CommandResult = serde_json::from_str(&json).expect("decode result");
        assert_eq!(decoded, result);
    }

    #[test]
    fn hello_message_roundtrips_client_identity() {
        let message = ClientMessage::Hello {
            cols: 120,
            rows: 35,
            attach_target: Some(AttachTarget::parse("s2:w1.p3").expect("parse target")),
            client_identity: Some("tty=/dev/pts/4|uid=501".to_string()),
        };
        let json = serde_json::to_string(&message).expect("encode hello");
        let decoded: ClientMessage = serde_json::from_str(&json).expect("decode hello");
        assert_eq!(decoded, message);
    }

    #[test]
    fn hello_message_defaults_client_identity_when_missing() {
        let json = r#"{"type":"hello","cols":80,"rows":24,"attach_target":null}"#;
        let decoded: ClientMessage = serde_json::from_str(json).expect("decode hello");
        assert_eq!(
            decoded,
            ClientMessage::Hello {
                cols: 80,
                rows: 24,
                attach_target: None,
                client_identity: None,
            }
        );
    }

    #[test]
    fn server_clipboard_message_roundtrips_json() {
        let message = ServerMessage::Clipboard {
            ansi: "\u{1b}]52;c;aGVsbG8=\u{1b}\\".to_string(),
        };
        let json = serde_json::to_string(&message).expect("encode server clipboard");
        let decoded: ServerMessage = serde_json::from_str(&json).expect("decode server clipboard");
        assert_eq!(decoded, message);
    }

    #[test]
    fn server_passthrough_message_roundtrips_json() {
        let message = ServerMessage::Passthrough {
            ansi: "\u{1b}]1337;SetUserVar=name=value\u{7}".to_string(),
        };
        let json = serde_json::to_string(&message).expect("encode server passthrough");
        let decoded: ServerMessage =
            serde_json::from_str(&json).expect("decode server passthrough");
        assert_eq!(decoded, message);
    }
}
