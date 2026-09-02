//! Single PTY-backed terminal session with runtime-free I/O.
//!
//! [`Session`] owns one PTY-backed terminal session and lets an external event
//! loop drive it: the caller registers the session's file descriptor with its
//! own poll loop, feeds readiness notifications back to the session, and reads
//! the resulting lifecycle events and poll source changes.
//!
//! The session never owns a poll loop. It exposes an opaque
//! [`RegistrationToken`] per registration so that a stale readiness
//! notification from an older registration can never be misapplied, even when
//! the OS reuses the same fd number. Owning, identifying, and scheduling
//! several sessions is the caller's responsibility; this module does not map
//! application-specific identifiers to sessions.

use std::{
    io::{self, ErrorKind, Read, Write},
    os::fd::{AsRawFd, RawFd},
    process::{Command, ExitStatus},
    sync::atomic::{AtomicU64, Ordering},
};

use crate::{
    input::{KeyEvent, encode_key, encode_paste},
    pty::{ClosingPtyProcess, ObservedExit, PtyProcess, SignalOutcome},
    size::Size,
    snapshot::TerminalSnapshot,
    terminal::{TerminalAction, TerminalState},
};

/// Maximum size of a single terminal reply the emulator can produce.
///
/// The emulator answers a CPR (cursor position report) request with
/// `ESC [ <row> ; <col> R`. Both numbers are at most five digits because the
/// grid is `u16` sized, so the longest reply is `ESC [ 65536 ; 65536 R`,
/// which is 14 bytes. Every configured queue limit must be able to hold at
/// least one full reply so that a reply is never silently dropped.
const MAX_REPLY_BYTES: usize = 14;

/// Opaque handle bound to one event-loop registration of a [`Session`].
///
/// A token pairs the session's unique instance id with its current
/// registration generation, so a token issued for one session can never be
/// accepted by a different session, and readiness reported with a stale token
/// (for example from before a registration update, or after the OS reused an
/// fd number for a new session) is rejected instead of being applied.
#[derive(Clone, Copy, PartialEq, Eq, Hash)]
pub struct RegistrationToken {
    instance: u64,
    generation: u64,
}

impl std::fmt::Debug for RegistrationToken {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RegistrationToken")
            .field("instance", &self.instance)
            .field("generation", &self.generation)
            .finish()
    }
}

/// Read/write interests an event loop should register for one fd.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub struct Interests {
    /// Whether readable events should be polled.
    pub readable: bool,
    /// Whether writable events should be polled.
    pub writable: bool,
}

/// Readiness flags observed on one fd and reported back to the session.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub struct Readiness {
    /// The fd became readable.
    pub readable: bool,
    /// The fd became writable.
    pub writable: bool,
    /// Hangup was observed (drain remaining output before treating as EOF).
    pub hangup: bool,
    /// An error condition was reported (drive a read/write to learn the cause).
    pub error: bool,
}

/// One registration the caller should apply to its event loop.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PollSourceEntry {
    /// File descriptor to register.
    pub fd: RawFd,
    /// Token to associate with this fd.
    pub token: RegistrationToken,
    /// Interests to register for this fd.
    pub interests: Interests,
}

/// Per-session options passed when creating a [`Session`].
///
/// These are resource limits for one session. The per-`drive` byte and syscall
/// budget is passed separately to [`Session::drive`] so the caller decides how
/// to allocate work between sessions.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SessionConfig {
    /// Initial terminal size. Rows and columns must both be at least 1.
    pub size: Size,
    /// Maximum bytes held in the outbound write queue.
    pub write_queue_limit: usize,
    /// Maximum unprocessed raw bytes held before decoding.
    pub read_buffer_limit: usize,
    /// Maximum reply bytes held while the outbound queue is full.
    pub pending_reply_limit: usize,
}

impl SessionConfig {
    /// Validates the configuration.
    ///
    /// Every limit must be non-zero, and the write queue and pending reply
    /// buffers must each be able to hold one full terminal reply (14 bytes)
    /// so that a reply produced by decoding is never dropped. Rows and
    /// columns must both be at least 1.
    pub fn validate(&self) -> io::Result<()> {
        let non_zero = |name: &str, value: usize| -> io::Result<()> {
            if value == 0 {
                Err(io::Error::new(
                    ErrorKind::InvalidInput,
                    format!("{name} must be non-zero"),
                ))
            } else {
                Ok(())
            }
        };
        non_zero("write_queue_limit", self.write_queue_limit)?;
        non_zero("read_buffer_limit", self.read_buffer_limit)?;
        non_zero("pending_reply_limit", self.pending_reply_limit)?;
        if self.size.rows.get() == 0 || self.size.cols.get() == 0 {
            return Err(io::Error::new(
                ErrorKind::InvalidInput,
                "size rows and cols must both be at least 1",
            ));
        }
        if self.write_queue_limit < MAX_REPLY_BYTES {
            return Err(io::Error::new(
                ErrorKind::InvalidInput,
                "write_queue_limit must be able to hold one full terminal reply",
            ));
        }
        if self.pending_reply_limit < MAX_REPLY_BYTES {
            return Err(io::Error::new(
                ErrorKind::InvalidInput,
                "pending_reply_limit must be able to hold one full terminal reply",
            ));
        }
        Ok(())
    }
}

impl Default for SessionConfig {
    /// Defaults tuned for a small interactive terminal session: a few hundred
    /// kilobytes of per-session memory at most. Raise the drive budget passed
    /// to [`Session::drive`] for throughput, or lower the queue and buffer
    /// limits to bound memory.
    fn default() -> Self {
        Self {
            size: Size::new(24, 80).expect("default size is non-zero"),
            write_queue_limit: 65536,
            read_buffer_limit: 65536,
            pending_reply_limit: 65536,
        }
    }
}

/// Byte and syscall bounds for one [`Session::drive`] call.
///
/// The caller chooses these values so it can allocate the per-drive budget
/// between its own sessions; the library does not impose a global budget.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DriveBudget {
    /// Maximum bytes processed (read + write + decode) during the drive.
    pub byte_quantum: usize,
    /// Maximum syscalls attempted during the drive.
    pub syscall_quantum: usize,
}

impl DriveBudget {
    /// Validates the budget. Both limits must be non-zero.
    pub fn validate(&self) -> io::Result<()> {
        let non_zero = |name: &str, value: usize| -> io::Result<()> {
            if value == 0 {
                Err(io::Error::new(
                    ErrorKind::InvalidInput,
                    format!("{name} must be non-zero"),
                ))
            } else {
                Ok(())
            }
        };
        non_zero("byte_quantum", self.byte_quantum)?;
        non_zero("syscall_quantum", self.syscall_quantum)
    }
}

impl Default for DriveBudget {
    fn default() -> Self {
        Self {
            byte_quantum: 65536,
            syscall_quantum: 64,
        }
    }
}

/// Result of one [`Session::drive`] call.
#[derive(Debug, Clone, Default)]
pub struct DriveResult {
    /// Whether the session still has ready work that a following drive should
    /// process before the caller blocks in its poll loop.
    pub has_pending_work: bool,
    /// Whether the poll source entry should be re-read (for example because
    /// an interest changed or the registration disappeared).
    pub poll_source_changed: bool,
    /// Lifecycle events produced during this drive.
    pub events: Vec<SessionEvent>,
    /// Tokens that were rejected because they no longer match the session's
    /// current registration.
    pub stale_tokens: Vec<RegistrationToken>,
}

/// A lifecycle event produced by a session.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SessionEvent {
    /// A child exit was observed without reaping the child.
    ///
    /// This event is emitted at most once per session, from
    /// [`Session::poll_process`]. The child is still owned by the session
    /// until [`Session::reap`] is called.
    SessionExited {
        /// The observed exit classification.
        exit: ObservedExit,
    },
}

/// Errors returned by a session.
#[derive(Debug)]
pub enum SessionError {
    /// The session was logically closed and no longer accepts this operation.
    SessionClosed,
    /// The direct child has already been reaped.
    Reaped,
    /// The child exit has not been observed yet; poll processes first.
    NotExited,
    /// The size has a zero row or column count.
    InvalidSize(Size),
    /// The enqueue would exceed the session's queue limit and was rejected
    /// without modifying the queue.
    Backpressure,
    /// The terminal reply exceeds the configured pending reply limit.
    ReplyOverflow,
    /// A configuration value is invalid.
    InvalidConfig(String),
    /// An I/O syscall failed.
    Io(io::Error),
}

impl std::fmt::Display for SessionError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::SessionClosed => write!(f, "session is closed"),
            Self::Reaped => write!(f, "session child has been reaped"),
            Self::NotExited => write!(f, "session child has not exited"),
            Self::InvalidSize(size) => write!(f, "invalid size {size:?}"),
            Self::Backpressure => write!(f, "write queue backpressure"),
            Self::ReplyOverflow => write!(f, "terminal reply exceeds configured limits"),
            Self::InvalidConfig(msg) => write!(f, "invalid session config: {msg}"),
            Self::Io(err) => write!(f, "io error: {err}"),
        }
    }
}

impl std::error::Error for SessionError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Io(err) => Some(err),
            _ => None,
        }
    }
}

impl From<io::Error> for SessionError {
    fn from(err: io::Error) -> Self {
        Self::Io(err)
    }
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
    /// Direct child reaped; the session is spent.
    Reaped,
}

/// Direction cursor used for read/write fairness within one session.
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
/// The session owns the PTY master fd, the emulator state, the bounded
/// read/write buffers, and the child-process lifecycle, but never owns a poll
/// loop. Call [`Session::poll_source`] to learn what to register, report
/// readiness back with [`Session::drive`], and call [`Session::poll_process`]
/// on a timer or `SIGCHLD` to observe child exits without reaping.
///
/// The caller owns any collection of sessions and decides processing order and
/// per-session drive budgets.
///
/// # Drop behavior
///
/// Dropping the session performs best-effort cleanup: the remaining child is
/// force-killed and reaped, which may block. Failures cannot be reported from
/// `Drop`; call [`Session::shutdown`] to observe the result.
pub struct Session {
    /// Unique per-process identifier so a token issued for one session can
    /// never be accepted by another, even when the OS reuses the same fd.
    instance: u64,
    generation: u64,
    pty: Option<Pty>,
    term: TerminalState,
    phase: Phase,
    /// Raw bytes read from the PTY but not yet decoded.
    read_buffer: Vec<u8>,
    /// Bytes waiting to be written, in chronological order.
    outbound: Vec<u8>,
    /// Number of leading `outbound` bytes already written.
    write_offset: usize,
    /// Reply bytes held while `outbound` is full.
    pending_reply: Vec<u8>,
    /// Observed readiness not yet consumed.
    ready: Readiness,
    /// Direction the next drive phase should start with.
    direction: Direction,
    /// Whether decoding is paused because a reply could not be admitted.
    read_paused: bool,
    /// Whether the exit event for this session has been emitted.
    exit_event_sent: bool,
    /// Whether the poll source changed since the last snapshot was taken.
    poll_dirty: bool,
    write_queue_limit: usize,
    pending_reply_limit: usize,
    read_buffer_limit: usize,
}

impl Session {
    /// Spawns `command` in a new PTY-backed terminal session.
    ///
    /// The configuration is validated first; on validation or spawn failure no
    /// session is created and any opened PTY fd and child are reclaimed.
    pub fn new(command: &mut Command, config: SessionConfig) -> Result<Self, SessionError> {
        config
            .validate()
            .map_err(|err| SessionError::InvalidConfig(err.to_string()))?;
        let mut pty = PtyProcess::spawn(command, config.size)?;
        if let Err(err) = pty.set_nonblocking(true) {
            // Reclaim the fd and the child before reporting the failure.
            let mut closing = pty.into_closing();
            let _ = closing.signal_kill();
            let _ = closing.wait_and_reap();
            return Err(SessionError::Io(err));
        }
        let term = TerminalState::new(config.size);
        Ok(Self {
            instance: next_instance(),
            generation: 0,
            pty: Some(Pty::Live(pty)),
            term,
            phase: Phase::Live,
            read_buffer: Vec::new(),
            outbound: Vec::new(),
            write_offset: 0,
            pending_reply: Vec::new(),
            ready: Readiness::default(),
            direction: Direction::Write,
            read_paused: false,
            exit_event_sent: false,
            poll_dirty: true,
            write_queue_limit: config.write_queue_limit,
            pending_reply_limit: config.pending_reply_limit,
            read_buffer_limit: config.read_buffer_limit,
        })
    }

    /// Returns the registration the caller should apply to its event loop.
    ///
    /// After a logical close or reap there is no fd to register, so `None` is
    /// returned. Callers replace their whole registration with this entry on
    /// every poll iteration.
    pub fn poll_source(&mut self) -> Option<PollSourceEntry> {
        if self.phase == Phase::Closing || self.phase == Phase::Reaped {
            return None;
        }
        let fd = self.pty.as_ref().and_then(Pty::fd)?;
        self.poll_dirty = false;
        Some(PollSourceEntry {
            fd,
            token: RegistrationToken {
                instance: self.instance,
                generation: self.generation,
            },
            interests: self.interests(),
        })
    }

    /// Records readiness and drives bounded work for this session.
    ///
    /// `readiness` reports poll events observed on the session's fd. A token
    /// that does not match the session's current generation is rejected and
    /// returned in [`DriveResult::stale_tokens`] without touching any state.
    ///
    /// Ready work is processed with alternating read and write phases within
    /// `budget`. When a direction hits `EAGAIN`/`WouldBlock` only that
    /// direction's readiness is cleared; a budget limit instead keeps the
    /// ready state for the next drive.
    pub fn drive(
        &mut self,
        readiness: Option<(RegistrationToken, Readiness)>,
        budget: DriveBudget,
    ) -> Result<DriveResult, SessionError> {
        budget
            .validate()
            .map_err(|err| SessionError::InvalidConfig(err.to_string()))?;
        let mut stale_tokens = Vec::new();
        if let Some((token, readiness)) = readiness {
            if token.instance == self.instance
                && token.generation == self.generation
                && self.phase != Phase::Closing
                && self.phase != Phase::Reaped
                && self.pty.as_ref().is_some_and(Pty::is_live)
            {
                self.ready.readable |= readiness.readable || readiness.hangup || readiness.error;
                self.ready.writable |= readiness.writable;
            } else {
                stale_tokens.push(token);
            }
        }

        let mut budget = Budget {
            bytes: budget.byte_quantum,
            syscalls: budget.syscall_quantum,
        };
        self.process(&mut budget)?;

        Ok(DriveResult {
            has_pending_work: self.has_work(),
            poll_source_changed: std::mem::take(&mut self.poll_dirty),
            events: Vec::new(),
            stale_tokens,
        })
    }

    /// Observes whether the direct child has exited without reaping it.
    ///
    /// Call this on a `SIGCHLD` notification or a timer. The exit event is
    /// returned at most once; after a successful [`Session::reap`] this
    /// returns `Ok(None)`.
    pub fn poll_process(&mut self) -> Result<Option<SessionEvent>, SessionError> {
        if self.phase == Phase::Reaped || self.exit_event_sent {
            return Ok(None);
        }
        let exit = match self.pty.as_mut() {
            Some(Pty::Live(pty)) => pty.observe_exit()?,
            Some(Pty::Closing(pty)) => pty.poll_exit()?,
            None => return Ok(None),
        };
        if let Some(exit) = exit {
            self.exit_event_sent = true;
            Ok(Some(SessionEvent::SessionExited { exit }))
        } else {
            Ok(None)
        }
    }

    /// Reaps the direct child.
    ///
    /// The child exit must already be observable (for example through
    /// [`Session::poll_process`]); otherwise [`SessionError::NotExited`] is
    /// returned and the session is left intact. After a successful reap the
    /// session enters the `Reaped` status and its registration token is no
    /// longer valid.
    pub fn reap(&mut self) -> Result<ExitStatus, SessionError> {
        if self.phase == Phase::Reaped {
            return Err(SessionError::Reaped);
        }
        let status = match self.pty.as_mut() {
            Some(Pty::Live(pty)) => match pty.try_wait()? {
                Some(status) => status,
                None => return Err(SessionError::NotExited),
            },
            Some(Pty::Closing(pty)) => {
                pty.poll_exit()?.ok_or(SessionError::NotExited)?;
                pty.wait_and_reap()?
            }
            None => return Err(SessionError::Reaped),
        };
        self.phase = Phase::Reaped;
        self.poll_dirty = true;
        Ok(status)
    }

    /// Logically closes the session.
    ///
    /// This synchronously invalidates the registration token, requests
    /// unregistration by removing the poll source entry, discards the write
    /// queue, pending reply and unread bytes, and closes the PTY master. The
    /// child is kept in a private closing state until reaped. After closing,
    /// new input, resize and read/decode are rejected, while the final
    /// terminal state, snapshots, process polling and signal delivery remain
    /// available until the child is reaped.
    pub fn close(&mut self) -> Result<(), SessionError> {
        if self.phase == Phase::Reaped {
            return Err(SessionError::Reaped);
        }
        if self.phase != Phase::Closing {
            self.generation += 1;
            self.outbound.clear();
            self.write_offset = 0;
            self.pending_reply.clear();
            self.read_buffer.clear();
            self.ready = Readiness::default();
            self.read_paused = false;
            self.phase = Phase::Closing;
            let pty = self.pty.take();
            self.pty = Some(match pty {
                Some(Pty::Live(pty)) => Pty::Closing(pty.into_closing()),
                other => other.expect("pty is present while closing"),
            });
            self.poll_dirty = true;
        }
        Ok(())
    }

    /// Resizes the session, updating both the kernel PTY size and the emulator.
    ///
    /// A size with a zero row or column count, or a closed session returns an
    /// error without side effects. Reapplying the current size succeeds
    /// without a syscall. The ioctl runs first; the emulator size is only
    /// updated when the ioctl succeeds.
    pub fn resize(&mut self, size: Size) -> Result<(), SessionError> {
        if size.rows.get() == 0 || size.cols.get() == 0 {
            return Err(SessionError::InvalidSize(size));
        }
        if self.phase != Phase::Live && self.phase != Phase::Eof {
            return Err(SessionError::SessionClosed);
        }
        if self.term.size() == size {
            return Ok(());
        }
        let Some(Pty::Live(pty)) = self.pty.as_mut() else {
            return Err(SessionError::SessionClosed);
        };
        pty.resize(size)?;
        self.term.resize(size);
        Ok(())
    }

    /// Enqueues a logical key, encoded for the session's current modes.
    pub fn enqueue_key(&mut self, event: KeyEvent) -> Result<(), SessionError> {
        // Logical key encodings are at most a few bytes; check capacity before
        // encoding.
        self.ensure_capacity(8)?;
        let modes = self.term.modes();
        let bytes = encode_key(event, modes);
        self.append_bytes(&bytes)
    }

    /// Enqueues UTF-8 text verbatim.
    pub fn enqueue_text(&mut self, text: &str) -> Result<(), SessionError> {
        self.append_bytes(text.as_bytes())
    }

    /// Enqueues a paste, wrapped in bracketed-paste markers when the terminal
    /// mode is active.
    pub fn enqueue_paste(&mut self, text: &str) -> Result<(), SessionError> {
        // Bracketed-paste markers add at most 12 bytes; check capacity before
        // encoding so an oversized paste never allocates first.
        let upper = text.len().saturating_add(12);
        self.ensure_capacity(upper)?;
        let modes = self.term.modes();
        let bytes = encode_paste(text, modes);
        self.append_bytes(&bytes)
    }

    /// Enqueues raw bytes without validation or mode interpretation.
    pub fn enqueue_raw(&mut self, bytes: &[u8]) -> Result<(), SessionError> {
        self.append_bytes(bytes)
    }

    /// Returns an owned snapshot of the session's terminal state.
    ///
    /// Available while the session is live, at EOF, or logically closed, until
    /// the child is reaped.
    pub fn snapshot(&self) -> Result<TerminalSnapshot, SessionError> {
        if self.phase == Phase::Reaped {
            return Err(SessionError::Reaped);
        }
        Ok(self.term.snapshot())
    }

    /// Returns read-only access to the session's terminal state.
    ///
    /// Available until the child is reaped.
    pub fn terminal_state(&self) -> Result<&TerminalState, SessionError> {
        if self.phase == Phase::Reaped {
            return Err(SessionError::Reaped);
        }
        Ok(&self.term)
    }

    /// Returns the observable lifecycle phase of the session.
    pub fn session_status(&self) -> SessionStatus {
        match self.phase {
            Phase::Live => SessionStatus::Live,
            Phase::Eof => SessionStatus::Eof,
            Phase::Closing => SessionStatus::Closing,
            Phase::Reaped => SessionStatus::Reaped,
        }
    }

    /// Sends a graceful termination signal (SIGTERM) to the session's process
    /// group.
    pub fn terminate(&mut self) -> Result<SignalOutcome, SessionError> {
        match self.pty.as_mut() {
            Some(Pty::Live(pty)) => Ok(pty.signal_group(libc::SIGTERM)?),
            Some(Pty::Closing(pty)) => Ok(pty.signal_terminate()?),
            None => Err(SessionError::Reaped),
        }
    }

    /// Sends a force-termination signal (SIGKILL) to the session's process
    /// group.
    pub fn force_terminate(&mut self) -> Result<SignalOutcome, SessionError> {
        match self.pty.as_mut() {
            Some(Pty::Live(pty)) => Ok(pty.signal_group(libc::SIGKILL)?),
            Some(Pty::Closing(pty)) => Ok(pty.signal_kill()?),
            None => Err(SessionError::Reaped),
        }
    }

    /// Cleans up this session, force-killing and reaping the direct child.
    ///
    /// This consumes the session and may block while reaping.
    pub fn shutdown(mut self) -> io::Result<ExitStatus> {
        let mut closing = match self.pty.take() {
            Some(Pty::Live(pty)) => pty.into_closing(),
            Some(Pty::Closing(closing)) => closing,
            None => return Err(io::Error::other("session already reaped")),
        };
        let _ = closing.signal_kill();
        closing.wait_and_reap()
    }

    fn ensure_capacity(&self, len: usize) -> Result<(), SessionError> {
        if self.phase != Phase::Live && self.phase != Phase::Eof {
            return Err(SessionError::SessionClosed);
        }
        if !capacity_ok(
            self.unsent(),
            self.pending_reply.len(),
            self.write_queue_limit,
            self.pending_reply_limit,
            len,
        ) {
            return Err(SessionError::Backpressure);
        }
        Ok(())
    }

    fn append_bytes(&mut self, bytes: &[u8]) -> Result<(), SessionError> {
        if self.phase != Phase::Live && self.phase != Phase::Eof {
            return Err(SessionError::SessionClosed);
        }
        if !capacity_ok(
            self.unsent(),
            self.pending_reply.len(),
            self.write_queue_limit,
            self.pending_reply_limit,
            bytes.len(),
        ) {
            return Err(SessionError::Backpressure);
        }
        if self.pending_reply.is_empty() {
            self.outbound.extend_from_slice(bytes);
        } else {
            self.pending_reply.extend_from_slice(bytes);
        }
        self.poll_dirty = true;
        Ok(())
    }

    /// Returns whether `outbound` holds any unsent bytes.
    fn outbound_empty(&self) -> bool {
        self.outbound.len() == self.write_offset
    }

    /// Returns the number of unsent bytes currently held.
    fn unsent(&self) -> usize {
        self.outbound.len() - self.write_offset
    }

    /// Returns how many more bytes `outbound` can accept.
    fn free_outbound(&self) -> usize {
        self.write_queue_limit.saturating_sub(self.unsent())
    }

    /// Whether the session still has work a following drive should process.
    ///
    /// Write work is reported only while writable readiness has been observed
    /// (or the pending reply can be re-admitted without any fd), and read work
    /// only while the session is not paused and not at EOF. This keeps
    /// [`DriveResult::has_pending_work`] false while the session is simply
    /// waiting for a new poll edge.
    fn has_work(&self) -> bool {
        if self.phase == Phase::Closing || self.phase == Phase::Reaped {
            return false;
        }
        if !self.pending_reply.is_empty() && self.pending_reply.len() <= self.free_outbound() {
            return true;
        }
        if !self.outbound_empty() {
            return self.ready.writable;
        }
        if self.read_paused {
            return false;
        }
        if !self.read_buffer.is_empty() {
            return true;
        }
        self.ready.readable
            && self.pty.as_ref().is_some_and(Pty::is_live)
            && self.phase != Phase::Eof
    }

    /// Interests the session's fd should currently be registered with.
    fn interests(&self) -> Interests {
        if self.phase == Phase::Closing || self.phase == Phase::Reaped {
            return Interests::default();
        }
        let mut interests = Interests::default();
        if self.phase != Phase::Eof
            && !self.read_paused
            && self.pty.as_ref().is_some_and(Pty::is_live)
        {
            interests.readable = true;
        }
        if !self.outbound_empty() || !self.pending_reply.is_empty() {
            interests.writable = true;
        }
        interests
    }

    /// Processes one session with alternating read/write phases until both
    /// directions are idle or the budgets are exhausted.
    fn process(&mut self, budget: &mut Budget) -> io::Result<()> {
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
                Direction::Write => self.write_phase(budget)?,
                Direction::Read => self.read_phase(budget)?,
            };
            if !progressed {
                break;
            }
            direction = direction.opposite();
            if !self.has_direction_work(direction) {
                break;
            }
        }
        self.direction = direction;
        Ok(())
    }

    /// Whether the given direction currently has work.
    fn has_direction_work(&self, direction: Direction) -> bool {
        match direction {
            Direction::Write => !self.outbound_empty() || !self.pending_reply.is_empty(),
            Direction::Read => {
                !self.read_paused
                    && self.phase != Phase::Closing
                    && self.phase != Phase::Reaped
                    && self.phase != Phase::Eof
                    && (!self.read_buffer.is_empty()
                        || (self.ready.readable && self.pty.as_ref().is_some_and(Pty::is_live)))
            }
        }
    }

    /// Writes pending bytes, bounded by the drive budget.
    fn write_phase(&mut self, budget: &mut Budget) -> io::Result<bool> {
        let mut progressed = false;
        if self.readmit_pending() {
            self.poll_dirty = true;
            progressed = true;
        }

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
            let Some(Pty::Live(pty)) = self.pty.as_mut() else {
                break;
            };
            let start = self.write_offset;
            let buf = self.outbound[start..start + cap].to_vec();
            let result = pty.write(&buf);
            budget.consume(0, 1);
            match result {
                Ok(0) => break,
                Ok(n) => {
                    self.write_offset += n;
                    budget.consume(n, 0);
                    if self.write_offset == self.outbound.len() {
                        self.outbound.clear();
                        self.write_offset = 0;
                        self.poll_dirty = true;
                    }
                    progressed = true;
                }
                Err(err) if err.kind() == ErrorKind::WouldBlock => {
                    self.ready.writable = false;
                    break;
                }
                Err(err) if err.kind() == ErrorKind::Interrupted => break,
                Err(err) => {
                    self.ready.writable = false;
                    return Err(err);
                }
            }
        }
        Ok(progressed)
    }

    /// Reads and decodes bytes, bounded by the drive budget.
    fn read_phase(&mut self, budget: &mut Budget) -> io::Result<bool> {
        let mut progressed = false;
        progressed |= self.decode_buffered(budget)? > 0;

        let should_read = self.phase == Phase::Live
            && self.ready.readable
            && self.pty.as_ref().is_some_and(Pty::is_live)
            && !self.read_paused;
        if !should_read {
            return Ok(progressed);
        }

        loop {
            if budget.exhausted() {
                break;
            }
            if self.read_paused || self.phase != Phase::Live {
                break;
            }
            let room = self
                .read_buffer_limit
                .saturating_sub(self.read_buffer.len());
            if room == 0 {
                break;
            }
            let cap = budget.bytes_left().min(room);
            if cap == 0 {
                break;
            }
            let Some(Pty::Live(pty)) = self.pty.as_mut() else {
                break;
            };
            let mut buf = vec![0u8; cap];
            let result = pty.read(&mut buf);
            budget.consume(0, 1);
            match result {
                Ok(0) => {
                    self.phase = Phase::Eof;
                    self.ready.readable = false;
                    self.poll_dirty = true;
                    break;
                }
                Ok(n) => {
                    self.read_buffer.extend_from_slice(&buf[..n]);
                    budget.consume(n, 0);
                    progressed = true;
                }
                Err(err) if err.kind() == ErrorKind::WouldBlock => {
                    self.ready.readable = false;
                    break;
                }
                Err(err) if err.kind() == ErrorKind::Interrupted => break,
                Err(err) if err.raw_os_error() == Some(libc::EIO) => {
                    // PTY slave close is reported as EIO on some platforms;
                    // normalize it to EOF.
                    self.phase = Phase::Eof;
                    self.ready.readable = false;
                    self.poll_dirty = true;
                    break;
                }
                Err(err) => {
                    self.ready.readable = false;
                    return Err(err);
                }
            }
            self.decode_buffered(budget)?;
            if self.read_paused {
                break;
            }
        }
        Ok(progressed)
    }

    /// Decodes already-buffered raw bytes, one byte at a time, until paused,
    /// empty, or the byte budget is exhausted. Returns the number of bytes
    /// decoded.
    fn decode_buffered(&mut self, budget: &mut Budget) -> io::Result<usize> {
        let mut decoded = 0;
        loop {
            if budget.bytes_left() == 0 {
                break;
            }
            if self.read_paused || self.read_buffer.is_empty() {
                break;
            }
            let byte = self.read_buffer.remove(0);
            budget.consume(1, 0);
            self.decode_byte(byte)?;
            decoded += 1;
        }
        if decoded > 0 {
            self.poll_dirty = true;
        }
        Ok(decoded)
    }

    /// Moves as much of the pending reply into `outbound` as fits.
    ///
    /// Returns whether anything was moved. When the whole pending reply is
    /// admitted, decoding resumes. Preceding (already accepted) bytes stay in
    /// front of the moved reply, and bytes accepted later are appended after
    /// it, preserving order.
    fn readmit_pending(&mut self) -> bool {
        if self.pending_reply.is_empty() {
            return false;
        }
        let can = self.free_outbound().min(self.pending_reply.len());
        if can == 0 {
            return false;
        }
        if self.write_offset > 0 {
            self.outbound.drain(..self.write_offset);
            self.write_offset = 0;
        }
        let head: Vec<u8> = self.pending_reply.drain(..can).collect();
        self.outbound.splice(0..0, head);
        if self.pending_reply.is_empty() {
            self.read_paused = false;
        }
        true
    }

    /// Feeds one byte into the emulator and admits any produced reply.
    ///
    /// A reply is appended to `outbound` when it fits; otherwise it is held in
    /// `pending_reply` and decoding pauses so that no reply is ever dropped.
    fn decode_byte(&mut self, byte: u8) -> io::Result<()> {
        self.term.feed(&[byte]);
        let actions = self.term.drain_actions();
        for action in actions {
            let TerminalAction::WritePty(reply) = action;
            if reply.len() <= self.free_outbound() {
                self.outbound.extend_from_slice(&reply);
            } else if self.pending_reply.len() + reply.len() <= self.pending_reply_limit {
                self.pending_reply.extend_from_slice(&reply);
                self.read_paused = true;
            } else {
                return Err(io::Error::new(
                    ErrorKind::InvalidData,
                    "terminal reply exceeds the configured pending reply limit",
                ));
            }
            if self.read_paused {
                break;
            }
        }
        Ok(())
    }
}

impl Drop for Session {
    fn drop(&mut self) {
        // Best-effort cleanup: force-kill and reap the remaining child. This
        // may block, and failures cannot be reported from `Drop`.
        let mut closing = match self.pty.take() {
            Some(Pty::Live(pty)) => pty.into_closing(),
            Some(Pty::Closing(closing)) => closing,
            None => return,
        };
        let _ = closing.signal_kill();
        let _ = closing.wait_and_reap();
    }
}

/// Admission decision for an enqueue of `len` bytes.
fn capacity_ok(
    unsent: usize,
    pending_len: usize,
    write_queue_limit: usize,
    pending_reply_limit: usize,
    len: usize,
) -> bool {
    if pending_len == 0 {
        len <= write_queue_limit.saturating_sub(unsent)
    } else {
        len <= pending_reply_limit.saturating_sub(pending_len)
    }
}

/// Per-drive byte and syscall budget accounting.
struct Budget {
    bytes: usize,
    syscalls: usize,
}

impl Budget {
    fn consume(&mut self, bytes: usize, syscalls: usize) {
        self.bytes = self.bytes.saturating_sub(bytes);
        self.syscalls = self.syscalls.saturating_sub(syscalls);
    }

    fn bytes_left(&self) -> usize {
        self.bytes
    }

    fn exhausted(&self) -> bool {
        self.bytes == 0 || self.syscalls == 0
    }
}

/// Allocates a unique per-process instance id for a new session.
///
/// Ids are allocated monotonically and never reused, so a stored token can
/// never be applied to a different session even when the OS reuses an fd
/// number.
fn next_instance() -> u64 {
    static NEXT_INSTANCE: AtomicU64 = AtomicU64::new(0);
    NEXT_INSTANCE.fetch_add(1, Ordering::Relaxed)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_session() -> Session {
        Session {
            instance: 0,
            generation: 0,
            pty: Some(Pty::Live(dummy_pty())),
            term: TerminalState::new(Size::new(1, 1).unwrap()),
            phase: Phase::Live,
            read_buffer: Vec::new(),
            outbound: Vec::new(),
            write_offset: 0,
            pending_reply: Vec::new(),
            ready: Readiness::default(),
            direction: Direction::Write,
            read_paused: false,
            exit_event_sent: false,
            poll_dirty: true,
            write_queue_limit: 64,
            pending_reply_limit: 64,
            read_buffer_limit: 64,
        }
    }

    /// A `/dev/null` fd and a throwaway child wrapped as a `PtyProcess`
    /// placeholder so pure state-transition tests can build a session without
    /// touching real PTY I/O.
    fn dummy_pty() -> PtyProcess {
        let master = std::fs::File::open("/dev/null").expect("open /dev/null");
        let child = std::process::Command::new("/bin/sh")
            .arg("-c")
            .arg("exit 0")
            .spawn()
            .expect("spawn placeholder");
        PtyProcess::from_parts(master, child)
    }

    #[test]
    fn default_config_is_valid() {
        SessionConfig::default()
            .validate()
            .expect("default is valid");
        DriveBudget::default().validate().expect("default is valid");
    }

    #[test]
    fn config_rejects_zero_limits() {
        let base = SessionConfig::default();
        for (name, value) in [
            ("write_queue_limit", base.write_queue_limit),
            ("read_buffer_limit", base.read_buffer_limit),
            ("pending_reply_limit", base.pending_reply_limit),
        ] {
            let mut config = base;
            match name {
                "write_queue_limit" => config.write_queue_limit = 0,
                "read_buffer_limit" => config.read_buffer_limit = 0,
                "pending_reply_limit" => config.pending_reply_limit = 0,
                _ => unreachable!(),
            }
            let err = config.validate().unwrap_err();
            assert!(
                err.to_string().contains(name),
                "expected {name} in error, got: {err}"
            );
            let _ = value;
        }
    }

    #[test]
    fn config_rejects_queue_limits_smaller_than_one_reply() {
        let config = SessionConfig {
            write_queue_limit: MAX_REPLY_BYTES - 1,
            ..SessionConfig::default()
        };
        assert!(config.validate().is_err());
        let config = SessionConfig {
            pending_reply_limit: MAX_REPLY_BYTES - 1,
            ..SessionConfig::default()
        };
        assert!(config.validate().is_err());
        let config = SessionConfig {
            write_queue_limit: MAX_REPLY_BYTES,
            pending_reply_limit: MAX_REPLY_BYTES,
            ..SessionConfig::default()
        };
        config.validate().expect("exactly one reply is allowed");
    }

    #[test]
    fn drive_budget_rejects_zero() {
        let budget = DriveBudget {
            byte_quantum: 0,
            ..DriveBudget::default()
        };
        assert!(budget.validate().is_err());
        let budget = DriveBudget {
            syscall_quantum: 0,
            ..DriveBudget::default()
        };
        assert!(budget.validate().is_err());
    }

    #[test]
    fn capacity_check_is_all_or_nothing() {
        // Pending reply absent: bounded by the write queue.
        assert!(capacity_ok(5, 0, 10, 10, 5));
        assert!(!capacity_ok(6, 0, 10, 10, 5));
        // Pending reply present: bounded by the pending reply buffer.
        assert!(capacity_ok(0, 5, 10, 10, 5));
        assert!(!capacity_ok(0, 6, 10, 10, 5));
        // Overflow-safe subtraction.
        assert!(!capacity_ok(usize::MAX, 0, 10, 10, 1));
    }

    #[test]
    fn readmit_pending_preserves_order() {
        let mut sess = test_session();
        sess.outbound = b"accepted-before".to_vec();
        sess.pending_reply = b"REPLY".to_vec();
        sess.read_paused = true;
        sess.write_queue_limit = 64;

        assert!(sess.readmit_pending());
        assert!(sess.pending_reply.is_empty());
        assert!(!sess.read_paused);
        // The pending reply is moved to the front, before accepted bytes.
        assert_eq!(sess.outbound, b"REPLYaccepted-before");
    }

    #[test]
    fn readmit_pending_waits_when_outbound_is_full() {
        let mut sess = test_session();
        sess.write_queue_limit = 4;
        sess.outbound = b"abcd".to_vec();
        sess.pending_reply = b"REPLY".to_vec();
        sess.read_paused = true;

        assert!(!sess.readmit_pending());
        assert_eq!(sess.pending_reply, b"REPLY");
        assert!(sess.read_paused);
    }

    #[test]
    fn decode_byte_holds_reply_when_outbound_is_full_and_resumes() {
        let mut sess = test_session();
        sess.write_queue_limit = 8;
        sess.outbound = b"abcd".to_vec();
        sess.pending_reply_limit = 8;
        // Feed the full primary DA sequence `ESC [ c`.
        for byte in [0x1b, b'[', b'c'] {
            sess.decode_byte(byte).expect("decode");
        }
        // The 5-byte reply does not fit in the 4 free bytes and is held.
        assert_eq!(sess.pending_reply, b"\x1b[?6c");
        assert!(sess.read_paused);
        // Once the outbound queue drains, the reply is re-admitted first and
        // decoding resumes.
        sess.outbound.clear();
        assert!(sess.readmit_pending());
        assert!(sess.pending_reply.is_empty());
        assert!(!sess.read_paused);
        assert_eq!(sess.outbound, b"\x1b[?6c");
    }

    #[test]
    fn interests_reflect_pending_state() {
        let mut sess = test_session();
        let interests = sess.interests();
        assert!(interests.readable);
        assert!(!interests.writable);

        sess.outbound = b"x".to_vec();
        assert!(sess.interests().writable);

        sess.read_paused = true;
        let interests = sess.interests();
        assert!(!interests.readable);
        assert!(interests.writable);

        sess.phase = Phase::Eof;
        sess.read_paused = false;
        let interests = sess.interests();
        assert!(!interests.readable);
        assert!(interests.writable);

        sess.phase = Phase::Closing;
        assert_eq!(
            sess.interests(),
            Interests {
                readable: false,
                writable: false
            }
        );
    }

    #[test]
    fn poll_source_is_none_after_close_and_reap() {
        let mut sess = test_session();
        assert!(sess.poll_source().is_some());
        sess.close().expect("close");
        assert!(sess.poll_source().is_none());
        assert_eq!(sess.session_status(), SessionStatus::Closing);
    }

    #[test]
    fn stale_token_is_rejected_without_touching_state() {
        let mut sess = test_session();
        let token = sess.poll_source().expect("source").token;
        // A registration update (for example after a close/reopen) changes
        // the generation, so the old token no longer matches.
        sess.generation += 1;
        let result = sess
            .drive(
                Some((
                    token,
                    Readiness {
                        readable: true,
                        writable: false,
                        hangup: false,
                        error: false,
                    },
                )),
                DriveBudget::default(),
            )
            .expect("drive");
        assert!(
            result.stale_tokens.contains(&token),
            "stale token should be reported"
        );
        // The readiness was not applied.
        assert!(!sess.ready.readable);
    }

    #[test]
    fn closed_session_rejects_enqueue_and_resize() {
        let mut sess = test_session();
        sess.close().expect("close");
        assert!(matches!(
            sess.enqueue_text("x"),
            Err(SessionError::SessionClosed)
        ));
        assert!(matches!(
            sess.resize(Size::new(2, 2).unwrap()),
            Err(SessionError::SessionClosed)
        ));
    }
}
