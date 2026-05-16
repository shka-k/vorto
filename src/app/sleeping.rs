//! Compressed snapshots of inactive buffers.
//!
//! Each entry in [`super::App::sleeping`] holds the full state of a
//! buffer the user has switched away from (lines, cursor, undo/redo
//! history, dirty flag, …) so a `<space>b` round-trip preserves
//! unsaved edits. To keep memory bounded — undo history alone can
//! easily duplicate the source 200x — we deflate the line content
//! lazily: if the *total* raw byte count for a buffer exceeds
//! [`COMPRESS_THRESHOLD`], we compress every line vector inside it.
//! Otherwise we leave the content raw to avoid the per-allocation
//! overhead on tiny scratch files.
//!
//! `dirty`, the cursor, and the path stay uncompressed regardless
//! so the picker and `:q`-time dirty check can read them without
//! paying for a thaw.

use std::io::{Read, Write};
use std::path::PathBuf;

use flate2::Compression;
use flate2::read::DeflateDecoder;
use flate2::write::DeflateEncoder;

use crate::editor::{Buffer, Cursor, Snapshot};

/// Minimum raw byte count (main + all undo + all redo) for a
/// sleeping buffer's line content to be compressed. Below this we
/// keep things raw — deflate carries ~15 bytes of framing overhead
/// and 200 tiny snapshots can still sum to a meaningful number, so
/// the threshold is on the *buffer total*, not per-snapshot.
const COMPRESS_THRESHOLD: usize = 4 * 1024;

/// Lines payload, either kept raw or stored as a deflate blob with
/// `\n` separators. The two variants are interchangeable through
/// [`Lines::thaw`] / [`Lines::freeze_raw`] / [`Lines::freeze_zip`].
#[derive(Debug)]
pub(super) enum Lines {
    Raw(Vec<String>),
    Compressed(Vec<u8>),
}

impl Lines {
    fn freeze_raw(lines: Vec<String>) -> Self {
        Lines::Raw(lines)
    }

    fn freeze_zip(lines: Vec<String>) -> Self {
        let joined = lines.join("\n");
        let mut enc = DeflateEncoder::new(Vec::new(), Compression::default());
        enc.write_all(joined.as_bytes())
            .expect("deflate write to Vec cannot fail");
        let blob = enc.finish().expect("deflate finish to Vec cannot fail");
        Lines::Compressed(blob)
    }

    fn thaw(self) -> Vec<String> {
        match self {
            Lines::Raw(v) => v,
            Lines::Compressed(blob) => {
                let mut dec = DeflateDecoder::new(blob.as_slice());
                let mut s = String::new();
                dec.read_to_string(&mut s)
                    .expect("deflate decode of a self-produced blob cannot fail");
                s.split('\n').map(|s| s.to_string()).collect()
            }
        }
    }

    /// Sum of the raw byte sizes of all lines (excluding `\n`
    /// separators). Used by the threshold check; we approximate
    /// separator bytes by adding `len - 1` once in the caller.
    fn raw_size(lines: &[String]) -> usize {
        lines.iter().map(|l| l.len()).sum()
    }
}

#[derive(Debug)]
pub(super) struct FrozenSnapshot {
    lines: Lines,
    cursor: Cursor,
    extra_cursors: Vec<Cursor>,
    dirty: bool,
}

/// All the state we hold for a sleeping buffer. Built by
/// [`SleepingBuffer::freeze`] and consumed by
/// [`SleepingBuffer::thaw`].
#[derive(Debug)]
pub struct SleepingBuffer {
    lines: Lines,
    cursor: Cursor,
    extra_cursors: Vec<Cursor>,
    path: Option<PathBuf>,
    pub dirty: bool,
    yank: String,
    version: u64,
    scroll: usize,
    col_scroll: usize,
    undo: Vec<FrozenSnapshot>,
    redo: Vec<FrozenSnapshot>,
    /// Preserved across freeze/thaw so the external-edit guard on
    /// `:w` still rejects a clobber when the user comes back to a
    /// buffer that was stashed while another tool rewrote the file.
    disk_meta: Option<crate::editor::FileMeta>,
}

impl SleepingBuffer {
    pub fn freeze(b: Buffer) -> Self {
        // Decide once, for the whole buffer: if the total raw byte
        // count is above the threshold, compress everything;
        // otherwise keep everything raw. This catches the "lots of
        // sub-threshold undo entries that collectively add up"
        // pattern that a per-snapshot threshold would miss.
        let main_size = Lines::raw_size(&b.lines);
        let undo_size: usize = b.undo_stack.iter().map(|s| Lines::raw_size(&s.lines)).sum();
        let redo_size: usize = b.redo_stack.iter().map(|s| Lines::raw_size(&s.lines)).sum();
        let compress = main_size + undo_size + redo_size > COMPRESS_THRESHOLD;
        let freeze = |lines: Vec<String>| -> Lines {
            if compress {
                Lines::freeze_zip(lines)
            } else {
                Lines::freeze_raw(lines)
            }
        };

        let undo: Vec<FrozenSnapshot> = b
            .undo_stack
            .into_iter()
            .map(|s| FrozenSnapshot {
                lines: freeze(s.lines),
                cursor: s.cursor,
                extra_cursors: s.extra_cursors,
                dirty: s.dirty,
            })
            .collect();
        let redo: Vec<FrozenSnapshot> = b
            .redo_stack
            .into_iter()
            .map(|s| FrozenSnapshot {
                lines: freeze(s.lines),
                cursor: s.cursor,
                extra_cursors: s.extra_cursors,
                dirty: s.dirty,
            })
            .collect();

        SleepingBuffer {
            lines: freeze(b.lines),
            cursor: b.cursor,
            extra_cursors: b.extra_cursors,
            path: b.path,
            dirty: b.dirty,
            yank: b.yank,
            version: b.version,
            scroll: b.scroll.get(),
            col_scroll: b.col_scroll.get(),
            undo,
            redo,
            disk_meta: b.disk_meta,
        }
    }

    pub fn thaw(self) -> Buffer {
        let mut b = Buffer::new();
        b.lines = self.lines.thaw();
        b.cursor = self.cursor;
        b.extra_cursors = self.extra_cursors;
        b.path = self.path;
        b.dirty = self.dirty;
        b.yank = self.yank;
        b.version = self.version;
        b.scroll.set(self.scroll);
        b.col_scroll.set(self.col_scroll);
        b.disk_meta = self.disk_meta;
        b.undo_stack = self
            .undo
            .into_iter()
            .map(|s| Snapshot {
                lines: s.lines.thaw(),
                cursor: s.cursor,
                extra_cursors: s.extra_cursors,
                dirty: s.dirty,
            })
            .collect();
        b.redo_stack = self
            .redo
            .into_iter()
            .map(|s| Snapshot {
                lines: s.lines.thaw(),
                cursor: s.cursor,
                extra_cursors: s.extra_cursors,
                dirty: s.dirty,
            })
            .collect();
        // Highlighter and viewport_height are intentionally left as
        // their defaults — the highlighter is rebuilt by a worker on
        // restore, and the UI re-publishes viewport_height on the
        // next draw. The VCS base, on the other hand, is re-fetched
        // here so an external commit while the buffer was sleeping
        // shows up in the gutter as soon as the user comes back.
        b.refresh_vcs_base();
        b
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn buf_from(lines: &[&str]) -> Buffer {
        let mut b = Buffer::new();
        b.lines = lines.iter().map(|s| s.to_string()).collect();
        b
    }

    #[test]
    fn freeze_thaw_roundtrip_preserves_lines() {
        let mut b = buf_from(&["hello", "world", "foo bar baz"]);
        b.cursor = Cursor { row: 1, col: 3 };
        b.dirty = true;
        b.yank = "yanked".to_string();
        b.version = 42;

        let frozen = SleepingBuffer::freeze(b);
        let thawed = frozen.thaw();
        assert_eq!(thawed.lines, vec!["hello", "world", "foo bar baz"]);
        assert_eq!(thawed.cursor, Cursor { row: 1, col: 3 });
        assert!(thawed.dirty);
        assert_eq!(thawed.yank, "yanked");
        assert_eq!(thawed.version, 42);
    }

    #[test]
    fn tiny_buffer_stays_uncompressed() {
        // Below the 4KB threshold — should be Raw, not Compressed.
        let b = buf_from(&["x"]);
        let frozen = SleepingBuffer::freeze(b);
        assert!(matches!(frozen.lines, Lines::Raw(_)));
    }

    #[test]
    fn over_threshold_compresses_everything_even_tiny_snapshots() {
        // A buffer whose *main* lines fit below the threshold but
        // whose undo stack adds up past it — every line vector must
        // end up Compressed, including the small main one.
        let mut b = buf_from(&["short main"]);
        // Push 200 small snapshots (200 × ~50B = 10KB > 4KB).
        for i in 0..200 {
            b.undo_stack.push(Snapshot {
                lines: vec![format!("snapshot {i:03} of fifty-something bytes per row")],
                cursor: Cursor::default(),
                extra_cursors: Vec::new(),
                dirty: false,
            });
        }
        let frozen = SleepingBuffer::freeze(b);
        assert!(matches!(frozen.lines, Lines::Compressed(_)));
        for s in &frozen.undo {
            assert!(matches!(s.lines, Lines::Compressed(_)));
        }
    }

    #[test]
    fn dirty_flag_visible_without_thaw() {
        let mut b = buf_from(&["x"]);
        b.dirty = true;
        let frozen = SleepingBuffer::freeze(b);
        // Cheap to read; no decompression needed even when compressed.
        assert!(frozen.dirty);
    }
}
