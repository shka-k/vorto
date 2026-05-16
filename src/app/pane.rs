//! Pane layout + buffer-pool boundary.
//!
//! ## Design summary
//!
//! - **Buffers** are the document model. They are owned at the
//!   application level — never by a pane. The active pane's buffer is
//!   held directly in `App.buffer` (a long-standing field referenced
//!   in many call sites); every other buffer that is currently visible
//!   in some pane lives in `App.parked_buffers`, keyed by
//!   [`crate::buffer_ref::BufferRef`]. Hidden buffers (not displayed
//!   in any pane) stay in the existing `App.sleeping` map.
//!
//! - **Panes** are display regions. A pane carries nothing more than a
//!   `BufferRef` pointing into the application's buffer pool, so
//!   `<space>b` from any pane can swap that ref to whatever the user
//!   picks. The `(PaneId → BufferRef)` mapping lives on `App` in
//!   `App.pane_refs`; this module just provides the layout tree and
//!   focus / split helpers.
//!
//! - **Tabs** are not implemented yet. The design here keeps tabs
//!   trivial to add later — each `Tab` would own a [`PaneLayout`],
//!   a `pane_refs` map, and an `active_pane`; the buffer pool stays
//!   shared at the `App` level so a buffer can appear in any tab.
//!
//! ## Two-pane sharing
//!
//! v1 does not support the same buffer being displayed in two panes
//! at once. The active buffer lives in `App.buffer` while every parked
//! buffer lives in `App.parked_buffers`; both can't be the same `Buffer`
//! struct simultaneously. `Self::switch_active_pane_buffer` rejects a
//! swap that would alias a buffer already shown by another pane. A
//! future refactor — splitting per-pane viewport state out of `Buffer`
//! — would lift this restriction.

use std::collections::HashMap;

use crate::buffer_ref::BufferRef;
use crate::editor::Buffer;

use super::{App, Toast};

/// Stable identifier for a pane. Minted once when the pane is opened
/// (initial buffer or new split) and stays attached to that on-screen
/// region until the pane is closed.
pub type PaneId = u32;

/// Orientation of a split node.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SplitDir {
    /// Side-by-side children (children laid out left → right).
    Vertical,
    /// Stacked children (children laid out top → bottom).
    Horizontal,
}

// `FocusDir` lives in `crate::action` so the action AST doesn't have
// to reach into `app::pane`. Re-exported here so the existing
// `pane::FocusDir` import path keeps working.
pub use crate::action::FocusDir;

/// Recursive tree describing how the buffer viewport is partitioned
/// into panes.
#[derive(Debug, Clone)]
pub enum PaneLayout {
    /// A single visible pane.
    Leaf(PaneId),
    /// A split with N (>= 2) children sharing the parent rect along
    /// `dir`. `ratios` is the same length as `children` and sums to
    /// approximately 1.0; renderers normalize before consuming.
    Split {
        dir: SplitDir,
        children: Vec<PaneLayout>,
        ratios: Vec<f32>,
    },
}

impl PaneLayout {
    /// Locate the leaf with the given id and return a mutable
    /// reference into the tree at that subtree.
    pub fn find_leaf_mut(&mut self, id: PaneId) -> Option<&mut PaneLayout> {
        match self {
            PaneLayout::Leaf(pid) if *pid == id => Some(self),
            PaneLayout::Leaf(_) => None,
            PaneLayout::Split { children, .. } => {
                for c in children {
                    if let Some(found) = c.find_leaf_mut(id) {
                        return Some(found);
                    }
                }
                None
            }
        }
    }

    /// Collect every leaf id in left-to-right / top-to-bottom traversal
    /// order. Used for `Ctrl-W w` cycle-window and for sanity checks.
    pub fn leaves(&self) -> Vec<PaneId> {
        let mut out = Vec::new();
        self.collect_leaves(&mut out);
        out
    }

    fn collect_leaves(&self, out: &mut Vec<PaneId>) {
        match self {
            PaneLayout::Leaf(id) => out.push(*id),
            PaneLayout::Split { children, .. } => {
                for c in children {
                    c.collect_leaves(out);
                }
            }
        }
    }

    /// Remove the leaf with the given id, collapsing any parent split
    /// that ends up with only one remaining child. Returns the id of a
    /// nearby surviving leaf — the caller uses it as the next "active"
    /// pane — or `None` when the removal would empty the tree (caller
    /// must handle that case before calling).
    pub fn remove_leaf(&mut self, target: PaneId) -> Option<PaneId> {
        enum RemoveResult {
            NotFound,
            RemoveSelf,
            Removed(Option<PaneId>),
        }
        fn rightmost_leaf(node: &PaneLayout) -> PaneId {
            match node {
                PaneLayout::Leaf(id) => *id,
                PaneLayout::Split { children, .. } => {
                    rightmost_leaf(children.last().expect("split has >= 1 child"))
                }
            }
        }
        fn walk(node: &mut PaneLayout, target: PaneId) -> RemoveResult {
            match node {
                PaneLayout::Leaf(id) if *id == target => RemoveResult::RemoveSelf,
                PaneLayout::Leaf(_) => RemoveResult::NotFound,
                PaneLayout::Split {
                    children, ratios, ..
                } => {
                    for i in 0..children.len() {
                        match walk(&mut children[i], target) {
                            RemoveResult::NotFound => continue,
                            RemoveResult::RemoveSelf => {
                                children.remove(i);
                                ratios.remove(i);
                                let sum: f32 = ratios.iter().sum();
                                if sum > 0.0 {
                                    for r in ratios.iter_mut() {
                                        *r /= sum;
                                    }
                                }
                                let neighbor = if children.is_empty() {
                                    None
                                } else {
                                    let pick = if i < children.len() { i } else { i - 1 };
                                    Some(rightmost_leaf(&children[pick]))
                                };
                                return RemoveResult::Removed(neighbor);
                            }
                            RemoveResult::Removed(n) => return RemoveResult::Removed(n),
                        }
                    }
                    RemoveResult::NotFound
                }
            }
        }
        let neighbor = match walk(self, target) {
            RemoveResult::Removed(n) => n,
            _ => return None,
        };
        collapse_singletons(self);
        neighbor
    }

    /// Replace this leaf with a 2-child Split. The existing leaf
    /// becomes one of the children; `new_id` is the new sibling.
    /// `place` chooses which side the existing pane ends up on.
    pub fn split_at(&mut self, dir: SplitDir, new_id: PaneId, place: SplitPlace) {
        let existing = std::mem::replace(self, PaneLayout::Leaf(0));
        let new = PaneLayout::Leaf(new_id);
        let (children, ratios) = match place {
            SplitPlace::After => (vec![existing, new], vec![0.5, 0.5]),
            SplitPlace::Before => (vec![new, existing], vec![0.5, 0.5]),
        };
        *self = PaneLayout::Split {
            dir,
            children,
            ratios,
        };
    }
}

/// Position of the existing pane relative to the new sibling when a
/// leaf is split into two.
#[derive(Debug, Clone, Copy)]
pub enum SplitPlace {
    /// Existing pane stays on the left/top, new pane on the right/bottom.
    After,
    /// Existing pane moves to the right/bottom, new pane on the left/top.
    #[allow(dead_code)]
    Before,
}

/// Fold any `Split` node that ended up with a single child into its
/// child. Runs after a `remove_leaf` so a tree like
/// `Split[Leaf(2)]` doesn't linger as a noop wrapper around `Leaf(2)`.
fn collapse_singletons(node: &mut PaneLayout) {
    loop {
        let collapsed = match node {
            PaneLayout::Leaf(_) => None,
            PaneLayout::Split { children, .. } if children.len() == 1 => Some(children.remove(0)),
            PaneLayout::Split { children, .. } => {
                for c in children.iter_mut() {
                    collapse_singletons(c);
                }
                None
            }
        };
        match collapsed {
            None => break,
            Some(replacement) => {
                *node = replacement;
            }
        }
    }
}

/// Per-frame pane rectangle, published by the UI after layout and read
/// by directional focus navigation. Standalone newtype so this module
/// stays free of any ratatui dependency.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct PaneRect {
    pub x: u16,
    pub y: u16,
    pub width: u16,
    pub height: u16,
}

pub type PaneRectMap = HashMap<PaneId, PaneRect>;

pub const INITIAL_PANE_ID: PaneId = 0;
pub const NEXT_PANE_ID_SEED: PaneId = 1;

// ────────────────────────────────────────────────────────────────────────
// App-side pane operations
// ────────────────────────────────────────────────────────────────────────

impl App {
    /// Open a new pane in direction `dir` alongside the currently-active
    /// pane. The new pane shares the same buffer as the current one
    /// (vim-style `:split`): no clone, no scratch creation, just a new
    /// `Leaf` in the tree pointing at the same `BufferRef`. Buffer
    /// state (cursor, scroll, undo, …) is stored on the `Buffer` itself,
    /// so the two panes are views onto the exact same content; edits in
    /// either pane apply to one underlying buffer.
    ///
    /// Focus moves to the new pane (matching vim's `:split` behaviour).
    /// The active pane stays a leaf with no `pane_refs` entry — the
    /// existing pane gets one pointing at the shared ref so the
    /// `<active_ref ⇒ App.buffer, else parked>` lookup keeps working.
    pub fn split_window(&mut self, dir: SplitDir) {
        let new_pane_id = self.mint_pane_id();
        let active_pane_id = self.active_pane;
        let shared_ref = self.active_ref();
        // The old leaf becomes inactive — record its ref so the
        // renderer / focus code can resolve it. Since this ref equals
        // the (new) active ref, no buffer movement is needed; the
        // `App.buffer` stays put and both panes look it up through the
        // shared-ref path.
        self.pane_refs.insert(active_pane_id, shared_ref);
        // New pane is the ACTIVE one — `pane_refs` only tracks
        // inactive panes, so nothing to insert for `new_pane_id`.
        let leaf = self
            .layout
            .find_leaf_mut(active_pane_id)
            .expect("active pane must be in the layout tree");
        leaf.split_at(dir, new_pane_id, SplitPlace::After);
        self.active_pane = new_pane_id;
        self.push_toast(Toast::info(format!(
            "split ({})",
            match dir {
                SplitDir::Vertical => "vertical",
                SplitDir::Horizontal => "horizontal",
            },
        )));
    }

    /// Look up which buffer (a `&Buffer`) the inactive pane `id` is
    /// currently showing. Returns `None` when `id` isn't an inactive
    /// leaf, or when the layout/`parked_buffers` invariant is violated
    /// (shouldn't happen in practice — exists as a soft failure for
    /// the renderer rather than a panic). Panes whose ref equals the
    /// active ref resolve back to `App.buffer`; otherwise we look in
    /// `parked_buffers`.
    pub fn buffer_for_pane(&self, id: PaneId) -> Option<&Buffer> {
        if id == self.active_pane {
            return Some(&self.buffer);
        }
        let pane_ref = self.pane_refs.get(&id)?;
        if *pane_ref == self.active_ref() {
            return Some(&self.buffer);
        }
        self.parked_buffers.get(pane_ref)
    }

    /// Does any *inactive* pane currently show `r`? Used when deciding
    /// whether the buffer currently held in `App.buffer` (or being
    /// stashed) can move to `sleeping` (gone from every visible pane)
    /// or has to stay live in `parked_buffers`.
    pub fn ref_used_by_inactive_pane(&self, r: &BufferRef) -> bool {
        self.pane_refs.values().any(|v| v == r)
    }

    /// Close the active pane. Three cases, all preserving the
    /// `parked_buffers` invariant (entries exist only for refs that
    /// are shown by some inactive pane AND differ from the active
    /// ref):
    ///
    /// 1. **Closing pane shares its ref with another pane.** Just drop
    ///    the leaf from the tree — no buffer changes, no stashing.
    ///    Focus moves to a neighbour; if that neighbour also shares
    ///    the ref, `App.buffer` stays put.
    /// 2. **Neighbour shares the closing pane's ref.** Same: focus
    ///    moves, `App.buffer` stays. The active ref is unchanged.
    /// 3. **Refs differ.** Stash `App.buffer` to `sleeping`, pull the
    ///    neighbour's buffer out of `parked_buffers`. Standard swap.
    ///
    /// No-op (with a toast) when only one pane is left.
    pub fn close_window(&mut self) {
        if self.pane_count() <= 1 {
            self.push_toast(Toast::error("only one pane (use :q to quit)"));
            return;
        }
        let closing_id = self.active_pane;
        let neighbor = match self.layout.remove_leaf(closing_id) {
            Some(n) => n,
            None => {
                self.push_toast(Toast::error("layout has no neighbour to close into"));
                return;
            }
        };
        let neighbour_ref = self
            .pane_refs
            .remove(&neighbor)
            .expect("neighbour leaf must have a buffer_ref entry");
        let closing_ref = self.active_ref();
        if neighbour_ref == closing_ref {
            // Active and neighbour share the same buffer. The buffer
            // stays in `App.buffer`; nothing to stash or swap.
            self.active_pane = neighbor;
            self.push_toast(Toast::info("pane closed"));
            return;
        }
        // The neighbour points at a different buffer — it was parked,
        // pull it into the active slot. Whether the closing buffer
        // goes to sleeping or parked depends on whether any other
        // inactive pane still references its ref.
        let neighbour_buf = self
            .parked_buffers
            .remove(&neighbour_ref)
            .expect("neighbour buffer must be parked");
        let mut closed_buffer = std::mem::replace(&mut self.buffer, neighbour_buf);
        self.current_scratch_id = match &neighbour_ref {
            BufferRef::Scratch(id) => Some(*id),
            _ => None,
        };
        if self.ref_used_by_inactive_pane(&closing_ref) {
            // Another pane still shows the closing ref — keep the
            // buffer live so that pane can read from it. No sleeping
            // freeze (which would compress and lose the highlighter).
            self.parked_buffers.insert(closing_ref, closed_buffer);
        } else {
            closed_buffer.highlighter = None;
            self.sleeping
                .insert(closing_ref, super::SleepingBuffer::freeze(closed_buffer));
        }
        self.active_pane = neighbor;
        self.lsp.detach_current();
        self.lsp.set_last_synced_version(self.buffer.version);
        // See `focus_pane` for why we skip the highlighter respawn in
        // the common case.
        if let Some(path) = self.buffer.path.clone() {
            if self.buffer.highlighter.is_none() {
                self.spawn_highlighter_worker(&path);
            }
            self.spawn_lsp_worker(&path);
        }
        self.push_toast(Toast::info("pane closed"));
    }

    /// Move focus to the pane lying in the requested cardinal direction.
    /// Resolves against the rectangles computed by the UI on the last
    /// frame. No-op when no pane sits in that direction.
    pub fn focus_window(&mut self, dir: FocusDir) {
        let Some(target) = self.pane_in_direction(dir) else {
            return;
        };
        self.focus_pane(target);
    }

    /// Cycle to the next pane in tree-traversal order. Bound to
    /// `Ctrl-W w`.
    pub fn cycle_window(&mut self) {
        let leaves = self.layout.leaves();
        if leaves.len() <= 1 {
            return;
        }
        let idx = leaves
            .iter()
            .position(|id| *id == self.active_pane)
            .unwrap_or(0);
        let next = leaves[(idx + 1) % leaves.len()];
        self.focus_pane(next);
    }

    /// Number of leaves in the current layout. `1` means "no splits";
    /// the value drives the `:close` guard so we don't try to close the
    /// last visible pane (vim's `:q` is the right tool there).
    pub fn pane_count(&self) -> usize {
        self.layout.leaves().len()
    }

    /// Swap focus to `target`. Two cases:
    ///
    /// 1. **Target shares the active buffer's ref.** Just rotate
    ///    `active_pane`; the underlying buffer stays in `App.buffer`
    ///    and the renderer's shared-ref path keeps painting the
    ///    correct content. Cursor / scroll are buffer-level state,
    ///    so they're naturally shared between the two panes.
    /// 2. **Target points to a different ref.** Pull its buffer out
    ///    of `parked_buffers` into `App.buffer`. The previous active
    ///    buffer goes into `parked_buffers[prev_ref]` (unconditionally
    ///    — it has to live somewhere live, and any other pane that
    ///    shares `prev_ref` will look it up there).
    pub(super) fn focus_pane(&mut self, target: PaneId) {
        if target == self.active_pane {
            return;
        }
        let target_ref = match self.pane_refs.get(&target).cloned() {
            Some(r) => r,
            None => return,
        };
        let prev_id = self.active_pane;
        let prev_ref = self.active_ref();
        if target_ref == prev_ref {
            // Shared ref: nothing to move. Just rotate which leaf is
            // active. pane_refs only tracks inactive panes, so we
            // remove the new active's entry and insert the previous
            // active's entry (pointing at the same shared ref).
            self.pane_refs.remove(&target);
            self.pane_refs.insert(prev_id, prev_ref);
            self.active_pane = target;
            self.record_opened(target_ref);
            return;
        }
        // Target points elsewhere — full buffer swap.
        self.pane_refs.remove(&target);
        let Some(target_buffer) = self.parked_buffers.remove(&target_ref) else {
            // pane_refs and parked_buffers are out of sync — put the
            // ref back so we don't lose track of what the pane shows.
            self.pane_refs.insert(target, target_ref);
            return;
        };
        let prev_buffer = std::mem::replace(&mut self.buffer, target_buffer);
        self.current_scratch_id = match &target_ref {
            BufferRef::Scratch(id) => Some(*id),
            _ => None,
        };
        self.parked_buffers.insert(prev_ref.clone(), prev_buffer);
        self.pane_refs.insert(prev_id, prev_ref);
        self.active_pane = target;
        self.lsp.detach_current();
        self.lsp.set_last_synced_version(self.buffer.version);
        // The parked buffer carries its existing highlighter, so the
        // common-case focus swap keeps syntax painted continuously.
        // Only respawn when the parked copy is missing one (rare —
        // either the open-time worker hadn't completed by the swap, or
        // the buffer's grammar wasn't available at open). Respawning
        // unconditionally would null the highlighter for a few frames
        // (see `spawn_highlighter_worker`) and flicker through plain
        // text.
        if let Some(path) = self.buffer.path.clone() {
            if self.buffer.highlighter.is_none() {
                self.spawn_highlighter_worker(&path);
            }
            self.spawn_lsp_worker(&path);
        }
        self.record_opened(target_ref);
    }

    /// Pick the leaf-pane id that sits in `dir` relative to the active
    /// pane. Resolves against `last_pane_rects`, populated by the UI on
    /// the most recent draw.
    fn pane_in_direction(&self, dir: FocusDir) -> Option<PaneId> {
        let rects = self.last_pane_rects.borrow();
        let active = rects.get(&self.active_pane).copied()?;
        let active_cx = active.x + active.width / 2;
        let active_cy = active.y + active.height / 2;
        let mut best: Option<(PaneId, i32)> = None;
        for (&id, &rect) in rects.iter() {
            if id == self.active_pane {
                continue;
            }
            let matches_dir = match dir {
                FocusDir::Left => rect.x + rect.width <= active.x,
                FocusDir::Right => rect.x >= active.x + active.width,
                FocusDir::Up => rect.y + rect.height <= active.y,
                FocusDir::Down => rect.y >= active.y + active.height,
            };
            if !matches_dir {
                continue;
            }
            let cx = rect.x + rect.width / 2;
            let cy = rect.y + rect.height / 2;
            let dist: i32 = match dir {
                FocusDir::Left | FocusDir::Right => {
                    (cx as i32 - active_cx as i32).abs() * 2 + (cy as i32 - active_cy as i32).abs()
                }
                FocusDir::Up | FocusDir::Down => {
                    (cy as i32 - active_cy as i32).abs() * 2 + (cx as i32 - active_cx as i32).abs()
                }
            };
            match best {
                Some((_, b)) if dist >= b => {}
                _ => best = Some((id, dist)),
            }
        }
        best.map(|(id, _)| id)
    }

    pub(super) fn mint_pane_id(&mut self) -> PaneId {
        let id = self.next_pane_id;
        self.next_pane_id = self.next_pane_id.saturating_add(1);
        id
    }
}
