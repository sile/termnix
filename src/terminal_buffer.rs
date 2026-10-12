//! Screen cell buffer and scrolling helpers.

use crate::size::Size;
use crate::terminal_types::{Cell, Position, Style};

/// What a [`Screen::resize`] displaced: the rows it pushed off the top, each a
/// full left-to-right row of cells at the pre-shrink width in top-to-bottom
/// order, plus how far every surviving row moved up.
///
/// The shift is zero on a grow, so a caller that owns a cursor can subtract it
/// unconditionally and then clamp.
pub(crate) struct Displaced {
    pub(crate) rows: Vec<Vec<Cell>>,
    pub(crate) row_shift: u16,
}

#[derive(Clone)]
pub(crate) struct Screen {
    size: Size,
    cells: Vec<Cell>,
    /// Set by the writing methods below when they store a cell. It is derived
    /// bookkeeping for change detection and is omitted from `Debug` below.
    dirty: bool,
}

impl std::fmt::Debug for Screen {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // `dirty` is derived bookkeeping, so it is omitted here.
        f.debug_struct("Screen")
            .field("size", &self.size)
            .field("cells", &self.cells)
            .finish_non_exhaustive()
    }
}

impl Screen {
    pub(crate) fn blank(size: Size) -> Self {
        Self {
            size,
            cells: vec![Cell::EMPTY; cell_count(size)],
            dirty: false,
        }
    }

    /// Returns whether a cell write happened since the last [`take_dirty`],
    /// clearing the flag.
    ///
    /// [`take_dirty`]: Screen::take_dirty
    pub(crate) fn take_dirty(&mut self) -> bool {
        std::mem::take(&mut self.dirty)
    }

    /// Marks the screen as changed without writing a cell.
    ///
    /// Used when a caller replaces the whole screen (a hard reset) rather than
    /// writing through the methods below.
    pub(crate) fn mark_dirty(&mut self) {
        self.dirty = true;
    }

    pub(crate) fn get(&self, at: Position) -> Option<Cell> {
        Some(self.cells[self.index(at)?])
    }

    pub(crate) fn cells(&self) -> &[Cell] {
        &self.cells
    }

    pub(crate) fn clear_all(&mut self, style: Style) {
        let blank = Cell::blank(style);
        self.cells.fill(blank);
        self.dirty = true;
    }

    /// Resizes the cell buffer, copying the overlapping region and clearing a
    /// broken wide character at the new right edge.
    ///
    /// A vertical shrink keeps the rows at the new bottom edge and returns the
    /// rows it pushes off the top in top-to-bottom order, each as a full
    /// left-to-right row of cells at the pre-shrink width, along with the
    /// distance every surviving row moved up ([`Displaced::row_shift`], zero on
    /// a grow). Keeping the newest rows visible and handing the oldest to the
    /// caller matches a full-screen scroll, so the caller can append them to
    /// the same history, and the shift lets a caller that owns a cursor move it
    /// by the same amount. A no-op resize returns no rows and a zero shift.
    pub(crate) fn resize(&mut self, size: Size) -> Displaced {
        if size == self.size {
            return Displaced {
                rows: Vec::new(),
                row_shift: 0,
            };
        }
        let old_rows = self.size.rows.get() as usize;
        let old_cols = self.size.cols.get() as usize;
        let copy_rows = self.size.rows.get().min(size.rows.get()) as usize;
        let copy_cols = self.size.cols.get().min(size.cols.get()) as usize;
        // The rows that do not fit at the new height leave at the top, so the
        // newest rows stay visible. `row_shift` is how far each surviving row
        // moves up (zero on a grow).
        let row_shift = old_rows - copy_rows;
        let mut dropped = Vec::new();
        for row in 0..row_shift {
            let start = row * old_cols;
            dropped.push(self.cells[start..start + old_cols].to_vec());
        }
        let mut cells = vec![Cell::EMPTY; cell_count(size)];
        for row in 0..copy_rows {
            let src_row = row + row_shift;
            for col in 0..copy_cols {
                let src = src_row * old_cols + col;
                let dst = row * size.cols.get() as usize + col;
                cells[dst] = self.cells[src];
            }
            if copy_cols > 0 {
                let edge = copy_cols - 1;
                let dst = row * size.cols.get() as usize + edge;
                if cells[dst].width == 2 {
                    cells[dst] = Cell::EMPTY;
                }
            }
        }
        self.size = size;
        self.cells = cells;
        self.dirty = true;
        Displaced {
            rows: dropped,
            row_shift: row_shift as u16,
        }
    }

    pub(crate) fn clear_cell(&mut self, row: u16, col: u16) {
        let Some(index) = self.index(Position { row, col }) else {
            return;
        };
        match self.cells[index].width {
            0 => {
                if col > 0 {
                    let lead = self.index(Position { row, col: col - 1 }).expect("lead");
                    self.cells[lead] = Cell::EMPTY;
                }
                self.cells[index] = Cell::EMPTY;
            }
            2 => {
                self.cells[index] = Cell::EMPTY;
                if let Some(trail) = self.index(Position { row, col: col + 1 }) {
                    self.cells[trail] = Cell::EMPTY;
                }
            }
            _ => {
                self.cells[index] = Cell::EMPTY;
            }
        }
        self.dirty = true;
    }

    pub(crate) fn put_glyph(&mut self, row: u16, col: u16, ch: char, width: u8, style: Style) {
        self.clear_cell(row, col);
        if width == 2 {
            self.clear_cell(row, col + 1);
        }
        let lead = self.index(Position { row, col }).expect("cursor in range");
        self.cells[lead] = Cell { ch, width, style };
        if width == 2 {
            let trail = self
                .index(Position { row, col: col + 1 })
                .expect("trail in range");
            self.cells[trail] = Cell {
                ch: ' ',
                width: 0,
                style,
            };
        }
        self.dirty = true;
    }

    pub(crate) fn erase_cells(&mut self, row: u16, col_start: u16, col_end: u16, style: Style) {
        let cols = self.size.cols.get();
        let start = col_start.min(cols);
        let end = col_end.min(cols);
        let blank = Cell::blank(style);
        for col in start..end {
            if let Some(index) = self.index(Position { row, col }) {
                self.clear_cell(row, col);
                self.cells[index] = blank;
                self.dirty = true;
            }
        }
    }

    pub(crate) fn erase_rows(&mut self, row_start: u16, row_end: u16, style: Style) {
        let rows = self.size.rows.get();
        let start = row_start.min(rows);
        let end = row_end.min(rows);
        for row in start..end {
            self.erase_cells(row, 0, self.size.cols.get(), style);
        }
    }

    pub(crate) fn insert_columns(&mut self, row: u16, col: u16, count: u16, style: Style) {
        let cols = self.size.cols.get();
        if row >= self.size.rows.get() || col >= cols || count == 0 {
            return;
        }
        let count = count.min(cols - col) as usize;
        let row_start = row as usize * cols as usize;
        let row_end = row_start + cols as usize;
        let insert_at = row_start + col as usize;
        self.cells[insert_at..row_end].rotate_right(count);
        let blank = Cell::blank(style);
        self.cells[insert_at..insert_at + count].fill(blank);
        self.fix_row_edge(row);
        self.dirty = true;
    }

    pub(crate) fn delete_columns(&mut self, row: u16, col: u16, count: u16, style: Style) {
        let cols = self.size.cols.get();
        if row >= self.size.rows.get() || col >= cols || count == 0 {
            return;
        }
        let count = count.min(cols - col) as usize;
        let row_start = row as usize * cols as usize;
        let row_end = row_start + cols as usize;
        let delete_at = row_start + col as usize;
        self.cells[delete_at..row_end].rotate_left(count);
        let blank = Cell::blank(style);
        self.cells[row_end - count..row_end].fill(blank);
        self.fix_row_edge(row);
        self.dirty = true;
    }

    pub(crate) fn insert_lines(
        &mut self,
        row: u16,
        count: u16,
        scroll_top: u16,
        scroll_bottom: u16,
        style: Style,
    ) {
        if count == 0 || row < scroll_top || row > scroll_bottom {
            return;
        }
        let cols = self.size.cols.get() as usize;
        let count = count.min(scroll_bottom - row + 1) as usize;
        let top = row as usize * cols;
        let bottom = (scroll_bottom as usize + 1) * cols;
        self.cells[top..bottom].rotate_right(count * cols);
        let blank = Cell::blank(style);
        self.cells[top..top + count * cols].fill(blank);
        self.dirty = true;
    }

    pub(crate) fn delete_lines(
        &mut self,
        row: u16,
        count: u16,
        scroll_top: u16,
        scroll_bottom: u16,
        style: Style,
    ) {
        if count == 0 || row < scroll_top || row > scroll_bottom {
            return;
        }
        let cols = self.size.cols.get() as usize;
        let count = count.min(scroll_bottom - row + 1) as usize;
        let top = row as usize * cols;
        let bottom = (scroll_bottom as usize + 1) * cols;
        self.cells[top..bottom].rotate_left(count * cols);
        let blank = Cell::blank(style);
        self.cells[bottom - count * cols..bottom].fill(blank);
        self.dirty = true;
    }

    /// Scrolls the region up by `count` rows and returns the displaced rows
    /// (the rows that left the region) in top-to-bottom order, each as a full
    /// left-to-right row of cells.
    ///
    /// The caller decides whether the displaced rows become scrollback; a
    /// partial region scroll must discard them.
    pub(crate) fn scroll_up(
        &mut self,
        count: u16,
        scroll_top: u16,
        scroll_bottom: u16,
        style: Style,
    ) -> Vec<Vec<Cell>> {
        if count == 0 || scroll_top > scroll_bottom {
            return Vec::new();
        }
        let cols = self.size.cols.get() as usize;
        let region_rows = (scroll_bottom - scroll_top + 1) as usize;
        let count = count as usize;
        let displaced_count = count.min(region_rows);
        let mut displaced = Vec::with_capacity(displaced_count);
        for i in 0..displaced_count {
            let start = (scroll_top as usize + i) * cols;
            displaced.push(self.cells[start..start + cols].to_vec());
        }
        if count >= region_rows {
            self.erase_rows(scroll_top, scroll_bottom + 1, style);
            return displaced;
        }
        let top = scroll_top as usize * cols;
        let bottom = (scroll_bottom as usize + 1) * cols;
        self.cells[top..bottom].rotate_left(count * cols);
        let blank = Cell::blank(style);
        self.cells[bottom - count * cols..bottom].fill(blank);
        self.dirty = true;
        displaced
    }

    pub(crate) fn scroll_down(
        &mut self,
        count: u16,
        scroll_top: u16,
        scroll_bottom: u16,
        style: Style,
    ) {
        if count == 0 || scroll_top > scroll_bottom {
            return;
        }
        let cols = self.size.cols.get() as usize;
        let region_rows = (scroll_bottom - scroll_top + 1) as usize;
        let count = count as usize;
        if count >= region_rows {
            self.erase_rows(scroll_top, scroll_bottom + 1, style);
            return;
        }
        let top = scroll_top as usize * cols;
        let bottom = (scroll_bottom as usize + 1) * cols;
        self.cells[top..bottom].rotate_right(count * cols);
        let blank = Cell::blank(style);
        self.cells[top..top + count * cols].fill(blank);
        self.dirty = true;
    }

    fn fix_row_edge(&mut self, row: u16) {
        let cols = self.size.cols.get();
        if cols == 0 {
            return;
        }
        let edge = cols - 1;
        if let Some(index) = self.index(Position { row, col: edge })
            && self.cells[index].width == 2
        {
            self.cells[index] = Cell::EMPTY;
            self.dirty = true;
        }
    }

    fn index(&self, at: Position) -> Option<usize> {
        if at.row >= self.size.rows.get() || at.col >= self.size.cols.get() {
            return None;
        }
        Some(at.row as usize * self.size.cols.get() as usize + at.col as usize)
    }
}

fn cell_count(size: Size) -> usize {
    size.rows.get() as usize * size.cols.get() as usize
}
