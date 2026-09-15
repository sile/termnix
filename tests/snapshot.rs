//! Tests for `termnix::TerminalSnapshot` and scrollback trimming.
//!
//! Two layers share this file:
//!
//! - Deterministic example tests for the snapshot API (row access, ownership,
//!   scrollback contents, and the `trim_scrollback` bounds).
//! - A property test whose oracle is a reference model that reproduces the
//!   emulator's behavior for plain text, wide characters, CR, and LF only
//!   (autowrap on, default full-screen scroll region, no cursor editing). The
//!   property compares the real visible screen and scrollback against the
//!   reference before and after `trim_scrollback`, checks the trimmed
//!   quantities against the requested bounds, verifies that whole and chunked
//!   feeding produce identical snapshots, and verifies that a snapshot taken
//!   before further updates stays unchanged.
//!
//! Reproduction:
//! `MUXNIX_PROPTEST_SEED=<seed> cargo test --test snapshot <name> -- --exact --nocapture`

const MAX_ROWS: u16 = 8;
const MAX_COLS: u16 = 12;
const MAX_TOKENS: usize = 32;
const CASE_BUDGET: usize = 2048;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct RefCell {
    ch: char,
    width: u8,
}

#[derive(Debug, Clone, Copy)]
enum Token {
    Char(char),
    Wide(char),
    Cr,
    Lf,
}

fn token_bytes(tok: &Token) -> Vec<u8> {
    match tok {
        Token::Char(c) => vec![*c as u8],
        Token::Wide(c) => c.encode_utf8(&mut [0; 4]).as_bytes().to_vec(),
        Token::Cr => vec![b'\r'],
        Token::Lf => vec![b'\n'],
    }
}

struct RefTerm {
    rows: usize,
    cols: usize,
    row: usize,
    col: usize,
    wrap_pending: bool,
    grid: Vec<Vec<RefCell>>,
    scrollback: Vec<Vec<RefCell>>,
    scrolls: usize,
    wraps: usize,
}

impl RefTerm {
    fn new(rows: usize, cols: usize) -> Self {
        Self {
            rows,
            cols,
            row: 0,
            col: 0,
            wrap_pending: false,
            grid: vec![vec![RefCell { ch: ' ', width: 1 }; cols]; rows],
            scrollback: Vec::new(),
            scrolls: 0,
            wraps: 0,
        }
    }
}

fn ref_clear_cell(t: &mut RefTerm, row: usize, col: usize) {
    if col >= t.cols {
        return;
    }
    let existing = t.grid[row][col];
    if existing.width == 0 && col > 0 {
        t.grid[row][col - 1] = RefCell { ch: ' ', width: 1 };
    }
    if existing.width == 2 && col + 1 < t.cols {
        t.grid[row][col + 1] = RefCell { ch: ' ', width: 1 };
    }
    t.grid[row][col] = RefCell { ch: ' ', width: 1 };
}

fn ref_cr(t: &mut RefTerm) {
    t.wrap_pending = false;
    t.col = 0;
}

fn ref_lf(t: &mut RefTerm) {
    t.wrap_pending = false;
    if t.row + 1 >= t.rows {
        ref_scroll(t);
    } else {
        t.row += 1;
    }
}

fn ref_scroll(t: &mut RefTerm) {
    t.scrolls += 1;
    let line = t.grid.remove(0);
    t.scrollback.push(line);
    t.grid.push(vec![RefCell { ch: ' ', width: 1 }; t.cols]);
}

fn ref_print(t: &mut RefTerm, ch: char, width: u8) {
    if t.wrap_pending {
        t.wrap_pending = false;
        t.col = 0;
        ref_lf(t);
        t.wraps += 1;
    }
    if t.col + width as usize > t.cols {
        t.col = 0;
        ref_lf(t);
        t.wraps += 1;
    }
    if t.col + width as usize > t.cols {
        return;
    }
    ref_clear_cell(t, t.row, t.col);
    if width == 2 {
        ref_clear_cell(t, t.row, t.col + 1);
    }
    t.grid[t.row][t.col] = RefCell { ch, width };
    if width == 2 {
        t.grid[t.row][t.col + 1] = RefCell { ch: ' ', width: 0 };
    }
    let next = t.col + width as usize;
    if next >= t.cols {
        t.col = t.cols - 1;
        t.wrap_pending = true;
    } else {
        t.col = next;
        t.wrap_pending = false;
    }
}

fn apply_token(t: &mut RefTerm, tok: Token) {
    match tok {
        Token::Char(c) => ref_print(t, c, 1),
        Token::Wide(c) => ref_print(t, c, 2),
        Token::Cr => ref_cr(t),
        Token::Lf => ref_lf(t),
    }
}

/// Mirrors `termnix::TerminalState::trim_scrollback` on the reference model so the
/// trimmed histories can be compared cell for cell.
fn ref_trim_scrollback(t: &mut RefTerm, max_lines: usize, max_cells: usize) {
    if max_lines == 0 || max_cells == 0 {
        t.scrollback.clear();
        return;
    }
    let cells = |t: &RefTerm| -> usize { t.scrollback.iter().map(Vec::len).sum() };
    while !t.scrollback.is_empty() && (t.scrollback.len() > max_lines || cells(t) > max_cells) {
        t.scrollback.remove(0);
    }
}

/// Draws a row/column count with 1, 2, and the maximum as first-class
/// boundaries; values below 1 are never generated.
fn sample_dimension(ctx: &mut noprop::TestCaseContext, max: u16) -> u16 {
    noprop::sample_with_boundaries(ctx, &[1u16, 2, max], noprop::Ratio::one_nth(4), |ctx| {
        noprop::sample_usize_in(ctx, 1..=max as usize) as u16
    })
}

fn sample_token_count(ctx: &mut noprop::TestCaseContext) -> usize {
    noprop::sample_with_boundaries(
        ctx,
        &[0usize, 1, MAX_TOKENS],
        noprop::Ratio::one_nth(4),
        |ctx| noprop::sample_usize_in(ctx, 0..=MAX_TOKENS),
    )
}

fn sample_token(ctx: &mut noprop::TestCaseContext) -> Token {
    const WEIGHTS: [u32; 4] = [4, 3, 2, 2];
    match noprop::sample_weighted_index(ctx, &WEIGHTS) {
        0 => Token::Char(noprop::sample_ascii_printable_char(ctx)),
        1 => Token::Wide(noprop::sample_choice(ctx, &['あ', '漢', '世'])),
        2 => Token::Cr,
        _ => Token::Lf,
    }
}

/// Draws trim bounds across clear-all, generous, line-bound, cell-bound, and
/// tight settings so every trim path stays reachable.
fn sample_trim(ctx: &mut noprop::TestCaseContext, cols: u16) -> (usize, usize) {
    const WEIGHTS: [u32; 5] = [1, 2, 3, 3, 2];
    match noprop::sample_weighted_index(ctx, &WEIGHTS) {
        0 => (0, usize::MAX),
        1 => {
            let lines = noprop::sample_usize_in(ctx, 2..=8);
            (lines, lines * cols as usize * 2)
        }
        2 => {
            let lines = noprop::sample_usize_in(ctx, 1..=3);
            (lines, usize::MAX)
        }
        3 => {
            let lines = noprop::sample_usize_in(ctx, 1..=8);
            let cells = noprop::sample_usize_in(ctx, 1..=cols as usize * 2);
            (lines, cells)
        }
        _ => {
            // max_cells below one full row, so at most a partial history can
            // survive the trim.
            let cols = cols as usize;
            let cells = if cols > 1 {
                noprop::sample_usize_in(ctx, 1..cols)
            } else {
                1
            };
            (4, cells)
        }
    }
}

fn sample_cuts(ctx: &mut noprop::TestCaseContext, len: usize) -> Vec<usize> {
    if len == 0 {
        return Vec::new();
    }
    let count = noprop::sample_usize_in(ctx, 0..=len.min(8));
    let mut cuts = Vec::with_capacity(count);
    for _ in 0..count {
        cuts.push(noprop::sample_usize_in(ctx, 0..=len));
    }
    cuts.sort_unstable();
    cuts.dedup();
    cuts
}

fn feed_with_cuts(term: &mut termnix::TerminalState, bytes: &[u8], cuts: &[usize]) {
    let mut start = 0;
    for &cut in cuts {
        if cut > start && cut < bytes.len() {
            term.feed(&bytes[start..cut]);
            start = cut;
        }
    }
    term.feed(&bytes[start..]);
}

fn hex(bytes: &[u8]) -> String {
    let mut out = String::new();
    for (i, byte) in bytes.iter().enumerate() {
        if i > 0 {
            out.push(' ');
        }
        out.push_str(&format!("{byte:02x}"));
    }
    out
}

fn compare_snapshot_to_reference(
    snap: &termnix::TerminalSnapshot,
    reference: &RefTerm,
    rows: u16,
    cols: u16,
    desc: &str,
) {
    assert_eq!(
        snap.size(),
        termnix::Size::new(rows, cols).expect("nonzero size"),
        "{desc}; size mismatch"
    );
    assert!(
        !snap.is_on_alternate_screen(),
        "{desc}; alternate screen mismatch"
    );
    assert_eq!(
        snap.cursor(),
        termnix::Position {
            row: reference.row as u16,
            col: reference.col as u16
        },
        "{desc}; cursor mismatch"
    );
    for row in 0..rows as usize {
        for col in 0..cols as usize {
            let expected = reference.grid[row][col];
            let actual = snap
                .cell(termnix::Position {
                    row: row as u16,
                    col: col as u16,
                })
                .expect("cell in range");
            assert_eq!(
                actual.ch, expected.ch,
                "{desc}; cell({row},{col}) glyph mismatch"
            );
            assert_eq!(
                actual.width, expected.width,
                "{desc}; cell({row},{col}) width mismatch"
            );
        }
    }
    assert_eq!(
        snap.scrollback().len(),
        reference.scrollback.len(),
        "{desc}; scrollback length mismatch"
    );
    for (actual_line, expected_line) in snap.scrollback().iter().zip(&reference.scrollback) {
        assert_eq!(
            actual_line.cells().len(),
            expected_line.len(),
            "{desc}; scrollback line length mismatch"
        );
        for (a, e) in actual_line.cells().iter().zip(expected_line) {
            assert_eq!(a.ch, e.ch, "{desc}; scrollback glyph mismatch");
            assert_eq!(a.width, e.width, "{desc}; scrollback width mismatch");
        }
    }
}

fn assert_within_bounds(
    snap: &termnix::TerminalSnapshot,
    max_lines: usize,
    max_cells: usize,
    desc: &str,
) {
    let lines = snap.scrollback().len();
    let cells: usize = snap.scrollback().iter().map(|l| l.cells().len()).sum();
    if max_lines == 0 || max_cells == 0 {
        assert_eq!(lines, 0, "{desc}; zero bound did not clear scrollback");
    } else {
        assert!(
            lines <= max_lines,
            "{desc}; retained lines {lines} exceed max {max_lines}"
        );
        assert!(
            cells <= max_cells,
            "{desc}; retained cells {cells} exceed max {max_cells}"
        );
    }
}

#[test]
fn snapshot_matches_reference_model() -> noprop::TestResult {
    let seed = noprop::seed_from_env_or_time("MUXNIX_PROPTEST_SEED")?;
    let saw_scroll = std::cell::Cell::new(false);
    let saw_wrap = std::cell::Cell::new(false);
    let saw_trim = std::cell::Cell::new(false);
    let saw_clear = std::cell::Cell::new(false);
    let mut runner = noprop::Runner::new(seed);

    runner.run(CASE_BUDGET, |ctx| {
        let rows = sample_dimension(ctx, MAX_ROWS);
        let cols = sample_dimension(ctx, MAX_COLS);
        let tokens = sample_token_count(ctx);
        let mut input = Vec::new();
        let mut reference = RefTerm::new(rows as usize, cols as usize);
        for _ in 0..tokens {
            let tok = sample_token(ctx);
            input.extend_from_slice(&token_bytes(&tok));
            apply_token(&mut reference, tok);
        }

        let (max_lines, max_cells) = sample_trim(ctx, cols);
        let desc = format!(
            "size={rows}x{cols} trim=(lines {max_lines}, cells {max_cells}) input=[{}] budget={CASE_BUDGET}",
            hex(&input),
        );

        let size = termnix::Size::new(rows, cols).expect("nonzero size");
        let mut whole = termnix::TerminalState::new(size);
        whole.feed(&input);

        let cuts = sample_cuts(ctx, input.len());
        let mut split = termnix::TerminalState::new(size);
        feed_with_cuts(&mut split, &input, &cuts);
        let snap_whole = whole.snapshot();
        let snap_split = split.snapshot();
        assert_eq!(
            snap_whole, snap_split,
            "{desc}; whole and split snapshots differ cuts={cuts:?}"
        );

        compare_snapshot_to_reference(&snap_whole, &reference, rows, cols, &desc);

        // Trimming must match the reference model and satisfy the bounds.
        let pre_trim_len = whole.scrollback_len();
        whole.trim_scrollback(max_lines, max_cells);
        ref_trim_scrollback(&mut reference, max_lines, max_cells);
        let snap_trimmed = whole.snapshot();
        compare_snapshot_to_reference(&snap_trimmed, &reference, rows, cols, &desc);
        assert_within_bounds(&snap_trimmed, max_lines, max_cells, &desc);
        // The cell counter stays consistent with the retained lines.
        let cells: usize = snap_trimmed
            .scrollback()
            .iter()
            .map(|l| l.cells().len())
            .sum();
        assert_eq!(
            whole.scrollback_cells(),
            cells,
            "{desc}; scrollback_cells mismatch"
        );
        assert_eq!(
            whole.scrollback_len(),
            snap_trimmed.scrollback().len(),
            "{desc}; scrollback_len mismatch"
        );

        // A snapshot taken after a prefix is not changed by feeding the
        // suffix to the same state: it still equals a fresh prefix-only state.
        let cut = noprop::sample_usize_in(ctx, 0..=input.len());
        let (prefix, suffix) = input.split_at(cut);
        let mut prefix_term = termnix::TerminalState::new(size);
        prefix_term.feed(prefix);
        let snap_prefix = prefix_term.snapshot();
        prefix_term.feed(suffix);
        let mut fresh = termnix::TerminalState::new(size);
        fresh.feed(prefix);
        assert_eq!(
            snap_prefix,
            fresh.snapshot(),
            "{desc}; snapshot changed after the state was updated"
        );

        saw_scroll.set(saw_scroll.get() || reference.scrolls > 0);
        saw_wrap.set(saw_wrap.get() || reference.wraps > 0);
        saw_trim.set(saw_trim.get() || whole.scrollback_len() < pre_trim_len);
        saw_clear.set(saw_clear.get() || max_lines == 0 || max_cells == 0);

        Ok(())
    })?;

    assert!(
        runner.stats().rejected_cases == 0,
        "valid-by-construction generator rejected cases\n{runner}"
    );
    assert!(
        saw_scroll.get(),
        "never scrolled a row into scrollback; seed=0x{seed:016x}\n{runner}"
    );
    assert!(
        saw_wrap.get(),
        "never wrapped a line; seed=0x{seed:016x}\n{runner}"
    );
    assert!(
        saw_trim.get(),
        "never removed a line with trim_scrollback; seed=0x{seed:016x}\n{runner}"
    );
    assert!(
        saw_clear.get(),
        "never used a zero trim bound; seed=0x{seed:016x}\n{runner}"
    );
    Ok(())
}

fn term(rows: u16, cols: u16) -> termnix::TerminalState {
    termnix::TerminalState::new(termnix::Size::new(rows, cols).expect("nonzero size"))
}

fn text_at(snap: &termnix::TerminalSnapshot, row: u16) -> String {
    let cols = snap.size().cols.get();
    let mut out = String::new();
    for col in 0..cols {
        let cell = snap
            .cell(termnix::Position { row, col })
            .expect("cell in range");
        if cell.width == 0 {
            continue;
        }
        out.push(cell.ch);
    }
    out.trim_end().to_string()
}

fn line_text(line: &termnix::TerminalLine) -> String {
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
fn rows_match_size_and_cell_access() {
    let mut t = term(3, 4);
    t.feed(b"ab\x1b[1;31mZ\r\ncd\r\nef");
    let snap = t.snapshot();

    let rows: Vec<&[termnix::Cell]> = snap.rows().collect();
    assert_eq!(rows.len(), snap.size().rows.get() as usize);
    for row in &rows {
        assert_eq!(row.len(), snap.size().cols.get() as usize);
    }

    // Every cell reachable by `rows()` matches `cell(Position)`.
    for (r, row) in rows.iter().enumerate() {
        for (c, cell) in row.iter().enumerate() {
            let at = termnix::Position {
                row: r as u16,
                col: c as u16,
            };
            assert_eq!(*cell, snap.cell(at).expect("cell in range"));
        }
    }
}

#[test]
fn row_returns_slices_and_rejects_out_of_range() {
    let mut t = term(2, 4);
    t.feed(b"ab\r\ncd");
    let snap = t.snapshot();

    assert_eq!(snap.row(0), snap.rows().next());
    assert_eq!(snap.row(1), snap.rows().nth(1));
    assert_eq!(snap.row(0).expect("row 0")[0].ch, 'a');
    assert_eq!(snap.row(1).expect("row 1")[0].ch, 'c');
    assert_eq!(snap.row(2), None);
    assert_eq!(snap.row(u16::MAX), None);
}

#[test]
fn rows_work_for_single_row_and_column() {
    let mut t = term(1, 1);
    t.feed(b"x");
    let snap = t.snapshot();
    let rows: Vec<&[termnix::Cell]> = snap.rows().collect();
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].len(), 1);
    assert_eq!(rows[0][0].ch, 'x');
    assert_eq!(snap.row(0), Some(&[rows[0][0]][..]));
    assert_eq!(snap.row(1), None);
}

#[test]
fn snapshot_owns_size_cells_cursor_modes_style_title_and_active() {
    let mut t = term(2, 4);
    t.feed(b"ab\x1b[1;31mZ");
    t.feed(b"\x1b[2;1H\x1b]2;snap-title\x07\x1b[?25l");
    let snap = t.snapshot();

    assert_eq!(snap.size(), termnix::Size::new(2, 4).expect("nonzero size"));
    assert_eq!(snap.size(), t.size());
    // Row-major cells: rows * cols count.
    assert_eq!(
        snap.cell(termnix::Position { row: 0, col: 0 }),
        t.cell(termnix::Position { row: 0, col: 0 })
    );
    assert_eq!(
        snap.cell(termnix::Position { row: 0, col: 1 }),
        t.cell(termnix::Position { row: 0, col: 1 })
    );
    assert_eq!(
        snap.cell(termnix::Position { row: 1, col: 3 }),
        t.cell(termnix::Position { row: 1, col: 3 })
    );
    assert_eq!(snap.cell(termnix::Position { row: 2, col: 0 }), None);
    assert_eq!(snap.cell(termnix::Position { row: 0, col: 4 }), None);

    assert_eq!(snap.cursor(), t.cursor());
    assert_eq!(snap.modes(), t.modes());
    assert!(!snap.modes().cursor_visible);
    assert_eq!(snap.style(), t.style());
    assert_eq!(snap.title(), t.title());
    assert_eq!(snap.title(), "snap-title");
    assert!(!snap.is_on_alternate_screen());
    assert!(snap.scrollback().is_empty());
}

#[test]
fn snapshot_is_owned_and_unaffected_by_later_updates() {
    let mut t = term(2, 4);
    t.feed(b"aa\r\nbb\r\ncc\r\ndd");
    let snap = t.snapshot();
    t.feed(b"ee\r\nff");

    // The snapshot taken earlier still equals a fresh term fed only the
    // original prefix; updating `t` did not mutate it.
    let mut fresh = term(2, 4);
    fresh.feed(b"aa\r\nbb\r\ncc\r\ndd");
    assert_eq!(snap, fresh.snapshot());
    assert_ne!(snap, t.snapshot());
}

#[test]
fn wide_character_lead_and_continuation_survive_in_snapshot() {
    let mut t = term(2, 4);
    t.feed("あ".as_bytes());
    t.feed(b"\r\n\r\n"); // cursor to bottom, then scrolls row 0 out
    let snap = t.snapshot();
    assert_eq!(snap.scrollback().len(), 1);
    let line = &snap.scrollback()[0];
    assert_eq!(line.cells()[0].ch, 'あ');
    assert_eq!(line.cells()[0].width, 2);
    assert_eq!(line.cells()[1].ch, ' ');
    assert_eq!(line.cells()[1].width, 0);
    assert_eq!(line.cells()[2], termnix::Cell::EMPTY);
}

#[test]
fn only_full_screen_primary_scroll_adds_history() {
    // Bottom-row LF with a full-screen region adds the displaced row.
    let mut t = term(2, 4);
    t.feed(b"top\r\n");
    assert!(t.snapshot().scrollback().is_empty());
    t.feed(b"x\r\n");
    let snap = t.snapshot();
    assert_eq!(snap.scrollback().len(), 1);
    assert_eq!(line_text(&snap.scrollback()[0]), "top");
}

#[test]
fn partial_scroll_region_is_not_added_to_history() {
    let mut t = term(4, 4);
    t.feed(b"aaaa\r\nbbbb\r\ncccc\r\ndddd");
    t.feed(b"\x1b[2;3r"); // scroll region rows 2-3
    t.feed(b"\x1b[3;1H\n"); // LF at region bottom scrolls within the region
    let snap = t.snapshot();
    assert!(snap.scrollback().is_empty());
    assert_eq!(text_at(&snap, 0), "aaaa");
}

#[test]
fn insert_lines_delete_lines_and_scroll_down_are_not_added() {
    let mut t = term(4, 4);
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
    let mut t = term(3, 4);
    t.feed(b"aaaa\r\nbbbb\r\ncccc\r\ndddd");
    // After the fourth line the top row "aaaa" scrolled out.
    let before = t.snapshot().scrollback().len();
    assert_eq!(before, 1);
    t.resize(termnix::Size::new(2, 4).expect("nonzero size"));
    let snap = t.snapshot();
    assert_eq!(snap.scrollback().len(), 1);
    assert_eq!(line_text(&snap.scrollback()[0]), "aaaa");
    assert_eq!(snap.scrollback()[0].cells().len(), 4);
}

#[test]
fn alternate_screen_scrolls_into_visible_not_history() {
    let mut t = term(3, 4);
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
    let mut t = term(2, 4);
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
fn ed3_clears_scrollback_keeps_visible_and_resets_cell_count() {
    let mut t = term(2, 4);
    t.feed(b"aaaa\r\nbbbb\r\ndddd"); // "aaaa" scrolled out; row0=bbbb,row1=dddd
    assert_eq!(t.snapshot().scrollback().len(), 1);
    assert_eq!(t.scrollback_cells(), 4);
    t.feed(b"\x1b[3J");
    let snap = t.snapshot();
    assert!(snap.scrollback().is_empty());
    assert_eq!(t.scrollback_cells(), 0);
    assert_eq!(text_at(&snap, 0), "bbbb");
    assert_eq!(text_at(&snap, 1), "dddd");
}

#[test]
fn ris_clears_screen_and_scrollback() {
    let mut t = term(2, 4);
    t.feed(b"aaaa\r\nbbbb\r\ndddd");
    t.feed(b"\x1b]2;gone\x07\x1b[?25l");
    assert_eq!(t.snapshot().scrollback().len(), 1);
    t.feed(b"\x1bc");
    let snap = t.snapshot();
    assert!(snap.scrollback().is_empty());
    assert_eq!(t.scrollback_cells(), 0);
    assert_eq!(text_at(&snap, 0), "");
    assert_eq!(text_at(&snap, 1), "");
    assert_eq!(snap.title(), "");
    assert!(snap.modes().cursor_visible);
    assert!(!snap.is_on_alternate_screen());
}

#[test]
fn csi_su_with_count_saves_displaced_rows_top_to_bottom() {
    let mut t = term(3, 4);
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
    let mut t = term(3, 4);
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
fn history_is_retained_without_a_built_in_limit() {
    let mut t = term(2, 4);
    t.feed(b"aaaa\r\nbbbb\r\ncccc\r\ndddd\r\neeee");
    let snap = t.snapshot();
    assert_eq!(snap.scrollback().len(), 3);
    assert_eq!(line_text(&snap.scrollback()[0]), "aaaa");
    assert_eq!(line_text(&snap.scrollback()[1]), "bbbb");
    assert_eq!(line_text(&snap.scrollback()[2]), "cccc");
}

#[test]
fn trim_scrollback_removes_oldest_lines_to_line_bound() {
    let mut t = term(2, 4);
    t.feed(b"aaaa\r\nbbbb\r\ncccc\r\ndddd\r\neeee");
    assert_eq!(t.scrollback_len(), 3);
    t.trim_scrollback(2, usize::MAX);
    assert_eq!(t.scrollback_len(), 2);
    let snap = t.snapshot();
    assert_eq!(line_text(&snap.scrollback()[0]), "bbbb");
    assert_eq!(line_text(&snap.scrollback()[1]), "cccc");
}

#[test]
fn trim_scrollback_removes_oldest_lines_to_cell_bound() {
    let mut t = term(2, 4);
    t.feed(b"aaaa\r\nbbbb\r\ncccc\r\ndddd\r\neeee");
    // Each line is 4 cells; max_cells 6 keeps exactly one line.
    t.trim_scrollback(usize::MAX, 6);
    assert_eq!(t.scrollback_len(), 1);
    assert_eq!(t.scrollback_cells(), 4);
    let snap = t.snapshot();
    assert_eq!(line_text(&snap.scrollback()[0]), "cccc");
}

#[test]
fn trim_scrollback_satisfies_both_bounds_and_keeps_cell_count_consistent() {
    let mut t = term(2, 4);
    t.feed(b"aaaa\r\nbbbb\r\ncccc\r\ndddd\r\neeee");
    t.trim_scrollback(1, 6);
    assert_eq!(t.scrollback_len(), 1);
    assert_eq!(t.scrollback_cells(), 4);
    let snap = t.snapshot();
    assert_eq!(line_text(&snap.scrollback()[0]), "cccc");
}

#[test]
fn trim_scrollback_with_zero_clears_everything() {
    let mut t = term(2, 4);
    t.feed(b"aaaa\r\nbbbb\r\ncccc");
    t.trim_scrollback(0, usize::MAX);
    assert!(t.snapshot().scrollback().is_empty());
    assert_eq!(t.scrollback_cells(), 0);
    t.feed(b"xxxx\r\nyyyy");
    t.trim_scrollback(usize::MAX, 0);
    assert!(t.snapshot().scrollback().is_empty());
    assert_eq!(t.scrollback_cells(), 0);
}

#[test]
fn trim_scrollback_keeps_newest_lines_whole() {
    let mut t = term(2, 4);
    t.feed(b"aaaa\r\nbbbb\r\ncccc\r\ndddd\r\neeee");
    t.trim_scrollback(2, usize::MAX);
    let snap = t.snapshot();
    // Newest content survives; no line is partially retained.
    assert_eq!(snap.scrollback()[0].cells().len(), 4);
    assert_eq!(snap.scrollback()[1].cells().len(), 4);
    assert_eq!(text_at(&snap, 1), "eeee");
}

#[test]
fn scrollback_lines_keep_full_row_width() {
    let mut t = term(2, 6);
    t.feed(b"ab\r\n\r\n");
    let snap = t.snapshot();
    assert_eq!(snap.scrollback().len(), 1);
    let line = &snap.scrollback()[0];
    assert_eq!(line.cells().len(), 6);
    assert_eq!(line.len(), 6);
    assert!(!line.is_empty());
    assert_eq!(line_text(line), "ab");
}
