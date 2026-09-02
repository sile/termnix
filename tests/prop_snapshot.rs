//! Property tests for terminal snapshot and bounded scrollback.
//!
//! The oracle is a reference model that reproduces the emulator's behavior
//! for plain text, wide characters, CR, and LF only (autowrap on, default
//! full-screen scroll region, no cursor editing). The property compares the
//! real visible screen and scrollback against the reference, checks the
//! retained quantities against the limits, verifies that whole and chunked
//! feeding produce identical snapshots, and verifies that a snapshot taken
//! before further updates stays unchanged.
//!
//! Reproduction:
//! `MUXNIX_PROPTEST_SEED=<seed> cargo test --test prop_snapshot <name> -- --exact --nocapture`

use std::cell::Cell;

use termnix::{Position, ScrollbackLimits, Size, TerminalSnapshot, TerminalState};

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
    max_lines: usize,
    max_cells: usize,
    scrolls: usize,
    evicted: usize,
    dropped: usize,
    wraps: usize,
}

impl RefTerm {
    fn new(rows: usize, cols: usize, limits: ScrollbackLimits) -> Self {
        Self {
            rows,
            cols,
            row: 0,
            col: 0,
            wrap_pending: false,
            grid: vec![vec![RefCell { ch: ' ', width: 1 }; cols]; rows],
            scrollback: Vec::new(),
            max_lines: limits.max_lines,
            max_cells: limits.max_cells,
            scrolls: 0,
            evicted: 0,
            dropped: 0,
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
    ref_append_line(t, line);
    t.grid.push(vec![RefCell { ch: ' ', width: 1 }; t.cols]);
}

fn ref_append_line(t: &mut RefTerm, line: Vec<RefCell>) {
    let max_lines = t.max_lines;
    let max_cells = t.max_cells;
    if max_lines == 0 || max_cells == 0 {
        return;
    }
    let new_cells = line.len();
    if new_cells > max_cells {
        t.dropped += 1;
        return;
    }
    while !t.scrollback.is_empty() {
        let retained_lines = t.scrollback.len();
        let retained_cells: usize = t.scrollback.iter().map(Vec::len).sum();
        if retained_lines < max_lines && new_cells <= max_cells - retained_cells {
            break;
        }
        t.scrollback.remove(0);
        t.evicted += 1;
    }
    t.scrollback.push(line);
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

/// Draws scrollback limits across disabled, generous, line-bound,
/// cell-bound, and single-line-overflow settings so every eviction and drop
/// path stays reachable.
fn sample_limits(ctx: &mut noprop::TestCaseContext, cols: u16) -> ScrollbackLimits {
    const WEIGHTS: [u32; 5] = [1, 2, 3, 3, 2];
    match noprop::sample_weighted_index(ctx, &WEIGHTS) {
        0 => ScrollbackLimits::DISABLED,
        1 => {
            let lines = noprop::sample_usize_in(ctx, 2..=8);
            ScrollbackLimits {
                max_lines: lines,
                max_cells: lines * cols as usize * 2,
            }
        }
        2 => {
            let lines = noprop::sample_usize_in(ctx, 1..=3);
            ScrollbackLimits {
                max_lines: lines,
                max_cells: 10_000,
            }
        }
        3 => {
            let lines = noprop::sample_usize_in(ctx, 1..=8);
            let cells = noprop::sample_usize_in(ctx, 1..=cols as usize * 2);
            ScrollbackLimits {
                max_lines: lines,
                max_cells: cells,
            }
        }
        _ => {
            // max_cells below one full row, so every scrolled-out line is
            // dropped entirely.
            let cols = cols as usize;
            let cells = if cols > 1 {
                noprop::sample_usize_in(ctx, 1..cols)
            } else {
                1
            };
            ScrollbackLimits {
                max_lines: 4,
                max_cells: cells,
            }
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

fn feed_with_cuts(term: &mut TerminalState, bytes: &[u8], cuts: &[usize]) {
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
    snap: &TerminalSnapshot,
    reference: &RefTerm,
    rows: u16,
    cols: u16,
    desc: &str,
) {
    assert_eq!(
        snap.size(),
        Size::new(rows, cols).unwrap(),
        "{desc}; size mismatch"
    );
    assert!(
        !snap.is_on_alternate_screen(),
        "{desc}; alternate screen mismatch"
    );
    assert_eq!(
        snap.cursor(),
        Position {
            row: reference.row as u16,
            col: reference.col as u16
        },
        "{desc}; cursor mismatch"
    );
    for row in 0..rows as usize {
        for col in 0..cols as usize {
            let expected = reference.grid[row][col];
            let actual = snap
                .cell(Position {
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

fn assert_within_limits(snap: &TerminalSnapshot, limits: ScrollbackLimits, desc: &str) {
    let lines = snap.scrollback().len();
    let cells: usize = snap.scrollback().iter().map(|l| l.cells().len()).sum();
    if limits.is_disabled() {
        assert_eq!(lines, 0, "{desc}; disabled scrollback is not empty");
    } else {
        assert!(
            lines <= limits.max_lines,
            "{desc}; retained lines {lines} exceed max {}",
            limits.max_lines
        );
        assert!(
            cells <= limits.max_cells,
            "{desc}; retained cells {cells} exceed max {}",
            limits.max_cells
        );
    }
}

#[test]
fn snapshot_matches_reference_model() -> noprop::TestResult {
    let seed = noprop::seed_from_env_or_time("MUXNIX_PROPTEST_SEED")?;
    let saw_scroll = Cell::new(false);
    let saw_eviction = Cell::new(false);
    let saw_drop = Cell::new(false);
    let saw_disabled = Cell::new(false);
    let saw_wrap = Cell::new(false);
    let mut runner = noprop::Runner::new(seed);

    runner.run(CASE_BUDGET, |ctx| {
        let rows = sample_dimension(ctx, MAX_ROWS);
        let cols = sample_dimension(ctx, MAX_COLS);
        let limits = sample_limits(ctx, cols);
        if limits.is_disabled() {
            saw_disabled.set(true);
        }
        let tokens = sample_token_count(ctx);
        let mut input = Vec::new();
        let mut reference = RefTerm::new(rows as usize, cols as usize, limits);
        for _ in 0..tokens {
            let tok = sample_token(ctx);
            input.extend_from_slice(&token_bytes(&tok));
            apply_token(&mut reference, tok);
        }

        let desc = format!(
            "size={rows}x{cols} limits=(lines {}, cells {}) input=[{}] budget={CASE_BUDGET}",
            limits.max_lines,
            limits.max_cells,
            hex(&input),
        );

        let size = Size::new(rows, cols).unwrap();
        let mut whole = TerminalState::with_scrollback(size, limits);
        whole.feed(&input);

        let cuts = sample_cuts(ctx, input.len());
        let mut split = TerminalState::with_scrollback(size, limits);
        feed_with_cuts(&mut split, &input, &cuts);
        let snap_whole = whole.snapshot();
        let snap_split = split.snapshot();
        assert_eq!(
            snap_whole, snap_split,
            "{desc}; whole and split snapshots differ cuts={cuts:?}"
        );

        compare_snapshot_to_reference(&snap_whole, &reference, rows, cols, &desc);
        assert_within_limits(&snap_whole, limits, &desc);

        // A snapshot taken after a prefix is not changed by feeding the
        // suffix to the same state: it still equals a fresh prefix-only state.
        let cut = noprop::sample_usize_in(ctx, 0..=input.len());
        let (prefix, suffix) = input.split_at(cut);
        let mut prefix_term = TerminalState::with_scrollback(size, limits);
        prefix_term.feed(prefix);
        let snap_prefix = prefix_term.snapshot();
        prefix_term.feed(suffix);
        let mut fresh = TerminalState::with_scrollback(size, limits);
        fresh.feed(prefix);
        assert_eq!(
            snap_prefix,
            fresh.snapshot(),
            "{desc}; snapshot changed after the state was updated"
        );

        saw_scroll.set(saw_scroll.get() || reference.scrolls > 0);
        saw_eviction.set(saw_eviction.get() || reference.evicted > 0);
        saw_drop.set(saw_drop.get() || reference.dropped > 0);
        saw_wrap.set(saw_wrap.get() || reference.wraps > 0);

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
        saw_eviction.get(),
        "never evicted an oldest line; seed=0x{seed:016x}\n{runner}"
    );
    assert!(
        saw_drop.get(),
        "never dropped a line exceeding a limit; seed=0x{seed:016x}\n{runner}"
    );
    assert!(
        saw_disabled.get(),
        "never used disabled scrollback; seed=0x{seed:016x}\n{runner}"
    );
    assert!(
        saw_wrap.get(),
        "never wrapped a line; seed=0x{seed:016x}\n{runner}"
    );
    Ok(())
}
