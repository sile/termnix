//! Multi-session terminal driver with runtime-free I/O.
//!
//! [`SessionDriver`] owns several PTY-backed terminal sessions and lets an
//! external event loop drive them: the caller registers file descriptors with
//! their own poll loop, feeds readiness notifications back to the driver, and
//! reads the resulting lifecycle events and poll source changes.
//!
//! The driver never owns a poll loop. It exposes an opaque
//! [`RegistrationToken`] per registration so that a stale readiness
//! notification from a removed session can never be misapplied to a new
//! session, even when the OS reuses the same fd number.

use std::{
    collections::BTreeMap,
    io::{self, ErrorKind, Read, Write},
    os::fd::{AsRawFd, RawFd},
    process::{Command, ExitStatus},
};

use crate::{
    input::{KeyEvent, encode_key, encode_paste},
    pty::{ClosingPtyProcess, ObservedExit, PtyProcess, SignalOutcome},
    size::Size,
    snapshot::TerminalSnapshot,
    terminal::{TerminalAction, TerminalModes, TerminalState},
    terminal_scrollback::ScrollbackLimits,
};

/// Maximum size of a single terminal reply the emulator can produce.
///
/// The emulator answers a CPR (cursor position report) request with
/// `ESC [ <row> ; <col> R`. Both numbers are at most five digits because the
/// grid is `u16` sized, so the longest reply is `ESC [ 65536 ; 65536 R`,
/// which is 14 bytes. Every configured queue limit must be able to hold at
/// least one full reply so that a reply is never silently dropped.
const MAX_REPLY_BYTES: usize = 14;

/// Stable identifier for one terminal session within the driver.
///
/// Identifiers are allocated monotonically and are never reused after a
/// session is reaped, so a stored [`SessionId`] can be used to detect whether
/// a handle still refers to a live session.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct SessionId(u64);

impl std::fmt::Debug for SessionId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_tuple("SessionId").field(&self.0).finish()
    }
}

/// Opaque handle bound to one event-loop registration of a session.
///
/// A token pairs the [`SessionId`] with the session's current registration
/// generation. Readiness reported with a stale token is rejected instead of
/// being applied to a different session, and a reused fd number can never be
/// resolved to the wrong session.
#[derive(Clone, Copy, PartialEq, Eq, Hash)]
pub struct RegistrationToken {
    id: SessionId,
    generation: u64,
}

impl std::fmt::Debug for RegistrationToken {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RegistrationToken")
            .field("id", &self.id)
            .field("generation", &self.generation)
            .finish()
    }
}

impl RegistrationToken {
    /// Returns the session this token was issued for.
    pub fn session_id(&self) -> SessionId {
        self.id
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

/// Readiness flags observed on one fd and reported back to the driver.
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

/// Snapshot of everything currently registered with the event loop.
///
/// The caller replaces its whole registration set with the entries in this
/// snapshot on every poll iteration, so a missed registration delta can always
/// be recovered.
#[derive(Debug, Clone, Default)]
pub struct PollSource {
    entries: Vec<PollSourceEntry>,
}

impl PollSource {
    /// Returns the entries to register.
    pub fn entries(&self) -> &[PollSourceEntry] {
        &self.entries
    }

    /// Returns whether no fd is currently registered.
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }
}

/// Limits that bound one `drive` call and one session's I/O.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DriverConfig {
    /// Maximum bytes read from one session per read phase.
    pub read_byte_quantum: usize,
    /// Maximum read syscalls attempted for one session per drive.
    pub read_syscall_quantum: usize,
    /// Maximum bytes written to one session per write phase.
    pub write_byte_quantum: usize,
    /// Maximum write syscalls attempted for one session per drive.
    pub write_syscall_quantum: usize,
    /// Maximum bytes processed (read + write + decode) per whole drive.
    pub drive_byte_budget: usize,
    /// Maximum syscalls attempted per whole drive.
    pub drive_syscall_budget: usize,
    /// Maximum bytes held in the outbound write queue of one session.
    pub write_queue_limit: usize,
    /// Maximum unprocessed raw bytes held per session.
    pub read_buffer_limit: usize,
    /// Maximum reply bytes held while the outbound queue is full.
    pub pending_reply_limit: usize,
}

impl DriverConfig {
    /// Validates the configuration.
    ///
    /// Every limit must be non-zero, and the write queue and pending reply
    /// buffers must each be able to hold one full terminal reply (14 bytes)
    /// so that a reply produced by decoding is never dropped.
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
        non_zero("read_byte_quantum", self.read_byte_quantum)?;
        non_zero("read_syscall_quantum", self.read_syscall_quantum)?;
        non_zero("write_byte_quantum", self.write_byte_quantum)?;
        non_zero("write_syscall_quantum", self.write_syscall_quantum)?;
        non_zero("drive_byte_budget", self.drive_byte_budget)?;
        non_zero("drive_syscall_budget", self.drive_syscall_budget)?;
        non_zero("write_queue_limit", self.write_queue_limit)?;
        non_zero("read_buffer_limit", self.read_buffer_limit)?;
        non_zero("pending_reply_limit", self.pending_reply_limit)?;
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

impl Default for DriverConfig {
    /// Defaults tuned for a small interactive multiplexer: a few sessions,
    /// sub-millisecond per-drive latency, and a few hundred kilobytes of
    /// per-session memory at most. Raise `drive_*_budget` for throughput, or
    /// lower the queue and buffer limits to bound memory.
    fn default() -> Self {
        Self {
            read_byte_quantum: 4096,
            read_syscall_quantum: 4,
            write_byte_quantum: 4096,
            write_syscall_quantum: 4,
            drive_byte_budget: 65536,
            drive_syscall_budget: 64,
            write_queue_limit: 65536,
            read_buffer_limit: 65536,
            pending_reply_limit: 65536,
        }
    }
}

/// Per-session options passed when creating a session.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SessionConfig {
    /// Initial terminal size. Rows and columns must both be at least 1.
    pub size: Size,
    /// Scrollback bounds for the session's emulator.
    pub scrollback_limits: ScrollbackLimits,
}

/// Result of one [`SessionDriver::drive`] call.
#[derive(Debug, Clone, Default)]
pub struct DriveResult {
    /// Whether the driver still has ready work that a following drive should
    /// process before the caller blocks in its poll loop.
    pub has_pending_work: bool,
    /// Whether the poll source snapshot should be re-read (for example
    /// because an interest or a registration changed).
    pub poll_source_changed: bool,
    /// Sessions whose I/O was processed during this drive (bounded by the
    /// drive budgets).
    pub processed: Vec<SessionId>,
    /// Lifecycle events produced during this drive (bounded by the number of
    /// sessions processed).
    pub events: Vec<DriverEvent>,
    /// Tokens that were rejected because they no longer match a live session.
    pub stale_tokens: Vec<RegistrationToken>,
}

/// A lifecycle event produced by the driver.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DriverEvent {
    /// A child exit was observed without reaping the child.
    ///
    /// This event is emitted at most once per session, from
    /// [`SessionDriver::poll_processes`]. The child is still owned by the
    /// driver until [`SessionDriver::reap_session`] is called.
    SessionExited {
        /// The session whose child exited.
        id: SessionId,
        /// The observed exit classification.
        exit: ObservedExit,
    },
}

/// Errors returned by the driver.
#[derive(Debug)]
pub enum DriverError {
    /// The session id does not refer to a live session (never existed, was
    /// already reaped, or is stale).
    StaleId(SessionId),
    /// The session was logically closed and no longer accepts this operation.
    SessionClosed(SessionId),
    /// The child exit has not been observed yet; poll processes first.
    NotExited(SessionId),
    /// The size has a zero row or column count.
    InvalidSize(Size),
    /// The enqueue would exceed the session's queue limit and was rejected
    /// without modifying the queue.
    Backpressure,
    /// Identifier space exhausted; no new session can be created.
    IdExhausted,
    /// The configured limits cannot hold a terminal reply.
    ReplyOverflow,
    /// A configuration value is invalid.
    InvalidConfig(String),
    /// An I/O syscall failed.
    Io(io::Error),
}

impl std::fmt::Display for DriverError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::StaleId(id) => write!(f, "stale session id {id:?}"),
            Self::SessionClosed(id) => write!(f, "session {id:?} is closed"),
            Self::NotExited(id) => write!(f, "session {id:?} child has not exited"),
            Self::InvalidSize(size) => write!(f, "invalid size {size:?}"),
            Self::Backpressure => write!(f, "write queue backpressure"),
            Self::IdExhausted => write!(f, "session id space exhausted"),
            Self::ReplyOverflow => write!(f, "terminal reply exceeds configured limits"),
            Self::InvalidConfig(msg) => write!(f, "invalid driver config: {msg}"),
            Self::Io(err) => write!(f, "io error: {err}"),
        }
    }
}

impl std::error::Error for DriverError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Io(err) => Some(err),
            _ => None,
        }
    }
}

impl From<io::Error> for DriverError {
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
}

/// Result of [`SessionDriver::shutdown`], one entry per session.
#[derive(Debug, Default)]
pub struct ShutdownOutcome {
    /// Per-session reap results. Sessions are listed in creation order.
    pub sessions: Vec<(SessionId, io::Result<ExitStatus>)>,
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

/// One private terminal session owned by the driver.
struct Session {
    id: SessionId,
    generation: u64,
    pty: Pty,
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
    // Per-session copies of the driver config so the session can be reasoned
    // about without borrowing the driver.
    write_queue_limit: usize,
    pending_reply_limit: usize,
    read_buffer_limit: usize,
    read_byte_quantum: usize,
    read_syscall_quantum: usize,
    write_byte_quantum: usize,
    write_syscall_quantum: usize,
}

impl Session {
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
}

/// Whether a session still has work a following drive should process.
///
/// Write work is reported only while writable readiness has been observed (or
/// the pending reply can be re-admitted without any fd), and read work only
/// while the session is not paused and not at EOF. This keeps
/// [`DriveResult::has_pending_work`] false while the driver is simply waiting
/// for a new poll edge.
fn session_has_work(sess: &Session) -> bool {
    if sess.phase == Phase::Closing {
        return false;
    }
    if !sess.pending_reply.is_empty() && sess.pending_reply.len() <= sess.free_outbound() {
        return true;
    }
    if !sess.outbound_empty() {
        return sess.ready.writable;
    }
    if sess.read_paused {
        return false;
    }
    if !sess.read_buffer.is_empty() {
        return true;
    }
    sess.ready.readable && sess.pty.is_live() && sess.phase != Phase::Eof
}

/// Interests a session's fd should currently be registered with.
fn session_interests(sess: &Session) -> Interests {
    if sess.phase == Phase::Closing {
        return Interests::default();
    }
    let mut interests = Interests::default();
    if sess.phase != Phase::Closing
        && sess.phase != Phase::Eof
        && !sess.read_paused
        && sess.pty.is_live()
    {
        interests.readable = true;
    }
    if !sess.outbound_empty() || !sess.pending_reply.is_empty() {
        interests.writable = true;
    }
    interests
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

/// Moves as much of the pending reply into `outbound` as fits.
///
/// Returns whether anything was moved. When the whole pending reply is
/// admitted, decoding resumes. Preceding (already accepted) bytes stay in
/// front of the moved reply, and bytes accepted later are appended after it,
/// preserving order.
fn readmit_pending(sess: &mut Session) -> bool {
    if sess.pending_reply.is_empty() {
        return false;
    }
    let can = sess.free_outbound().min(sess.pending_reply.len());
    if can == 0 {
        return false;
    }
    if sess.write_offset > 0 {
        sess.outbound.drain(..sess.write_offset);
        sess.write_offset = 0;
    }
    let head: Vec<u8> = sess.pending_reply.drain(..can).collect();
    sess.outbound.splice(0..0, head);
    if sess.pending_reply.is_empty() {
        sess.read_paused = false;
    }
    true
}

/// Feeds one byte into the emulator and admits any produced reply.
///
/// A reply is appended to `outbound` when it fits; otherwise it is held in
/// `pending_reply` and decoding pauses so that no reply is ever dropped.
fn decode_byte(sess: &mut Session, byte: u8) -> io::Result<()> {
    sess.term.feed(&[byte]);
    let actions = sess.term.drain_actions();
    for action in actions {
        let TerminalAction::WritePty(reply) = action;
        if reply.len() <= sess.free_outbound() {
            sess.outbound.extend_from_slice(&reply);
        } else if sess.pending_reply.len() + reply.len() <= sess.pending_reply_limit {
            sess.pending_reply.extend_from_slice(&reply);
            sess.read_paused = true;
        } else {
            return Err(io::Error::new(
                ErrorKind::InvalidData,
                "terminal reply exceeds the configured pending reply limit",
            ));
        }
        if sess.read_paused {
            break;
        }
    }
    Ok(())
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

/// A runtime-free driver for multiple PTY-backed terminal sessions.
///
/// The driver owns the PTY master fds, the emulator state, the bounded
/// read/write buffers, and the child-process lifecycle of each session, but
/// never owns a poll loop. Call [`SessionDriver::poll_sources`] to learn what
/// to register, report readiness back with [`SessionDriver::drive`], and call
/// [`SessionDriver::poll_processes`] on a timer or `SIGCHLD` to observe child
/// exits without reaping.
///
/// # Drop behavior
///
/// Dropping the driver performs best-effort cleanup: remaining children are
/// force-killed and reaped, which may block. Failures cannot be reported from
/// `Drop`; call [`SessionDriver::shutdown`] to observe per-session results.
pub struct SessionDriver {
    config: DriverConfig,
    sessions: BTreeMap<SessionId, Session>,
    next_id: u64,
    /// Session to start the next round-robin pass from.
    cursor: Option<SessionId>,
    /// Whether the poll source changed since the last snapshot was taken.
    poll_dirty: bool,
}

impl SessionDriver {
    /// Creates a driver, validating `config`.
    pub fn new(config: DriverConfig) -> io::Result<Self> {
        config.validate()?;
        Ok(Self {
            config,
            sessions: BTreeMap::new(),
            next_id: 0,
            cursor: None,
            poll_dirty: true,
        })
    }

    /// Creates a driver with the default configuration.
    pub fn with_default_config() -> Self {
        Self::new(DriverConfig::default()).expect("default config is valid")
    }

    /// Spawns `command` in a new PTY-backed terminal session.
    ///
    /// On failure the PTY fd and the child are reclaimed, and no session id or
    /// poll source entry is exposed. Rows and columns must both be at least 1.
    pub fn create_session(
        &mut self,
        command: &mut Command,
        config: SessionConfig,
    ) -> Result<SessionId, DriverError> {
        if config.size.rows == 0 || config.size.cols == 0 {
            return Err(DriverError::InvalidSize(config.size));
        }
        let mut pty = PtyProcess::spawn(command, config.size).map_err(DriverError::Io)?;
        if let Err(err) = pty.set_nonblocking(true) {
            // Reclaim the fd and the child before reporting the failure.
            let mut closing = pty.into_closing();
            let _ = closing.signal_kill();
            let _ = closing.wait_and_reap();
            return Err(DriverError::Io(err));
        }
        let id = self.allocate_id().ok_or(DriverError::IdExhausted)?;
        let term = TerminalState::with_scrollback(config.size, config.scrollback_limits);
        let session = Session {
            id,
            generation: 0,
            pty: Pty::Live(pty),
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
            write_queue_limit: self.config.write_queue_limit,
            pending_reply_limit: self.config.pending_reply_limit,
            read_buffer_limit: self.config.read_buffer_limit,
            read_byte_quantum: self.config.read_byte_quantum,
            read_syscall_quantum: self.config.read_syscall_quantum,
            write_byte_quantum: self.config.write_byte_quantum,
            write_syscall_quantum: self.config.write_syscall_quantum,
        };
        self.sessions.insert(id, session);
        self.poll_dirty = true;
        Ok(id)
    }

    /// Returns a snapshot of everything currently registered with the event
    /// loop. Callers replace their whole registration set with this snapshot
    /// on every poll iteration.
    pub fn poll_sources(&mut self) -> PollSource {
        let mut entries = Vec::new();
        for sess in self.sessions.values() {
            if sess.phase == Phase::Closing {
                continue;
            }
            let Some(fd) = sess.pty.fd() else {
                continue;
            };
            entries.push(PollSourceEntry {
                fd,
                token: RegistrationToken {
                    id: sess.id,
                    generation: sess.generation,
                },
                interests: session_interests(sess),
            });
        }
        self.poll_dirty = false;
        PollSource { entries }
    }

    /// Records readiness and drives ready sessions with bounded work.
    ///
    /// `readiness` maps observed poll events to registration tokens. Tokens
    /// that do not match a live session's current generation are rejected and
    /// returned in [`DriveResult::stale_tokens`] without touching any session.
    ///
    /// Ready sessions are processed round-robin, resuming from where the
    /// previous drive stopped, and each session alternates between its read
    /// and write direction within the global budget. When a direction hits
    /// `EAGAIN`/`WouldBlock` only that direction's readiness is cleared; a
    /// budget limit instead keeps the ready state for the next drive.
    pub fn drive(
        &mut self,
        readiness: &[(RegistrationToken, Readiness)],
    ) -> Result<DriveResult, DriverError> {
        let mut stale_tokens = Vec::new();
        for (token, readiness) in readiness {
            match self.sessions.get_mut(&token.id) {
                Some(sess)
                    if sess.generation == token.generation
                        && sess.phase != Phase::Closing
                        && sess.pty.is_live() =>
                {
                    sess.ready.readable |=
                        readiness.readable || readiness.hangup || readiness.error;
                    sess.ready.writable |= readiness.writable;
                }
                _ => stale_tokens.push(*token),
            }
        }

        let mut budget = Budget {
            bytes: self.config.drive_byte_budget,
            syscalls: self.config.drive_syscall_budget,
        };
        let mut processed = Vec::new();

        let ids: Vec<SessionId> = self.sessions.keys().copied().collect();
        if !ids.is_empty() {
            let start = match self.cursor {
                Some(cursor) => ids.binary_search(&cursor).unwrap_or(0),
                None => 0,
            };
            let count = ids.len();
            let mut index = start;
            for _ in 0..count {
                let id = ids[index];
                let has_work = self.sessions.get(&id).is_some_and(session_has_work);
                if has_work && !budget.exhausted() {
                    self.process_session(id, &mut budget)?;
                    processed.push(id);
                    self.cursor = Some(ids[(index + 1) % count]);
                    if budget.exhausted() {
                        break;
                    }
                }
                index = (index + 1) % count;
            }
        }

        let has_pending_work = self.sessions.values().any(session_has_work);
        Ok(DriveResult {
            has_pending_work,
            poll_source_changed: std::mem::take(&mut self.poll_dirty),
            processed,
            events: Vec::new(),
            stale_tokens,
        })
    }

    /// Observes child exits without reaping any child.
    ///
    /// Call this on a `SIGCHLD` notification or a timer. Each session's exit
    /// event is returned at most once.
    pub fn poll_processes(&mut self) -> Result<Vec<DriverEvent>, DriverError> {
        let ids: Vec<SessionId> = self.sessions.keys().copied().collect();
        let mut events = Vec::new();
        for id in ids {
            let Some(sess) = self.sessions.get_mut(&id) else {
                continue;
            };
            if sess.exit_event_sent {
                continue;
            }
            let exit = match &mut sess.pty {
                Pty::Live(pty) => pty.observe_exit().map_err(DriverError::Io)?,
                Pty::Closing(pty) => pty.poll_exit().map_err(DriverError::Io)?,
            };
            if let Some(exit) = exit {
                sess.exit_event_sent = true;
                events.push(DriverEvent::SessionExited { id, exit });
            }
        }
        Ok(events)
    }

    /// Reaps the direct child and removes the session from the driver.
    ///
    /// The child exit must already be observable (for example through
    /// [`SessionDriver::poll_processes`]); otherwise `NotExited` is returned
    /// and the session is left intact. After a successful reap the session id
    /// becomes stale and its registration token is no longer valid.
    pub fn reap_session(&mut self, id: SessionId) -> Result<ExitStatus, DriverError> {
        let status = {
            let sess = self.sessions.get_mut(&id).ok_or(DriverError::StaleId(id))?;
            match &mut sess.pty {
                Pty::Live(pty) => match pty.try_wait().map_err(DriverError::Io)? {
                    Some(status) => status,
                    None => return Err(DriverError::NotExited(id)),
                },
                Pty::Closing(pty) => {
                    pty.poll_exit()
                        .map_err(DriverError::Io)?
                        .ok_or(DriverError::NotExited(id))?;
                    pty.wait_and_reap().map_err(DriverError::Io)?
                }
            }
        };
        let sess = self.sessions.remove(&id).expect("session exists");
        drop(sess);
        self.poll_dirty = true;
        Ok(status)
    }

    /// Logically closes a session.
    ///
    /// This synchronously invalidates the registration token, requests
    /// unregistration by removing the poll source entry, discards the write
    /// queue, pending reply and unread bytes, and closes the PTY master. The
    /// child is kept in a private closing state until reaped. After closing,
    /// new input, resize and read/decode are rejected, while the final
    /// terminal state, snapshots, process polling and signal delivery remain
    /// available until the child is reaped.
    pub fn close_session(&mut self, id: SessionId) -> Result<(), DriverError> {
        let mut sess = self.sessions.remove(&id).ok_or(DriverError::StaleId(id))?;
        if sess.phase != Phase::Closing {
            sess.generation += 1;
            sess.outbound.clear();
            sess.write_offset = 0;
            sess.pending_reply.clear();
            sess.read_buffer.clear();
            sess.ready = Readiness::default();
            sess.read_paused = false;
            sess.phase = Phase::Closing;
            if let Pty::Live(pty) = sess.pty {
                sess.pty = Pty::Closing(pty.into_closing());
            }
            self.poll_dirty = true;
        }
        self.sessions.insert(id, sess);
        Ok(())
    }

    /// Resizes a session, updating both the kernel PTY size and the emulator.
    ///
    /// A size with a zero row or column count, a stale id, or a closed session
    /// returns an error without side effects. Reapplying the current size
    /// succeeds without a syscall. The ioctl runs first; the emulator size is
    /// only updated when the ioctl succeeds.
    pub fn resize(&mut self, id: SessionId, size: Size) -> Result<(), DriverError> {
        if size.rows == 0 || size.cols == 0 {
            return Err(DriverError::InvalidSize(size));
        }
        let sess = self.sessions.get_mut(&id).ok_or(DriverError::StaleId(id))?;
        if sess.phase != Phase::Live && sess.phase != Phase::Eof {
            return Err(DriverError::SessionClosed(id));
        }
        if sess.term.size() == size {
            return Ok(());
        }
        let Pty::Live(pty) = &mut sess.pty else {
            return Err(DriverError::SessionClosed(id));
        };
        pty.resize(size).map_err(DriverError::Io)?;
        sess.term.resize(size);
        Ok(())
    }

    /// Enqueues a logical key for the session, encoded for its current modes.
    pub fn enqueue_key(&mut self, id: SessionId, event: KeyEvent) -> Result<(), DriverError> {
        // Logical key encodings are at most a few bytes; check capacity before
        // encoding.
        self.ensure_capacity(id, 8)?;
        let modes = self.terminal_modes(id)?;
        let bytes = encode_key(event, modes);
        self.append_bytes(id, &bytes)
    }

    /// Enqueues UTF-8 text verbatim for the session.
    pub fn enqueue_text(&mut self, id: SessionId, text: &str) -> Result<(), DriverError> {
        self.append_bytes(id, text.as_bytes())
    }

    /// Enqueues a paste for the session, wrapped in bracketed-paste markers
    /// when the terminal mode is active.
    pub fn enqueue_paste(&mut self, id: SessionId, text: &str) -> Result<(), DriverError> {
        // Bracketed-paste markers add at most 12 bytes; check capacity before
        // encoding so an oversized paste never allocates first.
        let upper = text.len().saturating_add(12);
        self.ensure_capacity(id, upper)?;
        let modes = self.terminal_modes(id)?;
        let bytes = encode_paste(text, modes);
        self.append_bytes(id, &bytes)
    }

    /// Enqueues raw bytes without validation or mode interpretation.
    pub fn enqueue_raw(&mut self, id: SessionId, bytes: &[u8]) -> Result<(), DriverError> {
        self.append_bytes(id, bytes)
    }

    /// Returns an owned snapshot of the session's terminal state.
    ///
    /// Available while the session is live, at EOF, logically closed, or after
    /// exit detection, until the child is reaped. After reap the id is stale.
    pub fn snapshot(&self, id: SessionId) -> Result<TerminalSnapshot, DriverError> {
        self.sessions
            .get(&id)
            .map(|sess| sess.term.snapshot())
            .ok_or(DriverError::StaleId(id))
    }

    /// Returns read-only access to the session's terminal state.
    pub fn terminal_state(&self, id: SessionId) -> Result<&TerminalState, DriverError> {
        self.sessions
            .get(&id)
            .map(|sess| &sess.term)
            .ok_or(DriverError::StaleId(id))
    }

    /// Returns the observable lifecycle phase of a session.
    pub fn session_status(&self, id: SessionId) -> Result<SessionStatus, DriverError> {
        self.sessions
            .get(&id)
            .map(|sess| match sess.phase {
                Phase::Live => SessionStatus::Live,
                Phase::Eof => SessionStatus::Eof,
                Phase::Closing => SessionStatus::Closing,
            })
            .ok_or(DriverError::StaleId(id))
    }

    /// Sends a graceful termination signal (SIGTERM) to the session's process
    /// group.
    pub fn terminate_session(&mut self, id: SessionId) -> Result<SignalOutcome, DriverError> {
        let sess = self.sessions.get_mut(&id).ok_or(DriverError::StaleId(id))?;
        match &mut sess.pty {
            Pty::Live(pty) => pty.signal_group(libc::SIGTERM).map_err(DriverError::Io),
            Pty::Closing(pty) => pty.signal_terminate().map_err(DriverError::Io),
        }
    }

    /// Sends a force-termination signal (SIGKILL) to the session's process
    /// group.
    pub fn force_terminate_session(&mut self, id: SessionId) -> Result<SignalOutcome, DriverError> {
        let sess = self.sessions.get_mut(&id).ok_or(DriverError::StaleId(id))?;
        match &mut sess.pty {
            Pty::Live(pty) => pty.signal_group(libc::SIGKILL).map_err(DriverError::Io),
            Pty::Closing(pty) => pty.signal_kill().map_err(DriverError::Io),
        }
    }

    /// Cleans up all sessions, force-killing and reaping every remaining
    /// child, and returns one result per session.
    ///
    /// This consumes the driver and may block while reaping. An error on one
    /// session does not stop cleanup of the others.
    pub fn shutdown(mut self) -> ShutdownOutcome {
        let ids: Vec<SessionId> = self.sessions.keys().copied().collect();
        let mut outcomes = Vec::new();
        for id in ids {
            let Some(sess) = self.sessions.remove(&id) else {
                continue;
            };
            let result = match sess.pty {
                Pty::Live(pty) => {
                    let mut closing = pty.into_closing();
                    let _ = closing.signal_kill();
                    closing.wait_and_reap()
                }
                Pty::Closing(mut closing) => {
                    let _ = closing.signal_kill();
                    closing.wait_and_reap()
                }
            };
            outcomes.push((id, result));
        }
        ShutdownOutcome { sessions: outcomes }
    }

    fn allocate_id(&mut self) -> Option<SessionId> {
        if self.next_id == u64::MAX {
            return None;
        }
        let id = SessionId(self.next_id);
        self.next_id += 1;
        Some(id)
    }

    fn terminal_modes(&self, id: SessionId) -> Result<TerminalModes, DriverError> {
        self.sessions
            .get(&id)
            .map(|sess| sess.term.modes())
            .ok_or(DriverError::StaleId(id))
    }

    fn ensure_capacity(&self, id: SessionId, len: usize) -> Result<(), DriverError> {
        let sess = self.sessions.get(&id).ok_or(DriverError::StaleId(id))?;
        if sess.phase != Phase::Live && sess.phase != Phase::Eof {
            return Err(DriverError::SessionClosed(id));
        }
        if !capacity_ok(
            sess.unsent(),
            sess.pending_reply.len(),
            sess.write_queue_limit,
            sess.pending_reply_limit,
            len,
        ) {
            return Err(DriverError::Backpressure);
        }
        Ok(())
    }

    fn append_bytes(&mut self, id: SessionId, bytes: &[u8]) -> Result<(), DriverError> {
        let sess = self.sessions.get_mut(&id).ok_or(DriverError::StaleId(id))?;
        if sess.phase != Phase::Live && sess.phase != Phase::Eof {
            return Err(DriverError::SessionClosed(id));
        }
        if !capacity_ok(
            sess.unsent(),
            sess.pending_reply.len(),
            sess.write_queue_limit,
            sess.pending_reply_limit,
            bytes.len(),
        ) {
            return Err(DriverError::Backpressure);
        }
        if sess.pending_reply.is_empty() {
            sess.outbound.extend_from_slice(bytes);
        } else {
            sess.pending_reply.extend_from_slice(bytes);
        }
        self.poll_dirty = true;
        Ok(())
    }

    /// Processes one session with alternating read/write phases until both
    /// directions are idle or the budgets are exhausted.
    fn process_session(&mut self, id: SessionId, budget: &mut Budget) -> io::Result<()> {
        let mut direction = self
            .sessions
            .get(&id)
            .map(|sess| sess.direction)
            .unwrap_or(Direction::Write);
        for _ in 0..2 {
            if budget.exhausted() {
                break;
            }
            // Start with the direction that actually has work, so a session
            // with nothing to write is not blocked behind the write phase.
            if !self.session_has_direction_work(id, direction) {
                direction = direction.opposite();
                if !self.session_has_direction_work(id, direction) {
                    break;
                }
            }
            let progressed = match direction {
                Direction::Write => self.write_phase(id, budget)?,
                Direction::Read => self.read_phase(id, budget)?,
            };
            if !progressed {
                break;
            }
            direction = direction.opposite();
            if !self.session_has_direction_work(id, direction) {
                break;
            }
        }
        if let Some(sess) = self.sessions.get_mut(&id) {
            sess.direction = direction;
        }
        Ok(())
    }

    /// Whether the given direction currently has work for `id`.
    fn session_has_direction_work(&self, id: SessionId, direction: Direction) -> bool {
        let Some(sess) = self.sessions.get(&id) else {
            return false;
        };
        match direction {
            Direction::Write => !sess.outbound_empty() || !sess.pending_reply.is_empty(),
            Direction::Read => {
                !sess.read_paused
                    && sess.phase != Phase::Closing
                    && sess.phase != Phase::Eof
                    && (!sess.read_buffer.is_empty() || (sess.ready.readable && sess.pty.is_live()))
            }
        }
    }

    /// Writes pending bytes for one session, bounded by the per-session write
    /// quanta and the drive budgets.
    fn write_phase(&mut self, id: SessionId, budget: &mut Budget) -> io::Result<bool> {
        let mut progressed = false;
        if let Some(sess) = self.sessions.get_mut(&id)
            && readmit_pending(sess)
        {
            self.poll_dirty = true;
            progressed = true;
        }

        let mut attempts = 0;
        loop {
            if budget.exhausted() {
                break;
            }
            let Some(sess) = self.sessions.get_mut(&id) else {
                break;
            };
            if attempts >= sess.write_syscall_quantum {
                break;
            }
            if sess.phase == Phase::Closing {
                break;
            }
            let unsent = sess.unsent();
            if unsent == 0 {
                break;
            }
            let cap = sess.write_byte_quantum.min(budget.bytes_left()).min(unsent);
            if cap == 0 {
                break;
            }
            let Pty::Live(pty) = &mut sess.pty else {
                break;
            };
            let start = sess.write_offset;
            let buf = sess.outbound[start..start + cap].to_vec();
            let result = pty.write(&buf);
            attempts += 1;
            budget.consume(0, 1);
            match result {
                Ok(0) => break,
                Ok(n) => {
                    let sess = self.sessions.get_mut(&id).expect("session exists");
                    sess.write_offset += n;
                    budget.consume(n, 0);
                    if sess.write_offset == sess.outbound.len() {
                        sess.outbound.clear();
                        sess.write_offset = 0;
                        self.poll_dirty = true;
                    }
                    progressed = true;
                }
                Err(err) if err.kind() == ErrorKind::WouldBlock => {
                    let sess = self.sessions.get_mut(&id).expect("session exists");
                    sess.ready.writable = false;
                    break;
                }
                Err(err) if err.kind() == ErrorKind::Interrupted => break,
                Err(err) => {
                    let sess = self.sessions.get_mut(&id).expect("session exists");
                    sess.ready.writable = false;
                    return Err(err);
                }
            }
        }
        Ok(progressed)
    }

    /// Reads and decodes bytes for one session, bounded by the per-session
    /// read quanta and the drive budgets.
    fn read_phase(&mut self, id: SessionId, budget: &mut Budget) -> io::Result<bool> {
        let mut progressed = false;
        progressed |= self.decode_buffered(id, budget)? > 0;

        let should_read = self.sessions.get(&id).is_some_and(|sess| {
            !sess.read_paused
                && sess.phase == Phase::Live
                && sess.ready.readable
                && sess.pty.is_live()
        });
        if !should_read {
            return Ok(progressed);
        }

        let mut attempts = 0;
        loop {
            if budget.exhausted() {
                break;
            }
            let Some(sess) = self.sessions.get_mut(&id) else {
                break;
            };
            if attempts >= sess.read_syscall_quantum {
                break;
            }
            if sess.read_paused || sess.phase != Phase::Live {
                break;
            }
            let room = sess
                .read_buffer_limit
                .saturating_sub(sess.read_buffer.len());
            if room == 0 {
                break;
            }
            let cap = sess.read_byte_quantum.min(budget.bytes_left()).min(room);
            if cap == 0 {
                break;
            }
            let Pty::Live(pty) = &mut sess.pty else {
                break;
            };
            let mut buf = vec![0u8; cap];
            let result = pty.read(&mut buf);
            attempts += 1;
            budget.consume(0, 1);
            match result {
                Ok(0) => {
                    let sess = self.sessions.get_mut(&id).expect("session exists");
                    sess.phase = Phase::Eof;
                    sess.ready.readable = false;
                    self.poll_dirty = true;
                    break;
                }
                Ok(n) => {
                    let sess = self.sessions.get_mut(&id).expect("session exists");
                    sess.read_buffer.extend_from_slice(&buf[..n]);
                    budget.consume(n, 0);
                    progressed = true;
                }
                Err(err) if err.kind() == ErrorKind::WouldBlock => {
                    let sess = self.sessions.get_mut(&id).expect("session exists");
                    sess.ready.readable = false;
                    break;
                }
                Err(err) if err.kind() == ErrorKind::Interrupted => break,
                Err(err) if err.raw_os_error() == Some(libc::EIO) => {
                    // PTY slave close is reported as EIO on some platforms;
                    // normalize it to EOF.
                    let sess = self.sessions.get_mut(&id).expect("session exists");
                    sess.phase = Phase::Eof;
                    sess.ready.readable = false;
                    self.poll_dirty = true;
                    break;
                }
                Err(err) => {
                    let sess = self.sessions.get_mut(&id).expect("session exists");
                    sess.ready.readable = false;
                    return Err(err);
                }
            }
            self.decode_buffered(id, budget)?;
            if self.sessions.get(&id).is_some_and(|sess| sess.read_paused) {
                break;
            }
        }
        Ok(progressed)
    }

    /// Decodes already-buffered raw bytes, one byte at a time, until paused,
    /// empty, or the byte budget is exhausted. Returns the number of bytes
    /// decoded.
    fn decode_buffered(&mut self, id: SessionId, budget: &mut Budget) -> io::Result<usize> {
        let mut decoded = 0;
        loop {
            if budget.bytes_left() == 0 {
                break;
            }
            let byte = match self.sessions.get_mut(&id) {
                Some(sess) if !sess.read_paused && !sess.read_buffer.is_empty() => {
                    sess.read_buffer.remove(0)
                }
                _ => break,
            };
            budget.consume(1, 0);
            let sess = self.sessions.get_mut(&id).expect("session exists");
            decode_byte(sess, byte)?;
            decoded += 1;
        }
        if decoded > 0 {
            self.poll_dirty = true;
        }
        Ok(decoded)
    }
}

impl Drop for SessionDriver {
    fn drop(&mut self) {
        // Best-effort cleanup: force-kill and reap every remaining child.
        // This may block, and failures cannot be reported from `Drop`.
        let ids: Vec<SessionId> = self.sessions.keys().copied().collect();
        for id in ids {
            let Some(sess) = self.sessions.remove(&id) else {
                continue;
            };
            let mut closing = match sess.pty {
                Pty::Live(pty) => pty.into_closing(),
                Pty::Closing(closing) => closing,
            };
            let _ = closing.signal_kill();
            let _ = closing.wait_and_reap();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn id_allocation_is_monotonic_and_exhausts() {
        let mut driver = SessionDriver::with_default_config();
        driver.next_id = u64::MAX - 1;
        assert_eq!(driver.allocate_id(), Some(SessionId(u64::MAX - 1)));
        assert_eq!(driver.allocate_id(), None);
    }

    #[test]
    fn default_config_is_valid() {
        DriverConfig::default()
            .validate()
            .expect("default is valid");
    }

    #[test]
    fn config_rejects_zero_limits() {
        let base = DriverConfig::default();
        for (name, value) in [
            ("read_byte_quantum", base.read_byte_quantum),
            ("read_syscall_quantum", base.read_syscall_quantum),
            ("write_byte_quantum", base.write_byte_quantum),
            ("write_syscall_quantum", base.write_syscall_quantum),
            ("drive_byte_budget", base.drive_byte_budget),
            ("drive_syscall_budget", base.drive_syscall_budget),
            ("write_queue_limit", base.write_queue_limit),
            ("read_buffer_limit", base.read_buffer_limit),
            ("pending_reply_limit", base.pending_reply_limit),
        ] {
            let mut config = base;
            match name {
                "read_byte_quantum" => config.read_byte_quantum = 0,
                "read_syscall_quantum" => config.read_syscall_quantum = 0,
                "write_byte_quantum" => config.write_byte_quantum = 0,
                "write_syscall_quantum" => config.write_syscall_quantum = 0,
                "drive_byte_budget" => config.drive_byte_budget = 0,
                "drive_syscall_budget" => config.drive_syscall_budget = 0,
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
        let config = DriverConfig {
            write_queue_limit: MAX_REPLY_BYTES - 1,
            ..DriverConfig::default()
        };
        assert!(config.validate().is_err());
        let config = DriverConfig {
            pending_reply_limit: MAX_REPLY_BYTES - 1,
            ..DriverConfig::default()
        };
        assert!(config.validate().is_err());
        let config = DriverConfig {
            write_queue_limit: MAX_REPLY_BYTES,
            pending_reply_limit: MAX_REPLY_BYTES,
            ..DriverConfig::default()
        };
        config.validate().expect("exactly one reply is allowed");
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

        assert!(readmit_pending(&mut sess));
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

        assert!(!readmit_pending(&mut sess));
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
            decode_byte(&mut sess, byte).expect("decode");
        }
        // The 5-byte reply does not fit in the 4 free bytes and is held.
        assert_eq!(sess.pending_reply, b"\x1b[?6c");
        assert!(sess.read_paused);
        // Once the outbound queue drains, the reply is re-admitted first and
        // decoding resumes.
        sess.outbound.clear();
        assert!(readmit_pending(&mut sess));
        assert!(sess.pending_reply.is_empty());
        assert!(!sess.read_paused);
        assert_eq!(sess.outbound, b"\x1b[?6c");
    }

    #[test]
    fn session_interests_reflect_pending_state() {
        let mut sess = test_session();
        let interests = session_interests(&sess);
        assert!(interests.readable);
        assert!(!interests.writable);

        sess.outbound = b"x".to_vec();
        assert!(session_interests(&sess).writable);

        sess.read_paused = true;
        let interests = session_interests(&sess);
        assert!(!interests.readable);
        assert!(interests.writable);

        sess.phase = Phase::Eof;
        sess.read_paused = false;
        let interests = session_interests(&sess);
        assert!(!interests.readable);
        assert!(interests.writable);

        sess.phase = Phase::Closing;
        assert_eq!(
            session_interests(&sess),
            Interests {
                readable: false,
                writable: false
            }
        );
    }

    fn test_session() -> Session {
        Session {
            id: SessionId(0),
            generation: 0,
            pty: Pty::Live(dummy_pty()),
            term: TerminalState::new(Size { rows: 1, cols: 1 }),
            phase: Phase::Live,
            read_buffer: Vec::new(),
            outbound: Vec::new(),
            write_offset: 0,
            pending_reply: Vec::new(),
            ready: Readiness::default(),
            direction: Direction::Write,
            read_paused: false,
            exit_event_sent: false,
            write_queue_limit: 64,
            pending_reply_limit: 64,
            read_buffer_limit: 64,
            read_byte_quantum: 16,
            read_syscall_quantum: 2,
            write_byte_quantum: 16,
            write_syscall_quantum: 2,
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
}
