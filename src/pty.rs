//! PTY pair creation and child process lifecycle.
//!
//! This module is crate-internal. [`Session`](`crate::Session`) owns these
//! types and exposes the lifecycle through its public API. The master file
//! descriptor can be registered with an external event loop, the terminal can
//! be resized, ownership can move into a closing state that keeps only the
//! child handle, exit can be observed without reaping, the original process
//! group can be signaled, and the direct child can be reaped without blocking
//! in [`Drop`].

use std::{
    fs::File,
    io::{self, Error, ErrorKind, Read, Write},
    os::{
        fd::{AsRawFd, FromRawFd, RawFd},
        unix::process::{CommandExt, ExitStatusExt},
    },
    process::{Child, Command, ExitStatus},
};

use crate::size::Size;

/// A child process attached to a PTY.
///
/// The master side is owned by this type and can be read, written, resized, and
/// registered with an external event loop via [`AsRawFd`].
///
/// # Drop behavior
///
/// Dropping a [`PtyProcess`] closes the master file descriptor, but does **not**
/// wait for or signal the child. Callers should use [`PtyProcess::try_wait`],
/// [`PtyProcess::wait`], or [`PtyProcess::into_closing`] followed by
/// [`ClosingPtyProcess::wait_and_reap`] to reap the child. An unreaped child may
/// become a zombie until the parent process exits or later waits on it.
///
/// This crate is the sole waiter for the owned [`Child`]. Do not set
/// process-global `SIGCHLD` to `SIG_IGN` or `SA_NOCLDWAIT` while a
/// [`PtyProcess`] or [`ClosingPtyProcess`] is live.
#[derive(Debug)]
pub(crate) struct PtyProcess {
    master: File,
    child: Child,
    reaped_status: Option<ExitStatus>,
}

impl PtyProcess {
    /// Spawns `command` with its standard streams attached to a new PTY.
    ///
    /// The child becomes a session leader and the PTY slave is set as its
    /// controlling terminal before `exec`. The initial window size is applied
    /// to the PTY before the child starts. After a successful spawn, the child
    /// PID equals its process group ID (from `setsid` in the child).
    ///
    /// On failure, opened PTY file descriptors are closed before the error is
    /// returned.
    #[expect(
        unsafe_code,
        reason = "CommandExt::pre_exec is unsafe; it runs setup_child_session in the forked child"
    )]
    pub(crate) fn spawn(command: &mut Command, size: Size) -> io::Result<Self> {
        let (master, slave) = open_pty_pair()?;
        set_cloexec(master.as_raw_fd())?;
        set_cloexec(slave.as_raw_fd())?;
        set_winsize(master.as_raw_fd(), size)?;

        let master_fd = master.as_raw_fd();
        let slave_fd = slave.as_raw_fd();

        // SAFETY: The closure only performs async-signal-safe system calls and
        // returns errors through `io::Result`. The captured raw fds remain open
        // until this closure runs in the child (or spawn fails and the parent
        // `File` values are dropped).
        unsafe {
            command.pre_exec(move || setup_child_session(master_fd, slave_fd));
        }

        // On spawn failure, `master` and `slave` are closed by `File`'s `Drop`
        // as this function returns. Keep both locals scoped here so a later
        // refactor cannot move them into a longer-lived value on the error path.
        let child = command.spawn()?;

        // The parent retains only the master. `slave` is not moved into
        // `PtyProcess`, so leaving this function closes it and lets the parent
        // observe EOF once the child exits and closes its stdio.
        Ok(Self {
            master,
            child,
            reaped_status: None,
        })
    }

    /// Sets whether master I/O is non-blocking.
    pub(crate) fn set_nonblocking(&mut self, nonblocking: bool) -> io::Result<()> {
        set_nonblocking(self.master.as_raw_fd(), nonblocking)
    }

    /// Resizes the PTY and notifies the child via `TIOCSWINSZ`.
    pub(crate) fn resize(&self, size: Size) -> io::Result<()> {
        set_winsize(self.master.as_raw_fd(), size)
    }

    /// Checks whether the child has exited without blocking.
    ///
    /// A successful status is cached on this wrapper. Later calls return the
    /// cached value and do not wait on the OS again.
    pub(crate) fn try_wait(&mut self) -> io::Result<Option<ExitStatus>> {
        if let Some(status) = self.reaped_status {
            return Ok(Some(status));
        }
        match self.child.try_wait()? {
            Some(status) => {
                self.reaped_status = Some(status);
                Ok(Some(status))
            }
            None => Ok(None),
        }
    }

    /// Blocks until the child exits and returns its status.
    ///
    /// A successful status is cached on this wrapper. Later calls return the
    /// cached value and do not wait on the OS again.
    pub(crate) fn wait(&mut self) -> io::Result<ExitStatus> {
        if let Some(status) = self.reaped_status {
            return Ok(status);
        }
        let status = self.child.wait()?;
        self.reaped_status = Some(status);
        Ok(status)
    }

    /// Sends `signal` to the original process group (`kill(-pgid, signal)`).
    ///
    /// The child is a session leader created with `setsid`, so its PID equals
    /// its process group ID. No signal is sent once the child has been reaped.
    #[expect(unsafe_code, reason = "libc::kill signals the child's process group")]
    pub(crate) fn signal_group(&self, signal: libc::c_int) -> io::Result<SignalOutcome> {
        if self.reaped_status.is_some() {
            return Ok(SignalOutcome::AlreadyReaped);
        }
        let pgid = self.child.id() as libc::pid_t;
        // SAFETY: A negative pid selects the process group whose ID is `pgid`.
        // The child is a session leader, so its process group still exists as
        // long as the child has not been reaped.
        let rc = unsafe { libc::kill(-pgid, signal) };
        if rc == 0 {
            Ok(SignalOutcome::Sent)
        } else {
            let err = Error::last_os_error();
            if err.raw_os_error() == Some(libc::ESRCH) {
                Ok(SignalOutcome::GroupMissing)
            } else {
                Err(err)
            }
        }
    }

    /// Closes the master and transfers child ownership into a closing state.
    ///
    /// This does not wait for the child, does not send signals, and does not
    /// probe unreaped children with `try_wait` / `waitpid` / `waitid`. If the
    /// child was already reaped through [`PtyProcess::try_wait`] or
    /// [`PtyProcess::wait`], the closing value starts in the reaped state.
    pub(crate) fn into_closing(self) -> ClosingPtyProcess {
        let Self {
            master,
            child,
            reaped_status,
        } = self;
        drop(master);
        match reaped_status {
            Some(status) => ClosingPtyProcess {
                state: ClosingState::Reaped { status },
            },
            None => {
                let pgid = child.id() as libc::pid_t;
                ClosingPtyProcess {
                    state: ClosingState::Unreaped {
                        child,
                        pgid,
                        observed: None,
                        lifecycle_error: None,
                    },
                }
            }
        }
    }
}

impl Read for PtyProcess {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        self.master.read(buf)
    }
}

impl Write for PtyProcess {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        self.master.write(buf)
    }

    fn flush(&mut self) -> io::Result<()> {
        self.master.flush()
    }
}

impl AsRawFd for PtyProcess {
    fn as_raw_fd(&self) -> RawFd {
        self.master.as_raw_fd()
    }
}

/// Exit information observed without reaping the direct child.
///
/// Values come from `waitid` (`si_code` / `si_status`) or from a cached
/// [`ExitStatus`] after a successful reap. They are not waitpid raw status
/// words.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub(crate) enum ObservedExit {
    /// The child exited with `code`.
    Exited {
        /// Process exit code.
        code: i32,
    },
    /// The child was terminated by `signal`.
    Signaled {
        /// Terminating signal number.
        signal: i32,
        /// Whether a core dump was produced.
        core_dumped: bool,
    },
}

impl ObservedExit {
    fn from_exit_status(status: ExitStatus) -> Self {
        if let Some(code) = status.code() {
            Self::Exited { code }
        } else if let Some(signal) = status.signal() {
            Self::Signaled {
                signal,
                core_dumped: status.core_dumped(),
            }
        } else {
            // Fallback for unexpected ExitStatus shapes; treat as exit 0.
            Self::Exited { code: 0 }
        }
    }

    fn matches_status(self, status: ExitStatus) -> bool {
        self == Self::from_exit_status(status)
    }
}

/// Result of delivering a signal to the original process group.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum SignalOutcome {
    /// `kill(-pgid, signal)` returned success.
    Sent,
    /// `kill` returned `ESRCH` (no such process group).
    GroupMissing,
    /// The direct child was already reaped; no signal syscall was performed.
    AlreadyReaped,
    /// Direct-child ownership was lost (`ECHILD`); no signal syscall was
    /// performed.
    OwnershipLost,
}

/// Child ownership after the PTY master has been closed.
///
/// Dropping this type drops owned handles only. It does **not** send signals
/// or wait for the child. Call [`ClosingPtyProcess::wait_and_reap`] (and
/// typically [`ClosingPtyProcess::signal_terminate`] /
/// [`ClosingPtyProcess::signal_kill`] first) explicitly.
#[derive(Debug)]
pub(crate) struct ClosingPtyProcess {
    state: ClosingState,
}

#[derive(Debug)]
enum ClosingState {
    Unreaped {
        child: Child,
        pgid: libc::pid_t,
        observed: Option<ObservedExit>,
        lifecycle_error: Option<io::Error>,
    },
    Reaped {
        status: ExitStatus,
    },
    OwnershipLost {
        error: io::Error,
    },
}

impl ClosingPtyProcess {
    /// Observes whether the direct child has exited without reaping it.
    ///
    /// Uses `waitid(P_PID, ..., WEXITED | WNOHANG | WNOWAIT)`. Once an exit is
    /// observed it is cached and returned again on later calls. After a
    /// successful [`ClosingPtyProcess::wait_and_reap`], returns the
    /// classification of the cached [`ExitStatus`].
    pub(crate) fn poll_exit(&mut self) -> io::Result<Option<ObservedExit>> {
        match &mut self.state {
            ClosingState::Reaped { status } => Ok(Some(ObservedExit::from_exit_status(*status))),
            ClosingState::OwnershipLost { error } => Err(clone_io_error(error)),
            ClosingState::Unreaped {
                child,
                observed,
                lifecycle_error,
                ..
            } => {
                if let Some(err) = lifecycle_error {
                    return Err(clone_io_error(err));
                }
                if let Some(exit) = *observed {
                    return Ok(Some(exit));
                }
                match poll_exit_waitid(child.id() as libc::pid_t) {
                    Ok(Some(exit)) => {
                        *observed = Some(exit);
                        Ok(Some(exit))
                    }
                    Ok(None) => Ok(None),
                    Err(err) if err.raw_os_error() == Some(libc::ECHILD) => {
                        let lost = io::Error::other(format!("direct child ownership lost: {err}"));
                        self.state = ClosingState::OwnershipLost {
                            error: clone_io_error(&lost),
                        };
                        Err(lost)
                    }
                    Err(err) if err.kind() == ErrorKind::InvalidData => {
                        *lifecycle_error = Some(clone_io_error(&err));
                        Err(err)
                    }
                    Err(err) => Err(err),
                }
            }
        }
    }

    /// Reaps the direct child with [`Child::wait`].
    ///
    /// On success, later calls return the cached status without waiting again.
    /// On error (other than ownership loss), unreaped ownership is kept so the
    /// caller can retry. Temporary errors such as `EINTR` leave ownership
    /// intact.
    pub(crate) fn wait_and_reap(&mut self) -> io::Result<ExitStatus> {
        match &self.state {
            ClosingState::Reaped { status } => return Ok(*status),
            ClosingState::OwnershipLost { error } => return Err(clone_io_error(error)),
            ClosingState::Unreaped { .. } => {}
        }

        let prev = std::mem::replace(
            &mut self.state,
            ClosingState::OwnershipLost {
                error: io::Error::other("closing wait in progress"),
            },
        );
        let ClosingState::Unreaped {
            mut child,
            pgid,
            observed,
            lifecycle_error,
        } = prev
        else {
            self.state = prev;
            unreachable!("wait_and_reap: expected unreaped state");
        };

        if let Some(err) = lifecycle_error {
            self.state = ClosingState::Unreaped {
                child,
                pgid,
                observed,
                lifecycle_error: Some(clone_io_error(&err)),
            };
            return Err(err);
        }

        match child.wait() {
            Ok(status) => {
                if let Some(obs) = observed
                    && !obs.matches_status(status)
                {
                    self.state = ClosingState::Reaped { status };
                    return Err(io::Error::new(
                        ErrorKind::InvalidData,
                        "observed exit does not match reaped ExitStatus",
                    ));
                }
                self.state = ClosingState::Reaped { status };
                Ok(status)
            }
            Err(err) if err.raw_os_error() == Some(libc::ECHILD) => {
                let lost = io::Error::other(format!("direct child ownership lost: {err}"));
                self.state = ClosingState::OwnershipLost {
                    error: clone_io_error(&lost),
                };
                Err(lost)
            }
            Err(err) => {
                self.state = ClosingState::Unreaped {
                    child,
                    pgid,
                    observed,
                    lifecycle_error: None,
                };
                Err(err)
            }
        }
    }

    /// Sends `SIGTERM` to the original process group (`kill(-pgid, SIGTERM)`).
    pub(crate) fn signal_terminate(&mut self) -> io::Result<SignalOutcome> {
        self.signal_group(libc::SIGTERM)
    }

    /// Sends `SIGKILL` to the original process group (`kill(-pgid, SIGKILL)`).
    pub(crate) fn signal_kill(&mut self) -> io::Result<SignalOutcome> {
        self.signal_group(libc::SIGKILL)
    }

    #[expect(unsafe_code, reason = "libc::kill signals the child's process group")]
    fn signal_group(&mut self, signal: libc::c_int) -> io::Result<SignalOutcome> {
        match &self.state {
            ClosingState::Reaped { .. } => Ok(SignalOutcome::AlreadyReaped),
            ClosingState::OwnershipLost { .. } => Ok(SignalOutcome::OwnershipLost),
            ClosingState::Unreaped { pgid, .. } => {
                // SAFETY: `pgid` is the child's original process group ID,
                // captured when closing began and before any reap. Negative
                // pid selects that process group.
                let rc = unsafe { libc::kill(-*pgid, signal) };
                if rc == 0 {
                    Ok(SignalOutcome::Sent)
                } else {
                    let err = Error::last_os_error();
                    if err.raw_os_error() == Some(libc::ESRCH) {
                        Ok(SignalOutcome::GroupMissing)
                    } else {
                        Err(err)
                    }
                }
            }
        }
    }
}

fn clone_io_error(err: &io::Error) -> io::Error {
    io::Error::new(err.kind(), err.to_string())
}

/// Classifies a `waitid` result without performing a syscall.
///
/// Returns `Ok(None)` when `si_pid == 0` (no event). Unknown `si_code` values
/// become `Err` with [`ErrorKind::InvalidData`].
fn classify_waitid_event(
    si_pid: libc::pid_t,
    si_code: libc::c_int,
    si_status: libc::c_int,
) -> io::Result<Option<ObservedExit>> {
    if si_pid == 0 {
        return Ok(None);
    }
    match si_code {
        libc::CLD_EXITED => Ok(Some(ObservedExit::Exited { code: si_status })),
        libc::CLD_KILLED => Ok(Some(ObservedExit::Signaled {
            signal: si_status,
            core_dumped: false,
        })),
        libc::CLD_DUMPED => Ok(Some(ObservedExit::Signaled {
            signal: si_status,
            core_dumped: true,
        })),
        other => Err(io::Error::new(
            ErrorKind::InvalidData,
            format!("unknown waitid si_code {other}"),
        )),
    }
}

#[expect(unsafe_code, reason = "libc::waitid and siginfo_t access are unsafe")]
fn poll_exit_waitid(pid: libc::pid_t) -> io::Result<Option<ObservedExit>> {
    // Zero-initialize every call; kernels may only write parts of siginfo_t.
    let mut info: libc::siginfo_t = unsafe { std::mem::zeroed() };
    // SAFETY: `info` points to a valid zeroed siginfo_t for the duration of the
    // call. `P_PID` with `WNOHANG | WNOWAIT` does not reap the child.
    let rc = unsafe {
        libc::waitid(
            libc::P_PID,
            pid as libc::id_t,
            &mut info,
            libc::WEXITED | libc::WNOHANG | libc::WNOWAIT,
        )
    };
    if rc < 0 {
        return Err(Error::last_os_error());
    }
    // SAFETY: After a successful waitid, si_pid / si_code / si_status are the
    // fields the platform documents for child-exit events.
    let si_pid = unsafe { info.si_pid() };
    let si_code = info.si_code;
    let si_status = unsafe { info.si_status() };
    classify_waitid_event(si_pid, si_code, si_status)
}

/// Opens a PTY master/slave pair.
///
/// `openpty` is used instead of `posix_openpt` + `grantpt` + `unlockpt` +
/// `ptsname` because:
/// - it is available on both macOS and Linux through the `libc` crate
///   (Linux links `libutil`)
/// - it avoids `ptsname`, which is not thread-safe, and `ptsname_r`, which is
///   not exposed for macOS in the targeted `libc` bindings
///
/// Close-on-exec is applied afterward because `openpty` does not guarantee it.
#[expect(
    unsafe_code,
    reason = "libc::openpty and File::from_raw_fd take raw fds"
)]
fn open_pty_pair() -> io::Result<(File, File)> {
    let mut master = -1;
    let mut slave = -1;

    // SAFETY: `openpty` writes the resulting fds into the provided pointers.
    // Passing null for name/termios/winsize requests the defaults; the window
    // size is set afterward with `TIOCSWINSZ`.
    check_libc_zero(unsafe {
        libc::openpty(
            &mut master,
            &mut slave,
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            std::ptr::null_mut(),
        )
    })?;

    // SAFETY: `openpty` returned success, so both fds are open and uniquely
    // owned by this function until wrapped in `File`.
    let master = unsafe { File::from_raw_fd(master) };
    let slave = unsafe { File::from_raw_fd(slave) };
    Ok((master, slave))
}

/// Configures the child side of a PTY after `fork` and before `exec`.
///
/// This function must stay async-signal-safe: no allocation, locking, or
/// unwinding. After `setsid`, the child PID is the session ID and process
/// group ID.
#[expect(
    unsafe_code,
    reason = "called in the forked child; all calls are async-signal-safe libc syscalls"
)]
fn setup_child_session(master_fd: RawFd, slave_fd: RawFd) -> io::Result<()> {
    // SAFETY: Called only in the forked child before exec. `setsid` creates a
    // new session so the subsequent `TIOCSCTTY` can assign a controlling tty.
    check_libc_non_neg(unsafe { libc::setsid() })?;

    // SAFETY: `slave_fd` refers to the open PTY slave in this child.
    check_libc_non_neg(unsafe { libc::ioctl(slave_fd, libc::TIOCSCTTY as _, 0) })?;

    // SAFETY: Both descriptors are valid in the child at this point. In this
    // spawn path `slave_fd` is distinct from 0/1/2, so these calls always copy.
    check_libc_non_neg(unsafe { libc::dup2(slave_fd, 0) })?;
    check_libc_non_neg(unsafe { libc::dup2(slave_fd, 1) })?;
    check_libc_non_neg(unsafe { libc::dup2(slave_fd, 2) })?;

    // SAFETY: Closing fds that are not stdin/stdout/stderr is safe; those
    // descriptors remain open through the dup2 targets above.
    if slave_fd > 2 {
        unsafe {
            libc::close(slave_fd);
        }
    }
    if master_fd > 2 {
        unsafe {
            libc::close(master_fd);
        }
    }

    Ok(())
}

#[expect(unsafe_code, reason = "libc::ioctl sets the PTY window size")]
fn set_winsize(fd: RawFd, size: Size) -> io::Result<()> {
    let winsize = libc::winsize {
        ws_row: size.rows.get(),
        ws_col: size.cols.get(),
        ws_xpixel: 0,
        ws_ypixel: 0,
    };
    // SAFETY: `fd` is an open PTY descriptor owned by the caller; `winsize`
    // points to a valid local struct for the duration of the call.
    check_libc_zero(unsafe { libc::ioctl(fd, libc::TIOCSWINSZ as _, &winsize) })
}

#[expect(unsafe_code, reason = "libc::fcntl toggles the close-on-exec flag")]
fn set_cloexec(fd: RawFd) -> io::Result<()> {
    // SAFETY: `fd` is open and owned by the caller.
    let flags = check_libc_non_neg(unsafe { libc::fcntl(fd, libc::F_GETFD) })?;
    // SAFETY: Same fd; only updates the close-on-exec flag.
    check_libc_non_neg(unsafe { libc::fcntl(fd, libc::F_SETFD, flags | libc::FD_CLOEXEC) })?;
    Ok(())
}

#[expect(unsafe_code, reason = "libc::fcntl toggles the status flags")]
fn set_nonblocking(fd: RawFd, nonblocking: bool) -> io::Result<()> {
    // SAFETY: `fd` is open and owned by the caller.
    let flags = check_libc_non_neg(unsafe { libc::fcntl(fd, libc::F_GETFL) })?;
    let new_flags = if nonblocking {
        flags | libc::O_NONBLOCK
    } else {
        flags & !libc::O_NONBLOCK
    };
    // SAFETY: Same fd; only updates the status flags.
    check_libc_non_neg(unsafe { libc::fcntl(fd, libc::F_SETFL, new_flags) })?;
    Ok(())
}

/// Checks a libc return value where `0` means success.
fn check_libc_zero(result: libc::c_int) -> io::Result<()> {
    if result == 0 {
        Ok(())
    } else {
        Err(Error::last_os_error())
    }
}

/// Checks a libc return value where a negative value means failure.
fn check_libc_non_neg(result: libc::c_int) -> io::Result<libc::c_int> {
    if result < 0 {
        Err(Error::last_os_error())
    } else {
        Ok(result)
    }
}

#[cfg(test)]
mod tests;
