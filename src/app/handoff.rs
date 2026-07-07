//! Live server handoff state: export from the outgoing server, adoption by
//! its successor.
//!
//! The wire header reuses the persisted runtime-state types
//! ([`AppRuntimeState`] / `SessionRuntimeSnapshot`) for the layout and adds
//! per-pane transfer entries (fd index, child pid, ≤8 KiB replay tail,
//! title/cwd/agent metadata). The PTY fds themselves travel separately as
//! SCM_RIGHTS ancillary data; `fd_index` ties each pane to its descriptor.

use std::collections::HashMap;
use std::io;
use std::os::fd::OwnedFd;
use std::sync::Arc;
use std::time::Instant;

use base64::Engine as _;
use base64::engine::general_purpose::STANDARD as BASE64;
use serde::{Deserialize, Serialize};

use crate::agent::{AgentState, AgentStatus};
use crate::cli::Cli;
use crate::session::handoff_backend::{HandoffPaneFactory, PaneHandoffKey, PaneHandoffSource};
use crate::session::pty_backend::PtyPaneFactory;

use super::types::{AppRuntimeState, RUNTIME_STATE_VERSION};
use super::{App, AppBootstrap, LOCAL_CLIENT_ID};

/// Version of the handoff header/fd protocol.
pub const HANDOFF_VERSION: u32 = 1;

/// Hard cap on PTY fds transferred in one handoff (v1). A server hosting
/// more panes refuses the handoff with a clear error instead of a partial
/// transfer.
pub const MAX_FDS_PER_HANDOFF: usize = 64;

/// First line sent over the handoff socket: everything the successor needs
/// except the descriptors themselves.
#[derive(Debug, Serialize, Deserialize)]
pub struct HandoffHeader {
    version: u32,
    fd_count: usize,
    state: AppRuntimeState,
    panes: Vec<HandoffPaneEntry>,
}

impl HandoffHeader {
    pub fn version(&self) -> u32 {
        self.version
    }

    pub fn fd_count(&self) -> usize {
        self.fd_count
    }

    pub fn pane_count(&self) -> usize {
        self.panes.len()
    }
}

#[derive(Debug, Serialize, Deserialize)]
struct HandoffPaneEntry {
    session_id: String,
    pane_id: usize,
    /// Index into the SCM_RIGHTS fd stream (order of transmission).
    fd_index: usize,
    child_pid: Option<u32>,
    /// Last ≤8 KiB of raw pane output, base64 encoded.
    replay_b64: String,
    terminal_title: Option<String>,
    cwd: Option<String>,
    agent_kind: Option<String>,
    agent_state: Option<String>,
}

/// Pure pre-flight validation for a `server.handoff` request; kept separate
/// from `App` so the refusal rules are unit-testable without a server.
pub fn validate_handoff_request(attached_clients: usize, pane_count: usize) -> Result<(), String> {
    if attached_clients > 0 {
        return Err(format!(
            "handoff refused: {attached_clients} client(s) attached — detach all clients and retry \
             (v1 transfers panes only; clients must reattach manually)"
        ));
    }
    if pane_count > MAX_FDS_PER_HANDOFF {
        return Err(format!(
            "handoff refused: {pane_count} panes exceed the v1 fd transfer cap of {MAX_FDS_PER_HANDOFF}"
        ));
    }
    Ok(())
}

fn parse_agent_state(raw: &str) -> AgentState {
    match raw {
        "idle" => AgentState::Idle,
        "working" => AgentState::Working,
        "blocked" => AgentState::Blocked,
        _ => AgentState::Unknown,
    }
}

impl App {
    /// Number of socket clients currently attached (the implicit local
    /// client is not counted).
    pub fn attached_client_count(&self) -> usize {
        let active = usize::from(self.active_client_id != LOCAL_CLIENT_ID);
        active
            + self
                .inactive_client_states
                .keys()
                .filter(|id| **id != LOCAL_CLIENT_ID)
                .count()
    }

    /// Total pane count across all sessions.
    pub fn total_pane_count(&self) -> usize {
        self.sessions
            .iter()
            .map(|managed| managed.session.pane_count())
            .sum()
    }

    /// Handle a `server.handoff` API request: validate, and return the
    /// socket the successor must connect to. The actual transfer is driven
    /// by the server loop once the response is flushed.
    pub fn api_server_handoff(&mut self) -> Result<serde_json::Value, String> {
        validate_handoff_request(self.attached_client_count(), self.total_pane_count())?;
        self.write_log("server handoff requested");
        Ok(serde_json::json!({
            "socket": crate::ipc::socket_path::handoff_socket_path(),
            "version": HANDOFF_VERSION,
        }))
    }

    /// Export the full runtime state plus one duplicated PTY master fd per
    /// pane. The originals stay open and armed: nothing in the outgoing
    /// server changes until the successor acks receipt.
    pub fn export_handoff(&mut self) -> io::Result<(HandoffHeader, Vec<OwnedFd>)> {
        self.capture_active_client_focus_profile();
        let state = self.runtime_state_snapshot();

        let mut fds: Vec<OwnedFd> = Vec::new();
        let mut panes = Vec::new();
        for managed in &self.sessions {
            for pane_id in managed.session.all_pane_ids() {
                let export = managed.session.pane_handoff_export(pane_id)?;
                let duplicated = crate::ipc::fdpass::dup_fd_cloexec(export.master_fd)?;
                panes.push(HandoffPaneEntry {
                    session_id: managed.session_id.clone(),
                    pane_id,
                    fd_index: fds.len(),
                    child_pid: export.child_pid,
                    replay_b64: BASE64.encode(&export.replay),
                    terminal_title: managed.terminal_titles.get(&pane_id).cloned(),
                    cwd: managed.cwd_fallbacks.get(&pane_id).cloned(),
                    agent_kind: managed
                        .agents
                        .statuses
                        .get(&pane_id)
                        .map(|status| status.kind.clone()),
                    agent_state: managed
                        .agents
                        .statuses
                        .get(&pane_id)
                        .map(|status| status.state.as_str().to_string()),
                });
                fds.push(duplicated);
            }
        }

        if fds.len() > MAX_FDS_PER_HANDOFF {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                format!(
                    "{} panes exceed the v1 fd transfer cap of {MAX_FDS_PER_HANDOFF}",
                    fds.len()
                ),
            ));
        }

        Ok((
            HandoffHeader {
                version: HANDOFF_VERSION,
                fd_count: fds.len(),
                state,
                panes,
            },
            fds,
        ))
    }

    /// Log an aborted handoff (server loop hook; `write_log` is private to
    /// the app module).
    pub fn note_handoff_abort(&mut self, err: &str) {
        self.write_log(&format!("server handoff aborted; still serving: {err}"));
    }

    /// Disarm kill-on-drop for every pane child in every session. Called by
    /// the outgoing server after the successor acked the fd transfer, so
    /// process exit leaves all pane processes running.
    pub fn disarm_pane_children(&mut self) {
        for managed in &mut self.sessions {
            managed.session.disarm_pane_children();
        }
    }

    /// Build an `App` that adopts a running server's sessions: panes wrap
    /// the transferred PTY fds instead of spawning new processes, replay
    /// tails repaint each screen, and titles/cwds/agent kinds carry over.
    pub fn new_from_handoff(
        cli: Cli,
        cols: u16,
        rows: u16,
        header: HandoffHeader,
        fds: Vec<OwnedFd>,
    ) -> io::Result<Self> {
        let HandoffHeader {
            version,
            fd_count,
            state,
            panes,
        } = header;
        if version != HANDOFF_VERSION {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!("unsupported handoff version {version} (expected {HANDOFF_VERSION})"),
            ));
        }
        if fd_count != fds.len() || panes.len() != fds.len() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!(
                    "handoff fd mismatch: header declares {fd_count} fds for {} panes, received {}",
                    panes.len(),
                    fds.len()
                ),
            ));
        }
        if state.version != RUNTIME_STATE_VERSION {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!("unsupported runtime state version {}", state.version),
            ));
        }

        // Match every transferred fd to its pane; indices must form a
        // permutation of the fd stream.
        let mut slots: Vec<Option<OwnedFd>> = fds.into_iter().map(Some).collect();
        let mut sources: HashMap<PaneHandoffKey, PaneHandoffSource> = HashMap::new();
        for entry in &panes {
            let master = slots
                .get_mut(entry.fd_index)
                .and_then(Option::take)
                .ok_or_else(|| {
                    io::Error::new(
                        io::ErrorKind::InvalidData,
                        format!(
                            "handoff pane {} of session {} has invalid fd index {}",
                            entry.pane_id, entry.session_id, entry.fd_index
                        ),
                    )
                })?;
            sources.insert(
                (entry.session_id.clone(), entry.pane_id),
                PaneHandoffSource {
                    master,
                    child_pid: entry.child_pid,
                },
            );
        }

        let AppBootstrap {
            options,
            store,
            runtime_ui,
            started_unix,
        } = Self::bootstrap_from_cli(cli)?;

        let factory = Arc::new(HandoffPaneFactory::new(sources));
        let mut app = Self::build_from_runtime_state(
            &store,
            started_unix,
            options,
            runtime_ui,
            cols,
            rows,
            state,
            factory,
        )?;

        // Future pane spawns (splits, new windows) must create real PTYs.
        let pty_factory: Arc<dyn crate::session::pty_backend::PaneFactory> =
            Arc::new(PtyPaneFactory);
        for managed in &mut app.sessions {
            managed.session.set_pane_factory(Arc::clone(&pty_factory));
        }

        app.apply_handoff_pane_metadata(&panes)?;
        app.sync_tree_names();
        app.request_render(true);
        app.persist_active_session_info();
        app.write_log("session adopted via live server handoff");
        Ok(app)
    }

    /// Replay each pane's transferred output tail into its terminal state
    /// and restore per-pane metadata (titles, cwd fallbacks, agent kinds).
    fn apply_handoff_pane_metadata(&mut self, panes: &[HandoffPaneEntry]) -> io::Result<()> {
        let now = Instant::now();
        for entry in panes {
            let Some(managed) = self
                .sessions
                .iter_mut()
                .find(|managed| managed.session_id == entry.session_id)
            else {
                continue;
            };

            let replay = BASE64.decode(&entry.replay_b64).map_err(|err| {
                io::Error::new(
                    io::ErrorKind::InvalidData,
                    format!(
                        "invalid replay payload for pane {} of session {}: {err}",
                        entry.pane_id, entry.session_id
                    ),
                )
            })?;
            if !replay.is_empty() {
                managed.session.feed_pane_replay(entry.pane_id, &replay);
            }

            if let Some(title) = &entry.terminal_title {
                managed.terminal_titles.insert(entry.pane_id, title.clone());
            }
            if let Some(cwd) = &entry.cwd {
                managed.cwd_fallbacks.insert(entry.pane_id, cwd.clone());
                managed
                    .session
                    .seed_pane_cwd(entry.pane_id, std::path::PathBuf::from(cwd));
            }
            let auto_name = Self::resolve_auto_pane_name(managed, entry.pane_id);
            Self::set_name(
                &mut managed.pane_auto_names,
                entry.pane_id,
                auto_name.clone(),
            );
            if let Some(window_id) = managed.session.window_id_for_pane(entry.pane_id) {
                Self::set_name(&mut managed.window_auto_names, window_id, auto_name);
            }

            if let Some(kind) = &entry.agent_kind {
                let state = entry
                    .agent_state
                    .as_deref()
                    .map(parse_agent_state)
                    .unwrap_or(AgentState::Unknown);
                managed.agents.statuses.insert(
                    entry.pane_id,
                    AgentStatus {
                        kind: kind.clone(),
                        state,
                        since: now,
                    },
                );
                // Mark as seen so an idle agent never shows a spurious
                // "done" right after the handoff.
                managed.agents.seen.insert(entry.pane_id);
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;

    use super::super::types::{AppRuntimeState, RUNTIME_STATE_VERSION, SessionRuntimeState};
    use super::{
        HANDOFF_VERSION, HandoffHeader, HandoffPaneEntry, MAX_FDS_PER_HANDOFF,
        validate_handoff_request,
    };

    fn sample_header() -> HandoffHeader {
        let snapshot = crate::session::manager::SessionRuntimeSnapshot {
            session_name: "main".to_string(),
            next_pane_id: 3,
            next_window_id: 2,
            active_window: 0,
            windows: vec![crate::session::manager::SessionWindowSnapshot {
                id: 1,
                manager: crate::ui::window_manager::WindowManager::new(1).snapshot(),
                zoomed: false,
                synchronize_panes: false,
                zoom_snapshot: None,
            }],
            pane_cwds: HashMap::new(),
        };

        HandoffHeader {
            version: HANDOFF_VERSION,
            fd_count: 1,
            state: AppRuntimeState {
                version: RUNTIME_STATE_VERSION,
                active_session: 0,
                next_session_ordinal: 2,
                sessions: vec![SessionRuntimeState {
                    ordinal: 1,
                    session_id: "main-1".to_string(),
                    session: snapshot,
                    window_names: HashMap::new(),
                    pane_names: HashMap::new(),
                }],
                client_focus_profiles: HashMap::new(),
            },
            panes: vec![HandoffPaneEntry {
                session_id: "main-1".to_string(),
                pane_id: 1,
                fd_index: 0,
                child_pid: Some(4242),
                replay_b64: "aGVsbG8=".to_string(),
                terminal_title: Some("vim".to_string()),
                cwd: Some("/tmp".to_string()),
                agent_kind: Some("claude".to_string()),
                agent_state: Some("working".to_string()),
            }],
        }
    }

    #[test]
    fn handoff_header_roundtrips_through_json() {
        let header = sample_header();
        let line = serde_json::to_string(&header).expect("serialize header");
        let parsed: HandoffHeader = serde_json::from_str(&line).expect("parse header");

        assert_eq!(parsed.version(), HANDOFF_VERSION);
        assert_eq!(parsed.fd_count(), 1);
        assert_eq!(parsed.pane_count(), 1);
        let pane = &parsed.panes[0];
        assert_eq!(pane.session_id, "main-1");
        assert_eq!(pane.pane_id, 1);
        assert_eq!(pane.fd_index, 0);
        assert_eq!(pane.child_pid, Some(4242));
        assert_eq!(pane.replay_b64, "aGVsbG8=");
        assert_eq!(pane.terminal_title.as_deref(), Some("vim"));
        assert_eq!(pane.cwd.as_deref(), Some("/tmp"));
        assert_eq!(pane.agent_kind.as_deref(), Some("claude"));
        assert_eq!(pane.agent_state.as_deref(), Some("working"));
        assert_eq!(parsed.state.sessions.len(), 1);
        assert_eq!(parsed.state.sessions[0].session_id, "main-1");
    }

    #[test]
    fn handoff_request_is_refused_while_clients_are_attached() {
        let err = validate_handoff_request(2, 1).expect_err("attached clients refuse handoff");
        assert!(err.contains("2 client(s) attached"), "got: {err}");
        assert!(validate_handoff_request(0, 1).is_ok());
    }

    #[test]
    fn handoff_request_is_refused_over_the_fd_cap() {
        let err = validate_handoff_request(0, MAX_FDS_PER_HANDOFF + 1)
            .expect_err("too many panes refuse handoff");
        assert!(
            err.contains(&format!("cap of {MAX_FDS_PER_HANDOFF}")),
            "got: {err}"
        );
        assert!(validate_handoff_request(0, MAX_FDS_PER_HANDOFF).is_ok());
    }
}
