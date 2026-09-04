use super::*;
use crate::terminal_types::TerminalModes;

fn modes_normal() -> TerminalModes {
    TerminalModes::default()
}

fn modes_app_cursor() -> TerminalModes {
    TerminalModes {
        application_cursor: true,
        ..TerminalModes::default()
    }
}

fn modes_app_keypad() -> TerminalModes {
    TerminalModes {
        application_keypad: true,
        ..TerminalModes::default()
    }
}

fn modes_bracketed_paste() -> TerminalModes {
    TerminalModes {
        bracketed_paste: true,
        ..TerminalModes::default()
    }
}

fn key(code: KeyCode) -> KeyEvent {
    KeyEvent::new(code)
}

fn key_mods(code: KeyCode, modifiers: Modifiers) -> KeyEvent {
    KeyEvent { code, modifiers }
}

fn bytes(input: Input<'_>, modes: TerminalModes) -> Vec<u8> {
    let mut out = Vec::new();
    input.write_to(modes, &mut out);
    out
}

#[test]
fn arrow_keys_differ_between_normal_and_application_cursor() {
    let up = key(KeyCode::Up);
    assert_eq!(bytes(Input::Key(up), modes_normal()), b"\x1b[A".to_vec());
    assert_eq!(
        bytes(Input::Key(up), modes_app_cursor()),
        b"\x1bOA".to_vec()
    );

    let left = key(KeyCode::Left);
    assert_eq!(bytes(Input::Key(left), modes_normal()), b"\x1b[D".to_vec());
    assert_eq!(
        bytes(Input::Key(left), modes_app_cursor()),
        b"\x1bOD".to_vec()
    );
}

#[test]
fn keypad_digit_differs_between_normal_and_application_keypad() {
    let five = key(KeyCode::KeypadDigit(5));
    assert_eq!(bytes(Input::Key(five), modes_normal()), b"5".to_vec());
    assert_eq!(
        bytes(Input::Key(five), modes_app_keypad()),
        b"\x1bOu".to_vec()
    );
}

#[test]
fn ctrl_and_alt_modifiers_apply_to_characters() {
    let ctrl_a = key_mods(KeyCode::Char('a'), Modifiers::new().ctrl());
    assert_eq!(bytes(Input::Key(ctrl_a), modes_normal()), b"\x01".to_vec());

    let alt_x = key_mods(KeyCode::Char('x'), Modifiers::new().alt());
    assert_eq!(bytes(Input::Key(alt_x), modes_normal()), b"\x1bx".to_vec());

    let ctrl_alt_c = key_mods(KeyCode::Char('c'), Modifiers::new().ctrl().alt());
    assert_eq!(
        bytes(Input::Key(ctrl_alt_c), modes_normal()),
        b"\x1b\x03".to_vec()
    );
}

#[test]
fn bracketed_paste_markers_depend_on_mode() {
    let payload = "pasted";
    assert_eq!(
        bytes(Input::Paste(payload), modes_normal()),
        b"pasted".to_vec()
    );
    assert_eq!(
        bytes(Input::Paste(payload), modes_bracketed_paste()),
        b"\x1b[200~pasted\x1b[201~".to_vec()
    );
}

#[test]
fn encoding_uses_only_the_modes_argument() {
    let event = key(KeyCode::Down);
    let a = bytes(Input::Key(event), modes_app_cursor());
    let b = bytes(Input::Key(event), modes_app_cursor());
    assert_eq!(a, b);
    assert_eq!(a, b"\x1bOB".to_vec());
}

#[test]
fn enter_tab_backspace_escape() {
    assert_eq!(
        bytes(Input::Key(key(KeyCode::Enter)), modes_normal()),
        b"\r".to_vec()
    );
    assert_eq!(
        bytes(Input::Key(key(KeyCode::Tab)), modes_normal()),
        b"\t".to_vec()
    );
    assert_eq!(
        bytes(Input::Key(key(KeyCode::Backspace)), modes_normal()),
        b"\x7f".to_vec()
    );
    assert_eq!(
        bytes(Input::Key(key(KeyCode::Escape)), modes_normal()),
        b"\x1b".to_vec()
    );
}

#[test]
fn function_keys_use_xterm_defaults() {
    assert_eq!(
        bytes(Input::Key(key(KeyCode::Function(1))), modes_normal()),
        b"\x1bOP".to_vec()
    );
    assert_eq!(
        bytes(Input::Key(key(KeyCode::Function(5))), modes_normal()),
        b"\x1b[15~".to_vec()
    );
    assert_eq!(
        bytes(Input::Key(key(KeyCode::Function(12))), modes_normal()),
        b"\x1b[24~".to_vec()
    );
}

#[test]
fn byte_len_matches_written_bytes() {
    let key = Input::Key(key(KeyCode::Up));
    assert_eq!(
        key.byte_len(modes_normal()),
        bytes(key, modes_normal()).len()
    );
    let paste = Input::Paste("hi");
    assert_eq!(
        paste.byte_len(modes_bracketed_paste()),
        bytes(paste, modes_bracketed_paste()).len()
    );
    let raw = Input::Raw(b"abc");
    assert_eq!(raw.byte_len(modes_normal()), 3);
}

#[test]
fn mouse_button_and_position_are_available_for_later_reporting() {
    let _pos = crate::Position { row: 1, col: 2 };
    let _btn = MouseButton::Left;
}
