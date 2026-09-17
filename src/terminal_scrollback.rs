//! Scrollback storage and trimming for the terminal emulator.

use std::collections::VecDeque;

use crate::snapshot::ScrollbackLine;

/// Removes the oldest complete lines until both limits hold.
///
/// Lines are always removed whole, oldest first, so the newest content is
/// kept. If either limit is zero the entire history is cleared. The retained
/// cell total is kept in `cells` and updated as lines are removed. When a
/// single newest line alone exceeds `max_cells`, nothing can be retained and
/// the history ends empty.
pub(crate) fn trim_scrollback(
    scrollback: &mut VecDeque<ScrollbackLine>,
    cells: &mut usize,
    max_lines: usize,
    max_cells: usize,
) {
    if max_lines == 0 || max_cells == 0 {
        *cells = 0;
        scrollback.clear();
        return;
    }
    while !scrollback.is_empty() && (scrollback.len() > max_lines || *cells > max_cells) {
        *cells = cells.saturating_sub(scrollback.pop_front().expect("non-empty").len());
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::terminal_types::{Cell, Style};

    fn line(cells: usize, ch: char) -> ScrollbackLine {
        ScrollbackLine::new(vec![
            Cell {
                ch,
                width: 1,
                style: Style::default()
            };
            cells
        ])
    }

    fn lines(chars: &[char]) -> VecDeque<ScrollbackLine> {
        chars.iter().map(|ch| line(4, *ch)).collect()
    }

    fn kept_chars(sb: &VecDeque<ScrollbackLine>) -> Vec<char> {
        sb.iter().map(|l| l.cells()[0].ch).collect()
    }

    #[test]
    fn zero_limit_clears_everything() {
        let mut sb = lines(&['a', 'b', 'c']);
        let mut cells = 12;
        trim_scrollback(&mut sb, &mut cells, 0, 100);
        assert!(sb.is_empty());
        assert_eq!(cells, 0);

        let mut sb = lines(&['a', 'b', 'c']);
        let mut cells = 12;
        trim_scrollback(&mut sb, &mut cells, 100, 0);
        assert!(sb.is_empty());
        assert_eq!(cells, 0);
    }

    #[test]
    fn trims_oldest_lines_first_to_line_bound() {
        let mut sb = lines(&['a', 'b', 'c', 'd']);
        let mut cells = 16;
        trim_scrollback(&mut sb, &mut cells, 2, 100);
        assert_eq!(kept_chars(&sb), vec!['c', 'd']);
        assert_eq!(cells, 8);
    }

    #[test]
    fn trims_to_cell_bound() {
        let mut sb = lines(&['a', 'b', 'c', 'd']);
        let mut cells = 16;
        trim_scrollback(&mut sb, &mut cells, 100, 6);
        assert_eq!(kept_chars(&sb), vec!['d']);
        assert_eq!(cells, 4);
    }

    #[test]
    fn trims_until_both_bounds_hold() {
        let mut sb = lines(&['a', 'b', 'c', 'd']);
        let mut cells = 16;
        trim_scrollback(&mut sb, &mut cells, 3, 8);
        assert_eq!(kept_chars(&sb), vec!['c', 'd']);
        assert_eq!(cells, 8);
    }

    #[test]
    fn oversized_single_line_ends_empty() {
        let mut sb = VecDeque::new();
        sb.push_back(line(100, 'x'));
        let mut cells = 100;
        trim_scrollback(&mut sb, &mut cells, 10, 50);
        assert!(sb.is_empty());
        assert_eq!(cells, 0);
    }
}
