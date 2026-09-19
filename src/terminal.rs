//! I/O-free terminal emulator state.
//!
//! [`TerminalState`] accepts PTY output through [`TerminalState::feed()`] and
//! updates cells, styles, cursor, and modes. Query replies are held until the
//! caller reads them with [`TerminalState::pending_reply_bytes()`]; this type
//! never writes to a file descriptor.
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
    Cell, Color, MouseReporting, Position, ScrollbackLine, Style, TerminalModes,
};

use std::collections::VecDeque;

use crate::size::Size;
use crate::terminal_buffer::Screen;
use crate::terminal_types::SavedCursor;

/// Primary terminal emulator state (no I/O).
///
/// Feed PTY bytes with [`feed()`](TerminalState::feed), then read the screen
/// grid through [`rows()`](TerminalState::rows) and the retained history with
/// [`scrollback_lines()`](TerminalState::scrollback_lines). Query replies are
/// held in a buffer the caller reads with
/// [`pending_reply_bytes()`](TerminalState::pending_reply_bytes) and releases
/// with [`advance_reply_bytes()`](TerminalState::advance_reply_bytes), so this
/// type never touches a file descriptor.
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
/// - **Queries**: DSR, CPR, and primary DA, answered through
///   [`pending_reply_bytes()`](TerminalState::pending_reply_bytes)
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
    pub(crate) replies: ReplyBuf,
    pub(crate) scrollback: VecDeque<ScrollbackLine>,
    pub(crate) scrollback_cells: usize,
    pub(crate) revision: u64,
    /// Set by `osc_dispatch` when it stores a new title. The title is the one
    /// visible field that is not `Copy`, so it cannot join the before/after
    /// scalar compare without allocating a `String` per feed.
    pub(crate) title_changed: bool,
}

/// The `Copy` fields of [`TerminalState`] that count as part of the visible
/// state and are cheap to compare across a single `feed`.
///
/// The scroll region (`scroll_top` / `scroll_bottom`) is deliberately absent:
/// setting it changes how later scrolls behave, but nothing is drawn until such
/// a scroll happens, so a feed that only sets the region is not a visible
/// change. This matches the fields the previous fingerprint hashed.
#[derive(Clone, Copy, PartialEq)]
struct VisibleScalars {
    size: Size,
    on_alternate: bool,
    cursor: Position,
    wrap_pending: bool,
    pen: Style,
    modes: TerminalModes,
}

/// Bytes produced by the emulator that the caller still owes the PTY master.
///
/// Replies are appended in one piece (a reply is never split while being
/// generated), so [`pending()`](ReplyBuf::pending) is never a partial escape
/// sequence. The `bytes + offset` shape mirrors the session's read and write
/// queues: advancing a prefix just moves `offset`, and draining the last byte
/// reclaims the allocation so a long run of small replies does not grow the
/// buffer without bound.
#[derive(Debug)]
pub(crate) struct ReplyBuf {
    bytes: Vec<u8>,
    offset: usize,
}

impl ReplyBuf {
    fn new() -> Self {
        Self {
            bytes: Vec::new(),
            offset: 0,
        }
    }

    /// Bytes produced but not yet consumed.
    pub(crate) fn pending(&self) -> &[u8] {
        &self.bytes[self.offset..]
    }

    /// Appends a reply.
    pub(crate) fn push(&mut self, bytes: &[u8]) {
        self.bytes.extend_from_slice(bytes);
    }

    /// Marks `n` bytes consumed, reclaiming the buffer when fully drained.
    pub(crate) fn advance(&mut self, n: usize) {
        let len = self.pending().len();
        assert!(
            n <= len,
            "advanced {n} reply bytes, but only {len} are pending"
        );
        self.offset += n;
        if self.offset == self.bytes.len() {
            self.bytes.clear();
            self.offset = 0;
        }
    }
}

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
            .field("replies", &self.replies)
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
            replies: ReplyBuf::new(),
            scrollback: VecDeque::new(),
            scrollback_cells: 0,
            revision: 0,
            title_changed: false,
        }
    }

    /// Returns a counter that increments whenever the visible state changes.
    ///
    /// The visible state is the active screen's cells and styles, the cursor,
    /// the current drawing style (SGR pen), display-related modes (including
    /// cursor visibility, autowrap, mouse reporting, and alternate-screen
    /// selection), the screen size, and the window title. It deliberately
    /// excludes parser-internal progress (a partial sequence), scrollback-only
    /// changes, and undrained replies.
    ///
    /// The counter only guarantees *whether* the visible state changed since a
    /// previous read, not how many cells or bytes did; a single `feed` may
    /// bump it once even when many cells changed, and it stays unchanged when
    /// input produced no visible effect (BEL, an ignored sequence, or a
    /// partial escape). It wraps on overflow, which is unreachable in practice.
    ///
    /// The guarantee is one-directional. An unchanged counter means the visible
    /// state is certainly unchanged, but an advanced counter only means that
    /// some write reached the screen, not that anything on it differs: writing
    /// a cell the value it already held still counts as a write. Treat an
    /// advance as "repaint to be safe" rather than as "something is different".
    pub fn revision(&self) -> u64 {
        self.revision
    }

    /// Returns the cheap, `Copy` fields that make up part of the visible state.
    ///
    /// [`TerminalState::feed()`] snapshots this before parsing and compares it
    /// after, so a change to any of these fields is detected without hashing
    /// the grid. Cell writes are tracked separately by [`Screen`], and the
    /// title has its own flag because snapshotting a `String` would allocate.
    fn visible_scalars(&self) -> VisibleScalars {
        VisibleScalars {
            size: self.size,
            on_alternate: self.on_alternate,
            cursor: self.cursor,
            wrap_pending: self.wrap_pending,
            pen: self.pen,
            modes: self.modes,
        }
    }

    /// Feeds output bytes into the emulator.
    ///
    /// Bytes may end mid-sequence or mid-UTF-8 code unit; state is kept until
    /// a later `feed` completes the sequence. Query replies are appended to the
    /// reply buffer; read them with
    /// [`TerminalState::pending_reply_bytes()`] and report what you wrote with
    /// [`TerminalState::advance_reply_bytes()`].
    ///
    /// One `feed` bumps [`revision()`](TerminalState::revision) at most once,
    /// no matter how many cells changed within it.
    pub fn feed(&mut self, bytes: &[u8]) {
        let before = self.visible_scalars();
        self.title_changed = false;
        feed_bytes(self, bytes);
        // `take_dirty` clears the flag as it reads it, so it is taken into a
        // local first: the combination below is short-circuiting, and an
        // effectful call placed inside it would be skipped once an earlier
        // term is true, silently stranding the flag.
        //
        // Only the primary screen is polled. A write cannot land on the
        // alternate screen without also flipping `on_alternate`, which the
        // snapshot below already carries, so polling it would only ever
        // over-detect. A write to the hidden primary is invisible by definition.
        let primary_dirty = self.primary.take_dirty();
        let changed = primary_dirty || self.title_changed || (self.visible_scalars() != before);
        if changed {
            self.revision = self.revision.wrapping_add(1);
        }
    }

    /// Returns the bytes the emulator has produced in response to queries, if
    /// any.
    ///
    /// These are replies to queries the terminal was sent (DSR, CPR, primary
    /// DA). Whenever the slice is non-empty the caller should write it and then
    /// report how much it wrote with
    /// [`advance_reply_bytes()`](TerminalState::advance_reply_bytes). A caller
    /// that writes the whole slice advances by its length; a caller whose write
    /// was short advances by the number of bytes it actually wrote and leaves
    /// the rest pending for a later call.
    ///
    /// Bytes stay in the buffer until advanced past, so a reply is never lost
    /// by forgetting to look: nothing is taken implicitly.
    pub fn pending_reply_bytes(&self) -> &[u8] {
        self.replies.pending()
    }

    /// Marks `n` bytes of [`pending_reply_bytes()`](TerminalState::pending_reply_bytes)
    /// as written to the PTY master.
    ///
    /// `n == 0` is a no-op and is always valid. Advancing past the whole slice
    /// drains the buffer and reclaims it.
    ///
    /// # Panics
    ///
    /// Panics if `n` is greater than the current
    /// [`pending_reply_bytes()`](TerminalState::pending_reply_bytes) length.
    /// The check runs in release builds too, because an out-of-range count means
    /// the caller miscounted what it wrote and clamping would silently mark a
    /// reply consumed that never reached the PTY, with no later point at which
    /// to notice.
    pub fn advance_reply_bytes(&mut self, n: usize) {
        self.replies.advance(n);
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
    /// Rows outside the range `0..`[`Size::rows`](crate::size::Size::rows)
    /// return `None`.
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
    /// The retained lines are oldest-first, so an index into this deque stays
    /// valid as long as nothing is trimmed: new output is pushed onto the back
    /// and moves no existing index. Trimming is what shifts them, because it
    /// removes lines from the front. A caller that holds an index across a
    /// trim must therefore subtract the number of lines it dropped, which the
    /// caller knows because it asked for the trim through
    /// [`TerminalState::trim_scrollback()`].
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
        // A resize always changes the visible size, so the revision moves
        // unconditionally. Clear the primary screen's dirty flag anyway so a
        // later `feed` does not re-detect this resize's cell writes.
        self.primary.take_dirty();
        self.revision = self.revision.wrapping_add(1);
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
