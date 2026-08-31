//! Deterministic tests for `TerminalSnapshot` and bounded scrollback.
//!
//! Only the public API of `TerminalState` and the snapshot types is used.

use muxnix::{Position, ScrollbackLimits, Size, TerminalSnapshot, TerminalState};

fn term(rows: u16, cols: u16) -> TerminalState {
    TerminalState::new(Size { rows, cols })
}

fn term_with_scrollback(rows: u16, cols: u16, max_lines: usize, max_cells: usize) -> TerminalState {
    let limits = ScrollbackLimits::new(max_lines, max_cells).expect("valid limits");
    TerminalState::with_scrollback(Size { rows, cols }, limits)
}

fn text_at(snap: &TerminalSnapshot, row: u16) -> String {
    let cols = snap.size().cols;
    let mut out = String::new();
    for col in 0..cols {
        let cell = snap.cell(Position { row, col }).expect("cell in range");
        if cell.width == 0 {
            continue;
        }
        out.push(cell.ch);
    }
    out.trim_end().to_string()
}

fn line_text(line: &muxnix::TerminalLine) -> String {
    let mut out = String::new();
    for cell in line.cells() {
        if cell.width == 0 {
            continue;
        }
        out.push(cell.ch);
    }
    out.trim_end().to_string()
}

#[test]
fn snapshot_owns_size_cells_cursor_modes_style_title_and_active() {
    let mut t = term(2, 4);
    t.feed(b"ab\x1b[1;31mZ");
    t.feed(b"\x1b[2;1H\x1b]2;snap-title\x07\x1b[?25l");
    let snap = t.snapshot();

    assert_eq!(snap.size(), Size { rows: 2, cols: 4 });
    assert_eq!(snap.size(), t.size());
    // Row-major cells: rows * cols count.
    assert_eq!(
        snap.cell(Position { row: 0, col: 0 }),
        t.cell(Position { row: 0, col: 0 })
    );
    assert_eq!(
        snap.cell(Position { row: 0, col: 1 }),
        t.cell(Position { row: 0, col: 1 })
    );
    assert_eq!(
        snap.cell(Position { row: 1, col: 3 }),
        t.cell(Position { row: 1, col: 3 })
    );
    assert_eq!(snap.cell(Position { row: 2, col: 0 }), None);
    assert_eq!(snap.cell(Position { row: 0, col: 4 }), None);

    assert_eq!(snap.cursor(), t.cursor());
    assert_eq!(snap.modes(), t.modes());
    assert!(!snap.modes().cursor_visible);
    assert_eq!(snap.current_style(), t.current_style());
    assert_eq!(snap.title(), t.title());
    assert_eq!(snap.title(), "snap-title");
    assert!(!snap.is_on_alternate_screen());
    assert!(snap.scrollback().is_empty());
}

#[test]
fn snapshot_is_owned_and_unaffected_by_later_updates() {
    let mut t = term_with_scrollback(2, 4, 100, 10_000);
    t.feed(b"aa\r\nbb\r\ncc\r\ndd");
    let snap = t.snapshot();
    t.feed(b"ee\r\nff");

    // The snapshot taken earlier still equals a fresh term fed only the
    // original prefix; updating `t` did not mutate it.
    let mut fresh = term_with_scrollback(2, 4, 100, 10_000);
    fresh.feed(b"aa\r\nbb\r\ncc\r\ndd");
    assert_eq!(snap, fresh.snapshot());
    assert_ne!(snap, t.snapshot());
}

#[test]
fn wide_character_lead_and_continuation_survive_in_snapshot() {
    let mut t = term_with_scrollback(2, 4, 100, 10_000);
    t.feed("あ".as_bytes());
    t.feed(b"\r\n\r\n"); // cursor to bottom, then scrolls row 0 out
    let snap = t.snapshot();
    assert_eq!(snap.scrollback().len(), 1);
    let line = &snap.scrollback()[0];
    assert_eq!(line.cells()[0].ch, 'あ');
    assert_eq!(line.cells()[0].width, 2);
    assert_eq!(line.cells()[1].ch, ' ');
    assert_eq!(line.cells()[1].width, 0);
    assert_eq!(line.cells()[2], muxnix::Cell::EMPTY);
}

#[test]
fn only_full_screen_primary_scroll_adds_history() {
    // Bottom-row LF with a full-screen region adds the displaced row.
    let mut t = term_with_scrollback(2, 4, 100, 10_000);
    t.feed(b"top\r\n");
    assert!(t.snapshot().scrollback().is_empty());
    t.feed(b"x\r\n");
    let snap = t.snapshot();
    assert_eq!(snap.scrollback().len(), 1);
    assert_eq!(line_text(&snap.scrollback()[0]), "top");
}

#[test]
fn partial_scroll_region_is_not_added_to_history() {
    let mut t = term_with_scrollback(4, 4, 100, 10_000);
    t.feed(b"aaaa\r\nbbbb\r\ncccc\r\ndddd");
    t.feed(b"\x1b[2;3r"); // scroll region rows 2-3
    t.feed(b"\x1b[3;1H\n"); // LF at region bottom scrolls within the region
    let snap = t.snapshot();
    assert!(snap.scrollback().is_empty());
    assert_eq!(text_at(&snap, 0), "aaaa");
}

#[test]
fn insert_lines_delete_lines_and_scroll_down_are_not_added() {
    let mut t = term_with_scrollback(4, 4, 100, 10_000);
    t.feed(b"aaaa\r\nbbbb\r\ncccc\r\ndddd");
    t.feed(b"\x1b[2;4r"); // region rows 2-4
    t.feed(b"\x1b[2;1H\x1b[L"); // IL at row 1 within the region
    assert!(t.snapshot().scrollback().is_empty());
    t.feed(b"\x1b[2;1H\x1b[M"); // DL at row 1 within the region
    assert!(t.snapshot().scrollback().is_empty());
    t.feed(b"\x1b[2;1H\x1b[T"); // SD scrolls the region down
    assert!(t.snapshot().scrollback().is_empty());
    t.feed(b"\x1b[2;1H\x1bM"); // RI scrolls the region down
    assert!(t.snapshot().scrollback().is_empty());
}

#[test]
fn resize_does_not_add_or_reflow_history() {
    let mut t = term_with_scrollback(3, 4, 100, 10_000);
    t.feed(b"aaaa\r\nbbbb\r\ncccc\r\ndddd");
    // After the fourth line the top row "aaaa" scrolled out.
    let before = t.snapshot().scrollback().len();
    assert_eq!(before, 1);
    t.resize(Size { rows: 2, cols: 4 });
    let snap = t.snapshot();
    assert_eq!(snap.scrollback().len(), 1);
    assert_eq!(line_text(&snap.scrollback()[0]), "aaaa");
    assert_eq!(snap.scrollback()[0].cells().len(), 4);
}

#[test]
fn alternate_screen_scrolls_into_visible_not_history() {
    let mut t = term_with_scrollback(3, 4, 100, 10_000);
    t.feed(b"aaaa\r\nbbbb\r\ncccc\r\ndddd");
    // Scrollback holds "aaaa" from the primary screen scroll.
    assert_eq!(t.snapshot().scrollback().len(), 1);
    t.feed(b"\x1b[?1049h");
    t.feed(b"xy\r\nzz\r\n");
    let snap = t.snapshot();
    assert!(snap.is_on_alternate_screen());
    assert_eq!(text_at(&snap, 0), "xy");
    assert_eq!(snap.scrollback().len(), 1);
    assert_eq!(line_text(&snap.scrollback()[0]), "aaaa");
    // Leaving the alternate screen does not move its rows into history.
    t.feed(b"\x1b[?1049l");
    let snap = t.snapshot();
    assert!(!snap.is_on_alternate_screen());
    assert_eq!(snap.scrollback().len(), 1);
}

#[test]
fn ed2_clears_visible_keeps_scrollback() {
    let mut t = term_with_scrollback(2, 4, 100, 10_000);
    t.feed(b"aaaa\r\nbbbb\r\ndddd"); // "aaaa" scrolled out
    assert_eq!(t.snapshot().scrollback().len(), 1);
    t.feed(b"\x1b[2J");
    let snap = t.snapshot();
    assert_eq!(text_at(&snap, 0), "");
    assert_eq!(text_at(&snap, 1), "");
    assert_eq!(snap.scrollback().len(), 1);
    assert_eq!(line_text(&snap.scrollback()[0]), "aaaa");
}

#[test]
fn ed3_clears_scrollback_keeps_visible() {
    let mut t = term_with_scrollback(2, 4, 100, 10_000);
    t.feed(b"aaaa\r\nbbbb\r\ndddd"); // "aaaa" scrolled out; row0=bbbb,row1=dddd
    assert_eq!(t.snapshot().scrollback().len(), 1);
    t.feed(b"\x1b[3J");
    let snap = t.snapshot();
    assert!(snap.scrollback().is_empty());
    assert_eq!(text_at(&snap, 0), "bbbb");
    assert_eq!(text_at(&snap, 1), "dddd");
}

#[test]
fn ris_clears_screen_and_scrollback() {
    let mut t = term_with_scrollback(2, 4, 100, 10_000);
    t.feed(b"aaaa\r\nbbbb\r\ndddd");
    t.feed(b"\x1b]2;gone\x07\x1b[?25l");
    assert_eq!(t.snapshot().scrollback().len(), 1);
    t.feed(b"\x1bc");
    let snap = t.snapshot();
    assert!(snap.scrollback().is_empty());
    assert_eq!(text_at(&snap, 0), "");
    assert_eq!(text_at(&snap, 1), "");
    assert_eq!(snap.title(), "");
    assert!(snap.modes().cursor_visible);
    assert!(!snap.is_on_alternate_screen());
}

#[test]
fn csi_su_with_count_saves_displaced_rows_top_to_bottom() {
    let mut t = term_with_scrollback(3, 4, 100, 10_000);
    t.feed(b"aaaa\r\nbbbb\r\ncccc");
    t.feed(b"\x1b[2S");
    let snap = t.snapshot();
    assert_eq!(snap.scrollback().len(), 2);
    assert_eq!(line_text(&snap.scrollback()[0]), "aaaa");
    assert_eq!(line_text(&snap.scrollback()[1]), "bbbb");
    // The remaining row moved to the top.
    assert_eq!(text_at(&snap, 0), "cccc");
}

#[test]
fn csi_su_count_at_least_screen_height_saves_all_rows_once() {
    let mut t = term_with_scrollback(3, 4, 100, 10_000);
    t.feed(b"aaaa\r\nbbbb\r\ncccc");
    t.feed(b"\x1b[5S");
    let snap = t.snapshot();
    assert_eq!(snap.scrollback().len(), 3);
    assert_eq!(line_text(&snap.scrollback()[0]), "aaaa");
    assert_eq!(line_text(&snap.scrollback()[1]), "bbbb");
    assert_eq!(line_text(&snap.scrollback()[2]), "cccc");
    assert_eq!(text_at(&snap, 0), "");
    assert_eq!(text_at(&snap, 1), "");
    assert_eq!(text_at(&snap, 2), "");
}

#[test]
fn default_terminal_state_has_disabled_scrollback() {
    let mut t = term(2, 4);
    t.feed(b"aaaa\r\nbbbb\r\ncccc\r\ndddd\neeee");
    assert!(t.snapshot().scrollback().is_empty());
}

#[test]
fn partially_zero_limits_are_invalid() {
    assert_eq!(ScrollbackLimits::new(0, 5), None);
    assert_eq!(ScrollbackLimits::new(5, 0), None);
    assert_eq!(
        ScrollbackLimits::new(0, 0),
        Some(ScrollbackLimits::DISABLED)
    );
    assert!(ScrollbackLimits::new(5, 50).is_some());
}

#[test]
fn eviction_drops_oldest_lines_first() {
    let mut t = term_with_scrollback(2, 4, 2, 100);
    t.feed(b"aaaa\r\nbbbb\r\ncccc\r\ndddd\r\neeee");
    let snap = t.snapshot();
    assert_eq!(snap.scrollback().len(), 2);
    assert_eq!(line_text(&snap.scrollback()[0]), "bbbb");
    assert_eq!(line_text(&snap.scrollback()[1]), "cccc");
}

#[test]
fn eviction_by_cell_count_keeps_newest_lines() {
    // Each line is 4 cells; max_cells 6 keeps exactly one line.
    let mut t = term_with_scrollback(2, 4, 100, 6);
    t.feed(b"aaaa\r\nbbbb\r\ncccc\r\ndddd\r\neeee");
    let snap = t.snapshot();
    assert_eq!(snap.scrollback().len(), 1);
    assert_eq!(line_text(&snap.scrollback()[0]), "cccc");
}

#[test]
fn single_line_over_max_cells_is_not_partially_stored() {
    // Each full line is 4 cells; max_cells 2 can never hold one, so nothing
    // is retained and no partial line appears.
    let mut t = term_with_scrollback(2, 4, 100, 2);
    t.feed(b"aaaa\r\nbbbb\r\ncccc\r\ndddd\r\neeee");
    let snap = t.snapshot();
    assert!(snap.scrollback().is_empty());
    assert_eq!(text_at(&snap, 1), "eeee");
}

#[test]
fn scrollback_lines_keep_full_row_width() {
    let mut t = term_with_scrollback(2, 6, 100, 10_000);
    t.feed(b"ab\r\n\r\n");
    let snap = t.snapshot();
    assert_eq!(snap.scrollback().len(), 1);
    let line = &snap.scrollback()[0];
    assert_eq!(line.cells().len(), 6);
    assert_eq!(line.len(), 6);
    assert!(!line.is_empty());
    assert_eq!(line_text(line), "ab");
}
