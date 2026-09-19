//! Shared harness for the multi-session workflow test.
//!
//! Everything here drives sessions through the public API only: the test owns
//! the poll loop, so these helpers rebuild the poll set from
//! [`termnix::Session::fd`] and [`termnix::Session::interests`] on every
//! iteration instead of caching registrations. Waiting is always deadline +
//! `poll` + `pump_io`; no fixed sleep is used to wait for child progress.

use std::{
    io::{Error, ErrorKind},
    num::NonZeroU16,
    os::fd::RawFd,
    process::Command,
    time::{Duration, Instant},
};

use termnix::{Interests, PumpBudget, Session, SessionStatus, Size};

/// Whole-test deadline: any longer wait is a hang, not slow progress.
pub const DEADLINE: Duration = Duration::from_secs(15);

/// Fixed interval between process polls inside the workflow loop.
pub const PROCESS_POLL_INTERVAL: Duration = Duration::from_millis(20);

/// Longest single `poll` wait. Short enough that the process poll interval and
/// the test deadline stay observable without a busy loop.
const POLL_TIMEOUT_MS: libc::c_int = 10;

/// The `24x80` grid used by every session in the workflow test.
pub fn default_size() -> Size {
    size(24, 80)
}

/// Builds a grid size from two dimensions known non-zero at the call site.
pub fn size(rows: u16, cols: u16) -> Size {
    Size {
        rows: NonZeroU16::new(rows).expect("rows is non-zero"),
        cols: NonZeroU16::new(cols).expect("cols is non-zero"),
    }
}

/// Spawns `/bin/sh -c <script>` in a PTY-backed session of `size`.
pub fn spawn(script: &str, size: Size) -> Session {
    let mut command = Command::new("/bin/sh");
    command.arg("-c").arg(script);
    Session::new(&mut command, size).expect("create session")
}

/// Renders the terminal state as newline-terminated rows with trailing blanks
/// and wide-character continuation cells removed.
///
/// Retained scrollback is rendered before the visible rows, because a marker
/// printed early by a chatty child scrolls out of the visible grid and would
/// otherwise disappear from assertions. Normalization matters twice:
/// assertions match on it, and timeout diagnostics print it, so a failure
/// shows what the child actually emitted rather than a screenful of padding.
pub fn snapshot_text(session: &Session) -> String {
    let state = session.terminal_state();
    let mut out = String::new();
    for line in state.scrollback_lines().iter() {
        out.push_str(&render_row(line.cells()));
    }
    for row in state.rows() {
        out.push_str(&render_row(row));
    }
    out
}

/// Renders only the visible rows, ignoring scrollback.
///
/// Cheap enough to call inside a wait loop: freshly written output is always on
/// screen, and scrollback can be thousands of lines long.
pub fn screen_text(session: &Session) -> String {
    let mut out = String::new();
    for row in session.terminal_state().rows() {
        out.push_str(&render_row(row));
    }
    out
}

/// Renders one row of cells, dropping continuation cells and trailing blanks.
fn render_row(cells: &[termnix::Cell]) -> String {
    let mut line = String::new();
    for cell in cells {
        if cell.width == 0 {
            continue;
        }
        line.push(cell.ch);
    }
    line.push('\n');
    line
}

/// Pumps every session until its `needs_pump` loop is drained.
///
/// Suitable for ordinary progress: any session whose internal work is still
/// executable is taken to idle, so a caller does not have to re-poll.
pub fn pump_all(sessions: &mut [Session], budget: PumpBudget) {
    for session in sessions.iter_mut() {
        session.pump_io(budget).expect("pump");
        while session.needs_pump() {
            session.pump_io(budget).expect("pump");
        }
    }
}

/// Pumps every session exactly once, in order: one round-robin rotation.
///
/// Unlike [`pump_all`], a session that still has work is left for the next
/// rotation, so one rotation advances all sessions by ~one quantum each. This
/// is the scheduling quantum a fair multi-session loop uses, and it is what
/// lets a short burst on one session be observed before a large burst on
/// another is exhausted.
pub fn rotate_once(sessions: &mut [Session], budget: PumpBudget) {
    for session in sessions.iter_mut() {
        session.pump_io(budget).expect("pump");
    }
}

/// Drives `sessions` with one rotation per condition check until `cond` holds.
///
/// Used for fairness assertions: the loop never drains one session to idle, so
/// a large burst on one session cannot hide another's progress. `budget` is
/// the per-`pump_io` ceiling; a tight budget makes any non-empty backlog spill
/// across rotations even with a small fixture.
pub fn rotate_until<F>(sessions: &mut [Session], budget: PumpBudget, context: &str, mut cond: F)
where
    F: FnMut(&mut [Session]) -> bool,
{
    let deadline = Instant::now() + DEADLINE;
    while Instant::now() < deadline {
        rotate_once(sessions, budget);
        if cond(sessions) {
            return;
        }
        poll_once(sessions, POLL_TIMEOUT_MS);
    }
    panic!(
        "timed out waiting for {context}\nstatuses: {:?}\nsnapshots:\n{}",
        statuses(sessions),
        snapshots(sessions),
    );
}

/// Enqueues `input` into `sessions[index]`.
pub fn enqueue(sessions: &mut [Session], index: usize, input: termnix::Input<'_>) {
    sessions[index].enqueue_input(input).expect("enqueue input");
}

/// Drives `sessions` until `cond` holds.
///
/// Each iteration pumps every session once, re-reads the condition, then waits
/// on freshly collected fds. `cond` receives the sessions mutably so a check
/// can reap, close, or feed a session as part of the condition. Panics with
/// `context`, the per-session statuses, and the normalized snapshots when the
/// deadline expires. `budget` is the per-`pump_io` ceiling used for every pump.
pub fn pump_until<F>(sessions: &mut [Session], budget: PumpBudget, context: &str, mut cond: F)
where
    F: FnMut(&mut [Session]) -> bool,
{
    let deadline = Instant::now() + DEADLINE;
    let mut next_process_poll = Instant::now();
    while Instant::now() < deadline {
        pump_all(sessions, budget);
        if cond(sessions) {
            return;
        }
        let now = Instant::now();
        if now >= next_process_poll {
            poll_children(sessions);
            next_process_poll = now + PROCESS_POLL_INTERVAL;
        }
        poll_once(sessions, POLL_TIMEOUT_MS);
    }
    panic!(
        "timed out waiting for {context}\nstatuses: {:?}\nsnapshots:\n{}",
        statuses(sessions),
        snapshots(sessions),
    );
}

/// Reaps any child that has exited, without disabling its PTY I/O.
///
/// Errors are ignored: a poll attempt is opportunistic and `try_wait` is not
/// what the condition under test is about.
pub fn poll_children(sessions: &mut [Session]) {
    for session in sessions.iter_mut() {
        if session.fd().is_some() || session.status() == SessionStatus::Eof {
            let _ = session.try_wait();
        }
    }
}

/// Waits once on the sessions' current registrations, up to `timeout_ms`.
///
/// Returns the number of fds that reported readiness. `EINTR` is retried; any
/// other error is a test failure. A session without an fd contributes nothing,
/// which is how a closed or reaped session leaves the loop.
pub fn poll_once(sessions: &[Session], timeout_ms: libc::c_int) -> usize {
    let mut pollfds = Vec::new();
    for session in sessions {
        let Some(fd) = session.fd() else {
            continue;
        };
        let Interests { readable, writable } = session.interests();
        let mut events = 0;
        if readable {
            events |= libc::POLLIN;
        }
        if writable {
            events |= libc::POLLOUT;
        }
        if events != 0 {
            pollfds.push(libc::pollfd {
                fd,
                events,
                revents: 0,
            });
        }
    }
    if pollfds.is_empty() {
        return 0;
    }
    loop {
        let ready = unsafe {
            libc::poll(
                pollfds.as_mut_ptr(),
                pollfds.len() as libc::nfds_t,
                timeout_ms,
            )
        };
        if ready >= 0 {
            return ready as usize;
        }
        let err = Error::last_os_error();
        if err.kind() != ErrorKind::Interrupted {
            panic!("poll failed: {err}");
        }
    }
}

/// Writes `bytes` with a single `write(2)` on `fd`, returning the raw result.
///
/// Used to inject a stale fd into a session's drive path: writing to an fd the
/// session under test does not own must not move that session, so the caller
/// asserts on the returned error rather than the helper doing it here.
pub fn write_raw(fd: RawFd, bytes: &[u8]) -> std::io::Result<usize> {
    let written = unsafe { libc::write(fd, bytes.as_ptr().cast(), bytes.len()) };
    if written < 0 {
        Err(Error::last_os_error())
    } else {
        Ok(written as usize)
    }
}

/// Lifecycle phases for every session, for failure diagnostics.
fn statuses(sessions: &[Session]) -> Vec<SessionStatus> {
    sessions.iter().map(Session::status).collect()
}

/// Number of trailing snapshot lines kept for failure diagnostics.
///
/// A chatty session can scroll for thousands of lines; the tail is what shows
/// whether the awaited marker arrived, and printing everything would make a
/// failure expensive to read (and to capture).
const DIAGNOSTIC_TAIL_LINES: usize = 12;

/// Tail of each session's normalized snapshot, for failure diagnostics.
pub fn snapshots(sessions: &[Session]) -> String {
    let mut out = String::new();
    for (index, session) in sessions.iter().enumerate() {
        let text = snapshot_text(session);
        let lines: Vec<&str> = text.lines().filter(|line| !line.is_empty()).collect();
        let start = lines.len().saturating_sub(DIAGNOSTIC_TAIL_LINES);
        out.push_str(&format!(
            "--- session {index} (last {} of {} lines) ---\n",
            lines.len() - start,
            lines.len(),
        ));
        for line in &lines[start..] {
            out.push_str(line);
            out.push('\n');
        }
    }
    out
}

/// On-drop teardown that force-kills and reaps any session adopted into it.
///
/// `shutdown` consumes the session, so the workflow keeps its sessions in a
/// plain `Vec` while it drives them and hands them to the guard at the end.
/// Whatever the guard still holds when it drops is shut down, and any error is
/// reported rather than swallowed.
#[derive(Default)]
pub struct Teardown {
    sessions: Vec<Session>,
}

impl Teardown {
    /// Adopts one session for end-of-test teardown.
    pub fn adopt(&mut self, session: Session) {
        self.sessions.push(session);
    }

    /// How many sessions the guard is still holding.
    pub fn len(&self) -> usize {
        self.sessions.len()
    }
}

impl Drop for Teardown {
    fn drop(&mut self) {
        for (index, session) in std::mem::take(&mut self.sessions).into_iter().enumerate() {
            if let Err(err) = session.shutdown() {
                eprintln!("session {index}: shutdown failed during teardown: {err}");
            }
        }
    }
}
