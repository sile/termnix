//! I/O-free terminal emulator state.
//!
//! [`TerminalState`] accepts PTY output through [`TerminalState::feed()`] and
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
    Cell, Color, MouseReporting, Position, ScrollbackLine, Style, TerminalAction, TerminalModes,
};

use std::collections::VecDeque;
use std::hash::{Hash, Hasher};

use crate::size::Size;
use crate::terminal_buffer::Screen;
use crate::terminal_types::SavedCursor;

/// Primary terminal emulator state (no I/O).
///
/// Feed PTY bytes with [`feed()`](TerminalState::feed), then read the screen
/// grid through [`rows()`](TerminalState::rows) and the retained history with
/// [`scrollback_lines()`](TerminalState::scrollback_lines). Query replies are
/// queued as [`TerminalAction`] values for the caller to write, so this type
/// never touches a file descriptor.
///
/// The supported sequences are:
///
/// - **C0**: BEL (ignored), BS, HT, LF/VT/FF, CR
/// - **ESC**: IND, NEL, RI, DECSC/DECRC, RIS
/// - **CSI cursor**: CUU/CUD/CUF/CUB, CNL/CPL, CHA/HPA, VPA, CUP/HVP
/// - **CSI editing**: ICH/DCH/IL/DL/ED/EL/ECH/SU/SD, DECSTBM scroll regions
/// - **SGR**: bold, italic, underline, reverse, 16/256/24-bit color
/// - **DEC private modes**: application cursor/keypad, origin, autowrap,
///   cursor visibility, bracketed paste, mouse reporting (including SGR)
/// - **Alternate screen**: `?1049`, `?47`, `?1047`
/// - **OSC 0/2**: window title (stored)
/// - **Queries**: DSR, CPR, and primary DA, answered with
///   [`TerminalAction::WritePty`](TerminalAction::WritePty)
///
/// Sixel, Kitty graphics, iTerm2 images, and DCS payloads are ignored without
/// becoming visible text.
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
    pub(crate) scrollback: VecDeque<ScrollbackLine>,
    pub(crate) scrollback_cells: usize,
    pub(crate) revision: u64,
    pub(crate) last_visible: u64,
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
            && self.scrollback_cells == other.scrollback_cells
        // `revision` and `last_visible` are derived bookkeeping, not part of
        // the terminal's observable value, so they are excluded from equality.
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
            .field("scrollback_cells", &self.scrollback_cells)
            .finish_non_exhaustive()
    }
}

impl TerminalState {
    /// Creates a blank primary-screen state of `size`.
    ///
    /// Rows and columns are non-zero by construction of [`Size`].
    pub fn new(size: Size) -> Self {
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
            scroll_bottom: size.rows.get().saturating_sub(1),
            actions: Vec::new(),
            scrollback: VecDeque::new(),
            scrollback_cells: 0,
            revision: 0,
            last_visible: 0,
        }
    }

    /// Returns a counter that increments whenever the visible state changes.
    ///
    /// The visible state is the active screen's cells and styles, the cursor,
    /// the current drawing style (SGR pen), display-related modes (including
    /// cursor visibility, autowrap, mouse reporting, and alternate-screen
    /// selection), the screen size, and the window title. It deliberately
    /// excludes parser-internal progress (a partial sequence), scrollback-only
    /// changes, and undrained actions.
    ///
    /// The counter only guarantees *whether* the visible state changed since a
    /// previous read, not how many cells or bytes did; a single `feed` may
    /// bump it once even when many cells changed, and it stays unchanged when
    /// input produced no visible effect (BEL, an ignored sequence, or a
    /// partial escape). It wraps on overflow, which is unreachable in practice.
    pub fn revision(&self) -> u64 {
        self.revision
    }

    /// Returns a fingerprint of the fields that make up the visible state.
    ///
    /// Used by [`TerminalState::feed()`] to detect whether a feed changed
    /// anything the caller can see; scrollback and parser state are excluded.
    fn visible_fingerprint(&self) -> u64 {
        let mut hasher = std::collections::hash_map::DefaultHasher::new();
        self.size.hash(&mut hasher);
        self.active().cells().hash(&mut hasher);
        self.on_alternate.hash(&mut hasher);
        self.cursor.hash(&mut hasher);
        self.pen.hash(&mut hasher);
        self.modes.hash(&mut hasher);
        self.title.hash(&mut hasher);
        hasher.finish()
    }

    /// Feeds output bytes into the emulator.
    ///
    /// Bytes may end mid-sequence or mid-UTF-8 code unit; state is kept until
    /// a later `feed` completes the sequence. Query replies are appended to
    /// the action queue; call [`TerminalState::drain_actions()`] to collect them.
    pub fn feed(&mut self, bytes: &[u8]) {
        feed_bytes(self, bytes);
        self.refresh_revision();
    }

    /// Bumps [`revision`](Self::revision()) when the visible state changed.
    fn refresh_revision(&mut self) {
        let fingerprint = self.visible_fingerprint();
        if fingerprint != self.last_visible {
            self.last_visible = fingerprint;
            self.revision = self.revision.wrapping_add(1);
        }
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

    /// Returns the cell at `at` on the active screen, if in range.
    pub fn cell(&self, at: Position) -> Option<Cell> {
        self.active().get(at)
    }

    /// Returns each visible row as a left-to-right cell slice, top to bottom.
    ///
    /// The number of rows equals [`Size::rows`](crate::size::Size::rows) and
    /// every slice has exactly [`Size::cols`](crate::size::Size::cols) cells for
    /// the current size. Unlike the cells saved into scrollback, the visible
    /// rows are re-laid out by [`TerminalState::resize()`], so slices obtained
    /// before a resize may differ in length from slices obtained after it. The
    /// slices borrow the live screen; copy them (for example with
    /// [`to_vec()`](slice::to_vec)) to keep the contents once the state moves on.
    pub fn rows(&self) -> impl Iterator<Item = &[Cell]> {
        let cols = self.size.cols.get() as usize;
        self.active().cells().chunks(cols)
    }

    /// Returns the visible row at `row` as a cell slice, if in range.
    ///
    /// Rows outside `0..`[`Size::rows`](crate::size::Size::rows) return `None`.
    pub fn row(&self, row: u16) -> Option<&[Cell]> {
        if row >= self.size.rows.get() {
            return None;
        }
        self.rows().nth(row as usize)
    }

    /// Returns a copy of the current terminal modes.
    pub fn modes(&self) -> TerminalModes {
        self.modes
    }

    /// Returns whether the alternate screen buffer is active.
    pub fn is_on_alternate_screen(&self) -> bool {
        self.on_alternate
    }

    /// Returns the OSC window title last set by OSC 0 or OSC 2.
    pub fn title(&self) -> &str {
        &self.title
    }

    /// Returns the current drawing style (SGR pen).
    pub fn style(&self) -> Style {
        self.pen
    }

    /// Returns the retained scrollback lines, oldest-first.
    ///
    /// The lines are borrowed, not copied, and each keeps the width it had
    /// when it was saved; unlike the visible rows from
    /// [`TerminalState::rows()`], they are never reflowed by a later resize.
    /// The deque is exposed directly so callers can index, iterate, or measure
    /// the history without cloning it.
    ///
    /// # Keeping a place in the history
    ///
    /// The history moves underneath the caller: a push appends at the back
    /// and a trim drops from the front, so an index into this deque does not
    /// keep naming the same line across either event. A caller that needs to
    /// hold onto one line (for example a copy-mode cursor) should store its
    /// **distance from the newest line** instead of an index from the oldest.
    /// Counting from the back makes both events cheap to follow: a push moves
    /// the same physical line one step further from the newest line, so the
    /// distance grows by one, and a trim removes lines from the far end, so
    /// the distance is unchanged. No snapshot of the history is needed, and
    /// the line is simply gone once the distance reaches the history length,
    /// which the caller can test against [`VecDeque::len`].
    pub fn scrollback_lines(&self) -> &VecDeque<ScrollbackLine> {
        &self.scrollback
    }

    /// Removes the oldest scrollback lines until both limits hold.
    ///
    /// Lines are removed whole, oldest first; the newest content is kept.
    /// If either limit is zero the entire history is cleared. The specified
    /// amounts are not a hard cap: history grows past them until this is
    /// called.
    pub fn trim_scrollback(&mut self, max_lines: usize, max_cells: usize) {
        crate::terminal_scrollback::trim_scrollback(
            &mut self.scrollback,
            &mut self.scrollback_cells,
            max_lines,
            max_cells,
        );
    }

    /// Returns the number of retained scrollback cells, including blank and
    /// wide-character continuation cells.
    pub fn scrollback_cells(&self) -> usize {
        self.scrollback_cells
    }

    /// Resizes both primary and alternate screens.
    ///
    /// Existing contents are copied into the overlapping region. Broken wide
    /// characters at the new right edge are cleared. The cursor and scroll
    /// region are clamped into the new bounds.
    ///
    /// The visible grid reflows to the new size; the scrollback does not. Each
    /// retained [`ScrollbackLine`] keeps the width it had when it was
    /// scrolled off, so after a resize [`TerminalState::rows()`] and
    /// [`TerminalState::scrollback_lines()`] can have different column counts.
    /// Code that stitches them together should track the width per line or
    /// clear the scrollback via [`TerminalState::trim_scrollback()`] when a
    /// change of size makes mixed widths a problem.
    ///
    /// Changing the visible size here is independent of the kernel's view of
    /// the window: the PTY's `TIOCSWINSZ` is the caller's to set, and the
    /// child sees a `SIGWINCH` only when the caller arranges it.
    pub fn resize(&mut self, size: Size) {
        if size == self.size {
            return;
        }
        self.primary.resize(size);
        self.alternate.resize(size);
        self.size = size;
        self.scroll_top = 0;
        self.scroll_bottom = size.rows.get().saturating_sub(1);
        self.cursor.row = self.cursor.row.min(size.rows.get() - 1);
        self.cursor.col = self.cursor.col.min(size.cols.get() - 1);
        self.wrap_pending = false;
        self.repair_cursor_cell();
        self.refresh_revision();
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
    /// edits, and the alternate screen never do. History grows without a
    /// built-in bound; the caller trims it with [`Self::trim_scrollback()`].
    pub(crate) fn scroll_up_screen(&mut self, count: u16) {
        let top = self.scroll_top;
        let bottom = self.scroll_bottom;
        let full_screen = top == 0 && bottom + 1 == self.size.rows.get();
        let style = self.pen;
        let displaced = self.active_mut().scroll_up(count, top, bottom, style);
        if full_screen && !self.on_alternate {
            for row in displaced {
                let line = ScrollbackLine::new(row);
                self.scrollback_cells = self.scrollback_cells.saturating_add(line.len());
                self.scrollback.push_back(line);
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
