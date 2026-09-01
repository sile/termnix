fn modes_normal() -> termnix::TerminalModes {
    termnix::TerminalModes::default()
}

fn modes_app_cursor() -> termnix::TerminalModes {
    termnix::TerminalModes {
        application_cursor: true,
        ..termnix::TerminalModes::default()
    }
}

fn modes_app_keypad() -> termnix::TerminalModes {
    termnix::TerminalModes {
        application_keypad: true,
        ..termnix::TerminalModes::default()
    }
}

fn modes_bracketed_paste() -> termnix::TerminalModes {
    termnix::TerminalModes {
        bracketed_paste: true,
        ..termnix::TerminalModes::default()
    }
}

fn key(code: termnix::KeyCode) -> termnix::KeyEvent {
    termnix::KeyEvent::new(code)
}

fn key_mods(code: termnix::KeyCode, modifiers: termnix::Modifiers) -> termnix::KeyEvent {
    termnix::KeyEvent { code, modifiers }
}

#[test]
fn arrow_keys_differ_between_normal_and_application_cursor() {
    let up = key(termnix::KeyCode::Up);
    assert_eq!(termnix::encode_key(up, modes_normal()), b"\x1b[A".to_vec());
    assert_eq!(
        termnix::encode_key(up, modes_app_cursor()),
        b"\x1bOA".to_vec()
    );

    let left = key(termnix::KeyCode::Left);
    assert_eq!(termnix::encode_key(left, modes_normal()), b"\x1b[D".to_vec());
    assert_eq!(
        termnix::encode_key(left, modes_app_cursor()),
        b"\x1bOD".to_vec()
    );
}

#[test]
fn keypad_digit_differs_between_normal_and_application_keypad() {
    let five = key(termnix::KeyCode::KeypadDigit(5));
    assert_eq!(termnix::encode_key(five, modes_normal()), b"5".to_vec());
    assert_eq!(
        termnix::encode_key(five, modes_app_keypad()),
        b"\x1bOu".to_vec()
    );
}

#[test]
fn ctrl_and_alt_modifiers_apply_to_characters() {
    let ctrl_a = key_mods(termnix::KeyCode::Char('a'), termnix::Modifiers::new().ctrl());
    assert_eq!(termnix::encode_key(ctrl_a, modes_normal()), b"\x01".to_vec());

    let alt_x = key_mods(termnix::KeyCode::Char('x'), termnix::Modifiers::new().alt());
    assert_eq!(termnix::encode_key(alt_x, modes_normal()), b"\x1bx".to_vec());

    let ctrl_alt_c = key_mods(
        termnix::KeyCode::Char('c'),
        termnix::Modifiers::new().ctrl().alt(),
    );
    assert_eq!(
        termnix::encode_key(ctrl_alt_c, modes_normal()),
        b"\x1b\x03".to_vec()
    );
}

#[test]
fn bracketed_paste_markers_depend_on_mode() {
    let payload = "pasted";
    assert_eq!(
        termnix::encode_paste(payload, modes_normal()),
        b"pasted".to_vec()
    );
    assert_eq!(
        termnix::encode_paste(payload, modes_bracketed_paste()),
        b"\x1b[200~pasted\x1b[201~".to_vec()
    );
}

#[test]
fn encoding_uses_only_the_modes_argument() {
    // Same key + same modes must be stable regardless of any other state.
    let event = key(termnix::KeyCode::Down);
    let a = termnix::encode_key(event, modes_app_cursor());
    let b = termnix::encode_key(event, modes_app_cursor());
    assert_eq!(a, b);
    assert_eq!(a, b"\x1bOB".to_vec());
}

#[test]
fn enter_tab_backspace_escape() {
    assert_eq!(
        termnix::encode_key(key(termnix::KeyCode::Enter), modes_normal()),
        b"\r".to_vec()
    );
    assert_eq!(
        termnix::encode_key(key(termnix::KeyCode::Tab), modes_normal()),
        b"\t".to_vec()
    );
    assert_eq!(
        termnix::encode_key(key(termnix::KeyCode::Backspace), modes_normal()),
        b"\x7f".to_vec()
    );
    assert_eq!(
        termnix::encode_key(key(termnix::KeyCode::Escape), modes_normal()),
        b"\x1b".to_vec()
    );
}

#[test]
fn function_keys_use_xterm_defaults() {
    assert_eq!(
        termnix::encode_key(key(termnix::KeyCode::Function(1)), modes_normal()),
        b"\x1bOP".to_vec()
    );
    assert_eq!(
        termnix::encode_key(key(termnix::KeyCode::Function(5)), modes_normal()),
        b"\x1b[15~".to_vec()
    );
    assert_eq!(
        termnix::encode_key(key(termnix::KeyCode::Function(12)), modes_normal()),
        b"\x1b[24~".to_vec()
    );
}

#[test]
fn mouse_button_and_position_are_available_for_later_reporting() {
    let _pos = termnix::Position { row: 1, col: 2 };
    let _btn = termnix::MouseButton::Left;
}
