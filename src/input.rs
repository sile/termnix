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
//! Mouse reports follow xterm's X10 / `?1000` / `?1002` / `?1003` tracking
//! modes and the SGR (`?1006`) encoding, chosen from the same modes at enqueue
//! time. Grid coordinates reuse [`Position`](crate::Position).

use crate::terminal_types::{MouseReporting, Position, TerminalModes};

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

/// Mouse button identity for report encoding.
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

impl MouseButton {
    /// The button's report-code low bits, as defined by the mouse protocol.
    ///
    /// The values only encode the button identity; the release and motion
    /// bits are added by the report encoder.
    fn code(self) -> u32 {
        match self {
            Self::Left => 0,
            Self::Middle => 1,
            Self::Right => 2,
            Self::WheelUp => 64,
            Self::WheelDown => 65,
        }
    }
}

/// What happened to the mouse.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum MouseEventKind {
    /// A button went down.
    Press(MouseButton),
    /// A button came up.
    Release(MouseButton),
    /// The pointer moved. `button` is the held button, if any.
    ///
    /// The caller tracks which button is down: a session keeps no
    /// mouse-button state, so a drag is reported as `Some(button)` and a bare
    /// move as `None`. A held button has to be supplied by the caller rather
    /// than inferred, or the encoder cannot tell a drag from a bare move
    /// under `?1002`. Tracking it in the session would also make
    /// [`Input::byte_len`] depend on hidden state instead of on the event and
    /// the modes alone.
    Motion {
        /// The button held while moving, or `None` for a bare move.
        button: Option<MouseButton>,
    },
}

/// A logical mouse event at a grid position.
///
/// Report bytes follow the child's current [`TerminalModes`] at enqueue time.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct MouseEvent {
    /// What happened.
    pub kind: MouseEventKind,
    /// Zero-based position on the active screen.
    pub position: Position,
    /// Held modifiers (Shift / Alt / Ctrl).
    pub modifiers: Modifiers,
}

/// Application input to enqueue on a [`Session`](crate::Session).
///
/// `Key`, `Paste`, and `Mouse` are turned into PTY bytes with the session's
/// current modes at enqueue time. `Raw` is appended unchanged.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Input<'a> {
    /// Already-formed PTY bytes.
    Raw(&'a [u8]),
    /// A logical key press.
    Key(KeyEvent),
    /// Paste text; bracketed-paste markers follow the current modes.
    Paste(&'a str),
    /// A mouse event; report bytes follow the current modes.
    Mouse(MouseEvent),
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
            Self::Key(_) | Self::Paste(_) | Self::Mouse(_) => {
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
            Self::Mouse(event) => write_mouse(out, event, modes),
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

/// Highest coordinate the legacy `CSI M` form can represent.
///
/// The coordinate bytes are `0x20 + n`, so `n` must fit below `0xff - 0x20`.
const LEGACY_COORD_MAX: u32 = 0xff - 0x20;

fn write_mouse(out: &mut Vec<u8>, event: MouseEvent, modes: TerminalModes) {
    if !should_report(event.kind, modes.mouse) {
        return;
    }

    let mut code = button_code(event.kind, modes.mouse_sgr);
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

    if modes.mouse_sgr {
        let final_byte = if is_release(event.kind) { b'm' } else { b'M' };
        out.extend_from_slice(b"\x1b[<");
        push_u32(out, code);
        out.push(b';');
        push_u32(out, x);
        out.push(b';');
        push_u32(out, y);
        out.push(final_byte);
    } else {
        // The legacy form cannot represent a coordinate above 223. Clamp, as
        // xterm does, rather than wrap to a wrong cell.
        let x = x.min(LEGACY_COORD_MAX) as u8;
        let y = y.min(LEGACY_COORD_MAX) as u8;
        out.extend_from_slice(&[0x1b, b'[', b'M', 0x20 + code as u8, 0x20 + x, 0x20 + y]);
    }
}

/// Returns whether `mode` tracks this event at all.
///
/// The wheel buttons (`64`/`65`) were added with `?1000`, after `?9`, so `X10`
/// reports only `Left`/`Middle`/`Right` presses.
fn should_report(kind: MouseEventKind, mode: MouseReporting) -> bool {
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

/// Low bits of the report code for a reported event.
///
/// A release is button `3` only in the legacy form, which has no other way to
/// spell it; SGR keeps the real button code and marks the release with a
/// trailing `m`. A bare move is also button `3` (no button held) plus the
/// motion bit.
fn button_code(kind: MouseEventKind, sgr: bool) -> u32 {
    match kind {
        MouseEventKind::Press(button) => button.code(),
        MouseEventKind::Release(button) => {
            if sgr {
                button.code()
            } else {
                3
            }
        }
        MouseEventKind::Motion { button } => button.map_or(3, MouseButton::code) | 32,
    }
}

fn is_release(kind: MouseEventKind) -> bool {
    matches!(kind, MouseEventKind::Release(_))
}

/// Appends a decimal `u32` without allocating.
fn push_u32(out: &mut Vec<u8>, mut value: u32) {
    if value == 0 {
        out.push(b'0');
        return;
    }
    let mut buf = [0u8; 10];
    let mut idx = buf.len();
    while value > 0 {
        idx -= 1;
        buf[idx] = b'0' + (value % 10) as u8;
        value /= 10;
    }
    out.extend_from_slice(&buf[idx..]);
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
