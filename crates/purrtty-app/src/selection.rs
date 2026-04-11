//! Text selection state + pure helpers.
//!
//! The selection is stored as two `GridPoint`s (anchor and end). Both
//! points live in **absolute row coordinates** — row 0 is the oldest
//! scrollback row, row `scrollback_len + visible_rows - 1` is the
//! bottom of the live grid. This way a selection that straddles the
//! scrollback boundary stays anchored to the same content even as the
//! user scrolls the view.
//!
//! Conversion between pixel coords ↔ view row ↔ absolute row lives in
//! helpers that don't touch the grid at all, so they're easy to unit-test.

use purrtty_term::Grid;

/// Point in absolute grid coordinates (scrollback + visible rows).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct GridPoint {
    pub row: usize,
    pub col: usize,
}

impl GridPoint {
    pub fn new(row: usize, col: usize) -> Self {
        Self { row, col }
    }
}

/// A contiguous character selection anchored at `anchor` and extending
/// to `end`. Either endpoint may be earlier in reading order — use
/// `normalized` to get (start, end) in reading order.
#[derive(Debug, Clone, Copy)]
pub struct Selection {
    pub anchor: GridPoint,
    pub end: GridPoint,
}

impl Selection {
    pub fn new(at: GridPoint) -> Self {
        Self {
            anchor: at,
            end: at,
        }
    }

    pub fn update(&mut self, to: GridPoint) {
        self.end = to;
    }

    pub fn is_empty(&self) -> bool {
        self.anchor == self.end
    }

    /// Return the selection endpoints in reading order (top-left first).
    pub fn normalized(&self) -> (GridPoint, GridPoint) {
        if point_before(self.anchor, self.end) {
            (self.anchor, self.end)
        } else {
            (self.end, self.anchor)
        }
    }

    /// Does this selection cover the absolute cell at (row, col)?
    /// End is exclusive in cell terms.
    #[allow(dead_code)]
    pub fn contains_cell(&self, row: usize, col: usize) -> bool {
        if self.is_empty() {
            return false;
        }
        let (start, end) = self.normalized();
        let p = GridPoint::new(row, col);
        !point_before(p, start) && point_before(p, end)
    }
}

fn point_before(a: GridPoint, b: GridPoint) -> bool {
    a.row < b.row || (a.row == b.row && a.col < b.col)
}

/// Convert a mouse pixel coordinate to a (view_row, col) pair. Result
/// is clamped to the grid dimensions.
pub fn pixel_to_cell(
    x: f32,
    y: f32,
    pad_x: f32,
    pad_y: f32,
    cell_w: f32,
    cell_h: f32,
    rows: usize,
    cols: usize,
) -> (usize, usize) {
    let col = ((x - pad_x) / cell_w).floor().max(0.0) as usize;
    let row = ((y - pad_y) / cell_h).floor().max(0.0) as usize;
    (row.min(rows.saturating_sub(1)), col.min(cols.saturating_sub(1)))
}

/// Convert a visible row index (0 = top of the window) to an absolute
/// row index, honoring the current scroll offset and scrollback length.
pub fn view_row_to_absolute(view_row: usize, scroll_offset: usize, scrollback_len: usize) -> usize {
    let offset = scroll_offset.min(scrollback_len);
    (scrollback_len - offset) + view_row
}

/// Extract the text covered by `sel` from `grid`. Absolute row indices
/// outside the grid's live range are skipped gracefully. Wide-char
/// continuations (`WIDE_CONT`, `'\0'`) are omitted so the copied text
/// doesn't contain sentinel bytes.
pub fn selection_to_text(sel: &Selection, grid: &Grid) -> String {
    if sel.is_empty() {
        return String::new();
    }
    let (start, end) = sel.normalized();
    let cols = grid.cols();
    let mut out = String::new();

    for row in start.row..=end.row {
        let c_start = if row == start.row { start.col } else { 0 };
        let c_end = if row == end.row { end.col } else { cols };

        let mut row_buf = String::new();
        for col in c_start..c_end {
            if let Some(ch) = cell_char_absolute(grid, row, col) {
                if ch != '\0' {
                    row_buf.push(ch);
                }
            }
        }

        // Trim trailing spaces from intermediate rows — they're just
        // cell padding, not content. Keep them on the final row so
        // mid-line selections survive intact.
        if row < end.row {
            let trimmed = row_buf.trim_end_matches(' ').to_string();
            out.push_str(&trimmed);
            out.push('\n');
        } else {
            out.push_str(&row_buf);
        }
    }

    out
}

/// Look up a cell character by absolute row index (scrollback + live).
fn cell_char_absolute(grid: &Grid, abs_row: usize, col: usize) -> Option<char> {
    let sb_len = grid.scrollback_len();
    let rows = grid.rows();
    // Treat abs_row in [0, sb_len) as scrollback, [sb_len, sb_len+rows) as live.
    if abs_row < sb_len {
        // Scrollback row. Translate to `view_idx` that `row_at` uses by
        // pretending the scroll offset is (sb_len - abs_row).
        let offset = sb_len - abs_row;
        grid.row_at(0, offset).and_then(|r| r.get(col).map(|c| c.ch))
    } else {
        let view_idx = abs_row - sb_len;
        if view_idx >= rows {
            return None;
        }
        grid.row_at(view_idx, 0).and_then(|r| r.get(col).map(|c| c.ch))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use purrtty_term::Terminal;

    #[test]
    fn selection_empty_by_default() {
        let s = Selection::new(GridPoint::new(0, 0));
        assert!(s.is_empty());
    }

    #[test]
    fn selection_normalizes_backwards_drag() {
        let mut s = Selection::new(GridPoint::new(3, 5));
        s.update(GridPoint::new(1, 2));
        let (start, end) = s.normalized();
        assert_eq!(start, GridPoint::new(1, 2));
        assert_eq!(end, GridPoint::new(3, 5));
    }

    #[test]
    fn selection_contains_cell() {
        let mut s = Selection::new(GridPoint::new(1, 2));
        s.update(GridPoint::new(1, 5));
        assert!(s.contains_cell(1, 2));
        assert!(s.contains_cell(1, 3));
        assert!(s.contains_cell(1, 4));
        assert!(!s.contains_cell(1, 5), "end is exclusive");
        assert!(!s.contains_cell(1, 1));
        assert!(!s.contains_cell(0, 3));
    }

    #[test]
    fn pixel_to_cell_basic() {
        // cell 0x0 has its top-left at (16, 16) with 10x22 cells.
        let (r, c) = pixel_to_cell(16.0, 16.0, 16.0, 16.0, 10.0, 22.0, 24, 80);
        assert_eq!((r, c), (0, 0));
        // Middle of cell (3, 5) is at x = 16 + 5*10 + 5, y = 16 + 3*22 + 11.
        let (r, c) = pixel_to_cell(71.0, 93.0, 16.0, 16.0, 10.0, 22.0, 24, 80);
        assert_eq!((r, c), (3, 5));
        // Negative offset clamps to 0.
        let (r, c) = pixel_to_cell(0.0, 0.0, 16.0, 16.0, 10.0, 22.0, 24, 80);
        assert_eq!((r, c), (0, 0));
    }

    #[test]
    fn pixel_to_cell_clamps_to_grid() {
        let (r, c) = pixel_to_cell(9999.0, 9999.0, 16.0, 16.0, 10.0, 22.0, 24, 80);
        assert_eq!((r, c), (23, 79));
    }

    #[test]
    fn view_row_to_absolute_no_scroll() {
        // No scrollback → view row 0 maps to absolute 0.
        assert_eq!(view_row_to_absolute(0, 0, 0), 0);
        assert_eq!(view_row_to_absolute(5, 0, 0), 5);
    }

    #[test]
    fn view_row_to_absolute_with_scrollback() {
        // 100 rows of scrollback, not scrolled → view row 0 is the
        // first live row (absolute 100).
        assert_eq!(view_row_to_absolute(0, 0, 100), 100);
        // Scrolled up by 10: view row 0 is at absolute 90.
        assert_eq!(view_row_to_absolute(0, 10, 100), 90);
    }

    #[test]
    fn selection_to_text_single_row() {
        let mut term = Terminal::new(4, 20);
        term.advance(b"hello world");
        let sel = Selection {
            anchor: GridPoint::new(0, 0),
            end: GridPoint::new(0, 5),
        };
        let s = selection_to_text(&sel, term.grid());
        assert_eq!(s, "hello");
    }

    #[test]
    fn selection_to_text_multi_row() {
        let mut term = Terminal::new(4, 20);
        term.advance(b"line one\r\nline two\r\nline three");
        // Select from (0, 0) to (2, 4) — "line one\nline two\nline"
        let sel = Selection {
            anchor: GridPoint::new(0, 0),
            end: GridPoint::new(2, 4),
        };
        let s = selection_to_text(&sel, term.grid());
        assert_eq!(s, "line one\nline two\nline");
    }

    #[test]
    fn selection_skips_wide_char_continuation() {
        let mut term = Terminal::new(2, 10);
        term.advance("a한글b".as_bytes());
        // Select entire row.
        let sel = Selection {
            anchor: GridPoint::new(0, 0),
            end: GridPoint::new(0, 6),
        };
        let s = selection_to_text(&sel, term.grid());
        assert_eq!(s, "a한글b");
    }

    #[test]
    fn empty_selection_returns_empty_string() {
        let mut term = Terminal::new(4, 20);
        term.advance(b"hello");
        let sel = Selection::new(GridPoint::new(0, 0));
        assert_eq!(selection_to_text(&sel, term.grid()), "");
    }
}
