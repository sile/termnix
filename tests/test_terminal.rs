fn term(rows: u16, cols: u16) -> muxnix::TerminalState {
    muxnix::TerminalState::new(muxnix::Size { rows, cols })
}

fn text_at(term: &muxnix::TerminalState, row: u16) -> String {
    let cols = term.size().cols;
    let mut out = String::new();
    for col in 0..cols {
        let cell = term
            .cell(muxnix::Position { row, col })
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
    assert_eq!(t.cursor(), muxnix::Position { row: 0, col: 2 });
}

#[test]
fn carriage_return_and_line_feed() {
    let mut t = term(3, 8);
    t.feed(b"ab\r\ncd");
    assert_eq!(text_at(&t, 0), "ab");
    assert_eq!(text_at(&t, 1), "cd");
    assert_eq!(t.cursor(), muxnix::Position { row: 1, col: 2 });
}

#[test]
fn line_feed_alone_keeps_column() {
    let mut t = term(3, 8);
    t.feed(b"ab\ncd");
    assert_eq!(text_at(&t, 0), "ab");
    assert_eq!(text_at(&t, 1), "  cd");
    assert_eq!(t.cursor(), muxnix::Position { row: 1, col: 4 });
}

#[test]
fn backspace_moves_left_without_erasing() {
    let mut t = term(1, 8);
    t.feed(b"ab\x08c");
    assert_eq!(text_at(&t, 0), "ac");
    assert_eq!(t.cursor(), muxnix::Position { row: 0, col: 2 });
}

#[test]
fn horizontal_tab_moves_to_next_stop() {
    let mut t = term(1, 16);
    t.feed(b"a\tb");
    assert_eq!(
        t.cell(muxnix::Position { row: 0, col: 0 }),
        Some(muxnix::Cell { ch: 'a', width: 1 })
    );
    assert_eq!(
        t.cell(muxnix::Position { row: 0, col: 8 }),
        Some(muxnix::Cell { ch: 'b', width: 1 })
    );
    assert_eq!(t.cursor(), muxnix::Position { row: 0, col: 9 });
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
    assert_eq!(t.cursor(), muxnix::Position { row: 1, col: 1 });
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
        t.cell(muxnix::Position { row: 0, col: 0 }),
        Some(muxnix::Cell {
            ch: 'あ', width: 2
        })
    );
    assert_eq!(
        t.cell(muxnix::Position { row: 0, col: 1 }),
        Some(muxnix::Cell::CONTINUATION)
    );
    assert_eq!(t.cursor(), muxnix::Position { row: 0, col: 2 });
}

#[test]
fn overwriting_wide_character_clears_both_cells() {
    let mut t = term(1, 4);
    t.feed("あ".as_bytes());
    t.feed(b"\rx");
    assert_eq!(
        t.cell(muxnix::Position { row: 0, col: 0 }),
        Some(muxnix::Cell { ch: 'x', width: 1 })
    );
    assert_eq!(
        t.cell(muxnix::Position { row: 0, col: 1 }),
        Some(muxnix::Cell::EMPTY)
    );
}

#[test]
fn overwriting_continuation_clears_lead() {
    let mut t = term(1, 4);
    t.feed("あ".as_bytes());
    t.feed(b"\x08y");
    assert_eq!(
        t.cell(muxnix::Position { row: 0, col: 0 }),
        Some(muxnix::Cell::EMPTY)
    );
    assert_eq!(
        t.cell(muxnix::Position { row: 0, col: 1 }),
        Some(muxnix::Cell { ch: 'y', width: 1 })
    );
}

#[test]
fn wide_character_wraps_when_one_column_remains() {
    let mut t = term(2, 3);
    t.feed(b"ab");
    t.feed("あ".as_bytes());
    assert_eq!(text_at(&t, 0), "ab");
    assert_eq!(
        t.cell(muxnix::Position { row: 1, col: 0 }),
        Some(muxnix::Cell {
            ch: 'あ', width: 2
        })
    );
    assert_eq!(
        t.cell(muxnix::Position { row: 1, col: 1 }),
        Some(muxnix::Cell::CONTINUATION)
    );
}

#[test]
fn resize_preserves_overlap_and_clamps_cursor() {
    let mut t = term(2, 4);
    t.feed(b"abcd");
    t.feed(b"xy");
    t.resize(muxnix::Size { rows: 1, cols: 2 });
    assert_eq!(t.size(), muxnix::Size { rows: 1, cols: 2 });
    assert_eq!(text_at(&t, 0), "ab");
    assert_eq!(t.cursor(), muxnix::Position { row: 0, col: 1 });
}

#[test]
fn resize_clears_clipped_wide_character() {
    let mut t = term(1, 4);
    t.feed("あ".as_bytes());
    t.resize(muxnix::Size { rows: 1, cols: 1 });
    assert_eq!(
        t.cell(muxnix::Position { row: 0, col: 0 }),
        Some(muxnix::Cell::EMPTY)
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
        t.cell(muxnix::Position { row: 0, col: 0 }),
        Some(muxnix::Cell {
            ch: 'あ', width: 2
        })
    );
}

#[test]
fn invalid_utf8_becomes_replacement_character() {
    let mut t = term(1, 4);
    t.feed(&[0xff, b'x']);
    assert_eq!(
        t.cell(muxnix::Position { row: 0, col: 0 }),
        Some(muxnix::Cell {
            ch: '\u{FFFD}',
            width: 1
        })
    );
    assert_eq!(
        t.cell(muxnix::Position { row: 0, col: 1 }),
        Some(muxnix::Cell { ch: 'x', width: 1 })
    );
}

#[test]
fn combining_character_is_ignored() {
    let mut t = term(1, 4);
    t.feed("a\u{0301}".as_bytes());
    assert_eq!(text_at(&t, 0), "a");
    assert_eq!(t.cursor(), muxnix::Position { row: 0, col: 1 });
}

#[test]
fn feed_does_not_panic_on_controls_and_noise() {
    let mut t = term(2, 8);
    t.feed(&[0x00, 0x1b, 0x7f, 0x80, 0xff]);
    t.feed(b"ok");
    assert_eq!(text_at(&t, 0), "\u{FFFD}\u{FFFD}ok");
}
