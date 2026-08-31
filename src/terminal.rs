//! I/O-free terminal emulator state.
//!
//! [`TerminalState`] accepts PTY output through [`TerminalState::feed`] and
//! updates cells, styles, cursor, and modes. Query replies are returned as
//! [`TerminalAction`] values; this type never writes to a file descriptor.
//!
//! # Supported sequences (modes milestone)
//!
//! - C0: BEL (ignored), BS, HT, LF, VT, FF, CR
//! - ESC: IND (`D`), NEL (`E`), RI (`M`), DECSC/DECRC (`7`/`8`), RIS (`c`)
//! - CSI cursor: CUU/CUD/CUF/CUB, CNL/CPL, CHA/HPA, VPA, CUP/HVP
//! - CSI edit: ICH, DCH, IL, DL, ED, EL, ECH, SU, SD
//! - CSI scroll region: DECSTBM
//! - CSI modes: SM/RM including DEC private modes listed on [`TerminalModes`]
//! - CSI SGR (`m`): reset, bold, italic, underline, reverse, 16/256/24-bit color
//! - CSI queries: DSR, CPR, primary DA
//! - OSC 0/2: window title (stored); other OSC ignored without becoming text
//! - Alternate screen: DECSET/DECRST 1049 (also 47 / 1047)
//!
//! # Explicitly out of scope
//!
//! Sixel, Kitty graphics, iTerm2 image protocols, and DCS application payloads
//! are ignored by the tokenizer without corrupting subsequent text.
//!
//! Spec identifiers cited in implementation comments may change; treat them as
//! guidance rather than a compatibility guarantee.
//!
//! # Unicode policy
//!
//! - Display width uses the `unicode-width` crate (UAX #11) internally.
//! - Width 1 and 2 characters are placed on the active screen.
//! - Width 0 characters (for example combining marks) are ignored.
//! - Invalid UTF-8 becomes U+FFFD (width 1), via the `vte` tokenizer.
//! - Combining characters, emoji ZWJ sequences, and full East Asian Width
//!   edge cases beyond single-codepoint width are not modeled yet.

pub use crate::terminal_types::{
    Cell, Color, MouseReporting, Position, Style, TerminalAction, TerminalModes,
};

use crate::size::Size;
use crate::snapshot::{ActiveScreen, TerminalLine, TerminalSnapshot};
use crate::terminal_buffer::Screen;
use crate::terminal_scrollback::ScrollbackLimits;
use crate::terminal_types::SavedCursor;

/// Primary terminal emulator state (no I/O).
pub struct TerminalState {
    pub(crate) parser: vte::Parser,
    pub(crate) size: Size,
    pub(crate) primary: Screen,
    pub(crate) alternate: Screen,
    pub(crate) on_alternate: bool,
    pub(crate) cursor: Position,
    pub(crate) saved: SavedCursor,
    pub(crate) wrap_pending: bool,
    pub(crate) pen: Style,
    pub(crate) modes: TerminalModes,
    pub(crate) title: String,
    pub(crate) scroll_top: u16,
    pub(crate) scroll_bottom: u16,
    pub(crate) actions: Vec<TerminalAction>,
    pub(crate) scrollback: Vec<TerminalLine>,
    pub(crate) scrollback_limits: ScrollbackLimits,
}

impl PartialEq for TerminalState {
    fn eq(&self, other: &Self) -> bool {
        self.size == other.size
            && self.primary == other.primary
            && self.alternate == other.alternate
            && self.on_alternate == other.on_alternate
            && self.cursor == other.cursor
            && self.saved == other.saved
            && self.wrap_pending == other.wrap_pending
            && self.pen == other.pen
            && self.modes == other.modes
            && self.title == other.title
            && self.scroll_top == other.scroll_top
            && self.scroll_bottom == other.scroll_bottom
            && self.actions == other.actions
            && self.scrollback == other.scrollback
            && self.scrollback_limits == other.scrollback_limits
    }
}

impl Eq for TerminalState {}

impl std::fmt::Debug for TerminalState {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("TerminalState")
            .field("size", &self.size)
            .field("primary", &self.primary)
            .field("alternate", &self.alternate)
            .field("on_alternate", &self.on_alternate)
            .field("cursor", &self.cursor)
            .field("saved", &self.saved)
            .field("wrap_pending", &self.wrap_pending)
            .field("pen", &self.pen)
            .field("modes", &self.modes)
            .field("title", &self.title)
            .field("scroll_top", &self.scroll_top)
            .field("scroll_bottom", &self.scroll_bottom)
            .field("actions", &self.actions)
            .field("scrollback", &self.scrollback)
            .field("scrollback_limits", &self.scrollback_limits)
            .finish_non_exhaustive()
    }
}

impl TerminalState {
    /// Creates a blank primary-screen state of `size`.
    ///
    /// A zero row or column count is clamped to `1` so the screen always has
    /// at least one addressable cell.
    pub fn new(size: Size) -> Self {
        let size = clamp_size(size);
        Self {
            parser: vte::Parser::new(),
            size,
            primary: Screen::blank(size),
            alternate: Screen::blank(size),
            on_alternate: false,
            cursor: Position { row: 0, col: 0 },
            saved: SavedCursor::default(),
            wrap_pending: false,
            pen: Style::default(),
            modes: TerminalModes::default(),
            title: String::new(),
            scroll_top: 0,
            scroll_bottom: size.rows.saturating_sub(1),
            actions: Vec::new(),
            scrollback: Vec::new(),
            scrollback_limits: ScrollbackLimits::DISABLED,
        }
    }

    /// Creates a blank primary-screen state of `size` with scrollback enabled
    /// under `limits`.
    ///
    /// `limits` is valid by construction: [`ScrollbackLimits`] cannot be
    /// created in a partially zero state.
    pub fn with_scrollback(size: Size, limits: ScrollbackLimits) -> Self {
        let mut state = Self::new(size);
        state.scrollback_limits = limits;
        state
    }

    /// Feeds output bytes into the emulator.
    ///
    /// Bytes may end mid-sequence or mid-UTF-8 code unit; state is kept until
    /// a later `feed` completes the sequence. Query replies are appended to
    /// the action queue; call [`Self::drain_actions`] to collect them.
    pub fn feed(&mut self, bytes: &[u8]) {
        feed_bytes(self, bytes);
    }

    /// Removes and returns actions produced since the last drain (or creation).
    pub fn drain_actions(&mut self) -> Vec<TerminalAction> {
        std::mem::take(&mut self.actions)
    }

    /// Returns the current screen size.
    pub fn size(&self) -> Size {
        self.size
    }

    /// Returns the current cursor position.
    pub fn cursor(&self) -> Position {
        self.cursor
    }

    /// Returns whether the cursor should be drawn.
    pub fn cursor_visible(&self) -> bool {
        self.modes.cursor_visible
    }

    /// Returns the cell at `at` on the active screen, if in range.
    pub fn cell(&self, at: Position) -> Option<Cell> {
        self.active().get(at)
    }

    /// Returns a copy of the current terminal modes.
    pub fn modes(&self) -> TerminalModes {
        self.modes
    }

    /// Returns whether the alternate screen buffer is active.
    pub fn on_alternate_screen(&self) -> bool {
        self.on_alternate
    }

    /// Returns the OSC window title last set by OSC 0 or OSC 2.
    pub fn title(&self) -> &str {
        &self.title
    }

    /// Returns the current drawing style (SGR pen).
    pub fn current_style(&self) -> Style {
        self.pen
    }

    /// Returns an owned copy of the visible state and primary scrollback.
    ///
    /// No I/O is performed. The payload copies the active screen's cells,
    /// cursor, modes, current style, title, active screen, and primary-derived
    /// scrollback. Time and allocation scale with the number of visible cells,
    /// retained scrollback cells, and title bytes. Session identity, child
    /// status, file descriptors, parser state, and undrained actions are never
    /// included; retaining several snapshots long-term is the caller's concern.
    pub fn snapshot(&self) -> TerminalSnapshot {
        let active = self.active();
        let active_screen = if self.on_alternate {
            ActiveScreen::Alternate
        } else {
            ActiveScreen::Primary
        };
        TerminalSnapshot::new(
            self.size,
            active.cells().to_vec(),
            self.cursor,
            self.modes,
            self.pen,
            self.title.clone(),
            active_screen,
            self.scrollback.clone(),
        )
    }

    /// Resizes both primary and alternate screens.
    ///
    /// Existing contents are copied into the overlapping region. Broken wide
    /// characters at the new right edge are cleared. The cursor and scroll
    /// region are clamped into the new bounds.
    pub fn resize(&mut self, size: Size) {
        let size = clamp_size(size);
        if size == self.size {
            return;
        }
        self.primary.resize(size);
        self.alternate.resize(size);
        self.size = size;
        self.scroll_top = 0;
        self.scroll_bottom = size.rows.saturating_sub(1);
        self.cursor.row = self.cursor.row.min(size.rows - 1);
        self.cursor.col = self.cursor.col.min(size.cols - 1);
        self.wrap_pending = false;
        self.repair_cursor_cell();
    }

    pub(crate) fn active(&self) -> &Screen {
        if self.on_alternate {
            &self.alternate
        } else {
            &self.primary
        }
    }

    pub(crate) fn active_mut(&mut self) -> &mut Screen {
        if self.on_alternate {
            &mut self.alternate
        } else {
            &mut self.primary
        }
    }

    /// Scrolls the active screen up by `count` rows.
    ///
    /// A full-screen scroll on the primary screen pushes the displaced rows
    /// into scrollback (top-to-bottom); partial regions, scroll-downs, line
    /// edits, and the alternate screen never do.
    pub(crate) fn scroll_up_screen(&mut self, count: u16) {
        let top = self.scroll_top;
        let bottom = self.scroll_bottom;
        let full_screen = top == 0 && bottom + 1 == self.size.rows;
        let style = self.pen;
        let displaced = self.active_mut().scroll_up(count, top, bottom, style);
        if full_screen && !self.on_alternate {
            for row in displaced {
                crate::terminal_scrollback::append_line(
                    &mut self.scrollback,
                    self.scrollback_limits,
                    TerminalLine::new(row),
                );
            }
        }
    }

    pub(crate) fn repair_cursor_cell(&mut self) {
        if let Some(cell) = self.cell(self.cursor)
            && cell.width == 0
            && self.cursor.col > 0
        {
            self.cursor.col -= 1;
        }
    }
}

fn feed_bytes(term: &mut TerminalState, bytes: &[u8]) {
    // Extract the parser so `Emulator` can mutably borrow the remaining fields.
    let mut parser = std::mem::replace(&mut term.parser, vte::Parser::new());
    {
        let mut emu = crate::terminal_emu::Emulator { term };
        parser.advance(&mut emu, bytes);
    }
    term.parser = parser;
}

fn clamp_size(size: Size) -> Size {
    Size {
        rows: size.rows.max(1),
        cols: size.cols.max(1),
    }
}
