//! Apply parsed `TextEdit`s to an in-memory line buffer.

use super::types::TextEdit;

/// Apply a list of [`TextEdit`]s to an in-memory `Vec<String>`
/// (one entry per buffer line). Edits are sorted in **reverse**
/// document order before being applied so earlier positions stay
/// valid as we splice. Out-of-range edits are clamped to the buffer
/// rather than panicking — a stale server response shouldn't crash us.
pub fn apply_text_edits(lines: &mut Vec<String>, mut edits: Vec<TextEdit>) {
    edits.sort_by(|a, b| {
        b.range
            .start
            .line
            .cmp(&a.range.start.line)
            .then_with(|| b.range.start.character.cmp(&a.range.start.character))
    });
    for edit in edits {
        apply_one_edit(lines, &edit);
    }
}

fn apply_one_edit(lines: &mut Vec<String>, edit: &TextEdit) {
    if lines.is_empty() {
        lines.push(String::new());
    }
    let last = lines.len() - 1;
    let s_row = (edit.range.start.line as usize).min(last);
    let e_row = (edit.range.end.line as usize).min(last);
    let s_col_chars = edit.range.start.character as usize;
    let e_col_chars = edit.range.end.character as usize;
    let prefix: String = lines[s_row].chars().take(s_col_chars).collect();
    let suffix: String = lines[e_row].chars().skip(e_col_chars).collect();
    let new_lines: Vec<&str> = edit.new_text.split('\n').collect();
    let replacement: Vec<String> = if new_lines.len() == 1 {
        vec![format!("{}{}{}", prefix, new_lines[0], suffix)]
    } else {
        let mut v = Vec::with_capacity(new_lines.len());
        v.push(format!("{}{}", prefix, new_lines[0]));
        for &mid in &new_lines[1..new_lines.len() - 1] {
            v.push(mid.to_string());
        }
        v.push(format!("{}{}", new_lines[new_lines.len() - 1], suffix));
        v
    };
    lines.splice(s_row..=e_row, replacement);
}

#[cfg(test)]
mod tests {
    use super::*;
    use super::super::types::{Position, Range};

    #[test]
    fn apply_text_edits_single_line_replace() {
        let mut lines = vec!["let foo = 1;".to_string()];
        let edits = vec![TextEdit {
            range: Range {
                start: Position {
                    line: 0,
                    character: 4,
                },
                end: Position {
                    line: 0,
                    character: 7,
                },
            },
            new_text: "bar".to_string(),
        }];
        apply_text_edits(&mut lines, edits);
        assert_eq!(lines, vec!["let bar = 1;".to_string()]);
    }

    #[test]
    fn apply_text_edits_order_independent() {
        // Two edits on the same line — the apply step must process them
        // right-to-left so the earlier edit doesn't shift the later one.
        let mut lines = vec!["aaa bbb ccc".to_string()];
        let edits = vec![
            TextEdit {
                range: Range {
                    start: Position {
                        line: 0,
                        character: 0,
                    },
                    end: Position {
                        line: 0,
                        character: 3,
                    },
                },
                new_text: "XXXX".to_string(),
            },
            TextEdit {
                range: Range {
                    start: Position {
                        line: 0,
                        character: 8,
                    },
                    end: Position {
                        line: 0,
                        character: 11,
                    },
                },
                new_text: "Y".to_string(),
            },
        ];
        apply_text_edits(&mut lines, edits);
        assert_eq!(lines, vec!["XXXX bbb Y".to_string()]);
    }
}
