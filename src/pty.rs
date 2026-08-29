//! PTY pair creation and child process lifecycle.
//!
//! This module owns a PTY master/slave pair and the child process attached to
//! the slave side. Callers can register the master file descriptor with an
//! external event loop, resize the terminal, and reap the child without
//! blocking in [`Drop`].

use std::{
    fs::File,
    io::{self, Error, Read, Write},
    os::{
        fd::{AsRawFd, FromRawFd, RawFd},
        unix::process::CommandExt,
    },
    process::{Child, Command, ExitStatus},
};

/// Terminal size expressed in character cells.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct PtySize {
    /// Number of rows.
    pub rows: u16,
    /// Number of columns.
    pub cols: u16,
}

/// A child process attached to a PTY.
///
/// The master side is owned by this type and can be read, written, resized, and
/// registered with an external event loop via [`AsRawFd`].
///
/// # Drop behavior
///
/// Dropping a [`PtyProcess`] closes the master file descriptor if it is still
/// open, but does **not** wait for the child. Callers should use
/// [`PtyProcess::try_wait`], [`PtyProcess::wait`], or [`PtyProcess::close`] to
/// reap the child. An unreaped child may become a zombie until the parent
/// process exits or later waits on it.
#[derive(Debug)]
pub struct PtyProcess {
    master: Option<File>,
    child: Option<Child>,
}

impl PtyProcess {
    /// Spawns `command` with its standard streams attached to a new PTY.
    ///
    /// The child becomes a session leader and the PTY slave is set as its
    /// controlling terminal before `exec`. The initial window size is applied
    /// to the PTY before the child starts.
    ///
    /// On failure, opened PTY file descriptors are closed before the error is
    /// returned.
    pub fn spawn(command: &mut Command, size: PtySize) -> io::Result<Self> {
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

        let child = match command.spawn() {
            Ok(child) => child,
            Err(err) => {
                // Explicitly drop both ends so spawn failures never leak fds.
                drop(master);
                drop(slave);
                return Err(err);
            }
        };

        // The parent only keeps the master; closing the slave lets us observe
        // EOF when the child exits and closes its stdio.
        drop(slave);

        Ok(Self {
            master: Some(master),
            child: Some(child),
        })
    }

    /// Sets whether master I/O is non-blocking.
    pub fn set_nonblocking(&mut self, nonblocking: bool) -> io::Result<()> {
        set_nonblocking(self.master_fd()?, nonblocking)
    }

    /// Resizes the PTY and notifies the child via `TIOCSWINSZ`.
    pub fn resize(&self, size: PtySize) -> io::Result<()> {
        set_winsize(self.master_fd()?, size)
    }

    /// Checks whether the child has exited without blocking.
    pub fn try_wait(&mut self) -> io::Result<Option<ExitStatus>> {
        let child = self
            .child
            .as_mut()
            .ok_or_else(|| Error::other("child process has already been closed"))?;
        child.try_wait()
    }

    /// Blocks until the child exits and returns its status.
    pub fn wait(&mut self) -> io::Result<ExitStatus> {
        let child = self
            .child
            .as_mut()
            .ok_or_else(|| Error::other("child process has already been closed"))?;
        child.wait()
    }

    /// Closes the master file descriptor and waits for the child to exit.
    ///
    /// Both operations happen at most once for a given [`PtyProcess`].
    pub fn close(mut self) -> io::Result<ExitStatus> {
        self.master.take();
        let mut child = self
            .child
            .take()
            .ok_or_else(|| Error::other("child process has already been closed"))?;
        child.wait()
    }

    fn master_file(&mut self) -> io::Result<&mut File> {
        self.master
            .as_mut()
            .ok_or_else(|| Error::other("master file descriptor has already been closed"))
    }

    fn master_fd(&self) -> io::Result<RawFd> {
        self.master
            .as_ref()
            .map(AsRawFd::as_raw_fd)
            .ok_or_else(|| Error::other("master file descriptor has already been closed"))
    }
}

impl Read for PtyProcess {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        self.master_file()?.read(buf)
    }
}

impl Write for PtyProcess {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        self.master_file()?.write(buf)
    }

    fn flush(&mut self) -> io::Result<()> {
        self.master_file()?.flush()
    }
}

impl AsRawFd for PtyProcess {
    fn as_raw_fd(&self) -> RawFd {
        self.master
            .as_ref()
            .map(AsRawFd::as_raw_fd)
            .expect("master file descriptor has already been closed")
    }
}

impl Drop for PtyProcess {
    fn drop(&mut self) {
        // Close the master fd if the caller has not already closed it via
        // `close`. Do not wait for the child here; waiting could block Drop.
        self.master.take();
    }
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
fn open_pty_pair() -> io::Result<(File, File)> {
    let mut master = -1;
    let mut slave = -1;

    // SAFETY: `openpty` writes the resulting fds into the provided pointers.
    // Passing null for name/termios/winsize requests the defaults; the window
    // size is set afterward with `TIOCSWINSZ`.
    check_libc_result(unsafe {
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
/// unwinding.
fn setup_child_session(master_fd: RawFd, slave_fd: RawFd) -> io::Result<()> {
    // SAFETY: Called only in the forked child before exec. `setsid` creates a
    // new session so the subsequent `TIOCSCTTY` can assign a controlling tty.
    if unsafe { libc::setsid() } == -1 {
        return Err(Error::last_os_error());
    }

    // SAFETY: `slave_fd` refers to the open PTY slave in this child.
    if unsafe { libc::ioctl(slave_fd, libc::TIOCSCTTY as _, 0) } == -1 {
        return Err(Error::last_os_error());
    }

    dup2_or_close(slave_fd, 0)?;
    dup2_or_close(slave_fd, 1)?;
    dup2_or_close(slave_fd, 2)?;

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

fn dup2_or_close(src: RawFd, dst: RawFd) -> io::Result<()> {
    if src == dst {
        return Ok(());
    }
    // SAFETY: Both descriptors are valid in the child at this point.
    if unsafe { libc::dup2(src, dst) } == -1 {
        return Err(Error::last_os_error());
    }
    Ok(())
}

fn set_winsize(fd: RawFd, size: PtySize) -> io::Result<()> {
    let winsize = libc::winsize {
        ws_row: size.rows,
        ws_col: size.cols,
        ws_xpixel: 0,
        ws_ypixel: 0,
    };
    // SAFETY: `fd` is an open PTY descriptor owned by the caller; `winsize`
    // points to a valid local struct for the duration of the call.
    check_libc_result(unsafe { libc::ioctl(fd, libc::TIOCSWINSZ as _, &winsize) })
}

fn set_cloexec(fd: RawFd) -> io::Result<()> {
    // SAFETY: `fd` is open and owned by the caller.
    let flags = unsafe { libc::fcntl(fd, libc::F_GETFD) };
    if flags < 0 {
        return Err(Error::last_os_error());
    }
    // SAFETY: Same fd; only updates the close-on-exec flag.
    if unsafe { libc::fcntl(fd, libc::F_SETFD, flags | libc::FD_CLOEXEC) } < 0 {
        return Err(Error::last_os_error());
    }
    Ok(())
}

fn set_nonblocking(fd: RawFd, nonblocking: bool) -> io::Result<()> {
    // SAFETY: `fd` is open and owned by the caller.
    let flags = unsafe { libc::fcntl(fd, libc::F_GETFL) };
    if flags < 0 {
        return Err(Error::last_os_error());
    }
    let new_flags = if nonblocking {
        flags | libc::O_NONBLOCK
    } else {
        flags & !libc::O_NONBLOCK
    };
    // SAFETY: Same fd; only updates the status flags.
    if unsafe { libc::fcntl(fd, libc::F_SETFL, new_flags) } < 0 {
        return Err(Error::last_os_error());
    }
    Ok(())
}

fn check_libc_result(result: libc::c_int) -> io::Result<()> {
    if result == 0 {
        Ok(())
    } else {
        Err(Error::last_os_error())
    }
}

#[cfg(test)]
mod tests {
    use std::{mem::MaybeUninit, os::fd::AsRawFd};

    use super::*;

    #[test]
    fn open_pty_pair_returns_distinct_fds() {
        let (master, slave) = open_pty_pair().expect("open pty");
        assert_ne!(master.as_raw_fd(), slave.as_raw_fd());
        assert!(master.as_raw_fd() >= 0);
        assert!(slave.as_raw_fd() >= 0);
    }

    #[test]
    fn set_winsize_round_trips_on_master() {
        let (master, _slave) = open_pty_pair().expect("open pty");
        let size = PtySize { rows: 31, cols: 97 };
        set_winsize(master.as_raw_fd(), size).expect("set winsize");

        let mut winsize = MaybeUninit::<libc::winsize>::uninit();
        check_libc_result(unsafe {
            libc::ioctl(
                master.as_raw_fd(),
                libc::TIOCGWINSZ as _,
                winsize.as_mut_ptr(),
            )
        })
        .expect("get winsize");
        let winsize = unsafe { winsize.assume_init() };
        assert_eq!(winsize.ws_row, 31);
        assert_eq!(winsize.ws_col, 97);
    }
}
