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
    let payload = b"pasted";
    assert_eq!(
        bytes(Input::Paste(payload), modes_normal()),
        b"pasted".to_vec()
    );
    assert_eq!(
        bytes(Input::Paste(payload), modes_bracketed_paste()),
        b"\x1b[200~pasted\x1b[201~".to_vec()
    );
}

/// A paste payload is bytes, not text, so it must survive a round trip even
/// when it is not valid UTF-8: the markers wrap every byte, and the raw form
/// appends every byte. Refusing to send it, or replacing the invalid bytes,
/// would lose data the host asked to forward.
#[test]
fn paste_payload_may_be_non_utf8() {
    // 0xff and 0xfe are never valid UTF-8 lead bytes, and 0x80 is a lone
    // continuation byte, so this cannot be decoded as text at all.
    let payload: &[u8] = &[b'A', 0xff, 0xfe, 0x80, b'B'];
    assert_eq!(
        bytes(Input::Paste(payload), modes_normal()),
        vec![b'A', 0xff, 0xfe, 0x80, b'B']
    );
    assert_eq!(
        bytes(Input::Paste(payload), modes_bracketed_paste()),
        vec![
            0x1b, b'[', b'2', b'0', b'0', b'~', b'A', 0xff, 0xfe, 0x80, b'B', 0x1b, b'[', b'2',
            b'0', b'1', b'~'
        ]
    );
    assert_eq!(
        Input::Paste(payload).byte_len(modes_bracketed_paste()),
        bytes(Input::Paste(payload), modes_bracketed_paste()).len()
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
    let paste = Input::Paste(b"hi");
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

/// Every combination of tracking mode, event kind, and encoding, with the
/// bytes each must produce (or `None` when the mode does not report it). This
/// pins the whole `should_report` + `button_code` matrix in one place, so a
/// regression in any single cell fails here rather than only in the mode that
/// happens to have a focused test.
#[test]
fn reporting_matrix_covers_every_mode_and_kind() {
    let left = MouseButton::Left;
    let middle = MouseButton::Middle;
    let right = MouseButton::Right;
    let wheel_up = MouseButton::WheelUp;
    let drag = MouseEventKind::Motion { button: Some(left) };
    let bare = MouseEventKind::Motion { button: None };

    // (mode, sgr, kind, expected bytes)
    let cases: &[(MouseReporting, bool, MouseEventKind, Option<&[u8]>)] = &[
        // Off: nothing, in either encoding.
        (MouseReporting::Off, true, MouseEventKind::Press(left), None),
        (
            MouseReporting::Off,
            false,
            MouseEventKind::Press(left),
            None,
        ),
        // X10: only left/middle/right presses; no releases, motions, wheels.
        (
            MouseReporting::X10,
            true,
            MouseEventKind::Press(left),
            Some(b"\x1b[<0;1;1M"),
        ),
        (
            MouseReporting::X10,
            true,
            MouseEventKind::Press(middle),
            Some(b"\x1b[<1;1;1M"),
        ),
        (
            MouseReporting::X10,
            true,
            MouseEventKind::Press(right),
            Some(b"\x1b[<2;1;1M"),
        ),
        (
            MouseReporting::X10,
            true,
            MouseEventKind::Press(wheel_up),
            None,
        ),
        (
            MouseReporting::X10,
            true,
            MouseEventKind::Release(left),
            None,
        ),
        (MouseReporting::X10, true, drag, None),
        (MouseReporting::X10, true, bare, None),
        // Normal: press and release, no motion.
        (
            MouseReporting::Normal,
            true,
            MouseEventKind::Press(left),
            Some(b"\x1b[<0;1;1M"),
        ),
        (
            MouseReporting::Normal,
            true,
            MouseEventKind::Release(left),
            Some(b"\x1b[<0;1;1m"),
        ),
        (
            MouseReporting::Normal,
            true,
            MouseEventKind::Press(wheel_up),
            Some(b"\x1b[<64;1;1M"),
        ),
        (MouseReporting::Normal, true, drag, None),
        (MouseReporting::Normal, true, bare, None),
        // ButtonEvent: press, release, and drags; no bare motion.
        (
            MouseReporting::ButtonEvent,
            true,
            MouseEventKind::Press(left),
            Some(b"\x1b[<0;1;1M"),
        ),
        (
            MouseReporting::ButtonEvent,
            true,
            MouseEventKind::Release(left),
            Some(b"\x1b[<0;1;1m"),
        ),
        (
            MouseReporting::ButtonEvent,
            true,
            drag,
            Some(b"\x1b[<32;1;1M"),
        ),
        (MouseReporting::ButtonEvent, true, bare, None),
        // AnyEvent: everything, bare motion as button 3 + motion bit.
        (
            MouseReporting::AnyEvent,
            true,
            MouseEventKind::Press(left),
            Some(b"\x1b[<0;1;1M"),
        ),
        (
            MouseReporting::AnyEvent,
            true,
            MouseEventKind::Release(left),
            Some(b"\x1b[<0;1;1m"),
        ),
        (MouseReporting::AnyEvent, true, drag, Some(b"\x1b[<32;1;1M")),
        (MouseReporting::AnyEvent, true, bare, Some(b"\x1b[<35;1;1M")),
    ];

    for &(mode, sgr, kind, expected) in cases {
        let modes = modes_mouse(mode, sgr);
        let input = Input::Mouse(mouse(kind, 0, 0));
        let got = bytes(input, modes);
        match expected {
            Some(want) => {
                assert_eq!(got, want, "{mode:?} sgr={sgr} {kind:?}");
                assert_eq!(
                    input.byte_len(modes),
                    want.len(),
                    "byte_len {mode:?} {kind:?}"
                );
            }
            None => {
                assert!(
                    got.is_empty(),
                    "{mode:?} sgr={sgr} {kind:?} should be silent"
                );
                assert_eq!(input.byte_len(modes), 0, "byte_len {mode:?} {kind:?}");
            }
        }
    }
}

/// The release button code differs by encoding: SGR keeps the real button
/// code and marks the release with a trailing `m`; the legacy form cannot,
/// so it always spells release as button `3`. Check every button, since the
/// SGR case was the subtle one.
#[test]
fn sgr_release_keeps_button_code_and_legacy_release_is_three() {
    let sgr = modes_mouse(MouseReporting::Normal, true);
    let legacy = modes_mouse(MouseReporting::Normal, false);
    let releases = [
        (MouseButton::Left, 0u8),
        (MouseButton::Middle, 1),
        (MouseButton::Right, 2),
    ];
    for (button, code) in releases {
        let event = mouse(MouseEventKind::Release(button), 0, 0);
        let sgr_expected = format!("\x1b[<{code};1;1m").into_bytes();
        assert_eq!(
            bytes(Input::Mouse(event), sgr),
            sgr_expected,
            "sgr {button:?}"
        );
        assert_eq!(
            bytes(Input::Mouse(event), legacy),
            vec![0x1b, b'[', b'M', 0x20 + 3, 0x21, 0x21],
            "legacy {button:?}"
        );
    }
}

/// Motion under `?1002`/`?1003` carries the held button and the motion bit;
/// a drag with the middle or right button must report that button, not left.
/// This is the case a `Motion` variant that discarded its button would fail.
#[test]
fn drag_reports_the_held_button() {
    let modes = modes_mouse(MouseReporting::ButtonEvent, true);
    let drags = [
        (MouseButton::Left, 32u32),
        (MouseButton::Middle, 33),
        (MouseButton::Right, 34),
    ];
    for (button, code) in drags {
        let event = mouse(
            MouseEventKind::Motion {
                button: Some(button),
            },
            0,
            0,
        );
        let expected = format!("\x1b[<{code};1;1M").into_bytes();
        assert_eq!(bytes(Input::Mouse(event), modes), expected, "{button:?}");
    }
}

/// Each modifier bit must land in its own position; checking them only as a
/// sum would let a swap between two bits pass.
#[test]
fn modifiers_set_independent_bits() {
    let modes = modes_mouse(MouseReporting::Normal, true);
    let single = [
        (Modifiers::new().shift(), 4u32),
        (Modifiers::new().alt(), 8),
        (Modifiers::new().ctrl(), 16),
    ];
    for (modifiers, code) in single {
        let event = MouseEvent {
            kind: MouseEventKind::Press(MouseButton::Left),
            position: Position { row: 0, col: 0 },
            modifiers,
        };
        let expected = format!("\x1b[<{code};1;1M").into_bytes();
        assert_eq!(bytes(Input::Mouse(event), modes), expected, "{modifiers:?}");
    }
}

/// The clamp only touches the legacy coordinate bytes; SGR must carry the
/// exact coordinate past 223, and both dimensions clamp independently.
#[test]
fn clamping_is_independent_per_axis_and_legacy_only() {
    let legacy = modes_mouse(MouseReporting::Normal, false);
    let sgr = modes_mouse(MouseReporting::Normal, true);

    // x clamps, y stays exact.
    let wide = mouse(MouseEventKind::Press(MouseButton::Left), 9, 500);
    assert_eq!(
        bytes(Input::Mouse(wide), legacy),
        vec![0x1b, b'[', b'M', 0x20, 0xff, 0x21 + 9]
    );
    // y clamps, x stays exact.
    let tall = mouse(MouseEventKind::Press(MouseButton::Left), 500, 9);
    assert_eq!(
        bytes(Input::Mouse(tall), legacy),
        vec![0x1b, b'[', b'M', 0x20, 0x21 + 9, 0xff]
    );
    // SGR is not clamped.
    assert_eq!(bytes(Input::Mouse(wide), sgr), b"\x1b[<0;501;10M".to_vec());
}

// ---------------------------------------------------------------------------
// Property tests (noprop)
// ---------------------------------------------------------------------------
//
// The fixed tests above pin named cells of the report matrix. These property
// tests sweep the whole input domain instead, so a regression that happens to
// miss every hand-picked value is still caught. The oracle is a differential
// model: `mouse_encode_model` re-encodes an event straight from the xterm
// report layout, sharing no helper with `write_mouse`, so a wrong constant
// cannot agree with itself.

/// Four sampling modes other than `Off`, named once so the generator weights
/// and the gate messages cannot drift apart.
const ACTIVE_MOUSE_MODES: [MouseReporting; 4] = [
    MouseReporting::X10,
    MouseReporting::Normal,
    MouseReporting::ButtonEvent,
    MouseReporting::AnyEvent,
];

const ALL_MOUSE_BUTTONS: [MouseButton; 5] = [
    MouseButton::Left,
    MouseButton::Middle,
    MouseButton::Right,
    MouseButton::WheelUp,
    MouseButton::WheelDown,
];

/// Highest value the legacy `CSI M` coordinate byte can carry, mirrored from
/// `LEGACY_COORD_MAX` so the model does not borrow the encoder's constant.
const MODEL_LEGACY_COORD_MAX: u32 = 0xff - 0x20;

/// Draws a coordinate with both sides of the legacy clamp as first-class
/// boundaries. `222`, `223`, `224` bracket the limit so an off-by-one clamp is
/// still reachable, and `u16::MAX` reaches the largest expressible cell.
fn sample_coord(ctx: &mut noprop::TestCaseContext) -> u16 {
    noprop::sample_with_boundaries(
        ctx,
        &[0u16, 222, 223, 224, u16::MAX],
        noprop::Ratio::one_nth(3),
        |ctx| noprop::sample_usize_in(ctx, 0..=usize::from(u16::MAX)) as u16,
    )
}

fn sample_button(ctx: &mut noprop::TestCaseContext) -> MouseButton {
    noprop::sample_choice(ctx, &ALL_MOUSE_BUTTONS)
}

/// Draws the three modifier bits from one integer so all eight combinations
/// are reachable. Independent draws would make "all" and "none" each 1/8,
/// so the every-bit-set gate would fire only rarely.
fn sample_modifiers(ctx: &mut noprop::TestCaseContext) -> Modifiers {
    let bits = noprop::sample_usize_in(ctx, 0..8);
    Modifiers {
        ctrl: bits & 1 != 0,
        alt: bits & 2 != 0,
        shift: bits & 4 != 0,
    }
}

/// Press 3, Release 3, drag 2, bare move 2. The bare move is the arm only
/// `?1003` reports, so it must not be rarer than a drag.
fn sample_kind(ctx: &mut noprop::TestCaseContext) -> MouseEventKind {
    match noprop::sample_weighted_index(ctx, &[3, 3, 2, 2]) {
        0 => MouseEventKind::Press(sample_button(ctx)),
        1 => MouseEventKind::Release(sample_button(ctx)),
        2 => MouseEventKind::Motion {
            button: Some(sample_button(ctx)),
        },
        _ => MouseEventKind::Motion { button: None },
    }
}

/// Uniform over the five tracking modes and over both encodings, so reporting
/// off and each wire form appear under every mode.
fn sample_reporting(ctx: &mut noprop::TestCaseContext) -> (MouseReporting, bool) {
    let mode = match noprop::sample_usize_in(ctx, 0..5) {
        0 => MouseReporting::Off,
        1 => ACTIVE_MOUSE_MODES[0],
        2 => ACTIVE_MOUSE_MODES[1],
        3 => ACTIVE_MOUSE_MODES[2],
        _ => ACTIVE_MOUSE_MODES[3],
    };
    (mode, noprop::sample_bool(ctx))
}

fn mouse_button_code(button: MouseButton) -> u32 {
    match button {
        MouseButton::Left => 0,
        MouseButton::Middle => 1,
        MouseButton::Right => 2,
        MouseButton::WheelUp => 64,
        MouseButton::WheelDown => 65,
    }
}

/// Whether `mode` reports this kind, from the specification.
fn model_should_report(kind: MouseEventKind, mode: MouseReporting) -> bool {
    match mode {
        MouseReporting::Off => false,
        MouseReporting::X10 => matches!(
            kind,
            MouseEventKind::Press(MouseButton::Left | MouseButton::Middle | MouseButton::Right)
        ),
        MouseReporting::Normal => {
            matches!(kind, MouseEventKind::Press(_) | MouseEventKind::Release(_))
        }
        MouseReporting::ButtonEvent => match kind {
            MouseEventKind::Press(_) | MouseEventKind::Release(_) => true,
            MouseEventKind::Motion { button } => button.is_some(),
        },
        MouseReporting::AnyEvent => true,
    }
}

fn model_base_code(kind: MouseEventKind, sgr: bool) -> u32 {
    match kind {
        MouseEventKind::Press(button) => mouse_button_code(button),
        MouseEventKind::Release(button) => {
            if sgr {
                mouse_button_code(button)
            } else {
                3
            }
        }
        MouseEventKind::Motion { button } => button.map_or(3, mouse_button_code) | 32,
    }
}

/// Independent re-encoding of one event under one mode, straight from the
/// xterm report layout. It deliberately shares no helper with `src/input.rs`:
/// a shared helper would let a wrong constant agree with itself.
fn mouse_encode_model(event: MouseEvent, mode: MouseReporting, sgr: bool) -> Vec<u8> {
    if !model_should_report(event.kind, mode) {
        return Vec::new();
    }

    let mut code = model_base_code(event.kind, sgr);
    if event.modifiers.shift {
        code |= 4;
    }
    if event.modifiers.alt {
        code |= 8;
    }
    if event.modifiers.ctrl {
        code |= 16;
    }

    // Coordinates are 1-based on the wire; the event carries zero-based cells.
    let x = u32::from(event.position.col) + 1;
    let y = u32::from(event.position.row) + 1;

    if sgr {
        let final_byte = if matches!(event.kind, MouseEventKind::Release(_)) {
            b'm'
        } else {
            b'M'
        };
        let mut out = format!("\x1b[<{code};{x};{y}").into_bytes();
        out.push(final_byte);
        out
    } else {
        let x = x.min(MODEL_LEGACY_COORD_MAX) as u8;
        let y = y.min(MODEL_LEGACY_COORD_MAX) as u8;
        vec![0x1b, b'[', b'M', 0x20 + code as u8, 0x20 + x, 0x20 + y]
    }
}

#[test]
fn mouse_report_matches_the_model_over_the_whole_domain() -> noprop::TestResult {
    let seed = noprop::seed_from_env_or_time("TERMNIX_PBT_SEED")?;

    let reported = std::cell::Cell::new(0usize);
    let suppressed = std::cell::Cell::new(0usize);
    let clamped = std::cell::Cell::new(0usize);
    let sgr_release = std::cell::Cell::new(0usize);
    let legacy_release = std::cell::Cell::new(0usize);
    let modifier_bits = std::cell::Cell::new(0u8);

    let mut runner = noprop::Runner::new(seed);
    runner.run(2048, |ctx| {
        let event = MouseEvent {
            kind: sample_kind(ctx),
            position: Position {
                row: sample_coord(ctx),
                col: sample_coord(ctx),
            },
            modifiers: sample_modifiers(ctx),
        };
        let (mode, sgr) = sample_reporting(ctx);
        let modes = modes_mouse(mode, sgr);

        let mut out = Vec::new();
        Input::Mouse(event).write_to(modes, &mut out);
        let expected = mouse_encode_model(event, mode, sgr);
        assert_eq!(
            out, expected,
            "event={event:?} mode={mode:?} sgr={sgr}\nactual={:02x?}\nexpected={:02x?}",
            out, expected
        );

        // `byte_len` drives the write-queue budget, so it must equal the bytes
        // actually appended -- including the zero-byte case.
        assert_eq!(
            Input::Mouse(event).byte_len(modes),
            out.len(),
            "byte_len disagrees with write_to for event={event:?} mode={mode:?} sgr={sgr}"
        );

        // Gates update only after both assertions, and only for the case under
        // test, so a rejected or failing case leaves no evidence behind.
        if expected.is_empty() {
            suppressed.set(suppressed.get() + 1);
        } else {
            reported.set(reported.get() + 1);
            let x = usize::from(event.position.col) + 1;
            let y = usize::from(event.position.row) + 1;
            if !sgr && (x > MODEL_LEGACY_COORD_MAX as usize || y > MODEL_LEGACY_COORD_MAX as usize)
            {
                clamped.set(clamped.get() + 1);
            }
            if matches!(event.kind, MouseEventKind::Release(_)) {
                if sgr {
                    sgr_release.set(sgr_release.get() + 1);
                } else {
                    legacy_release.set(legacy_release.get() + 1);
                }
            }
        }
        let seen = modifier_bits.get()
            | (u8::from(event.modifiers.ctrl))
            | (u8::from(event.modifiers.alt) << 1)
            | (u8::from(event.modifiers.shift) << 2);
        modifier_bits.set(seen);
        Ok(())
    })?;

    assert!(reported.get() > 0, "no case produced a report\n{runner}");
    assert!(
        suppressed.get() > 0,
        "no case was suppressed by a disabled or untracked mode\n{runner}"
    );
    assert!(
        clamped.get() > 0,
        "no case exercised the legacy >=224 coordinate clamp\n{runner}"
    );
    assert!(
        sgr_release.get() > 0,
        "no SGR release case was reported\n{runner}"
    );
    assert!(
        legacy_release.get() > 0,
        "no legacy release case was reported\n{runner}"
    );
    assert_eq!(
        modifier_bits.get(),
        0b111,
        "not every modifier bit was exercised\n{runner}"
    );
    Ok(())
}

#[test]
fn x10_never_reports_wheel_or_motion() -> noprop::TestResult {
    let seed = noprop::seed_from_env_or_time("TERMNIX_PBT_SEED")?;
    let x10_wheel = std::cell::Cell::new(0usize);
    let x10_press = std::cell::Cell::new(0usize);

    let mut runner = noprop::Runner::new(seed);
    runner.run(2048, |ctx| {
        let button = sample_button(ctx);
        let sgr = noprop::sample_bool(ctx);
        let position = Position {
            row: sample_coord(ctx),
            col: sample_coord(ctx),
        };
        let modifiers = sample_modifiers(ctx);
        let event = MouseEvent {
            kind: MouseEventKind::Press(button),
            position,
            modifiers,
        };
        let got = bytes(Input::Mouse(event), modes_mouse(MouseReporting::X10, sgr));
        if matches!(button, MouseButton::WheelUp | MouseButton::WheelDown) {
            assert!(
                got.is_empty(),
                "X10 reported wheel button {button:?}: {got:02x?}"
            );
            x10_wheel.set(x10_wheel.get() + 1);
        } else {
            assert!(!got.is_empty(), "X10 dropped a {button:?} press");
            x10_press.set(x10_press.get() + 1);
        }

        // Motion is never part of X10, with or without a held button.
        for kind in [
            MouseEventKind::Motion {
                button: Some(button),
            },
            MouseEventKind::Motion { button: None },
        ] {
            let motion = MouseEvent {
                kind,
                position,
                modifiers,
            };
            assert!(
                bytes(Input::Mouse(motion), modes_mouse(MouseReporting::X10, sgr)).is_empty(),
                "X10 reported {kind:?}"
            );
        }
        Ok(())
    })?;

    assert!(
        x10_wheel.get() > 0,
        "no X10 wheel case was generated\n{runner}"
    );
    assert!(
        x10_press.get() > 0,
        "no X10 button press was generated\n{runner}"
    );
    Ok(())
}

#[test]
fn sgr_coordinates_are_unbounded_and_legacy_ones_clamp_per_axis() -> noprop::TestResult {
    let seed = noprop::seed_from_env_or_time("TERMNIX_PBT_SEED")?;
    let sgr_large = std::cell::Cell::new(0usize);

    let mut runner = noprop::Runner::new(seed);
    runner.run(2048, |ctx| {
        let col = sample_coord(ctx);
        let row = sample_coord(ctx);
        let button = sample_button(ctx);
        let event = MouseEvent {
            kind: MouseEventKind::Press(button),
            position: Position { row, col },
            modifiers: sample_modifiers(ctx),
        };

        // SGR carries the full 1-based coordinate whatever its magnitude.
        let sgr = bytes(
            Input::Mouse(event),
            modes_mouse(MouseReporting::Normal, true),
        );
        let mut code = mouse_button_code(button);
        if event.modifiers.shift {
            code |= 4;
        }
        if event.modifiers.alt {
            code |= 8;
        }
        if event.modifiers.ctrl {
            code |= 16;
        }
        let want = format!(
            "\x1b[<{};{};{}M",
            code,
            u32::from(col) + 1,
            u32::from(row) + 1
        )
        .into_bytes();
        assert_eq!(sgr, want, "SGR coordinate mismatch for {event:?}");
        if col >= 224 || row >= 224 {
            sgr_large.set(sgr_large.get() + 1);
        }

        // Legacy clamps each axis to 223, independently.
        let legacy = bytes(
            Input::Mouse(event),
            modes_mouse(MouseReporting::Normal, false),
        );
        let cx = u8::try_from((u32::from(col) + 1).min(MODEL_LEGACY_COORD_MAX))
            .expect("clamped x fits in u8");
        let cy = u8::try_from((u32::from(row) + 1).min(MODEL_LEGACY_COORD_MAX))
            .expect("clamped y fits in u8");
        let want = vec![0x1b, b'[', b'M', 0x20 + code as u8, 0x20 + cx, 0x20 + cy];
        assert_eq!(legacy, want, "legacy coordinate mismatch for {event:?}");
        assert_eq!(legacy.len(), 6, "the legacy form is always six bytes");
        Ok(())
    })?;

    assert!(
        sgr_large.get() > 0,
        "no case drove an SGR coordinate past the legacy limit\n{runner}"
    );
    Ok(())
}

/// One `NumBuffer` is reused across the coordinates of a single report, so a
/// later call must not read a stale prefix left by an earlier, longer one.
///
/// `format_into`'s own decimal output is std's contract, covered by std's
/// tests; what is ours is the reuse, which is what this pins.
#[test]
fn push_u32_reuses_one_buffer_without_stale_digits() {
    let mut scratch = NumBuffer::<u32>::new();
    let mut out = Vec::new();
    for value in [u32::MAX, 0u32, 7, u32::MAX, 12] {
        out.push(b';');
        push_u32(&mut out, &mut scratch, value);
    }
    assert_eq!(out, b";4294967295;0;7;4294967295;12");
}
