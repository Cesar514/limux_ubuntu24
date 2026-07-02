use std::cell::{Cell, RefCell};
use std::rc::Rc;

use gtk::glib;
use gtk::prelude::*;
use gtk4 as gtk;

use crate::layout_state::{self, LayoutNodeState, PaneState, SplitOrientation, SplitState};
use crate::pane;
use crate::window::{
    apply_split_ratio_after_layout, attach_split_position_persistence, update_split_ratio_state,
    State,
};

const PANE_RESIZE_RATIO_STEP: f64 = 0.02;
const PANE_RESIZE_MAX_RATIO_DELTA: f64 = 0.40;

// ---------------------------------------------------------------------------
// SplitNode — runtime data model for the split tree
// ---------------------------------------------------------------------------

/// Runtime split tree node. Source of truth for the split layout.
/// The widget tree is rebuilt from this on every structural change.
pub(crate) enum SplitNode {
    Leaf {
        pane_widget: gtk::Widget,
    },
    Split {
        orientation: gtk::Orientation,
        /// Shared with the Paned's position_notify handler so resize drags
        /// update the data model directly.
        ratio: Rc<RefCell<f64>>,
        left: Box<SplitNode>,
        right: Box<SplitNode>,
    },
}

impl SplitNode {
    pub(crate) fn is_leaf(&self) -> bool {
        matches!(self, SplitNode::Leaf { .. })
    }

    /// Find the leaf containing `target` and replace it with `replacement`.
    pub(crate) fn replace(&mut self, target: &gtk::Widget, replacement: SplitNode) -> bool {
        match self {
            SplitNode::Leaf { pane_widget } => {
                if pane_widget == target {
                    *self = replacement;
                    true
                } else {
                    false
                }
            }
            SplitNode::Split { left, right, .. } => {
                // Check containment first to route ownership to the correct subtree
                if left.contains_pane(target) {
                    left.replace(target, replacement)
                } else {
                    right.replace(target, replacement)
                }
            }
        }
    }

    fn contains_pane(&self, target: &gtk::Widget) -> bool {
        match self {
            SplitNode::Leaf { pane_widget } => pane_widget == target,
            SplitNode::Split { left, right, .. } => {
                left.contains_pane(target) || right.contains_pane(target)
            }
        }
    }

    /// Find the leaf containing `target` and promote its sibling in place.
    pub(crate) fn remove(&mut self, target: &gtk::Widget) -> bool {
        match self {
            SplitNode::Leaf { .. } => false,
            SplitNode::Split { left, right, .. } => {
                if matches!(left.as_ref(), SplitNode::Leaf { pane_widget } if pane_widget == target)
                {
                    // Target is left child — promote right sibling.
                    *self = std::mem::replace(
                        right.as_mut(),
                        SplitNode::Leaf {
                            pane_widget: target.clone(),
                        },
                    );
                    return true;
                }
                if matches!(right.as_ref(), SplitNode::Leaf { pane_widget } if pane_widget == target)
                {
                    // Target is right child — promote left sibling.
                    *self = std::mem::replace(
                        left.as_mut(),
                        SplitNode::Leaf {
                            pane_widget: target.clone(),
                        },
                    );
                    return true;
                }
                left.remove(target) || right.remove(target)
            }
        }
    }

    /// Snapshot to the serializable layout format for session persistence.
    pub(crate) fn snapshot(&self, working_directory: Option<&str>) -> LayoutNodeState {
        match self {
            SplitNode::Leaf { pane_widget } => pane::snapshot_pane_state(pane_widget)
                .map(LayoutNodeState::Pane)
                .unwrap_or_else(|| LayoutNodeState::Pane(PaneState::fallback(working_directory))),
            SplitNode::Split {
                orientation,
                ratio,
                left,
                right,
            } => LayoutNodeState::Split(SplitState {
                orientation: if *orientation == gtk::Orientation::Horizontal {
                    SplitOrientation::Horizontal
                } else {
                    SplitOrientation::Vertical
                },
                ratio: *ratio.borrow(),
                start: Box::new(left.snapshot(working_directory)),
                end: Box::new(right.snapshot(working_directory)),
            }),
        }
    }
}

// ---------------------------------------------------------------------------
// SplitTreeContainer — manages async widget-tree rebuild lifecycle
// ---------------------------------------------------------------------------

/// Manages the workspace's split layout following Ghostty's atomic rebuild
/// pattern. Holds a SplitNode data model (source of truth) and a gtk::Box
/// container for the built widget tree. On structural changes, tears down the
/// old widget tree and rebuilds from the data model on the next idle tick.
pub(crate) struct SplitTreeContainer {
    tree: RefCell<SplitNode>,
    bin: gtk::Box,
    rebuild_source: RefCell<Option<glib::SourceId>>,
    last_focused: RefCell<Option<gtk::Widget>>,
    zoomed_pane: RefCell<Option<gtk::Widget>>,
    state: State,
}

impl SplitTreeContainer {
    /// Create a new container with a single pane (no splits).
    pub(crate) fn new(state: &State, initial_pane: gtk::Widget) -> Rc<Self> {
        let bin = gtk::Box::new(gtk::Orientation::Vertical, 0);
        bin.set_hexpand(true);
        bin.set_vexpand(true);
        bin.append(&initial_pane);

        Rc::new(Self {
            tree: RefCell::new(SplitNode::Leaf {
                pane_widget: initial_pane,
            }),
            bin,
            rebuild_source: RefCell::new(None),
            last_focused: RefCell::new(None),
            zoomed_pane: RefCell::new(None),
            state: state.clone(),
        })
    }

    /// Create a container from a pre-built tree (for session restore).
    pub(crate) fn new_from_tree(state: &State, node: SplitNode) -> Rc<Self> {
        let bin = gtk::Box::new(gtk::Orientation::Vertical, 0);
        bin.set_hexpand(true);
        bin.set_vexpand(true);

        // Build the initial widget tree synchronously (no async needed on first build)
        let widget = build_widget_tree(&node, state);
        bin.append(&widget);

        Rc::new(Self {
            tree: RefCell::new(node),
            bin,
            rebuild_source: RefCell::new(None),
            last_focused: RefCell::new(None),
            zoomed_pane: RefCell::new(None),
            state: state.clone(),
        })
    }

    /// The container widget to add to the gtk::Stack.
    pub(crate) fn widget(&self) -> &gtk::Box {
        &self.bin
    }

    /// Borrow the tree for reading (e.g. session snapshot).
    pub(crate) fn tree(&self) -> std::cell::Ref<'_, SplitNode> {
        self.tree.borrow()
    }

    /// Whether the tree is a single leaf (no splits).
    pub(crate) fn is_single_pane(&self) -> bool {
        self.tree.borrow().is_leaf()
    }

    pub(crate) fn toggle_zoom(self: &Rc<Self>, target: &gtk::Widget) -> bool {
        if self.zoomed_pane.borrow().is_some() {
            self.restore_zoom();
            false
        } else {
            self.zoom_pane(target);
            true
        }
    }

    fn zoom_pane(self: &Rc<Self>, target: &gtk::Widget) {
        self.save_focus();
        *self.zoomed_pane.borrow_mut() = Some(target.clone());
        *self.last_focused.borrow_mut() = Some(target.clone());
        self.trigger_rebuild();
    }

    fn restore_zoom(self: &Rc<Self>) {
        self.save_focus();
        self.zoomed_pane.borrow_mut().take();
        self.trigger_rebuild();
    }

    /// Split a pane. Mutates the data model, then triggers async rebuild.
    pub(crate) fn can_split(&self, target: &gtk::Widget, orientation: gtk::Orientation) -> bool {
        pane_has_room_to_split(target, orientation)
    }

    pub(crate) fn split(
        self: &Rc<Self>,
        target: &gtk::Widget,
        new_pane: gtk::Widget,
        orientation: gtk::Orientation,
        new_pane_first: bool,
        ratio: f64,
    ) -> bool {
        if !pane_has_room_to_split(target, orientation) {
            return false;
        }

        self.save_focus();
        self.zoomed_pane.borrow_mut().take();
        *self.last_focused.borrow_mut() = Some(new_pane.clone());

        let shared_ratio = Rc::new(RefCell::new(layout_state::clamp_split_ratio(ratio)));
        let new_node = if new_pane_first {
            SplitNode::Split {
                orientation,
                ratio: shared_ratio,
                left: Box::new(SplitNode::Leaf {
                    pane_widget: new_pane,
                }),
                right: Box::new(SplitNode::Leaf {
                    pane_widget: target.clone(),
                }),
            }
        } else {
            SplitNode::Split {
                orientation,
                ratio: shared_ratio,
                left: Box::new(SplitNode::Leaf {
                    pane_widget: target.clone(),
                }),
                right: Box::new(SplitNode::Leaf {
                    pane_widget: new_pane,
                }),
            }
        };

        let replaced = {
            let mut tree = self.tree.borrow_mut();
            tree.replace(target, new_node)
        };

        if replaced {
            self.trigger_rebuild();
        }
        replaced
    }

    /// Remove a pane. Mutates the data model, then triggers async rebuild.
    pub(crate) fn remove(self: &Rc<Self>, target: &gtk::Widget) -> bool {
        self.save_focus();
        self.zoomed_pane.borrow_mut().take();

        let removed = {
            let mut tree = self.tree.borrow_mut();
            tree.remove(target)
        };

        if removed {
            self.trigger_rebuild();
        }
        removed
    }

    /// Resize the nearest matching split around a pane.
    pub(crate) fn resize_pane(
        self: &Rc<Self>,
        target: &gtk::Widget,
        direction: &str,
        amount: u64,
    ) -> Option<f64> {
        let direction = PaneResizeDirection::from_str(direction)?;
        let delta = resize_ratio_delta(amount);
        let ratio = {
            let mut tree = self.tree.borrow_mut();
            resize_node_for_pane(&mut tree, target, direction, delta)
        }?;
        self.save_focus();
        *self.last_focused.borrow_mut() = Some(target.clone());
        self.trigger_rebuild();
        Some(ratio)
    }

    /// Tear down the old widget tree and schedule a rebuild on the next idle
    /// tick. The one-tick separation between unrealize (teardown) and realize
    /// (rebuild) is what prevents GLArea breakage.
    fn trigger_rebuild(self: &Rc<Self>) {
        // Cancel any pending rebuild
        if let Some(source) = self.rebuild_source.take() {
            source.remove();
        }

        // Clear the bin — tears down the old widget tree.
        // unrealize cascades to all GLAreas in the subtree.
        while let Some(child) = self.bin.first_child() {
            self.bin.remove(&child);
        }

        // Rebuild on the next idle tick. The tick separation between
        // unrealize (above) and realize (rebuild) is critical.
        self.schedule_rebuild();
    }

    /// Schedule the actual rebuild on the next idle tick.
    fn schedule_rebuild(self: &Rc<Self>) {
        if self.rebuild_source.borrow().is_some() {
            return;
        }
        let container = Rc::clone(self);
        let source = glib::idle_add_local_once(move || {
            container.rebuild_source.replace(None);
            container.do_rebuild();
        });
        self.rebuild_source.replace(Some(source));
    }

    /// Build new widget tree from data model, attach atomically.
    fn do_rebuild(self: &Rc<Self>) {
        // Pane widgets may still be parented to old (floating) Paneds from
        // the previous tree. GTK4 won't let us add them to new containers
        // until they're unparented. Detach them all first.
        let tree = self.tree.borrow();
        let zoomed = self.zoomed_pane.borrow().clone();
        if let Some(pane) = zoomed {
            if pane.parent().is_some() {
                detach_pane_from_old_parent(&pane);
                self.schedule_rebuild();
                return;
            }
            self.bin.append(&pane);
        } else {
            if tree_has_pane_parents(&tree) {
                detach_panes_from_old_tree(&tree);
                self.schedule_rebuild();
                return;
            }
            let widget = build_widget_tree(&tree, &self.state);
            self.bin.append(&widget);
        }
        refresh_terminal_displays_after_rebuild(self.bin.upcast_ref());

        // Newly created panes are tracked as pane containers rather than the
        // inner terminal/browser widget, so restore through the pane helper
        // when possible and fall back to plain widget focus otherwise.
        if let Some(focused) = self.last_focused.borrow().as_ref() {
            if !pane::focus_active_tab_in_pane(focused) {
                focused.grab_focus();
            }
        }
    }

    fn save_focus(&self) {
        let focus = self
            .bin
            .root()
            .and_then(|r| r.downcast::<gtk::Window>().ok())
            .and_then(|w| gtk::prelude::GtkWindowExt::focus(&w));
        *self.last_focused.borrow_mut() = focus;
    }
}

impl Drop for SplitTreeContainer {
    fn drop(&mut self) {
        if let Some(source) = self.rebuild_source.take() {
            source.remove();
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum PaneResizeDirection {
    Left,
    Right,
    Up,
    Down,
}

impl PaneResizeDirection {
    fn from_str(raw: &str) -> Option<Self> {
        match raw {
            "left" => Some(Self::Left),
            "right" => Some(Self::Right),
            "up" => Some(Self::Up),
            "down" => Some(Self::Down),
            _ => None,
        }
    }

    fn orientation(self) -> gtk::Orientation {
        match self {
            Self::Left | Self::Right => gtk::Orientation::Horizontal,
            Self::Up | Self::Down => gtk::Orientation::Vertical,
        }
    }
}

// purpose: Convert tmux resize amount units into a bounded split-ratio delta.
// inputs: Resize amount supplied by the CLI or bridge.
// returns/effects: Returns a non-zero ratio delta clamped to avoid pane collapse.
fn resize_ratio_delta(amount: u64) -> f64 {
    ((amount.max(1) as f64) * PANE_RESIZE_RATIO_STEP).min(PANE_RESIZE_MAX_RATIO_DELTA)
}

// purpose: Adjust a split ratio when a pane border is moved in a direction.
// inputs: Current ratio, whether the pane is in the start child, movement direction, and delta.
// returns/effects: Returns the clamped ratio after applying the movement.
fn resized_ratio(
    ratio: f64,
    pane_in_start_child: bool,
    direction: PaneResizeDirection,
    delta: f64,
) -> f64 {
    let grows_start = match direction {
        PaneResizeDirection::Right | PaneResizeDirection::Down => pane_in_start_child,
        PaneResizeDirection::Left | PaneResizeDirection::Up => !pane_in_start_child,
    };
    let next = if grows_start {
        ratio + delta
    } else {
        ratio - delta
    };
    layout_state::clamp_split_ratio(next)
}

// purpose: Resize the deepest split with a matching orientation around a pane.
// inputs: Split tree node, target pane widget, resize direction, and ratio delta.
// returns/effects: Mutates the selected split ratio and returns its new value.
fn resize_node_for_pane(
    node: &mut SplitNode,
    target: &gtk::Widget,
    direction: PaneResizeDirection,
    delta: f64,
) -> Option<f64> {
    let SplitNode::Split {
        orientation,
        ratio,
        left,
        right,
    } = node
    else {
        return None;
    };

    if let Some(updated) = resize_node_for_pane(left, target, direction, delta) {
        return Some(updated);
    }
    if let Some(updated) = resize_node_for_pane(right, target, direction, delta) {
        return Some(updated);
    }
    if *orientation != direction.orientation() {
        return None;
    }

    let pane_in_start_child = left.contains_pane(target);
    if !pane_in_start_child && !right.contains_pane(target) {
        return None;
    }
    let updated = resized_ratio(*ratio.borrow(), pane_in_start_child, direction, delta);
    *ratio.borrow_mut() = updated;
    Some(updated)
}

// ---------------------------------------------------------------------------
// Widget tree helpers
// ---------------------------------------------------------------------------

/// Detach pane widgets from their old parents (floating Paneds left over
/// from the previous widget tree). GTK4 requires a widget to have no parent
/// before it can be added to a new container.
fn detach_panes_from_old_tree(node: &SplitNode) {
    match node {
        SplitNode::Leaf { pane_widget } => {
            if let Some(parent) = pane_widget.parent() {
                if let Some(paned) = parent.downcast_ref::<gtk::Paned>() {
                    // Detach from the old Paned by clearing whichever slot holds us
                    if paned
                        .start_child()
                        .map(|c| c == *pane_widget)
                        .unwrap_or(false)
                    {
                        paned.set_start_child(gtk::Widget::NONE);
                    } else {
                        paned.set_end_child(gtk::Widget::NONE);
                    }
                }
            }
        }
        SplitNode::Split { left, right, .. } => {
            detach_panes_from_old_tree(left);
            detach_panes_from_old_tree(right);
        }
    }
}

fn tree_has_pane_parents(node: &SplitNode) -> bool {
    match node {
        SplitNode::Leaf { pane_widget } => pane_widget.parent().is_some(),
        SplitNode::Split { left, right, .. } => {
            tree_has_pane_parents(left) || tree_has_pane_parents(right)
        }
    }
}

fn detach_pane_from_old_parent(pane_widget: &gtk::Widget) {
    if let Some(parent) = pane_widget.parent() {
        if let Some(paned) = parent.downcast_ref::<gtk::Paned>() {
            if paned
                .start_child()
                .map(|child| child == *pane_widget)
                .unwrap_or(false)
            {
                paned.set_start_child(gtk::Widget::NONE);
            } else {
                paned.set_end_child(gtk::Widget::NONE);
            }
        } else if let Some(container) = parent.downcast_ref::<gtk::Box>() {
            container.remove(pane_widget);
        }
    }
}

/// Build a GTK widget tree from the SplitNode data model.
fn build_widget_tree(node: &SplitNode, state: &State) -> gtk::Widget {
    match node {
        SplitNode::Leaf { pane_widget } => pane_widget.clone(),
        SplitNode::Split {
            orientation,
            ratio,
            left,
            right,
        } => {
            let paned = gtk::Paned::builder()
                .orientation(*orientation)
                .hexpand(true)
                .vexpand(true)
                .build();
            paned.set_shrink_start_child(false);
            paned.set_shrink_end_child(false);
            paned.set_resize_start_child(true);
            paned.set_resize_end_child(true);

            let ratio_val = *ratio.borrow();
            update_split_ratio_state(&paned, ratio_val);
            attach_split_position_persistence(state, &paned);

            // Flag to suppress position_notify during programmatic set_position calls
            // (initial layout and workspace re-map). Without this, set_position triggers
            // position_notify which recalculates the ratio from the not-yet-stable pixel
            // position, corrupting the stored ratio.
            let applying = Rc::new(Cell::new(false));

            // Wire resize drags back to the shared ratio cell in the data model.
            let shared_ratio = ratio.clone();
            let applying_for_notify = applying.clone();
            paned.connect_position_notify(move |paned| {
                if applying_for_notify.get() {
                    return;
                }
                let allocation = paned.allocation();
                let size = if paned.orientation() == gtk::Orientation::Horizontal {
                    allocation.width()
                } else {
                    allocation.height()
                };
                let new_ratio = layout_state::snapshot_split_ratio(
                    paned.position(),
                    size,
                    Some(*shared_ratio.borrow()),
                );
                *shared_ratio.borrow_mut() = layout_state::clamp_split_ratio(new_ratio);
            });

            let left_widget = build_widget_tree(left, state);
            let right_widget = build_widget_tree(right, state);
            paned.set_start_child(Some(&left_widget));
            paned.set_end_child(Some(&right_widget));

            apply_split_ratio_after_layout(&paned, *orientation, ratio.clone(), applying);

            paned.upcast()
        }
    }
}

fn pane_has_room_to_split(target: &gtk::Widget, orientation: gtk::Orientation) -> bool {
    let allocation = target.allocation();
    let size = if orientation == gtk::Orientation::Horizontal {
        allocation.width()
    } else {
        allocation.height()
    };
    size <= 0 || split_extent_has_room(size, orientation)
}

fn minimum_split_extent(orientation: gtk::Orientation) -> i32 {
    if orientation == gtk::Orientation::Horizontal {
        pane::MIN_PANE_WIDTH
    } else {
        pane::MIN_PANE_HEIGHT
    }
}

fn split_extent_has_room(size: i32, orientation: gtk::Orientation) -> bool {
    size >= minimum_split_extent(orientation) * 2
}

fn refresh_terminal_displays_after_rebuild(root: &gtk::Widget) {
    pane::refresh_terminal_displays_in_root(root);

    let idle_root = root.clone();
    glib::idle_add_local_once(move || {
        pane::refresh_terminal_displays_in_root(&idle_root);
    });

    let first_frame_root = root.clone();
    glib::timeout_add_local_once(std::time::Duration::from_millis(16), move || {
        pane::refresh_terminal_displays_in_root(&first_frame_root);
    });

    let settled_root = root.clone();
    glib::timeout_add_local_once(std::time::Duration::from_millis(80), move || {
        pane::refresh_terminal_displays_in_root(&settled_root);
    });
}

// ---------------------------------------------------------------------------
// Conversion from serialized LayoutNodeState to runtime SplitNode
// ---------------------------------------------------------------------------

/// Build a SplitNode tree from a persisted LayoutNodeState.
pub(crate) fn build_split_node_from_layout(
    state: &State,
    shortcuts: &Rc<crate::shortcut_config::ResolvedShortcutConfig>,
    ws_id: &str,
    working_directory: Option<&str>,
    layout: &LayoutNodeState,
) -> SplitNode {
    match layout {
        LayoutNodeState::Pane(pane_state) => {
            let pane = crate::window::create_pane_for_workspace(
                state,
                shortcuts,
                ws_id,
                working_directory,
                Some(pane_state),
                false,
            );
            SplitNode::Leaf {
                pane_widget: pane.upcast(),
            }
        }
        LayoutNodeState::Split(split_state) => {
            let orientation = match split_state.orientation {
                SplitOrientation::Horizontal => gtk::Orientation::Horizontal,
                SplitOrientation::Vertical => gtk::Orientation::Vertical,
            };
            SplitNode::Split {
                orientation,
                ratio: Rc::new(RefCell::new(layout_state::clamp_split_ratio(
                    split_state.ratio,
                ))),
                left: Box::new(build_split_node_from_layout(
                    state,
                    shortcuts,
                    ws_id,
                    working_directory,
                    &split_state.start,
                )),
                right: Box::new(build_split_node_from_layout(
                    state,
                    shortcuts,
                    ws_id,
                    working_directory,
                    &split_state.end,
                )),
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn split_extent_requires_room_for_both_children() {
        assert!(!split_extent_has_room(
            pane::MIN_PANE_WIDTH * 2 - 1,
            gtk::Orientation::Horizontal
        ));
        assert!(split_extent_has_room(
            pane::MIN_PANE_WIDTH * 2,
            gtk::Orientation::Horizontal
        ));
        assert!(!split_extent_has_room(
            pane::MIN_PANE_HEIGHT * 2 - 1,
            gtk::Orientation::Vertical
        ));
        assert!(split_extent_has_room(
            pane::MIN_PANE_HEIGHT * 2,
            gtk::Orientation::Vertical
        ));
    }

    #[test]
    fn resized_ratio_moves_border_relative_to_target_child() {
        assert!(resized_ratio(0.5, true, PaneResizeDirection::Right, 0.1) > 0.5);
        assert!(resized_ratio(0.5, false, PaneResizeDirection::Right, 0.1) < 0.5);
        assert!(resized_ratio(0.5, true, PaneResizeDirection::Left, 0.1) < 0.5);
        assert!(resized_ratio(0.5, false, PaneResizeDirection::Left, 0.1) > 0.5);
        assert!(resized_ratio(0.5, true, PaneResizeDirection::Down, 0.1) > 0.5);
        assert!(resized_ratio(0.5, false, PaneResizeDirection::Up, 0.1) > 0.5);
    }

    #[test]
    fn resize_ratio_delta_is_bounded_and_nonzero() {
        assert_eq!(resize_ratio_delta(0), PANE_RESIZE_RATIO_STEP);
        assert_eq!(resize_ratio_delta(1), PANE_RESIZE_RATIO_STEP);
        assert_eq!(resize_ratio_delta(100), PANE_RESIZE_MAX_RATIO_DELTA);
    }
}
