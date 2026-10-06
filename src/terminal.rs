//! I/O-free terminal emulator state.
//!
//! [`TerminalState`] accepts PTY output through [`TerminalState::feed()`] and
//! updates cells, styles, cursor, and modes. Query replies are held until the
//! caller reads them with [`TerminalState::pending_reply_bytes()`]; this type
//! never writes to a file descriptor.
//!
//! # Supported sequences (modes milestone)
//!
//! - C0: BEL (reported as [`ChildRequest::RingBell`]), BS, HT, LF, VT, FF, CR
//! - ESC: IND (`D`), NEL (`E`), RI (`M`), DECSC/DECRC (`7`/`8`), RIS (`c`)
//! - CSI cursor: CUU/CUD/CUF/CUB, CNL/CPL, CHA/HPA, VPA, CUP/HVP
//! - CSI edit: ICH, DCH, IL, DL, ED, EL, ECH, SU, SD
//! - CSI scroll region: DECSTBM
//! - CSI modes: SM/RM including DEC private modes listed on [`TerminalModes`]
//! - CSI SGR (`m`): reset, bold, italic, underline, reverse, 16/256/24-bit color
//! - CSI queries: DSR, CPR, primary DA (`CSI c` / `CSI ? c`); DA2 and DA3 are
//!   recognized but not answered
//! - OSC 0/2: window title (stored); OSC 4: palette entry set/queried; OSC
//!   10/11/12: default foreground/background/cursor set/queried; OSC 52:
//!   clipboard request (reported through [`TerminalState::next_event()`]);
//!   other OSC offered to the caller as [`ChildRequest::OtherOsc`] without
//!   becoming text
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
    Cell, ChildRequest, ClipboardSelection, Color, Event, MouseReporting, Position, Rgb,
    ScrollbackLine, Style, TerminalModes,
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
/// - **C0**: BEL (reported as [`ChildRequest::RingBell`]), BS, HT, LF/VT/FF, CR
/// - **ESC**: IND, NEL, RI, DECSC/DECRC, RIS
/// - **CSI cursor**: CUU/CUD/CUF/CUB, CNL/CPL, CHA/HPA, VPA, CUP/HVP
/// - **CSI editing**: ICH/DCH/IL/DL/ED/EL/ECH/SU/SD, DECSTBM scroll regions
/// - **SGR**: bold, italic, underline, reverse, 16/256/24-bit color
/// - **DEC private modes**: application cursor/keypad, origin, autowrap,
///   cursor visibility, bracketed paste, mouse reporting (including SGR)
/// - **Alternate screen**: `?1049`, `?47`, `?1047`
/// - **OSC 0/2**: window title (stored); **OSC 4**: palette entries, and
///   **OSC 10/11/12**: the default foreground, background, and cursor colours,
///   all read back through [`palette_color()`](TerminalState::palette_color)
///   and the `default_*` accessors and answered from state on a `?` query;
///   **OSC 52**: clipboard request, reported through
///   [`next_event()`](TerminalState::next_event); any other identifier is
///   offered uninterpreted as [`ChildRequest::OtherOsc`]
/// - **Queries**: DSR, CPR, and primary DA, answered through
///   [`pending_reply_bytes()`](TerminalState::pending_reply_bytes). DA2
///   (`CSI > c`) and DA3 (`CSI = c`) are recognized but deliberately not
///   answered, so a caller probing with those forms gets no reply instead of a
///   DA1-shaped one
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
    /// Events that have happened since the caller last drained them.
    ///
    /// The merged state-change flags and the queue of unmerged requests, held
    /// together so one [`next_event()`](TerminalState::next_event) is the whole
    /// channel. Not part of `VisibleScalars`: an undrained event is not itself
    /// a visible change, and draining one is not either.
    pub(crate) events: Events,
    /// Per-entry palette overrides the child set with OSC 4, `None` until it
    /// sets one.
    ///
    /// Boxed because the array is 768 bytes: keeping it behind a pointer stops
    /// every `TerminalState` from carrying that much inline, and the entries
    /// are read through the palette accessor rather than in bulk.
    pub(crate) palette: Box<[Option<Rgb>; 256]>,
    /// The OSC 10/11/12 slots (default foreground, background, cursor), `None`
    /// until the child sets one.
    pub(crate) default_colors: DefaultColors,
    /// Set by `osc_dispatch` when it stores a new title, and cleared at the
    /// start of each `feed`.
    ///
    /// This is the per-feed change detector, distinct from the
    /// `title_updated` event flag: the title is the one visible field that is
    /// not `Copy`, so it cannot join the before/after scalar compare without
    /// allocating a `String` per feed, and reading-and-clearing it must not
    /// wipe a `TitleUpdated` event that is still waiting to be drained.
    pub(crate) title_changed: bool,
}

/// Ceiling on the bytes one terminal reply may add.
///
/// This type generates two kinds of reply. A CSI reply (DSR, CPR, DA1) is
/// produced by a single final byte and has a fixed, small shape. An OSC reply
/// (OSC 4 and 10-12, answering a `?` query) echoes a value whose length the
/// child chose, so its shape is bounded only by the query: an `rgb:` triple is
/// at most 12 digits plus separators and framing. The ceiling here bounds the
/// *pending buffer*, not one decode step, so it only has to be larger than any
/// single reply; a CSI reply is asserted against its own fixed bound where it
/// is built ([`crate::session::CSI_REPLY_BYTES`]), and the session reads
/// [`take_reply_overflow()`](TerminalState::take_reply_overflow) for the OSC
/// side.
const MAX_TERMINAL_REPLY_BYTES: usize = 4096;

/// The events the caller has not drained yet.
///
/// This is the mechanism behind
/// [`TerminalState::next_event()`](TerminalState::next_event); it is private,
/// and the only way a caller reaches an [`Event`] is the public accessor. The
/// state-change events are merged: each is a flag, so many changes of one kind
/// collapse to a single event. Requests are not merged: they are held in a
/// queue, oldest-first, so every ask survives and the order the child sent them
/// is preserved.
#[derive(Debug, Default)]
pub(crate) struct Events {
    terminal_reset: bool,
    screen_updated: bool,
    scrollback_line_added: bool,
    title_updated: bool,
    requests: VecDeque<ChildRequest>,
}

impl Events {
    /// Marks the screen as having changed since the caller last drained.
    pub(crate) fn mark_screen_updated(&mut self) {
        self.screen_updated = true;
    }

    /// Marks a line as having been appended to the retained history.
    pub(crate) fn mark_scrollback_line_added(&mut self) {
        self.scrollback_line_added = true;
    }

    /// Marks the window title as having changed.
    pub(crate) fn mark_title_updated(&mut self) {
        self.title_updated = true;
    }

    /// Marks the whole terminal as having been reset.
    pub(crate) fn mark_terminal_reset(&mut self) {
        self.terminal_reset = true;
    }

    /// Queues a request from the child.
    pub(crate) fn push_request(&mut self, request: ChildRequest) {
        self.requests.push_back(request);
    }

    /// Drops every queued request.
    ///
    /// A reset restores the terminal to its defaults and leaves no pending ask
    /// behind, so requests from before it must not outlive it.
    pub(crate) fn clear_requests(&mut self) {
        self.requests.clear();
    }
}

impl Iterator for Events {
    type Item = Event;

    /// Yields the next pending event, in the crate's fixed order: a reset
    /// first (it invalidates everything read before it), then the merged
    /// state updates, then the unmerged requests last (where coming last
    /// cannot lose them). Each flag is cleared as its event is yielded, so it
    /// is reported once; a request is popped as it is yielded.
    fn next(&mut self) -> Option<Event> {
        if std::mem::take(&mut self.terminal_reset) {
            return Some(Event::TerminalReset);
        }
        if std::mem::take(&mut self.screen_updated) {
            return Some(Event::ScreenUpdated);
        }
        if std::mem::take(&mut self.scrollback_line_added) {
            return Some(Event::ScrollbackLineAppended);
        }
        if std::mem::take(&mut self.title_updated) {
            return Some(Event::TitleUpdated);
        }
        self.requests.pop_front().map(Event::RequestReceived)
    }
}

/// The OSC 10/11/12 colour slots, each `None` until the child sets it.
///
/// A struct rather than three loose fields so the three slots move together:
/// RIS clears them as a unit and the accessors read them through one helper.
/// The slots are not a palette: `OSC 10` is defined as a slot, not an index, so
/// a host may set index 0 and the default foreground independently and the
/// crate must not conflate them.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(crate) struct DefaultColors {
    /// Default foreground (`OSC 10`).
    pub(crate) foreground: Option<Rgb>,
    /// Default background (`OSC 11`).
    pub(crate) background: Option<Rgb>,
    /// Cursor colour (`OSC 12`).
    pub(crate) cursor: Option<Rgb>,
}

/// The `Copy` fields of [`TerminalState`] that count as part of the visible
/// state and are cheap to compare across a single `feed`.
///
/// The scroll region (`scroll_top` / `scroll_bottom`) is deliberately absent:
/// setting it changes how later scrolls behave, but nothing is drawn until such
/// a scroll happens, so a feed that only sets the region is not a visible
/// change.
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
///
/// The buffer also carries the bound on a *single* reply. Most replies are
/// generated by one final byte and are short enough that the session can check
/// their length after the fact, but an OSC query replies with a value the
/// child chose the length of (a palette query echoes an `rgb:` triple), so the
/// check has to happen where the reply is built, against the ceiling the
/// session set. A reply that would cross it is not stored at all: half an
/// answer would leave the child parsing a value the terminal never sent, so
/// [`overflowed`](ReplyBuf::overflowed) records the event for the session to
/// turn into an error instead.
#[derive(Debug)]
pub(crate) struct ReplyBuf {
    bytes: Vec<u8>,
    offset: usize,
    /// Ceiling on the bytes a single reply may add.
    limit: usize,
    /// Set when a reply was refused for crossing [`limit`](ReplyBuf::limit).
    overflowed: bool,
}

impl ReplyBuf {
    fn new(limit: usize) -> Self {
        Self {
            bytes: Vec::new(),
            offset: 0,
            limit,
            overflowed: false,
        }
    }

    /// Bytes produced but not yet consumed.
    pub(crate) fn pending(&self) -> &[u8] {
        &self.bytes[self.offset..]
    }

    /// Appends a CSI reply.
    ///
    /// A CSI reply is emitted by one decoded byte and has a length that is
    /// fixed by the sequence's shape (a CPR is at most 14 bytes; DA1 is 5), so
    /// the caller knows it fits and there is nothing to check at run time. The
    /// assertion below records that: it must never be the case that a reply
    /// built by this path would cross the emulator's ceiling, because a length
    /// the crate did not bound is exactly the bug the OSC path's
    /// [`push_bounded`](ReplyBuf::push_bounded) exists to handle.
    pub(crate) fn push(&mut self, bytes: &[u8]) {
        debug_assert!(
            bytes.len() <= crate::session::CSI_REPLY_BYTES,
            "a fixed-shape reply grew past the CSI bound: {bytes:?}"
        );
        self.bytes.extend_from_slice(bytes);
    }

    /// Appends a reply whose length depends on what the child asked for.
    ///
    /// An OSC query echoes a value the child chose the length of, so the reply
    /// can be neither known at compile time nor truncated safely. It is stored
    /// only if it fits under the ceiling; otherwise nothing is stored and the
    /// overflow is recorded for the session to report. The answer is
    /// all-or-nothing, because a truncated reply is a different (wrong) value
    /// rather than a shorter one.
    pub(crate) fn push_bounded(&mut self, bytes: &[u8]) {
        if self.pending_len().saturating_add(bytes.len()) > self.limit {
            self.overflowed = true;
            return;
        }
        self.bytes.extend_from_slice(bytes);
    }

    /// Bytes still owed to the PTY master.
    fn pending_len(&self) -> usize {
        self.bytes.len() - self.offset
    }

    /// Takes the overflow flag, clearing it.
    pub(crate) fn take_overflow(&mut self) -> bool {
        std::mem::take(&mut self.overflowed)
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
            .field("palette", &self.palette)
            .field("default_colors", &self.default_colors)
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
            replies: ReplyBuf::new(MAX_TERMINAL_REPLY_BYTES),
            scrollback: VecDeque::new(),
            scrollback_cells: 0,
            events: Events::default(),
            palette: Box::new([None; 256]),
            default_colors: DefaultColors::default(),
            title_changed: false,
        }
    }

    /// Returns the next pending event and consumes it, or `None` if none is
    /// pending.
    ///
    /// This is the one place a caller learns what the child did that it may
    /// need to react to: a repaint, new history, a title change, a whole
    /// terminal reset, or a request the caller has to carry out. Drain it by
    /// looping until it returns `None`:
    ///
    /// ```
    /// # fn example(term: &mut termnix::TerminalState) {
    /// while let Some(event) = term.next_event() {
    ///     match event {
    ///         termnix::Event::ScreenUpdated => { /* repaint */ }
    ///         termnix::Event::TerminalReset => { /* drop derived state, repaint */ }
    ///         // History has no built-in bound; cap it (here 1000 lines).
    ///         termnix::Event::ScrollbackLineAppended => term.trim_scrollback(1000, 100_000),
    ///         termnix::Event::TitleUpdated => { /* read term.title() */ }
    ///         termnix::Event::RequestReceived(request) => { /* act on `request` */ }
    ///     }
    /// }
    /// # }
    /// ```
    ///
    /// The events arrive in a fixed order when more than one is pending: a
    /// [`TerminalReset`](Event::TerminalReset) first, then the merged state
    /// updates, then the requests last. The state updates are merged, so
    /// several changes of the same kind since the last drain arrive as one
    /// event and each is reported once; read the current state itself through
    /// [`rows()`](TerminalState::rows),
    /// [`scrollback_lines()`](TerminalState::scrollback_lines), and
    /// [`title()`](TerminalState::title). A request is not merged: every ask is
    /// yielded, in the order the child sent them.
    ///
    /// # Why this takes `&mut self`
    ///
    /// Every other accessor here borrows `&self`, because the values it returns
    /// are plain properties of the terminal. An event is not a property: it is
    /// a fact that is over once it has been read. Without a take, a caller
    /// could not say "I have handled this", and the same event would be
    /// re-delivered on every later feed and every repaint. This is deliberately
    /// not shaped like the reply buffer
    /// ([`pending_reply_bytes()`](TerminalState::pending_reply_bytes) plus
    /// [`advance_reply_bytes()`](TerminalState::advance_reply_bytes)): a reply
    /// is a byte stream the caller may write only partially, while an event is
    /// discrete and has no partial form.
    ///
    /// Nothing is lost while a caller does not look: requests wait in a queue,
    /// and a state-change flag stays set until it is yielded, so a change
    /// missed on one turn is still reported on the next.
    pub fn next_event(&mut self) -> Option<Event> {
        self.events.next()
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
    /// One `feed` raises [`ScreenUpdated`](Event::ScreenUpdated) at most once,
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
            self.events.mark_screen_updated();
        }
    }

    /// Takes the flag that a reply was refused for exceeding the reply bound,
    /// clearing it.
    ///
    /// The bound belongs to the caller (see [`Session`](crate::Session), which
    /// owns the PTY and decides how large a reply may be), so this type records
    /// the refusal rather than deciding what to do about it. Reading it is a
    /// take: the session raises an error for the feed that overflowed, and a
    /// flag left set would report the same refusal again on the next one.
    pub(crate) fn take_reply_overflow(&mut self) -> bool {
        self.replies.take_overflow()
    }

    /// Returns the bytes the emulator has produced in response to queries, if
    /// any.
    ///
    /// These are replies to queries the terminal was sent (DSR, CPR, primary
    /// DA). DA2 and DA3 are recognized but produce no reply. Whenever the slice
    /// is non-empty the caller should write it and then
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

    /// Returns the colour the terminal resolves palette `index` to.
    ///
    /// The child can redefine a palette entry with OSC 4; when it has, this
    /// returns that entry, and otherwise the built-in xterm default that
    /// [`Color::to_rgb`] also resolves an [`Indexed`](Color::Indexed) colour
    /// through. The result is total: every `u8` names a palette entry, and the
    /// built-in table gives all of them a value, so there is no `None` to
    /// unwrap. (`Color::to_rgb` returns an `Option` only because
    /// [`Color::Default`] is a real "no value" case, which has no analogue
    /// here.)
    ///
    /// The override is consulted first and the slot is not: indices 0-15 are a
    /// palette, and `OSC 10` is a separate slot even though a host may render
    /// [`Color::Default`] with it. Mixing them would make `palette_color(0)`
    /// change meaning when the child sets `OSC 10`, which is not what the
    /// sequences say.
    ///
    /// This is the accessor a host rendering a cell whose [`Style`] holds
    /// [`Color::Indexed`] should use; [`Color::to_rgb`] answers "what is the
    /// index in the default table", which is a different question once the
    /// child has recoloured anything.
    pub fn palette_color(&self, index: u8) -> Rgb {
        self.palette[index as usize].unwrap_or_else(|| crate::terminal_types::indexed_rgb(index))
    }

    /// Returns the default foreground colour the child set with OSC 10.
    ///
    /// `None` means the child never set one. There is no fallback: the default
    /// foreground is the *host's* until the child overrides it, and the crate
    /// does not know the host's colour, so inventing one would be worse than
    /// answering nothing.
    pub fn default_foreground_color(&self) -> Option<Rgb> {
        self.default_colors.foreground
    }

    /// Returns the default background colour the child set with OSC 11.
    ///
    /// `None` means the child never set one; see
    /// [`default_foreground_color()`](TerminalState::default_foreground_color)
    /// for why there is no fallback.
    pub fn default_background_color(&self) -> Option<Rgb> {
        self.default_colors.background
    }

    /// Returns the cursor colour the child set with OSC 12.
    ///
    /// `None` means the child never set one; see
    /// [`default_foreground_color()`](TerminalState::default_foreground_color)
    /// for why there is no fallback.
    pub fn default_cursor_color(&self) -> Option<Rgb> {
        self.default_colors.cursor
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
    /// This method only re-lays out the emulator: it touches no file
    /// descriptor, sets no kernel winsize, and sends no signal. It knows
    /// nothing about a PTY or a child process. A caller that owns a PTY and
    /// wants the child notified must set `TIOCSWINSZ` on it, which is what
    /// [`Session::resize()`](crate::Session::resize()) does — and setting the
    /// winsize is what makes the kernel deliver `SIGWINCH` to the child. This
    /// method alone does none of that.
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
        // A resize always changes the visible size, so the screen is marked
        // unconditionally. Clear the primary screen's dirty flag anyway so a
        // later `feed` does not re-detect this resize's cell writes.
        self.primary.take_dirty();
        self.events.mark_screen_updated();
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
                self.events.mark_scrollback_line_added();
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

#[cfg(test)]
mod tests {
    use super::ReplyBuf;

    #[test]
    fn bounded_push_refuses_the_whole_reply_and_flags_it() {
        // All or nothing: a reply that does not fit is not stored, because a
        // truncated one would be a different value rather than a shorter one.
        let mut buf = ReplyBuf::new(16);
        buf.push_bounded(b"0123456789");
        buf.push_bounded(b"012345");
        assert_eq!(buf.pending(), b"0123456789012345");
        buf.push_bounded(b"x");
        assert_eq!(buf.pending(), b"0123456789012345");
        assert!(buf.take_overflow());
        // Taking the flag clears it, so a refusal is reported once.
        assert!(!buf.take_overflow());
    }

    #[test]
    fn bounded_push_measures_against_the_unwritten_remainder() {
        // Advancing past a prefix frees room; the bound is about the bytes the
        // PTY master is still owed, not the allocation.
        let mut buf = ReplyBuf::new(8);
        buf.push_bounded(b"12345678");
        assert!(!buf.take_overflow());
        buf.advance(8);
        buf.push_bounded(b"abcdefgh");
        assert_eq!(buf.pending(), b"abcdefgh");
        assert!(!buf.take_overflow());
    }

    #[test]
    fn colour_queries_answer_within_the_emulator_ceiling() {
        // OSC 4 and 10-12 answer a `?` with a value whose length the child
        // chose the *shape* of (an `rgb:` triple plus framing), and it may
        // exceed the fixed 14-byte CSI bound. It must still fit the emulator's
        // own larger ceiling, or the session would report an overflow for a
        // reply the terminal itself generated. Each query is answered from
        // state the terminal holds, so this also pins that the answer does not
        // grow with repeated queries.
        use super::TerminalState;
        // Any grid works: the reply length is the colour's, not the grid's.
        // A small one keeps the test from allocating a grid it never reads.
        let mut term = TerminalState::new(crate::Size {
            rows: std::num::NonZeroU16::new(24).expect("non-zero rows"),
            cols: std::num::NonZeroU16::new(80).expect("non-zero cols"),
        });
        term.feed(b"\x1b]4;255;rgb:ff/ff/ff\x07");
        term.advance_reply_bytes(term.pending_reply_bytes().len());
        for query in [
            b"\x1b]4;255;?\x07".as_slice(),
            b"\x1b]10;rgb:ff/ff/ff\x07\x1b]10;?\x07",
        ] {
            term.feed(query);
            let reply = term.pending_reply_bytes().to_vec();
            assert!(
                reply.len() >= 14,
                "a colour reply should be at least CSI-sized: {reply:?}"
            );
            assert!(
                reply.len() <= super::MAX_TERMINAL_REPLY_BYTES,
                "a colour reply must fit the emulator ceiling: {reply:?}"
            );
            assert!(
                !term.take_reply_overflow(),
                "a colour reply must not be refused: {reply:?}"
            );
            term.advance_reply_bytes(reply.len());
        }
    }
}
