//! Undo / redo stack management.

use super::{Buffer, Snapshot};

const MAX_UNDO_DEPTH: usize = 200;

impl Buffer {
    /// Save the current buffer state to the undo stack and clear redo.
    /// Callers should invoke this immediately *before* a mutation so the
    /// stored state represents "what to come back to" on undo.
    pub fn snapshot(&mut self) {
        self.undo_stack.push(Snapshot {
            lines: self.lines.clone(),
            cursor: self.cursor,
            extra_cursors: self.extra_cursors.clone(),
            dirty: self.dirty,
        });
        self.redo_stack.clear();
        if self.undo_stack.len() > MAX_UNDO_DEPTH {
            self.undo_stack.remove(0);
        }
    }

    /// Step back one snapshot. Returns false when the undo stack is empty.
    pub fn undo(&mut self) -> bool {
        let Some(prev) = self.undo_stack.pop() else {
            return false;
        };
        self.redo_stack.push(Snapshot {
            lines: std::mem::replace(&mut self.lines, prev.lines),
            cursor: std::mem::replace(&mut self.cursor, prev.cursor),
            extra_cursors: std::mem::replace(&mut self.extra_cursors, prev.extra_cursors),
            dirty: std::mem::replace(&mut self.dirty, prev.dirty),
        });
        self.version = self.version.wrapping_add(1);
        true
    }

    /// Reapply the most recently undone snapshot.
    pub fn redo(&mut self) -> bool {
        let Some(next) = self.redo_stack.pop() else {
            return false;
        };
        self.undo_stack.push(Snapshot {
            lines: std::mem::replace(&mut self.lines, next.lines),
            cursor: std::mem::replace(&mut self.cursor, next.cursor),
            extra_cursors: std::mem::replace(&mut self.extra_cursors, next.extra_cursors),
            dirty: std::mem::replace(&mut self.dirty, next.dirty),
        });
        self.version = self.version.wrapping_add(1);
        true
    }
}
