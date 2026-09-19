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
