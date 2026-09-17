//! Public value types for the terminal emulator.

/// Zero-based position on the active screen.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct Position {
    /// Zero-based row.
    pub row: u16,
    /// Zero-based column.
    pub col: u16,
}

impl Position {
    /// The origin of the screen, `row` and `col` both zero.
    pub const ORIGIN: Self = Self { row: 0, col: 0 };
}

/// Cell color.
///
/// Indexed values use the usual ANSI/xterm numbering: 0–15 are the system
/// palette, 16–255 are the 256-color cube and grayscale ramp. SGR color forms
/// follow ECMA-48 / ITU T.416 practice (`38;5`, `38;2`, …) and may gain new
/// encodings in host terminals; termnix stores the decoded color.
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

impl Color {
    /// Resolves this color to concrete 24-bit RGB.
    ///
    /// [`Color::Default`] has no fixed value (it is whatever the host terminal
    /// uses for default foreground/background) and returns `None`.
    /// [`Color::Rgb`] is returned unchanged. [`Color::Indexed`] is resolved
    /// through the xterm 256-color palette.
    ///
    /// The indexed interpretation is the usual xterm convention: indices
    /// `0..=15` are the system palette, `16..=231` are the 6x6x6 color cube,
    /// and `232..=255` are the grayscale ramp. Host terminals may override the
    /// first 16 entries, so an application that needs exact on-screen colors
    /// should consult its own palette instead of relying on these values.
    pub fn to_rgb(&self) -> Option<(u8, u8, u8)> {
        match *self {
            Self::Default => None,
            Self::Rgb(r, g, b) => Some((r, g, b)),
            Self::Indexed(index) => Some(indexed_rgb(index)),
        }
    }
}

/// The 0--15 ANSI/xterm system palette.
const XTERM_SYSTEM: [(u8, u8, u8); 16] = [
    (0, 0, 0),
    (205, 0, 0),
    (0, 205, 0),
    (205, 205, 0),
    (0, 0, 238),
    (205, 0, 205),
    (0, 205, 205),
    (229, 229, 229),
    (127, 127, 127),
    (255, 0, 0),
    (0, 255, 0),
    (255, 255, 0),
    (92, 92, 255),
    (255, 0, 255),
    (0, 255, 255),
    (255, 255, 255),
];

/// Resolves an xterm 256-color index to concrete RGB.
///
/// See [`Color::to_rgb`] for the palette layout.
fn indexed_rgb(index: u8) -> (u8, u8, u8) {
    match index {
        0..=15 => XTERM_SYSTEM[index as usize],
        16..=231 => {
            let n = index - 16;
            let r = palette_level(n / 36);
            let g = palette_level((n % 36) / 6);
            let b = palette_level(n % 6);
            (r, g, b)
        }
        232..=255 => {
            let level = 8 + (index - 232) * 10;
            (level, level, level)
        }
    }
}

/// Maps a 6-level color-cube component index to its 0--255 intensity.
fn palette_level(index: u8) -> u8 {
    if index == 0 { 0 } else { 55 + index * 40 }
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
    ///
    /// Identical to [`Cell::CONTINUATION`] except for [`Cell::width`], which is
    /// `1` here and `0` there.
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
    ///
    /// Identical to [`Cell::EMPTY`] except for [`Cell::width`], which is `0`
    /// here and `1` there. Choose between the two by width, not by the glyph.
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
/// host terminal practice; termnix only stores which reporting style is active.
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

#[cfg(test)]
mod tests {
    use super::Color;

    #[test]
    fn default_has_no_rgb() {
        assert_eq!(Color::Default.to_rgb(), None);
    }

    #[test]
    fn rgb_is_returned_unchanged() {
        assert_eq!(Color::Rgb(1, 2, 3).to_rgb(), Some((1, 2, 3)));
        assert_eq!(Color::Rgb(255, 0, 128).to_rgb(), Some((255, 0, 128)));
    }

    #[test]
    fn indexed_system_palette_boundaries() {
        assert_eq!(Color::Indexed(0).to_rgb(), Some((0, 0, 0)));
        assert_eq!(Color::Indexed(15).to_rgb(), Some((255, 255, 255)));
    }

    #[test]
    fn indexed_color_cube() {
        assert_eq!(Color::Indexed(16).to_rgb(), Some((0, 0, 0)));
        assert_eq!(Color::Indexed(21).to_rgb(), Some((0, 0, 255)));
        assert_eq!(Color::Indexed(196).to_rgb(), Some((255, 0, 0)));
        assert_eq!(Color::Indexed(231).to_rgb(), Some((255, 255, 255)));
    }

    #[test]
    fn indexed_grayscale_ramp() {
        assert_eq!(Color::Indexed(232).to_rgb(), Some((8, 8, 8)));
        assert_eq!(Color::Indexed(255).to_rgb(), Some((238, 238, 238)));
    }
}
