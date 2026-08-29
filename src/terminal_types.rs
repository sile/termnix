//! Public value types for the terminal emulator.

/// Zero-based position on the active screen.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct Position {
    /// Zero-based row.
    pub row: u16,
    /// Zero-based column.
    pub col: u16,
}

/// Cell color.
///
/// Indexed values use the usual ANSI/xterm numbering: 0–15 are the system
/// palette, 16–255 are the 256-color cube and grayscale ramp. SGR color forms
/// follow ECMA-48 / ITU T.416 practice (`38;5`, `38;2`, …) and may gain new
/// encodings in host terminals; muxnix stores the decoded color.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub enum Color {
    /// Terminal default foreground or background.
    #[default]
    Default,
    /// Palette or 256-color index (`0..=255`).
    Indexed(u8),
    /// Direct 24-bit color.
    Rgb(u8, u8, u8),
}

/// Graphic rendition applied to a cell.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub struct Style {
    /// Foreground color.
    pub foreground: Color,
    /// Background color.
    pub background: Color,
    /// Bold / increased intensity (SGR 1).
    pub bold: bool,
    /// Italic (SGR 3).
    pub italic: bool,
    /// Underline (SGR 4).
    pub underline: bool,
    /// Reverse video (SGR 7).
    pub reverse: bool,
}

/// One cell on the active screen.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct Cell {
    /// Glyph stored in this cell.
    ///
    /// Empty cells use `' '`. Wide-character continuation cells also use
    /// `' '` with [`Cell::width`] equal to `0`.
    pub ch: char,
    /// Display width contributed by this cell: `1` or `2` for a leading
    /// glyph cell, or `0` for the trailing half of a width-2 glyph.
    pub width: u8,
    /// Graphic rendition for this cell.
    pub style: Style,
}

impl Cell {
    /// An empty single-column cell with default style.
    pub const EMPTY: Self = Self {
        ch: ' ',
        width: 1,
        style: Style {
            foreground: Color::Default,
            background: Color::Default,
            bold: false,
            italic: false,
            underline: false,
            reverse: false,
        },
    };

    /// Trailing half of a width-2 glyph.
    pub const CONTINUATION: Self = Self {
        ch: ' ',
        width: 0,
        style: Style {
            foreground: Color::Default,
            background: Color::Default,
            bold: false,
            italic: false,
            underline: false,
            reverse: false,
        },
    };

    pub(crate) fn blank(style: Style) -> Self {
        Self {
            ch: ' ',
            width: 1,
            style,
        }
    }
}

/// Mouse reporting mode retained for input encoding and snapshots.
///
/// Mode numbers follow common xterm DEC private modes and may change with
/// host terminal practice; muxnix only stores which reporting style is active.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub enum MouseReporting {
    /// Mouse reports disabled.
    #[default]
    Off,
    /// X10 mouse reporting (`?9h`).
    X10,
    /// Normal tracking (`?1000h`).
    Normal,
    /// Button-event tracking (`?1002h`).
    ButtonEvent,
    /// Any-event tracking (`?1003h`).
    AnyEvent,
}

impl MouseReporting {
    /// Returns true when any mouse reporting mode is enabled.
    pub fn is_active(self) -> bool {
        !matches!(self, Self::Off)
    }
}

/// Terminal modes that affect display and later input encoding.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct TerminalModes {
    /// DEC auto-wrap mode (`?7`); default on.
    pub autowrap: bool,
    /// DEC origin mode (`?6`); default off.
    pub origin: bool,
    /// IRM insert mode (SM/RM 4); default off (replace).
    pub insert: bool,
    /// Application cursor keys (`?1`); default off (normal).
    pub application_cursor: bool,
    /// Application keypad (`?66`); default off.
    pub application_keypad: bool,
    /// Bracketed paste (`?2004`); default off.
    pub bracketed_paste: bool,
    /// Cursor visibility (`?25`); default on.
    pub cursor_visible: bool,
    /// Mouse reporting style.
    pub mouse: MouseReporting,
    /// SGR mouse encoding (`?1006`).
    pub mouse_sgr: bool,
}

impl Default for TerminalModes {
    fn default() -> Self {
        Self {
            autowrap: true,
            origin: false,
            insert: false,
            application_cursor: false,
            application_keypad: false,
            bracketed_paste: false,
            cursor_visible: true,
            mouse: MouseReporting::Off,
            mouse_sgr: false,
        }
    }
}

/// Side effect produced while feeding the emulator.
///
/// The emulator never writes to a PTY itself; the caller applies these
/// actions (typically by writing to the PTY master).
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum TerminalAction {
    /// Bytes that should be written to the PTY master (for example CPR or DA replies).
    WritePty(Vec<u8>),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct SavedCursor {
    pub cursor: Position,
    pub pen: Style,
    pub wrap_pending: bool,
    pub origin: bool,
}

impl Default for SavedCursor {
    fn default() -> Self {
        Self {
            cursor: Position { row: 0, col: 0 },
            pen: Style::default(),
            wrap_pending: false,
            origin: false,
        }
    }
}
