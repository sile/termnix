//! Public value types for the terminal emulator.

/// One physical row saved into scrollback, oldest-first.
///
/// Cells are owned in left-to-right order and are never reflowed by later
/// resizes, unlike the visible rows returned by
/// [`TerminalState::rows()`](crate::TerminalState::rows), which are re-laid out
/// on resize.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ScrollbackLine {
    cells: Vec<Cell>,
}

impl ScrollbackLine {
    pub(crate) fn new(cells: Vec<Cell>) -> Self {
        Self { cells }
    }

    /// Returns the row's cells in left-to-right order.
    pub fn cells(&self) -> &[Cell] {
        &self.cells
    }

    /// Returns the number of cells in the line (the row width at save time).
    pub fn len(&self) -> usize {
        self.cells.len()
    }

    /// Returns whether the line has no cells.
    pub fn is_empty(&self) -> bool {
        self.cells.is_empty()
    }
}

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

/// A 24-bit RGB colour, one byte per channel.
///
/// The channels are the direct value of a colour, not an index into a palette.
/// A palette index stays a `u8`, and a host resolves it through
/// [`TerminalState::palette_color()`](crate::TerminalState::palette_color).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct Rgb {
    /// The red channel.
    pub r: u8,
    /// The green channel.
    pub g: u8,
    /// The blue channel.
    pub b: u8,
}

impl Rgb {
    /// Builds a colour from its three channels.
    pub const fn new(r: u8, g: u8, b: u8) -> Self {
        Self { r, g, b }
    }
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
    /// Resolves this color to concrete 24-bit [`Rgb`].
    ///
    /// [`Color::Default`] has no fixed value (it is whatever the host terminal
    /// uses for default foreground/background) and returns `None`.
    /// [`Color::Rgb`] is returned unchanged. [`Color::Indexed`] is resolved
    /// through the xterm 256-color palette.
    ///
    /// The indexed interpretation is the usual xterm convention: indices
    /// `0..=15` are the system palette, `16..=231` are the 6x6x6 color cube,
    /// and `232..=255` are the grayscale ramp.
    ///
    /// This always resolves the *built-in default* table. A child can redefine
    /// a palette entry with OSC 4, and this method never sees that override, so
    /// a host rendering a cell that carries [`Color::Indexed`] should resolve
    /// the index through
    /// [`TerminalState::palette_color()`](crate::TerminalState::palette_color)
    /// instead - which falls back to this table for an entry the child never
    /// touched. The split is deliberate: `Color` is a `Copy` value type and
    /// cannot read the terminal's mutable state.
    pub fn to_rgb(&self) -> Option<Rgb> {
        match *self {
            Self::Default => None,
            Self::Rgb(r, g, b) => Some(Rgb::new(r, g, b)),
            Self::Indexed(index) => Some(indexed_rgb(index)),
        }
    }
}

/// The 0--15 ANSI/xterm system palette.
const XTERM_SYSTEM: [Rgb; 16] = [
    Rgb::new(0, 0, 0),
    Rgb::new(205, 0, 0),
    Rgb::new(0, 205, 0),
    Rgb::new(205, 205, 0),
    Rgb::new(0, 0, 238),
    Rgb::new(205, 0, 205),
    Rgb::new(0, 205, 205),
    Rgb::new(229, 229, 229),
    Rgb::new(127, 127, 127),
    Rgb::new(255, 0, 0),
    Rgb::new(0, 255, 0),
    Rgb::new(255, 255, 0),
    Rgb::new(92, 92, 255),
    Rgb::new(255, 0, 255),
    Rgb::new(0, 255, 255),
    Rgb::new(255, 255, 255),
];

/// Resolves an xterm 256-color index to concrete RGB.
///
/// See [`Color::to_rgb`] for the palette layout. This is the *built-in* table;
/// it knows nothing about a palette override the child set with OSC 4, which
/// [`TerminalState::palette_color`](crate::TerminalState::palette_color) layers
/// on top for the same index.
pub(crate) fn indexed_rgb(index: u8) -> Rgb {
    match index {
        0..=15 => XTERM_SYSTEM[index as usize],
        16..=231 => {
            let n = index - 16;
            let r = palette_level(n / 36);
            let g = palette_level((n % 36) / 6);
            let b = palette_level(n % 6);
            Rgb::new(r, g, b)
        }
        232..=255 => {
            let level = 8 + (index - 232) * 10;
            Rgb::new(level, level, level)
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

/// The selection an OSC 52 sequence addressed.
///
/// OSC 52 names the selection it targets (`ESC ] 52 ; <Pc> ; <Pd> ST`). `c` and
/// `p` are the two termnix gives a meaning to; any other name is kept in
/// [`ClipboardSelection::Other`] rather than dropped, so a caller that honours
/// one can still see it. Names follow the xterm convention and may grow with
/// host terminal practice; an unmodeled name lands in `Other` instead of being
/// discarded, which is what keeps a later selection name from needing a new
/// variant.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum ClipboardSelection {
    /// The system clipboard (`c`), also what a missing selection means.
    Clipboard,
    /// The primary selection (`p`).
    Primary,
    /// A selection name termnix does not model, kept as written.
    ///
    /// The name is raw bytes rather than a string: `Pc` is whatever the child
    /// sent between the framing semicolons and is not required to be valid
    /// UTF-8, and a name the caller may have to hand back to a host clipboard
    /// is worth keeping unrepaired.
    Other(Vec<u8>),
}

/// Something a child asked for in an OSC sequence, retained for the caller.
///
/// termnix interprets a handful of command sequences and does not act on the
/// resources they name itself (a clipboard, a bell). For those it records the
/// ask and leaves carrying it out to the caller - a write to make, a bell to
/// ring, or, for a clipboard read, a question only the caller can answer. A
/// sequence it does not interpret at all is also offered, under
/// [`ChildRequest::OtherOsc`], so an unmodelled number reaches a caller that
/// knows it instead of being discarded. Every such ask arrives as
/// `Event::RequestReceived`, one variant at a time; the enum is the whole
/// family of asks, so the one a caller cares about is an arm it matches rather
/// than a method it remembers to call.
///
/// A request is not merely an observation: it is something the caller is
/// expected to do, and it is not merged with the others, so three asks in one
/// `feed` are three asks to carry out.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ChildRequest {
    /// An OSC 52 clipboard write (`ESC ] 52 ; <Pc> ; <Pd> ST`).
    ///
    /// The sequence asks the terminal to change a selection; termnix does not
    /// own a clipboard, so the ask is what a caller receives.
    SetClipboard {
        /// Decoded selection text.
        ///
        /// Raw bytes rather than a string: OSC 52 carries an opaque payload,
        /// so the decoded bytes need not be valid UTF-8 and must not be
        /// repaired (what the child asked to copy is whatever it asked to
        /// copy).
        ///
        /// An empty value is meaningful: `ESC ] 52 ; c ; ST` asks for the
        /// selection to be cleared, which is not the same as no request at
        /// all.
        text: Vec<u8>,
        /// Which selection the application addressed.
        selection: ClipboardSelection,
        /// Whether the application asked to append instead of replace.
        ///
        /// This is the `+` prefix on the payload
        /// (`ESC ] 52 ; c ; +<b64> ST`), xterm's append form. It is the
        /// child's own distinction, recorded verbatim so a caller mirroring
        /// the text into a host clipboard can mirror an append as an append.
        append: bool,
    },
    /// An OSC 52 clipboard read (`ESC ] 52 ; <Pc> ; ? ST`).
    ///
    /// The sequence asks the terminal to send a selection's contents back to
    /// the child. termnix owns no clipboard, so it cannot answer and writes
    /// nothing to the reply buffer: the caller that owns the selection is the
    /// one that answers, by writing an OSC 52 set sequence back to the PTY
    /// master. A caller that ignores this variant leaves the child waiting,
    /// which is what a caller that only forwards writes will do.
    GetClipboard {
        /// Which selection the application asked for.
        selection: ClipboardSelection,
    },
    /// The child rang the bell (`BEL`, `0x07`).
    ///
    /// termnix has no way to ring anything, so it only records that the child
    /// asked for one; making a sound (or flashing, or notifying) is the
    /// caller's job. Bells are not merged: `BEL BEL` is two asks, and a caller
    /// that wants a burst collapsed counts them itself.
    RingBell,
    /// An OSC sequence termnix does not interpret, handed over as written.
    ///
    /// The crate frames every OSC and routes it by identifier; one it has no
    /// meaning for is not dropped but offered, so a caller that does know the
    /// number (a working-directory announcement, a prompt mark, a vendor
    /// extension) can act on it without tokenizing the PTY stream itself.
    ///
    /// The variant carries no action verb, unlike the others, because the
    /// crate did not interpret the sequence and so cannot say which direction
    /// it points: the child may have asked for a change, asked for contents,
    /// or merely reported something. `OtherOsc` keeps the frame in the name
    /// because that much is known - the sequence is an OSC, and this is the
    /// home for the ones termnix leaves unread.
    OtherOsc {
        /// The identifier field (`<number>` in `ESC ] <number> ; ... ST`).
        ///
        /// Raw bytes rather than a number or a string: identifiers are
        /// conventional rather than numeric, so a vendor extension may use a
        /// non-integer one, and the field is not required to be valid UTF-8.
        /// A caller that knows its own identifier compares bytes.
        id: Vec<u8>,
        /// The argument fields, split on `;` exactly as the tokenizer framed
        /// them and otherwise untouched.
        ///
        /// Empty when the sequence had no arguments. The split is framing, not
        /// interpretation: the crate knows where an OSC's fields end, so it
        /// passes them on whole rather than making every caller re-split the
        /// same bytes. Nothing past the split has been decoded.
        params: Vec<Vec<u8>>,
    },
}

/// Something that happened in the terminal that a caller may want to react to.
///
/// A session driver reacts to what the child does through one channel: it
/// drains [`TerminalState::next_event()`], matching each event. The events are
/// of two kinds. Some report that state moved - the screen, the retained
/// history, the window title - and are merged, so several changes of the same
/// kind in one [`feed()`](crate::TerminalState::feed) arrive as one event; a
/// caller reads the current state through the same accessor it always used
/// ([`rows()`](crate::TerminalState::rows),
/// [`scrollback_lines()`](crate::TerminalState::scrollback_lines),
/// [`title()`](crate::TerminalState::title)). The others are requests:
/// [`Event::RequestReceived`] carries one [`ChildRequest`] the child asked for
/// and termnix cannot carry out itself, and the whole sequence is delivered in
/// order.
///
/// [`TerminalState::next_event()`]: crate::TerminalState::next_event
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Event {
    /// The whole terminal was reset (RIS, `ESC c`).
    ///
    /// Everything derived from the child before this point is stale: the
    /// screen is blank, the cursor is at the origin, the modes and pen are
    /// back to their defaults, the title is cleared, and the retained history
    /// is gone. A caller holding state mirrored from the child (a rendered
    /// frame, a clipboard it filled from an OSC 52 request) should discard it.
    /// termnix has already performed the reset by the time this is reported;
    /// the event exists so the caller knows its own derivations are stale.
    TerminalReset,
    /// The visible screen changed.
    ///
    /// Scrollback-only changes and undrained replies do not raise this, and it
    /// says only *whether* the screen moved, not what changed: a single `feed`
    /// that writes many cells still reports one `ScreenUpdated`. Treat it as
    /// "repaint to be safe": a write of a cell over the value it already held
    /// counts, while input with no visible effect (an ignored sequence, a
    /// partial escape) does not.
    ScreenUpdated,
    /// A line was appended to the retained history.
    ///
    /// The history only ever grows here: a full-screen scroll on the primary
    /// screen pushes the displaced rows onto the back. Nothing is rewritten, so
    /// this never means an existing line changed; trimming the history is the
    /// caller's own action
    /// ([`trim_scrollback()`](crate::TerminalState::trim_scrollback)) and is
    /// not reported. Read the new lines through
    /// [`scrollback_lines()`](crate::TerminalState::scrollback_lines).
    ScrollbackLineAppended,
    /// The window title changed (OSC 0 / OSC 2, or a reset clearing it).
    ///
    /// The title is state termnix holds; read it through
    /// [`title()`](crate::TerminalState::title).
    TitleUpdated,
    /// The child asked for something termnix cannot do itself.
    ///
    /// Carries one [`ChildRequest`]. Requests are not merged, so a caller that
    /// drains until `None` sees every ask in the order the child sent them,
    /// including several that arrived in one `feed`.
    RequestReceived(ChildRequest),
}

#[cfg(test)]
mod tests {
    use super::{Color, Rgb};

    #[test]
    fn default_has_no_rgb() {
        assert_eq!(Color::Default.to_rgb(), None);
    }

    #[test]
    fn rgb_is_returned_unchanged() {
        assert_eq!(Color::Rgb(1, 2, 3).to_rgb(), Some(Rgb::new(1, 2, 3)));
        assert_eq!(
            Color::Rgb(255, 0, 128).to_rgb(),
            Some(Rgb::new(255, 0, 128))
        );
    }

    #[test]
    fn indexed_system_palette_boundaries() {
        assert_eq!(Color::Indexed(0).to_rgb(), Some(Rgb::new(0, 0, 0)));
        assert_eq!(Color::Indexed(15).to_rgb(), Some(Rgb::new(255, 255, 255)));
    }

    #[test]
    fn indexed_color_cube() {
        assert_eq!(Color::Indexed(16).to_rgb(), Some(Rgb::new(0, 0, 0)));
        assert_eq!(Color::Indexed(21).to_rgb(), Some(Rgb::new(0, 0, 255)));
        assert_eq!(Color::Indexed(196).to_rgb(), Some(Rgb::new(255, 0, 0)));
        assert_eq!(Color::Indexed(231).to_rgb(), Some(Rgb::new(255, 255, 255)));
    }

    #[test]
    fn indexed_grayscale_ramp() {
        assert_eq!(Color::Indexed(232).to_rgb(), Some(Rgb::new(8, 8, 8)));
        assert_eq!(Color::Indexed(255).to_rgb(), Some(Rgb::new(238, 238, 238)));
    }
}
