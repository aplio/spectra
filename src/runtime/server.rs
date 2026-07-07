#![cfg(unix)]

use std::collections::VecDeque;
use std::fs;
use std::io::{self, Read, Write};
use std::os::fd::AsRawFd;
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::PathBuf;
use std::sync::{Arc, mpsc};
use std::thread;
use std::time::{Duration, Instant};

use crossterm::event::{KeyEvent, MouseEvent};
use polling::{Event, Events, Poller};

use crate::app::{App, AppSignal, ClientId, LOCAL_CLIENT_ID};
use crate::cli::Cli;
use crate::io::terminal;
use crate::ipc::codec::{DecodeResult, decode_messages, encode_message};
use crate::ipc::protocol::{ClientMessage, MAX_PASTE_IMAGE_BYTES, PROTOCOL_VERSION, ServerMessage};
use crate::ipc::socket_path;
use crate::ui::render::FrameRenderer;

const DEFAULT_COLS: u16 = 80;
const DEFAULT_ROWS: u16 = 24;
const API_MAX_LINE_BYTES: usize = 1024 * 1024;

/// Upper bound on one poll wait. Deadlines computed by
/// [`App::next_deadline`] normally wake the loop sooner; this cap is a
/// safety net so that even a missed deadline source (or wall-clock work
/// with no fd, like pane-exit detection via `try_wait`) is never delayed
/// past 250ms. Idle cost: ~4 wakeups per second.
const POLL_HEARTBEAT: std::time::Duration = std::time::Duration::from_millis(250);

/// Poller key of the main (client) listener.
const POLL_KEY_CLIENT_LISTENER: usize = 0;
/// Poller key of the JSON-RPC API listener.
const POLL_KEY_API_LISTENER: usize = 1;
/// First poller key handed to API connections; they use odd keys >= 3
/// (client connections use even keys >= 2), so keys stay unique for the
/// lifetime of the server without any shared registry.
const POLL_KEY_API_FIRST: usize = 3;

/// Poller key for a client connection: even keys >= 2, derived from the
/// monotonically increasing client id (ids start at 1 and are never
/// reused), so a key can never be confused with a listener or an API key.
fn client_poll_key(id: ClientId) -> usize {
    (id as usize).saturating_mul(2)
}

/// Poller interest for a connection: always readable, and additionally
/// writable only while queued bytes could not be flushed (`WouldBlock`),
/// so an idle wait never spins on always-writable sockets.
fn poll_interest(key: usize, wants_write: bool) -> Event {
    if wants_write {
        Event::all(key)
    } else {
        Event::readable(key)
    }
}

/// Re-arm poller interest for every live source. The `polling` crate
/// delivers events in oneshot mode — a delivered event clears that source's
/// interest — so instead of tracking which events fired, the loop re-arms
/// everything right before each wait. A handful of `modify` calls per
/// wakeup is negligible next to the alternative failure mode: one missed
/// re-arm silently stalling a connection.
fn rearm_poll_interest(
    poller: &Poller,
    listener: &UnixListener,
    api_listener: &UnixListener,
    clients: &[ClientConnection],
    api_connections: &[ApiConnection],
) -> io::Result<()> {
    poller.modify(listener, Event::readable(POLL_KEY_CLIENT_LISTENER))?;
    poller.modify(api_listener, Event::readable(POLL_KEY_API_LISTENER))?;
    for client in clients {
        poller.modify(
            &client.stream,
            poll_interest(client_poll_key(client.id), !client.write_queue.is_empty()),
        )?;
    }
    for connection in api_connections {
        poller.modify(
            &connection.stream,
            poll_interest(connection.poll_key, !connection.write_buffer.is_empty()),
        )?;
    }
    Ok(())
}

pub fn run(cli: Cli) -> io::Result<()> {
    serve(cli, None)
}

/// Run the server around sessions adopted from a live handoff: the panes
/// wrap PTY fds received from the previous server instead of spawning new
/// processes. Called by `spectra server-handoff --foreground` after the
/// takeover exchange completed and the old server released its sockets.
pub(crate) fn run_adopted(
    cli: Cli,
    takeover: crate::runtime::handoff::HandoffTakeover,
) -> io::Result<()> {
    serve(cli, Some(takeover))
}

fn serve(cli: Cli, takeover: Option<crate::runtime::handoff::HandoffTakeover>) -> io::Result<()> {
    let socket = socket_path::socket_path();
    socket_path::prepare_listener_socket(&socket)?;
    let listener = UnixListener::bind(&socket)?;
    listener.set_nonblocking(true)?;
    let mut socket_cleanup = SocketCleanupGuard::new(socket);

    let api_socket = socket_path::api_socket_path();
    socket_path::prepare_listener_socket(&api_socket)?;
    let api_listener = UnixListener::bind(&api_socket)?;
    api_listener.set_nonblocking(true)?;
    let mut api_socket_cleanup = SocketCleanupGuard::new(api_socket);

    // Readiness-based loop core: the poller owns interest for both
    // listeners and every connection stream; fd-less producers (PTY reader
    // threads, the update-check thread) wake it through `wake::notify`.
    // Install the waker before `App::new_with_size` so panes spawned during
    // startup can already wake the loop.
    let poller = Arc::new(Poller::new()?);
    let _wake_guard = crate::runtime::wake::install(Arc::clone(&poller));
    // SAFETY: both listeners outlive the poller registrations — they are
    // dropped together at the end of this function.
    unsafe {
        poller.add(&listener, Event::readable(POLL_KEY_CLIENT_LISTENER))?;
        poller.add(&api_listener, Event::readable(POLL_KEY_API_LISTENER))?;
    }

    let adopted = takeover.is_some();
    let mut app = match takeover {
        Some(takeover) => App::new_from_handoff(
            cli.without_server_flag(),
            DEFAULT_COLS,
            DEFAULT_ROWS,
            takeover.header,
            takeover.fds,
        )?,
        None => App::new_with_size(cli.without_server_flag(), DEFAULT_COLS, DEFAULT_ROWS)?,
    };
    if adopted {
        // One status line for the `server-handoff` coordinator, printed only
        // once the sockets are bound and the adopted state is live.
        crate::runtime::handoff::announce_takeover_ready(app.total_pane_count());
    }
    app.request_render(true);
    // Plugins load only in the server: on_event/service commands need the
    // API socket, and services must be supervised by the server's lifetime
    // (their kill-on-drop guards die with `app` at the end of this fn).
    app.load_plugins();

    // Update check: a fresh cache answers immediately; otherwise one named
    // background thread performs the check so startup is never delayed.
    let mut update_check_rx = if app.load_cached_update_check() {
        None
    } else {
        spawn_update_check(crate::upgrade::check_latest)
    };

    let mut clients = Vec::new();
    let mut api_connections: Vec<ApiConnection> = Vec::new();
    let mut next_client_id: ClientId = 1;
    let mut next_api_poll_key = POLL_KEY_API_FIRST;
    let mut poll_events = Events::new();
    loop {
        let mut did_work = false;

        did_work |= accept_clients(
            &listener,
            &mut clients,
            &mut app,
            &mut next_client_id,
            &poller,
        )?;
        did_work |= process_client_input(&mut clients, &mut app)?;
        did_work |= accept_api_connections(
            &api_listener,
            &mut api_connections,
            &poller,
            &mut next_api_poll_key,
        )?;
        let mut handoff_requested = false;
        did_work |= process_api_input(&mut api_connections, &mut app, &mut handoff_requested);
        if handoff_requested {
            did_work = true;
            match perform_handoff(&mut app, &mut api_connections, &poller) {
                Ok(successor) => {
                    // Point of no return: the successor holds a copy of
                    // every PTY fd and has acked. Let the children outlive
                    // this process and release the listener sockets so the
                    // successor can bind fresh ones, then signal completion
                    // and exit without touching the panes again.
                    app.disarm_pane_children();
                    socket_cleanup.remove_now();
                    api_socket_cleanup.remove_now();
                    let _ = crate::runtime::handoff::write_line(
                        &successor,
                        crate::runtime::handoff::COMPLETE,
                    );
                    return Ok(());
                }
                Err(err) => {
                    // Nothing was disarmed or unbound: this server stays
                    // fully functional and keeps serving.
                    app.note_handoff_abort(&err.to_string());
                }
            }
        }
        did_work |= poll_update_check(&mut update_check_rx, &mut app);

        let had_pending_render_before_tick = app.has_pending_render();
        app.tick();
        if app.has_pending_render() && !had_pending_render_before_tick {
            did_work = true;
        }

        did_work |= queue_pending_passthrough_messages(&mut clients, &mut app)?;
        did_work |= queue_pending_image_paste_requests(&mut clients, &mut app)?;
        did_work |= fan_out_api_events(&mut api_connections, &mut app);

        // A synchronized-output hold (DECSET 2026) defers frame delivery
        // until the guest releases it or the hold times out; needs_render
        // stays set so the frame flushes on the next pass.
        if app.has_pending_render() && !app.render_hold_for_sync_output() {
            did_work = true;
            did_work |= queue_render_for_clients(&mut clients, &mut app)?;
        }

        if app.should_quit() {
            for client in &mut clients {
                let _ = client.queue_control_message(&ServerMessage::Shutdown {
                    reason: "spectra session ended".to_string(),
                });
            }
            let _ = flush_clients(&mut clients, &mut app, &poller);
            break;
        }

        did_work |= flush_clients(&mut clients, &mut app, &poller)?;
        did_work |= flush_api_connections(&mut api_connections, &poller);
        if did_work {
            // Something progressed; more work may be immediately runnable
            // (partial writes, renders released from a hold), so run the
            // phases again before blocking.
            continue;
        }

        // Idle: sleep until fd readiness, a `wake::notify` from an fd-less
        // producer thread, the next tick deadline, or the heartbeat cap —
        // whichever comes first. Every phase above is written against
        // nonblocking sockets and `try_recv`, so spurious wakeups are safe.
        rearm_poll_interest(
            &poller,
            &listener,
            &api_listener,
            &clients,
            &api_connections,
        )?;
        let now = Instant::now();
        let timeout = app.next_deadline(now).map_or(POLL_HEARTBEAT, |deadline| {
            deadline.saturating_duration_since(now).min(POLL_HEARTBEAT)
        });
        poll_events.clear();
        // The events themselves are not inspected: the phases poll every
        // source anyway, so readiness only needs to end the wait.
        poller.wait(&mut poll_events, Some(timeout))?;
    }

    Ok(())
}

type UpdateCheckResult = Result<Option<String>, String>;

/// Spawn the named background update-check thread. The checker is injected
/// so tests never touch the network; production passes
/// [`crate::upgrade::check_latest`].
fn spawn_update_check<F>(checker: F) -> Option<mpsc::Receiver<UpdateCheckResult>>
where
    F: FnOnce() -> UpdateCheckResult + Send + 'static,
{
    let (tx, rx) = mpsc::channel();
    thread::Builder::new()
        .name("spectra-update-check".to_string())
        .spawn(move || {
            let _ = tx.send(checker());
            // Wake the server loop so the result is applied immediately
            // instead of on the next heartbeat.
            crate::runtime::wake::notify();
        })
        .ok()
        .map(|_handle| rx)
}

/// Drain the update-check channel without ever blocking the server loop.
fn poll_update_check(
    receiver: &mut Option<mpsc::Receiver<UpdateCheckResult>>,
    app: &mut App,
) -> bool {
    let Some(rx) = receiver else {
        return false;
    };
    match rx.try_recv() {
        Ok(result) => {
            app.apply_update_check_result(result);
            *receiver = None;
            true
        }
        Err(mpsc::TryRecvError::Empty) => false,
        Err(mpsc::TryRecvError::Disconnected) => {
            *receiver = None;
            false
        }
    }
}

fn accept_api_connections(
    listener: &UnixListener,
    connections: &mut Vec<ApiConnection>,
    poller: &Poller,
    next_poll_key: &mut usize,
) -> io::Result<bool> {
    let mut accepted = false;
    loop {
        match listener.accept() {
            Ok((stream, _addr)) => {
                stream.set_nonblocking(true)?;
                let poll_key = *next_poll_key;
                *next_poll_key = next_poll_key.saturating_add(2);
                // SAFETY: the stream is owned by `connections` and is
                // deleted from the poller before removal drops it
                // (`flush_api_connections`).
                unsafe { poller.add(&stream, Event::readable(poll_key))? };
                connections.push(ApiConnection::new(stream, poll_key));
                accepted = true;
            }
            Err(err) if err.kind() == io::ErrorKind::WouldBlock => break,
            Err(err) if err.kind() == io::ErrorKind::Interrupted => continue,
            Err(err) => return Err(err),
        }
    }
    Ok(accepted)
}

fn process_api_input(
    connections: &mut [ApiConnection],
    app: &mut App,
    handoff_requested: &mut bool,
) -> bool {
    let mut had_input = false;
    for connection in connections {
        if connection.disconnected {
            continue;
        }
        for request in connection.read_request_lines() {
            had_input = true;
            if request.trim().is_empty() {
                continue;
            }
            let outcome = crate::api::dispatch(app, &request);
            connection.queue_response(&outcome.response);
            if let Some(subscription) = outcome.subscription {
                connection.subscription = Some(subscription);
            }
            *handoff_requested |= outcome.handoff_requested;
        }
    }
    had_input
}

/// Drain the app's queued API events and queue each on every subscribed
/// connection whose filter matches. Events are dropped when no connection
/// is subscribed, so the queue never grows without consumers.
fn fan_out_api_events(connections: &mut [ApiConnection], app: &mut App) -> bool {
    let events = app.take_pending_api_events();
    if events.is_empty() {
        return false;
    }
    let mut queued = false;
    for event in &events {
        let line = event.event_line();
        for connection in connections.iter_mut() {
            if connection.disconnected {
                continue;
            }
            let Some(subscription) = &connection.subscription else {
                continue;
            };
            if subscription.matches(&event.name) {
                connection.queue_response(&line);
                queued = true;
            }
        }
    }
    queued
}

fn flush_api_connections(connections: &mut Vec<ApiConnection>, poller: &Poller) -> bool {
    let mut progressed = false;
    let mut index = 0usize;
    while index < connections.len() {
        progressed |= connections[index].flush();
        if connections[index].disconnected {
            let removed = connections.remove(index);
            let _ = poller.delete(&removed.stream);
            progressed = true;
        } else {
            index += 1;
        }
    }
    progressed
}

struct ApiConnection {
    stream: UnixStream,
    /// Stable poller registration key (odd, assigned at accept and never
    /// reused); vector indexes shift on removal so they cannot serve as keys.
    poll_key: usize,
    read_buffer: Vec<u8>,
    write_buffer: Vec<u8>,
    /// Set by `events.subscribe`; when present, matching API events are
    /// pushed to this connection as `{"event": ..., "params": ...}` lines.
    subscription: Option<crate::api::EventSubscription>,
    disconnected: bool,
}

impl ApiConnection {
    fn new(stream: UnixStream, poll_key: usize) -> Self {
        Self {
            stream,
            poll_key,
            read_buffer: Vec::new(),
            write_buffer: Vec::new(),
            subscription: None,
            disconnected: false,
        }
    }

    fn read_request_lines(&mut self) -> Vec<String> {
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

        let mut lines = Vec::new();
        while let Some(newline) = self.read_buffer.iter().position(|byte| *byte == b'\n') {
            let line: Vec<u8> = self.read_buffer.drain(..=newline).collect();
            lines.push(String::from_utf8_lossy(&line[..line.len() - 1]).into_owned());
        }
        if self.read_buffer.len() > API_MAX_LINE_BYTES {
            self.disconnected = true;
        }
        lines
    }

    fn queue_response(&mut self, response: &str) {
        self.write_buffer.extend_from_slice(response.as_bytes());
        self.write_buffer.push(b'\n');
    }

    fn flush(&mut self) -> bool {
        if self.disconnected || self.write_buffer.is_empty() {
            return false;
        }

        let mut written = 0usize;
        while written < self.write_buffer.len() {
            match self.stream.write(&self.write_buffer[written..]) {
                Ok(0) => {
                    self.disconnected = true;
                    break;
                }
                Ok(n) => written += n,
                Err(err) if err.kind() == io::ErrorKind::WouldBlock => break,
                Err(err) if err.kind() == io::ErrorKind::Interrupted => continue,
                Err(_err) => {
                    self.disconnected = true;
                    break;
                }
            }
        }
        if written > 0 {
            self.write_buffer.drain(..written);
        }
        written > 0
    }
}

fn accept_clients(
    listener: &UnixListener,
    clients: &mut Vec<ClientConnection>,
    app: &mut App,
    next_client_id: &mut ClientId,
    poller: &Poller,
) -> io::Result<bool> {
    let mut accepted = false;
    loop {
        match listener.accept() {
            Ok((stream, _addr)) => {
                stream.set_nonblocking(true)?;
                let client_id = *next_client_id;
                *next_client_id = client_id.saturating_add(1);
                // SAFETY: the stream is owned by `clients` and is deleted
                // from the poller before removal drops it (`flush_clients`).
                unsafe { poller.add(&stream, Event::readable(client_poll_key(client_id)))? };
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
            protocol_version,
            host_colors,
        } => {
            // `None` marks a legacy client that predates version negotiation
            // and is still accepted; an explicit mismatch is rejected.
            if let Some(version) = protocol_version
                && version != PROTOCOL_VERSION
            {
                client.queue_control_message(&ServerMessage::Error {
                    message: format!(
                        "protocol version mismatch (client {version}, server {PROTOCOL_VERSION}) — update spectra on both ends"
                    ),
                })?;
                client.close_after_flush = true;
                return Ok(());
            }
            // Cache the client's host terminal colors so guests can query
            // default fg/bg via OSC 10/11 (last attached client wins; a
            // legacy client without the field resets to "unknown").
            app.set_host_colors(host_colors.unwrap_or_default());
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
        ClientMessage::PasteImage { image } => {
            let image = image.and_then(|payload| decode_paste_image_payload(&payload));
            if let Err(err) = app.handle_paste_image_for_client(client.id, image) {
                client.queue_control_message(&ServerMessage::Error {
                    message: format!("image paste failed: {err}"),
                })?;
            }
        }
    }
    queue_pending_clipboard_messages(client, app)?;
    Ok(())
}

/// Decode and bound a bridged clipboard image; `None` (treated as "no image
/// on the clipboard") when the payload is malformed or oversized.
fn decode_paste_image_payload(
    payload: &crate::ipc::protocol::PasteImagePayload,
) -> Option<(String, Vec<u8>)> {
    use base64::Engine as _;

    // Bound before decoding: 4 base64 chars encode 3 bytes.
    if payload.data_base64.len() > MAX_PASTE_IMAGE_BYTES / 3 * 4 + 4 {
        return None;
    }
    let bytes = base64::engine::general_purpose::STANDARD
        .decode(&payload.data_base64)
        .ok()?;
    if bytes.is_empty() || bytes.len() > MAX_PASTE_IMAGE_BYTES {
        return None;
    }
    Some((payload.format.clone(), bytes))
}

/// Turn each client's pending paste-image flag into a `PasteImageRequest`
/// frame, mirroring the passthrough drain. The local (in-process) client
/// has no socket to answer on, so its flag is dropped.
fn queue_pending_image_paste_requests(
    clients: &mut [ClientConnection],
    app: &mut App,
) -> io::Result<bool> {
    let mut queued = false;
    for client in clients {
        if client.disconnected || !client.renders_enabled {
            continue;
        }
        if app.take_pending_image_paste_request_for_client(client.id) {
            client.queue_control_message(&ServerMessage::PasteImageRequest)?;
            queued = true;
        }
    }
    let _ = app.take_pending_image_paste_request_for_client(LOCAL_CLIENT_ID);
    Ok(queued)
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
        // Keep the client's host-terminal mouse capture in sync with what
        // this frame needs. Sent as a control message so a later render
        // replacing this one in the queue cannot drop the toggle.
        if snapshot.wants_mouse_capture != client.host_mouse_capture {
            client.queue_control_message(&ServerMessage::Passthrough {
                ansi: terminal::mouse_capture_sequence(snapshot.wants_mouse_capture),
            })?;
            client.host_mouse_capture = snapshot.wants_mouse_capture;
        }
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

fn flush_clients(
    clients: &mut Vec<ClientConnection>,
    app: &mut App,
    poller: &Poller,
) -> io::Result<bool> {
    let mut progressed = false;
    let mut index = 0usize;
    while index < clients.len() {
        progressed |= clients[index].flush()?;
        if clients[index].disconnected {
            let removed = clients.remove(index);
            let _ = poller.delete(&removed.stream);
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
    /// Whether the client's host terminal currently has mouse capture
    /// enabled. Starts false (client setup does not capture); toggles are
    /// sent as passthrough control messages when render snapshots demand a
    /// different state.
    host_mouse_capture: bool,
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
            host_mouse_capture: false,
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

/// Serve one accepted `server.handoff` request as the outgoing server:
/// bind the handoff socket, flush the pending API response so the
/// successor learns the socket path, wait for it to connect, and run the
/// header/fd exchange up to the successor's final ack.
///
/// Errors anywhere in here leave this server untouched (nothing disarmed,
/// nothing unbound); the caller logs and keeps serving. The temporary
/// handoff socket file is removed by its own guard either way.
fn perform_handoff(
    app: &mut App,
    api_connections: &mut Vec<ApiConnection>,
    poller: &Poller,
) -> io::Result<UnixStream> {
    use crate::runtime::handoff::{EXCHANGE_TIMEOUT, FDS_ACK, HEADER_ACK, read_line};

    let handoff_socket = socket_path::handoff_socket_path();
    socket_path::prepare_listener_socket(&handoff_socket)?;
    let handoff_listener = UnixListener::bind(&handoff_socket)?;
    handoff_listener.set_nonblocking(true)?;
    let _handoff_cleanup = SocketCleanupGuard::new(handoff_socket);

    // Deliver the queued `server.handoff` response (with the socket path)
    // before waiting for the successor to connect.
    let deadline = Instant::now() + EXCHANGE_TIMEOUT;
    while api_connections
        .iter()
        .any(|connection| !connection.disconnected && !connection.write_buffer.is_empty())
    {
        flush_api_connections(api_connections, poller);
        if Instant::now() >= deadline {
            return Err(io::Error::new(
                io::ErrorKind::TimedOut,
                "timed out flushing the server.handoff response",
            ));
        }
        thread::sleep(Duration::from_millis(5));
    }

    let successor = loop {
        match handoff_listener.accept() {
            Ok((stream, _addr)) => break stream,
            Err(err) if err.kind() == io::ErrorKind::WouldBlock => {
                if Instant::now() >= deadline {
                    return Err(io::Error::new(
                        io::ErrorKind::TimedOut,
                        "no successor connected to the handoff socket",
                    ));
                }
                thread::sleep(Duration::from_millis(10));
            }
            Err(err) if err.kind() == io::ErrorKind::Interrupted => continue,
            Err(err) => return Err(err),
        }
    };
    successor.set_nonblocking(false)?;
    successor.set_read_timeout(Some(EXCHANGE_TIMEOUT))?;
    successor.set_write_timeout(Some(EXCHANGE_TIMEOUT))?;

    // Drain pending pane output first so the replay tails are current.
    // Bytes the reader threads consume after this snapshot are lost to the
    // replay (not to the panes), which is why replay is best-effort only.
    app.tick();
    let (header, fds) = app.export_handoff()?;
    let mut header_line = serde_json::to_vec(&header).map_err(io::Error::other)?;
    header_line.push(b'\n');
    (&successor).write_all(&header_line)?;

    let ack = read_line(&successor, 4096)?;
    if ack.trim() != HEADER_ACK {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("expected header ack, got {ack:?}"),
        ));
    }

    for batch in fds.chunks(crate::ipc::fdpass::MAX_FDS_PER_MESSAGE) {
        let raw: Vec<std::os::fd::RawFd> = batch.iter().map(AsRawFd::as_raw_fd).collect();
        crate::ipc::fdpass::send_with_fds(&successor, b"F", &raw)?;
    }
    // The duplicated fds in `fds` drop here; the successor holds its own
    // copies and this server's originals stay open in the pane backends.

    let ack = read_line(&successor, 4096)?;
    if ack.trim() != FDS_ACK {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("expected fd ack, got {ack:?}"),
        ));
    }
    Ok(successor)
}

struct SocketCleanupGuard {
    path: Option<PathBuf>,
}

impl SocketCleanupGuard {
    fn new(path: PathBuf) -> Self {
        Self { path: Some(path) }
    }

    /// Remove the socket file immediately and defuse the guard. Used during
    /// a handoff: the file must disappear before the successor binds, and
    /// this process's later exit must not delete the successor's socket.
    fn remove_now(&mut self) {
        if let Some(path) = self.path.take() {
            let _ = fs::remove_file(&path);
        }
    }
}

impl Drop for SocketCleanupGuard {
    fn drop(&mut self) {
        if let Some(path) = &self.path {
            let _ = fs::remove_file(path);
        }
    }
}

#[cfg(test)]
mod tests {
    use std::os::unix::net::UnixStream;

    use super::{ClientConnection, QueuedMessage, render_payload_with_window_title};
    use crate::ipc::protocol::ServerMessage;

    #[test]
    fn spawn_update_check_delivers_injected_result_from_named_thread() {
        let rx = super::spawn_update_check(|| {
            assert_eq!(
                std::thread::current().name(),
                Some("spectra-update-check"),
                "update check must run on the named background thread"
            );
            Ok(Some("9.9.9".to_string()))
        })
        .expect("spawn update check thread");

        let result = rx
            .recv_timeout(std::time::Duration::from_secs(5))
            .expect("update check result over channel");
        assert_eq!(result, Ok(Some("9.9.9".to_string())));
    }

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
