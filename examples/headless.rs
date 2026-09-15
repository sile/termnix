//! Headless use of a public [`termnix::Session`] without a host TTY or UI.
//!
//! termnix does not own poll loops, drawing, or host terminal mode. This
//! example shows how a non-interactive caller—automation, a test harness, a
//! remote frontend, or process orchestration—still drives one PTY-backed
//! session with only the public API: register [`termnix::Session::fd`] /
//! [`termnix::Session::interests`] with `libc::poll`, advance with
//! [`termnix::Session::pump_io`] / [`termnix::Session::needs_pump`], observe
//! child state through owned snapshots (not by scanning the raw PTY byte
//! stream), inject text and keys with [`termnix::Session::enqueue_input`], and
//! tear down with an explicit [`termnix::Session::shutdown`] after exit and
//! EOF.
//!
//! The child is a deterministic `/bin/sh -c` script (no user shell rc, locale,
//! network, or extra apps). It asks for primary device attributes, checks
//! termnix's fixed reply, prints markers, reads one line, echoes it, and
//! exits. Success prints a single `headless session ok` line; that line is an
//! acceptance signal, not a walkthrough—the source is the documentation.
//! Failure prints an English error that includes lifecycle status and a
//! normalized snapshot when useful.
//!
//! Run with stdin disconnected from a TTY, for example:
//! `cargo run --quiet --example headless </dev/null`.

use std::{
    io::{self, ErrorKind},
    os::fd::RawFd,
    process::Command,
    time::{Duration, Instant},
};

/// Overall deadline for the example. Does not rely on fixed sleeps.
const OVERALL_DEADLINE: Duration = Duration::from_secs(15);

/// Interval between `try_wait` attempts.
const PROCESS_POLL_INTERVAL: Duration = Duration::from_millis(50);

/// Soft write-queue limit for caller-side backpressure.
const WRITE_SOFT_LIMIT: usize = 4096;

/// Bound on `needs_pump` drains inside one outer-loop iteration.
const PUMP_DRAIN_BUDGET: usize = 64;

/// Application text sent to the child.
const PAYLOAD: &str = "hello";

fn main() {
    match run() {
        Ok(()) => println!("headless session ok"),
        Err(err) => {
            eprintln!("{err}");
            std::process::exit(1);
        }
    }
}

/// Runs spawn through shutdown, attempting shutdown once even on failure.
fn run() -> Result<(), AppError> {
    let mut session = spawn_session()?;
    let workflow = workflow(&mut session);
    let shutdown = session.shutdown();
    match (workflow, shutdown) {
        (Ok(()), Ok(_)) => Ok(()),
        (Ok(()), Err(err)) => Err(AppError::shutdown_only(err)),
        (Err(err), Ok(_)) => Err(err),
        (Err(mut err), Err(shutdown_err)) => {
            err.set_shutdown(shutdown_err);
            Err(err)
        }
    }
}

fn spawn_session() -> Result<termnix::Session, AppError> {
    let script = child_script();
    let mut command = Command::new("/bin/sh");
    command.arg("-c").arg(script);
    let size = termnix::Size::new(24, 80).expect("non-zero size");
    termnix::Session::new(&mut command, size).map_err(AppError::io)
}

/// Child that validates a primary DA reply, then echoes one line and exits.
fn child_script() -> &'static str {
    concat!(
        "stty -echo -icanon min 1 time 0 || { printf 'STTY_ERROR\\n'; exit 1; }; ",
        "printf '\\033[c'; ",
        "reply=$(dd bs=1 count=5 2>/dev/null); ",
        "expected=$(printf '\\033[?6c'); ",
        "if [ \"$reply\" = \"$expected\" ]; then ",
        "printf 'REPLY_OK\\n'; printf 'READY\\n'; ",
        "else printf 'REPLY_ERROR\\n'; exit 1; fi; ",
        "stty icanon || { printf 'STTY_ERROR\\n'; exit 1; }; ",
        "IFS= read -r line || { printf 'READ_ERROR\\n'; exit 1; }; ",
        "printf 'ECHO:%s\\n' \"$line\""
    )
}

fn workflow(session: &mut termnix::Session) -> Result<(), AppError> {
    let deadline = Instant::now() + OVERALL_DEADLINE;
    let mut next_process_poll = Instant::now();

    drive_until(session, deadline, &mut next_process_poll, |session| {
        let rows = visible_rows(session.terminal_state().snapshot());
        Ok(row_contains(&rows, "REPLY_OK") && row_contains(&rows, "READY"))
    })?;

    enqueue_payload(session, deadline, &mut next_process_poll)?;

    drive_until(session, deadline, &mut next_process_poll, |session| {
        let rows = visible_rows(session.terminal_state().snapshot());
        Ok(row_contains(&rows, &format!("ECHO:{PAYLOAD}")))
    })?;

    let mut exit_status = None;
    drive_until(session, deadline, &mut next_process_poll, |session| {
        if exit_status.is_none() {
            match session.try_wait() {
                Ok(Some(status)) => exit_status = Some(status),
                Ok(None) => {}
                Err(err) => return Err(AppError::io(err)),
            }
        }
        let drained = matches!(session.status(), termnix::SessionStatus::Eof)
            || (session.fd().is_none() && !session.needs_pump());
        Ok(exit_status.is_some() && drained)
    })?;

    let rows = visible_rows(session.terminal_state().snapshot());
    if !row_contains(&rows, "REPLY_OK")
        || !row_contains(&rows, "READY")
        || !row_contains(&rows, &format!("ECHO:{PAYLOAD}"))
    {
        return Err(AppError::protocol(
            "final snapshot missing expected markers",
            session,
        ));
    }

    let status = exit_status.expect("exit observed");
    if !status.success() {
        return Err(AppError::protocol(
            format!("child exited unsuccessfully: {status:?}"),
            session,
        ));
    }

    Ok(())
}

fn enqueue_payload(
    session: &mut termnix::Session,
    deadline: Instant,
    next_process_poll: &mut Instant,
) -> Result<(), AppError> {
    let text = termnix::Input::Raw(PAYLOAD.as_bytes());
    let enter = termnix::Input::Key(termnix::KeyEvent::new(termnix::KeyCode::Enter));
    try_enqueue(session, text, deadline, next_process_poll)?;
    try_enqueue(session, enter, deadline, next_process_poll)?;
    Ok(())
}

/// Drains the write queue while over the soft limit, then enqueues.
fn try_enqueue(
    session: &mut termnix::Session,
    input: termnix::Input<'_>,
    deadline: Instant,
    next_process_poll: &mut Instant,
) -> Result<(), AppError> {
    let modes = session.terminal_state().modes();
    let need = input.byte_len(modes);
    while session.metrics().pending_write_bytes.saturating_add(need) > WRITE_SOFT_LIMIT {
        if Instant::now() >= deadline {
            return Err(AppError::timeout(
                "timed out waiting for write queue room",
                session,
            ));
        }
        pump_session(session)?;
        poll_once(session, deadline, *next_process_poll)?;
        maybe_poll_process(session, next_process_poll)?;
    }
    session.enqueue_input(input).map_err(AppError::io)?;
    // Make progress on any writable interest immediately.
    pump_session(session)
}

fn drive_until<F>(
    session: &mut termnix::Session,
    deadline: Instant,
    next_process_poll: &mut Instant,
    mut pred: F,
) -> Result<(), AppError>
where
    F: FnMut(&mut termnix::Session) -> Result<bool, AppError>,
{
    while Instant::now() < deadline {
        pump_session(session)?;
        maybe_poll_process(session, next_process_poll)?;
        if pred(session)? {
            return Ok(());
        }
        poll_once(session, deadline, *next_process_poll)?;
    }
    Err(AppError::timeout(
        "timed out waiting for session progress",
        session,
    ))
}

fn pump_session(session: &mut termnix::Session) -> Result<(), AppError> {
    session
        .pump_io(termnix::PumpBudget::default())
        .map_err(AppError::io)?;
    let mut budget = PUMP_DRAIN_BUDGET;
    while session.needs_pump() && budget > 0 {
        session
            .pump_io(termnix::PumpBudget::default())
            .map_err(AppError::io)?;
        budget -= 1;
    }
    Ok(())
}

fn maybe_poll_process(
    session: &mut termnix::Session,
    next_process_poll: &mut Instant,
) -> Result<(), AppError> {
    let now = Instant::now();
    if now < *next_process_poll {
        return Ok(());
    }
    let _ = session.try_wait().map_err(AppError::io)?;
    *next_process_poll = now + PROCESS_POLL_INTERVAL;
    Ok(())
}

fn poll_once(
    session: &mut termnix::Session,
    deadline: Instant,
    next_process_poll: Instant,
) -> Result<(), AppError> {
    if session.needs_pump() {
        return Ok(());
    }
    let Some(fd) = session.fd() else {
        return Ok(());
    };
    let interests = session.interests();
    let mut events = 0;
    if interests.readable {
        events |= libc::POLLIN;
    }
    if interests.writable {
        events |= libc::POLLOUT;
    }
    if events == 0 {
        // No I/O interest: wait until the process-poll deadline.
        let timeout = poll_timeout_ms(deadline, next_process_poll);
        let rc = unsafe { libc::poll(std::ptr::null_mut(), 0, timeout) };
        return map_poll_rc(rc);
    }

    let mut pollfd = libc::pollfd {
        fd: fd as RawFd,
        events,
        revents: 0,
    };
    let timeout = poll_timeout_ms(deadline, next_process_poll);
    let rc = unsafe { libc::poll(&mut pollfd, 1, timeout) };
    map_poll_rc(rc)?;
    if pollfd.revents & (libc::POLLERR | libc::POLLNVAL) != 0 {
        return Err(AppError::msg(format!(
            "poll reported error on session fd: revents={}",
            pollfd.revents
        )));
    }
    Ok(())
}

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

fn poll_timeout_ms(deadline: Instant, next_process_poll: Instant) -> i32 {
    let now = Instant::now();
    let until = if deadline < next_process_poll {
        deadline
    } else {
        next_process_poll
    };
    if now >= until {
        return 0;
    }
    let ms = until.duration_since(now).as_millis();
    i32::try_from(ms).unwrap_or(i32::MAX).max(0)
}

/// Builds normalized row strings from scrollback and the visible screen.
fn visible_rows(snapshot: termnix::TerminalSnapshot) -> Vec<String> {
    let mut rows = Vec::new();
    for line in snapshot.scrollback() {
        rows.push(normalize_cells(line.cells()));
    }
    let size = snapshot.size();
    for row in 0..size.rows.get() {
        let mut cells = Vec::with_capacity(size.cols.get() as usize);
        for col in 0..size.cols.get() {
            let cell = snapshot
                .cell(termnix::Position { row, col })
                .expect("cell in range");
            cells.push(cell);
        }
        rows.push(normalize_cells(&cells));
    }
    rows
}

fn normalize_cells(cells: &[termnix::Cell]) -> String {
    let mut line = String::new();
    for cell in cells {
        if cell.width == 0 {
            continue;
        }
        line.push(cell.ch);
    }
    line.trim_end().to_string()
}

fn row_contains(rows: &[String], marker: &str) -> bool {
    rows.iter().any(|row| row.contains(marker))
}

#[derive(Debug)]
struct AppError {
    message: String,
    snapshot: Option<String>,
    status: Option<String>,
    shutdown: Option<String>,
}

impl AppError {
    fn msg(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
            snapshot: None,
            status: None,
            shutdown: None,
        }
    }

    fn io(err: io::Error) -> Self {
        Self::msg(format!("I/O error: {err}"))
    }

    fn protocol(message: impl Into<String>, session: &termnix::Session) -> Self {
        let mut err = Self::msg(message);
        err.attach_session(session);
        err
    }

    fn timeout(message: impl Into<String>, session: &termnix::Session) -> Self {
        Self::protocol(message, session)
    }

    fn shutdown_only(err: io::Error) -> Self {
        Self {
            message: "shutdown failed".into(),
            snapshot: None,
            status: None,
            shutdown: Some(err.to_string()),
        }
    }

    fn set_shutdown(&mut self, err: io::Error) {
        self.shutdown = Some(err.to_string());
    }

    fn attach_session(&mut self, session: &termnix::Session) {
        let rows = visible_rows(session.terminal_state().snapshot());
        self.snapshot = Some(rows.join("\n"));
        self.status = Some(format!("{:?}", session.status()));
    }
}

impl std::fmt::Display for AppError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.message)?;
        if let Some(status) = &self.status {
            write!(f, "; status={status}")?;
        }
        if let Some(snapshot) = &self.snapshot {
            write!(f, "; snapshot=\n{snapshot}")?;
        }
        if let Some(shutdown) = &self.shutdown {
            write!(f, "; shutdown error: {shutdown}")?;
        }
        Ok(())
    }
}

impl std::error::Error for AppError {}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalize_cells_skips_continuation_and_trims() {
        let mut cells = vec![termnix::Cell::EMPTY; 4];
        cells[0].ch = 'a';
        cells[1].width = 0;
        cells[2].ch = 'b';
        cells[3].ch = ' ';
        assert_eq!(normalize_cells(&cells), "ab");
    }

    #[test]
    fn visible_rows_keep_row_boundaries() {
        let mut term = termnix::TerminalState::new(termnix::Size::new(2, 4).expect("size"));
        term.feed(b"ab\r\ncd");
        let rows = visible_rows(term.snapshot());
        assert!(rows.iter().any(|row| row == "ab"), "rows={rows:?}");
        assert!(rows.iter().any(|row| row == "cd"), "rows={rows:?}");
    }

    #[test]
    fn row_contains_matches_within_one_row_only() {
        let rows = vec!["READY".into(), "ECHO:hello".into()];
        assert!(row_contains(&rows, "READY"));
        assert!(!row_contains(&rows, "READYECHO"));
    }

    #[test]
    fn poll_timeout_is_non_negative_and_finite() {
        let now = Instant::now();
        let ms = poll_timeout_ms(now + Duration::from_millis(5), now + Duration::from_secs(1));
        assert!(ms >= 0);
        assert!(ms <= 1000);
    }

    #[test]
    fn default_style_is_available_for_cells() {
        let _ = termnix::Color::Default;
        let _ = termnix::Position { row: 0, col: 0 };
    }
}
