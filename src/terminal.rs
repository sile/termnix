//! I/O-free terminal emulator state for a primary screen.
//!
//! [`Terminal`] accepts output bytes through [`Terminal::feed`] and updates
//! cells and the cursor. Escape sequences beyond basic C0 controls are out of
//! scope here; incomplete UTF-8 sequences are buffered across `feed` calls.
//!
//! # Unicode policy (initial core)
//!
//! - Display width uses the `unicode-width` crate (UAX #11) internally.
//! - Width 1 and 2 characters are placed on the primary screen.
//! - Width 0 characters (for example combining marks) are ignored.
//! - Control characters other than the handled C0 set are ignored.
//! - Invalid UTF-8 bytes become U+FFFD (width 1).
//! - Combining characters, emoji ZWJ sequences, and full East Asian Width
//!   edge cases beyond single-codepoint width are not modeled yet.
//!
//! No ANSI parser crate is used: this core only needs UTF-8 and a small C0
//! set, so a hand-rolled incremental decoder keeps the dependency surface
//! small and makes chunk-boundary behavior explicit.

use std::cmp::min;

use unicode_width::UnicodeWidthChar;

use crate::size::Size;

/// Zero-based position on the primary screen.
///
/// Used both for the active cursor and for addressing cells through
/// [`Terminal::cell`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct Position {
    /// Zero-based row.
    pub row: u16,
    /// Zero-based column.
    pub col: u16,
}

/// One cell on the primary screen.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct Cell {
    /// Glyph stored in this cell.
    ///
    /// Empty cells use `' '`. Wide-character continuation cells also use
    /// `' '` with [`Cell::width`] equal to `0`.
    pub ch: char,
    /// Display width contributed by this cell: `1` or `2` for a leading
    /// glyph cell, or `0` for the trailing half of a width-2 glyph.
    pub width: u8,
}

impl Cell {
    /// An empty single-column cell.
    pub const EMPTY: Self = Self { ch: ' ', width: 1 };

    /// Trailing half of a width-2 glyph.
    pub const CONTINUATION: Self = Self { ch: ' ', width: 0 };
}

/// Primary-screen terminal state.
///
/// The emulator owns no file descriptors and performs no I/O. Callers feed
/// PTY output (or any byte stream) and read cells for rendering or headless
/// inspection.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Terminal {
    size: Size,
    cursor: Position,
    cells: Vec<Cell>,
    pending_utf8: Vec<u8>,
    /// When set, the next printable character wraps before being placed.
    ///
    /// This matches common VT autowrap behavior: writing into the last column
    /// leaves the cursor there until the next character arrives.
    wrap_pending: bool,
}

impl Terminal {
    /// Creates a blank terminal of `size`.
    ///
    /// A zero row or column count is clamped to `1` so the screen always has
    /// at least one addressable cell.
    pub fn new(size: Size) -> Self {
        let size = clamp_size(size);
        let cells = vec![Cell::EMPTY; cell_count(size)];
        Self {
            size,
            cursor: Position { row: 0, col: 0 },
            cells,
            pending_utf8: Vec::new(),
            wrap_pending: false,
        }
    }

    /// Feeds output bytes into the emulator.
    ///
    /// Bytes may end in the middle of a UTF-8 sequence; the remainder is kept
    /// until a later `feed` completes or rejects it.
    pub fn feed(&mut self, bytes: &[u8]) {
        let mut input = std::mem::take(&mut self.pending_utf8);
        input.extend_from_slice(bytes);
        let mut idx = 0;
        while idx < input.len() {
            let byte = input[idx];
            if byte < 0x80 {
                idx += 1;
                self.handle_byte(byte);
                continue;
            }

            let needed = utf8_width(byte);
            if needed == 0 {
                idx += 1;
                self.put_char('\u{FFFD}');
                continue;
            }
            if idx + needed > input.len() {
                self.pending_utf8.extend_from_slice(&input[idx..]);
                break;
            }
            let chunk = &input[idx..idx + needed];
            idx += needed;
            match std::str::from_utf8(chunk) {
                Ok(s) => {
                    let ch = s.chars().next().expect("non-empty UTF-8 chunk");
                    self.put_char(ch);
                }
                Err(_) => self.put_char('\u{FFFD}'),
            }
        }
    }

    /// Returns the current screen size.
    pub fn size(&self) -> Size {
        self.size
    }

    /// Returns the current cursor position.
    pub fn cursor(&self) -> Position {
        self.cursor
    }

    /// Returns the cell at `at`, if that position lies on screen.
    pub fn cell(&self, at: Position) -> Option<Cell> {
        let index = self.index(at.row, at.col)?;
        Some(self.cells[index])
    }

    /// Resizes the primary screen.
    ///
    /// Existing contents are copied into the overlapping region. Broken wide
    /// characters at the new right edge are cleared. The cursor is clamped
    /// into the new bounds.
    pub fn resize(&mut self, size: Size) {
        let size = clamp_size(size);
        if size == self.size {
            return;
        }

        let mut cells = vec![Cell::EMPTY; cell_count(size)];
        let copy_rows = min(self.size.rows, size.rows);
        let copy_cols = min(self.size.cols, size.cols);
        for row in 0..copy_rows {
            for col in 0..copy_cols {
                let src = self.index(row, col).expect("source cell in range");
                let dst = row as usize * size.cols as usize + col as usize;
                cells[dst] = self.cells[src];
            }
            // A width-2 glyph whose continuation was clipped off the new
            // right edge must not leave an orphan lead cell.
            if copy_cols > 0 {
                let edge = copy_cols - 1;
                let dst = row as usize * size.cols as usize + edge as usize;
                if cells[dst].width == 2 {
                    cells[dst] = Cell::EMPTY;
                }
            }
        }

        self.size = size;
        self.cells = cells;
        self.cursor.row = min(self.cursor.row, size.rows - 1);
        self.cursor.col = min(self.cursor.col, size.cols - 1);
        self.wrap_pending = false;
        self.repair_cursor_cell();
    }

    fn handle_byte(&mut self, byte: u8) {
        match byte {
            0x07 => {} // BEL: ignored in the initial core
            0x08 => self.backspace(),
            0x09 => self.horizontal_tab(),
            0x0a => self.line_feed(),
            0x0d => self.carriage_return(),
            0x00..=0x1f | 0x7f => {} // other C0 / DEL: ignored
            _ => {
                let ch = char::from(byte);
                self.put_char(ch);
            }
        }
    }

    fn put_char(&mut self, ch: char) {
        let Some(width) = char_display_width(ch) else {
            return;
        };
        if width == 0 {
            return;
        }

        if self.wrap_pending {
            self.wrap_pending = false;
            self.carriage_return();
            self.line_feed();
        }

        if self.cursor.col as usize + width > self.size.cols as usize {
            self.carriage_return();
            self.line_feed();
        }

        // If still no room (1-column terminal and width 2), drop the glyph.
        if self.cursor.col as usize + width > self.size.cols as usize {
            return;
        }

        let row = self.cursor.row;
        let col = self.cursor.col;
        self.clear_cell(row, col);
        if width == 2 {
            self.clear_cell(row, col + 1);
        }

        let lead = self.index(row, col).expect("cursor in range");
        self.cells[lead] = Cell {
            ch,
            width: width as u8,
        };
        if width == 2 {
            let trail = self.index(row, col + 1).expect("trail in range");
            self.cells[trail] = Cell::CONTINUATION;
        }

        let next_col = col + width as u16;
        if next_col >= self.size.cols {
            self.cursor.col = self.size.cols - 1;
            self.wrap_pending = true;
        } else {
            self.cursor.col = next_col;
            self.wrap_pending = false;
        }
    }

    fn backspace(&mut self) {
        self.wrap_pending = false;
        if self.cursor.col > 0 {
            self.cursor.col -= 1;
        }
    }

    fn horizontal_tab(&mut self) {
        self.wrap_pending = false;
        let next = (self.cursor.col - self.cursor.col % 8) + 8;
        self.cursor.col = min(next, self.size.cols - 1);
    }

    fn line_feed(&mut self) {
        self.wrap_pending = false;
        if self.cursor.row + 1 < self.size.rows {
            self.cursor.row += 1;
        } else {
            self.scroll_up();
        }
    }

    fn carriage_return(&mut self) {
        self.wrap_pending = false;
        self.cursor.col = 0;
    }

    fn scroll_up(&mut self) {
        let cols = self.size.cols as usize;
        let rows = self.size.rows as usize;
        if rows == 0 {
            return;
        }
        self.cells.copy_within(cols.., 0);
        let start = (rows - 1) * cols;
        for cell in &mut self.cells[start..] {
            *cell = Cell::EMPTY;
        }
    }

    fn clear_cell(&mut self, row: u16, col: u16) {
        let Some(index) = self.index(row, col) else {
            return;
        };
        match self.cells[index].width {
            0 => {
                if col > 0 {
                    let lead = self.index(row, col - 1).expect("lead in range");
                    self.cells[lead] = Cell::EMPTY;
                }
                self.cells[index] = Cell::EMPTY;
            }
            2 => {
                self.cells[index] = Cell::EMPTY;
                if let Some(trail) = self.index(row, col + 1) {
                    self.cells[trail] = Cell::EMPTY;
                }
            }
            _ => {
                self.cells[index] = Cell::EMPTY;
            }
        }
    }

    fn repair_cursor_cell(&mut self) {
        if let Some(cell) = self.cell(self.cursor)
            && cell.width == 0
            && self.cursor.col > 0
        {
            self.cursor.col -= 1;
        }
    }

    fn index(&self, row: u16, col: u16) -> Option<usize> {
        if row >= self.size.rows || col >= self.size.cols {
            return None;
        }
        Some(row as usize * self.size.cols as usize + col as usize)
    }
}

fn clamp_size(size: Size) -> Size {
    Size {
        rows: size.rows.max(1),
        cols: size.cols.max(1),
    }
}

fn cell_count(size: Size) -> usize {
    size.rows as usize * size.cols as usize
}

fn utf8_width(first: u8) -> usize {
    match first {
        0xc2..=0xdf => 2,
        0xe0..=0xef => 3,
        0xf0..=0xf4 => 4,
        _ => 0,
    }
}

fn char_display_width(ch: char) -> Option<usize> {
    match ch.width() {
        None => None,
        Some(0) => Some(0),
        Some(1) => Some(1),
        Some(2) => Some(2),
        Some(_) => Some(1),
    }
}
