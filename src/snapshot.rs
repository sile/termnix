//! Owned, I/O-free snapshots of the terminal state.
//!
//! A [`TerminalSnapshot`] copies the visible screen, cursor, modes, current
//! style, title, whether the alternate screen is active, and primary-derived
//! scrollback out of a [`TerminalState`] so the caller can keep the data while
//! the session keeps running. It never borrows from the emulator.

use crate::size::Size;
use crate::terminal_types::{Cell, Position, Style, TerminalModes};

/// One physical row saved into scrollback, oldest-first.
///
/// Cells are owned in left-to-right order and are never reflowed by later
/// resizes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TerminalLine {
    cells: Vec<Cell>,
}

impl TerminalLine {
    pub(crate) fn new(cells: Vec<Cell>) -> Self {
        Self { cells }
    }

    /// Returns the row's cells in left-to-right order.
    pub fn cells(&self) -> &[Cell] {
        &self.cells
    }

    /// Returns the number of cells in the line (the row width at save time).
    pub fn len(&self) -> usize {
        self.cells.len()
    }

    /// Returns whether the line has no cells.
    pub fn is_empty(&self) -> bool {
        self.cells.is_empty()
    }
}

/// Owned copy of the terminal state at snapshot time.
///
/// Construction copies the visible screen, cursor, modes, current style,
/// title, whether the alternate screen was active, primary-derived
/// scrollback, and the source revision. Time and allocation scale with the
/// number of visible cells, retained scrollback cells, and title bytes.
/// Snapshot payloads never
/// include session or pane identity, child process status, file descriptors,
/// parser state, or undrained
/// [`TerminalAction`](crate::terminal_types::TerminalAction)s.
#[derive(Debug, Clone)]
pub struct TerminalSnapshot {
    size: Size,
    cells: Vec<Cell>,
    cursor: Position,
    modes: TerminalModes,
    style: Style,
    title: String,
    on_alternate: bool,
    scrollback: Vec<TerminalLine>,
    revision: u64,
}

impl PartialEq for TerminalSnapshot {
    fn eq(&self, other: &Self) -> bool {
        self.size == other.size
            && self.cells == other.cells
            && self.cursor == other.cursor
            && self.modes == other.modes
            && self.style == other.style
            && self.title == other.title
            && self.on_alternate == other.on_alternate
            && self.scrollback == other.scrollback
        // `revision` is derived bookkeeping whose value depends on how the
        // input was partitioned across `feed` calls, so it is excluded from
        // equality: two snapshots with identical visible content compare equal
        // even when captured at different revisions.
    }
}

impl Eq for TerminalSnapshot {}

impl TerminalSnapshot {
    #[expect(
        clippy::too_many_arguments,
        reason = "the constructor collects every field of a snapshot in one place"
    )]
    pub(crate) fn new(
        size: Size,
        cells: Vec<Cell>,
        cursor: Position,
        modes: TerminalModes,
        style: Style,
        title: String,
        on_alternate: bool,
        scrollback: Vec<TerminalLine>,
        revision: u64,
    ) -> Self {
        Self {
            size,
            cells,
            cursor,
            modes,
            style,
            title,
            on_alternate,
            scrollback,
            revision,
        }
    }

    /// Returns the captured screen size.
    pub fn size(&self) -> Size {
        self.size
    }

    /// Returns the source terminal's visible-state revision at capture time.
    ///
    /// See [`TerminalState::revision`](crate::TerminalState::revision) for what
    /// the value tracks. Two snapshots with equal revisions were taken while
    /// the visible state had not changed in between.
    pub fn revision(&self) -> u64 {
        self.revision
    }

    /// Returns the cell at `at` on the captured active screen, if in range.
    pub fn cell(&self, at: Position) -> Option<Cell> {
        if at.row >= self.size.rows.get() || at.col >= self.size.cols.get() {
            return None;
        }
        Some(self.cells[at.row as usize * self.size.cols.get() as usize + at.col as usize])
    }

    /// Returns each visible row as a left-to-right cell slice, top to bottom.
    ///
    /// The number of rows equals [`Size::rows`](crate::size::Size::rows) and
    /// every slice has exactly [`Size::cols`](crate::size::Size::cols) cells.
    /// As with [`TerminalLine::cells`], a row slice keeps the width it had at
    /// snapshot time and is never reflowed by later resizes.
    pub fn rows(&self) -> impl Iterator<Item = &[Cell]> {
        let cols = self.size.cols.get() as usize;
        self.cells.chunks(cols)
    }

    /// Returns the visible row at `row` as a cell slice, if in range.
    ///
    /// Equivalent to indexing the [`rows`](Self::rows) iterator by row
    /// number; out-of-range rows return `None`.
    pub fn row(&self, row: u16) -> Option<&[Cell]> {
        if row >= self.size.rows.get() {
            return None;
        }
        self.rows().nth(row as usize)
    }

    /// Returns the captured cursor position.
    pub fn cursor(&self) -> Position {
        self.cursor
    }

    /// Returns the captured terminal modes.
    pub fn modes(&self) -> TerminalModes {
        self.modes
    }

    /// Returns the captured drawing style (SGR pen).
    pub fn style(&self) -> Style {
        self.style
    }

    /// Returns the captured OSC window title.
    pub fn title(&self) -> &str {
        &self.title
    }

    /// Returns whether the alternate screen buffer was active at capture time.
    pub fn is_on_alternate_screen(&self) -> bool {
        self.on_alternate
    }

    /// Returns the captured primary-derived scrollback, oldest-first.
    pub fn scrollback(&self) -> &[TerminalLine] {
        &self.scrollback
    }
}
