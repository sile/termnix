fn term(rows: u16, cols: u16) -> termnix::TerminalState {
    termnix::TerminalState::new(termnix::Size { rows, cols })
}

fn text_at(term: &termnix::TerminalState, row: u16) -> String {
    let cols = term.size().cols;
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
    t.resize(termnix::Size { rows: 1, cols: 2 });
    assert_eq!(t.size(), termnix::Size { rows: 1, cols: 2 });
    assert_eq!(text_at(&t, 0), "ab");
    assert_eq!(t.cursor(), termnix::Position { row: 0, col: 1 });
}

#[test]
fn resize_clears_clipped_wide_character() {
    let mut t = term(1, 4);
    t.feed("あ".as_bytes());
    t.resize(termnix::Size { rows: 1, cols: 1 });
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
