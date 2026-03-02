#![cfg(unix)]

use std::collections::VecDeque;
use std::fs;
use std::io::{self, Read, Write};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::PathBuf;
use std::thread;

use crossterm::event::{KeyEvent, MouseEvent};

use crate::app::{App, AppSignal, ClientId, LOCAL_CLIENT_ID};
use crate::cli::Cli;
use crate::io::terminal;
use crate::ipc::codec::{DecodeResult, decode_messages, encode_message};
use crate::ipc::protocol::{ClientMessage, ServerMessage};
use crate::ipc::socket_path;
use crate::ui::render::FrameRenderer;

const DEFAULT_COLS: u16 = 80;
const DEFAULT_ROWS: u16 = 24;
const IDLE_LOOP_BACKOFF: std::time::Duration = std::time::Duration::from_millis(1);

pub fn run(cli: Cli) -> io::Result<()> {
    let socket = socket_path::socket_path();
    socket_path::prepare_listener_socket(&socket)?;
    let listener = UnixListener::bind(&socket)?;
    listener.set_nonblocking(true)?;
    let _cleanup = SocketCleanupGuard::new(socket);

    let mut app = App::new_with_size(cli.without_server_flag(), DEFAULT_COLS, DEFAULT_ROWS)?;
    app.request_render(true);

    let mut clients = Vec::new();
    let mut next_client_id: ClientId = 1;
    loop {
        let mut did_work = false;

        did_work |= accept_clients(&listener, &mut clients, &mut app, &mut next_client_id)?;
        did_work |= process_client_input(&mut clients, &mut app)?;

        let had_pending_render_before_tick = app.has_pending_render();
        app.tick();
        if app.has_pending_render() && !had_pending_render_before_tick {
            did_work = true;
        }

        did_work |= queue_pending_passthrough_messages(&mut clients, &mut app)?;

        if app.has_pending_render() {
            did_work = true;
            did_work |= queue_render_for_clients(&mut clients, &mut app)?;
        }

        if app.should_quit() {
            for client in &mut clients {
                let _ = client.queue_control_message(&ServerMessage::Shutdown {
                    reason: "spectra session ended".to_string(),
                });
            }
            let _ = flush_clients(&mut clients, &mut app);
            break;
        }

        did_work |= flush_clients(&mut clients, &mut app)?;
        if !did_work {
            thread::sleep(IDLE_LOOP_BACKOFF);
        }
    }

    Ok(())
}

fn accept_clients(
    listener: &UnixListener,
    clients: &mut Vec<ClientConnection>,
    app: &mut App,
    next_client_id: &mut ClientId,
) -> io::Result<bool> {
    let mut accepted = false;
    loop {
        match listener.accept() {
            Ok((stream, _addr)) => {
                stream.set_nonblocking(true)?;
                let client_id = *next_client_id;
                *next_client_id = client_id.saturating_add(1);
                app.register_client(client_id, DEFAULT_COLS, DEFAULT_ROWS);
                clients.push(ClientConnection::new(client_id, stream));
                accepted = true;
            }
            Err(err) if err.kind() == io::ErrorKind::WouldBlock => break,
            Err(err) if err.kind() == io::ErrorKind::Interrupted => continue,
            Err(err) => return Err(err),
        }
    }
    Ok(accepted)
}

fn process_client_input(clients: &mut [ClientConnection], app: &mut App) -> io::Result<bool> {
    let mut had_input = false;
    for client in clients {
        if client.disconnected {
            continue;
        }

        let decoded = client.read_messages()?;
        if !decoded.errors.is_empty() || !decoded.messages.is_empty() {
            had_input = true;
        }
        for error in decoded.errors {
            client.queue_control_message(&ServerMessage::Error {
                message: format!("invalid client frame: {error}"),
            })?;
            client.close_after_flush = true;
        }
        for message in decoded.messages {
            handle_client_message(client, message, app)?;
        }
    }
    Ok(had_input)
}

fn handle_client_message(
    client: &mut ClientConnection,
    message: ClientMessage,
    app: &mut App,
) -> io::Result<()> {
    match message {
        ClientMessage::Hello {
            cols,
            rows,
            attach_target,
            client_identity,
        } => {
            app.register_client_identity(client.id, client_identity);
            if let Some(target) = attach_target
                && let Err(err) = app.apply_attach_target_for_client(client.id, &target)
            {
                client.queue_control_message(&ServerMessage::Error {
                    message: format!("attach failed: {err}"),
                })?;
                client.close_after_flush = true;
                return Ok(());
            }

            if let Err(err) = app.handle_client_resize_event(client.id, cols, rows) {
                client.queue_control_message(&ServerMessage::Error {
                    message: format!("resize failed: {err}"),
                })?;
            } else {
                client.renders_enabled = true;
                app.request_render(false);
            }
        }
        ClientMessage::Resize { cols, rows } => {
            if let Err(err) = app.handle_client_resize_event(client.id, cols, rows) {
                client.queue_control_message(&ServerMessage::Error {
                    message: format!("resize failed: {err}"),
                })?;
            }
        }
        ClientMessage::Paste { text } => {
            if let Err(err) = app.handle_paste_text_for_client(client.id, text) {
                client.queue_control_message(&ServerMessage::Error {
                    message: format!("paste failed: {err}"),
                })?;
            }
        }
        ClientMessage::Key { key } => match KeyEvent::try_from(key) {
            Ok(key_event) => match app.handle_key_event_for_client(client.id, key_event) {
                Ok(signal) => {
                    if signal == AppSignal::DetachClient {
                        client.queue_control_message(&ServerMessage::Detached {
                            reason: "client detached".to_string(),
                        })?;
                        client.close_after_flush = true;
                    }
                }
                Err(err) => {
                    client.queue_control_message(&ServerMessage::Error {
                        message: format!("key handling failed: {err}"),
                    })?;
                }
            },
            Err(err) => {
                client.queue_control_message(&ServerMessage::Error {
                    message: format!("invalid key event: {err}"),
                })?;
            }
        },
        ClientMessage::Mouse { mouse } => match MouseEvent::try_from(mouse) {
            Ok(mouse_event) => {
                if let Err(err) = app.handle_mouse_event_for_client(client.id, mouse_event) {
                    client.queue_control_message(&ServerMessage::Error {
                        message: format!("mouse handling failed: {err}"),
                    })?;
                }
            }
            Err(err) => {
                client.queue_control_message(&ServerMessage::Error {
                    message: format!("invalid mouse event: {err}"),
                })?;
            }
        },
        ClientMessage::Command { request } => {
            match app.execute_command(request) {
                Ok(result) => {
                    client.queue_control_message(&ServerMessage::CommandResult { result })?;
                }
                Err(message) => {
                    client.queue_control_message(&ServerMessage::Error { message })?;
                }
            }
            client.renders_enabled = false;
            client.close_after_flush = true;
        }
    }
    queue_pending_clipboard_messages(client, app)?;
    Ok(())
}

fn queue_pending_clipboard_messages(
    client: &mut ClientConnection,
    app: &mut App,
) -> io::Result<()> {
    for ansi in app.take_pending_clipboard_ansi_for_client(client.id) {
        client.queue_control_message(&ServerMessage::Clipboard { ansi })?;
    }
    Ok(())
}

fn queue_pending_passthrough_messages(
    clients: &mut [ClientConnection],
    app: &mut App,
) -> io::Result<bool> {
    let mut queued = false;
    for client in clients {
        if client.disconnected || !client.renders_enabled {
            continue;
        }
        for ansi in app.take_pending_passthrough_ansi_for_client(client.id) {
            client.queue_control_message(&ServerMessage::Passthrough { ansi })?;
            queued = true;
        }
    }
    let _ = app.take_pending_passthrough_ansi_for_client(LOCAL_CLIENT_ID);
    Ok(queued)
}

fn queue_render_for_clients(clients: &mut [ClientConnection], app: &mut App) -> io::Result<bool> {
    let mut queued = false;
    for client in clients {
        if client.disconnected || !client.renders_enabled {
            continue;
        }
        let Some(snapshot) = app.render_snapshot_for_client(client.id) else {
            continue;
        };
        let mut bytes = Vec::new();
        let full_clear = snapshot.full_clear
            || client.force_full_clear
            || client.has_pending_unsent_render_that_will_be_replaced();
        client.renderer.render_to_writer_with_status_style(
            &mut bytes,
            &snapshot.frame,
            &snapshot.status_line,
            snapshot.status_style,
            snapshot.cols,
            snapshot.rows,
            full_clear,
            snapshot.overlay.as_ref(),
            snapshot.side_window_tree.as_ref(),
        )?;
        let ansi = render_payload_with_window_title(
            snapshot.window_title.as_deref(),
            String::from_utf8_lossy(&bytes).into_owned(),
        );
        client.queue_render_frame(ansi)?;
        queued = true;
        client.force_full_clear = false;
    }
    app.finish_render_cycle();
    Ok(queued)
}

fn render_payload_with_window_title(window_title: Option<&str>, ansi: String) -> String {
    match window_title {
        Some(window_title) => format!("{}{}", terminal::osc2_title_sequence(window_title), ansi),
        None => ansi,
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum QueuedMessage {
    Render(Vec<u8>),
    Control(Vec<u8>),
}

impl QueuedMessage {
    fn bytes(&self) -> &[u8] {
        match self {
            Self::Render(bytes) | Self::Control(bytes) => bytes,
        }
    }

    fn is_render(&self) -> bool {
        matches!(self, Self::Render(_))
    }
}

fn flush_clients(clients: &mut Vec<ClientConnection>, app: &mut App) -> io::Result<bool> {
    let mut progressed = false;
    let mut index = 0usize;
    while index < clients.len() {
        progressed |= clients[index].flush()?;
        if clients[index].disconnected {
            let removed = clients.remove(index);
            app.unregister_client(removed.id);
            progressed = true;
        } else {
            index += 1;
        }
    }
    Ok(progressed)
}

struct ClientConnection {
    id: ClientId,
    stream: UnixStream,
    read_buffer: Vec<u8>,
    write_queue: VecDeque<QueuedMessage>,
    write_offset: usize,
    renderer: FrameRenderer,
    force_full_clear: bool,
    renders_enabled: bool,
    close_after_flush: bool,
    disconnected: bool,
}

impl ClientConnection {
    fn new(id: ClientId, stream: UnixStream) -> Self {
        Self {
            id,
            stream,
            read_buffer: Vec::new(),
            write_queue: VecDeque::new(),
            write_offset: 0,
            renderer: FrameRenderer::new(),
            force_full_clear: true,
            renders_enabled: false,
            close_after_flush: false,
            disconnected: false,
        }
    }

    fn read_messages(&mut self) -> io::Result<DecodeResult<ClientMessage>> {
        let mut chunk = [0u8; 16 * 1024];
        loop {
            match self.stream.read(&mut chunk) {
                Ok(0) => {
                    self.disconnected = true;
                    break;
                }
                Ok(n) => {
                    self.read_buffer.extend_from_slice(&chunk[..n]);
                }
                Err(err) if err.kind() == io::ErrorKind::WouldBlock => break,
                Err(err) if err.kind() == io::ErrorKind::Interrupted => continue,
                Err(_err) => {
                    self.disconnected = true;
                    break;
                }
            }
        }
        Ok(decode_messages::<ClientMessage>(&mut self.read_buffer))
    }

    fn queue_control_message(&mut self, message: &ServerMessage) -> io::Result<()> {
        self.write_queue
            .push_back(QueuedMessage::Control(encode_message(message)?));
        Ok(())
    }

    fn queue_render_frame(&mut self, ansi: String) -> io::Result<()> {
        let keep_until = usize::from(self.write_offset > 0);
        if !self.write_queue.is_empty() {
            self.drop_pending_renders_from(keep_until);
        }
        self.write_queue
            .push_back(QueuedMessage::Render(encode_message(
                &ServerMessage::Render { ansi },
            )?));
        Ok(())
    }

    fn has_pending_unsent_render_that_will_be_replaced(&self) -> bool {
        let keep_until = usize::from(self.write_offset > 0);
        self.write_queue
            .iter()
            .enumerate()
            .any(|(index, entry)| index >= keep_until && entry.is_render())
    }

    fn drop_pending_renders_from(&mut self, keep_until: usize) {
        let mut compacted = VecDeque::with_capacity(self.write_queue.len());
        for (index, entry) in self.write_queue.drain(..).enumerate() {
            if index >= keep_until && entry.is_render() {
                continue;
            }
            compacted.push_back(entry);
        }
        self.write_queue = compacted;
    }

    fn flush(&mut self) -> io::Result<bool> {
        if self.disconnected {
            return Ok(false);
        }

        let mut progressed = false;
        while let Some(message) = self.write_queue.front() {
            let bytes = message.bytes();
            let slice = &bytes[self.write_offset..];
            match self.stream.write(slice) {
                Ok(0) => {
                    self.disconnected = true;
                    progressed = true;
                    return Ok(progressed);
                }
                Ok(n) => {
                    progressed = true;
                    self.write_offset += n;
                    if self.write_offset >= bytes.len() {
                        self.write_queue.pop_front();
                        self.write_offset = 0;
                    }
                }
                Err(err) if err.kind() == io::ErrorKind::WouldBlock => break,
                Err(err) if err.kind() == io::ErrorKind::Interrupted => continue,
                Err(_err) => {
                    self.disconnected = true;
                    progressed = true;
                    return Ok(progressed);
                }
            }
        }

        if self.close_after_flush && self.write_queue.is_empty() {
            self.disconnected = true;
            progressed = true;
        }
        Ok(progressed)
    }
}

struct SocketCleanupGuard {
    path: PathBuf,
}

impl SocketCleanupGuard {
    fn new(path: PathBuf) -> Self {
        Self { path }
    }
}

impl Drop for SocketCleanupGuard {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.path);
    }
}

#[cfg(test)]
mod tests {
    use std::os::unix::net::UnixStream;

    use super::{ClientConnection, QueuedMessage, render_payload_with_window_title};
    use crate::ipc::protocol::ServerMessage;

    fn decode(entry: &QueuedMessage) -> ServerMessage {
        let payload = std::str::from_utf8(entry.bytes()).expect("valid utf-8 message payload");
        serde_json::from_str(payload.trim_end_matches('\n')).expect("decode server message")
    }

    fn test_client() -> ClientConnection {
        let (stream, _peer) = UnixStream::pair().expect("unix socket pair");
        stream
            .set_nonblocking(true)
            .expect("set client stream nonblocking");
        ClientConnection::new(1, stream)
    }

    #[test]
    fn render_queue_keeps_only_latest_pending_render() {
        let mut client = test_client();

        client
            .queue_render_frame("old-render".to_string())
            .expect("queue first render");
        client
            .queue_render_frame("middle-render".to_string())
            .expect("queue second render");
        client
            .queue_render_frame("latest-render".to_string())
            .expect("queue latest render");

        assert_eq!(client.write_queue.len(), 1);
        match decode(client.write_queue.front().expect("queued render")) {
            ServerMessage::Render { ansi } => assert_eq!(ansi, "latest-render"),
            other => panic!("expected render message, got {other:?}"),
        }
    }

    #[test]
    fn partial_front_render_is_preserved_while_tail_render_is_replaced() {
        let mut client = test_client();

        client
            .queue_render_frame("front-render".to_string())
            .expect("queue front render");
        client.write_offset = 5;
        client
            .queue_render_frame("stale-render".to_string())
            .expect("queue stale render");
        client
            .queue_control_message(&ServerMessage::Error {
                message: "keep-control".to_string(),
            })
            .expect("queue control message");
        client
            .queue_render_frame("latest-render".to_string())
            .expect("queue latest render");

        let queued = client.write_queue.iter().map(decode).collect::<Vec<_>>();
        assert_eq!(queued.len(), 3);
        match &queued[0] {
            ServerMessage::Render { ansi } => assert_eq!(ansi, "front-render"),
            other => panic!("expected front render, got {other:?}"),
        }
        match &queued[1] {
            ServerMessage::Error { message } => assert_eq!(message, "keep-control"),
            other => panic!("expected control error, got {other:?}"),
        }
        match &queued[2] {
            ServerMessage::Render { ansi } => assert_eq!(ansi, "latest-render"),
            other => panic!("expected latest render, got {other:?}"),
        }
    }

    #[test]
    fn control_messages_are_not_dropped_or_reordered() {
        let mut client = test_client();

        client
            .queue_control_message(&ServerMessage::Error {
                message: "first".to_string(),
            })
            .expect("queue first control");
        client
            .queue_control_message(&ServerMessage::Detached {
                reason: "second".to_string(),
            })
            .expect("queue second control");
        client
            .queue_render_frame("stale-render".to_string())
            .expect("queue stale render");
        client
            .queue_control_message(&ServerMessage::Shutdown {
                reason: "third".to_string(),
            })
            .expect("queue third control");
        client
            .queue_render_frame("latest-render".to_string())
            .expect("queue latest render");

        let queued = client.write_queue.iter().map(decode).collect::<Vec<_>>();
        assert_eq!(queued.len(), 4);
        match &queued[0] {
            ServerMessage::Error { message } => assert_eq!(message, "first"),
            other => panic!("expected first control, got {other:?}"),
        }
        match &queued[1] {
            ServerMessage::Detached { reason } => assert_eq!(reason, "second"),
            other => panic!("expected second control, got {other:?}"),
        }
        match &queued[2] {
            ServerMessage::Shutdown { reason } => assert_eq!(reason, "third"),
            other => panic!("expected third control, got {other:?}"),
        }
        match &queued[3] {
            ServerMessage::Render { ansi } => assert_eq!(ansi, "latest-render"),
            other => panic!("expected latest render, got {other:?}"),
        }
    }

    #[test]
    fn render_payload_prefixes_window_title_sequence() {
        let ansi = render_payload_with_window_title(Some("build"), "frame".to_string());
        assert_eq!(ansi, "\x1b]2;build\x07frame");
    }

    #[test]
    fn render_payload_keeps_frame_when_window_title_missing() {
        let ansi = render_payload_with_window_title(None, "frame".to_string());
        assert_eq!(ansi, "frame");
    }
}
