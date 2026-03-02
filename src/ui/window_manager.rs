use serde::{Deserialize, Serialize};

use crate::ui::layout as engine;

pub type PaneId = usize;
pub type WindowId = engine::WindowId;

pub use engine::{Direction, Divider, DividerOrientation, PaneRect, SplitAxis};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct PaneLayout {
    pub window_id: WindowId,
    pub pane_id: PaneId,
    pub rect: PaneRect,
}

#[derive(Debug, Default, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Layout {
    pub panes: Vec<PaneLayout>,
    pub dividers: Vec<Divider>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WindowManagerSnapshot {
    core: engine::WindowTreeSnapshot<PaneId>,
}

impl WindowManagerSnapshot {
    pub fn ordered_pane_ids(&self) -> Vec<PaneId> {
        engine::WindowTree::from_snapshot(self.core.clone())
            .map(|tree| tree.ordered_item_ids())
            .unwrap_or_default()
    }
}

pub struct WindowManager {
    core: engine::WindowTree<PaneId>,
}

impl WindowManager {
    pub fn new(initial_pane_id: PaneId) -> Self {
        Self {
            core: engine::WindowTree::new(initial_pane_id),
        }
    }

    pub fn window_count(&self) -> usize {
        self.core.window_count()
    }

    pub fn pane_count(&self) -> usize {
        self.core.window_count()
    }

    pub fn focused_pane_id(&self) -> Option<PaneId> {
        self.core.focused_item_id()
    }

    pub fn split_focused(&mut self, axis: SplitAxis, new_pane_id: PaneId) {
        self.core.split_focused(axis, new_pane_id);
    }

    pub fn layout(&self, area: PaneRect) -> Layout {
        Layout::from(self.core.layout(area))
    }

    pub fn focus_direction(&mut self, direction: Direction, area: PaneRect) -> Result<(), String> {
        self.core.focus_direction(direction, area)
    }

    pub fn close_focused(&mut self) -> Result<PaneId, String> {
        self.core.close_focused()
    }

    pub fn focused_window_id(&self) -> WindowId {
        self.core.focused_window_id()
    }

    pub fn ordered_window_ids(&self) -> Vec<WindowId> {
        self.core.ordered_window_ids()
    }

    pub fn ordered_pane_ids(&self) -> Vec<PaneId> {
        self.core.ordered_item_ids()
    }

    pub fn focused_window_index(&self) -> Option<usize> {
        self.core.focused_window_index()
    }

    pub fn focus_window_index(&mut self, index: usize) -> Result<(), String> {
        self.core.focus_window_index(index)
    }

    pub fn focus_pane_id(&mut self, pane_id: PaneId) -> Result<(), String> {
        self.core.focus_item_id(pane_id)
    }

    pub fn contains_pane_id(&self, pane_id: PaneId) -> bool {
        self.core.contains_item_id(pane_id)
    }

    pub fn focus_next_window(&mut self) -> Result<(), String> {
        self.core.focus_next_window()
    }

    pub fn focus_prev_window(&mut self) -> Result<(), String> {
        self.core.focus_prev_window()
    }

    pub fn swap_with_next_window(&mut self) -> Result<(), String> {
        self.core.swap_with_next_window()
    }

    pub fn swap_with_prev_window(&mut self) -> Result<(), String> {
        self.core.swap_with_prev_window()
    }

    pub fn resize_focused(&mut self, direction: Direction, amount: u16) -> Result<(), String> {
        self.core.resize_focused(direction, amount)
    }

    pub fn close_others(&mut self) {
        self.core.close_others();
    }

    pub fn snapshot(&self) -> WindowManagerSnapshot {
        WindowManagerSnapshot {
            core: self.core.snapshot(),
        }
    }

    pub fn from_snapshot(snapshot: WindowManagerSnapshot) -> Result<Self, String> {
        Ok(Self {
            core: engine::WindowTree::from_snapshot(snapshot.core)?,
        })
    }
}

impl From<engine::PaneLayout<PaneId>> for PaneLayout {
    fn from(value: engine::PaneLayout<PaneId>) -> Self {
        Self {
            window_id: value.window_id,
            pane_id: value.item_id,
            rect: value.rect,
        }
    }
}

impl From<engine::Layout<PaneId>> for Layout {
    fn from(value: engine::Layout<PaneId>) -> Self {
        Self {
            panes: value.panes.into_iter().map(PaneLayout::from).collect(),
            dividers: value.dividers,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn area() -> PaneRect {
        PaneRect {
            x: 0,
            y: 0,
            width: 80,
            height: 20,
        }
    }

    #[test]
    fn split_and_focus() {
        let mut wm = WindowManager::new(1);
        wm.split_focused(SplitAxis::Vertical, 2);
        assert_eq!(wm.focused_pane_id(), Some(2));
        wm.focus_direction(Direction::Left, area())
            .expect("focus left");
        assert_eq!(wm.focused_pane_id(), Some(1));
    }

    #[test]
    fn close_focused_pane() {
        let mut wm = WindowManager::new(1);
        wm.split_focused(SplitAxis::Vertical, 2);
        let closed = wm.close_focused().expect("close focused pane");
        assert_eq!(closed, 2);
        assert_eq!(wm.window_count(), 1);
        assert_eq!(wm.focused_pane_id(), Some(1));
    }

    #[test]
    fn focus_next_prev_cycles() {
        let mut wm = WindowManager::new(1);
        wm.split_focused(SplitAxis::Vertical, 2);
        wm.split_focused(SplitAxis::Horizontal, 3);

        assert_eq!(wm.focused_pane_id(), Some(3));
        wm.focus_next_window().expect("focus next");
        assert_eq!(wm.focused_pane_id(), Some(1));
        wm.focus_prev_window().expect("focus prev");
        assert_eq!(wm.focused_pane_id(), Some(3));
    }

    #[test]
    fn focus_pane_id_selects_window_by_pane() {
        let mut wm = WindowManager::new(1);
        wm.split_focused(SplitAxis::Vertical, 2);
        wm.split_focused(SplitAxis::Horizontal, 3);

        wm.focus_pane_id(1).expect("focus pane 1");
        assert_eq!(wm.focused_pane_id(), Some(1));
        wm.focus_pane_id(3).expect("focus pane 3");
        assert_eq!(wm.focused_pane_id(), Some(3));
        assert!(wm.focus_pane_id(99).is_err());
    }

    #[test]
    fn swap_with_prev_swaps_pane_ids() {
        let mut wm = WindowManager::new(1);
        wm.split_focused(SplitAxis::Vertical, 2);
        let before = wm.ordered_pane_ids();
        assert_eq!(before, vec![1, 2]);

        wm.swap_with_prev_window().expect("swap prev");
        let after = wm.ordered_pane_ids();
        assert_eq!(after, vec![2, 1]);
        assert_eq!(wm.focused_pane_id(), Some(2));
    }

    #[test]
    fn resize_changes_layout_widths() {
        let mut wm = WindowManager::new(1);
        wm.split_focused(SplitAxis::Vertical, 2);
        wm.focus_direction(Direction::Left, area())
            .expect("focus left window");

        let before = wm.layout(area());
        let left_before = before
            .panes
            .iter()
            .find(|pane| pane.pane_id == 1)
            .expect("left pane")
            .rect
            .width;

        wm.resize_focused(Direction::Left, 10).expect("resize left");
        let after = wm.layout(area());
        let left_after = after
            .panes
            .iter()
            .find(|pane| pane.pane_id == 1)
            .expect("left pane")
            .rect
            .width;

        assert!(left_after > left_before);
    }

    #[test]
    fn snapshot_roundtrip_restores_order_and_focus() {
        let mut wm = WindowManager::new(1);
        wm.split_focused(SplitAxis::Vertical, 2);
        wm.split_focused(SplitAxis::Horizontal, 3);
        wm.focus_pane_id(1).expect("focus pane 1");

        let before_layout = wm.layout(area());
        let before_order = wm.ordered_pane_ids();
        let snapshot = wm.snapshot();

        let restored = WindowManager::from_snapshot(snapshot).expect("restore window manager");
        let after_layout = restored.layout(area());
        let after_order = restored.ordered_pane_ids();

        assert_eq!(after_order, before_order);
        assert_eq!(restored.focused_pane_id(), Some(1));
        assert_eq!(after_layout.panes, before_layout.panes);
        assert_eq!(after_layout.dividers, before_layout.dividers);
    }
}
