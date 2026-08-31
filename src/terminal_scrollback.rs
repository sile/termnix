//! Bounded scrollback storage and limits for the terminal emulator.

use crate::snapshot::TerminalLine;

/// Limits that bound the primary-screen scrollback.
///
/// Both bounds must be positive together, or both zero for disabled
/// scrollback. A partially zero setting is invalid and cannot be constructed
/// through [`ScrollbackLimits::new`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct ScrollbackLimits {
    max_lines: usize,
    max_cells: usize,
}

impl ScrollbackLimits {
    /// Scrollback disabled: no history is retained.
    pub const DISABLED: Self = Self {
        max_lines: 0,
        max_cells: 0,
    };

    /// Returns limits with both bounds positive, or `None` when exactly one
    /// bound is zero (an invalid combination).
    ///
    /// Both bounds at zero is valid and means scrollback is disabled.
    pub fn new(max_lines: usize, max_cells: usize) -> Option<Self> {
        let both_zero = max_lines == 0 && max_cells == 0;
        let both_positive = max_lines > 0 && max_cells > 0;
        if both_zero || both_positive {
            Some(Self {
                max_lines,
                max_cells,
            })
        } else {
            None
        }
    }

    /// Returns the maximum number of retained lines.
    pub fn max_lines(&self) -> usize {
        self.max_lines
    }

    /// Returns the maximum number of retained cells, including blank cells
    /// and wide-character continuation cells.
    pub fn max_cells(&self) -> usize {
        self.max_cells
    }

    /// Returns whether scrollback is disabled (no history retained).
    pub fn is_disabled(&self) -> bool {
        self.max_lines == 0 && self.max_cells == 0
    }
}

impl Default for ScrollbackLimits {
    fn default() -> Self {
        Self::DISABLED
    }
}

/// Appends `line` to `scrollback` under `limits`, evicting the oldest
/// complete lines first.
///
/// A line that alone exceeds either limit is dropped entirely; a line is
/// never stored partially. Both limits are satisfied after the call whenever
/// scrollback is enabled, and the retained length never grows unboundedly
/// because every append evicts before pushing.
pub(crate) fn append_line(
    scrollback: &mut Vec<TerminalLine>,
    limits: ScrollbackLimits,
    line: TerminalLine,
) {
    let max_lines = limits.max_lines();
    let max_cells = limits.max_cells();
    if max_lines == 0 || max_cells == 0 {
        return;
    }
    let new_cells = line.len();
    if new_cells > max_cells {
        return;
    }
    while !scrollback.is_empty() {
        let retained_cells: usize = scrollback.iter().map(TerminalLine::len).sum();
        if fits(scrollback.len(), retained_cells, limits, new_cells) {
            break;
        }
        scrollback.remove(0);
    }
    scrollback.push(line);
}

/// Overflow-safe admission check: would `new_cells` fit on top of the
/// retained totals without exceeding either limit?
///
/// The `new_cells <= max_cells - retained_cells` form avoids an addition
/// overflow on `retained_cells + new_cells`; the `retained_cells <=
/// max_cells` guard makes the subtraction total even if the invariant were
/// ever violated.
fn fits(
    retained_lines: usize,
    retained_cells: usize,
    limits: ScrollbackLimits,
    new_cells: usize,
) -> bool {
    let max_lines = limits.max_lines();
    let max_cells = limits.max_cells();
    if max_lines == 0 || max_cells == 0 {
        return false;
    }
    retained_lines < max_lines
        && retained_cells <= max_cells
        && new_cells <= max_cells - retained_cells
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::terminal_types::{Cell, Style};

    fn line(cells: usize, ch: char) -> TerminalLine {
        TerminalLine::new(vec![
            Cell {
                ch,
                width: 1,
                style: Style::default()
            };
            cells
        ])
    }

    fn limits(max_lines: usize, max_cells: usize) -> ScrollbackLimits {
        ScrollbackLimits::new(max_lines, max_cells).expect("valid limits")
    }

    #[test]
    fn constructor_rejects_partially_zero_limits() {
        assert!(ScrollbackLimits::new(0, 0).is_some());
        assert_eq!(ScrollbackLimits::new(0, 5), None);
        assert_eq!(ScrollbackLimits::new(5, 0), None);
        assert!(ScrollbackLimits::new(5, 5).is_some());
        assert!(ScrollbackLimits::DISABLED.is_disabled());
        assert_eq!(ScrollbackLimits::default(), ScrollbackLimits::DISABLED);
    }

    #[test]
    fn admission_boundary_values() {
        // `new_cells == max_cells - retained_cells` fits exactly.
        assert!(fits(0, 0, limits(10, 10), 10));
        assert!(fits(1, 6, limits(10, 10), 4));
        // One more cell than the room available does not fit.
        assert!(!fits(1, 6, limits(10, 10), 5));
        // Line-count boundary.
        assert!(fits(9, 0, limits(10, 100), 1));
        assert!(!fits(10, 0, limits(10, 100), 1));
        // Disabled limits never admit.
        assert!(!fits(0, 0, ScrollbackLimits::DISABLED, 1));
        // A single line larger than max_cells never fits, even when empty.
        assert!(!fits(0, 0, limits(10, 5), 6));
        // Retained cells above max_cells never admit (defensive guard).
        assert!(!fits(0, 11, limits(10, 10), 1));
    }

    #[test]
    fn append_evicts_oldest_lines_first() {
        let mut scrollback = Vec::new();
        let lim = limits(2, 100);
        append_line(&mut scrollback, lim, line(4, 'a'));
        append_line(&mut scrollback, lim, line(4, 'b'));
        append_line(&mut scrollback, lim, line(4, 'c'));
        assert_eq!(scrollback.len(), 2);
        assert_eq!(scrollback[0].cells()[0].ch, 'b');
        assert_eq!(scrollback[1].cells()[0].ch, 'c');
    }

    #[test]
    fn append_bounds_total_cells() {
        let mut scrollback = Vec::new();
        let lim = limits(100, 6);
        for ch in ['a', 'b', 'c', 'd', 'e'] {
            append_line(&mut scrollback, lim, line(4, ch));
        }
        assert_eq!(scrollback.len(), 1);
        assert_eq!(scrollback[0].cells()[0].ch, 'e');
        let cells: usize = scrollback.iter().map(TerminalLine::len).sum();
        assert!(cells <= 6);
    }

    #[test]
    fn append_drops_single_line_over_max_cells() {
        let mut scrollback = Vec::new();
        append_line(&mut scrollback, limits(10, 3), line(4, 'a'));
        assert!(scrollback.is_empty());
    }

    #[test]
    fn disabled_append_never_retains() {
        let mut scrollback = Vec::new();
        append_line(&mut scrollback, ScrollbackLimits::DISABLED, line(4, 'a'));
        assert!(scrollback.is_empty());
    }
}
