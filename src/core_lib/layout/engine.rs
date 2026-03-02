use std::collections::HashMap;

use serde::{Deserialize, Serialize};

pub type WindowId = usize;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum SplitAxis {
    Vertical,
    Horizontal,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Direction {
    Left,
    Down,
    Up,
    Right,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum DividerOrientation {
    Vertical,
    Horizontal,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct PaneRect {
    pub x: usize,
    pub y: usize,
    pub width: usize,
    pub height: usize,
}

impl PaneRect {
    fn right(self) -> usize {
        self.x + self.width
    }

    fn bottom(self) -> usize {
        self.y + self.height
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct Divider {
    pub orientation: DividerOrientation,
    pub x: usize,
    pub y: usize,
    pub len: usize,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PaneLayout<ItemId>
where
    ItemId: Copy + Eq,
{
    pub window_id: WindowId,
    pub item_id: ItemId,
    pub rect: PaneRect,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Layout<ItemId>
where
    ItemId: Copy + Eq,
{
    pub panes: Vec<PaneLayout<ItemId>>,
    pub dividers: Vec<Divider>,
}

impl<ItemId> Default for Layout<ItemId>
where
    ItemId: Copy + Eq,
{
    fn default() -> Self {
        Self {
            panes: Vec::new(),
            dividers: Vec::new(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
enum Node {
    Leaf {
        window_id: WindowId,
    },
    Split {
        axis: SplitAxis,
        ratio_percent: u8,
        first: Box<Node>,
        second: Box<Node>,
    },
}

impl Node {
    fn first_leaf(&self) -> WindowId {
        match self {
            Self::Leaf { window_id } => *window_id,
            Self::Split { first, .. } => first.first_leaf(),
        }
    }
}

enum RemoveResult {
    NotFound(Node),
    Removed {
        node: Option<Node>,
        focus_hint: Option<WindowId>,
    },
}

enum ResizeResult {
    NotFound,
    Found,
    Resized,
}

pub struct WindowTree<ItemId>
where
    ItemId: Copy + Eq,
{
    root: Node,
    windows: HashMap<WindowId, ItemId>,
    focused_window: WindowId,
    next_window_id: WindowId,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WindowTreeSnapshot<ItemId>
where
    ItemId: Copy + Eq,
{
    root: Node,
    windows: HashMap<WindowId, ItemId>,
    focused_window: WindowId,
    next_window_id: WindowId,
}

impl<ItemId> WindowTree<ItemId>
where
    ItemId: Copy + Eq,
{
    pub fn new(initial_item_id: ItemId) -> Self {
        let first_window_id = 1;
        let mut windows = HashMap::new();
        windows.insert(first_window_id, initial_item_id);
        Self {
            root: Node::Leaf {
                window_id: first_window_id,
            },
            windows,
            focused_window: first_window_id,
            next_window_id: first_window_id + 1,
        }
    }

    pub fn window_count(&self) -> usize {
        self.windows.len()
    }

    pub fn focused_window_id(&self) -> WindowId {
        self.focused_window
    }

    pub fn focused_item_id(&self) -> Option<ItemId> {
        self.windows.get(&self.focused_window).copied()
    }

    pub fn contains_item_id(&self, item_id: ItemId) -> bool {
        self.windows.values().any(|current| *current == item_id)
    }

    pub fn snapshot(&self) -> WindowTreeSnapshot<ItemId> {
        WindowTreeSnapshot {
            root: self.root.clone(),
            windows: self.windows.clone(),
            focused_window: self.focused_window,
            next_window_id: self.next_window_id,
        }
    }

    pub fn from_snapshot(snapshot: WindowTreeSnapshot<ItemId>) -> Result<Self, String> {
        let mut ordered = Vec::new();
        Self::collect_leaf_order(&snapshot.root, &mut ordered);
        if ordered.is_empty() {
            return Err("window snapshot has no leaves".to_string());
        }

        let mut windows = HashMap::new();
        for window_id in ordered.iter().copied() {
            let Some(item_id) = snapshot.windows.get(&window_id).copied() else {
                return Err(format!("window snapshot missing item for leaf {window_id}"));
            };
            windows.insert(window_id, item_id);
        }

        let focused_window = if windows.contains_key(&snapshot.focused_window) {
            snapshot.focused_window
        } else {
            ordered[0]
        };
        let max_window_id = windows.keys().copied().max().unwrap_or(0);

        Ok(Self {
            root: snapshot.root,
            windows,
            focused_window,
            next_window_id: snapshot.next_window_id.max(max_window_id + 1),
        })
    }

    pub fn set_focused_item(&mut self, item_id: ItemId) {
        if let Some(current) = self.windows.get_mut(&self.focused_window) {
            *current = item_id;
            return;
        }
        let window_id = self.root.first_leaf();
        self.focused_window = window_id;
        self.windows.insert(window_id, item_id);
    }

    pub fn replace_item_refs(&mut self, old_item_id: ItemId, new_item_id: ItemId) {
        for item_id in self.windows.values_mut() {
            if *item_id == old_item_id {
                *item_id = new_item_id;
            }
        }
    }

    pub fn split_focused(&mut self, axis: SplitAxis, new_item_id: ItemId) {
        let new_window_id = self.next_window_id;
        self.next_window_id += 1;
        self.windows.insert(new_window_id, new_item_id);

        let root = std::mem::replace(&mut self.root, Node::Leaf { window_id: 0 });
        let (new_root, split_applied) =
            Self::split_node(root, self.focused_window, axis, new_window_id);
        if split_applied {
            self.root = new_root;
            self.focused_window = new_window_id;
        } else {
            self.windows.remove(&new_window_id);
            self.root = new_root;
        }
    }

    fn split_node(
        node: Node,
        target: WindowId,
        axis: SplitAxis,
        new_window_id: WindowId,
    ) -> (Node, bool) {
        match node {
            Node::Leaf { window_id } if window_id == target => (
                Node::Split {
                    axis,
                    ratio_percent: 50,
                    first: Box::new(Node::Leaf { window_id }),
                    second: Box::new(Node::Leaf {
                        window_id: new_window_id,
                    }),
                },
                true,
            ),
            Node::Leaf { window_id } => (Node::Leaf { window_id }, false),
            Node::Split {
                axis: node_axis,
                ratio_percent,
                first,
                second,
            } => {
                let (first_node, split_applied) =
                    Self::split_node(*first, target, axis, new_window_id);
                if split_applied {
                    (
                        Node::Split {
                            axis: node_axis,
                            ratio_percent,
                            first: Box::new(first_node),
                            second,
                        },
                        true,
                    )
                } else {
                    let (second_node, split_applied) =
                        Self::split_node(*second, target, axis, new_window_id);
                    (
                        Node::Split {
                            axis: node_axis,
                            ratio_percent,
                            first: Box::new(first_node),
                            second: Box::new(second_node),
                        },
                        split_applied,
                    )
                }
            }
        }
    }

    pub fn layout(&self, area: PaneRect) -> Layout<ItemId> {
        if area.width == 0 || area.height == 0 {
            return Layout::default();
        }
        let mut layout = Layout::default();
        self.collect_layout(&self.root, area, &mut layout);
        layout
    }

    fn collect_layout(&self, node: &Node, area: PaneRect, layout: &mut Layout<ItemId>) {
        if area.width == 0 || area.height == 0 {
            return;
        }
        match node {
            Node::Leaf { window_id } => {
                if let Some(item_id) = self.windows.get(window_id).copied() {
                    layout.panes.push(PaneLayout {
                        window_id: *window_id,
                        item_id,
                        rect: area,
                    });
                }
            }
            Node::Split {
                axis: SplitAxis::Vertical,
                ratio_percent,
                first,
                second,
            } => {
                if area.width <= 1 {
                    self.collect_layout(first, area, layout);
                    return;
                }
                let available_width = area.width - 1;
                let first_width = split_span(available_width, *ratio_percent);
                let second_width = available_width.saturating_sub(first_width);
                let divider_x = area.x + first_width;

                let first_area = PaneRect {
                    x: area.x,
                    y: area.y,
                    width: first_width,
                    height: area.height,
                };
                let second_area = PaneRect {
                    x: divider_x + 1,
                    y: area.y,
                    width: second_width,
                    height: area.height,
                };
                self.collect_layout(first, first_area, layout);
                self.collect_layout(second, second_area, layout);
                layout.dividers.push(Divider {
                    orientation: DividerOrientation::Vertical,
                    x: divider_x,
                    y: area.y,
                    len: area.height,
                });
            }
            Node::Split {
                axis: SplitAxis::Horizontal,
                ratio_percent,
                first,
                second,
            } => {
                if area.height <= 1 {
                    self.collect_layout(first, area, layout);
                    return;
                }
                let available_height = area.height - 1;
                let first_height = split_span(available_height, *ratio_percent);
                let second_height = available_height.saturating_sub(first_height);
                let divider_y = area.y + first_height;

                let first_area = PaneRect {
                    x: area.x,
                    y: area.y,
                    width: area.width,
                    height: first_height,
                };
                let second_area = PaneRect {
                    x: area.x,
                    y: divider_y + 1,
                    width: area.width,
                    height: second_height,
                };
                self.collect_layout(first, first_area, layout);
                self.collect_layout(second, second_area, layout);
                layout.dividers.push(Divider {
                    orientation: DividerOrientation::Horizontal,
                    x: area.x,
                    y: divider_y,
                    len: area.width,
                });
            }
        }
    }

    pub fn focused_pane(&self, area: PaneRect) -> Option<PaneLayout<ItemId>> {
        self.layout(area)
            .panes
            .into_iter()
            .find(|pane| pane.window_id == self.focused_window)
    }

    pub fn focus_direction(&mut self, direction: Direction, area: PaneRect) -> Result<(), String> {
        let layout = self.layout(area);
        let focused = layout
            .panes
            .iter()
            .find(|pane| pane.window_id == self.focused_window)
            .copied()
            .ok_or_else(|| "No focused pane".to_string())?;
        let Some(target) = Self::directional_neighbor(&layout.panes, focused, direction) else {
            return Err("No pane in that direction".to_string());
        };
        self.focused_window = target.window_id;
        Ok(())
    }

    pub fn focus_next_by_geometry(&mut self, area: PaneRect) -> Result<(), String> {
        let mut panes = self.layout(area).panes;
        if panes.is_empty() {
            return Err("No panes available".to_string());
        }
        panes.sort_by_key(|pane| (pane.rect.y, pane.rect.x, pane.window_id));
        let idx = panes
            .iter()
            .position(|pane| pane.window_id == self.focused_window)
            .unwrap_or(0);
        let next_idx = (idx + 1) % panes.len();
        self.focused_window = panes[next_idx].window_id;
        Ok(())
    }

    pub fn swap_direction(&mut self, direction: Direction, area: PaneRect) -> Result<(), String> {
        let layout = self.layout(area);
        let focused = layout
            .panes
            .iter()
            .find(|pane| pane.window_id == self.focused_window)
            .copied()
            .ok_or_else(|| "No focused pane".to_string())?;
        let Some(target) = Self::directional_neighbor(&layout.panes, focused, direction) else {
            return Err("No pane in that direction".to_string());
        };

        let Some(focused_item) = self.windows.get(&focused.window_id).copied() else {
            return Err("Focused pane missing".to_string());
        };
        let Some(target_item) = self.windows.get(&target.window_id).copied() else {
            return Err("Target pane missing".to_string());
        };

        self.windows.insert(focused.window_id, target_item);
        self.windows.insert(target.window_id, focused_item);
        Ok(())
    }

    pub fn close_focused(&mut self) -> Result<ItemId, String> {
        if self.window_count() <= 1 {
            return Err("Cannot close the last pane".to_string());
        }

        let closed = self
            .windows
            .get(&self.focused_window)
            .copied()
            .ok_or_else(|| "Focused pane missing".to_string())?;

        let root = std::mem::replace(&mut self.root, Node::Leaf { window_id: 0 });
        match Self::remove_window(root, self.focused_window) {
            RemoveResult::Removed {
                node: Some(new_root),
                focus_hint,
            } => {
                self.windows.remove(&self.focused_window);
                self.root = new_root;
                self.focused_window = focus_hint.unwrap_or_else(|| self.root.first_leaf());
                Ok(closed)
            }
            RemoveResult::Removed { node: None, .. } => {
                Err("Cannot close the last pane".to_string())
            }
            RemoveResult::NotFound(restored) => {
                self.root = restored;
                Err("Focused pane not found".to_string())
            }
        }
    }

    pub fn close_others(&mut self) {
        let Some(focused_item) = self.windows.get(&self.focused_window).copied() else {
            return;
        };
        self.root = Node::Leaf {
            window_id: self.focused_window,
        };
        self.windows.clear();
        self.windows.insert(self.focused_window, focused_item);
    }

    pub fn ordered_window_ids(&self) -> Vec<WindowId> {
        let mut ids = Vec::new();
        Self::collect_leaf_order(&self.root, &mut ids);
        ids
    }

    pub fn ordered_item_ids(&self) -> Vec<ItemId> {
        self.ordered_window_ids()
            .into_iter()
            .filter_map(|window_id| self.windows.get(&window_id).copied())
            .collect()
    }

    pub fn focused_window_index(&self) -> Option<usize> {
        self.ordered_window_ids()
            .iter()
            .position(|window_id| *window_id == self.focused_window)
    }

    pub fn focus_window_index(&mut self, index: usize) -> Result<(), String> {
        let order = self.ordered_window_ids();
        let Some(window_id) = order.get(index).copied() else {
            return Err("Window index out of range".to_string());
        };
        self.focused_window = window_id;
        Ok(())
    }

    pub fn focus_item_id(&mut self, item_id: ItemId) -> Result<(), String> {
        let window_id = self
            .windows
            .iter()
            .find_map(|(window_id, current_item_id)| {
                (*current_item_id == item_id).then_some(*window_id)
            })
            .ok_or_else(|| "Pane ID not found".to_string())?;
        self.focused_window = window_id;
        Ok(())
    }

    pub fn focus_next_window(&mut self) -> Result<(), String> {
        self.focus_offset(1)
    }

    pub fn focus_prev_window(&mut self) -> Result<(), String> {
        self.focus_offset(-1)
    }

    fn focus_offset(&mut self, offset: isize) -> Result<(), String> {
        let order = self.ordered_window_ids();
        if order.is_empty() {
            return Err("No windows available".to_string());
        }
        let current = order
            .iter()
            .position(|window_id| *window_id == self.focused_window)
            .ok_or_else(|| "Focused pane missing".to_string())?;
        let len = order.len() as isize;
        let target = (current as isize + offset).rem_euclid(len) as usize;
        self.focused_window = order[target];
        Ok(())
    }

    pub fn swap_with_next_window(&mut self) -> Result<(), String> {
        self.swap_with_offset(1)
    }

    pub fn swap_with_prev_window(&mut self) -> Result<(), String> {
        self.swap_with_offset(-1)
    }

    fn swap_with_offset(&mut self, offset: isize) -> Result<(), String> {
        let order = self.ordered_window_ids();
        if order.len() < 2 {
            return Err("Need at least two windows to swap".to_string());
        }
        let current = order
            .iter()
            .position(|window_id| *window_id == self.focused_window)
            .ok_or_else(|| "Focused pane missing".to_string())?;
        let target = (current as isize + offset).rem_euclid(order.len() as isize) as usize;
        let focused_window = order[current];
        let target_window = order[target];

        let Some(focused_item) = self.windows.get(&focused_window).copied() else {
            return Err("Focused pane missing".to_string());
        };
        let Some(target_item) = self.windows.get(&target_window).copied() else {
            return Err("Target pane missing".to_string());
        };

        self.windows.insert(focused_window, target_item);
        self.windows.insert(target_window, focused_item);
        self.focused_window = target_window;
        Ok(())
    }

    pub fn resize_focused(&mut self, direction: Direction, amount: u16) -> Result<(), String> {
        if amount == 0 {
            return Ok(());
        }

        match Self::resize_node(
            &mut self.root,
            self.focused_window,
            direction,
            amount as i16,
        ) {
            ResizeResult::Resized => Ok(()),
            ResizeResult::Found => Err("No resizable split in that direction".to_string()),
            ResizeResult::NotFound => Err("Focused pane missing".to_string()),
        }
    }

    pub fn resize_window(
        &mut self,
        window_id: WindowId,
        direction: Direction,
        amount: u16,
    ) -> Result<(), String> {
        if amount == 0 {
            return Ok(());
        }
        if !self.windows.contains_key(&window_id) {
            return Err("Window not found".to_string());
        }

        match Self::resize_node(&mut self.root, window_id, direction, amount as i16) {
            ResizeResult::Resized => Ok(()),
            ResizeResult::Found => Err("No resizable split in that direction".to_string()),
            ResizeResult::NotFound => Err("Window not found".to_string()),
        }
    }

    pub fn resize_between_windows(
        &mut self,
        primary_window: WindowId,
        secondary_window: WindowId,
        orientation: DividerOrientation,
        delta: i16,
    ) -> Result<(), String> {
        if delta == 0 {
            return Ok(());
        }
        if primary_window == secondary_window {
            return Err("Window pair must be distinct".to_string());
        }
        if !self.windows.contains_key(&primary_window)
            || !self.windows.contains_key(&secondary_window)
        {
            return Err("Window not found".to_string());
        }

        let axis = match orientation {
            DividerOrientation::Vertical => SplitAxis::Vertical,
            DividerOrientation::Horizontal => SplitAxis::Horizontal,
        };
        match Self::resize_between_node(
            &mut self.root,
            primary_window,
            secondary_window,
            axis,
            delta,
        ) {
            ResizeResult::Resized => Ok(()),
            ResizeResult::Found => Err("No resizable split in that direction".to_string()),
            ResizeResult::NotFound => Err("Target divider not found".to_string()),
        }
    }

    fn resize_node(
        node: &mut Node,
        target_window: WindowId,
        direction: Direction,
        amount: i16,
    ) -> ResizeResult {
        match node {
            Node::Leaf { window_id } => {
                if *window_id == target_window {
                    ResizeResult::Found
                } else {
                    ResizeResult::NotFound
                }
            }
            Node::Split {
                axis,
                ratio_percent,
                first,
                second,
            } => {
                let first_result = Self::resize_node(first, target_window, direction, amount);
                if matches!(first_result, ResizeResult::Resized) {
                    return ResizeResult::Resized;
                }

                let second_result = Self::resize_node(second, target_window, direction, amount);
                if matches!(second_result, ResizeResult::Resized) {
                    return ResizeResult::Resized;
                }

                let in_first = !matches!(first_result, ResizeResult::NotFound);
                let in_second = !matches!(second_result, ResizeResult::NotFound);
                if !in_first && !in_second {
                    return ResizeResult::NotFound;
                }

                let delta = resize_delta(*axis, direction, in_first, in_second, amount);
                if delta != 0 {
                    let next_ratio = (*ratio_percent as i16 + delta).clamp(10, 90);
                    *ratio_percent = next_ratio as u8;
                    return ResizeResult::Resized;
                }

                ResizeResult::Found
            }
        }
    }

    fn contains_window(node: &Node, target_window: WindowId) -> bool {
        match node {
            Node::Leaf { window_id } => *window_id == target_window,
            Node::Split { first, second, .. } => {
                Self::contains_window(first, target_window)
                    || Self::contains_window(second, target_window)
            }
        }
    }

    fn resize_between_node(
        node: &mut Node,
        primary_window: WindowId,
        secondary_window: WindowId,
        axis: SplitAxis,
        delta: i16,
    ) -> ResizeResult {
        match node {
            Node::Leaf { .. } => ResizeResult::NotFound,
            Node::Split {
                axis: node_axis,
                ratio_percent,
                first,
                second,
            } => {
                let first_has_primary = Self::contains_window(first, primary_window);
                let first_has_secondary = Self::contains_window(first, secondary_window);
                let second_has_primary = Self::contains_window(second, primary_window);
                let second_has_secondary = Self::contains_window(second, secondary_window);

                if !first_has_primary
                    && !first_has_secondary
                    && !second_has_primary
                    && !second_has_secondary
                {
                    return ResizeResult::NotFound;
                }

                if *node_axis == axis
                    && ((first_has_primary && second_has_secondary)
                        || (first_has_secondary && second_has_primary))
                {
                    let signed_delta = if first_has_primary && second_has_secondary {
                        delta
                    } else {
                        -delta
                    };
                    let next_ratio = (*ratio_percent as i16 + signed_delta).clamp(10, 90);
                    if next_ratio == *ratio_percent as i16 {
                        return ResizeResult::Found;
                    }
                    *ratio_percent = next_ratio as u8;
                    return ResizeResult::Resized;
                }

                let first_result =
                    Self::resize_between_node(first, primary_window, secondary_window, axis, delta);
                if matches!(first_result, ResizeResult::Resized) {
                    return ResizeResult::Resized;
                }
                let second_result = Self::resize_between_node(
                    second,
                    primary_window,
                    secondary_window,
                    axis,
                    delta,
                );
                if matches!(second_result, ResizeResult::Resized) {
                    return ResizeResult::Resized;
                }
                if matches!(first_result, ResizeResult::Found)
                    || matches!(second_result, ResizeResult::Found)
                {
                    ResizeResult::Found
                } else {
                    ResizeResult::NotFound
                }
            }
        }
    }

    fn collect_leaf_order(node: &Node, out: &mut Vec<WindowId>) {
        match node {
            Node::Leaf { window_id } => out.push(*window_id),
            Node::Split { first, second, .. } => {
                Self::collect_leaf_order(first, out);
                Self::collect_leaf_order(second, out);
            }
        }
    }

    fn remove_window(node: Node, target_window: WindowId) -> RemoveResult {
        match node {
            Node::Leaf { window_id } => {
                if window_id == target_window {
                    RemoveResult::Removed {
                        node: None,
                        focus_hint: None,
                    }
                } else {
                    RemoveResult::NotFound(Node::Leaf { window_id })
                }
            }
            Node::Split {
                axis,
                ratio_percent,
                first,
                second,
            } => match Self::remove_window(*first, target_window) {
                RemoveResult::Removed {
                    node: new_first,
                    focus_hint,
                } => {
                    if let Some(first_node) = new_first {
                        let new_node = Node::Split {
                            axis,
                            ratio_percent,
                            first: Box::new(first_node),
                            second,
                        };
                        RemoveResult::Removed {
                            focus_hint: focus_hint.or(Some(new_node.first_leaf())),
                            node: Some(new_node),
                        }
                    } else {
                        let survivor = *second;
                        RemoveResult::Removed {
                            focus_hint: Some(survivor.first_leaf()),
                            node: Some(survivor),
                        }
                    }
                }
                RemoveResult::NotFound(restored_first) => {
                    match Self::remove_window(*second, target_window) {
                        RemoveResult::Removed {
                            node: new_second,
                            focus_hint,
                        } => {
                            if let Some(second_node) = new_second {
                                let new_node = Node::Split {
                                    axis,
                                    ratio_percent,
                                    first: Box::new(restored_first),
                                    second: Box::new(second_node),
                                };
                                RemoveResult::Removed {
                                    focus_hint: focus_hint.or(Some(new_node.first_leaf())),
                                    node: Some(new_node),
                                }
                            } else {
                                RemoveResult::Removed {
                                    focus_hint: Some(restored_first.first_leaf()),
                                    node: Some(restored_first),
                                }
                            }
                        }
                        RemoveResult::NotFound(restored_second) => {
                            RemoveResult::NotFound(Node::Split {
                                axis,
                                ratio_percent,
                                first: Box::new(restored_first),
                                second: Box::new(restored_second),
                            })
                        }
                    }
                }
            },
        }
    }

    fn directional_neighbor(
        panes: &[PaneLayout<ItemId>],
        focused: PaneLayout<ItemId>,
        direction: Direction,
    ) -> Option<PaneLayout<ItemId>> {
        let mut best: Option<(usize, usize, PaneLayout<ItemId>)> = None;

        for candidate in panes.iter().copied() {
            if candidate.window_id == focused.window_id {
                continue;
            }
            let overlap = match direction {
                Direction::Left | Direction::Right => interval_overlap(
                    focused.rect.y,
                    focused.rect.bottom(),
                    candidate.rect.y,
                    candidate.rect.bottom(),
                ),
                Direction::Up | Direction::Down => interval_overlap(
                    focused.rect.x,
                    focused.rect.right(),
                    candidate.rect.x,
                    candidate.rect.right(),
                ),
            };
            if overlap == 0 {
                continue;
            }

            let touching = match direction {
                Direction::Left => candidate.rect.x + candidate.rect.width + 1 == focused.rect.x,
                Direction::Right => focused.rect.x + focused.rect.width + 1 == candidate.rect.x,
                Direction::Up => candidate.rect.y + candidate.rect.height + 1 == focused.rect.y,
                Direction::Down => focused.rect.y + focused.rect.height + 1 == candidate.rect.y,
            };
            if !touching {
                continue;
            }

            let distance = match direction {
                Direction::Left => focused
                    .rect
                    .x
                    .saturating_sub(candidate.rect.x + candidate.rect.width),
                Direction::Right => candidate
                    .rect
                    .x
                    .saturating_sub(focused.rect.x + focused.rect.width),
                Direction::Up => focused
                    .rect
                    .y
                    .saturating_sub(candidate.rect.y + candidate.rect.height),
                Direction::Down => candidate
                    .rect
                    .y
                    .saturating_sub(focused.rect.y + focused.rect.height),
            };

            match best {
                None => best = Some((overlap, usize::MAX - distance, candidate)),
                Some((best_overlap, best_distance, _)) => {
                    let rank = (overlap, usize::MAX - distance);
                    if rank > (best_overlap, best_distance) {
                        best = Some((overlap, usize::MAX - distance, candidate));
                    }
                }
            }
        }

        best.map(|(_, _, pane)| pane)
    }
}

fn interval_overlap(start_a: usize, end_a: usize, start_b: usize, end_b: usize) -> usize {
    let start = start_a.max(start_b);
    let end = end_a.min(end_b);
    end.saturating_sub(start)
}

fn split_span(total: usize, ratio_percent: u8) -> usize {
    if total <= 1 {
        return total;
    }

    let raw = (total * usize::from(ratio_percent)) / 100;
    raw.clamp(1, total - 1)
}

fn resize_delta(
    axis: SplitAxis,
    direction: Direction,
    in_first: bool,
    in_second: bool,
    amount: i16,
) -> i16 {
    match (axis, direction) {
        (SplitAxis::Vertical, Direction::Left) => {
            if in_first {
                amount
            } else if in_second {
                -amount
            } else {
                0
            }
        }
        (SplitAxis::Vertical, Direction::Right) => {
            if in_first {
                -amount
            } else if in_second {
                amount
            } else {
                0
            }
        }
        (SplitAxis::Horizontal, Direction::Up) => {
            if in_first {
                amount
            } else if in_second {
                -amount
            } else {
                0
            }
        }
        (SplitAxis::Horizontal, Direction::Down) => {
            if in_first {
                -amount
            } else if in_second {
                amount
            } else {
                0
            }
        }
        _ => 0,
    }
}

#[cfg(test)]
mod tests {
    use super::{Direction, Divider, DividerOrientation, Layout, PaneRect, SplitAxis, WindowTree};

    fn area() -> PaneRect {
        PaneRect {
            x: 0,
            y: 0,
            width: 80,
            height: 24,
        }
    }

    fn divider_window_pair(layout: &Layout<usize>, divider: Divider) -> (usize, usize) {
        match divider.orientation {
            DividerOrientation::Vertical => {
                let left = layout
                    .panes
                    .iter()
                    .find(|pane| pane.rect.x + pane.rect.width == divider.x)
                    .expect("left pane");
                let right = layout
                    .panes
                    .iter()
                    .find(|pane| pane.rect.x == divider.x + 1)
                    .expect("right pane");
                (left.window_id, right.window_id)
            }
            DividerOrientation::Horizontal => {
                let top = layout
                    .panes
                    .iter()
                    .find(|pane| pane.rect.y + pane.rect.height == divider.y)
                    .expect("top pane");
                let bottom = layout
                    .panes
                    .iter()
                    .find(|pane| pane.rect.y == divider.y + 1)
                    .expect("bottom pane");
                (top.window_id, bottom.window_id)
            }
        }
    }

    #[test]
    fn snapshot_roundtrip_preserves_focus_and_layout() {
        let mut tree = WindowTree::new(1usize);
        tree.split_focused(SplitAxis::Vertical, 2);
        tree.split_focused(SplitAxis::Horizontal, 3);
        tree.focus_item_id(1).expect("focus pane 1");

        let before_order = tree.ordered_item_ids();
        let before_focused = tree.focused_item_id();
        let before_layout = tree.layout(area());
        let snapshot = tree.snapshot();

        let restored = WindowTree::from_snapshot(snapshot).expect("restore from snapshot");

        assert_eq!(restored.ordered_item_ids(), before_order);
        assert_eq!(restored.focused_item_id(), before_focused);
        assert_eq!(restored.layout(area()).panes, before_layout.panes);
        assert_eq!(restored.layout(area()).dividers, before_layout.dividers);
    }

    #[test]
    fn restore_rejects_missing_leaf_item_mapping() {
        let mut tree = WindowTree::new(1usize);
        tree.split_focused(SplitAxis::Vertical, 2);
        let mut snapshot = tree.snapshot();
        let missing_leaf = snapshot.root.first_leaf();
        snapshot.windows.remove(&missing_leaf);

        let err = match WindowTree::from_snapshot(snapshot) {
            Ok(_) => panic!("missing mapping should fail"),
            Err(err) => err,
        };
        assert!(err.contains("missing item"));
    }

    #[test]
    fn resize_window_changes_target_without_focus_change() {
        let mut tree = WindowTree::new(1usize);
        tree.split_focused(SplitAxis::Vertical, 2);

        let focused_before = tree.focused_window_id();
        let before = tree.layout(area());
        let left = before
            .panes
            .iter()
            .find(|pane| pane.item_id == 1)
            .expect("left pane before");

        tree.resize_window(left.window_id, Direction::Left, 10)
            .expect("resize target window");

        let after = tree.layout(area());
        let left_after = after
            .panes
            .iter()
            .find(|pane| pane.item_id == 1)
            .expect("left pane after");

        assert!(left_after.rect.width > left.rect.width);
        assert_eq!(tree.focused_window_id(), focused_before);
    }

    #[test]
    fn resize_window_returns_error_for_missing_window() {
        let mut tree = WindowTree::new(1usize);
        let err = tree
            .resize_window(999, Direction::Left, 5)
            .expect_err("missing window should error");
        assert_eq!(err, "Window not found");
    }

    #[test]
    fn resize_window_zero_amount_is_noop() {
        let mut tree = WindowTree::new(1usize);
        tree.split_focused(SplitAxis::Vertical, 2);
        let before = tree.layout(area());
        let target_window = before
            .panes
            .iter()
            .find(|pane| pane.item_id == 1)
            .expect("target pane")
            .window_id;

        tree.resize_window(target_window, Direction::Left, 0)
            .expect("zero resize succeeds");

        let after = tree.layout(area());
        assert_eq!(after.panes, before.panes);
        assert_eq!(after.dividers, before.dividers);
    }

    #[test]
    fn resize_between_windows_targets_outer_split_for_nested_vertical_layout() {
        let mut tree = WindowTree::new(1usize);
        tree.split_focused(SplitAxis::Vertical, 2);
        tree.focus_direction(Direction::Left, area())
            .expect("focus left window");
        tree.split_focused(SplitAxis::Vertical, 3);

        let before = tree.layout(area());
        let outer_divider = before
            .dividers
            .iter()
            .filter(|divider| divider.orientation == DividerOrientation::Vertical)
            .max_by_key(|divider| divider.x)
            .copied()
            .expect("outer divider");
        let (primary, secondary) = divider_window_pair(&before, outer_divider);
        let primary_before = before
            .panes
            .iter()
            .find(|pane| pane.window_id == primary)
            .expect("primary pane before")
            .rect
            .width;

        tree.resize_between_windows(primary, secondary, DividerOrientation::Vertical, 4)
            .expect("resize outer divider");

        let after = tree.layout(area());
        let outer_after = after
            .dividers
            .iter()
            .filter(|divider| divider.orientation == DividerOrientation::Vertical)
            .max_by_key(|divider| divider.x)
            .copied()
            .expect("outer divider after");
        let primary_after = after
            .panes
            .iter()
            .find(|pane| pane.window_id == primary)
            .expect("primary pane after")
            .rect
            .width;

        assert!(outer_after.x > outer_divider.x);
        assert!(primary_after > primary_before);
    }
}
