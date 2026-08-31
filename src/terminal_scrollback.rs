//! Bounded scrollback storage and limits for the terminal emulator.

use crate::snapshot::TerminalLine;

/// Limits that bound the primary-screen scrollback.
///
/// If either bound is zero, scrollback is disabled and no history is retained.
/// Both bounds positive means history is kept until either limit is hit.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct ScrollbackLimits {
    /// Maximum number of retained lines.
    pub max_lines: usize,
    /// Maximum number of retained cells, including blank cells and
    /// wide-character continuation cells.
    pub max_cells: usize,
}

impl ScrollbackLimits {
    /// Scrollback disabled: no history is retained.
    pub const DISABLED: Self = Self {
        max_lines: 0,
        max_cells: 0,
    };

    /// Returns whether scrollback is disabled (either bound is zero).
    pub const fn is_disabled(self) -> bool {
        self.max_lines == 0 || self.max_cells == 0
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
    if limits.is_disabled() {
        return;
    }
    let max_cells = limits.max_cells;
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
    if limits.is_disabled() {
        return false;
    }
    let max_lines = limits.max_lines;
    let max_cells = limits.max_cells;
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
        ScrollbackLimits {
            max_lines,
            max_cells,
        }
    }

    #[test]
    fn either_zero_bound_disables_scrollback() {
        assert!(ScrollbackLimits::DISABLED.is_disabled());
        assert!(limits(0, 5).is_disabled());
        assert!(limits(5, 0).is_disabled());
        assert!(limits(0, 0).is_disabled());
        assert!(!limits(5, 5).is_disabled());
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
        assert!(!fits(0, 0, limits(0, 5), 1));
        assert!(!fits(0, 0, limits(5, 0), 1));
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
        append_line(&mut scrollback, limits(0, 100), line(4, 'a'));
        assert!(scrollback.is_empty());
        append_line(&mut scrollback, limits(100, 0), line(4, 'a'));
        assert!(scrollback.is_empty());
    }
}
