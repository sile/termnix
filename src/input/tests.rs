use super::*;
use crate::terminal_types::{MouseReporting, TerminalModes};

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

fn modes_mouse(mode: MouseReporting, sgr: bool) -> TerminalModes {
    TerminalModes {
        mouse: mode,
        mouse_sgr: sgr,
        ..TerminalModes::default()
    }
}

fn mouse(kind: MouseEventKind, row: u16, col: u16) -> MouseEvent {
    MouseEvent {
        kind,
        position: Position { row, col },
        modifiers: Modifiers::new(),
    }
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
fn mouse_off_emits_nothing() {
    let event = mouse(MouseEventKind::Press(MouseButton::Left), 4, 9);
    assert!(bytes(Input::Mouse(event), modes_normal()).is_empty());
    assert_eq!(Input::Mouse(event).byte_len(modes_normal()), 0);
}

#[test]
fn sgr_press_release_and_coordinates_are_one_based() {
    let modes = modes_mouse(MouseReporting::Normal, true);
    // Zero-based (row 4, col 9) is 1-based (x 10, y 5).
    let press = mouse(MouseEventKind::Press(MouseButton::Left), 4, 9);
    assert_eq!(bytes(Input::Mouse(press), modes), b"\x1b[<0;10;5M".to_vec());
    let release = mouse(MouseEventKind::Release(MouseButton::Left), 4, 9);
    assert_eq!(
        bytes(Input::Mouse(release), modes),
        b"\x1b[<0;10;5m".to_vec()
    );
}

#[test]
fn sgr_button_codes() {
    let modes = modes_mouse(MouseReporting::Normal, true);
    let at = |b| mouse(MouseEventKind::Press(b), 0, 0);
    assert_eq!(
        bytes(Input::Mouse(at(MouseButton::Middle)), modes),
        b"\x1b[<1;1;1M".to_vec()
    );
    assert_eq!(
        bytes(Input::Mouse(at(MouseButton::Right)), modes),
        b"\x1b[<2;1;1M".to_vec()
    );
    assert_eq!(
        bytes(Input::Mouse(at(MouseButton::WheelUp)), modes),
        b"\x1b[<64;1;1M".to_vec()
    );
    assert_eq!(
        bytes(Input::Mouse(at(MouseButton::WheelDown)), modes),
        b"\x1b[<65;1;1M".to_vec()
    );
}

#[test]
fn legacy_form_uses_byte_offsets() {
    let modes = modes_mouse(MouseReporting::Normal, false);
    // b=0, x=10, y=5 -> 0x20, 0x2a, 0x25.
    let press = mouse(MouseEventKind::Press(MouseButton::Left), 4, 9);
    assert_eq!(
        bytes(Input::Mouse(press), modes),
        vec![0x1b, b'[', b'M', 0x20, 0x2a, 0x25]
    );
    // Release encodes as button 3 in the legacy form, not the `m` suffix.
    let release = mouse(MouseEventKind::Release(MouseButton::Left), 4, 9);
    assert_eq!(
        bytes(Input::Mouse(release), modes),
        vec![0x1b, b'[', b'M', 0x23, 0x2a, 0x25]
    );
}

#[test]
fn legacy_coordinates_above_223_are_clamped() {
    let modes = modes_mouse(MouseReporting::Normal, false);
    let press = mouse(MouseEventKind::Press(MouseButton::Left), 300, 400);
    assert_eq!(
        bytes(Input::Mouse(press), modes),
        vec![0x1b, b'[', b'M', 0x20, 0xff, 0xff]
    );
    // 223 is the last exact cell; 224 clamps.
    let last = mouse(MouseEventKind::Press(MouseButton::Left), 222, 222);
    assert_eq!(
        bytes(Input::Mouse(last), modes),
        vec![0x1b, b'[', b'M', 0x20, 0xff, 0xff]
    );
    let next = mouse(MouseEventKind::Press(MouseButton::Left), 223, 223);
    assert_eq!(
        bytes(Input::Mouse(next), modes),
        vec![0x1b, b'[', b'M', 0x20, 0xff, 0xff]
    );
}

#[test]
fn x10_reports_only_button_presses() {
    let modes = modes_mouse(MouseReporting::X10, true);
    let press = mouse(MouseEventKind::Press(MouseButton::Left), 0, 0);
    assert_eq!(bytes(Input::Mouse(press), modes), b"\x1b[<0;1;1M".to_vec());

    let release = mouse(MouseEventKind::Release(MouseButton::Left), 0, 0);
    assert!(bytes(Input::Mouse(release), modes).is_empty());

    // The wheel was added after ?9; an X10 child must not receive it.
    let wheel = mouse(MouseEventKind::Press(MouseButton::WheelUp), 0, 0);
    assert!(bytes(Input::Mouse(wheel), modes).is_empty());
}

#[test]
fn normal_tracking_ignores_motion() {
    let modes = modes_mouse(MouseReporting::Normal, true);
    let motion = mouse(
        MouseEventKind::Motion {
            button: Some(MouseButton::Left),
        },
        0,
        0,
    );
    assert!(bytes(Input::Mouse(motion), modes).is_empty());
}

#[test]
fn button_event_tracking_reports_drag_but_not_bare_motion() {
    let modes = modes_mouse(MouseReporting::ButtonEvent, true);
    let drag = mouse(
        MouseEventKind::Motion {
            button: Some(MouseButton::Left),
        },
        9,
        19,
    );
    // Button 0 with the motion bit (32): x=20, y=10.
    assert_eq!(
        bytes(Input::Mouse(drag), modes),
        b"\x1b[<32;20;10M".to_vec()
    );

    let bare = mouse(MouseEventKind::Motion { button: None }, 9, 19);
    assert!(bytes(Input::Mouse(bare), modes).is_empty());
}

#[test]
fn any_event_tracking_reports_bare_motion_as_button_three() {
    let modes = modes_mouse(MouseReporting::AnyEvent, true);
    let bare = mouse(MouseEventKind::Motion { button: None }, 0, 0);
    // Button 3 (no button) plus the motion bit: 35.
    assert_eq!(bytes(Input::Mouse(bare), modes), b"\x1b[<35;1;1M".to_vec());
}

#[test]
fn modifiers_add_xterm_bits_in_sgr_and_legacy() {
    let sgr = modes_mouse(MouseReporting::Normal, true);
    let legacy = modes_mouse(MouseReporting::Normal, false);
    let event = MouseEvent {
        kind: MouseEventKind::Press(MouseButton::Left),
        position: Position { row: 0, col: 0 },
        modifiers: Modifiers::new().shift().alt().ctrl(),
    };
    // 0 + shift 4 + alt 8 + ctrl 16 = 28.
    assert_eq!(bytes(Input::Mouse(event), sgr), b"\x1b[<28;1;1M".to_vec());
    assert_eq!(
        bytes(Input::Mouse(event), legacy),
        vec![0x1b, b'[', b'M', 0x20 + 28, 0x21, 0x21]
    );
}

#[test]
fn mouse_byte_len_matches_written_bytes_and_can_be_zero() {
    let press = Input::Mouse(mouse(MouseEventKind::Press(MouseButton::Left), 4, 9));
    let modes = modes_mouse(MouseReporting::Normal, true);
    assert_eq!(press.byte_len(modes), bytes(press, modes).len());
    assert_eq!(press.byte_len(modes_normal()), 0);
}
