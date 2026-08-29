//! Property tests for chunk-independent terminal feeding.

fn sample_control(ctx: &mut noprop::TestCaseContext) -> u8 {
    const CONTROLS: [u8; 5] = [0x07, 0x08, 0x09, 0x0a, 0x0d];
    CONTROLS[noprop::sample_usize_in(ctx, 0..CONTROLS.len())]
}

fn sample_csi_fragment(ctx: &mut noprop::TestCaseContext) -> &'static [u8] {
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

fn sample_byte(ctx: &mut noprop::TestCaseContext) -> u8 {
    match noprop::sample_weighted_index(ctx, &[8, 2, 1]) {
        0 => noprop::sample_ascii_printable_char(ctx) as u8,
        1 => sample_control(ctx),
        _ => {
            // Bytes from UTF-8 encoding of 'あ' so wide glyphs can split.
            const WIDE: [u8; 3] = [0xe3, 0x81, 0x82];
            WIDE[noprop::sample_usize_in(ctx, 0..WIDE.len())]
        }
    }
}

fn sample_input(ctx: &mut noprop::TestCaseContext) -> Vec<u8> {
    let chunks = noprop::sample_usize_in(ctx, 0..=24);
    let mut bytes = Vec::new();
    for _ in 0..chunks {
        match noprop::sample_weighted_index(ctx, &[5, 2]) {
            0 => bytes.push(sample_byte(ctx)),
            _ => bytes.extend_from_slice(sample_csi_fragment(ctx)),
        }
    }
    bytes
}

fn sample_cuts(ctx: &mut noprop::TestCaseContext, len: usize) -> Vec<usize> {
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

fn feed_with_cuts(term: &mut muxnix::TerminalState, bytes: &[u8], cuts: &[usize]) {
    let mut start = 0;
    for &cut in cuts {
        if cut > start && cut <= bytes.len() {
            term.feed(&bytes[start..cut]);
            start = cut;
        }
    }
    term.feed(&bytes[start..]);
}

#[test]
fn chunk_boundaries_do_not_change_terminal_state() -> noprop::TestResult {
    let seed = noprop::seed_from_env_or_time("MUXNIX_PROPTEST_SEED")?;
    let saw_split = std::cell::Cell::new(false);
    let saw_nonempty = std::cell::Cell::new(false);
    let saw_escape = std::cell::Cell::new(false);

    noprop::Runner::new(seed).run(1024, |ctx| {
        let rows = noprop::sample_usize_in(ctx, 1..=6) as u16;
        let cols = noprop::sample_usize_in(ctx, 1..=12) as u16;
        let input = sample_input(ctx);
        if !input.is_empty() {
            saw_nonempty.set(true);
        }
        if input.contains(&0x1b) {
            saw_escape.set(true);
        }
        let cuts = sample_cuts(ctx, input.len());
        if !cuts.is_empty() && cuts.iter().any(|&c| c > 0 && c < input.len()) {
            saw_split.set(true);
        }

        let mut whole = muxnix::TerminalState::new(muxnix::Size { rows, cols });
        whole.feed(&input);

        let mut split = muxnix::TerminalState::new(muxnix::Size { rows, cols });
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
