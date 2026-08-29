//! Property tests for chunk-independent terminal feeding.

fn sample_control(ctx: &mut noprop::TestCaseContext) -> u8 {
    const CONTROLS: [u8; 5] = [0x07, 0x08, 0x09, 0x0a, 0x0d];
    CONTROLS[noprop::sample_usize_in(ctx, 0..CONTROLS.len())]
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
    let len = noprop::sample_usize_in(ctx, 0..=64);
    let mut bytes = Vec::with_capacity(len);
    for _ in 0..len {
        bytes.push(sample_byte(ctx));
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

fn feed_with_cuts(term: &mut muxnix::Terminal, bytes: &[u8], cuts: &[usize]) {
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

    noprop::Runner::new(seed).run(1024, |ctx| {
        let rows = noprop::sample_usize_in(ctx, 1..=6) as u16;
        let cols = noprop::sample_usize_in(ctx, 1..=12) as u16;
        let input = sample_input(ctx);
        if !input.is_empty() {
            saw_nonempty.set(true);
        }
        let cuts = sample_cuts(ctx, input.len());
        if !cuts.is_empty() && cuts.iter().any(|&c| c > 0 && c < input.len()) {
            saw_split.set(true);
        }

        let mut whole = muxnix::Terminal::new(muxnix::Size { rows, cols });
        whole.feed(&input);

        let mut split = muxnix::Terminal::new(muxnix::Size { rows, cols });
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
    Ok(())
}
