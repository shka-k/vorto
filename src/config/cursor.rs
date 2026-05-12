//! Cursor-shape configuration. `[cursor]` table in `config.toml`
//! resolved into per-mode [`CursorShape`]s applied by the main loop
//! via DECSCUSR.

use anyhow::{Context, Result, bail};
use serde::Deserialize;

use crate::mode::Mode;

#[derive(Debug, Default, Deserialize)]
pub struct CursorConfig {
    pub normal: Option<String>,
    pub insert: Option<String>,
    pub visual: Option<String>,
    pub visual_line: Option<String>,
    pub visual_block: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CursorShape {
    Block,
    Bar,
    Underbar,
}

#[derive(Debug, Clone, Copy)]
pub struct CursorShapes {
    pub normal: CursorShape,
    pub insert: CursorShape,
    pub visual: CursorShape,
    pub visual_line: CursorShape,
    pub visual_block: CursorShape,
}

impl Default for CursorShapes {
    fn default() -> Self {
        Self {
            normal: CursorShape::Block,
            insert: CursorShape::Bar,
            visual: CursorShape::Underbar,
            visual_line: CursorShape::Underbar,
            visual_block: CursorShape::Underbar,
        }
    }
}

impl CursorShapes {
    pub fn for_mode(&self, mode: Mode) -> CursorShape {
        match mode {
            Mode::Normal => self.normal,
            Mode::Insert => self.insert,
            Mode::Visual => self.visual,
            Mode::VisualLine => self.visual_line,
            Mode::VisualBlock => self.visual_block,
        }
    }
}

fn parse_cursor_shape(s: &str) -> Result<CursorShape> {
    match s.to_lowercase().as_str() {
        "block" => Ok(CursorShape::Block),
        "bar" | "line" => Ok(CursorShape::Bar),
        "underbar" | "underscore" | "underline" => Ok(CursorShape::Underbar),
        other => bail!(
            "unknown cursor shape `{}` (expected block|bar|underbar)",
            other
        ),
    }
}

/// Resolve the `[cursor]` table into concrete `CursorShapes`, falling
/// back to the per-mode defaults for any field the user didn't set.
pub fn resolve_cursor_shapes(c: &CursorConfig) -> Result<CursorShapes> {
    let mut shapes = CursorShapes::default();
    if let Some(s) = &c.normal {
        shapes.normal = parse_cursor_shape(s).with_context(|| "cursor.normal")?;
    }
    if let Some(s) = &c.insert {
        shapes.insert = parse_cursor_shape(s).with_context(|| "cursor.insert")?;
    }
    if let Some(s) = &c.visual {
        shapes.visual = parse_cursor_shape(s).with_context(|| "cursor.visual")?;
    }
    if let Some(s) = &c.visual_line {
        shapes.visual_line = parse_cursor_shape(s).with_context(|| "cursor.visual_line")?;
    }
    if let Some(s) = &c.visual_block {
        shapes.visual_block = parse_cursor_shape(s).with_context(|| "cursor.visual_block")?;
    }
    Ok(shapes)
}
