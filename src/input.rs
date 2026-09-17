//! Logical input for PTY-bound terminal sessions.
//!
//! [`Input`] carries raw bytes, a key event, or a paste payload.
//! [`Session`](crate::Session) turns `Key` / `Paste` into PTY bytes using the
//! session's current [`TerminalModes`](crate::TerminalModes) at enqueue time.
//! Host focus and the local tty mode are never consulted.
//!
//! # Encoding references
//!
//! Cursor and keypad forms follow common xterm / VT practice (DECCKM, DECNKM /
//! application keypad). Bracketed paste uses the xterm markers `CSI 200~` /
//! `CSI 201~`. These identifiers may evolve with host terminals; termnix stores
//! modes separately from I/O and applies them when enqueueing input.
//!
//! Mouse report byte sequences are out of scope here; only [`MouseButton`]
//! is defined so application-side routing can share button identity. Grid
//! coordinates reuse [`Position`](crate::Position).

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
/// Report coordinates use [`Position`](crate::Position) (grid-local, zero-based cells).
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

/// Application input to enqueue on a [`Session`](crate::Session).
///
/// `Key` and `Paste` are turned into PTY bytes with the session's current
/// modes at enqueue time. `Raw` is appended unchanged.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Input<'a> {
    /// Already-formed PTY bytes.
    Raw(&'a [u8]),
    /// A logical key press.
    Key(KeyEvent),
    /// Paste text; bracketed-paste markers follow the current modes.
    Paste(&'a str),
}

impl<'a> Input<'a> {
    /// Returns how many bytes this input would add to the write queue.
    ///
    /// For `Key` and `Paste`, `modes` selects the same sequences
    /// [`Session::enqueue_input()`](crate::Session::enqueue_input) would write.
    /// For `Raw`, the slice length is returned and `modes` is ignored.
    pub fn byte_len(self, modes: TerminalModes) -> usize {
        match self {
            Self::Raw(bytes) => bytes.len(),
            Self::Key(_) | Self::Paste(_) => {
                let mut buf = Vec::new();
                self.write_to(modes, &mut buf);
                buf.len()
            }
        }
    }

    /// Appends the PTY bytes for this input to `out`.
    pub(crate) fn write_to(self, modes: TerminalModes, out: &mut Vec<u8>) {
        match self {
            Self::Raw(bytes) => out.extend_from_slice(bytes),
            Self::Key(event) => write_key(out, event, modes),
            Self::Paste(text) => write_paste(out, text, modes),
        }
    }
}

fn write_key(out: &mut Vec<u8>, event: KeyEvent, modes: TerminalModes) {
    if event.modifiers.alt {
        out.push(0x1b);
    }
    write_key_body(out, event, modes);
}

fn write_paste(out: &mut Vec<u8>, text: &str, modes: TerminalModes) {
    if modes.bracketed_paste {
        out.extend_from_slice(b"\x1b[200~");
        out.extend_from_slice(text.as_bytes());
        out.extend_from_slice(b"\x1b[201~");
    } else {
        out.extend_from_slice(text.as_bytes());
    }
}

fn write_key_body(out: &mut Vec<u8>, event: KeyEvent, modes: TerminalModes) {
    let mods = event.modifiers;
    match event.code {
        KeyCode::Char(ch) => write_char(out, ch, mods),
        KeyCode::Enter => out.push(b'\r'),
        KeyCode::Tab => out.push(b'\t'),
        KeyCode::Backspace => out.push(0x7f),
        KeyCode::Escape => out.push(0x1b),
        KeyCode::Up => write_cursor(out, modes.application_cursor, b'A'),
        KeyCode::Down => write_cursor(out, modes.application_cursor, b'B'),
        KeyCode::Right => write_cursor(out, modes.application_cursor, b'C'),
        KeyCode::Left => write_cursor(out, modes.application_cursor, b'D'),
        KeyCode::Home => write_cursor(out, modes.application_cursor, b'H'),
        KeyCode::End => write_cursor(out, modes.application_cursor, b'F'),
        KeyCode::Insert => out.extend_from_slice(b"\x1b[2~"),
        KeyCode::Delete => out.extend_from_slice(b"\x1b[3~"),
        KeyCode::PageUp => out.extend_from_slice(b"\x1b[5~"),
        KeyCode::PageDown => out.extend_from_slice(b"\x1b[6~"),
        KeyCode::Function(n) => write_function(out, n),
        KeyCode::KeypadDigit(d) => write_keypad_digit(out, d, modes.application_keypad),
        KeyCode::KeypadDecimal => write_keypad(out, modes.application_keypad, b'.', b'n'),
        KeyCode::KeypadEnter => {
            if modes.application_keypad {
                out.extend_from_slice(b"\x1bOM");
            } else {
                out.push(b'\r');
            }
        }
        KeyCode::KeypadAdd => write_keypad(out, modes.application_keypad, b'+', b'k'),
        KeyCode::KeypadSubtract => write_keypad(out, modes.application_keypad, b'-', b'm'),
        KeyCode::KeypadMultiply => write_keypad(out, modes.application_keypad, b'*', b'j'),
        KeyCode::KeypadDivide => write_keypad(out, modes.application_keypad, b'/', b'o'),
    }
}

fn write_char(out: &mut Vec<u8>, ch: char, mods: Modifiers) {
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

fn write_cursor(out: &mut Vec<u8>, application: bool, final_byte: u8) {
    if application {
        out.extend_from_slice(&[0x1b, b'O', final_byte]);
    } else {
        out.extend_from_slice(&[0x1b, b'[', final_byte]);
    }
}

fn write_function(out: &mut Vec<u8>, n: u8) {
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

fn write_keypad_digit(out: &mut Vec<u8>, digit: u8, application: bool) {
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

fn write_keypad(out: &mut Vec<u8>, application: bool, normal: u8, app_final: u8) {
    if application {
        out.extend_from_slice(&[0x1b, b'O', app_final]);
    } else {
        out.push(normal);
    }
}

#[cfg(test)]
mod tests;
