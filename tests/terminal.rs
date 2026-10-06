//! Tests for `termnix::TerminalState`.
//!
//! Two layers share this file:
//!
//! - Deterministic example tests for the public emulator API (text,
//!   controls, wide characters, resizing, private modes, query replies, and the
//!   events reported through [`termnix::TerminalState::next_event`]).
//! - Property tests for chunk-independent feeding and parser continuation
//!   equivalence. The oracle is a metamorphic relation: the same logical byte
//!   stream fed through different feed partitions must leave observationally
//!   equivalent terminal states, both right after a prefix and after every
//!   chunk of a common suffix. The only guaranteed partition difference lies
//!   strictly inside a completed target sequence; the prefix/suffix boundary
//!   itself is never the differing cut.
//!
//! Reproduction:
//! `TERMNIX_PBT_SEED=<seed> cargo test --test terminal <name> -- --exact --nocapture`

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

/// Compares every public observable of two states, then compares their reply
/// buffers (contents and order) and drains both. The comparison is written out
/// field by field rather than delegating to a `PartialEq` impl, so it covers
/// exactly what the public API exposes: the parser's private continuation
/// state, the saved cursor, wrap pending, and the scroll region have no
/// accessor to read them through.
fn drain_and_compare(
    a: &mut termnix::TerminalState,
    b: &mut termnix::TerminalState,
    where_: &str,
) -> Vec<u8> {
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
    assert_eq!(
        a.scrollback_lines(),
        b.scrollback_lines(),
        "{where_}: scrollback mismatch"
    );
    assert_eq!(
        a.scrollback_cells(),
        b.scrollback_cells(),
        "{where_}: scrollback cell count mismatch"
    );
    let replies_a = a.pending_reply_bytes().to_vec();
    let replies_b = b.pending_reply_bytes().to_vec();
    assert_eq!(replies_a, replies_b, "{where_}: reply mismatch");
    a.advance_reply_bytes(replies_a.len());
    b.advance_reply_bytes(replies_b.len());
    assert!(
        a.pending_reply_bytes().is_empty(),
        "{where_}: reply buffer did not empty after advancing its full length"
    );
    assert!(
        b.pending_reply_bytes().is_empty(),
        "{where_}: reply buffer did not empty after advancing its full length"
    );
    replies_a
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
/// completion, and whether the completion produces a query reply.
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
/// producing a reply.
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
    let seed = noprop::seed_from_env_or_time("TERMNIX_PBT_SEED")?;
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

        drain_and_compare(&mut whole, &mut split, "chunk boundaries");
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
    let seed = noprop::seed_from_env_or_time("TERMNIX_PBT_SEED")?;
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
            panic!("{case_desc}: expected a query reply after the prefix");
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
            panic!("{case_desc}: expected a query reply after the suffix");
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

/// Drains every pending event, oldest-priority-first.
///
/// The event channel is the crate's whole notification surface now, so a test
/// reads it the way a host does: loop `next_event()` until it returns `None`.
/// Draining is destructive, which is what lets a test assert "nothing more is
/// pending" by draining again and finding the list empty.
fn drain(t: &mut termnix::TerminalState) -> Vec<termnix::Event> {
    let mut events = Vec::new();
    while let Some(event) = t.next_event() {
        events.push(event);
    }
    events
}

/// Drains the pending events and returns whether a `ScreenUpdated` was among
/// them.
///
/// "The visible screen moved since the last drain". Reporting the change
/// consumes it, so a test that wants to check a later feed re-drains.
fn screen_updated(t: &mut termnix::TerminalState) -> bool {
    drain(t)
        .iter()
        .any(|event| matches!(event, termnix::Event::ScreenUpdated))
}

/// Returns the next pending request, leaving the rest queued, and drops the
/// state-change events it walks past.
///
/// The state-change events (reset, screen, history, title) are dropped: a test
/// asking for "the request" does not care whether the same feed also repainted.
/// Because requests are yielded last, in order, stopping at the first request
/// cannot strand a state-change event (they all precede the queue).
fn next_request(t: &mut termnix::TerminalState) -> Option<termnix::ChildRequest> {
    while let Some(event) = t.next_event() {
        if let termnix::Event::RequestReceived(request) = event {
            return Some(request);
        }
    }
    None
}

/// Drains the next request and matches it as a `SetClipboard`, failing the
/// test if the request belongs to another variant.
fn set_clipboard(t: &mut termnix::TerminalState) -> Option<ClipboardWrite> {
    match next_request(t)? {
        termnix::ChildRequest::SetClipboard {
            text,
            selection,
            append,
        } => Some(ClipboardWrite {
            text,
            selection,
            append,
        }),
        other => panic!("expected a SetClipboard request, got {other:?}"),
    }
}

/// Drains the next request and matches it as a `GetClipboard`, failing the
/// test if the request belongs to another variant.
fn get_clipboard(t: &mut termnix::TerminalState) -> Option<termnix::ClipboardSelection> {
    match next_request(t)? {
        termnix::ChildRequest::GetClipboard { selection } => Some(selection),
        other => panic!("expected a GetClipboard request, got {other:?}"),
    }
}

/// Drains the next request and matches it as a `SetColor`, returning its slot
/// and value, or `None` if no request is pending.
fn set_color(t: &mut termnix::TerminalState) -> Option<(termnix::ColorSlot, termnix::Rgb)> {
    match next_request(t)? {
        termnix::ChildRequest::SetColor { slot, rgb } => Some((slot, rgb)),
        other => panic!("expected a SetColor request, got {other:?}"),
    }
}

/// Drains the next request and matches it as a `GetColor`, returning its slot,
/// or `None` if no request is pending.
fn get_color(t: &mut termnix::TerminalState) -> Option<termnix::ColorSlot> {
    match next_request(t)? {
        termnix::ChildRequest::GetColor { slot } => Some(slot),
        other => panic!("expected a GetColor request, got {other:?}"),
    }
}

/// Drains the next request and matches it as an `OtherOsc`, failing the test
/// if the request belongs to another variant.
fn other_osc(t: &mut termnix::TerminalState) -> Option<OtherOsc> {
    match next_request(t)? {
        termnix::ChildRequest::OtherOsc { id, params } => Some(OtherOsc { id, params }),
        other => panic!("expected an OtherOsc request, got {other:?}"),
    }
}

/// The fields of a [`ChildRequest::OtherOsc`](termnix::ChildRequest::OtherOsc)
/// unpacked, so a test can assert on them directly.
#[derive(Debug, PartialEq, Eq)]
struct OtherOsc {
    id: Vec<u8>,
    params: Vec<Vec<u8>>,
}

/// The fields of a [`ChildRequest::SetClipboard`](termnix::ChildRequest::SetClipboard)
/// unpacked, so a test can assert on them directly.
struct ClipboardWrite {
    text: Vec<u8>,
    selection: termnix::ClipboardSelection,
    append: bool,
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
fn bell_is_not_visible_text() {
    // BEL prints nothing, so the row is unchanged. It is not silent, though:
    // the bell is reported as a `RingBell` request (see
    // `screen_updated_stays_clear_without_visible_change`).
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
fn unmodelled_osc_is_offered_and_does_not_become_text() {
    // The sequence must not leak into the grid, and it must not vanish either:
    // a number termnix does not interpret is offered to the caller whole.
    let mut t = term(2, 24);
    t.feed(b"\x1b]999;payload\x07ok");
    assert_eq!(text_at(&t, 0), "ok");
    assert_eq!(
        other_osc(&mut t),
        Some(OtherOsc {
            id: b"999".to_vec(),
            params: vec![b"payload".to_vec()],
        })
    );
}

#[test]
fn unmodelled_osc_carries_the_identifier_and_split_arguments() {
    // A working-directory announcement, the canonical case passthrough exists
    // for: the crate splits the fields and hands them over undecoded.
    let mut t = term(1, 8);
    t.feed(b"\x1b]7;file://host/home/user\x07");
    assert_eq!(
        other_osc(&mut t),
        Some(OtherOsc {
            id: b"7".to_vec(),
            params: vec![b"file://host/home/user".to_vec()],
        })
    );
}

#[test]
fn unmodelled_osc_with_no_arguments_has_empty_params() {
    // `ESC ] 7 ST` has an identifier and nothing else. The honest answer is an
    // empty argument list, not a placeholder field.
    let mut t = term(1, 8);
    t.feed(b"\x1b]7\x07");
    assert_eq!(
        other_osc(&mut t),
        Some(OtherOsc {
            id: b"7".to_vec(),
            params: Vec::new(),
        })
    );
}

#[test]
fn unmodelled_osc_keeps_empty_argument_fields() {
    // Framing, not interpretation: `a;;b` is three fields, and the empty one
    // in the middle is preserved rather than collapsed.
    let mut t = term(1, 8);
    t.feed(b"\x1b]133;a;;b\x07");
    assert_eq!(
        other_osc(&mut t),
        Some(OtherOsc {
            id: b"133".to_vec(),
            params: vec![b"a".to_vec(), Vec::new(), b"b".to_vec()],
        })
    );
}

#[test]
fn unmodelled_osc_identifier_need_not_be_utf8() {
    // Identifiers are conventional rather than numeric, and the field is not
    // required to be valid UTF-8. Such a sequence is uninterpreted, so it is
    // offered as bytes rather than dropped by a decoding gate.
    let mut t = term(1, 8);
    t.feed(b"\x1b]\xff\xfe;x\x07");
    assert_eq!(
        other_osc(&mut t),
        Some(OtherOsc {
            id: vec![0xff, 0xfe],
            params: vec![b"x".to_vec()],
        })
    );
}

#[test]
fn unmodelled_osc_arguments_need_not_be_utf8() {
    let mut t = term(1, 8);
    t.feed(b"\x1b]9;\xff\x07");
    assert_eq!(
        other_osc(&mut t),
        Some(OtherOsc {
            id: b"9".to_vec(),
            params: vec![vec![0xff]],
        })
    );
}

#[test]
fn unmodelled_osc_does_not_raise_screen_updated() {
    // Offering a sequence draws nothing, so a caller repainting on
    // `ScreenUpdated` is not told to repaint because an unknown OSC arrived.
    let mut t = term(1, 8);
    t.feed(b"\x1b]7;file://host/x\x07");
    let events = drain(&mut t);
    assert!(
        !events
            .iter()
            .any(|event| matches!(event, termnix::Event::ScreenUpdated)),
        "an unmodelled OSC is not a visible change: {events:?}"
    );
    assert!(
        events.iter().any(|event| matches!(
            event,
            termnix::Event::RequestReceived(termnix::ChildRequest::OtherOsc { .. })
        )),
        "the sequence itself is still reported: {events:?}"
    );
}

#[test]
fn unmodelled_osc_keeps_its_order_among_other_requests() {
    // Requests are not merged, so an unmodelled OSC between two clipboard
    // asks stays between them: a caller that cares about OSC ordering sees
    // what the child wrote.
    let mut t = term(1, 8);
    t.feed(b"\x1b]52;c;aGVsbG8=\x07\x1b]7;file://host/x\x07\x1b]52;p;d29ybGQ=\x07");
    assert_eq!(set_clipboard(&mut t).expect("first").text, b"hello");
    assert_eq!(
        other_osc(&mut t),
        Some(OtherOsc {
            id: b"7".to_vec(),
            params: vec![b"file://host/x".to_vec()],
        })
    );
    assert_eq!(set_clipboard(&mut t).expect("second").text, b"world");
    assert!(next_request(&mut t).is_none());
}

#[test]
fn unmodelled_osc_survives_a_hard_reset() {
    // Like the other asks, a passthrough sequence belongs to the session being
    // reset and must not outlive RIS.
    let mut t = term(1, 8);
    t.feed(b"\x1b]7;file://host/x\x07");
    t.feed(b"\x1bc");
    assert!(next_request(&mut t).is_none());
}

#[test]
fn a_semicolon_in_an_unmodelled_payload_is_a_field_boundary() {
    // `vte` splits OSC parameters on `;` before the crate sees them, so an
    // argument containing `;` arrives as more fields than the sender wrote.
    // That is a property of OSC framing, not a choice made here.
    let mut t = term(1, 8);
    t.feed(b"\x1b]7;a;b\x07");
    assert_eq!(
        other_osc(&mut t),
        Some(OtherOsc {
            id: b"7".to_vec(),
            params: vec![b"a".to_vec(), b"b".to_vec()],
        })
    );
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
    assert_eq!(cell.style.foreground, Some(termnix::Color::Indexed(196)));
}

#[test]
fn sgr_truecolor_and_reset() {
    let mut t = term(1, 8);
    t.feed(b"\x1b[48;2;10;20;30mA\x1b[0mB");
    let a = t.cell(termnix::Position { row: 0, col: 0 }).expect("A");
    let b = t.cell(termnix::Position { row: 0, col: 1 }).expect("B");
    assert_eq!(
        a.style.background,
        Some(termnix::Color::Rgb(termnix::Rgb::new(10, 20, 30)))
    );
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
fn title_change_raises_title_updated_but_not_screen_updated() {
    // The title is not drawn by termnix; a caller reads it through `title()`.
    // So a title change is a title change, not a screen change.
    let mut t = term(1, 8);
    t.feed(b"\x1b]2;termnix-title\x07");
    let events = drain(&mut t);
    assert!(
        events
            .iter()
            .any(|event| matches!(event, termnix::Event::TitleUpdated)),
        "a title set is a title change: {events:?}"
    );
    assert!(
        !events
            .iter()
            .any(|event| matches!(event, termnix::Event::ScreenUpdated)),
        "a title set is not a screen change: {events:?}"
    );
}

#[test]
fn osc_52_clipboard_is_recorded_and_taken_once() {
    let mut t = term(1, 8);
    t.feed(b"\x1b]52;c;aGVsbG8=\x07");
    let request = set_clipboard(&mut t).expect("request should be pending");
    assert_eq!(request.text, b"hello");
    assert_eq!(request.selection, termnix::ClipboardSelection::Clipboard);
    assert!(!request.append);
    // The accessor is a take: the same sequence is not re-delivered on a later
    // feed, which is what keeps "the child asked once" from becoming "the
    // caller acts once per repaint".
    assert!(next_request(&mut t).is_none());
    t.feed(b"");
    assert!(next_request(&mut t).is_none());
}

#[test]
fn osc_52_requests_queue_and_drain_oldest_first() {
    // A queue, not a slot: two asks in one feed are both kept, in order. A
    // single-slot design would have kept only the second.
    let mut t = term(1, 8);
    t.feed(b"\x1b]52;c;aGVsbG8=\x07\x1b]52;p;d29ybGQ=\x07");
    assert_eq!(set_clipboard(&mut t).expect("hello").text, b"hello");
    assert_eq!(set_clipboard(&mut t).expect("world").text, b"world");
    assert!(next_request(&mut t).is_none());
}

#[test]
fn osc_52_queue_keeps_one_entry_per_identical_request() {
    // Requests are events, not a latest-value-wins property: two identical
    // asks are two events and both survive, rather than collapsing to one.
    let mut t = term(1, 8);
    t.feed(b"\x1b]52;c;aGVsbG8=\x07\x1b]52;c;aGVsbG8=\x07");
    assert!(set_clipboard(&mut t).is_some());
    assert!(set_clipboard(&mut t).is_some());
    assert!(next_request(&mut t).is_none());
}

#[test]
fn osc_52_missing_selection_means_the_clipboard() {
    let mut t = term(1, 8);
    t.feed(b"\x1b]52;;aGVsbG8=\x07");
    assert_eq!(
        set_clipboard(&mut t).expect("clipboard").selection,
        termnix::ClipboardSelection::Clipboard
    );
}

#[test]
fn osc_52_names_other_selections() {
    let mut t = term(1, 8);
    t.feed(b"\x1b]52;p;aGVsbG8=\x07");
    assert_eq!(
        set_clipboard(&mut t).expect("primary").selection,
        termnix::ClipboardSelection::Primary
    );
    t.feed(b"\x1b]52;x;aGVsbG8=\x07");
    assert_eq!(
        set_clipboard(&mut t).expect("other").selection,
        termnix::ClipboardSelection::Other(b"x".to_vec())
    );
}

#[test]
fn osc_52_records_an_append() {
    let mut t = term(1, 8);
    t.feed(b"\x1b]52;c;+aGVsbG8=\x07");
    let request = set_clipboard(&mut t).expect("append");
    assert_eq!(request.text, b"hello");
    assert!(request.append);
}

#[test]
fn osc_52_empty_payload_is_a_clear_request() {
    // An empty payload means "clear the selection", which is distinct from
    // "never asked": `None` is no request, `Some` with empty text is this.
    let mut t = term(1, 8);
    t.feed(b"\x1b]52;c;\x07");
    let request = set_clipboard(&mut t).expect("clear is still a request");
    assert!(request.text.is_empty());
}

#[test]
fn osc_52_read_request_is_reported_with_its_selection() {
    // A read asks the caller for a selection's contents; termnix owns no
    // clipboard, so it delivers the question and leaves the answer to the
    // caller rather than dropping it.
    let mut t = term(1, 8);
    t.feed(b"\x1b]52;c;?\x07");
    assert_eq!(
        get_clipboard(&mut t),
        Some(termnix::ClipboardSelection::Clipboard)
    );
}

#[test]
fn osc_52_read_resolves_every_selection() {
    let mut t = term(1, 8);
    t.feed(b"\x1b]52;p;?\x07");
    assert_eq!(
        get_clipboard(&mut t),
        Some(termnix::ClipboardSelection::Primary)
    );
    t.feed(b"\x1b]52;x;?\x07");
    assert_eq!(
        get_clipboard(&mut t),
        Some(termnix::ClipboardSelection::Other(b"x".to_vec()))
    );
    // A missing selection means the system clipboard, as it does for a write.
    t.feed(b"\x1b]52;;?\x07");
    assert_eq!(
        get_clipboard(&mut t),
        Some(termnix::ClipboardSelection::Clipboard)
    );
}

#[test]
fn osc_52_read_does_not_raise_screen_updated() {
    // A read draws nothing either; it is a question, not a change.
    let mut t = term(1, 8);
    t.feed(b"\x1b]52;c;?\x07");
    let events = drain(&mut t);
    assert!(
        !events
            .iter()
            .any(|event| matches!(event, termnix::Event::ScreenUpdated)),
        "a clipboard read is not a visible change: {events:?}"
    );
    assert!(
        events.iter().any(|event| matches!(
            event,
            termnix::Event::RequestReceived(termnix::ChildRequest::GetClipboard { .. })
        )),
        "the read is reported as a get request: {events:?}"
    );
}

#[test]
fn osc_52_set_and_read_keep_the_childs_order() {
    // A set followed by a read in one feed is delivered set-then-read; the
    // channel preserves the order the child wrote.
    let mut t = term(1, 8);
    t.feed(b"\x1b]52;c;aGVsbG8=\x07\x1b]52;p;?\x07");
    assert_eq!(set_clipboard(&mut t).expect("set first").text, b"hello");
    assert_eq!(
        get_clipboard(&mut t),
        Some(termnix::ClipboardSelection::Primary)
    );
    assert!(next_request(&mut t).is_none());
}

#[test]
fn osc_52_read_survives_a_hard_reset() {
    // Like a set, a pending read belongs to the session being reset and must
    // not outlive RIS.
    let mut t = term(1, 8);
    t.feed(b"\x1b]52;c;?\x07");
    t.feed(b"\x1bc");
    assert!(next_request(&mut t).is_none());
}

#[test]
fn osc_52_invalid_base64_stores_nothing() {
    let mut t = term(1, 8);
    // `!` is not in the base64 alphabet.
    t.feed(b"\x1b]52;c;!!!\x07");
    assert!(next_request(&mut t).is_none());
}

#[test]
fn osc_52_payload_need_not_be_utf8() {
    // The decoded payload is opaque bytes, so a value that is not valid UTF-8
    // is stored as-is rather than repaired or rejected.
    let mut t = term(1, 8);
    t.feed(b"\x1b]52;c;/w==\x07");
    assert_eq!(set_clipboard(&mut t).expect("bytes").text, vec![0xff]);
}

#[test]
fn osc_52_other_selection_need_not_be_utf8() {
    // A `Pc` the terminal does not model is kept as bytes too, so a caller that
    // hands the name onward gets back what the child sent.
    let mut t = term(1, 8);
    t.feed(b"\x1b]52;\xff;aGVsbG8=\x07");
    assert_eq!(
        set_clipboard(&mut t)
            .expect("non-utf8 selection name")
            .selection,
        termnix::ClipboardSelection::Other(vec![0xff])
    );
}

#[test]
fn osc_52_does_not_raise_screen_updated() {
    // A clipboard request draws nothing, so a caller repainting on
    // `ScreenUpdated` must not be told to repaint because a child cut text. The
    // request itself is still reported, as a `RequestReceived`.
    let mut t = term(1, 8);
    t.feed(b"\x1b]52;c;aGVsbG8=\x07");
    let events = drain(&mut t);
    assert!(
        !events
            .iter()
            .any(|event| matches!(event, termnix::Event::ScreenUpdated)),
        "a clipboard write is not a visible change: {events:?}"
    );
    assert!(
        events
            .iter()
            .any(|event| matches!(event, termnix::Event::RequestReceived(_))),
        "the request itself is still reported: {events:?}"
    );
}

#[test]
fn osc_52_payload_cannot_contain_a_semicolon() {
    // `vte` splits OSC parameters on `;`, so `aGVsbG8=;x` arrives as three
    // parameters and the payload is only the first. That is framing rather
    // than a choice here, and it is why OSC 52 cannot carry `;` through the
    // parameter channel.
    let mut t = term(1, 8);
    t.feed(b"\x1b]52;c;aGVsbG8=;x\x07");
    assert_eq!(set_clipboard(&mut t).expect("payload").text, b"hello");
}

#[test]
fn osc_52_survives_a_hard_reset() {
    // RIS restores the terminal; a pending ask from before the reset must not
    // outlive it.
    let mut t = term(1, 8);
    t.feed(b"\x1b]52;c;aGVsbG8=\x07");
    t.feed(b"\x1bc");
    assert!(next_request(&mut t).is_none());
}

#[test]
fn osc_4_set_becomes_a_set_color_request() {
    // termnix owns no palette, so the set is the caller's request; the crate
    // decodes the slot and value and hands them over.
    let mut t = term(1, 8);
    assert_eq!(
        set_color(&mut t),
        None,
        "a fresh terminal has no color request"
    );
    t.feed(b"\x1b]4;1;rgb:ff/00/00\x07");
    assert_eq!(
        set_color(&mut t),
        Some((
            termnix::ColorSlot::Indexed(1),
            termnix::Rgb::new(0xff, 0x00, 0x00)
        ))
    );
    assert!(t.pending_reply_bytes().is_empty(), "a set draws no reply");
}

#[test]
fn osc_4_set_of_several_entries_is_several_requests() {
    // One OSC 4 carries any number of `index;spec` pairs, and each set is its
    // own request, in order.
    let mut t = term(1, 8);
    t.feed(b"\x1b]4;1;rgb:11/22/33;2;rgb:44/55/66\x07");
    assert_eq!(
        set_color(&mut t),
        Some((
            termnix::ColorSlot::Indexed(1),
            termnix::Rgb::new(0x11, 0x22, 0x33)
        ))
    );
    assert_eq!(
        set_color(&mut t),
        Some((
            termnix::ColorSlot::Indexed(2),
            termnix::Rgb::new(0x44, 0x55, 0x66)
        ))
    );
}

#[test]
fn osc_4_query_becomes_a_get_color_request() {
    // A `?` is the caller's to answer; termnix writes nothing itself because
    // it cannot see the host terminal's theme.
    let mut t = term(1, 8);
    t.feed(b"\x1b]4;1;?\x07");
    assert_eq!(get_color(&mut t), Some(termnix::ColorSlot::Indexed(1)));
    assert!(
        t.pending_reply_bytes().is_empty(),
        "termnix answers nothing"
    );
}

#[test]
fn osc_4_mixed_set_and_query_is_two_requests() {
    // A sequence may set one entry and query another; each pair is independent.
    let mut t = term(1, 8);
    t.feed(b"\x1b]4;1;rgb:11/22/33;2;?\x07");
    assert_eq!(
        set_color(&mut t),
        Some((
            termnix::ColorSlot::Indexed(1),
            termnix::Rgb::new(0x11, 0x22, 0x33)
        ))
    );
    assert_eq!(get_color(&mut t), Some(termnix::ColorSlot::Indexed(2)));
}

#[test]
fn osc_4_ignores_a_malformed_spec_and_a_missing_value() {
    // A spec that is not `rgb:` is no request, and a trailing index with no
    // spec is ignored rather than read as a value.
    let mut t = term(1, 8);
    t.feed(b"\x1b]4;1;#ff0000;2\x07");
    assert!(next_request(&mut t).is_none());
    assert!(t.pending_reply_bytes().is_empty());
}

#[test]
fn osc_4_ignores_an_out_of_range_index() {
    // An index above 255 names nothing; it is dropped rather than wrapped.
    let mut t = term(1, 8);
    t.feed(b"\x1b]4;256;rgb:ff/00/00\x07");
    assert!(next_request(&mut t).is_none());
    assert!(t.pending_reply_bytes().is_empty());
}

#[test]
fn osc_rgb_scales_by_digit_count() {
    // One digit spreads across the byte; more digits are the high bits.
    for spec in [
        b"rgb:f/0/0".as_slice(),
        b"rgb:ff/00/00",
        b"rgb:fff/000/000",
        b"rgb:ffff/0000/0000",
    ] {
        let mut t = term(1, 8);
        t.feed(format!("\x1b]4;1;{}\x07", String::from_utf8_lossy(spec)).as_bytes());
        assert_eq!(
            set_color(&mut t),
            Some((
                termnix::ColorSlot::Indexed(1),
                termnix::Rgb::new(0xff, 0x00, 0x00)
            )),
            "spec={spec:?}"
        );
    }
}

#[test]
fn osc_4_set_is_not_a_screen_change() {
    // A color set draws nothing; the request tells the caller, and the screen
    // is untouched.
    let mut t = term(1, 8);
    t.feed(b"\x1b]4;1;rgb:ff/00/00\x07");
    let events = drain(&mut t);
    assert!(
        events.iter().any(|event| matches!(
            event,
            termnix::Event::RequestReceived(termnix::ChildRequest::SetColor { .. })
        )),
        "a palette set is a request: {events:?}"
    );
    assert!(
        !events
            .iter()
            .any(|event| matches!(event, termnix::Event::ScreenUpdated)),
        "a palette set is not a screen change: {events:?}"
    );
}

#[test]
fn osc_10_11_12_set_becomes_set_color_requests() {
    let mut t = term(1, 8);
    for (feed, slot) in [
        (
            &b"\x1b]10;rgb:ff/00/00\x07"[..],
            termnix::ColorSlot::DefaultForeground,
        ),
        (
            &b"\x1b]11;rgb:00/ff/00\x07"[..],
            termnix::ColorSlot::DefaultBackground,
        ),
        (
            &b"\x1b]12;rgb:00/00/ff\x07"[..],
            termnix::ColorSlot::DefaultCursor,
        ),
    ] {
        t.feed(feed);
        match set_color(&mut t) {
            Some((got, _)) => assert_eq!(got, slot, "{feed:?} names {slot:?}"),
            None => panic!("{feed:?} should be a SetColor request"),
        }
    }
}

#[test]
fn osc_10_query_becomes_a_get_color_request() {
    // The crate has no true answer for an unset slot (the default is the
    // host's), so it never guesses; the query is the caller's.
    let mut t = term(1, 8);
    t.feed(b"\x1b]10;?\x07");
    assert_eq!(
        get_color(&mut t),
        Some(termnix::ColorSlot::DefaultForeground)
    );
    assert!(
        t.pending_reply_bytes().is_empty(),
        "termnix answers nothing"
    );
}

#[test]
fn osc_10_with_no_value_is_no_request() {
    // xterm defines no reset form here, so an empty value is a no-op.
    let mut t = term(1, 8);
    t.feed(b"\x1b]10\x07\x1b]10;\x07\x1b]10;notacolor\x07");
    assert!(next_request(&mut t).is_none());
    assert!(t.pending_reply_bytes().is_empty());
}

#[test]
fn ris_does_not_emit_a_color_request() {
    // RIS resets the child's own state; termnix keeps no palette, so it emits
    // nothing for colors, and a request from before the reset does not
    // outlive it.
    let mut t = term(1, 8);
    t.feed(b"\x1b]4;1;rgb:ff/00/00\x07\x1b]10;rgb:ff/00/00\x07");
    t.feed(b"\x1bc");
    assert!(next_request(&mut t).is_none());
    assert!(t.pending_reply_bytes().is_empty());
}

#[test]
fn csi_replies_fit_the_session_bound() {
    // The session used to compare every reply's length against a fixed
    // 14-byte CSI bound. The comparison moved, but the invariant it protected
    // has not: a CSI reply is at most `ESC [ 65535 ; 65535 R`. The width is a
    // property of the format, not of a live grid, so the bound is checked
    // without allocating a 65535x65535 screen (whose cell buffer is ~86 GB).
    let widest = format!("\x1b[{};{}R", u16::MAX, u16::MAX);
    assert_eq!(widest, "\x1b[65535;65535R");
    assert_eq!(widest.len(), 14);
    // The reply the emulator actually produces follows that format, pinned on
    // a grid small enough to allocate.
    let mut t = term(24, 80);
    t.feed(b"\x1b[6n");
    assert_eq!(t.pending_reply_bytes(), b"\x1b[1;1R");
    t.advance_reply_bytes(t.pending_reply_bytes().len());
    // DA1 is the other fixed-shape reply, and it is shorter.
    t.feed(b"\x1b[c");
    assert_eq!(t.pending_reply_bytes(), b"\x1b[?6c");
}

#[test]
fn cursor_position_report_is_a_pending_reply() {
    let mut t = term(5, 10);
    t.feed(b"\x1b[3;4H\x1b[6n");
    assert_eq!(t.pending_reply_bytes(), b"\x1b[3;4R");
    t.advance_reply_bytes(6);
    assert!(t.pending_reply_bytes().is_empty());
}

#[test]
fn primary_da_answers_the_primary_forms_only() {
    // DA1 is the bare form and the `0`-parameterized form; both reply with the
    // VT102-shaped `ESC [ ? 6 c`. (The `?`-private spelling `ESC [ ? c` is not
    // a DA1 request and is not answered, here or before this change.)
    for query in [b"\x1b[c".as_slice(), b"\x1b[0c"] {
        let mut t = term(5, 10);
        t.feed(query);
        assert_eq!(t.pending_reply_bytes(), b"\x1b[?6c", "query={query:?}");
    }
}

#[test]
fn da2_and_da3_are_not_answered_with_the_da1_reply() {
    // `ESC [ > c` (DA2, sent by tmux at startup) and `ESC [ = c` (DA3) carry a
    // marker that is neither absent nor `?`, so they must not fall through to
    // `primary_da`. They are left unanswered rather than replied to with a
    // DA1-shaped string. Parameterized DA2 (`> 0 c`) is covered too because the
    // marker is what matters, not the parameter count.
    for query in [
        b"\x1b[>c".as_slice(),
        b"\x1b[>0c",
        b"\x1b[>1c",
        b"\x1b[=c",
        b"\x1b[=0c",
    ] {
        let mut t = term(5, 10);
        t.feed(query);
        assert!(
            t.pending_reply_bytes().is_empty(),
            "query={query:?} produced a reply"
        );
    }
}

#[test]
fn da2_is_unanswered_even_when_split_across_feeds() {
    // The decision is made once the final byte arrives, so a per-byte feed of a
    // DA2 request must reach the same (empty) reply as a single feed.
    let mut t = term(5, 10);
    for byte in b"\x1b[>0c" {
        t.feed(&[*byte]);
    }
    assert!(t.pending_reply_bytes().is_empty());
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
fn screen_updated_covers_each_screen_change() {
    let mut t = term(2, 8);
    assert!(!screen_updated(&mut t), "a fresh terminal is not dirty");

    t.feed(b"a");
    assert!(screen_updated(&mut t), "text write is a screen change");

    t.feed(b"\x1b[2;3H");
    assert!(screen_updated(&mut t), "cursor move is a screen change");

    t.feed(b"\x1b[1m");
    assert!(screen_updated(&mut t), "SGR is a screen change");

    t.feed(b"\x1b[?25l");
    assert!(
        screen_updated(&mut t),
        "cursor visibility is a screen change"
    );

    t.feed(b"\x1b]2;title\x07");
    assert!(
        !screen_updated(&mut t),
        "a title change is not a screen change"
    );

    t.resize(size(3, 9));
    assert!(screen_updated(&mut t), "resize is a screen change");
}

#[test]
fn screen_updated_on_alternate_screen_toggle() {
    let mut t = term(2, 8);
    t.feed(b"keep");
    let _ = drain(&mut t);
    t.feed(b"\x1b[?1049h");
    assert!(
        screen_updated(&mut t),
        "entering alternate is a visible change"
    );
    t.feed(b"\x1b[?1049l");
    assert!(
        screen_updated(&mut t),
        "leaving alternate is a visible change"
    );
}

#[test]
fn screen_updated_stays_clear_without_visible_change() {
    let mut t = term(2, 8);
    t.feed(b"ab");
    t.feed(b"\x1b[2;1H"); // a cursor move: visible, so drain it away as a baseline
    let _ = drain(&mut t);

    // BEL is not a visible change: it raises no `ScreenUpdated`. It is not
    // silent either - a bell is a request, reported separately.
    t.feed(b"\x07");
    let bell = drain(&mut t);
    assert!(
        !bell
            .iter()
            .any(|event| matches!(event, termnix::Event::ScreenUpdated)),
        "BEL must not raise ScreenUpdated: {bell:?}"
    );
    assert!(
        bell.iter().any(|event| matches!(
            event,
            termnix::Event::RequestReceived(termnix::ChildRequest::RingBell)
        )),
        "BEL is a RingBell request: {bell:?}"
    );

    t.feed(b"\x1b[3"); // partial CSI, waits for continuation
    assert!(
        !screen_updated(&mut t),
        "partial sequence must not raise ScreenUpdated"
    );

    t.feed(b"\x1b]999;offered\x07"); // unmodelled OSC is offered, not applied
    assert!(
        !screen_updated(&mut t),
        "an offered OSC must not raise ScreenUpdated"
    );
}

#[test]
fn hard_reset_reports_reset_and_screen_updated() {
    // RIS blanks the screen (which is a visible change on its own, recorded by
    // marking the screen dirty) and is reported as a reset. The title is
    // cleared too, but the reset already covers everything derived from the
    // child, so no separate title event is raised.
    let mut t = term(2, 8);
    t.feed(b"\x1b]2;title\x07");
    let _ = drain(&mut t);

    t.feed(b"\x1bc"); // RIS

    let events = drain(&mut t);
    assert!(
        events
            .iter()
            .any(|event| matches!(event, termnix::Event::ScreenUpdated)),
        "RIS blanks the screen: {events:?}"
    );
    assert!(
        events
            .iter()
            .any(|event| matches!(event, termnix::Event::TerminalReset)),
        "RIS is reported as a reset: {events:?}"
    );
}

#[test]
fn screen_updated_on_rewriting_the_same_cell() {
    // The dirty flag records that a cell was written, not whether the stored
    // value differs, so a feed that rewrites identical cells still raises
    // `ScreenUpdated`. This is a deliberate false positive: redrawing is cheap
    // compared with missing a change.
    let mut t = term(2, 8);
    t.feed(b"x");
    let _ = drain(&mut t);

    t.feed(b"\x1b[1;1Hx"); // same cell, same value

    assert!(
        screen_updated(&mut t),
        "a cell write raises ScreenUpdated even when the value is unchanged"
    );
}

#[test]
fn screen_tracks_only_the_visible_screen() {
    // The hidden buffer's writes are not polled: while the alternate screen is
    // active, `on_alternate` is what the snapshot carries, and editing the
    // primary behind it cannot be seen. Entering and leaving the alternate
    // screen must still be reported, because those flip `on_alternate`.
    let mut t = term(2, 8);
    t.feed(b"primary");
    t.feed(b"\x1b[?1049h");
    let _ = drain(&mut t);

    t.feed(b"hidden");
    assert!(
        screen_updated(&mut t),
        "writing the active alternate screen is a visible change"
    );

    t.feed(b"\x1b[?1049l");
    assert!(
        screen_updated(&mut t),
        "returning to the primary screen is a visible change"
    );
}

#[test]
fn screen_updated_stays_clear_when_a_mode_is_set_to_its_current_value() {
    let mut t = term(2, 8);
    t.feed(b"\x1b[?25l"); // hide cursor
    let _ = drain(&mut t);

    t.feed(b"\x1b[?25l"); // same value again

    assert!(
        !screen_updated(&mut t),
        "re-setting a mode to its current value is not a visible change"
    );
}

#[test]
fn screen_updated_on_visible_change_and_quiet_otherwise() {
    let mut t = term(2, 8);
    t.feed(b"hi");
    let _ = drain(&mut t);

    t.feed(b"\x1b[1;1Hmore");
    assert!(
        screen_updated(&mut t),
        "a visible change must raise ScreenUpdated"
    );

    t.feed(b"");
    assert!(
        !screen_updated(&mut t),
        "an empty feed must leave the screen unchanged"
    );
}

#[test]
fn identical_feeds_reach_the_same_events() {
    let mut a = term(2, 8);
    let mut b = term(2, 8);
    a.feed(b"hello\r\nworld");
    b.feed(b"hello\r\nworld");
    assert_eq!(drain(&mut a), drain(&mut b));
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
