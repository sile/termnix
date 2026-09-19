//! Tests for `termnix::TerminalState`.
//!
//! Two layers share this file:
//!
//! - Deterministic example tests for the public emulator API (text,
//!   controls, wide characters, resizing, private modes, actions, and the
//!   visible-state revision counter).
//! - Property tests for chunk-independent feeding and parser continuation
//!   equivalence. The oracle is a metamorphic relation: the same logical byte
//!   stream fed through different feed partitions must leave observationally
//!   equivalent terminal states, both right after a prefix and after every
//!   chunk of a common suffix. The only guaranteed partition difference lies
//!   strictly inside a completed target sequence; the prefix/suffix boundary
//!   itself is never the differing cut.
//!
//! Reproduction:
//! `MUXNIX_PROPTEST_SEED=<seed> cargo test --test terminal <name> -- --exact --nocapture`

const MAX_ROWS: u16 = 8;
const MAX_COLS: u16 = 16;
const MAX_EXTRA_CUTS: usize = 6;
const MAX_SUFFIX_CUTS: usize = 12;
const MAX_TOKENS: usize = 8;
const CASE_BUDGET: usize = 1024;

/// Printable marker appended after a scenario's completion. It must be
/// visible on the active screen once the parser is back in ground state.
const SENTINEL: &[u8] = b"#";

// Scenario weights, kept in one place; none may be zero.
//
// Miss probability estimates for the coverage gates below. Each gate is
// reached by the query scenario with p >= 2/15 (its arm weight) plus extra
// contributions from the general and csi arms, so a run of CASE_BUDGET cases
// misses a gate with probability at most (1 - 2/15)^1024 ~ e^-137 ~ 0.
const SCENARIO_WEIGHTS: [u32; 7] = [3, 2, 2, 2, 2, 2, 2];

struct Case {
    name: &'static str,
    prefix: Vec<u8>,
    target_cut: usize,
    suffix: Vec<u8>,
    expect_prefix_action: bool,
    expect_suffix_action: bool,
    expect_title: Option<&'static str>,
    expect_cursor_visible: Option<bool>,
    expect_glyph: Option<char>,
}

/// Draws a row/column count with 1, 2, and the maximum as first-class
/// boundaries. Values below 1 are never generated, so no later clamping hides
/// a zero-size difference between the two feed paths.
fn sample_dimension(ctx: &mut noprop::TestCaseContext, max: u16) -> u16 {
    noprop::sample_with_boundaries(ctx, &[1u16, 2, max], noprop::Ratio::one_nth(4), |ctx| {
        noprop::sample_usize_in(ctx, 1..=max as usize) as u16
    })
}

/// Strict interior cut `0 < cut < target.len()`. Every scenario guarantees
/// this cut exists so whole and split really feed the target through
/// different call boundaries.
fn sample_strict_interior_cut(ctx: &mut noprop::TestCaseContext, target: &[u8]) -> usize {
    debug_assert!(target.len() >= 2);
    noprop::sample_usize_in(ctx, 1..target.len())
}

/// Bounded token count with 0, 1, and the maximum as boundaries.
fn sample_token_count(ctx: &mut noprop::TestCaseContext) -> usize {
    noprop::sample_with_boundaries(
        ctx,
        &[0usize, 1, MAX_TOKENS],
        noprop::Ratio::one_nth(4),
        |ctx| noprop::sample_usize_in(ctx, 0..=MAX_TOKENS),
    )
}

/// Cut positions for the split side of the suffix, applied identically to both
/// states. Every-byte chunks and a single whole chunk are explicit branches so
/// both extremes stay reachable.
fn sample_suffix_cuts(ctx: &mut noprop::TestCaseContext, len: usize) -> Vec<usize> {
    if len <= 1 {
        return Vec::new();
    }
    match noprop::sample_weighted_index(ctx, &[1, 1, 4]) {
        0 => (1..len).collect(),
        1 => Vec::new(),
        _ => {
            let cap = (len - 1).min(MAX_SUFFIX_CUTS);
            let count = noprop::sample_with_boundaries(
                ctx,
                &[1usize, 2, cap],
                noprop::Ratio::one_nth(4),
                |ctx| noprop::sample_usize_in(ctx, 1..=cap),
            );
            let mut positions: Vec<usize> = Vec::with_capacity(count);
            for _ in 0..count {
                positions.push(noprop::sample_usize_in(ctx, 1..len));
            }
            positions.sort_unstable();
            positions.dedup();
            positions
        }
    }
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

/// Compares every public observable of two states, drains and compares the
/// action queues (contents and order), then compares the full emulator state
/// via `PartialEq` (which covers saved cursor, wrap pending, scroll region,
/// and the inactive screen, but not the parser's private continuation state).
fn drain_and_compare(
    a: &mut termnix::TerminalState,
    b: &mut termnix::TerminalState,
    where_: &str,
) -> Vec<termnix::TerminalAction> {
    let size = a.size();
    assert_eq!(size, b.size(), "{where_}: size mismatch");
    assert_eq!(a.cursor(), b.cursor(), "{where_}: cursor mismatch");
    for row in 0..size.rows.get() {
        for col in 0..size.cols.get() {
            let at = termnix::Position { row, col };
            assert_eq!(a.cell(at), b.cell(at), "{where_}: cell mismatch at {at:?}");
        }
    }
    assert_eq!(a.modes(), b.modes(), "{where_}: modes mismatch");
    assert_eq!(
        a.is_on_alternate_screen(),
        b.is_on_alternate_screen(),
        "{where_}: alternate screen mismatch"
    );
    assert_eq!(a.title(), b.title(), "{where_}: title mismatch");
    assert_eq!(a.style(), b.style(), "{where_}: style mismatch");
    let actions_a = a.drain_actions();
    let actions_b = b.drain_actions();
    assert_eq!(actions_a, actions_b, "{where_}: action mismatch");
    assert_eq!(a, b, "{where_}: internal emulator state mismatch");
    actions_a
}

fn sentinel_visible(term: &termnix::TerminalState) -> bool {
    let size = term.size();
    for row in 0..size.rows.get() {
        for col in 0..size.cols.get() {
            if let Some(cell) = term.cell(termnix::Position { row, col })
                && cell.ch == '#'
            {
                return true;
            }
        }
    }
    false
}

fn glyph_visible(term: &termnix::TerminalState, glyph: char) -> bool {
    let size = term.size();
    for row in 0..size.rows.get() {
        for col in 0..size.cols.get() {
            if let Some(cell) = term.cell(termnix::Position { row, col })
                && cell.ch == glyph
            {
                return true;
            }
        }
    }
    false
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

/// Completes a sequence interrupted by the prefix. Returns the probe, its
/// completion, and whether the completion produces a query reply action.
const GENERAL_PROBES: &[(&[u8], &[u8])] = &[
    (b"\x1b", b"M"),
    (b"\x1b[", b"6n"),
    (b"\x1b[0", b"c"),
    (b"\x1b[", b"?25l"),
    (b"\x1b[", b"K"),
    (b"\x1b]2;probe", b"\x07"),
    (&[0xe3, 0x81], b"\x82"),
    (b"\xff", b"X"),
];
fn completion_is_query(completion: &[u8]) -> bool {
    completion == b"6n" || completion == b"0c" || completion == b"c"
}

/// General stream: printable ASCII, C0 controls, valid and invalid UTF-8,
/// complete CSI/OSC/DEC private mode/query tokens, with a completed target
/// carrying the guaranteed strict cut and an incomplete probe at the end.
///
/// Class weights below keep every sequence class reachable within the case
/// budget instead of relying on uniform token sampling.
fn gen_general(ctx: &mut noprop::TestCaseContext) -> Case {
    let mut prefix = Vec::new();
    let mut prefix_has_query = false;
    let head_tokens = sample_token_count(ctx);
    for _ in 0..head_tokens {
        let (tok, query) = sample_general_token(ctx);
        prefix_has_query |= query;
        prefix.extend_from_slice(tok);
    }
    let (target, query) = sample_general_target(ctx);
    prefix_has_query |= query;
    let target_cut = sample_strict_interior_cut(ctx, target);
    prefix.extend_from_slice(target);
    let tail_tokens = sample_token_count(ctx);
    for _ in 0..tail_tokens {
        let (tok, query) = sample_general_token(ctx);
        prefix_has_query |= query;
        prefix.extend_from_slice(tok);
    }
    let (probe, completion) = noprop::sample_choice(ctx, GENERAL_PROBES);
    prefix.extend_from_slice(probe);

    let mut suffix = completion.to_vec();
    suffix.extend_from_slice(SENTINEL);
    Case {
        name: "general",
        prefix,
        target_cut,
        suffix,
        expect_prefix_action: prefix_has_query,
        expect_suffix_action: completion_is_query(completion),
        expect_title: None,
        expect_cursor_visible: None,
        expect_glyph: None,
    }
}

/// One general-stream token chosen by class weight (text 4, control 2, utf8 2,
/// csi 2, dec-mode 2, osc 1, query 2, invalid 2) so every class stays
/// reachable within the case budget.
fn sample_general_token(ctx: &mut noprop::TestCaseContext) -> (&'static [u8], bool) {
    const CLASS_WEIGHTS: [u32; 8] = [4, 2, 2, 2, 2, 1, 2, 2];
    match noprop::sample_weighted_index(ctx, &CLASS_WEIGHTS) {
        0 => (
            noprop::sample_choice(ctx, &[b"abc", b"XYZ", b" ", b"123"]),
            false,
        ),
        1 => (
            noprop::sample_choice(ctx, &[b"\x0a", b"\x0d", b"\x09", b"\x07"]),
            false,
        ),
        2 => (
            noprop::sample_choice(ctx, &["あ".as_bytes(), "漢".as_bytes()]),
            false,
        ),
        3 => (
            noprop::sample_choice(
                ctx,
                &[b"\x1b[1;1H", b"\x1b[2J", b"\x1b[K", b"\x1b[1m", b"\x1b[0m"],
            ),
            false,
        ),
        4 => (
            noprop::sample_choice(
                ctx,
                &[b"\x1b[?25h", b"\x1b[?25l", b"\x1b[?1049h", b"\x1b[?1049l"],
            ),
            false,
        ),
        5 => (b"\x1b]2;gen\x07", false),
        6 => {
            let query = noprop::sample_choice(
                ctx,
                &[
                    b"\x1b[6n".as_slice(),
                    b"\x1b[5n".as_slice(),
                    b"\x1b[0c".as_slice(),
                    b"\x1b[c".as_slice(),
                ],
            );
            (query, true)
        }
        _ => (noprop::sample_choice(ctx, &[b"\xff", b"\xc0\xaf"]), false),
    }
}

/// A completed general-stream target (length >= 2 so a strict interior cut is
/// always possible), chosen by class weight (csi 3, dec-mode 2, query 2, utf8 2,
/// osc 1, esc/dcs 2, invalid 2).
fn sample_general_target(ctx: &mut noprop::TestCaseContext) -> (&'static [u8], bool) {
    const CLASS_WEIGHTS: [u32; 7] = [3, 2, 2, 2, 1, 2, 2];
    match noprop::sample_weighted_index(ctx, &CLASS_WEIGHTS) {
        0 => (
            noprop::sample_choice(
                ctx,
                &[b"\x1b[1;1H", b"\x1b[2J", b"\x1b[K", b"\x1b[1m", b"\x1b[0m"],
            ),
            false,
        ),
        1 => (
            noprop::sample_choice(
                ctx,
                &[b"\x1b[?25h", b"\x1b[?25l", b"\x1b[?1049h", b"\x1b[?1049l"],
            ),
            false,
        ),
        2 => {
            let query = noprop::sample_choice(
                ctx,
                &[
                    b"\x1b[6n".as_slice(),
                    b"\x1b[5n".as_slice(),
                    b"\x1b[0c".as_slice(),
                    b"\x1b[c".as_slice(),
                ],
            );
            (query, true)
        }
        3 => (
            noprop::sample_choice(ctx, &["あ".as_bytes(), "漢".as_bytes()]),
            false,
        ),
        4 => (b"\x1b]2;g\x07", false),
        5 => (
            noprop::sample_choice(ctx, &[b"\x1bM", b"\x1bc", b"\x1bP1;2|g\x1b\\"]),
            false,
        ),
        _ => (b"\xff\xff", false),
    }
}

/// Prefix cut right after ESC, suffix completing the escape sequence.
fn gen_escape(ctx: &mut noprop::TestCaseContext) -> Case {
    const TARGETS: &[&[u8]] = &[b"\x1b7", b"\x1b8", b"\x1bM", b"\x1bD", b"\x1bc"];
    const COMPLETIONS: &[u8] = b"7MD8c";
    let target = noprop::sample_choice(ctx, TARGETS);
    let target_cut = sample_strict_interior_cut(ctx, target);
    let completion = noprop::sample_choice(ctx, COMPLETIONS);
    let mut prefix = target.to_vec();
    prefix.extend_from_slice(b"\x1b");
    let mut suffix = vec![completion];
    suffix.extend_from_slice(SENTINEL);
    Case {
        name: "escape",
        prefix,
        target_cut,
        suffix,
        expect_prefix_action: false,
        expect_suffix_action: false,
        expect_title: None,
        expect_cursor_visible: None,
        expect_glyph: None,
    }
}

/// Prefix cut inside a CSI (including DEC private mode and query forms),
/// suffix providing the final byte.
fn gen_csi(ctx: &mut noprop::TestCaseContext) -> Case {
    const TARGETS: &[&[u8]] = &[
        b"\x1b[1;1H",
        b"\x1b[2J",
        b"\x1b[K",
        b"\x1b[1m",
        b"\x1b[0m",
        b"\x1b[?25h",
        b"\x1b[?25l",
        b"\x1b[?1049h",
        b"\x1b[?1049l",
        b"\x1b[6n",
        b"\x1b[0c",
        b"\x1b[38;5;12m",
        b"\x1b[48;2;1;2;3m",
    ];
    const COMPLETIONS: &[&[u8]] = &[b"2J", b"K", b"6n", b"1m", b"?25h", b"?25l", b"0c", b"1;1H"];
    let target = noprop::sample_choice(ctx, TARGETS);
    let target_cut = sample_strict_interior_cut(ctx, target);
    let completion = noprop::sample_choice(ctx, COMPLETIONS);
    let mut prefix = target.to_vec();
    prefix.extend_from_slice(b"\x1b[");
    let mut suffix = completion.to_vec();
    suffix.extend_from_slice(SENTINEL);
    let expect_cursor_visible = match completion {
        b"?25h" => Some(true),
        b"?25l" => Some(false),
        _ => None,
    };
    Case {
        name: "csi",
        prefix,
        target_cut,
        suffix,
        expect_prefix_action: false,
        expect_suffix_action: completion_is_query(completion),
        expect_title: None,
        expect_cursor_visible,
        expect_glyph: None,
    }
}

/// Prefix cut mid-OSC payload, suffix closing with BEL or ST.
fn gen_osc(ctx: &mut noprop::TestCaseContext) -> Case {
    const TARGETS: &[&[u8]] = &[b"\x1b]2;hello\x07", b"\x1b]0;title\x1b\\", b"\x1b]2;\x07"];
    const PROBES: &[(&[u8], &str)] = &[
        (b"\x1b]2;par", "par"),
        (b"\x1b]0;xyz", "xyz"),
        (b"\x1b]2;", ""),
    ];
    const COMPLETIONS: &[&[u8]] = &[b"\x07", b"\x1b\\"];
    let target = noprop::sample_choice(ctx, TARGETS);
    let target_cut = sample_strict_interior_cut(ctx, target);
    let (probe, expected_title) = noprop::sample_choice(ctx, PROBES);
    let completion = noprop::sample_choice(ctx, COMPLETIONS);
    let mut prefix = target.to_vec();
    prefix.extend_from_slice(probe);
    let mut suffix = completion.to_vec();
    suffix.extend_from_slice(SENTINEL);
    Case {
        name: "osc",
        prefix,
        target_cut,
        suffix,
        expect_prefix_action: false,
        expect_suffix_action: false,
        expect_title: Some(expected_title),
        expect_cursor_visible: None,
        expect_glyph: None,
    }
}

/// Prefix cut inside a valid multibyte UTF-8 sequence, suffix completing the
/// code point.
fn gen_utf8(ctx: &mut noprop::TestCaseContext) -> Case {
    const TARGETS: &[&[u8]] = &[
        "あ".as_bytes(),
        "漢".as_bytes(),
        "😀".as_bytes(),
        "é".as_bytes(),
    ];
    // (probe, completion) pairs reconstruct a complete code point.
    const PAIRS: &[(&[u8], &[u8])] = &[
        (&[0xe3, 0x81], b"\x82"),
        (&[0xf0, 0x9f, 0x98], b"\x80"),
        (b"\xc3", b"\xa9"),
        (&[0xe6, 0xbc], b"\xa2"),
        (b"\xe3", b"\x81\x82"),
        (b"\xe8", b"\xaa\x9e"),
    ];
    let target = noprop::sample_choice(ctx, TARGETS);
    let target_cut = sample_strict_interior_cut(ctx, target);
    let (probe, completion) = noprop::sample_choice(ctx, PAIRS);
    let mut completed = probe.to_vec();
    completed.extend_from_slice(completion);
    let glyph = std::str::from_utf8(&completed)
        .expect("paired probe and completion decode as UTF-8")
        .chars()
        .next()
        .expect("completed bytes contain a code point");
    let mut prefix = target.to_vec();
    prefix.extend_from_slice(probe);
    let mut suffix = completion.to_vec();
    suffix.extend_from_slice(SENTINEL);
    Case {
        name: "utf8",
        prefix,
        target_cut,
        suffix,
        expect_prefix_action: false,
        expect_suffix_action: false,
        expect_title: None,
        expect_cursor_visible: None,
        expect_glyph: Some(glyph),
    }
}

/// Prefix cut inside an incomplete terminal query, suffix completing it and
/// producing a reply action.
fn gen_query(ctx: &mut noprop::TestCaseContext) -> Case {
    const TARGETS: &[&[u8]] = &[b"\x1b[6n", b"\x1b[5n", b"\x1b[0c", b"\x1b[c"];
    const PROBES: &[(&[u8], &[u8])] = &[
        (b"\x1b[6", b"n"),
        (b"\x1b[5", b"n"),
        (b"\x1b[0", b"c"),
        (b"\x1b[", b"c"),
    ];
    let target = noprop::sample_choice(ctx, TARGETS);
    let target_cut = sample_strict_interior_cut(ctx, target);
    let (probe, completion) = noprop::sample_choice(ctx, PROBES);
    let mut prefix = target.to_vec();
    prefix.extend_from_slice(probe);
    let mut suffix = completion.to_vec();
    suffix.extend_from_slice(SENTINEL);
    Case {
        name: "query",
        prefix,
        target_cut,
        suffix,
        expect_prefix_action: true,
        expect_suffix_action: true,
        expect_title: None,
        expect_cursor_visible: None,
        expect_glyph: None,
    }
}

/// Prefix cut inside or right after an invalid, overlong, or unsupported
/// sequence; the suffix completes it and a sentinel observes parser recovery.
fn gen_recovery(ctx: &mut noprop::TestCaseContext) -> Case {
    const TARGETS: &[&[u8]] = &[
        &[0xf5, 0x80],
        &[0xc0, 0xaf],
        &[0xe2, 0x28, 0xa1],
        b"\x1bP1;2|payload\x1b\\",
        b"\x1b[1;2;3;4;5;6;7;8;9;10;11;12;13;14;15;16;17;18;19;20;21;22;23;24;25;26;27;28;29;30;31;32;33Z",
    ];
    const PROBES: &[(&[u8], &[u8])] = &[
        (b"\xff", b"X"),
        (b"\xc0", b"\xaf"),
        (&[0xe2, 0x28], b"\xa1"),
        (b"\x1bP1;2|payload", b"\x1b\\"),
        (b"\x1b[1;2;3;4;5;6;7;8;9;10;11;12;13;14;15;16;17;18;19;20;21;22;23;24;25;26;27;28;29;30;31;32;33", b"Z"),
    ];
    let target = noprop::sample_choice(ctx, TARGETS);
    let target_cut = sample_strict_interior_cut(ctx, target);
    let (probe, completion) = noprop::sample_choice(ctx, PROBES);
    let mut prefix = target.to_vec();
    prefix.extend_from_slice(probe);
    let mut suffix = completion.to_vec();
    suffix.extend_from_slice(SENTINEL);
    Case {
        name: "recovery",
        prefix,
        target_cut,
        suffix,
        expect_prefix_action: false,
        expect_suffix_action: false,
        expect_title: None,
        expect_cursor_visible: None,
        expect_glyph: None,
    }
}

fn generate_case(ctx: &mut noprop::TestCaseContext) -> Case {
    match noprop::sample_weighted_index(ctx, &SCENARIO_WEIGHTS) {
        0 => gen_general(ctx),
        1 => gen_escape(ctx),
        2 => gen_csi(ctx),
        3 => gen_osc(ctx),
        4 => gen_utf8(ctx),
        5 => gen_query(ctx),
        _ => gen_recovery(ctx),
    }
}

#[test]
fn chunk_boundaries_do_not_change_terminal_state() -> noprop::TestResult {
    let seed = noprop::seed_from_env_or_time("MUXNIX_PROPTEST_SEED")?;
    let saw_split = std::cell::Cell::new(false);
    let saw_nonempty = std::cell::Cell::new(false);
    let saw_escape = std::cell::Cell::new(false);

    noprop::Runner::new(seed).run(CASE_BUDGET, |ctx| {
        let rows = noprop::sample_usize_in(ctx, 1..=MAX_ROWS as usize) as u16;
        let cols = noprop::sample_usize_in(ctx, 1..=MAX_COLS as usize) as u16;
        let input = sample_legacy_input(ctx);
        if !input.is_empty() {
            saw_nonempty.set(true);
        }
        if input.contains(&0x1b) {
            saw_escape.set(true);
        }
        let cuts = sample_legacy_cuts(ctx, input.len());
        if !cuts.is_empty() && cuts.iter().any(|&c| c > 0 && c < input.len()) {
            saw_split.set(true);
        }

        let mut whole = termnix::TerminalState::new(size(rows, cols));
        whole.feed(&input);

        let mut split = termnix::TerminalState::new(size(rows, cols));
        feed_with_cuts(&mut split, &input, &cuts);

        assert_eq!(whole, split);
        Ok(())
    })?;

    assert!(
        saw_nonempty.get(),
        "property never fed non-empty input; seed=0x{seed:016x}"
    );
    assert!(
        saw_split.get(),
        "property never exercised a mid-input split; seed=0x{seed:016x}"
    );
    assert!(
        saw_escape.get(),
        "property never fed an ESC-bearing sequence; seed=0x{seed:016x}"
    );
    Ok(())
}

fn sample_legacy_byte(ctx: &mut noprop::TestCaseContext) -> u8 {
    match noprop::sample_weighted_index(ctx, &[8, 2, 1]) {
        0 => noprop::sample_ascii_printable_char(ctx) as u8,
        1 => {
            const CONTROLS: [u8; 5] = [0x07, 0x08, 0x09, 0x0a, 0x0d];
            CONTROLS[noprop::sample_usize_in(ctx, 0..CONTROLS.len())]
        }
        _ => {
            const WIDE: [u8; 3] = [0xe3, 0x81, 0x82];
            WIDE[noprop::sample_usize_in(ctx, 0..WIDE.len())]
        }
    }
}

fn sample_legacy_csi_fragment(ctx: &mut noprop::TestCaseContext) -> &'static [u8] {
    const FRAGMENTS: &[&[u8]] = &[
        b"\x1b[1;1H",
        b"\x1b[2J",
        b"\x1b[K",
        b"\x1b[1m",
        b"\x1b[0m",
        b"\x1b[38;5;12m",
        b"\x1b[48;2;1;2;3m",
        b"\x1b[?25h",
        b"\x1b[?25l",
        b"\x1b[?1049h",
        b"\x1b[?1049l",
        b"\x1b[6n",
        b"\x1b]2;t\x07",
        b"\x1bD",
        b"\x1bM",
        // Overlong CSI is ignored without becoming text.
        b"\x1b[1;2;3;4;5;6;7;8;9;10;11;12;13;14;15;16;17;18;19;20;21;22;23;24;25;26;27;28;29;30;31;32;33Z",
    ];
    FRAGMENTS[noprop::sample_usize_in(ctx, 0..FRAGMENTS.len())]
}

fn sample_legacy_input(ctx: &mut noprop::TestCaseContext) -> Vec<u8> {
    let chunks = noprop::sample_usize_in(ctx, 0..=24);
    let mut bytes = Vec::new();
    for _ in 0..chunks {
        match noprop::sample_weighted_index(ctx, &[5, 2]) {
            0 => bytes.push(sample_legacy_byte(ctx)),
            _ => bytes.extend_from_slice(sample_legacy_csi_fragment(ctx)),
        }
    }
    bytes
}

fn sample_legacy_cuts(ctx: &mut noprop::TestCaseContext, len: usize) -> Vec<usize> {
    if len == 0 {
        return Vec::new();
    }
    let split_count = noprop::sample_usize_in(ctx, 0..=len.min(8));
    let mut cuts = Vec::with_capacity(split_count);
    for _ in 0..split_count {
        cuts.push(noprop::sample_usize_in(ctx, 0..=len));
    }
    cuts.sort_unstable();
    cuts.dedup();
    cuts
}

#[test]
fn continuation_equivalence_holds_across_feed_partitions() -> noprop::TestResult {
    let seed = noprop::seed_from_env_or_time("MUXNIX_PROPTEST_SEED")?;
    let target_split = std::cell::Cell::new(0usize);
    let prefix_action = std::cell::Cell::new(0usize);
    let suffix_action = std::cell::Cell::new(0usize);
    let mut runner = noprop::Runner::new(seed);

    runner.run(CASE_BUDGET, |ctx| {
        let rows = sample_dimension(ctx, MAX_ROWS);
        let cols = sample_dimension(ctx, MAX_COLS);
        let grid = size(rows, cols);
        let case = generate_case(ctx);

        // The guaranteed strict cut inside the completed target, plus optional
        // extra cuts strictly inside the prefix. Every cut stays below
        // `prefix.len()` so the prefix/suffix boundary itself never differs
        // between the two feed paths.
        let mut prefix_cuts = vec![case.target_cut];
        let extra = noprop::sample_with_boundaries(
            ctx,
            &[0usize, 1, MAX_EXTRA_CUTS],
            noprop::Ratio::one_nth(4),
            |ctx| noprop::sample_usize_in(ctx, 0..=MAX_EXTRA_CUTS),
        );
        for _ in 0..extra {
            if case.prefix.len() > 1 {
                prefix_cuts.push(noprop::sample_usize_in(ctx, 1..case.prefix.len()));
            }
        }
        prefix_cuts.sort_unstable();
        prefix_cuts.dedup();

        let mut whole = termnix::TerminalState::new(grid);
        whole.feed(&case.prefix);

        let mut split = termnix::TerminalState::new(grid);
        feed_with_cuts(&mut split, &case.prefix, &prefix_cuts);

        let case_desc = format!(
            "scenario={} size={rows}x{cols} prefix=[{}] target_cut={} prefix_cuts={prefix_cuts:?} suffix=[{}] budget={CASE_BUDGET}",
            case.name,
            hex(&case.prefix),
            case.target_cut,
            hex(&case.suffix),
        );

        // The completed target must have entered different feed calls on the
        // whole and split sides. Always true by construction; the gate guards
        // against a refactor weakening the strict-cut guarantee.
        let prefix_actions = drain_and_compare(
            &mut whole,
            &mut split,
            &format!("{case_desc}; after prefix"),
        );
        target_split.set(target_split.get() + 1);
        if !prefix_actions.is_empty() {
            prefix_action.set(prefix_action.get() + 1);
        }
        if case.expect_prefix_action && prefix_actions.is_empty() {
            panic!("{case_desc}: expected a query reply action after the prefix");
        }

        // Suffix: identical cuts on both states, compared after every chunk.
        let suffix_cuts = sample_suffix_cuts(ctx, case.suffix.len());
        let mut start = 0;
        let mut suffix_actions = Vec::new();
        for &cut in &suffix_cuts {
            if cut > start {
                let chunk = &case.suffix[start..cut];
                whole.feed(chunk);
                split.feed(chunk);
                suffix_actions.extend(drain_and_compare(
                    &mut whole,
                    &mut split,
                    &format!("{case_desc}; after suffix chunk {start}..{cut}"),
                ));
                start = cut;
            }
        }
        let last = &case.suffix[start..];
        whole.feed(last);
        split.feed(last);
        suffix_actions.extend(drain_and_compare(
            &mut whole,
            &mut split,
            &format!("{case_desc}; after final suffix chunk"),
        ));

        if !suffix_actions.is_empty() {
            suffix_action.set(suffix_action.get() + 1);
        }
        if case.expect_suffix_action && suffix_actions.is_empty() {
            panic!("{case_desc}: expected a query reply action after the suffix");
        }

        // Scenario-specific semantic effects, observed after the suffix
        // completes the interrupted probe.
        assert!(
            sentinel_visible(&whole),
            "{case_desc}: sentinel not visible after suffix completion"
        );
        if let Some(expected) = case.expect_title {
            assert_eq!(
                whole.title(),
                expected,
                "{case_desc}: title mismatch after OSC completion"
            );
        }
        if let Some(expected) = case.expect_cursor_visible {
            assert_eq!(
                whole.modes().cursor_visible,
                expected,
                "{case_desc}: cursor visibility mismatch after mode completion"
            );
        }
        // The completed UTF-8 code point must land on the grid. At least two
        // rows and three columns are required: a width-2 glyph plus the
        // sentinel cannot share a two-column row, and on a single-row screen
        // the sentinel's wrap scrolls (and clears) the glyph row. Equivalence
        // still covers the smaller sizes.
        if let Some(glyph) = case.expect_glyph
            && rows >= 2
            && cols >= 3
        {
            assert!(
                glyph_visible(&whole, glyph),
                "{case_desc}: completed glyph {glyph:?} not visible"
            );
        }
        Ok(())
    })?;

    assert!(
        runner.stats().rejected_cases == 0,
        "valid-by-construction generator rejected cases\n{runner}"
    );
    assert!(
        target_split.get() > 0,
        "no case split inside a completed target; seed=0x{seed:016x}\n{runner}"
    );
    assert!(
        prefix_action.get() > 0,
        "no case completed a query in the prefix; seed=0x{seed:016x}\n{runner}"
    );
    assert!(
        suffix_action.get() > 0,
        "no case completed a query in the suffix; seed=0x{seed:016x}\n{runner}"
    );
    Ok(())
}

/// Builds a grid size from two dimensions the sampler has already bounded to
/// at least 1, so the non-zero conversion cannot fail.
fn size(rows: u16, cols: u16) -> termnix::Size {
    termnix::Size {
        rows: std::num::NonZeroU16::new(rows).expect("rows is non-zero"),
        cols: std::num::NonZeroU16::new(cols).expect("cols is non-zero"),
    }
}

fn term(rows: u16, cols: u16) -> termnix::TerminalState {
    termnix::TerminalState::new(size(rows, cols))
}

fn text_at(term: &termnix::TerminalState, row: u16) -> String {
    let cols = term.size().cols.get();
    let mut out = String::new();
    for col in 0..cols {
        let cell = term
            .cell(termnix::Position { row, col })
            .expect("cell in range");
        if cell.width == 0 {
            continue;
        }
        out.push(cell.ch);
    }
    out.trim_end().to_string()
}

#[test]
fn printable_ascii_advances_cursor() {
    let mut t = term(2, 8);
    t.feed(b"hi");
    assert_eq!(text_at(&t, 0), "hi");
    assert_eq!(t.cursor(), termnix::Position { row: 0, col: 2 });
}

#[test]
fn carriage_return_and_line_feed() {
    let mut t = term(3, 8);
    t.feed(b"ab\r\ncd");
    assert_eq!(text_at(&t, 0), "ab");
    assert_eq!(text_at(&t, 1), "cd");
    assert_eq!(t.cursor(), termnix::Position { row: 1, col: 2 });
}

#[test]
fn line_feed_alone_keeps_column() {
    let mut t = term(3, 8);
    t.feed(b"ab\ncd");
    assert_eq!(text_at(&t, 0), "ab");
    assert_eq!(text_at(&t, 1), "  cd");
    assert_eq!(t.cursor(), termnix::Position { row: 1, col: 4 });
}

#[test]
fn backspace_moves_left_without_erasing() {
    let mut t = term(1, 8);
    t.feed(b"ab\x08c");
    assert_eq!(text_at(&t, 0), "ac");
    assert_eq!(t.cursor(), termnix::Position { row: 0, col: 2 });
}

#[test]
fn horizontal_tab_moves_to_next_stop() {
    let mut t = term(1, 16);
    t.feed(b"a\tb");
    assert_eq!(
        t.cell(termnix::Position { row: 0, col: 0 }),
        Some(termnix::Cell {
            ch: 'a',
            width: 1,
            style: termnix::Style::default()
        })
    );
    assert_eq!(
        t.cell(termnix::Position { row: 0, col: 8 }),
        Some(termnix::Cell {
            ch: 'b',
            width: 1,
            style: termnix::Style::default()
        })
    );
    assert_eq!(t.cursor(), termnix::Position { row: 0, col: 9 });
}

#[test]
fn bell_is_ignored() {
    let mut t = term(1, 8);
    t.feed(b"a\x07b");
    assert_eq!(text_at(&t, 0), "ab");
}

#[test]
fn wrap_at_end_of_line() {
    let mut t = term(2, 3);
    t.feed(b"abcd");
    assert_eq!(text_at(&t, 0), "abc");
    assert_eq!(text_at(&t, 1), "d");
    assert_eq!(t.cursor(), termnix::Position { row: 1, col: 1 });
}

#[test]
fn scroll_at_bottom() {
    let mut t = term(2, 4);
    t.feed(b"aa\r\nbb\r\ncc");
    assert_eq!(text_at(&t, 0), "bb");
    assert_eq!(text_at(&t, 1), "cc");
}

#[test]
fn wide_character_occupies_two_cells() {
    let mut t = term(1, 4);
    t.feed("あ".as_bytes());
    assert_eq!(
        t.cell(termnix::Position { row: 0, col: 0 }),
        Some(termnix::Cell {
            ch: 'あ',
            width: 2,
            style: termnix::Style::default()
        })
    );
    assert_eq!(
        t.cell(termnix::Position { row: 0, col: 1 }),
        Some(termnix::Cell::CONTINUATION)
    );
    assert_eq!(t.cursor(), termnix::Position { row: 0, col: 2 });
}

#[test]
fn overwriting_wide_character_clears_both_cells() {
    let mut t = term(1, 4);
    t.feed("あ".as_bytes());
    t.feed(b"\rx");
    assert_eq!(
        t.cell(termnix::Position { row: 0, col: 0 }),
        Some(termnix::Cell {
            ch: 'x',
            width: 1,
            style: termnix::Style::default()
        })
    );
    assert_eq!(
        t.cell(termnix::Position { row: 0, col: 1 }),
        Some(termnix::Cell::EMPTY)
    );
}

#[test]
fn overwriting_continuation_clears_lead() {
    let mut t = term(1, 4);
    t.feed("あ".as_bytes());
    t.feed(b"\x08y");
    assert_eq!(
        t.cell(termnix::Position { row: 0, col: 0 }),
        Some(termnix::Cell::EMPTY)
    );
    assert_eq!(
        t.cell(termnix::Position { row: 0, col: 1 }),
        Some(termnix::Cell {
            ch: 'y',
            width: 1,
            style: termnix::Style::default()
        })
    );
}

#[test]
fn wide_character_wraps_when_one_column_remains() {
    let mut t = term(2, 3);
    t.feed(b"ab");
    t.feed("あ".as_bytes());
    assert_eq!(text_at(&t, 0), "ab");
    assert_eq!(
        t.cell(termnix::Position { row: 1, col: 0 }),
        Some(termnix::Cell {
            ch: 'あ',
            width: 2,
            style: termnix::Style::default()
        })
    );
    assert_eq!(
        t.cell(termnix::Position { row: 1, col: 1 }),
        Some(termnix::Cell::CONTINUATION)
    );
}

#[test]
fn resize_preserves_overlap_and_clamps_cursor() {
    let mut t = term(2, 4);
    t.feed(b"abcd");
    t.feed(b"xy");
    t.resize(size(1, 2));
    assert_eq!(t.size(), size(1, 2));
    assert_eq!(text_at(&t, 0), "ab");
    assert_eq!(t.cursor(), termnix::Position { row: 0, col: 1 });
}

#[test]
fn resize_clears_clipped_wide_character() {
    let mut t = term(1, 4);
    t.feed("あ".as_bytes());
    t.resize(size(1, 1));
    assert_eq!(
        t.cell(termnix::Position { row: 0, col: 0 }),
        Some(termnix::Cell::EMPTY)
    );
}

#[test]
fn incomplete_utf8_is_completed_across_feed_calls() {
    let mut t = term(1, 4);
    let bytes = "あ".as_bytes();
    t.feed(&bytes[..1]);
    assert_eq!(text_at(&t, 0), "");
    t.feed(&bytes[1..]);
    assert_eq!(
        t.cell(termnix::Position { row: 0, col: 0 }),
        Some(termnix::Cell {
            ch: 'あ',
            width: 2,
            style: termnix::Style::default()
        })
    );
}

#[test]
fn invalid_utf8_becomes_replacement_character() {
    let mut t = term(1, 4);
    t.feed(&[0xff, b'x']);
    assert_eq!(
        t.cell(termnix::Position { row: 0, col: 0 }),
        Some(termnix::Cell {
            ch: '\u{FFFD}',
            width: 1,
            style: termnix::Style::default()
        })
    );
    assert_eq!(
        t.cell(termnix::Position { row: 0, col: 1 }),
        Some(termnix::Cell {
            ch: 'x',
            width: 1,
            style: termnix::Style::default()
        })
    );
}

#[test]
fn combining_character_is_ignored() {
    let mut t = term(1, 4);
    t.feed("a\u{0301}".as_bytes());
    assert_eq!(text_at(&t, 0), "a");
    assert_eq!(t.cursor(), termnix::Position { row: 0, col: 1 });
}

#[test]
fn incomplete_csi_recovers_for_later_text() {
    let mut t = term(2, 16);
    // Ignore flag trips when the parameter list is absurdly long; the final
    // byte ends the sequence without applying it, then text must still print.
    t.feed(b"\x1b[");
    t.feed(b"1;2;3;4;5;6;7;8;9;10;11;12;13;14;15;16;17;18;19;20;21;22;23;24;25;26;27;28;29;30;31;32;33Z");
    t.feed(b"ok");
    assert_eq!(text_at(&t, 0), "ok");
}

#[test]
fn unsupported_osc_does_not_become_text() {
    let mut t = term(2, 24);
    t.feed(b"\x1b]999;payload-should-vanish\x07ok");
    assert_eq!(text_at(&t, 0), "ok");
}

#[test]
fn cup_and_el_move_and_erase() {
    let mut t = term(3, 8);
    t.feed(b"abcdef\x1b[1;3H\x1b[KX");
    assert_eq!(text_at(&t, 0), "abX");
    assert_eq!(t.cursor(), termnix::Position { row: 0, col: 3 });
}

#[test]
fn sgr_bold_and_256_color_attach_to_cells() {
    let mut t = term(1, 8);
    t.feed(b"\x1b[1;38;5;196mZ");
    let cell = t.cell(termnix::Position { row: 0, col: 0 }).expect("cell");
    assert_eq!(cell.ch, 'Z');
    assert!(cell.style.bold);
    assert_eq!(cell.style.foreground, termnix::Color::Indexed(196));
}

#[test]
fn sgr_truecolor_and_reset() {
    let mut t = term(1, 8);
    t.feed(b"\x1b[48;2;10;20;30mA\x1b[0mB");
    let a = t.cell(termnix::Position { row: 0, col: 0 }).expect("A");
    let b = t.cell(termnix::Position { row: 0, col: 1 }).expect("B");
    assert_eq!(a.style.background, termnix::Color::Rgb(10, 20, 30));
    assert_eq!(b.style, termnix::Style::default());
}

#[test]
fn alternate_screen_1049_restores_primary() {
    let mut t = term(2, 8);
    t.feed(b"keep");
    t.feed(b"\x1b[?1049h");
    t.feed(b"temp");
    assert!(t.is_on_alternate_screen());
    assert_eq!(text_at(&t, 0), "temp");
    t.feed(b"\x1b[?1049l");
    assert!(!t.is_on_alternate_screen());
    assert_eq!(text_at(&t, 0), "keep");
}

#[test]
fn private_modes_are_retained() {
    let mut t = term(2, 8);
    t.feed(b"\x1b[?1h\x1b[?25l\x1b[?2004h\x1b[?1000h\x1b[?1006h");
    let modes = t.modes();
    assert!(modes.application_cursor);
    assert!(!modes.cursor_visible);
    assert!(modes.bracketed_paste);
    assert_eq!(modes.mouse, termnix::MouseReporting::Normal);
    assert!(modes.mouse_sgr);
}

#[test]
fn osc_title_is_stored() {
    let mut t = term(1, 8);
    t.feed(b"\x1b]2;termnix-title\x07");
    assert_eq!(t.title(), "termnix-title");
}

#[test]
fn cursor_position_report_is_an_action() {
    let mut t = term(5, 10);
    t.feed(b"\x1b[3;4H\x1b[6n");
    let actions = t.drain_actions();
    assert_eq!(
        actions,
        vec![termnix::TerminalAction::WritePty(b"\x1b[3;4R".to_vec())]
    );
}

#[test]
fn scroll_region_limits_index() {
    let mut t = term(4, 4);
    t.feed(b"aaaa\r\nbbbb\r\ncccc\r\ndddd");
    t.feed(b"\x1b[2;3r"); // scroll region rows 2-3 (1-based)
    t.feed(b"\x1b[3;1H\n"); // at bottom of region, LF scrolls
    // Row 0 (aaaa) stays; region scrolled.
    assert_eq!(text_at(&t, 0), "aaaa");
}

#[test]
fn insert_mode_shifts_cells() {
    let mut t = term(1, 6);
    t.feed(b"abc");
    t.feed(b"\x1b[1;1H\x1b[4hX");
    assert_eq!(text_at(&t, 0), "Xabc");
}

#[test]
fn revision_advances_on_visible_changes() {
    let mut t = term(2, 8);
    let start = t.revision();
    t.feed(b"a");
    let after_text = t.revision();
    assert_ne!(after_text, start, "text write is a visible change");

    t.feed(b"\x1b[2;3H");
    let after_move = t.revision();
    assert_ne!(after_move, after_text, "cursor move is a visible change");

    t.feed(b"\x1b[1m");
    let after_sgr = t.revision();
    assert_ne!(after_sgr, after_move, "SGR is a visible change");

    t.feed(b"\x1b[?25l");
    let after_cursor = t.revision();
    assert_ne!(
        after_cursor, after_sgr,
        "cursor visibility is a visible change"
    );

    t.feed(b"\x1b]2;title\x07");
    let after_title = t.revision();
    assert_ne!(
        after_title, after_cursor,
        "title change is a visible change"
    );

    t.resize(size(3, 9));
    assert_ne!(t.revision(), after_title, "resize is a visible change");
}

#[test]
fn revision_advances_on_alternate_screen_toggle() {
    let mut t = term(2, 8);
    t.feed(b"keep");
    let before = t.revision();
    t.feed(b"\x1b[?1049h");
    let after_enter = t.revision();
    assert_ne!(
        after_enter, before,
        "entering alternate is a visible change"
    );
    t.feed(b"\x1b[?1049l");
    assert_ne!(
        t.revision(),
        after_enter,
        "leaving alternate is a visible change"
    );
}

#[test]
fn revision_stays_put_without_visible_change() {
    let mut t = term(2, 8);
    t.feed(b"ab");
    t.feed(b"\x1b[2;1H"); // a cursor move: visible, so re-baseline after it
    let base = t.revision();

    t.feed(b"\x07"); // BEL is ignored
    assert_eq!(t.revision(), base, "BEL must not advance revision");

    t.feed(b"\x1b[3"); // partial CSI, waits for continuation
    assert_eq!(
        t.revision(),
        base,
        "partial sequence must not advance revision"
    );

    t.feed(b"\x1b]999;ignored\x07"); // unsupported OSC is ignored
    assert_eq!(t.revision(), base, "ignored OSC must not advance revision");
}

#[test]
fn revision_advances_on_visible_change_and_holds_otherwise() {
    let mut t = term(2, 8);
    t.feed(b"hi");
    let captured = t.revision();

    t.feed(b"\x1b[1;1Hmore");
    assert_ne!(
        t.revision(),
        captured,
        "a visible change must advance revision"
    );

    let after_change = t.revision();
    t.feed(b"");
    assert_eq!(
        t.revision(),
        after_change,
        "an empty feed must leave revision unchanged"
    );
}

#[test]
fn identical_feeds_reach_the_same_revision() {
    let mut a = term(2, 8);
    let mut b = term(2, 8);
    a.feed(b"hello\r\nworld");
    b.feed(b"hello\r\nworld");
    assert_eq!(a.revision(), b.revision());
}

#[test]
fn origin_is_row_and_column_zero() {
    let origin = termnix::Position::ORIGIN;
    assert_eq!(origin.row, 0);
    assert_eq!(origin.col, 0);
    assert_eq!(
        origin,
        termnix::Position { row: 0, col: 0 },
        "ORIGIN must equal the literal zero position"
    );
}

#[test]
fn rows_match_size_and_cell_access() {
    let mut t = term(3, 4);
    t.feed(b"ab\x1b[1;31mZ\r\ncd\r\nef");

    let rows: Vec<&[termnix::Cell]> = t.rows().collect();
    assert_eq!(rows.len(), t.size().rows.get() as usize);
    for row in &rows {
        assert_eq!(row.len(), t.size().cols.get() as usize);
    }

    // Every cell reachable by `rows()` matches `cell(Position)`.
    for (r, row) in rows.iter().enumerate() {
        for (c, cell) in row.iter().enumerate() {
            let at = termnix::Position {
                row: r as u16,
                col: c as u16,
            };
            assert_eq!(*cell, t.cell(at).expect("cell in range"));
        }
    }
}

#[test]
fn row_returns_the_same_slice_as_rows_and_rejects_out_of_range() {
    let mut t = term(2, 4);
    t.feed(b"ab\r\ncd");

    assert_eq!(t.row(0), t.rows().next());
    assert_eq!(t.row(1), t.rows().nth(1));
    assert_eq!(t.row(0).expect("row 0")[0].ch, 'a');
    assert_eq!(t.row(1).expect("row 1")[0].ch, 'c');
    assert_eq!(t.row(2), None);
    assert_eq!(t.row(u16::MAX), None);
}

#[test]
fn rows_work_for_a_single_cell_screen() {
    let mut t = term(1, 1);
    t.feed(b"x");

    let rows: Vec<&[termnix::Cell]> = t.rows().collect();
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].len(), 1);
    assert_eq!(rows[0][0].ch, 'x');
    assert_eq!(t.row(0), Some(&[rows[0][0]][..]));
    assert_eq!(t.row(1), None);
}

#[test]
fn rows_follow_the_active_screen_through_resize_and_alternate_screen() {
    let mut t = term(2, 4);
    t.feed(b"ab\r\ncd");
    let before = text_at(&t, 0);
    assert_eq!(before, "ab");

    // Resizing re-lays the cells; the first row is still reachable through
    // the same accessor.
    t.resize(size(2, 6));
    assert_eq!(t.row(0).expect("row 0").len(), 6);
    assert_eq!(text_at(&t, 0), "ab");

    // The alternate screen is what `rows()` reports once it is active.
    t.feed(b"\x1b[?1049h\x1b[1;1HZZ");
    assert!(t.is_on_alternate_screen());
    assert_eq!(text_at(&t, 0), "ZZ");
}
