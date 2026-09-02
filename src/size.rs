//! Shared geometry value for PTY and terminal grid sizing.

use std::num::NonZeroU16;

/// Grid size in character cells.
///
/// Used both for PTY window size (`TIOCSWINSZ`) and for the emulator's
/// screen. One type covers both geometries without assuming they always
/// agree. Rows and columns are [`NonZeroU16`] so an unaddressable zero-sized
/// grid cannot be constructed.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct Size {
    /// Number of rows.
    pub rows: NonZeroU16,
    /// Number of columns.
    pub cols: NonZeroU16,
}

impl Size {
    /// Builds a size, returning `None` if either dimension is zero.
    pub fn new(rows: u16, cols: u16) -> Option<Self> {
        Some(Self {
            rows: NonZeroU16::new(rows)?,
            cols: NonZeroU16::new(cols)?,
        })
    }
}
