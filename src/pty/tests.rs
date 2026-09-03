use std::{
    io::{ErrorKind, Read, Write},
    mem::MaybeUninit,
    os::{fd::AsRawFd, unix::process::ExitStatusExt},
    process::Command,
    time::{Duration, Instant},
};

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
    let size = Size::new(31, 97).expect("nonzero size");
    set_winsize(master.as_raw_fd(), size).expect("set winsize");

    let mut winsize = MaybeUninit::<libc::winsize>::uninit();
    check_libc_zero(unsafe {
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

#[test]
fn classify_waitid_no_event_when_si_pid_zero() {
    assert_eq!(
        classify_waitid_event(0, libc::CLD_EXITED, 0).expect("no event"),
        None
    );
}

#[test]
fn classify_waitid_exited_killed_dumped() {
    assert_eq!(
        classify_waitid_event(7, libc::CLD_EXITED, 42).expect("exited"),
        Some(ObservedExit::Exited { code: 42 })
    );
    assert_eq!(
        classify_waitid_event(7, libc::CLD_KILLED, libc::SIGTERM).expect("killed"),
        Some(ObservedExit::Signaled {
            signal: libc::SIGTERM,
            core_dumped: false
        })
    );
    assert_eq!(
        classify_waitid_event(7, libc::CLD_DUMPED, libc::SIGABRT).expect("dumped"),
        Some(ObservedExit::Signaled {
            signal: libc::SIGABRT,
            core_dumped: true
        })
    );
}

#[test]
fn classify_waitid_unknown_code_is_invalid_data() {
    let err = classify_waitid_event(1, 999, 0).expect_err("unknown code");
    assert_eq!(err.kind(), ErrorKind::InvalidData);
}

fn spawn_shell(script: &str, size: Size) -> PtyProcess {
    let mut command = Command::new("/bin/sh");
    command.arg("-c").arg(script);
    PtyProcess::spawn(&mut command, size).expect("spawn pty child")
}

fn read_until<F>(pty: &mut PtyProcess, deadline: Instant, mut pred: F) -> Vec<u8>
where
    F: FnMut(&[u8]) -> bool,
{
    let mut buf = Vec::new();
    let mut chunk = [0u8; 1024];
    while Instant::now() < deadline {
        match pty.read(&mut chunk) {
            Ok(0) => break,
            Ok(n) => {
                buf.extend_from_slice(&chunk[..n]);
                if pred(&buf) {
                    return buf;
                }
            }
            Err(err) if err.kind() == ErrorKind::Interrupted => continue,
            Err(err) if err.kind() == ErrorKind::WouldBlock => {
                std::thread::sleep(Duration::from_millis(10));
            }
            Err(err) => panic!("read failed: {err}"),
        }
    }
    panic!(
        "timed out waiting for PTY output; got: {:?}",
        String::from_utf8_lossy(&buf)
    );
}

fn force_reap(closing: &mut ClosingPtyProcess) {
    let _ = closing.signal_kill();
    let status = closing.wait_and_reap();
    assert!(
        status.is_ok(),
        "force reap failed: {:?}",
        status.err().map(|e| e.to_string())
    );
}

fn poll_until_exit(closing: &mut ClosingPtyProcess, deadline: Instant) -> ObservedExit {
    while Instant::now() < deadline {
        match closing.poll_exit() {
            Ok(Some(exit)) => return exit,
            Ok(None) => std::thread::sleep(Duration::from_millis(10)),
            Err(err) => panic!("poll_exit failed: {err}"),
        }
    }
    panic!("timed out waiting for ObservedExit");
}

#[test]
fn master_reads_child_output() {
    let mut pty = spawn_shell(
        "printf 'hello-pty'",
        Size::new(24, 80).expect("nonzero size"),
    );
    let output = read_until(&mut pty, Instant::now() + Duration::from_secs(5), |buf| {
        buf.windows(9).any(|w| w == b"hello-pty")
    });
    assert!(
        output.windows(9).any(|w| w == b"hello-pty"),
        "output={:?}",
        String::from_utf8_lossy(&output)
    );
    // Wait before dropping the master. Closing the master while the child is
    // still exiting can deliver SIGHUP and make a successful script look failed.
    let status = pty.wait().expect("wait");
    assert!(status.success(), "status={status:?}");
}

#[test]
fn child_reads_master_input() {
    let mut pty = spawn_shell(
        "stty -echo 2>/dev/null; IFS= read -r line; printf 'GOT:%s' \"$line\"",
        Size::new(24, 80).expect("nonzero size"),
    );
    pty.write_all(b"ping-input\n").expect("write");
    pty.flush().expect("flush");

    let output = read_until(&mut pty, Instant::now() + Duration::from_secs(5), |buf| {
        buf.windows(14).any(|w| w == b"GOT:ping-input")
    });
    assert!(
        output.windows(14).any(|w| w == b"GOT:ping-input"),
        "output={:?}",
        String::from_utf8_lossy(&output)
    );
    let status = pty.wait().expect("wait");
    assert!(status.success(), "status={status:?}");
}

#[test]
fn stdio_are_connected_to_controlling_terminal() {
    let mut pty = spawn_shell(
        "test -t 0 && test -t 1 && test -t 2 && printf 'tty-ok'",
        Size::new(24, 80).expect("nonzero size"),
    );
    let output = read_until(&mut pty, Instant::now() + Duration::from_secs(5), |buf| {
        buf.windows(6).any(|w| w == b"tty-ok")
    });
    assert!(
        output.windows(6).any(|w| w == b"tty-ok"),
        "output={:?}",
        String::from_utf8_lossy(&output)
    );
    let status = pty.wait().expect("wait");
    assert!(status.success(), "status={status:?}");
}

#[test]
fn resize_is_visible_to_child() {
    let size = Size::new(37, 91).expect("nonzero size");
    let mut pty = spawn_shell("stty size", size);
    let output = read_until(&mut pty, Instant::now() + Duration::from_secs(5), |buf| {
        String::from_utf8_lossy(buf).contains("37 91")
    });
    assert!(
        String::from_utf8_lossy(&output).contains("37 91"),
        "output={:?}",
        String::from_utf8_lossy(&output)
    );
    let status = pty.wait().expect("wait");
    assert!(status.success(), "status={status:?}");
}

#[test]
fn exit_status_is_reaped() {
    let mut pty = spawn_shell("exit 42", Size::new(24, 80).expect("nonzero size"));
    let status = pty.wait().expect("wait");
    assert_eq!(status.code(), Some(42));
}

#[test]
fn nonblocking_read_returns_would_block() {
    let mut pty = spawn_shell(
        "trap '' HUP; printf READY; sleep 30",
        Size::new(24, 80).expect("nonzero size"),
    );
    pty.set_nonblocking(true).expect("set nonblocking");

    let mut buf = [0u8; 16];
    let deadline = Instant::now() + Duration::from_secs(2);
    let err = loop {
        if Instant::now() >= deadline {
            panic!("timed out waiting for WouldBlock");
        }
        match pty.read(&mut buf) {
            Err(err) if err.kind() == ErrorKind::Interrupted => continue,
            Err(err) if err.kind() == ErrorKind::WouldBlock => break err,
            Err(err) => panic!("unexpected read error: {err}"),
            Ok(0) => panic!("unexpected EOF before WouldBlock"),
            Ok(n) if buf[..n].windows(5).any(|w| w == b"READY") => {
                // Marker arrived; keep reading until WouldBlock or continue.
            }
            Ok(_) => {}
        }
    };
    assert_eq!(err.kind(), ErrorKind::WouldBlock);

    let mut closing = pty.into_closing();
    force_reap(&mut closing);
}

#[test]
fn spawn_failure_does_not_leave_usable_process() {
    let before = count_open_fds();
    for _ in 0..64 {
        let mut command = Command::new("/path/that/does/not/exist/termnix-pty");
        let err = PtyProcess::spawn(&mut command, Size::new(24, 80).expect("nonzero size"))
            .expect_err("spawn should fail");
        assert_eq!(err.kind(), ErrorKind::NotFound);
    }
    let after = count_open_fds();
    assert!(
        after <= before + 2,
        "fd leak suspected: before={before}, after={after}"
    );
}

#[test]
fn as_raw_fd_matches_master() {
    let mut pty = spawn_shell(
        "trap '' HUP; printf x; sleep 30",
        Size::new(24, 80).expect("nonzero size"),
    );
    assert!(pty.as_raw_fd() >= 0);
    let _ = read_until(&mut pty, Instant::now() + Duration::from_secs(5), |buf| {
        buf.contains(&b'x')
    });
    let mut closing = pty.into_closing();
    force_reap(&mut closing);
}

#[test]
fn into_closing_returns_without_waiting_for_running_child() {
    let mut pty = spawn_shell(
        "trap '' HUP; printf READY; while true; do sleep 1; done",
        Size::new(24, 80).expect("nonzero size"),
    );
    let _ = read_until(&mut pty, Instant::now() + Duration::from_secs(5), |buf| {
        buf.windows(5).any(|w| w == b"READY")
    });
    let mut closing = pty.into_closing();
    assert_eq!(closing.poll_exit().expect("poll"), None);
    force_reap(&mut closing);
}

#[test]
fn poll_exit_sees_natural_exit_without_reaping() {
    let mut pty = spawn_shell(
        "trap '' HUP; printf READY; exit 17",
        Size::new(24, 80).expect("nonzero size"),
    );
    let _ = read_until(&mut pty, Instant::now() + Duration::from_secs(5), |buf| {
        buf.windows(5).any(|w| w == b"READY")
    });
    let mut closing = pty.into_closing();
    let observed = poll_until_exit(&mut closing, Instant::now() + Duration::from_secs(5));
    assert_eq!(observed, ObservedExit::Exited { code: 17 });
    // Repeated poll must not reap; wait_and_reap still works.
    assert_eq!(
        closing.poll_exit().expect("poll again"),
        Some(ObservedExit::Exited { code: 17 })
    );
    let status = closing.wait_and_reap().expect("reap");
    assert_eq!(status.code(), Some(17));
    assert_eq!(
        closing.poll_exit().expect("poll after reap"),
        Some(ObservedExit::Exited { code: 17 })
    );
    let status_again = closing.wait_and_reap().expect("cached reap");
    assert_eq!(status_again.code(), Some(17));
}

#[test]
fn signal_terminate_reaches_process_group() {
    let mut pty = spawn_shell(
        "trap '' HUP; printf READY; sleep 60",
        Size::new(24, 80).expect("nonzero size"),
    );
    let _ = read_until(&mut pty, Instant::now() + Duration::from_secs(5), |buf| {
        buf.windows(5).any(|w| w == b"READY")
    });
    let mut closing = pty.into_closing();
    assert_eq!(
        closing.signal_terminate().expect("term"),
        SignalOutcome::Sent
    );
    let observed = poll_until_exit(&mut closing, Instant::now() + Duration::from_secs(5));
    match observed {
        ObservedExit::Signaled { signal, .. } => assert_eq!(signal, libc::SIGTERM),
        other => panic!("expected Signaled SIGTERM, got {other:?}"),
    }
    let status = closing.wait_and_reap().expect("reap");
    assert_eq!(status.signal(), Some(libc::SIGTERM));
}

#[test]
fn signal_kill_reaps_term_ignoring_child() {
    let mut pty = spawn_shell(
        "trap '' HUP TERM; printf READY; while true; do sleep 1; done",
        Size::new(24, 80).expect("nonzero size"),
    );
    let _ = read_until(&mut pty, Instant::now() + Duration::from_secs(5), |buf| {
        buf.windows(5).any(|w| w == b"READY")
    });
    let mut closing = pty.into_closing();
    assert_eq!(
        closing.signal_terminate().expect("term"),
        SignalOutcome::Sent
    );
    std::thread::sleep(Duration::from_millis(100));
    assert_eq!(closing.poll_exit().expect("still running"), None);
    assert_eq!(closing.signal_kill().expect("kill"), SignalOutcome::Sent);
    let observed = poll_until_exit(&mut closing, Instant::now() + Duration::from_secs(5));
    match observed {
        ObservedExit::Signaled { signal, .. } => assert_eq!(signal, libc::SIGKILL),
        other => panic!("expected Signaled SIGKILL, got {other:?}"),
    }
    let status = closing.wait_and_reap().expect("reap");
    assert_eq!(status.signal(), Some(libc::SIGKILL));
    assert_eq!(
        closing.signal_kill().expect("after reap"),
        SignalOutcome::AlreadyReaped
    );
}

#[test]
fn try_wait_cache_moves_into_reaped_closing_state() {
    let mut pty = spawn_shell("exit 9", Size::new(24, 80).expect("nonzero size"));
    let deadline = Instant::now() + Duration::from_secs(5);
    let status = loop {
        if Instant::now() >= deadline {
            panic!("timed out waiting for exit");
        }
        match pty.try_wait().expect("try_wait") {
            Some(status) => break status,
            None => std::thread::sleep(Duration::from_millis(10)),
        }
    };
    assert_eq!(status.code(), Some(9));
    let cached = pty.try_wait().expect("cached try_wait").expect("some");
    assert_eq!(cached.code(), Some(9));
    let mut closing = pty.into_closing();
    assert_eq!(
        closing.poll_exit().expect("poll"),
        Some(ObservedExit::Exited { code: 9 })
    );
    assert_eq!(
        closing.signal_terminate().expect("signal"),
        SignalOutcome::AlreadyReaped
    );
    assert_eq!(closing.wait_and_reap().expect("reap").code(), Some(9));
}

#[test]
fn signal_race_with_natural_exit_keeps_ownership() {
    let mut pty = spawn_shell(
        "trap '' HUP; printf READY; exit 0",
        Size::new(24, 80).expect("nonzero size"),
    );
    let _ = read_until(&mut pty, Instant::now() + Duration::from_secs(5), |buf| {
        buf.windows(5).any(|w| w == b"READY")
    });
    let mut closing = pty.into_closing();
    // Race: signal may run before or after the child becomes a zombie.
    let outcome = closing.signal_terminate();
    match outcome {
        Ok(SignalOutcome::Sent | SignalOutcome::GroupMissing) => {}
        // macOS may return EPERM against a zombie process group.
        Err(err) if err.kind() == ErrorKind::PermissionDenied => {}
        other => panic!("unexpected signal outcome: {other:?}"),
    }
    let status = closing.wait_and_reap().expect("reap keeps ownership");
    assert!(
        status.success() || status.signal() == Some(libc::SIGTERM),
        "status={status:?}"
    );
}

fn count_open_fds() -> usize {
    #[cfg(target_os = "linux")]
    {
        std::fs::read_dir("/proc/self/fd")
            .map(|entries| entries.count())
            .unwrap_or(0)
    }
    #[cfg(target_os = "macos")]
    {
        let mut count = 0;
        for fd in 0..1024 {
            let flags = unsafe { libc::fcntl(fd, libc::F_GETFD) };
            if flags >= 0 {
                count += 1;
            }
        }
        count
    }
    #[cfg(not(any(target_os = "linux", target_os = "macos")))]
    {
        0
    }
}
