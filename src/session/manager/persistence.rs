use std::io;
use std::sync::Arc;

use crate::session::pty_backend::{PaneFactory, PtyPaneFactory};
use crate::ui::window_manager::WindowManager;

use super::*;

impl SessionManager {
    pub fn from_runtime_snapshot(
        options: SessionOptions,
        snapshot: SessionRuntimeSnapshot,
        cols: u16,
        rows: u16,
    ) -> io::Result<Self> {
        Self::with_factory_from_runtime_snapshot(
            options,
            Arc::new(PtyPaneFactory),
            snapshot,
            cols,
            rows,
        )
    }

    pub fn with_factory_from_runtime_snapshot(
        options: SessionOptions,
        pane_factory: Arc<dyn PaneFactory>,
        snapshot: SessionRuntimeSnapshot,
        cols: u16,
        rows: u16,
    ) -> io::Result<Self> {
        if snapshot.windows.is_empty() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "runtime snapshot has no windows",
            ));
        }

        let area = workspace_area(cols, rows);
        let mut windows = Vec::new();
        let mut pane_ids = Vec::new();

        for window_snapshot in snapshot.windows {
            let manager = WindowManager::from_snapshot(window_snapshot.manager).map_err(|err| {
                io::Error::new(
                    io::ErrorKind::InvalidData,
                    format!("restore window failed: {err}"),
                )
            })?;
            let mut current_panes = manager.ordered_pane_ids();
            if let Some(snapshot) = window_snapshot.zoom_snapshot.as_ref() {
                current_panes.extend(snapshot.ordered_pane_ids());
            }
            if current_panes.is_empty() {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "runtime snapshot window has no panes",
                ));
            }
            pane_ids.extend(current_panes);
            let zoomed = window_snapshot.zoomed && window_snapshot.zoom_snapshot.is_some();
            windows.push(SessionWindow {
                id: window_snapshot.id,
                manager,
                zoomed,
                synchronize_panes: window_snapshot.synchronize_panes,
                zoom_snapshot: if zoomed {
                    window_snapshot.zoom_snapshot
                } else {
                    None
                },
            });
        }

        pane_ids.sort_unstable();
        pane_ids.dedup();

        let mut panes = HashMap::new();
        for pane_id in pane_ids.iter().copied() {
            let pane = spawn_pane(
                &options,
                &*pane_factory,
                area.width.max(1),
                area.height.max(1),
            )?;
            panes.insert(pane_id, pane);
        }

        let max_pane_id = pane_ids.into_iter().max().unwrap_or(0);
        let max_window_id = windows.iter().map(|window| window.id).max().unwrap_or(0);

        let mut session = Self {
            options,
            pane_factory,
            panes,
            windows,
            active_window: snapshot.active_window,
            next_pane_id: snapshot.next_pane_id.max(max_pane_id + 1),
            next_window_id: snapshot.next_window_id.max(max_window_id + 1),
            session_name: snapshot.session_name,
            pending_passthrough: Vec::new(),
            pending_terminal_events: Vec::new(),
        };
        session.active_window = session
            .active_window
            .min(session.windows.len().saturating_sub(1));
        session
            .apply_layout_sizes(cols, rows)
            .map_err(|err| io::Error::other(err.to_string()))?;
        Ok(session)
    }

    pub fn runtime_snapshot(&self) -> SessionRuntimeSnapshot {
        SessionRuntimeSnapshot {
            session_name: self.session_name.clone(),
            next_pane_id: self.next_pane_id,
            next_window_id: self.next_window_id,
            active_window: self.active_window,
            windows: self
                .windows
                .iter()
                .map(|window| SessionWindowSnapshot {
                    id: window.id,
                    manager: window.manager.snapshot(),
                    zoomed: window.zoomed,
                    synchronize_panes: window.synchronize_panes,
                    zoom_snapshot: window.zoom_snapshot.clone(),
                })
                .collect(),
        }
    }
}
