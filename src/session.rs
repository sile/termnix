//! Single PTY-backed terminal session with runtime-free I/O.
//!
//! [`Session`] owns one PTY-backed terminal session. The caller drives it
//! from its own poll loop:
//!
//! ```no_run
//! # fn main() -> std::io::Result<()> {
//! # let mut session: termnix::Session = unimplemented!();
//! loop {
//!     // 1. Run everything that can be done without a new readiness edge.
//!     //    An edge-triggered loop must drain this before blocking, or the
//!     //    poll can miss the edge that a later pump would have consumed.
//!     //    Draining is right for one session; with several, visit each
//!     //    runnable session once per round instead (see below).
//!     while session.needs_pump() {
//!         session.pump_io(termnix::PumpBudget::default())?;
//!     }
//!     // 2. Nothing more is possible without readiness, so read the
//!     //    registration and wait. `interests` is re-read every round
//!     //    because it changes as the write queue drains and decoding
//!     //    resumes.
//!     let interests = session.interests();
//!     if session.fd().is_none() {
//!         break;
//!     }
//! #   let _ = interests;
//!     // 3. poll(...) on session.fd() for interests, then loop.
//! #   break;
//! }
//! # Ok(())
//! # }
//! ```
//!
//! Step 1 must come first: `interests()` reports what blocking would
//! usefully wait for, not what `pump_io` can do right now. Waiting while
//! `needs_pump()` is still true can hang, because the session has work that
//! produces no further readiness edge.
//!
//! The session never owns a poll loop and takes no readiness flags: the fd is
//! non-blocking, so `pump_io` learns what is possible from the `WouldBlock`
//! results of its own `read`/`write` calls. Owning, identifying, and
//! scheduling several sessions is the caller's responsibility. One `pump_io`
//! call is the scheduling quantum; a multi-session loop should rotate among
//! runnable sessions rather than draining one session to idle. How much work a
//! single call may do is bounded by the [`PumpBudget`] passed to it, so a
//! session with a large backlog cannot monopolise the loop.
//!
//! Application input and terminal replies share one FIFO write queue. A reply
//! is appended in chronological order and decoding pauses until that reply is
//! fully written, so at most one reply (bounded by a fixed internal size) is
//! ever pending and replies never overtake previously accepted input. The
//! write queue itself is unbounded; callers apply backpressure by comparing
//! [`Session::write_queue_len()`] against a limit of their own.
//!
//! Child exit and PTY EOF are tracked separately: reaping the child does not
//! disable I/O, so any remaining master-side output can still be drained until
//! EOF.

use std::{
    io::{self, ErrorKind, Read, Write},
    ops::Range,
    os::fd::{AsRawFd, RawFd},
    process::{Command, ExitStatus},
};

use crate::{
    input::Input,
    pty::{ClosingPtyProcess, PtyProcess, SignalOutcome},
    size::Size,
    terminal::TerminalState,
};

/// Maximum size of a single pending terminal reply.
///
/// The emulator answers a CPR (cursor position report) request with
/// `ESC [ <row> ; <col> R`. Both numbers are at most five digits because the
/// grid is `u16` sized, so the longest reply is `ESC [ 65535 ; 65535 R`,
/// which is 14 bytes. (Five digits is what matters for the length: the grid
/// cannot report 65536, and widening the type would not change the bound.) A
/// single decode unit must produce at most this many reply bytes; the
/// invariant is pinned by tests.
const MAX_PENDING_REPLY_BYTES: usize = 14;

/// Maximum raw bytes held between reading from the PTY and decoding.
const READ_BUFFER_LIMIT: usize = 65536;

/// Drop already-written `outbound` prefix once it reaches this size.
const OUTBOUND_COMPACT_THRESHOLD: usize = 4096;

/// Drop already-decoded `read_buffer` prefix once it reaches this size.
const READ_COMPACT_THRESHOLD: usize = 4096;

/// Ceiling on the work a single [`Session::pump_io()`] call performs.
///
/// One `pump_io` call is the scheduling quantum at the caller's level: the
/// caller decides, by rotating among sessions, how the work is interleaved.
/// This struct bounds *how much work* one such call may do in total. A ceiling
/// is required for that rotation to be possible at all: without it a child
/// that never stops writing could hold the PTY in one call forever, starving
/// the other sessions. The ceiling is a defaulted, inspectable value rather
/// than a hidden constant so callers can tighten it, for example to observe
/// [`SessionCounters::pump_budget_exhaustions`] with a small fixture, or widen
/// it to drain a single session in one call.
///
/// The ceiling counts both bytes moved (read + decoded + written) and
/// syscalls attempted. `pump_io` stops at whichever is reached first.
///
/// # Choosing a value
///
/// Most callers should pass [`PumpBudget::default()`], which is the recommended
/// 64 KiB / 64 syscall ceiling. Reach for a struct literal only when you are
/// deliberately tightening the ceiling (for instance to observe
/// [`SessionCounters::pump_budget_exhaustions`] with a small fixture) or
/// widening it (to drain a single session in one call).
///
/// Both fields are public and every value is valid, including zero: a zero in
/// either field makes every `pump_io` stop immediately, which reports an
/// exhausted budget without doing work. That is not an error — the call
/// returns `Ok(())`, increments
/// [`SessionCounters::pump_budget_exhaustions`], and leaves
/// [`Session::needs_pump()`] true for any work still pending.
///
/// # Examples
///
/// ```
/// # use termnix::PumpBudget;
/// // The usual case: take the recommended ceiling.
/// let budget = PumpBudget::default();
///
/// // Tighten it, so a pump stops after a single syscall.
/// let budget = PumpBudget { bytes: 65536, syscalls: 1 };
/// ```
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PumpBudget {
    /// Maximum bytes the pump may move in one call, counting reads, decoded
    /// output, and writes together.
    ///
    /// Zero makes every `pump_io` stop before moving anything, which reports an
    /// exhausted budget without doing work.
    pub bytes: usize,
    /// Maximum read and write syscalls the pump may issue in one call.
    ///
    /// Zero makes every `pump_io` stop before issuing any syscall, which
    /// reports an exhausted budget without doing work.
    pub syscalls: usize,
}

impl PumpBudget {
    /// Charges `bytes` and `syscalls` against the ceiling.
    fn consume(&mut self, bytes: usize, syscalls: usize) {
        self.bytes = self.bytes.saturating_sub(bytes);
        self.syscalls = self.syscalls.saturating_sub(syscalls);
    }

    /// Bytes still allowed in this pump.
    fn bytes_left(&self) -> usize {
        self.bytes
    }

    /// Whether either ceiling has been reached.
    fn exhausted(&self) -> bool {
        self.bytes == 0 || self.syscalls == 0
    }
}

impl Default for PumpBudget {
    /// The recommended ceiling: 64 KiB of traffic or 64 syscalls, whichever
    /// comes first.
    ///
    /// See [`PumpBudget`] for when to choose your own instead.
    fn default() -> Self {
        Self {
            bytes: 65536,
            syscalls: 64,
        }
    }
}

/// Read/write interests an event loop should register for the session's fd.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub struct Interests {
    /// Whether readable events should be polled.
    pub readable: bool,
    /// Whether writable events should be polled.
    pub writable: bool,
}

/// Cumulative counters tracked by a session, returned by [`Session::counters()`].
///
/// Every field is a running total, which is never reset for the lifetime of the
/// session. How much is buffered right now is not here: the write queue's
/// occupancy is [`Session::write_queue_len()`], and the scrollback size is
/// reached through [`Session::terminal_state()`]. This type never mixes running
/// totals with the quantities derived from them.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SessionCounters {
    /// Cumulative `pump_io` calls.
    pub pump_calls: u64,
    /// Cumulative pumps that ended with their [`PumpBudget`] exhausted.
    ///
    /// A pump that exhausts its budget stopped with work still pending, so
    /// [`Session::needs_pump()`] stays true. Callers can use a delta of this
    /// counter as an oracle that a single pump did not finish the backlog,
    /// without knowing the budget's numeric value.
    pub pump_budget_exhaustions: u64,
    /// Cumulative bytes read from the PTY.
    pub pty_bytes_read: u64,
    /// Cumulative bytes fed to the terminal emulator.
    pub terminal_bytes_processed: u64,
    /// Cumulative reply bytes generated by the terminal emulator.
    pub terminal_reply_bytes_generated: u64,
    /// Cumulative bytes accepted through [`Session::enqueue_input()`].
    pub input_bytes_enqueued: u64,
    /// Cumulative application input bytes written to the PTY.
    ///
    /// Together with [`Self::reply_bytes_written`], this accounts for every
    /// byte written to the PTY.
    pub input_bytes_written: u64,
    /// Cumulative reply bytes written to the PTY.
    ///
    /// Together with [`Self::input_bytes_written`], this accounts for every
    /// byte written to the PTY.
    pub reply_bytes_written: u64,
    /// Cumulative `read` syscalls attempted.
    pub read_syscalls: u64,
    /// Cumulative `write` syscalls attempted.
    pub write_syscalls: u64,
    /// Cumulative `read` syscalls that returned `WouldBlock`.
    pub read_would_block: u64,
    /// Cumulative `write` syscalls that returned `WouldBlock`.
    pub write_would_block: u64,
}

/// Observable lifecycle phase of a session.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SessionStatus {
    /// PTY master open, I/O active.
    Live,
    /// PTY EOF observed; remaining output drained.
    Eof,
    /// Logically closed; master closed, awaiting reap.
    Closing,
    /// Direct child reaped and I/O finished; the session is spent.
    ///
    /// This is the *session* being over, not the *child* having exited: it
    /// requires both that the child was reaped and that I/O finished (PTY EOF
    /// or a logical close). A child that has exited while output is still
    /// being drained is not here; ask [`Session::exit_status()`] for that
    /// question instead.
    Reaped,
}

/// Direction cursor used for read/write fairness within one pump.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Direction {
    Read,
    Write,
}

impl Direction {
    fn opposite(self) -> Self {
        match self {
            Self::Read => Self::Write,
            Self::Write => Self::Read,
        }
    }
}

/// Lifecycle phase of one session.
///
/// Tracks I/O and logical close. Child exit is stored separately in
/// [`Session::exit_status()`]; reaping alone does not enter [`Phase::Reaped`].
#[derive(Clone, Copy, PartialEq, Eq)]
enum Phase {
    Live,
    Eof,
    Closing,
    Reaped,
}

/// Owned PTY side of a session.
enum Pty {
    Live(PtyProcess),
    Closing(ClosingPtyProcess),
}

impl Pty {
    fn is_live(&self) -> bool {
        matches!(self, Self::Live(_))
    }

    fn fd(&self) -> Option<RawFd> {
        match self {
            Self::Live(pty) => Some(pty.as_raw_fd()),
            Self::Closing(_) => None,
        }
    }
}

/// A single PTY-backed terminal session.
///
/// The session owns the PTY master fd, the emulator state, a bounded read
/// buffer, an unbounded write queue, and the child-process lifecycle, but
/// never owns a poll loop. Register [`Session::fd()`] with
/// [`Session::interests()`], call [`Session::pump_io()`] when the fd is ready (or
/// after any state change), and drain [`Session::needs_pump()`] before
/// blocking in the poll loop; the module documentation shows the canonical
/// loop. With several sessions, treat each `pump_io` as one quantum and rotate
/// among runnable sessions; pass a [`PumpBudget`] to bound how much work one
/// quantum does.
///
/// # Drop behavior
///
/// Dropping the session performs best-effort cleanup: the remaining child is
/// force-killed and reaped, which may block. Failures cannot be reported from
/// `Drop`; call [`Session::shutdown()`] to observe the result.
pub struct Session {
    pty: Option<Pty>,
    term: TerminalState,
    phase: Phase,
    /// Cached status once the direct child has been reaped.
    ///
    /// Independent of [`Self::phase`]: I/O may continue until PTY EOF after
    /// the child exits.
    exit_status: Option<ExitStatus>,
    /// Raw bytes read from the PTY but not yet decoded.
    read_buffer: Vec<u8>,
    /// Number of leading `read_buffer` bytes already decoded.
    read_offset: usize,
    /// Bytes waiting to be written, in chronological order.
    outbound: Vec<u8>,
    /// Number of leading `outbound` bytes already written.
    write_offset: usize,
    /// Range of the one unsent terminal reply inside `outbound`.
    ///
    /// While set, reading and decoding are paused until the reply is fully
    /// written, so at most one reply is ever pending.
    pending_reply_range: Option<Range<usize>>,
    /// Direction the next pump should start with.
    direction: Direction,
    /// Whether the most recent pump stopped reading on `WouldBlock`.
    read_would_block: bool,
    /// Whether the most recent pump stopped writing on `WouldBlock`.
    write_would_block: bool,
    counters: SessionCounters,
}

impl Session {
    /// Spawns `command` in a new PTY-backed terminal session.
    ///
    /// On failure no session is created and any opened PTY fd and child are
    /// reclaimed.
    pub fn new(command: &mut Command, size: Size) -> io::Result<Self> {
        let mut pty = PtyProcess::spawn(command, size)?;
        if let Err(err) = pty.set_nonblocking(true) {
            // Reclaim the fd and the child before reporting the failure.
            let mut closing = pty.into_closing();
            let _ = closing.signal_kill();
            let _ = closing.wait_and_reap();
            return Err(err);
        }
        let term = TerminalState::new(size);
        Ok(Self {
            pty: Some(Pty::Live(pty)),
            term,
            phase: Phase::Live,
            exit_status: None,
            read_buffer: Vec::new(),
            read_offset: 0,
            outbound: Vec::new(),
            write_offset: 0,
            pending_reply_range: None,
            direction: Direction::Write,
            read_would_block: false,
            write_would_block: false,
            counters: SessionCounters::default(),
        })
    }

    /// Returns the fd to register with an event loop, if any.
    ///
    /// When `Some`, the fd is always non-blocking; `Session` sets that mode at
    /// construction and never changes it. `None` is returned after a logical
    /// close or once the session is spent; callers should unregister the
    /// previous registration then. Reaping the child while the master is still
    /// open does not clear the fd.
    pub fn fd(&self) -> Option<RawFd> {
        if self.phase == Phase::Closing || self.phase == Phase::Reaped {
            return None;
        }
        self.pty.as_ref().and_then(Pty::fd)
    }

    /// Returns the poll interests the fd should currently be registered with.
    ///
    /// Re-read after any state change (notably after `enqueue_input`, which
    /// may turn on the writable interest) and after every `pump_io`.
    pub fn interests(&self) -> Interests {
        if self.phase == Phase::Closing || self.phase == Phase::Reaped {
            return Interests::default();
        }
        let mut interests = Interests::default();
        if self.phase != Phase::Eof
            && !self.read_paused()
            && self.pty.as_ref().is_some_and(Pty::is_live)
        {
            interests.readable = true;
        }
        if !self.outbound_empty() {
            interests.writable = true;
        }
        interests
    }

    /// Whether `pump_io` has more work that does not need a new readiness
    /// edge.
    ///
    /// Returns `false` when the last pump stopped because a `read` or `write`
    /// returned `WouldBlock`; the caller should then wait for the interest
    /// reported by [`Session::interests()`]. Returns `true` when internal work
    /// (buffered decoding, a pending reply, or a pump interrupted by its
    /// [`PumpBudget`]) is still executable immediately, which an
    /// edge-triggered loop must drain before blocking. With several sessions,
    /// prefer rotating among `needs_pump` sessions instead of draining one
    /// session in a tight loop.
    pub fn needs_pump(&self) -> bool {
        if self.phase == Phase::Closing || self.phase == Phase::Reaped {
            return false;
        }
        if !self.outbound_empty() && !self.write_would_block {
            return true;
        }
        if self.phase == Phase::Live && !self.read_paused() && !self.read_would_block {
            return true;
        }
        self.buffered_read_len() > 0 && !self.read_paused()
    }

    /// Advances the session: writes queued bytes, reads and decodes PTY
    /// output, and detects EOF, bounded by `budget`.
    ///
    /// The fd is non-blocking, so no readiness flags are taken; `WouldBlock`
    /// results decide how far a single call goes. A call stops early when
    /// `budget` is exhausted, which [`SessionCounters::pump_budget_exhaustions`]
    /// counts; [`needs_pump`](Self::needs_pump()) then stays true so the caller
    /// can return to this session on a later rotation. After a logical close
    /// this is a successful no-op.
    ///
    /// Pass [`PumpBudget::default()`] for the customary 64 KiB / 64 syscall
    /// ceiling.
    ///
    /// # Errors
    ///
    /// Returns any error surfaced by the underlying `read` or `write`, except
    /// that the `EIO` reported by the kernel when the last slave fd closes is
    /// not an error. (Its Rust face is usually
    /// [`ErrorKind::ReadOnlyFilesystem`], but the `ErrorKind` is not portable;
    /// the crate classifies by the raw `EIO` code.) This is the normal way for
    /// the master side to learn that the child side is gone, and it can arrive
    /// before or after [`Session::try_wait()`] observes the exit; both the read
    /// and the write direction treat it as PTY EOF, so a write that meets it
    /// retires the queue and the session reaches [`SessionStatus::Eof`] just
    /// as a read-side `EIO` would. A
    /// [`ErrorKind::InvalidData`] error means the emulator produced a reply
    /// longer than its internal bound, which is a library invariant violation
    /// rather than a recoverable condition.
    pub fn pump_io(&mut self, budget: PumpBudget) -> io::Result<()> {
        if self.phase == Phase::Closing || self.phase == Phase::Reaped {
            return Ok(());
        }
        self.counters.pump_calls = self.counters.pump_calls.saturating_add(1);
        self.read_would_block = false;
        self.write_would_block = false;
        let mut budget = budget;
        let mut direction = self.direction;
        for _ in 0..2 {
            if budget.exhausted() {
                break;
            }
            // Start with the direction that actually has work, so a session
            // with nothing to write is not blocked behind the write phase.
            if !self.has_direction_work(direction) {
                direction = direction.opposite();
                if !self.has_direction_work(direction) {
                    break;
                }
            }
            let progressed = match direction {
                Direction::Write => self.write_phase(&mut budget)?,
                Direction::Read => self.read_phase(&mut budget)?,
            };
            if !progressed {
                // A WouldBlock (or empty) on one direction must not skip the
                // other: e.g. after writing some input, a read WouldBlock used
                // to leave later queued bytes unflushed until the next readiness
                // edge, while needs_pump stayed true and skipped poll.
                let other = direction.opposite();
                if self.has_direction_work(other) {
                    direction = other;
                    continue;
                }
                break;
            }
            direction = direction.opposite();
            if !self.has_direction_work(direction) {
                break;
            }
        }
        self.direction = direction;
        if budget.exhausted() {
            self.counters.pump_budget_exhaustions =
                self.counters.pump_budget_exhaustions.saturating_add(1);
        }
        Ok(())
    }

    /// Returns how many bytes are queued for writing to the PTY and have not
    /// been written yet, counting application input and terminal replies
    /// together.
    ///
    /// Application input and terminal replies share a single write queue, in
    /// chronological order. The queue is unbounded, and the session applies no
    /// backpressure policy of its own: compare this against a limit of your
    /// own to decide whether to enqueue, hold, or drop an input. Add
    /// [`Session::input_byte_len()`] for the size the input would contribute.
    ///
    /// Takes `&self` and performs no syscall.
    pub fn write_queue_len(&self) -> usize {
        self.unsent()
    }

    /// Returns how many bytes `input` would add to the write queue with the
    /// session's current terminal modes.
    ///
    /// Equal to `input.byte_len(self.terminal_state().modes())`. Provided
    /// because the modes are part of the session's state: a caller applying an
    /// input limit through [`Session::write_queue_len()`] needs this size and
    /// would otherwise have to fetch the modes from
    /// [`Session::terminal_state()`].
    ///
    /// Takes `&self` and performs no syscall. The session's modes can change
    /// between calls as the child writes escape sequences, so a size is only
    /// valid for the modes at the moment it was taken; size an input and
    /// enqueue it back to back.
    pub fn input_byte_len(&self, input: Input<'_>) -> usize {
        input.byte_len(self.term.modes())
    }

    /// Enqueues application [`Input`] to be written to the PTY.
    ///
    /// `Key` and `Paste` are turned into bytes with the session's current
    /// terminal modes; `Raw` is appended unchanged. All accepted bytes join
    /// the write queue in chronological order. The session applies no
    /// backpressure policy; compare [`Session::write_queue_len()`] plus
    /// [`Session::input_byte_len()`] against a limit of your own to decide
    /// whether to enqueue, hold the input on the caller side, or drop it. A
    /// closed session returns [`ErrorKind::BrokenPipe`].
    pub fn enqueue_input(&mut self, input: Input<'_>) -> io::Result<()> {
        if self.phase != Phase::Live && self.phase != Phase::Eof {
            return Err(io::Error::new(ErrorKind::BrokenPipe, "session is closed"));
        }
        let modes = self.term.modes();
        let before = self.outbound.len();
        input.write_to(modes, &mut self.outbound);
        let n = self.outbound.len() - before;
        self.counters.input_bytes_enqueued =
            self.counters.input_bytes_enqueued.saturating_add(n as u64);
        self.write_would_block = false;
        Ok(())
    }

    /// Resizes the session, updating both the kernel PTY size and the
    /// emulator.
    ///
    /// Reapplying the current size succeeds without a syscall. The ioctl runs
    /// first; the emulator size is only updated when the ioctl succeeds. A
    /// closed session returns [`ErrorKind::BrokenPipe`].
    pub fn resize(&mut self, size: Size) -> io::Result<()> {
        if self.phase != Phase::Live && self.phase != Phase::Eof {
            return Err(io::Error::new(ErrorKind::BrokenPipe, "session is closed"));
        }
        if self.term.size() == size {
            return Ok(());
        }
        let Some(Pty::Live(pty)) = self.pty.as_mut() else {
            return Err(io::Error::new(ErrorKind::BrokenPipe, "session is closed"));
        };
        pty.resize(size)?;
        self.term.resize(size);
        Ok(())
    }

    /// Logically closes the session.
    ///
    /// This synchronously discards the write queue, pending reply and unread
    /// bytes, and closes the PTY master. The child is kept in a private
    /// closing state until reaped. After closing, `pump_io` is a successful
    /// no-op, `enqueue_input` and `resize` return [`ErrorKind::BrokenPipe`],
    /// while the final terminal state, counters, process polling and signal
    /// delivery remain available until the child is reaped. Idempotent.
    pub fn close(&mut self) {
        if self.phase == Phase::Reaped {
            return;
        }
        if self.phase != Phase::Closing {
            self.outbound.clear();
            self.write_offset = 0;
            self.pending_reply_range = None;
            self.read_buffer.clear();
            self.read_offset = 0;
            self.phase = Phase::Closing;
            let pty = self.pty.take();
            self.pty = Some(match pty {
                Some(Pty::Live(pty)) => Pty::Closing(pty.into_closing()),
                other => other.expect("pty is present while closing"),
            });
        }
    }

    /// Returns the cached exit status without touching the child.
    ///
    /// `None` until the child has been reaped, whether by [`Session::try_wait()`],
    /// [`Session::wait()`], [`Session::shutdown()`], or a [`Drop`]. Unlike
    /// `try_wait`, this performs no syscall and cannot fail, so it is the way
    /// to ask "has the child already exited?" from code that must not block
    /// or reap, for example while driving other sessions.
    ///
    /// This is the exit of the direct child only. It says nothing about PTY
    /// I/O, which continues until EOF independently of the child's state.
    ///
    /// # "The child exited" and "the session ended" are different questions
    ///
    /// `exit_status().is_some()` answers *the child has exited*. It becomes
    /// true as soon as the child is reaped, including while output the child
    /// wrote before exiting is still being drained.
    ///
    /// [`Session::status()`] answering [`SessionStatus::Reaped`] answers *the
    /// session is over*, which additionally requires I/O to have finished.
    ///
    /// The two are not alternatives. A pane that closes when its program ends
    /// usually has to keep draining after the child is gone: closing on the
    /// child's exit alone would lose that output, while waiting for `Reaped`
    /// would hold the pane open past the last useful byte. Neither is a
    /// substitute for the other, and since `exit_status().is_some()` already
    /// implies the child is gone, pairing it with a `Reaped` comparison adds
    /// nothing.
    ///
    /// ```no_run
    /// # let mut session: termnix::Session = unimplemented!();
    /// // The child has exited; keep going until its output is drained.
    /// let child_gone = session.exit_status().is_some();
    /// let session_over = session.status() == termnix::SessionStatus::Reaped;
    /// # let _ = (child_gone, session_over);
    /// ```
    ///
    /// Two runnable examples show the split: `examples/headless.rs` tracks the
    /// child's exit separately from draining its output, and
    /// `examples/tuinix.rs` drops a pane only once the session is `Reaped`.
    pub fn exit_status(&self) -> Option<ExitStatus> {
        self.exit_status
    }

    /// Observes and reaps the child without blocking.
    ///
    /// Returns `Ok(Some(status))` once the child has exited, caching the
    /// status; later calls return the same value. Reaping does not stop PTY
    /// I/O: while the master is still open the session stays readable until
    /// EOF. The session enters [`SessionStatus::Reaped`] only after the child
    /// is reaped and I/O is finished (EOF or a logical close).
    ///
    /// A child that has exited is not a session that is over; for the yes/no
    /// question alone, [`Session::exit_status()`] answers without a syscall.
    pub fn try_wait(&mut self) -> io::Result<Option<ExitStatus>> {
        if let Some(status) = self.exit_status {
            return Ok(Some(status));
        }
        match self.pty.as_mut() {
            Some(Pty::Live(pty)) => {
                let status = pty.try_wait()?;
                if let Some(status) = status {
                    self.note_exit(status);
                    Ok(Some(status))
                } else {
                    Ok(None)
                }
            }
            Some(Pty::Closing(pty)) => {
                if pty.poll_exit()?.is_none() {
                    return Ok(None);
                }
                let status = pty.wait_and_reap()?;
                self.exit_status = Some(status);
                self.phase = Phase::Reaped;
                Ok(Some(status))
            }
            None => Ok(None),
        }
    }

    /// Blocks until the child exits and returns its status.
    ///
    /// After a successful reap the status is cached and later calls return it.
    /// As with [`Session::try_wait()`], reaping alone does not disable PTY I/O.
    pub fn wait(&mut self) -> io::Result<ExitStatus> {
        if let Some(status) = self.exit_status {
            return Ok(status);
        }
        match self.pty.as_mut() {
            Some(Pty::Live(pty)) => {
                let status = pty.wait()?;
                self.note_exit(status);
                Ok(status)
            }
            Some(Pty::Closing(pty)) => {
                let status = pty.wait_and_reap()?;
                self.exit_status = Some(status);
                self.phase = Phase::Reaped;
                Ok(status)
            }
            None => Err(io::Error::new(
                ErrorKind::BrokenPipe,
                "session already reaped",
            )),
        }
    }

    /// Returns read-only access to the session's terminal state.
    ///
    /// Available in every phase, including after the child has been reaped.
    /// The returned reference exposes the live state directly, so a caller
    /// that keeps the data after the session moves on should copy what it
    /// needs out of the state.
    pub fn terminal_state(&self) -> &TerminalState {
        &self.term
    }

    /// Returns the session's cumulative activity counters.
    ///
    /// The reference exposes the live counters directly, and they are never
    /// reset for the lifetime of the session. A caller that keeps the values
    /// after the session moves on should clone them, as with
    /// [`Session::terminal_state()`].
    pub fn counters(&self) -> &SessionCounters {
        &self.counters
    }

    /// Removes the oldest scrollback lines until both limits hold.
    ///
    /// Delegates to [`TerminalState::trim_scrollback()`]; lines are removed
    /// whole, oldest first, and either limit at zero clears the history.
    pub fn trim_scrollback(&mut self, max_lines: usize, max_cells: usize) {
        self.term.trim_scrollback(max_lines, max_cells);
    }

    /// Returns the observable lifecycle phase of the session.
    pub fn status(&self) -> SessionStatus {
        match self.phase {
            Phase::Live => SessionStatus::Live,
            Phase::Eof => SessionStatus::Eof,
            Phase::Closing => SessionStatus::Closing,
            Phase::Reaped => SessionStatus::Reaped,
        }
    }

    /// Sends a graceful termination signal (SIGTERM) to the session's process
    /// group.
    ///
    /// See [`SignalOutcome`] for what each result means for the caller.
    pub fn terminate(&mut self) -> io::Result<SignalOutcome> {
        match self.pty.as_mut() {
            Some(Pty::Live(pty)) => pty.signal_group(libc::SIGTERM),
            Some(Pty::Closing(pty)) => pty.signal_terminate(),
            None => Err(io::Error::new(
                ErrorKind::BrokenPipe,
                "session already reaped",
            )),
        }
    }

    /// Sends a force-termination signal (SIGKILL) to the session's process
    /// group.
    ///
    /// See [`SignalOutcome`] for what each result means for the caller.
    pub fn force_terminate(&mut self) -> io::Result<SignalOutcome> {
        match self.pty.as_mut() {
            Some(Pty::Live(pty)) => pty.signal_group(libc::SIGKILL),
            Some(Pty::Closing(pty)) => pty.signal_kill(),
            None => Err(io::Error::new(
                ErrorKind::BrokenPipe,
                "session already reaped",
            )),
        }
    }

    /// Cleans up this session, force-killing and reaping the direct child.
    ///
    /// This consumes the session and may block while reaping. Both this and
    /// [`Drop`] can block for as long as the child takes to die, so a session
    /// that shares a poll loop with others must be taken out of the rotation
    /// first; blocking here would stall every other session in that loop.
    /// Prefer [`Session::terminate()`] (SIGTERM) or [`Session::force_terminate()`]
    /// (SIGKILL) to request the exit without blocking, then reap with
    /// `try_wait` from the loop.
    pub fn shutdown(mut self) -> io::Result<ExitStatus> {
        if let Some(status) = self.exit_status.take() {
            // Child already reaped; still close the master if it remains.
            match self.pty.take() {
                Some(Pty::Live(pty)) => {
                    let _ = pty.into_closing();
                }
                Some(Pty::Closing(_)) | None => {}
            }
            return Ok(status);
        }
        let mut closing = match self.pty.take() {
            Some(Pty::Live(pty)) => pty.into_closing(),
            Some(Pty::Closing(closing)) => closing,
            None => {
                return Err(io::Error::new(
                    ErrorKind::BrokenPipe,
                    "session already reaped",
                ));
            }
        };
        let _ = closing.signal_kill();
        closing.wait_and_reap()
    }

    /// Records a freshly reaped exit status and enters `Reaped` if I/O is done.
    fn note_exit(&mut self, status: ExitStatus) {
        self.exit_status = Some(status);
        if self.phase == Phase::Eof {
            self.phase = Phase::Reaped;
        }
    }

    /// Marks PTY EOF and enters `Reaped` when the child was already reaped.
    fn note_eof(&mut self) {
        self.phase = Phase::Eof;
        if self.exit_status.is_some() {
            self.phase = Phase::Reaped;
        }
    }

    /// Whether decoding and reading are paused on a pending terminal reply.
    fn read_paused(&self) -> bool {
        self.pending_reply_range.is_some()
    }

    /// Returns whether `outbound` holds any unsent bytes.
    fn outbound_empty(&self) -> bool {
        self.outbound.len() == self.write_offset
    }

    /// Returns the number of unsent bytes currently held.
    fn unsent(&self) -> usize {
        self.outbound.len() - self.write_offset
    }

    /// Returns the number of buffered but not yet decoded read bytes.
    fn buffered_read_len(&self) -> usize {
        self.read_buffer.len() - self.read_offset
    }

    /// Drops the already-written prefix of `outbound` when it grows large.
    fn compact_outbound_if_needed(&mut self) {
        if self.write_offset == 0 {
            return;
        }
        if self.write_offset < OUTBOUND_COMPACT_THRESHOLD && !self.outbound_empty() {
            return;
        }
        if let Some(range) = self.pending_reply_range.as_ref()
            && self.write_offset >= range.end
        {
            self.pending_reply_range = None;
        }
        let drained = self.write_offset;
        self.outbound.drain(..drained);
        if let Some(range) = self.pending_reply_range.as_mut() {
            // write_offset may sit inside the reply range after a partial write.
            range.start = range.start.saturating_sub(drained);
            range.end -= drained;
        }
        self.write_offset = 0;
    }

    /// Drops the already-decoded prefix of `read_buffer` when it grows large.
    fn compact_read_buffer_if_needed(&mut self) {
        if self.read_offset == 0 {
            return;
        }
        if self.read_offset < READ_COMPACT_THRESHOLD && self.buffered_read_len() > 0 {
            return;
        }
        self.read_buffer.drain(..self.read_offset);
        self.read_offset = 0;
    }

    /// Whether the given direction currently has work.
    fn has_direction_work(&self, direction: Direction) -> bool {
        match direction {
            Direction::Write => !self.outbound_empty(),
            Direction::Read => {
                (self.buffered_read_len() > 0 && !self.read_paused())
                    || (self.phase == Phase::Live
                        && !self.read_paused()
                        && self.pty.as_ref().is_some_and(Pty::is_live))
            }
        }
    }

    /// Writes pending bytes, bounded by the pump budget.
    fn write_phase(&mut self, budget: &mut PumpBudget) -> io::Result<bool> {
        let mut progressed = false;
        loop {
            if budget.exhausted() {
                break;
            }
            if self.phase == Phase::Closing || self.phase == Phase::Reaped {
                break;
            }
            let unsent = self.unsent();
            if unsent == 0 {
                break;
            }
            let cap = budget.bytes_left().min(unsent);
            if cap == 0 {
                break;
            }
            let Some(Pty::Live(mut pty)) = self.pty.take() else {
                break;
            };
            let start = self.write_offset;
            let result = pty.write(&self.outbound[start..start + cap]);
            self.pty = Some(Pty::Live(pty));
            budget.consume(0, 1);
            self.counters.write_syscalls = self.counters.write_syscalls.saturating_add(1);
            match result {
                Ok(0) => break,
                Ok(n) => {
                    budget.consume(n, 0);
                    // Split the written span into reply bytes and input bytes by
                    // its overlap with the pending reply range, so the two
                    // consumption totals stay separable.
                    let reply_written =
                        pending_reply_unsent_len(start, self.pending_reply_range.as_ref())
                            .saturating_sub(pending_reply_unsent_len(
                                start + n,
                                self.pending_reply_range.as_ref(),
                            ));
                    self.write_offset += n;
                    self.counters.reply_bytes_written = self
                        .counters
                        .reply_bytes_written
                        .saturating_add(reply_written as u64);
                    self.counters.input_bytes_written = self
                        .counters
                        .input_bytes_written
                        .saturating_add((n - reply_written) as u64);
                    self.compact_outbound_if_needed();
                    progressed = true;
                }
                Err(err) if err.kind() == ErrorKind::WouldBlock => {
                    self.counters.write_would_block =
                        self.counters.write_would_block.saturating_add(1);
                    self.write_would_block = true;
                    break;
                }
                Err(err) if err.kind() == ErrorKind::Interrupted => break,
                Err(err) if is_pty_gone(&err) => {
                    // The slave end is fully closed, so no future write can
                    // succeed. Retire the queue and report EOF, the same way a
                    // read-side `EIO` does, so both directions agree that "the
                    // pty went away" is the peer being gone rather than a
                    // `pump_io` failure. The queue is discarded without
                    // touching the byte counters, matching `close()`.
                    self.outbound.clear();
                    self.write_offset = 0;
                    self.pending_reply_range = None;
                    self.note_eof();
                    break;
                }
                Err(err) => return Err(err),
            }
        }
        // If the pending reply was fully written but later input remains,
        // decoding may resume.
        if let Some(range) = self.pending_reply_range.as_ref()
            && self.write_offset >= range.end
        {
            self.pending_reply_range = None;
        }
        Ok(progressed)
    }

    /// Reads and decodes bytes, bounded by the pump budget.
    fn read_phase(&mut self, budget: &mut PumpBudget) -> io::Result<bool> {
        let mut progressed = false;
        progressed |= self.decode_buffered(budget)? > 0;

        if self.phase != Phase::Live || self.read_paused() {
            return Ok(progressed);
        }

        loop {
            if budget.exhausted() {
                break;
            }
            if self.read_paused() || self.phase != Phase::Live {
                break;
            }
            let room = READ_BUFFER_LIMIT.saturating_sub(self.buffered_read_len());
            if room == 0 {
                break;
            }
            let cap = budget.bytes_left().min(room);
            if cap == 0 {
                break;
            }
            self.compact_read_buffer_if_needed();
            let Some(Pty::Live(mut pty)) = self.pty.take() else {
                break;
            };
            let start = self.read_buffer.len();
            self.read_buffer.resize(start + cap, 0);
            let result = pty.read(&mut self.read_buffer[start..]);
            self.pty = Some(Pty::Live(pty));
            budget.consume(0, 1);
            self.counters.read_syscalls = self.counters.read_syscalls.saturating_add(1);
            match result {
                Ok(0) => {
                    self.read_buffer.truncate(start);
                    self.note_eof();
                    break;
                }
                Ok(n) => {
                    self.read_buffer.truncate(start + n);
                    budget.consume(n, 0);
                    self.counters.pty_bytes_read =
                        self.counters.pty_bytes_read.saturating_add(n as u64);
                    progressed = true;
                    self.decode_buffered(budget)?;
                    if self.read_paused() {
                        break;
                    }
                }
                Err(err) if err.kind() == ErrorKind::WouldBlock => {
                    self.read_buffer.truncate(start);
                    self.counters.read_would_block =
                        self.counters.read_would_block.saturating_add(1);
                    self.read_would_block = true;
                    break;
                }
                Err(err) if err.kind() == ErrorKind::Interrupted => {
                    self.read_buffer.truncate(start);
                    break;
                }
                Err(err) if is_pty_gone(&err) => {
                    // PTY slave close is reported as EIO on some platforms;
                    // normalize it to EOF.
                    self.read_buffer.truncate(start);
                    self.note_eof();
                    break;
                }
                Err(err) => {
                    self.read_buffer.truncate(start);
                    return Err(err);
                }
            }
        }
        Ok(progressed)
    }

    /// Decodes already-buffered raw bytes, one at a time, until paused, empty,
    /// or the budget is exhausted. Returns the number of bytes decoded.
    fn decode_buffered(&mut self, budget: &mut PumpBudget) -> io::Result<usize> {
        let mut decoded = 0;
        loop {
            if budget.bytes_left() == 0 {
                break;
            }
            if self.read_paused() || self.buffered_read_len() == 0 {
                break;
            }
            let byte = self.read_buffer[self.read_offset];
            self.read_offset += 1;
            budget.consume(1, 0);
            self.counters.terminal_bytes_processed =
                self.counters.terminal_bytes_processed.saturating_add(1);
            self.decode_byte(byte)?;
            decoded += 1;
        }
        self.compact_read_buffer_if_needed();
        Ok(decoded)
    }

    /// Feeds one byte into the emulator and appends any produced reply.
    ///
    /// A reply is appended to the write queue unconditionally, preserving
    /// chronological order behind previously accepted input, and decoding
    /// pauses until that reply is fully written. At most one reply is pending
    /// at a time because no further bytes are decoded while paused.
    ///
    /// The emulator keeps its reply until the caller says it was written, but
    /// the session is the caller that writes it: it copies the bytes into
    /// `outbound` here and advances the emulator buffer in the same step, so
    /// ownership moves in one hop and `pending_reply_range` (not the emulator
    /// buffer) is what keeps a reply ordered ahead of later input.
    fn decode_byte(&mut self, byte: u8) -> io::Result<()> {
        self.term.feed(&[byte]);
        let replies = self.term.pending_reply_bytes();
        // Scrollback may have grown even when no reply was produced.
        if replies.is_empty() {
            return Ok(());
        }
        if replies.len() > MAX_PENDING_REPLY_BYTES {
            return Err(io::Error::new(
                ErrorKind::InvalidData,
                "terminal reply exceeds the internal pending reply bound",
            ));
        }
        let start = self.outbound.len();
        self.outbound.extend_from_slice(replies);
        let len = replies.len();
        self.term.advance_reply_bytes(len);
        self.counters.terminal_reply_bytes_generated = self
            .counters
            .terminal_reply_bytes_generated
            .saturating_add(len as u64);
        self.pending_reply_range = Some(start..start + len);
        Ok(())
    }
}

impl Drop for Session {
    fn drop(&mut self) {
        // Best-effort cleanup: force-kill and reap the remaining child. This
        // may block, and failures cannot be reported from `Drop`.
        if self.exit_status.is_some() {
            match self.pty.take() {
                Some(Pty::Live(pty)) => {
                    let _ = pty.into_closing();
                }
                Some(Pty::Closing(_)) | None => {}
            }
            return;
        }
        let mut closing = match self.pty.take() {
            Some(Pty::Live(pty)) => pty.into_closing(),
            Some(Pty::Closing(closing)) => closing,
            None => return,
        };
        let _ = closing.signal_kill();
        let _ = closing.wait_and_reap();
    }
}

/// Whether an I/O error means the PTY slave end is gone.
///
/// Closing the last slave fd makes the kernel report [`libc::EIO`] on both
/// directions of the master, which the crate treats as end-of-stream rather
/// than a failure: it is the normal way for the master to learn the child side
/// has ended, and it can arrive before or after [`Session::try_wait()`]
/// observes the exit. Both the read and write phases classify through this one
/// predicate so the two directions cannot drift.
fn is_pty_gone(err: &io::Error) -> bool {
    err.raw_os_error() == Some(libc::EIO)
}

/// Length of the intersection between unsent outbound bytes and a reply range.
fn pending_reply_unsent_len(write_offset: usize, range: Option<&Range<usize>>) -> usize {
    match range {
        Some(range) => range.end.saturating_sub(write_offset.max(range.start)),
        None => 0,
    }
}

#[cfg(test)]
mod tests {
    use super::{is_pty_gone, pending_reply_unsent_len};
    use std::{
        io::{Error, ErrorKind},
        ops::Range,
    };

    #[test]
    fn pending_reply_counts_only_intersection_with_unsent() {
        let range: Range<usize> = 100..105;
        assert_eq!(pending_reply_unsent_len(0, Some(&range)), 5);
        assert_eq!(pending_reply_unsent_len(100, Some(&range)), 5);
        assert_eq!(pending_reply_unsent_len(102, Some(&range)), 3);
        assert_eq!(pending_reply_unsent_len(105, Some(&range)), 0);
        assert_eq!(pending_reply_unsent_len(0, None), 0);
    }

    #[test]
    fn pty_gone_only_matches_eio() {
        // Classification keys on `raw_os_error()`, not `ErrorKind`: `EIO`'s
        // `kind()` is not stable across toolchains (it may be
        // `Uncategorized`), so matching the OS error is what keeps both
        // directions agreeing.
        assert!(is_pty_gone(&Error::from_raw_os_error(libc::EIO)));

        assert!(!is_pty_gone(&Error::from(ErrorKind::WouldBlock)));
        assert!(!is_pty_gone(&Error::from(ErrorKind::Interrupted)));
        assert!(!is_pty_gone(&Error::from(ErrorKind::BrokenPipe)));
        // A same-kind error without the OS code must not be treated as gone.
        assert!(!is_pty_gone(&Error::from(ErrorKind::ReadOnlyFilesystem)));
    }
}
