//! Logical input encoding for PTY-bound terminal modes.
//!
//! Converts keys and paste into byte sequences using an explicitly
//! supplied [`TerminalModes`](crate::TerminalModes) value. Host focus and the
//! local tty mode are never consulted.
//!
//! Plain UTF-8 text and already-formed byte sequences are written by the
//! caller; this module only covers mode-sensitive encoding.
//!
//! # Encoding references
//!
//! Cursor and keypad forms follow common xterm / VT practice (DECCKM, DECNKM /
//! application keypad). Bracketed paste uses the xterm markers `CSI 200~` /
//! `CSI 201~`. These identifiers may evolve with host terminals; termnix encodes
//! the usual sequences and stores modes separately from I/O.
//!
//! Mouse report byte sequences are out of scope here; only [`MouseButton`]
//! is defined so application-side routing can share button identity. Grid
//! coordinates reuse [`crate::Position`].

use crate::terminal_types::TerminalModes;

/// Modifier keys held with a logical key.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub struct Modifiers {
    /// Control.
    pub ctrl: bool,
    /// Alt / Meta.
    pub alt: bool,
    /// Shift.
    pub shift: bool,
}

impl Modifiers {
    /// No modifiers.
    pub const fn new() -> Self {
        Self {
            ctrl: false,
            alt: false,
            shift: false,
        }
    }

    /// Sets Control and returns the updated value.
    pub const fn ctrl(self) -> Self {
        Self { ctrl: true, ..self }
    }

    /// Sets Alt / Meta and returns the updated value.
    pub const fn alt(self) -> Self {
        Self { alt: true, ..self }
    }

    /// Sets Shift and returns the updated value.
    pub const fn shift(self) -> Self {
        Self {
            shift: true,
            ..self
        }
    }
}

/// Logical key without host-toolkit bindings.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum KeyCode {
    /// Unicode character (after layout), not a named control key.
    Char(char),
    /// Enter / Return.
    Enter,
    /// Tab.
    Tab,
    /// Backspace.
    Backspace,
    /// Escape.
    Escape,
    /// Up arrow.
    Up,
    /// Down arrow.
    Down,
    /// Left arrow.
    Left,
    /// Right arrow.
    Right,
    /// Home.
    Home,
    /// End.
    End,
    /// Page Up.
    PageUp,
    /// Page Down.
    PageDown,
    /// Insert.
    Insert,
    /// Delete (forward).
    Delete,
    /// Function key `1..=12`.
    Function(u8),
    /// Numeric keypad digit `0..=9`.
    KeypadDigit(u8),
    /// Keypad decimal separator.
    KeypadDecimal,
    /// Keypad enter.
    KeypadEnter,
    /// Keypad add.
    KeypadAdd,
    /// Keypad subtract.
    KeypadSubtract,
    /// Keypad multiply.
    KeypadMultiply,
    /// Keypad divide.
    KeypadDivide,
}

/// A logical key press with modifiers.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct KeyEvent {
    /// Key identity.
    pub code: KeyCode,
    /// Held modifiers.
    pub modifiers: Modifiers,
}

impl KeyEvent {
    /// Builds a key event with no modifiers.
    pub fn new(code: KeyCode) -> Self {
        Self {
            code,
            modifiers: Modifiers::new(),
        }
    }
}

/// Mouse button identity for later report encoding.
///
/// Report coordinates use [`crate::Position`] (grid-local, zero-based cells).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum MouseButton {
    /// Left button.
    Left,
    /// Middle button.
    Middle,
    /// Right button.
    Right,
    /// Wheel up.
    WheelUp,
    /// Wheel down.
    WheelDown,
}

/// Encodes a logical key for the given terminal modes.
///
/// Modes must be passed explicitly (typically from the destination terminal
/// state's [`TerminalState::modes`](crate::TerminalState::modes)). Nothing
/// global is read.
pub fn encode_key(event: KeyEvent, modes: TerminalModes) -> Vec<u8> {
    let mut out = Vec::new();
    if event.modifiers.alt {
        out.push(0x1b);
    }
    encode_key_body(&mut out, event, modes);
    out
}

/// Encodes a paste payload, wrapping with bracketed-paste markers when enabled.
pub fn encode_paste(text: &str, modes: TerminalModes) -> Vec<u8> {
    if modes.bracketed_paste {
        let mut out = Vec::with_capacity(text.len() + 12);
        out.extend_from_slice(b"\x1b[200~");
        out.extend_from_slice(text.as_bytes());
        out.extend_from_slice(b"\x1b[201~");
        out
    } else {
        text.as_bytes().to_vec()
    }
}

fn encode_key_body(out: &mut Vec<u8>, event: KeyEvent, modes: TerminalModes) {
    let mods = event.modifiers;
    match event.code {
        KeyCode::Char(ch) => encode_char(out, ch, mods),
        KeyCode::Enter => out.push(b'\r'),
        KeyCode::Tab => out.push(b'\t'),
        KeyCode::Backspace => out.push(0x7f),
        KeyCode::Escape => out.push(0x1b),
        KeyCode::Up => encode_cursor(out, modes.application_cursor, b'A'),
        KeyCode::Down => encode_cursor(out, modes.application_cursor, b'B'),
        KeyCode::Right => encode_cursor(out, modes.application_cursor, b'C'),
        KeyCode::Left => encode_cursor(out, modes.application_cursor, b'D'),
        KeyCode::Home => encode_cursor(out, modes.application_cursor, b'H'),
        KeyCode::End => encode_cursor(out, modes.application_cursor, b'F'),
        KeyCode::Insert => out.extend_from_slice(b"\x1b[2~"),
        KeyCode::Delete => out.extend_from_slice(b"\x1b[3~"),
        KeyCode::PageUp => out.extend_from_slice(b"\x1b[5~"),
        KeyCode::PageDown => out.extend_from_slice(b"\x1b[6~"),
        KeyCode::Function(n) => encode_function(out, n),
        KeyCode::KeypadDigit(d) => encode_keypad_digit(out, d, modes.application_keypad),
        KeyCode::KeypadDecimal => encode_keypad(out, modes.application_keypad, b'.', b'n'),
        KeyCode::KeypadEnter => {
            if modes.application_keypad {
                out.extend_from_slice(b"\x1bOM");
            } else {
                out.push(b'\r');
            }
        }
        KeyCode::KeypadAdd => encode_keypad(out, modes.application_keypad, b'+', b'k'),
        KeyCode::KeypadSubtract => encode_keypad(out, modes.application_keypad, b'-', b'm'),
        KeyCode::KeypadMultiply => encode_keypad(out, modes.application_keypad, b'*', b'j'),
        KeyCode::KeypadDivide => encode_keypad(out, modes.application_keypad, b'/', b'o'),
    }
}

fn encode_char(out: &mut Vec<u8>, ch: char, mods: Modifiers) {
    if mods.ctrl
        && let Some(byte) = ctrl_byte(ch)
    {
        out.push(byte);
        return;
    }
    let mut buf = [0u8; 4];
    out.extend_from_slice(ch.encode_utf8(&mut buf).as_bytes());
}

fn ctrl_byte(ch: char) -> Option<u8> {
    match ch {
        '@' | ' ' => Some(0x00),
        'a'..='z' => Some((ch as u8) - b'a' + 1),
        'A'..='Z' => Some((ch as u8) - b'A' + 1),
        '[' | '3' => Some(0x1b),
        '\\' | '4' => Some(0x1c),
        ']' | '5' => Some(0x1d),
        '^' | '6' => Some(0x1e),
        '_' | '7' | '/' => Some(0x1f),
        '?' => Some(0x7f),
        c if c.is_ascii() && (c as u8) < 0x20 => Some(c as u8),
        _ => None,
    }
}

fn encode_cursor(out: &mut Vec<u8>, application: bool, final_byte: u8) {
    if application {
        out.extend_from_slice(&[0x1b, b'O', final_byte]);
    } else {
        out.extend_from_slice(&[0x1b, b'[', final_byte]);
    }
}

fn encode_function(out: &mut Vec<u8>, n: u8) {
    // xterm defaults: F1–F4 via SS3, F5–F12 via CSI n~.
    match n {
        1 => out.extend_from_slice(b"\x1bOP"),
        2 => out.extend_from_slice(b"\x1bOQ"),
        3 => out.extend_from_slice(b"\x1bOR"),
        4 => out.extend_from_slice(b"\x1bOS"),
        5 => out.extend_from_slice(b"\x1b[15~"),
        6 => out.extend_from_slice(b"\x1b[17~"),
        7 => out.extend_from_slice(b"\x1b[18~"),
        8 => out.extend_from_slice(b"\x1b[19~"),
        9 => out.extend_from_slice(b"\x1b[20~"),
        10 => out.extend_from_slice(b"\x1b[21~"),
        11 => out.extend_from_slice(b"\x1b[23~"),
        12 => out.extend_from_slice(b"\x1b[24~"),
        _ => {}
    }
}

fn encode_keypad_digit(out: &mut Vec<u8>, digit: u8, application: bool) {
    if digit > 9 {
        return;
    }
    if application {
        // Application keypad: ESC O p..y for 0..9.
        out.extend_from_slice(&[0x1b, b'O', b'p' + digit]);
    } else {
        out.push(b'0' + digit);
    }
}

fn encode_keypad(out: &mut Vec<u8>, application: bool, normal: u8, app_final: u8) {
    if application {
        out.extend_from_slice(&[0x1b, b'O', app_final]);
    } else {
        out.push(normal);
    }
}
