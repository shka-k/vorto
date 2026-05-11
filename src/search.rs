use crate::editor::{Buffer, Cursor};

#[derive(Debug, Default)]
pub struct SearchState {
    pub query: String,
    pub last_forward: bool,
}

impl SearchState {
    pub fn set(&mut self, query: String, forward: bool) {
        self.query = query;
        self.last_forward = forward;
    }

    pub fn find_next(&self, buffer: &Buffer, forward: bool) -> Option<Cursor> {
        if self.query.is_empty() {
            return None;
        }
        if forward {
            find_forward(buffer, &self.query)
        } else {
            find_backward(buffer, &self.query)
        }
    }
}

fn find_forward(buffer: &Buffer, query: &str) -> Option<Cursor> {
    let start_row = buffer.cursor.row;
    let start_col = buffer.cursor.col + 1;

    for (offset, _) in buffer.lines.iter().enumerate().cycle().take(buffer.lines.len() + 1) {
        let row = (start_row + offset) % buffer.lines.len();
        let line = &buffer.lines[row];
        let search_from_byte = if offset == 0 {
            char_to_byte(line, start_col)
        } else {
            0
        };
        if search_from_byte > line.len() {
            continue;
        }
        if let Some(byte_idx) = line[search_from_byte..].find(query) {
            let abs_byte = search_from_byte + byte_idx;
            let col = byte_to_char(line, abs_byte);
            return Some(Cursor { row, col });
        }
    }
    None
}

fn find_backward(buffer: &Buffer, query: &str) -> Option<Cursor> {
    let n = buffer.lines.len();
    let start_row = buffer.cursor.row;
    let start_col = buffer.cursor.col;

    for offset in 0..=n {
        let row = (start_row + n - offset) % n;
        let line = &buffer.lines[row];
        let search_until_byte = if offset == 0 {
            char_to_byte(line, start_col)
        } else {
            line.len()
        };
        if let Some(byte_idx) = line[..search_until_byte].rfind(query) {
            let col = byte_to_char(line, byte_idx);
            return Some(Cursor { row, col });
        }
    }
    None
}

fn char_to_byte(s: &str, char_idx: usize) -> usize {
    s.char_indices()
        .nth(char_idx)
        .map(|(b, _)| b)
        .unwrap_or(s.len())
}

fn byte_to_char(s: &str, byte_idx: usize) -> usize {
    s[..byte_idx].chars().count()
}
