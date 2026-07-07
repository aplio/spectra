use super::*;

impl SessionManager {
    pub fn split_focused(&mut self, axis: SplitAxis, cols: u16, rows: u16) -> Result<(), String> {
        self.ensure_active_window_unzoomed()?;
        let area = workspace_area(cols, rows);
        let inherited_cwd = self.focused_pane_cwd();
        let new_pane_id = self.next_pane_id;
        self.next_pane_id += 1;

        let mut pane_options = self.options.clone();
        if let Some(cwd) = inherited_cwd {
            pane_options.cwd = Some(cwd);
        }
        let mut new_pane = spawn_pane(
            &pane_options,
            &*self.pane_factory,
            new_pane_id,
            area.width.max(1),
            area.height.max(1),
        )
        .map_err(|err| err.to_string())?;

        let _ = new_pane.resize(1, 1);
        self.panes.insert(new_pane_id, new_pane);
        self.active_window_mut()?
            .manager
            .split_focused(axis, new_pane_id);
        self.apply_layout_sizes(cols, rows)
            .map_err(|err| err.to_string())
    }

    pub fn new_window(&mut self, cols: u16, rows: u16) -> Result<(), String> {
        let inherited_cwd = self.focused_pane_cwd();
        self.new_window_with_command_cwd(cols, rows, self.options.command.clone(), inherited_cwd)
    }

    pub fn new_window_with_command(
        &mut self,
        cols: u16,
        rows: u16,
        command: Vec<String>,
    ) -> Result<(), String> {
        self.new_window_with_command_cwd(cols, rows, command, None)
    }

    fn new_window_with_command_cwd(
        &mut self,
        cols: u16,
        rows: u16,
        command: Vec<String>,
        cwd: Option<PathBuf>,
    ) -> Result<(), String> {
        let area = workspace_area(cols, rows);
        let new_pane_id = self.next_pane_id;
        self.next_pane_id += 1;

        let mut pane_options = self.options.clone();
        pane_options.command = command;
        if let Some(cwd) = cwd {
            pane_options.cwd = Some(cwd);
        }
        let new_pane = spawn_pane(
            &pane_options,
            &*self.pane_factory,
            new_pane_id,
            area.width.max(1),
            area.height.max(1),
        )
        .map_err(|err| err.to_string())?;
        self.panes.insert(new_pane_id, new_pane);

        let window_id = self.next_window_id;
        self.next_window_id += 1;
        self.windows.push(SessionWindow {
            id: window_id,
            manager: WindowManager::new(new_pane_id),
            zoomed: false,
            synchronize_panes: false,
            zoom_snapshot: None,
        });
        self.active_window = self.windows.len().saturating_sub(1);
        self.apply_layout_sizes(cols, rows)
            .map_err(|err| err.to_string())
    }

    /// Current working directory of the focused pane, resolved from the cwd it
    /// reported via OSC 7 (or the cwd it was spawned with). Used so a freshly
    /// split pane or new window starts in the same directory the user is
    /// already in, rather than the session's startup directory. Returns `None`
    /// (falling back to the session cwd) when there is no focused pane, no cwd
    /// has been tracked yet, or the tracked path is no longer a directory (e.g.
    /// a stale or remote OSC 7 path that does not resolve locally).
    fn focused_pane_cwd(&self) -> Option<PathBuf> {
        let pane_id = self.focused_pane_id()?;
        let cwd = self.panes.get(&pane_id)?.cwd()?;
        cwd.is_dir().then(|| cwd.to_path_buf())
    }

    pub fn focus(&mut self, direction: Direction, cols: u16, rows: u16) -> Result<(), String> {
        self.active_window_mut()?
            .manager
            .focus_direction(direction, workspace_area(cols, rows))
    }

    pub fn focus_next_window(&mut self) -> Result<(), String> {
        if self.windows.is_empty() {
            return Err("No windows available".to_string());
        }
        self.active_window = (self.active_window + 1) % self.windows.len();
        Ok(())
    }

    pub fn focus_prev_window(&mut self) -> Result<(), String> {
        if self.windows.is_empty() {
            return Err("No windows available".to_string());
        }
        if self.active_window == 0 {
            self.active_window = self.windows.len().saturating_sub(1);
        } else {
            self.active_window -= 1;
        }
        Ok(())
    }

    pub fn focus_window_number(&mut self, number: usize) -> Result<(), String> {
        if number == 0 {
            return Err("Window number must be >= 1".to_string());
        }
        let index = number - 1;
        if index >= self.windows.len() {
            return Err("Window number out of range".to_string());
        }
        self.active_window = index;
        Ok(())
    }

    pub fn focus_pane_id(&mut self, pane_id: PaneId) -> Result<(), String> {
        let Some(window_index) = self
            .windows
            .iter()
            .position(|window| window.manager.contains_pane_id(pane_id))
        else {
            return Err("Pane ID not found".to_string());
        };
        self.active_window = window_index;
        self.active_window_mut()?.manager.focus_pane_id(pane_id)
    }

    pub fn swap_prev_window(&mut self) -> Result<(), String> {
        if self.windows.len() < 2 {
            return Err("Need at least two windows to swap".to_string());
        }
        let target = if self.active_window == 0 {
            self.windows.len().saturating_sub(1)
        } else {
            self.active_window - 1
        };
        self.windows.swap(self.active_window, target);
        self.active_window = target;
        Ok(())
    }

    pub fn swap_next_window(&mut self) -> Result<(), String> {
        if self.windows.len() < 2 {
            return Err("Need at least two windows to swap".to_string());
        }
        let target = (self.active_window + 1) % self.windows.len();
        self.windows.swap(self.active_window, target);
        self.active_window = target;
        Ok(())
    }

    pub fn resize_focused(
        &mut self,
        direction: Direction,
        amount: u16,
        cols: u16,
        rows: u16,
    ) -> Result<(), String> {
        self.ensure_active_window_unzoomed()?;
        self.active_window_mut()?
            .manager
            .resize_focused(direction, amount)?;
        self.apply_layout_sizes(cols, rows)
            .map_err(|err| err.to_string())
    }

    pub fn toggle_zoom_active_window(&mut self, cols: u16, rows: u16) -> Result<bool, String> {
        let window = self.active_window_mut()?;
        if window.zoomed {
            Self::restore_zoom(window)?;
        } else {
            window.zoom_snapshot = Some(window.manager.snapshot());
            window.manager.close_others();
            window.zoomed = true;
        }
        self.apply_layout_sizes(cols, rows)
            .map_err(|err| err.to_string())?;
        Ok(self.active_window_zoomed())
    }

    pub fn toggle_synchronize_panes_active_window(&mut self) -> Result<bool, String> {
        let window = self.active_window_mut()?;
        window.synchronize_panes = !window.synchronize_panes;
        Ok(window.synchronize_panes)
    }

    pub fn close_focused(&mut self, cols: u16, rows: u16) -> Result<(), String> {
        self.ensure_active_window_unzoomed()?;
        let active_index = self.active_window;
        let window_pane_count = self
            .windows
            .get(active_index)
            .ok_or_else(|| "No windows available".to_string())?
            .manager
            .pane_count();

        if window_pane_count > 1 {
            let (window_id, snapshot) = {
                let window = self
                    .windows
                    .get(active_index)
                    .ok_or_else(|| "No windows available".to_string())?;
                (window.id, window.manager.snapshot())
            };
            let pane_id = self
                .active_window_mut()?
                .manager
                .close_focused()
                .map_err(|err| err.to_string())?;
            if let Some(pane) = self.panes.remove(&pane_id) {
                self.retain_closed_pane(pane_id, pane, window_id, active_index, snapshot);
            }
        } else if self.windows.len() > 1 {
            let window = self
                .windows
                .get(active_index)
                .ok_or_else(|| "No windows available".to_string())?;
            let pane_id = window
                .manager
                .focused_pane_id()
                .ok_or_else(|| "No focused pane".to_string())?;
            let window_id = window.id;
            let snapshot = window.manager.snapshot();
            if let Some(pane) = self.panes.remove(&pane_id) {
                self.retain_closed_pane(pane_id, pane, window_id, active_index, snapshot);
            }
            self.windows.remove(active_index);
            if self.active_window >= self.windows.len() {
                self.active_window = self.windows.len().saturating_sub(1);
            }
        } else {
            return Err("Cannot close the last pane".to_string());
        }
        self.apply_layout_sizes(cols, rows)
            .map_err(|err| err.to_string())
    }

    pub fn close_pane(&mut self, pane_id: PaneId, cols: u16, rows: u16) -> Result<(), String> {
        let Some(window_index) = self
            .windows
            .iter()
            .position(|window| window.manager.contains_pane_id(pane_id))
        else {
            return Err(format!("pane {pane_id} not found"));
        };

        {
            let window = self
                .windows
                .get_mut(window_index)
                .ok_or_else(|| "No windows available".to_string())?;
            Self::restore_zoom(window)?;
        }

        let window_pane_count = self
            .windows
            .get(window_index)
            .ok_or_else(|| "No windows available".to_string())?
            .manager
            .pane_count();

        if window_pane_count > 1 {
            let window = self
                .windows
                .get_mut(window_index)
                .ok_or_else(|| "No windows available".to_string())?;
            let window_id = window.id;
            let snapshot = window.manager.snapshot();
            window.manager.focus_pane_id(pane_id)?;
            let closed = window
                .manager
                .close_focused()
                .map_err(|err| err.to_string())?;
            if let Some(pane) = self.panes.remove(&closed) {
                self.retain_closed_pane(closed, pane, window_id, window_index, snapshot);
            }
        } else if self.windows.len() > 1 {
            let window = self
                .windows
                .get(window_index)
                .ok_or_else(|| "No windows available".to_string())?;
            let window_id = window.id;
            let snapshot = window.manager.snapshot();
            if let Some(pane) = self.panes.remove(&pane_id) {
                self.retain_closed_pane(pane_id, pane, window_id, window_index, snapshot);
            }
            self.windows.remove(window_index);
            if self.active_window >= self.windows.len() {
                self.active_window = self.windows.len().saturating_sub(1);
            } else if window_index < self.active_window {
                self.active_window = self.active_window.saturating_sub(1);
            }
        } else {
            return Err("Cannot close the last pane".to_string());
        }

        self.apply_layout_sizes(cols, rows)
            .map_err(|err| err.to_string())
    }

    /// Restore the most recently closed pane (undo close). When the window
    /// it lived in still exists and its layout has not changed since the
    /// close, the pre-close snapshot is restored so the pane returns to its
    /// exact position and size; otherwise the pane re-enters by splitting
    /// that window's focused pane. A window the close removed entirely is
    /// recreated at its old index.
    pub fn restore_last_closed_pane(&mut self, cols: u16, rows: u16) -> Result<PaneId, String> {
        self.purge_expired_closed_panes();
        let Some(entry) = self.closed_panes.pop() else {
            return Err("no recently closed pane to restore".to_string());
        };
        let ClosedPaneEntry {
            pane_id,
            pane,
            window_id,
            window_index,
            window_snapshot,
            ..
        } = entry;

        if let Some(index) = self
            .windows
            .iter()
            .position(|window| window.id == window_id)
        {
            let window = self
                .windows
                .get_mut(index)
                .ok_or_else(|| "No windows available".to_string())?;
            Self::restore_zoom(window)?;
            let mut current = window.manager.ordered_pane_ids();
            current.sort_unstable();
            let mut expected = window_snapshot.ordered_pane_ids();
            expected.retain(|id| *id != pane_id);
            expected.sort_unstable();
            if current == expected {
                window.manager = WindowManager::from_snapshot(window_snapshot)?;
            } else {
                window.manager.split_focused(SplitAxis::Vertical, pane_id);
            }
            window.manager.focus_pane_id(pane_id)?;
            self.active_window = index;
        } else {
            let manager = WindowManager::from_snapshot(window_snapshot)?;
            let index = window_index.min(self.windows.len());
            self.windows.insert(
                index,
                SessionWindow {
                    id: window_id,
                    manager,
                    zoomed: false,
                    synchronize_panes: false,
                    zoom_snapshot: None,
                },
            );
            self.windows[index].manager.focus_pane_id(pane_id)?;
            self.active_window = index;
        }

        self.panes.insert(pane_id, pane);
        self.apply_layout_sizes(cols, rows)
            .map_err(|err| err.to_string())?;
        Ok(pane_id)
    }

    pub fn close_active_window(&mut self, cols: u16, rows: u16) -> Result<(), String> {
        self.close_window(self.active_window, cols, rows)
    }

    pub fn close_window(
        &mut self,
        window_index: usize,
        cols: u16,
        rows: u16,
    ) -> Result<(), String> {
        if window_index >= self.windows.len() {
            return Err(format!("window index {window_index} out of range"));
        }
        if self.windows.len() <= 1 {
            return Err("Cannot close the last window".to_string());
        }

        let window = &self.windows[window_index];

        let mut pane_ids = window.manager.ordered_pane_ids();
        if let Some(snapshot) = &window.zoom_snapshot {
            pane_ids.extend(snapshot.ordered_pane_ids());
        }
        pane_ids.sort_unstable();
        pane_ids.dedup();

        for pane_id in pane_ids {
            self.panes.remove(&pane_id);
        }

        self.windows.remove(window_index);

        if self.active_window >= self.windows.len() {
            self.active_window = self.windows.len().saturating_sub(1);
        } else if window_index < self.active_window {
            self.active_window -= 1;
        }

        self.apply_layout_sizes(cols, rows)
            .map_err(|err| err.to_string())
    }

    pub fn window_entries(&self) -> Vec<WindowEntry> {
        self.windows
            .iter()
            .enumerate()
            .filter_map(|(index, window)| {
                let pane_ids = window.manager.ordered_pane_ids();
                let pane_id = window
                    .manager
                    .focused_pane_id()
                    .or_else(|| pane_ids.first().copied())?;
                let preview = self
                    .panes
                    .get(&pane_id)
                    .map(|pane| pane.row_text(0).trim_end().to_string())
                    .unwrap_or_default();
                Some(WindowEntry {
                    index: index + 1,
                    window_id: window.id,
                    pane_id,
                    pane_ids,
                    focused: index == self.active_window,
                    preview,
                })
            })
            .collect()
    }

    pub fn pane_ids_for_window_number(&self, number: usize) -> Option<Vec<PaneId>> {
        let index = number.checked_sub(1)?;
        self.windows
            .get(index)
            .map(|window| window.manager.ordered_pane_ids())
    }

    pub fn window_id_for_pane(&self, pane_id: PaneId) -> Option<WindowId> {
        self.windows
            .iter()
            .find(|window| window.manager.contains_pane_id(pane_id))
            .map(|window| window.id)
    }

    pub fn all_pane_ids(&self) -> Vec<PaneId> {
        self.windows
            .iter()
            .flat_map(|window| window.manager.ordered_pane_ids())
            .collect()
    }

    pub fn active_window_zoomed(&self) -> bool {
        self.active_window().is_some_and(|window| window.zoomed)
    }

    pub fn active_window_synchronize_panes(&self) -> bool {
        self.active_window()
            .is_some_and(|window| window.synchronize_panes)
    }
}
