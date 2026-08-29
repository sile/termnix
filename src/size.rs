//! Shared geometry for PTY windows and terminal screens.

/// Grid size in character cells.
///
/// Used both for PTY window size (`TIOCSWINSZ`) and for the emulator's
/// primary screen. The two stay in lockstep for a pane, so one type avoids
/// redundant conversions without hiding that relationship.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct Size {
    /// Number of rows.
    pub rows: u16,
    /// Number of columns.
    pub cols: u16,
}
