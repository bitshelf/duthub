//! Pure terminal text selection model, hit-testing math, and text extraction.
//!
//! Shared by the main serial terminal (`workspace.rs`) and the side-panel USB
//! shell (`side_panel.rs`). Kept free of GPUI types so every boundary case can
//! be unit-tested directly.

use std::borrow::Cow;

/// A character boundary in a flattened list of terminal lines.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub struct TextPoint {
    /// Zero-based item index in the rendered terminal list.
    pub row: usize,
    /// Character boundary offset within the row (`0..=row.chars().count()`).
    pub col: usize,
}

/// How mouse clicks and drags expand a terminal selection.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SelectionGranularity {
    /// Character-by-character selection (single click + drag).
    Char,
    /// Word/token selection (double click + drag).
    Word,
    /// Full-line selection (triple click + drag).
    Line,
}

/// Vertical hit-test classification against a uniform-height row list.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RowHit {
    /// Pointer is above row 0.
    Above,
    /// Pointer lands inside this row index.
    Row(usize),
    /// Pointer is below the last row.
    Below,
}

/// Active or dragging text selection in a terminal viewport.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TerminalSelection {
    anchor_start: TextPoint,
    anchor_end: TextPoint,
    focus_start: TextPoint,
    focus_end: TextPoint,
    pub granularity: SelectionGranularity,
    pub dragging: bool,
}

impl TerminalSelection {
    /// Starts a character selection at `point`.
    pub fn start_char(point: TextPoint) -> Self {
        Self {
            anchor_start: point,
            anchor_end: point,
            focus_start: point,
            focus_end: point,
            granularity: SelectionGranularity::Char,
            dragging: true,
        }
    }

    /// Starts a word selection around `char_idx` on `row`.
    pub fn start_word(row: usize, char_idx: usize, row_text: &str) -> Self {
        let (start, end) = word_bounds_at(row_text, char_idx);
        let a = TextPoint { row, col: start };
        let b = TextPoint { row, col: end };
        Self {
            anchor_start: a,
            anchor_end: b,
            focus_start: a,
            focus_end: b,
            granularity: SelectionGranularity::Word,
            dragging: true,
        }
    }

    /// Starts a full-line selection on `row`.
    pub fn start_line(row: usize, row_text: &str) -> Self {
        let end_col = row_text.chars().count();
        let a = TextPoint { row, col: 0 };
        let b = TextPoint { row, col: end_col };
        Self {
            anchor_start: a,
            anchor_end: b,
            focus_start: a,
            focus_end: b,
            granularity: SelectionGranularity::Line,
            dragging: true,
        }
    }

    /// Updates the focus end of the selection while dragging, returning `true`
    /// if the selection span changed.
    pub fn update(&mut self, point: TextPoint, row_text: &str) -> bool {
        let (next_start, next_end) = match self.granularity {
            SelectionGranularity::Char => (point, point),
            SelectionGranularity::Word => {
                let (start, end) = word_bounds_at(row_text, point.col);
                (
                    TextPoint {
                        row: point.row,
                        col: start,
                    },
                    TextPoint {
                        row: point.row,
                        col: end,
                    },
                )
            }
            SelectionGranularity::Line => (
                TextPoint {
                    row: point.row,
                    col: 0,
                },
                TextPoint {
                    row: point.row,
                    col: row_text.chars().count(),
                },
            ),
        };
        if self.focus_start == next_start && self.focus_end == next_end {
            return false;
        }
        self.focus_start = next_start;
        self.focus_end = next_end;
        true
    }

    /// Returns the ordered `(start, end)` endpoints of this selection.
    pub fn normalized(&self) -> (TextPoint, TextPoint) {
        (
            self.anchor_start.min(self.focus_start),
            self.anchor_end.max(self.focus_end),
        )
    }

    /// Returns `true` when the selection covers zero characters.
    pub fn is_empty(&self) -> bool {
        let (start, end) = self.normalized();
        start >= end
    }

    /// Shifts all row indices downward by `delta` when older lines are prepended.
    pub fn shift_rows(&mut self, delta: usize) {
        self.anchor_start.row = self.anchor_start.row.saturating_add(delta);
        self.anchor_end.row = self.anchor_end.row.saturating_add(delta);
        self.focus_start.row = self.focus_start.row.saturating_add(delta);
        self.focus_end.row = self.focus_end.row.saturating_add(delta);
    }

    /// Returns the selected character range `(start_col, end_col)` for `row`,
    /// or `None` if `row` is outside the selection or has no selected chars.
    pub fn row_range(&self, row: usize, row_char_len: usize) -> Option<(usize, usize)> {
        let (start, end) = self.normalized();
        if start >= end || row < start.row || row > end.row {
            return None;
        }
        let col_start = if row == start.row {
            start.col.min(row_char_len)
        } else {
            0
        };
        let col_end = if row == end.row {
            end.col.min(row_char_len)
        } else {
            row_char_len
        };
        (col_start < col_end).then_some((col_start, col_end))
    }

    /// Extracts the selected text across `0..item_count`, joining rows with `\n`.
    /// Returns `None` if the selection is empty or yields no text.
    pub fn extract_text<'a>(
        &self,
        item_count: usize,
        row_text: impl Fn(usize) -> Option<Cow<'a, str>>,
    ) -> Option<String> {
        if item_count == 0 {
            return None;
        }
        let (start, end) = self.normalized();
        if start >= end || start.row >= item_count {
            return None;
        }
        let last_row = end.row.min(item_count - 1);
        let mut lines = Vec::with_capacity(last_row - start.row + 1);
        for row in start.row..=last_row {
            let text = row_text(row).unwrap_or(Cow::Borrowed(""));
            let char_len = text.chars().count();
            let col_start = if row == start.row {
                start.col.min(char_len)
            } else {
                0
            };
            let col_end = if row == end.row {
                end.col.min(char_len)
            } else {
                char_len
            };
            lines.push(slice_chars(text.as_ref(), col_start, col_end).to_owned());
        }
        let joined = lines.join("\n");
        (!joined.is_empty()).then_some(joined)
    }
}

/// Classifies a vertical coordinate (relative to content top) against a
/// uniform-height list of `item_count` rows.
pub fn hit_test_row(rel_y: f32, row_height: f32, item_count: usize) -> Option<RowHit> {
    if item_count == 0 || row_height <= 0.0 || !rel_y.is_finite() {
        return None;
    }
    if rel_y < 0.0 {
        return Some(RowHit::Above);
    }
    #[expect(
        clippy::cast_possible_truncation,
        clippy::cast_sign_loss,
        reason = "rel_y >= 0.0 and row_height > 0.0 are guarded above; saturating cast is bounds-checked against item_count"
    )]
    let row = (rel_y / row_height).floor() as usize;
    if row >= item_count {
        Some(RowHit::Below)
    } else {
        Some(RowHit::Row(row))
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum CharClass {
    Word,
    Space,
    Other,
}

fn char_class(ch: char) -> CharClass {
    if ch.is_whitespace() {
        CharClass::Space
    } else if ch.is_alphanumeric() || matches!(ch, '_' | '-' | '.' | '/' | ':' | '~' | '+') {
        CharClass::Word
    } else {
        CharClass::Other
    }
}

/// Returns the `(start, end)` character range of the word/token at `char_idx`.
/// Clicking past the end of `text` returns an empty range at the line end.
pub fn word_bounds_at(text: &str, char_idx: usize) -> (usize, usize) {
    let chars: Vec<char> = text.chars().collect();
    let len = chars.len();
    if len == 0 || char_idx >= len {
        return (len, len);
    }
    let target = char_class(chars[char_idx]);
    let mut start = char_idx;
    while start > 0 && char_class(chars[start - 1]) == target {
        start -= 1;
    }
    let mut end = char_idx + 1;
    while end < len && char_class(chars[end]) == target {
        end += 1;
    }
    (start, end)
}

/// Converts a UTF-8 byte offset into a character offset safely.
pub fn byte_to_char_index(text: &str, byte_idx: usize) -> usize {
    let clamped = byte_idx.min(text.len());
    let mut chars = 0;
    for (idx, _) in text.char_indices() {
        if idx >= clamped {
            return chars;
        }
        chars += 1;
    }
    chars
}

/// Slices `text` by character offsets `[start_char..end_char)`.
pub fn slice_chars(text: &str, start_char: usize, end_char: usize) -> &str {
    if start_char >= end_char {
        return "";
    }
    let mut start_byte = text.len();
    let mut end_byte = text.len();
    for (char_idx, (byte_idx, _)) in text.char_indices().enumerate() {
        if char_idx == start_char {
            start_byte = byte_idx;
        }
        if char_idx == end_char {
            end_byte = byte_idx;
            break;
        }
    }
    if start_byte >= end_byte {
        ""
    } else {
        &text[start_byte..end_byte]
    }
}

/// Splits a plain text line into `(slice, selected)` runs for rendering.
pub fn split_plain_selection(text: &str, range: Option<(usize, usize)>) -> Vec<(&str, bool)> {
    let Some((start, end)) = range else {
        return if text.is_empty() {
            Vec::new()
        } else {
            vec![(text, false)]
        };
    };
    let char_len = text.chars().count();
    let start = start.min(char_len);
    let end = end.min(char_len);
    if start >= end {
        return if text.is_empty() {
            Vec::new()
        } else {
            vec![(text, false)]
        };
    }
    let mut out = Vec::with_capacity(3);
    let before = slice_chars(text, 0, start);
    if !before.is_empty() {
        out.push((before, false));
    }
    let mid = slice_chars(text, start, end);
    if !mid.is_empty() {
        out.push((mid, true));
    }
    let after = slice_chars(text, end, char_len);
    if !after.is_empty() {
        out.push((after, false));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rows<'a>(items: &'a [&'a str]) -> impl Fn(usize) -> Option<Cow<'a, str>> {
        move |idx| items.get(idx).copied().map(Cow::Borrowed)
    }

    #[test]
    fn char_selection_handles_forward_backward_and_multiline_extraction() {
        let lines = [
            "root@KC:~# ls",
            "emmc_cp_test.sh   seq_write_multi.0.0   shutdown.sh",
            "",
            "root@KC:~# ",
        ];
        // Forward single-line drag.
        let mut sel = TerminalSelection::start_char(TextPoint { row: 1, col: 0 });
        assert!(sel.is_empty());
        assert_eq!(sel.extract_text(lines.len(), rows(&lines)), None);
        assert!(sel.update(TextPoint { row: 1, col: 15 }, lines[1]));
        assert_eq!(
            sel.extract_text(lines.len(), rows(&lines)).as_deref(),
            Some("emmc_cp_test.sh")
        );

        // Backward multi-line drag preserving blank lines.
        let mut back = TerminalSelection::start_char(TextPoint { row: 3, col: 4 });
        back.update(TextPoint { row: 1, col: 40 }, lines[1]);
        assert_eq!(back.row_range(0, lines[0].len()), None);
        assert_eq!(back.row_range(1, lines[1].len()), Some((40, 51)));
        assert_eq!(back.row_range(2, lines[2].len()), None);
        assert_eq!(back.row_range(3, lines[3].len()), Some((0, 4)));
        assert_eq!(
            back.extract_text(lines.len(), rows(&lines)).as_deref(),
            Some("shutdown.sh\n\nroot")
        );
    }

    #[test]
    fn word_and_line_selection_cover_terminal_filenames_and_drag_extension() {
        let line = "emmc_cp_test.sh   seq_write_multi.0.0   shutdown.sh";
        let mut word = TerminalSelection::start_word(0, 5, line);
        assert_eq!(
            word.extract_text(1, rows(&[line])).as_deref(),
            Some("emmc_cp_test.sh")
        );
        // Dragging into the second token extends by full tokens.
        word.update(TextPoint { row: 0, col: 22 }, line);
        assert_eq!(
            word.extract_text(1, rows(&[line])).as_deref(),
            Some("emmc_cp_test.sh   seq_write_multi.0.0")
        );

        // Double-clicking past EOL produces an empty selection.
        let past_eol = TerminalSelection::start_word(0, line.len(), line);
        assert!(past_eol.is_empty());

        // Triple-click selects the entire line and drags across full lines.
        let lines = ["first line", "second line", "third line"];
        let mut full = TerminalSelection::start_line(1, lines[1]);
        assert_eq!(
            full.extract_text(lines.len(), rows(&lines)).as_deref(),
            Some("second line")
        );
        full.update(TextPoint { row: 0, col: 2 }, lines[0]);
        assert_eq!(
            full.extract_text(lines.len(), rows(&lines)).as_deref(),
            Some("first line\nsecond line")
        );
    }

    #[test]
    fn multibyte_slicing_and_byte_to_char_conversion_are_utf8_safe() {
        let text = "中文🎉ok";
        assert_eq!(byte_to_char_index(text, 0), 0);
        assert_eq!(byte_to_char_index(text, 3), 1);
        assert_eq!(byte_to_char_index(text, 6), 2);
        assert_eq!(byte_to_char_index(text, 10), 3);
        assert_eq!(byte_to_char_index(text, text.len()), 5);
        assert_eq!(slice_chars(text, 1, 4), "文🎉o");
        assert_eq!(
            split_plain_selection(text, Some((1, 3))),
            vec![("中", false), ("文🎉", true), ("ok", false)]
        );
    }

    #[test]
    fn hit_test_row_and_shrunk_buffer_clamping_never_panic() {
        assert_eq!(hit_test_row(0.0, 20.0, 0), None);
        assert_eq!(hit_test_row(-5.0, 20.0, 3), Some(RowHit::Above));
        assert_eq!(hit_test_row(25.0, 20.0, 3), Some(RowHit::Row(1)));
        assert_eq!(hit_test_row(65.0, 20.0, 3), Some(RowHit::Below));

        let mut sel = TerminalSelection::start_char(TextPoint { row: 0, col: 0 });
        sel.update(TextPoint { row: 10, col: 50 }, "short");
        assert_eq!(
            sel.extract_text(2, rows(&["alpha", "beta"])).as_deref(),
            Some("alpha\nbeta")
        );
    }

    #[test]
    fn shift_rows_keeps_selection_aligned_after_history_prepend() {
        let mut sel = TerminalSelection::start_char(TextPoint { row: 2, col: 1 });
        sel.update(TextPoint { row: 4, col: 5 }, "123456");
        sel.shift_rows(3000);
        assert_eq!(
            sel.normalized(),
            (
                TextPoint { row: 3002, col: 1 },
                TextPoint { row: 3004, col: 5 }
            )
        );
    }

    #[test]
    fn hit_test_row_stays_accurate_deep_in_scrollback_with_snapped_row_height() {
        // Simulates a 4000-row scrollback where Taffy snapped 20.6 px to 21.0 px (100% DPI).
        let snapped_row_height = 21.0_f32;
        let item_count = 4000;
        let target_row = 3950;
        let rel_y = 3950.0_f32 * snapped_row_height + 5.0;
        assert_eq!(
            hit_test_row(rel_y, snapped_row_height, item_count),
            Some(RowHit::Row(target_row))
        );
    }
}
