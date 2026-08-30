//! Shared geometry value for PTY and terminal grid sizing.

/// Grid size in character cells.
///
/// Used both for PTY window size (`TIOCSWINSZ`) and for the emulator's
/// screen. One type covers both geometries without assuming they always
/// agree.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct Size {
    /// Number of rows.
    pub rows: u16,
    /// Number of columns.
    pub cols: u16,
}
