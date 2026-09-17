//! Interactive two-session example that drives `termnix` from a `tuinix`
//! host terminal.
//!
//! termnix owns neither poll loops, drawing, nor the host terminal: this
//! example shows how little an application needs on top of the public
//! [`termnix::Session`](termnix::Session) API to integrate with a TUI toolkit. The
//! host input fd, the resize-signal fd, and each live [`Session::fd`](termnix::Session::fd) are
//! watched by one example-local `libc::poll` loop; readiness is turned into
//! [`termnix::Session::pump_io`](termnix::Session::pump_io) calls with
//! [`termnix::Session::needs_pump`](termnix::Session::needs_pump), keys are converted
//! and enqueued with caller-side backpressure, and the selected session's
//! [`termnix::TerminalSnapshot`](termnix::TerminalSnapshot) is projected into a
//! full-screen [`Frame`](tuinix::Frame).
//!
//! `tuinix` keeps the pure types ([`Frame`](tuinix::Frame), [`Char`](tuinix::Char),
//! [`InputDecoder`](tuinix::InputDecoder)) separate from the I/O types
//! ([`TerminalDriver`](tuinix::TerminalDriver)), so this example owns both the
//! event loop and the frame-to-frame diffing: a [`Frame`](tuinix::Frame) is built
//! from the snapshot, rendered against the previously rendered frame, and the
//! bytes are written to the driver.
//!
//! Keystrokes: `Ctrl+T` switches the display and input target between the two
//! sessions (no-op while only one session is left), `Ctrl+Q` quits. Other keys
//! go to the selected session. The children are deterministic `/bin/sh -c`
//! scripts (no user shell rc, no extra applications): each prints an
//! identifying header, a wide-character line, a styled line, and then loops
//! echoing input lines; typing `quit` in a session exits that child.
//!
//! The example intentionally adds no window, pane, layout, split, focus,
//! popup, border, or label model. Terminal mode restoration is handled by
//! `tuinix`'s [`TerminalDriver`](tuinix::TerminalDriver) drop.
//!
//! Run it from a terminal: `cargo run --quiet --example tuinix`.

use std::{
    io::{self, ErrorKind, Read as _, Write as _},
    os::fd::RawFd,
    process::Command,
    time::{Duration, Instant},
};

use tuinix::{Frame, TerminalDriver};

/// Number of session slots; the example fixes it at two.
const SESSION_COUNT: usize = 2;

/// Bytes read from the host terminal in one `read` call.
const HOST_READ_BUFFER: usize = 4096;

/// How often the direct children are polled with `try_wait`.
const PROCESS_POLL_INTERVAL: Duration = Duration::from_millis(50);

/// Maximum `pump_io` calls per outer iteration across all sessions.
const PUMP_DRAIN_BUDGET: usize = 64;

/// Maximum host input reads (and decoded inputs) handled per drain.
const INPUT_DRAIN_BUDGET: usize = 64;

/// Caller-side write-queue limit; holds the key when above this.
const WRITE_SOFT_LIMIT: usize = 4096;

/// Which fd a poll entry watches.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PollTarget {
    /// Host terminal input fd.
    HostInput,
    /// Host terminal resize-signal fd.
    HostResize,
    /// PTY fd of session `usize`.
    Session(usize),
}

/// One fd registered with `poll`, paired with its owner.
#[derive(Debug, Clone, Copy)]
struct PollEntry {
    fd: RawFd,
    events: i16,
    target: PollTarget,
}

/// What `poll` reported for one registered fd.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PollOutcome {
    /// No relevant revent.
    Idle,
    /// Host input fd is readable.
    HostInput,
    /// Resize-signal fd is readable.
    HostResize,
    /// Session PTY fd is readable and/or writable.
    SessionReady(usize),
    /// Fatal condition on a host fd; the example must shut down.
    HostError(&'static str),
    /// Invalid session fd; the example must shut down.
    SessionError(usize),
}

/// Example-local command handled before child delivery.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum AppCommand {
    /// Switch display and input target.
    Switch,
    /// Leave the loop and shut down.
    Quit,
}

/// A key press held back by caller-side backpressure.
#[derive(Debug, Clone, Copy)]
struct PendingInput {
    session: usize,
    event: termnix::KeyEvent,
}

/// Inspectable projection of one session's visible screen.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Projection {
    size: termnix::Size,
    rows: Vec<Vec<termnix::Cell>>,
    cursor: termnix::Position,
    cursor_visible: bool,
}

impl Projection {
    /// Copies a snapshot into an example-local, side-effect-free form.
    ///
    /// The cursor is validated against the captured size so callers never
    /// project an out-of-range position.
    fn from_snapshot(snapshot: &termnix::TerminalSnapshot) -> Result<Self, String> {
        let size = snapshot.size();
        let cursor = snapshot.cursor();
        if cursor.row >= size.rows.get() || cursor.col >= size.cols.get() {
            return Err(format!(
                "snapshot cursor {cursor:?} is outside the {size:?} grid"
            ));
        }
        let rows = snapshot.rows().map(|row| row.to_vec()).collect();
        Ok(Self {
            size,
            rows,
            cursor,
            cursor_visible: snapshot.modes().cursor_visible,
        })
    }
}

/// Application state: two session slots plus the event-loop bookkeeping.
struct App {
    sessions: [Option<termnix::Session>; SESSION_COUNT],
    selected: usize,
    pending: Option<PendingInput>,
    next_process_poll: Instant,
    pump_cursor: usize,
    quit: bool,
    /// Visible-state revision last written to the host for the selected
    /// session, or `None` when the current selection has not been drawn yet.
    drawn_revision: Option<u64>,
    /// Frame most recently rendered to the host.
    ///
    /// [`Frame::render`](tuinix::Frame::render) emits only the cells that
    /// differ from this frame; it is also the reference for the host cursor.
    /// `None` after a resize or a selection switch, so the whole screen is
    /// repainted rather than diffed against a stale frame.
    prev_frame: Option<Frame>,
}

impl App {
    /// Spawns both fixed child scripts at `size`.
    fn spawn(size: termnix::Size) -> Result<Self, AppError> {
        let mut sessions: [Option<termnix::Session>; SESSION_COUNT] = [None, None];
        for (index, slot) in sessions.iter_mut().enumerate() {
            let mut command = Command::new("/bin/sh");
            command.arg("-c").arg(child_script(index));
            *slot = Some(termnix::Session::new(&mut command, size).map_err(AppError::io)?);
        }
        Ok(Self {
            sessions,
            selected: 0,
            pending: None,
            next_process_poll: Instant::now() + PROCESS_POLL_INTERVAL,
            pump_cursor: 0,
            quit: false,
            drawn_revision: None,
            prev_frame: None,
        })
    }

    /// Reaps children when the process-poll deadline passes.
    fn poll_processes(&mut self) -> Result<(), AppError> {
        let now = Instant::now();
        if now < self.next_process_poll {
            return Ok(());
        }
        for slot in &mut self.sessions {
            let _ = slot
                .as_mut()
                .map(termnix::Session::try_wait)
                .transpose()
                .map_err(AppError::io)?;
        }
        self.next_process_poll = Instant::now() + PROCESS_POLL_INTERVAL;
        Ok(())
    }

    /// Drops finished sessions and repoints the selection.
    fn reap_finished(&mut self) {
        for slot in 0..SESSION_COUNT {
            let reaped = self.sessions[slot]
                .as_ref()
                .is_some_and(|session| session.status() == termnix::SessionStatus::Reaped);
            if reaped {
                self.sessions[slot] = None;
                if let Some(pending) = self.pending
                    && pending.session == slot
                {
                    self.pending = None;
                }
            }
        }
        if self.sessions[self.selected].is_none() {
            let next = self.live_indices().first().copied();
            self.selected = next.unwrap_or(0);
        }
    }

    /// Whether every slot is gone.
    fn all_done(&self) -> bool {
        self.sessions.iter().all(|slot| slot.is_none())
    }

    /// Whether any remaining session has immediate work.
    fn any_needs_pump(&self) -> bool {
        self.sessions
            .iter()
            .any(|slot| slot.as_ref().is_some_and(termnix::Session::needs_pump))
    }

    /// Slots that still own a session.
    fn live_indices(&self) -> Vec<usize> {
        (0..SESSION_COUNT)
            .filter(|&slot| self.sessions[slot].is_some())
            .collect()
    }

    /// Moves the selection to the next live slot; no-op with fewer than two.
    fn switch_selected(&mut self) {
        let live = self.live_indices();
        if live.len() < 2 {
            return;
        }
        let index = live
            .iter()
            .position(|&slot| slot == self.selected)
            .unwrap_or(0);
        self.selected = live[(index + 1) % live.len()];
        self.drawn_revision = None;
        self.prev_frame = None;
    }

    /// Applies a host resize to every session whose PTY is still open.
    ///
    /// The cached frame is dropped because it was built at the old size;
    /// `render` would otherwise diff against a frame of a different geometry.
    fn apply_resize(&mut self, size: termnix::Size) -> Result<(), AppError> {
        self.prev_frame = None;
        self.drawn_revision = None;
        for slot in &mut self.sessions {
            let live = slot.as_ref().is_some_and(|s| s.fd().is_some());
            if live {
                slot.as_mut()
                    .expect("live slot")
                    .resize(size)
                    .map_err(AppError::io)?;
            }
        }
        Ok(())
    }

    /// Forwards one host input: commands first, then a key to the selected
    /// session. Mouse events, bracketed pastes, and bytes the decoder could not
    /// parse are dropped explicitly.
    fn handle_input(&mut self, input: tuinix::Input) -> Result<(), AppError> {
        let tuinix::Input::Key(key) = input else {
            // Mouse events, bracketed pastes, and `Unrecognized` byte runs are
            // out of scope for this example; do not forward them.
            return Ok(());
        };
        let Some(event) = key_event_from_host(key) else {
            // BackTab, standalone Escape, function keys and other unsupported
            // keys are dropped rather than remapped to a different key.
            return Ok(());
        };
        match command_from_key(event) {
            Some(AppCommand::Switch) => self.switch_selected(),
            Some(AppCommand::Quit) => self.quit = true,
            None => self.try_enqueue(event)?,
        }
        Ok(())
    }

    /// Enqueues `event` to the selected session or holds it for backpressure.
    fn try_enqueue(&mut self, event: termnix::KeyEvent) -> Result<(), AppError> {
        let input = termnix::Input::Key(event);
        let held = {
            let Some(session) = self.sessions[self.selected].as_mut() else {
                return Ok(());
            };
            let need = input.byte_len(session.terminal_state().modes());
            if session.counters().unwritten().saturating_add(need) > WRITE_SOFT_LIMIT {
                true
            } else {
                match session.enqueue_input(input) {
                    Ok(()) => session
                        .pump_io(termnix::PumpBudget::default())
                        .map_err(AppError::io)?,
                    Err(err) if err.kind() == ErrorKind::BrokenPipe => {
                        // Target session closed underneath us: drop the key.
                    }
                    Err(err) => return Err(AppError::io(err)),
                }
                false
            }
        };
        if held {
            self.pending = Some(PendingInput {
                session: self.selected,
                event,
            });
        }
        Ok(())
    }

    /// Re-drives a held key once the write queue has room.
    ///
    /// A pending key is delivered only when the target session is still open
    /// and the queue is below the soft limit; otherwise it stays held, or is
    /// dropped as undeliverable when the target session ended.
    fn deliver_pending(&mut self) -> Result<(), AppError> {
        let Some(pending) = self.pending else {
            return Ok(());
        };
        let Some(session) = self.sessions[pending.session].as_mut() else {
            self.pending = None;
            return Ok(());
        };
        session
            .pump_io(termnix::PumpBudget::default())
            .map_err(AppError::io)?;
        let input = termnix::Input::Key(pending.event);
        let need = input.byte_len(session.terminal_state().modes());
        if session.counters().unwritten().saturating_add(need) > WRITE_SOFT_LIMIT {
            return Ok(());
        }
        match session.enqueue_input(input) {
            Ok(()) => session
                .pump_io(termnix::PumpBudget::default())
                .map_err(AppError::io)?,
            Err(err) if err.kind() == ErrorKind::BrokenPipe => {}
            Err(err) => return Err(AppError::io(err)),
        }
        self.pending = None;
        Ok(())
    }

    /// Pumps runnable sessions round-robin, bounded by a per-iteration budget.
    ///
    /// Sessions that were reported ready by `poll` or that report
    /// [`Session::needs_pump`](termnix::Session::needs_pump) are pumped in rotating order so one busy
    /// session cannot starve the other.
    fn drain_runnable(&mut self, ready: &[usize]) -> Result<(), AppError> {
        let mut ready = ready;
        let mut budget = PUMP_DRAIN_BUDGET;
        loop {
            if budget == 0 {
                break;
            }
            let mut progressed = false;
            for offset in 0..SESSION_COUNT {
                let slot = (self.pump_cursor + offset) % SESSION_COUNT;
                let Some(session) = self.sessions[slot].as_mut() else {
                    continue;
                };
                if !session.needs_pump() && !ready.contains(&slot) {
                    continue;
                }
                session
                    .pump_io(termnix::PumpBudget::default())
                    .map_err(AppError::io)?;
                budget -= 1;
                progressed = true;
            }
            ready = &[];
            self.pump_cursor = (self.pump_cursor + 1) % SESSION_COUNT;
            if !progressed {
                break;
            }
            if !self.any_needs_pump() {
                break;
            }
        }
        Ok(())
    }

    /// Explicitly shuts down every remaining session exactly once.
    ///
    /// The result of the first failing shutdown is reported; a session whose
    /// child was already reaped returns its cached status.
    fn shutdown_all(&mut self) -> Result<(), AppError> {
        let mut first_error: Option<io::Error> = None;
        for slot in &mut self.sessions {
            if let Some(session) = slot.take()
                && let Err(err) = session.shutdown()
                && first_error.is_none()
            {
                first_error = Some(err);
            }
        }
        match first_error {
            None => Ok(()),
            Some(err) => Err(AppError::msg(format!("shutdown failed: {err}"))),
        }
    }
}

/// Builds the `pollfd` list for one iteration.
///
/// The host input fd is omitted while a key is held for backpressure; the
/// resize-signal fd is always present; each open session contributes its fd
/// with the interests reported by [`Session::interests`](termnix::Session::interests).
fn build_poll_entries(
    host_input_fd: RawFd,
    host_signal_fd: RawFd,
    sessions: &[Option<termnix::Session>],
    include_host_input: bool,
) -> Vec<PollEntry> {
    let mut entries = Vec::new();
    if include_host_input {
        entries.push(PollEntry {
            fd: host_input_fd,
            events: libc::POLLIN,
            target: PollTarget::HostInput,
        });
    }
    entries.push(PollEntry {
        fd: host_signal_fd,
        events: libc::POLLIN,
        target: PollTarget::HostResize,
    });
    for (slot, session) in sessions.iter().enumerate() {
        let Some(session) = session else { continue };
        let Some(fd) = session.fd() else { continue };
        let interests = session.interests();
        let mut events = 0i16;
        if interests.readable {
            events |= libc::POLLIN;
        }
        if interests.writable {
            events |= libc::POLLOUT;
        }
        if events != 0 {
            entries.push(PollEntry {
                fd,
                events,
                target: PollTarget::Session(slot),
            });
        }
    }
    entries
}

/// Classifies one poll result.
fn classify_revents(entry: PollEntry, revents: i16) -> PollOutcome {
    let fatal_bits = libc::POLLERR | libc::POLLNVAL;
    match entry.target {
        PollTarget::HostInput => {
            if revents & fatal_bits != 0 {
                PollOutcome::HostError("host input fd reported POLLERR/POLLNVAL")
            } else if revents & libc::POLLHUP != 0 {
                PollOutcome::HostError("host input closed (POLLHUP)")
            } else if revents & libc::POLLIN != 0 {
                PollOutcome::HostInput
            } else {
                PollOutcome::Idle
            }
        }
        PollTarget::HostResize => {
            if revents & fatal_bits != 0 {
                PollOutcome::HostError("resize-signal fd reported POLLERR/POLLNVAL")
            } else if revents & libc::POLLHUP != 0 {
                PollOutcome::HostError("resize-signal closed (POLLHUP)")
            } else if revents & libc::POLLIN != 0 {
                PollOutcome::HostResize
            } else {
                PollOutcome::Idle
            }
        }
        PollTarget::Session(slot) => {
            if revents & libc::POLLNVAL != 0 {
                PollOutcome::SessionError(slot)
            } else if revents & (libc::POLLIN | libc::POLLOUT | libc::POLLHUP | libc::POLLERR) != 0
            {
                // POLLHUP/POLLERR on a PTY master surfaces as EOF or EIO
                // inside pump_io; forward the session so it drains.
                PollOutcome::SessionReady(slot)
            } else {
                PollOutcome::Idle
            }
        }
    }
}

/// Runs `poll` over `entries`, returning one outcome per entry.
///
/// `EINTR` is retried by the caller: it returns no outcomes and the loop
/// rechecks its state. The timeout is in milliseconds and never negative.
fn poll_once(entries: &[PollEntry], timeout_ms: i32) -> Result<Vec<PollOutcome>, AppError> {
    if entries.is_empty() {
        let rc = unsafe { libc::poll(std::ptr::null_mut(), 0, timeout_ms) };
        map_poll_rc(rc)?;
        return Ok(Vec::new());
    }
    let mut pollfds: Vec<libc::pollfd> = entries
        .iter()
        .map(|entry| libc::pollfd {
            fd: entry.fd,
            events: entry.events,
            revents: 0,
        })
        .collect();
    let rc = unsafe {
        libc::poll(
            pollfds.as_mut_ptr(),
            pollfds.len() as libc::nfds_t,
            timeout_ms,
        )
    };
    map_poll_rc(rc)?;
    Ok(entries
        .iter()
        .zip(&pollfds)
        .map(|(entry, pollfd)| classify_revents(*entry, pollfd.revents))
        .collect())
}

/// Turns a non-negative `poll` return into `Ok(())`; `EINTR` is tolerated.
fn map_poll_rc(rc: i32) -> Result<(), AppError> {
    if rc >= 0 {
        return Ok(());
    }
    let err = io::Error::last_os_error();
    if err.kind() == ErrorKind::Interrupted {
        Ok(())
    } else {
        Err(AppError::io(err))
    }
}

/// Milliseconds until `deadline`, floored, so `poll` never waits past it.
fn poll_timeout_ms(deadline: Instant) -> i32 {
    let now = Instant::now();
    if now >= deadline {
        return 0;
    }
    let ms = deadline.duration_since(now).as_millis();
    i32::try_from(ms).unwrap_or(i32::MAX).max(0)
}

/// Reads raw host bytes into `decoder`, stopping at `WouldBlock`, end of
/// input, or after `budget` read syscalls.
///
/// A short `Ok(0)` from a terminal that is still open is not treated as end of
/// input: the read is retried until `budget` is spent. The decoder holds
/// partial sequences across calls, so a sequence split over two `poll`
/// wakeups is still parsed exactly once.
fn drain_host_inputs(
    driver: &mut TerminalDriver,
    decoder: &mut tuinix::InputDecoder,
    budget: usize,
) -> Result<(), AppError> {
    let mut buf = [0u8; HOST_READ_BUFFER];
    for _ in 0..budget {
        match driver.read(&mut buf) {
            Ok(0) => break,
            Ok(n) => decoder.feed(&buf[..n]),
            Err(err) if err.kind() == ErrorKind::WouldBlock => break,
            Err(err) if err.kind() == ErrorKind::Interrupted => continue,
            Err(err) => return Err(AppError::io(err)),
        }
    }
    Ok(())
}

/// Pulls every complete [`Input`](tuinix::Input) the decoder can produce.
///
/// A lone `ESC` is left uncommitted: the example never forwards Escape to a
/// child, so waiting for more bytes can only ever turn it into an Alt+key
/// sequence, which is the more faithful reading of what the user typed.
///
/// `next()` returns `None` only while an incomplete sequence is held, so a
/// stopped loop means no further input is ready; bytes the decoder cannot
/// parse come back as [`Input::Unrecognized`](tuinix::Input::Unrecognized)
/// and are handed to `handle_input`, which drops them.
fn collect_inputs(decoder: &mut tuinix::InputDecoder, budget: usize) -> Vec<tuinix::Input> {
    let mut inputs = Vec::new();
    for _ in 0..budget {
        match decoder.next() {
            Some(input) => inputs.push(input),
            None => break,
        }
    }
    inputs
}

/// Converts a supported host key into a termnix key event.
///
/// tuinix reports `ctrl` and `alt` on keys and has no shift bit: shift is
/// already applied to `Char` and named keys are unchanged. See
/// [`key_event_from_host`] for the supported set.
fn key_event_from_host(key: tuinix::KeyInput) -> Option<termnix::KeyEvent> {
    let modifiers = termnix::Modifiers {
        ctrl: key.ctrl,
        alt: key.alt,
        shift: false,
    };
    let code = match key.code {
        tuinix::KeyCode::Char(ch) => termnix::KeyCode::Char(ch),
        tuinix::KeyCode::Enter => termnix::KeyCode::Enter,
        tuinix::KeyCode::Backspace => termnix::KeyCode::Backspace,
        tuinix::KeyCode::Tab => termnix::KeyCode::Tab,
        tuinix::KeyCode::Delete => termnix::KeyCode::Delete,
        tuinix::KeyCode::Insert => termnix::KeyCode::Insert,
        tuinix::KeyCode::Up => termnix::KeyCode::Up,
        tuinix::KeyCode::Down => termnix::KeyCode::Down,
        tuinix::KeyCode::Left => termnix::KeyCode::Left,
        tuinix::KeyCode::Right => termnix::KeyCode::Right,
        tuinix::KeyCode::Home => termnix::KeyCode::Home,
        tuinix::KeyCode::End => termnix::KeyCode::End,
        tuinix::KeyCode::PageUp => termnix::KeyCode::PageUp,
        tuinix::KeyCode::PageDown => termnix::KeyCode::PageDown,
        // BackTab, standalone Escape, function keys and mouse events stay
        // unsupported so the example never silently forwards a different key.
        tuinix::KeyCode::Escape | tuinix::KeyCode::BackTab | tuinix::KeyCode::F(_) => {
            return None;
        }
    };
    Some(termnix::KeyEvent { code, modifiers })
}

/// Maps a key event to a command; `None` forwards it to the session.
fn command_from_key(event: termnix::KeyEvent) -> Option<AppCommand> {
    if event.modifiers.ctrl && !event.modifiers.alt && !event.modifiers.shift {
        match event.code {
            termnix::KeyCode::Char('t' | 'T') => return Some(AppCommand::Switch),
            termnix::KeyCode::Char('q' | 'Q') => return Some(AppCommand::Quit),
            _ => {}
        }
    }
    None
}

/// Converts a tuinix terminal size into a termnix grid size.
///
/// Zero dimensions and values beyond `u16` are rejected instead of clamped or
/// silently cast.
fn size_from_host(size: tuinix::Size) -> Result<termnix::Size, String> {
    let rows = u16::try_from(size.rows)
        .map_err(|_| format!("unsupported terminal rows: {}", size.rows))?;
    let cols = u16::try_from(size.cols)
        .map_err(|_| format!("unsupported terminal cols: {}", size.cols))?;
    termnix::Size::new(rows, cols).ok_or_else(|| format!("unsupported terminal size: {size:?}"))
}

/// Converts a termnix grid position into a host position.
///
/// The frame and the grid share an origin and are built at the same size, so
/// the conversion is total; the `usize` widening cannot fail.
fn position_to_host(position: termnix::Position) -> tuinix::Position {
    tuinix::Position {
        row: position.row as usize,
        col: position.col as usize,
    }
}

/// Maps a termnix color to tuinix's RGB-only representation.
///
/// `Default` maps to the terminal's default (no explicit color); everything
/// else resolves through [`termnix::Color::to_rgb`] (the xterm 256-color
/// palette for `Indexed`).
fn to_host_color(color: termnix::Color) -> Option<tuinix::Color> {
    color.to_rgb().map(|(r, g, b)| tuinix::Color::new(r, g, b))
}

/// Maps a termnix style to a tuinix style, dropping unsupported attributes.
///
/// `blink`, `dim`, and `strikethrough` have no termnix counterpart and are
/// left disabled rather than guessed at.
fn to_host_style(style: termnix::Style) -> tuinix::Style {
    let mut out = tuinix::Style::new();
    if style.bold {
        out = out.bold();
    }
    if style.italic {
        out = out.italic();
    }
    if style.underline {
        out = out.underline();
    }
    if style.reverse {
        out = out.reverse();
    }
    if let Some(color) = to_host_color(style.foreground) {
        out = out.fg_color(color);
    }
    if let Some(color) = to_host_color(style.background) {
        out = out.bg_color(color);
    }
    out
}

/// Builds a full-screen frame from one row-major grid.
///
/// Each grid cell carries its own display width, so the width is passed
/// straight through to [`Char::new`](tuinix::Char::new) and no width estimator
/// is needed. Width-0 continuation cells are skipped: they cover columns that
/// the preceding wide character already owns.
///
/// A cell that [`push_char`](tuinix::Frame::push_char) clips means the grid and
/// the frame disagree about their geometry; that is a bug in this example, not
/// a condition to paper over, so it is reported as an error.
fn write_grid(frame: &mut Frame, grid: &Projection) -> Result<(), String> {
    for row in &grid.rows {
        for cell in row {
            if cell.width == 0 {
                continue;
            }
            let style = to_host_style(cell.style);
            let ch =
                tuinix::Char::new(cell.ch, usize::from(cell.width), style).ok_or_else(|| {
                    format!(
                        "cell {:?} is not framable (control char or zero width)",
                        cell.ch
                    )
                })?;
            let pos = frame.next_position();
            if !frame.push_char(ch) {
                return Err(format!(
                    "cell {:?} was clipped at {}x{} position ({}, {})",
                    cell.ch,
                    frame.size().rows,
                    frame.size().cols,
                    pos.row,
                    pos.col
                ));
            }
        }
        frame.push_newline();
    }
    Ok(())
}

/// Draws the selected session to the host terminal.
///
/// Rendering is skipped when the session's visible-state revision matches the
/// last drawn one, so idle polls no longer re-emit the frame or the host
/// cursor show/hide sequences. The revision is captured from the snapshot so
/// the recorded value always matches what was actually rendered.
fn draw_selected(driver: &mut TerminalDriver, app: &mut App) -> Result<(), AppError> {
    let Some(session) = app.sessions[app.selected].as_ref() else {
        app.drawn_revision = None;
        app.prev_frame = None;
        return Ok(());
    };
    let snapshot = session.terminal_state().snapshot();
    let revision = snapshot.revision();
    if app.drawn_revision == Some(revision) {
        return Ok(());
    }
    let projection = Projection::from_snapshot(&snapshot).map_err(AppError::msg)?;
    let frame_size = tuinix::Size {
        rows: usize::from(projection.size.rows.get()),
        cols: usize::from(projection.size.cols.get()),
    };
    let mut frame = Frame::new(frame_size);
    write_grid(&mut frame, &projection).map_err(AppError::msg)?;
    // `None` hides the host cursor; `Some` shows it at the session cursor.
    let cursor = projection
        .cursor_visible
        .then(|| position_to_host(projection.cursor));
    let bytes = frame.render(app.prev_frame.as_ref(), cursor);
    driver
        .write_all(&bytes)
        .and_then(|()| driver.flush())
        .map_err(AppError::io)?;
    app.prev_frame = Some(frame);
    app.drawn_revision = Some(revision);
    Ok(())
}

/// Runs the main loop until quit, error, or both sessions are gone.
fn run_loop(
    driver: &mut TerminalDriver,
    decoder: &mut tuinix::InputDecoder,
    app: &mut App,
    host_input_fd: RawFd,
    host_signal_fd: RawFd,
) -> Result<(), AppError> {
    let mut ready: Vec<usize> = Vec::new();
    loop {
        app.poll_processes()?;
        app.reap_finished();
        if app.all_done() || app.quit {
            return Ok(());
        }

        app.deliver_pending()?;
        app.drain_runnable(&ready)?;
        ready.clear();

        // `size()` drains the resize-signal pipe itself; a burst is collapsed
        // to one query because only the latest size matters.
        let size = driver.size();
        let size = size_from_host(size).map_err(AppError::msg)?;
        app.apply_resize(size)?;

        draw_selected(driver, app)?;

        if app.pending.is_none() {
            drain_host_inputs(driver, decoder, INPUT_DRAIN_BUDGET)?;
            deliver_host_inputs(decoder, app)?;
            if app.quit {
                return Ok(());
            }
        }

        let include_host_input = app.pending.is_none();
        let entries = build_poll_entries(
            host_input_fd,
            host_signal_fd,
            &app.sessions,
            include_host_input,
        );
        let timeout = if app.any_needs_pump() {
            0
        } else {
            poll_timeout_ms(app.next_process_poll)
        };
        for outcome in poll_once(&entries, timeout)? {
            match outcome {
                PollOutcome::HostInput => {
                    drain_host_inputs(driver, decoder, INPUT_DRAIN_BUDGET)?;
                    deliver_host_inputs(decoder, app)?;
                    if app.quit {
                        return Ok(());
                    }
                }
                PollOutcome::HostResize => {
                    // The signal fd was reported readable, so `size()` re-queries
                    // the terminal instead of returning the cached value.
                    let size = driver.size();
                    let size = size_from_host(size).map_err(AppError::msg)?;
                    app.apply_resize(size)?;
                }
                PollOutcome::SessionReady(slot) => ready.push(slot),
                PollOutcome::HostError(message) => return Err(AppError::msg(message)),
                PollOutcome::SessionError(slot) => {
                    return Err(AppError::msg(format!(
                        "session {slot} fd reported POLLNVAL"
                    )));
                }
                PollOutcome::Idle => {}
            }
        }
    }
}

/// Hands every complete decoded input to the application.
fn deliver_host_inputs(decoder: &mut tuinix::InputDecoder, app: &mut App) -> Result<(), AppError> {
    for input in collect_inputs(decoder, INPUT_DRAIN_BUDGET) {
        app.handle_input(input)?;
        if app.quit {
            return Ok(());
        }
    }
    Ok(())
}

/// Drives the example: setup, loop, and a single explicit shutdown pass.
fn run() -> Result<(), AppError> {
    let mut driver = TerminalDriver::new().map_err(AppError::io)?;
    let host_input_fd = driver.input_fd();
    let host_signal_fd = driver.resize_signal_fd();
    let mut decoder = tuinix::InputDecoder::new();
    let size = size_from_host(driver.size()).map_err(AppError::msg)?;
    let mut app = App::spawn(size)?;

    let primary = run_loop(
        &mut driver,
        &mut decoder,
        &mut app,
        host_input_fd,
        host_signal_fd,
    );
    // Shutdown completes even when the loop failed, and holds both results.
    let shutdown = app.shutdown_all();
    match (primary, shutdown) {
        (Ok(()), Ok(())) => Ok(()),
        (Ok(()), Err(err)) => Err(err),
        (Err(err), Ok(())) => Err(err),
        (Err(mut err), Err(shutdown_err)) => {
            err.set_shutdown(shutdown_err);
            Err(err)
        }
    }
}

/// Deterministic child scripts: a header, wide/styled lines, then an echo loop.
fn child_script(index: usize) -> &'static str {
    match index {
        0 => concat!(
            "stty -echo || exit 1; ",
            "printf 'SESSION 0 READY\\n'; ",
            "printf 'SESSION 0 WIDE: Japanese 日本語\\n'; ",
            "printf '\\033[1;32mSESSION 0 GREEN\\033[0m\\n'; ",
            "printf 'SESSION 0 line 4\\n'; ",
            "printf 'SESSION 0 line 5\\n'; ",
            "while IFS= read -r line; do ",
            "case \"$line\" in ",
            "quit) printf 'SESSION 0 BYE\\n'; exit 0;; ",
            "*) printf 'SESSION 0 echo: %s\\n' \"$line\";; ",
            "esac; ",
            "done"
        ),
        1 => concat!(
            "stty -echo || exit 1; ",
            "printf 'SESSION 1 READY\\n'; ",
            "printf 'SESSION 1 WIDE: Kyoto 京都\\n'; ",
            "printf '\\033[1;36mSESSION 1 CYAN\\033[0m\\n'; ",
            "printf 'SESSION 1 line 4\\n'; ",
            "printf 'SESSION 1 line 5\\n'; ",
            "while IFS= read -r line; do ",
            "case \"$line\" in ",
            "quit) printf 'SESSION 1 BYE\\n'; exit 0;; ",
            "*) printf 'SESSION 1 echo: %s\\n' \"$line\";; ",
            "esac; ",
            "done"
        ),
        _ => unreachable!("fixed session count"),
    }
}

#[derive(Debug)]
struct AppError {
    message: String,
    shutdown: Option<String>,
}

impl AppError {
    fn msg(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
            shutdown: None,
        }
    }

    fn io(err: io::Error) -> Self {
        Self::msg(format!("I/O error: {err}"))
    }

    fn set_shutdown(&mut self, err: AppError) {
        self.shutdown = Some(err.message);
    }
}

impl std::fmt::Display for AppError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.message)?;
        if let Some(shutdown) = &self.shutdown {
            write!(f, "; shutdown error: {shutdown}")?;
        }
        Ok(())
    }
}

impl std::error::Error for AppError {}

fn main() {
    match run() {
        Ok(()) => {}
        Err(err) => {
            eprintln!("{err}");
            std::process::exit(1);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key(ctrl: bool, alt: bool, code: tuinix::KeyCode) -> tuinix::KeyInput {
        tuinix::KeyInput { ctrl, alt, code }
    }

    /// An empty [`App`] for tests that only exercise pure helpers.
    fn empty_app() -> App {
        App {
            sessions: [None, None],
            selected: 0,
            pending: None,
            next_process_poll: Instant::now(),
            pump_cursor: 0,
            quit: false,
            drawn_revision: None,
            prev_frame: None,
        }
    }

    /// The xterm-256 palette entry for a `termnix` indexed color.
    fn host_palette(index: u8) -> Option<tuinix::Color> {
        to_host_color(termnix::Color::Indexed(index))
    }

    #[test]
    fn supported_keys_are_converted_with_modifiers() {
        let supported = [
            (
                tuinix::KeyCode::Char('a'),
                Some(termnix::KeyCode::Char('a')),
            ),
            (tuinix::KeyCode::Enter, Some(termnix::KeyCode::Enter)),
            (
                tuinix::KeyCode::Backspace,
                Some(termnix::KeyCode::Backspace),
            ),
            (tuinix::KeyCode::Tab, Some(termnix::KeyCode::Tab)),
            (tuinix::KeyCode::Delete, Some(termnix::KeyCode::Delete)),
            (tuinix::KeyCode::Insert, Some(termnix::KeyCode::Insert)),
            (tuinix::KeyCode::Up, Some(termnix::KeyCode::Up)),
            (tuinix::KeyCode::Down, Some(termnix::KeyCode::Down)),
            (tuinix::KeyCode::Left, Some(termnix::KeyCode::Left)),
            (tuinix::KeyCode::Right, Some(termnix::KeyCode::Right)),
            (tuinix::KeyCode::Home, Some(termnix::KeyCode::Home)),
            (tuinix::KeyCode::End, Some(termnix::KeyCode::End)),
            (tuinix::KeyCode::PageUp, Some(termnix::KeyCode::PageUp)),
            (tuinix::KeyCode::PageDown, Some(termnix::KeyCode::PageDown)),
        ];
        for (host, expected) in supported {
            let event = key_event_from_host(key(true, true, host)).expect("supported key");
            assert_eq!(event.code, expected.expect("supported key"));
            assert!(event.modifiers.ctrl);
            assert!(event.modifiers.alt);
            assert!(!event.modifiers.shift, "shift is never set for keys");
        }
    }

    #[test]
    fn unsupported_keys_are_not_mapped() {
        let event = key_event_from_host(key(false, false, tuinix::KeyCode::Escape));
        assert!(event.is_none());
        let event = key_event_from_host(key(false, false, tuinix::KeyCode::BackTab));
        assert!(event.is_none());
        let mouse = tuinix::Input::Mouse(tuinix::MouseInput {
            kind: tuinix::MouseInputKind::LeftPress,
            position: tuinix::Position::ORIGIN,
            ctrl: false,
            alt: false,
            shift: false,
        });
        assert!(matches!(mouse, tuinix::Input::Mouse(_)));
        let mut app = empty_app();
        app.handle_input(mouse).expect("mouse is dropped");
        assert!(!app.quit);
    }

    #[test]
    fn ctrl_t_and_ctrl_q_are_commands_before_children() {
        let quit = command_from_key(
            key_event_from_host(key(true, false, tuinix::KeyCode::Char('q'))).expect("key"),
        );
        assert_eq!(quit, Some(AppCommand::Quit));
        let switch = command_from_key(
            key_event_from_host(key(true, false, tuinix::KeyCode::Char('t'))).expect("key"),
        );
        assert_eq!(switch, Some(AppCommand::Switch));
        let plain_q = command_from_key(
            key_event_from_host(key(false, false, tuinix::KeyCode::Char('q'))).expect("key"),
        );
        assert_eq!(plain_q, None);
        let ctrl_alt_q = command_from_key(
            key_event_from_host(key(true, true, tuinix::KeyCode::Char('q'))).expect("key"),
        );
        assert_eq!(ctrl_alt_q, None);
    }

    #[test]
    fn size_conversion_rejects_zero_and_u16_overflow() {
        assert!(size_from_host(tuinix::Size { rows: 0, cols: 80 }).is_err());
        assert!(size_from_host(tuinix::Size { rows: 24, cols: 0 }).is_err());
        assert!(
            size_from_host(tuinix::Size {
                rows: 24,
                cols: u16::MAX as usize + 1
            })
            .is_err()
        );
        let size = size_from_host(tuinix::Size {
            rows: 24,
            cols: u16::MAX as usize,
        })
        .expect("max");
        assert_eq!(size.cols.get(), u16::MAX);
    }

    #[test]
    fn position_conversion_is_total() {
        assert_eq!(
            position_to_host(termnix::Position { row: 0, col: 0 }),
            tuinix::Position::ORIGIN
        );
        assert_eq!(
            position_to_host(termnix::Position { row: 23, col: 79 }),
            tuinix::Position { row: 23, col: 79 }
        );
    }

    #[test]
    fn color_and_style_conversion() {
        assert_eq!(to_host_color(termnix::Color::Default), None);
        assert_eq!(
            to_host_color(termnix::Color::Rgb(1, 2, 3)),
            Some(tuinix::Color::new(1, 2, 3))
        );
        assert_eq!(host_palette(0), Some(tuinix::Color::BLACK));
        assert_eq!(host_palette(15), Some(tuinix::Color::new(255, 255, 255)));
        assert_eq!(host_palette(16), Some(tuinix::Color::new(0, 0, 0)));
        assert_eq!(host_palette(21), Some(tuinix::Color::new(0, 0, 255)));
        assert_eq!(host_palette(196), Some(tuinix::Color::new(255, 0, 0)));
        assert_eq!(host_palette(231), Some(tuinix::Color::new(255, 255, 255)));
        assert_eq!(host_palette(232), Some(tuinix::Color::new(8, 8, 8)));
        assert_eq!(host_palette(255), Some(tuinix::Color::new(238, 238, 238)));

        let style = to_host_style(termnix::Style {
            foreground: termnix::Color::Indexed(1),
            background: termnix::Color::Rgb(10, 20, 30),
            bold: true,
            italic: true,
            underline: true,
            reverse: true,
        });
        assert!(style.bold);
        assert!(style.italic);
        assert!(style.underline);
        assert!(style.reverse);
        assert_eq!(style.fg_color, Some(tuinix::Color::new(205, 0, 0)));
        assert_eq!(style.bg_color, Some(tuinix::Color::new(10, 20, 30)));
        assert_eq!(style.blink, false, "tuinix-only attributes stay off");
        assert_eq!(style.dim, false);
        assert_eq!(style.strikethrough, false);

        let plain = to_host_style(termnix::Style::default());
        assert_eq!(plain, tuinix::Style::new());
    }

    #[test]
    fn projection_carries_ascii_wide_continuation_and_cursor() {
        let mut terminal = termnix::TerminalState::new(termnix::Size::new(2, 10).expect("size"));
        terminal.feed(b"a\xe6\x97\xa5b\r\ncd");
        terminal.feed(b"\x1b[?25l");
        let projection = Projection::from_snapshot(&terminal.snapshot()).expect("projection");

        assert_eq!(projection.size, termnix::Size::new(2, 10).expect("size"));
        assert_eq!(projection.cursor, termnix::Position { row: 1, col: 2 });
        assert!(!projection.cursor_visible);
        let row0 = &projection.rows[0];
        assert_eq!(row0[0].ch, 'a');
        assert_eq!(row0[0].width, 1);
        assert_eq!(row0[1].ch, '日');
        assert_eq!(row0[1].width, 2);
        assert_eq!(row0[2].width, 0, "continuation cell");
        assert_eq!(row0[3].ch, 'b');
        assert_eq!(row0[4].ch, ' ');
    }

    #[test]
    fn frame_writer_handles_wide_and_styled_cells() {
        let grid = Projection {
            size: termnix::Size::new(2, 6).expect("size"),
            rows: vec![
                vec![
                    termnix::Cell::EMPTY,
                    termnix::Cell {
                        ch: 'あ',
                        width: 2,
                        style: termnix::Style {
                            bold: true,
                            ..termnix::Style::default()
                        },
                    },
                    termnix::Cell::CONTINUATION,
                    termnix::Cell::EMPTY,
                    termnix::Cell::EMPTY,
                    termnix::Cell::EMPTY,
                ],
                vec![termnix::Cell::EMPTY; 6],
            ],
            cursor: termnix::Position { row: 0, col: 0 },
            cursor_visible: true,
        };
        let mut frame = Frame::new(tuinix::Size { rows: 2, cols: 6 });
        write_grid(&mut frame, &grid).expect("write");
        assert_eq!(
            frame.next_position().row,
            2,
            "explicit newline terminates each row"
        );
        assert_eq!(frame.next_position().col, 0);

        let written = frame
            .chars()
            .filter(|(_, c)| !c.is_blank())
            .map(|(_, c)| c.value())
            .collect::<String>();
        assert_eq!(written, "あ", "the wide char is stored once, not twice");
    }

    #[test]
    fn write_grid_reports_a_clipped_cell() {
        // Five 2-column cells cannot fit in a 6-column row: the grid and the
        // frame disagree, which `write_grid` reports instead of truncating.
        let wide = termnix::Cell {
            ch: 'あ',
            width: 2,
            style: termnix::Style::default(),
        };
        let grid = Projection {
            size: termnix::Size::new(1, 6).expect("size"),
            rows: vec![vec![wide; 6]],
            cursor: termnix::Position { row: 0, col: 0 },
            cursor_visible: true,
        };
        let mut frame = Frame::new(tuinix::Size { rows: 1, cols: 6 });
        let err = write_grid(&mut frame, &grid).expect_err("clipped cell");
        assert!(err.contains("clipped"), "unexpected error: {err}");
    }

    #[test]
    fn classify_host_fds() {
        let input = PollEntry {
            fd: 10,
            events: libc::POLLIN,
            target: PollTarget::HostInput,
        };
        assert_eq!(
            classify_revents(input, libc::POLLIN),
            PollOutcome::HostInput
        );
        assert_eq!(
            classify_revents(input, libc::POLLHUP),
            PollOutcome::HostError("host input closed (POLLHUP)")
        );
        assert_eq!(
            classify_revents(input, libc::POLLERR),
            PollOutcome::HostError("host input fd reported POLLERR/POLLNVAL")
        );
        assert_eq!(
            classify_revents(input, libc::POLLNVAL),
            PollOutcome::HostError("host input fd reported POLLERR/POLLNVAL")
        );
        assert_eq!(classify_revents(input, 0), PollOutcome::Idle);

        let resize = PollEntry {
            fd: 11,
            events: libc::POLLIN,
            target: PollTarget::HostResize,
        };
        assert_eq!(
            classify_revents(resize, libc::POLLIN),
            PollOutcome::HostResize
        );
        assert_eq!(
            classify_revents(resize, libc::POLLHUP),
            PollOutcome::HostError("resize-signal closed (POLLHUP)")
        );
    }

    #[test]
    fn classify_session_fds() {
        let entry = PollEntry {
            fd: 12,
            events: libc::POLLIN,
            target: PollTarget::Session(1),
        };
        assert_eq!(
            classify_revents(entry, libc::POLLIN),
            PollOutcome::SessionReady(1)
        );
        assert_eq!(
            classify_revents(entry, libc::POLLOUT),
            PollOutcome::SessionReady(1)
        );
        assert_eq!(
            classify_revents(entry, libc::POLLHUP),
            PollOutcome::SessionReady(1)
        );
        assert_eq!(
            classify_revents(entry, libc::POLLNVAL),
            PollOutcome::SessionError(1)
        );
        assert_eq!(classify_revents(entry, 0), PollOutcome::Idle);
    }

    #[test]
    fn poll_entries_drop_host_input_while_pending_and_keep_signal() {
        let entries = build_poll_entries(10, 11, &[None, None], true);
        assert_eq!(entries.len(), 2);
        assert_eq!(entries[0].target, PollTarget::HostInput);
        assert_eq!(entries[1].target, PollTarget::HostResize);
        let entries = build_poll_entries(10, 11, &[None, None], false);
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].target, PollTarget::HostResize);
    }

    #[test]
    fn poll_timeout_is_floored_and_non_negative() {
        let deadline = Instant::now() + Duration::from_millis(51);
        let timeout = poll_timeout_ms(deadline);
        assert!((0..=51).contains(&timeout));
        assert_eq!(poll_timeout_ms(Instant::now() - Duration::from_secs(1)), 0);
        assert_eq!(poll_timeout_ms(Instant::now()), 0);
    }

    #[test]
    fn collect_inputs_stops_at_budget_and_incomplete_escapes() {
        let mut decoder = tuinix::InputDecoder::new();
        assert!(collect_inputs(&mut decoder, 10).is_empty());

        // Three complete keys, then an unterminated sequence.
        decoder.feed(b"abc\x1b");
        let inputs = collect_inputs(&mut decoder, 2);
        assert_eq!(inputs.len(), 2, "budget stops before the third input");
        let inputs = collect_inputs(&mut decoder, 10);
        assert_eq!(inputs.len(), 1);
        assert!(
            decoder.has_uncommitted_escape(),
            "a lone ESC is held, not decoded"
        );
        let stray = collect_inputs(&mut decoder, 10);
        assert!(stray.is_empty(), "held ESC must not decode: {stray:?}");

        // The held ESC is abandoned for the next part of the test: leaving it
        // buffered would glue it to the following `ESC` as an Alt+Escape.
        decoder.commit_escape();
        assert_eq!(collect_inputs(&mut decoder, 10).len(), 1);
        assert_eq!(decoder.buffered_bytes(), 0);

        // A sequence split across two feeds is parsed exactly once. The
        // terminal sends the arrow as `ESC [ 1 A`; splitting it after the
        // introducer crosses both a CSI prefix and the parameter run.
        decoder.feed(b"\x1b[");
        assert!(collect_inputs(&mut decoder, 10).is_empty());
        decoder.feed(b"1");
        assert!(collect_inputs(&mut decoder, 10).is_empty());
        decoder.feed(b"A");
        let inputs = collect_inputs(&mut decoder, 10);
        assert_eq!(inputs.len(), 1);
        assert_eq!(
            inputs[0],
            tuinix::Input::Key(key(false, false, tuinix::KeyCode::Up))
        );
        assert!(
            collect_inputs(&mut decoder, 10).is_empty(),
            "the sequence is consumed only once"
        );
    }

    #[test]
    fn pending_input_becomes_undeliverable_when_target_gone() {
        let mut app = empty_app();
        app.pending = Some(PendingInput {
            session: 0,
            event: termnix::KeyEvent::new(termnix::KeyCode::Char('x')),
        });
        app.deliver_pending().expect("drop is silent");
        assert!(app.pending.is_none());
    }

    #[test]
    fn switch_selected_requires_two_live_sessions() {
        let mut app = empty_app();
        app.switch_selected();
        assert_eq!(app.selected, 0, "no-op with no sessions");
        app.sessions[1] = None;
        app.switch_selected();
        assert_eq!(app.selected, 0, "no-op with one session");
    }

    #[test]
    fn child_scripts_are_distinct_and_loop() {
        let script0 = child_script(0);
        let script1 = child_script(1);
        assert_ne!(script0, script1);
        assert!(script0.contains("SESSION 0"));
        assert!(script1.contains("SESSION 1"));
        assert!(script0.contains("while IFS= read -r line"));
    }

    #[test]
    fn write_grid_terminates_each_row_with_a_newline() {
        let grid = Projection {
            size: termnix::Size::new(2, 2).expect("size"),
            rows: vec![
                vec![termnix::Cell::EMPTY, termnix::Cell::EMPTY],
                vec![termnix::Cell::EMPTY, termnix::Cell::EMPTY],
            ],
            cursor: termnix::Position { row: 0, col: 0 },
            cursor_visible: true,
        };
        let mut frame = Frame::new(tuinix::Size { rows: 2, cols: 2 });
        write_grid(&mut frame, &grid).expect("write");
        assert_eq!(frame.next_position(), tuinix::Position { row: 2, col: 0 });
    }
}
