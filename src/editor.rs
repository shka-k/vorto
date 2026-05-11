use std::fs;
use std::path::{Path, PathBuf};

use anyhow::Result;

#[derive(Debug, Default)]
pub struct Buffer {
    pub lines: Vec<String>,
    pub cursor: Cursor,
    pub path: Option<PathBuf>,
    pub dirty: bool,
    pub yank: String,
}

#[derive(Debug, Default, Clone, Copy)]
pub struct Cursor {
    pub row: usize,
    pub col: usize,
}

impl Buffer {
    pub fn new() -> Self {
        Self {
            lines: vec![String::new()],
            ..Default::default()
        }
    }

    pub fn load(path: &Path) -> Result<Self> {
        let text = if path.exists() {
            fs::read_to_string(path)?
        } else {
            String::new()
        };
        let mut lines: Vec<String> = text.split('\n').map(|s| s.to_string()).collect();
        if lines.is_empty() {
            lines.push(String::new());
        }
        Ok(Self {
            lines,
            cursor: Cursor::default(),
            path: Some(path.to_path_buf()),
            dirty: false,
            yank: String::new(),
        })
    }

    pub fn save(&mut self) -> Result<()> {
        if let Some(path) = &self.path {
            fs::write(path, self.lines.join("\n"))?;
            self.dirty = false;
        }
        Ok(())
    }

    pub fn save_as(&mut self, path: &Path) -> Result<()> {
        self.path = Some(path.to_path_buf());
        self.save()
    }

    pub fn current_line(&self) -> &str {
        &self.lines[self.cursor.row]
    }

    pub fn current_line_len(&self) -> usize {
        self.current_line().chars().count()
    }

    pub fn clamp_col(&mut self, allow_after_end: bool) {
        let max = self.current_line_len();
        let limit = if allow_after_end || max == 0 {
            max
        } else {
            max - 1
        };
        if self.cursor.col > limit {
            self.cursor.col = limit;
        }
    }

    pub fn move_left(&mut self) {
        if self.cursor.col > 0 {
            self.cursor.col -= 1;
        }
    }

    pub fn move_right(&mut self, allow_after_end: bool) {
        let max = self.current_line_len();
        let limit = if allow_after_end || max == 0 {
            max
        } else {
            max.saturating_sub(1)
        };
        if self.cursor.col < limit {
            self.cursor.col += 1;
        }
    }

    pub fn move_up(&mut self) {
        if self.cursor.row > 0 {
            self.cursor.row -= 1;
            self.clamp_col(false);
        }
    }

    pub fn move_down(&mut self) {
        if self.cursor.row + 1 < self.lines.len() {
            self.cursor.row += 1;
            self.clamp_col(false);
        }
    }

    pub fn move_line_start(&mut self) {
        self.cursor.col = 0;
    }

    pub fn move_line_end(&mut self) {
        let max = self.current_line_len();
        self.cursor.col = max.saturating_sub(1).max(0);
    }

    pub fn move_file_start(&mut self) {
        self.cursor.row = 0;
        self.cursor.col = 0;
    }

    pub fn move_file_end(&mut self) {
        self.cursor.row = self.lines.len().saturating_sub(1);
        self.clamp_col(false);
    }

    pub fn move_word_forward(&mut self) {
        let line = self.current_line().to_string();
        let chars: Vec<char> = line.chars().collect();
        let mut i = self.cursor.col;
        while i < chars.len() && !chars[i].is_whitespace() {
            i += 1;
        }
        while i < chars.len() && chars[i].is_whitespace() {
            i += 1;
        }
        if i >= chars.len() && self.cursor.row + 1 < self.lines.len() {
            self.cursor.row += 1;
            self.cursor.col = 0;
        } else {
            self.cursor.col = i.min(chars.len().saturating_sub(1));
        }
    }

    pub fn move_word_backward(&mut self) {
        let line = self.current_line().to_string();
        let chars: Vec<char> = line.chars().collect();
        if self.cursor.col == 0 {
            if self.cursor.row > 0 {
                self.cursor.row -= 1;
                self.cursor.col = self.current_line_len().saturating_sub(1);
            }
            return;
        }
        let mut i = self.cursor.col;
        i = i.saturating_sub(1);
        while i > 0 && chars[i].is_whitespace() {
            i -= 1;
        }
        while i > 0 && !chars[i - 1].is_whitespace() {
            i -= 1;
        }
        self.cursor.col = i;
    }

    pub fn insert_char(&mut self, c: char) {
        let line = &mut self.lines[self.cursor.row];
        let byte_idx = char_to_byte(line, self.cursor.col);
        line.insert(byte_idx, c);
        self.cursor.col += 1;
        self.dirty = true;
    }

    pub fn insert_newline(&mut self) {
        let line = self.lines[self.cursor.row].clone();
        let byte_idx = char_to_byte(&line, self.cursor.col);
        let (left, right) = line.split_at(byte_idx);
        self.lines[self.cursor.row] = left.to_string();
        self.lines.insert(self.cursor.row + 1, right.to_string());
        self.cursor.row += 1;
        self.cursor.col = 0;
        self.dirty = true;
    }

    pub fn insert_line_below(&mut self) {
        self.lines.insert(self.cursor.row + 1, String::new());
        self.cursor.row += 1;
        self.cursor.col = 0;
        self.dirty = true;
    }

    pub fn insert_line_above(&mut self) {
        self.lines.insert(self.cursor.row, String::new());
        self.cursor.col = 0;
        self.dirty = true;
    }

    pub fn delete_char_under_cursor(&mut self) {
        let line = &mut self.lines[self.cursor.row];
        if self.cursor.col < line.chars().count() {
            let byte_idx = char_to_byte(line, self.cursor.col);
            let ch = line[byte_idx..].chars().next().unwrap();
            line.replace_range(byte_idx..byte_idx + ch.len_utf8(), "");
            self.dirty = true;
            self.clamp_col(false);
        }
    }

    pub fn delete_char_before(&mut self) {
        if self.cursor.col > 0 {
            let line = &mut self.lines[self.cursor.row];
            let byte_idx = char_to_byte(line, self.cursor.col - 1);
            let ch = line[byte_idx..].chars().next().unwrap();
            line.replace_range(byte_idx..byte_idx + ch.len_utf8(), "");
            self.cursor.col -= 1;
            self.dirty = true;
        } else if self.cursor.row > 0 {
            let line = self.lines.remove(self.cursor.row);
            self.cursor.row -= 1;
            self.cursor.col = self.current_line_len();
            self.lines[self.cursor.row].push_str(&line);
            self.dirty = true;
        }
    }

    pub fn delete_line(&mut self) {
        if self.lines.len() == 1 {
            self.yank = self.lines[0].clone();
            self.lines[0].clear();
        } else {
            self.yank = self.lines.remove(self.cursor.row);
            if self.cursor.row >= self.lines.len() {
                self.cursor.row = self.lines.len() - 1;
            }
        }
        self.clamp_col(false);
        self.dirty = true;
    }

    pub fn yank_line(&mut self) {
        self.yank = self.lines[self.cursor.row].clone();
    }

    pub fn paste_after(&mut self) {
        if self.yank.is_empty() {
            return;
        }
        self.lines.insert(self.cursor.row + 1, self.yank.clone());
        self.cursor.row += 1;
        self.cursor.col = 0;
        self.dirty = true;
    }
}

fn char_to_byte(s: &str, char_idx: usize) -> usize {
    s.char_indices()
        .nth(char_idx)
        .map(|(b, _)| b)
        .unwrap_or(s.len())
}
