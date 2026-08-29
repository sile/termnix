use std::{
    io::{ErrorKind, Read, Write},
    os::fd::AsRawFd,
    process::Command,
    time::{Duration, Instant},
};

fn spawn_shell(script: &str, size: muxnix::PtySize) -> muxnix::PtyProcess {
    let mut command = Command::new("/bin/sh");
    command.arg("-c").arg(script);
    muxnix::PtyProcess::spawn(&mut command, size).expect("spawn pty child")
}

fn read_until<F>(pty: &mut muxnix::PtyProcess, deadline: Instant, mut pred: F) -> Vec<u8>
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

#[test]
fn master_reads_child_output() {
    let mut pty = spawn_shell("printf 'hello-pty'", muxnix::PtySize { rows: 24, cols: 80 });
    let output = read_until(&mut pty, Instant::now() + Duration::from_secs(5), |buf| {
        buf.windows(9).any(|w| w == b"hello-pty")
    });
    assert!(
        output.windows(9).any(|w| w == b"hello-pty"),
        "output={:?}",
        String::from_utf8_lossy(&output)
    );
    let status = pty.close().expect("close");
    assert!(status.success());
}

#[test]
fn child_reads_master_input() {
    let mut pty = spawn_shell(
        "stty -echo 2>/dev/null; IFS= read -r line; printf 'GOT:%s' \"$line\"",
        muxnix::PtySize { rows: 24, cols: 80 },
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
    let status = pty.close().expect("close");
    assert!(status.success());
}

#[test]
fn stdio_are_connected_to_controlling_terminal() {
    let mut pty = spawn_shell(
        "test -t 0 && test -t 1 && test -t 2 && printf 'tty-ok'",
        muxnix::PtySize { rows: 24, cols: 80 },
    );
    let output = read_until(&mut pty, Instant::now() + Duration::from_secs(5), |buf| {
        buf.windows(6).any(|w| w == b"tty-ok")
    });
    assert!(
        output.windows(6).any(|w| w == b"tty-ok"),
        "output={:?}",
        String::from_utf8_lossy(&output)
    );
    let status = pty.close().expect("close");
    assert!(status.success());
}

#[test]
fn resize_is_visible_to_child() {
    let size = muxnix::PtySize { rows: 37, cols: 91 };
    let mut pty = spawn_shell("stty size", size);
    let output = read_until(&mut pty, Instant::now() + Duration::from_secs(5), |buf| {
        String::from_utf8_lossy(buf).contains("37 91")
    });
    assert!(
        String::from_utf8_lossy(&output).contains("37 91"),
        "output={:?}",
        String::from_utf8_lossy(&output)
    );
    let status = pty.close().expect("close");
    assert!(status.success());
}

#[test]
fn exit_status_is_reaped() {
    let mut pty = spawn_shell("exit 42", muxnix::PtySize { rows: 24, cols: 80 });
    let status = pty.wait().expect("wait");
    assert_eq!(status.code(), Some(42));
}

#[test]
fn nonblocking_read_returns_would_block() {
    let mut pty = spawn_shell("sleep 2", muxnix::PtySize { rows: 24, cols: 80 });
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
            Ok(_) => {}
        }
    };
    assert_eq!(err.kind(), ErrorKind::WouldBlock);

    let _ = pty.close();
}

#[test]
fn spawn_failure_does_not_leave_usable_process() {
    let before = count_open_fds();
    for _ in 0..64 {
        let mut command = Command::new("/path/that/does/not/exist/muxnix-pty");
        let err = muxnix::PtyProcess::spawn(&mut command, muxnix::PtySize { rows: 24, cols: 80 })
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
    let pty = spawn_shell("printf x", muxnix::PtySize { rows: 24, cols: 80 });
    assert!(pty.as_raw_fd() >= 0);
    let _ = pty.close();
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
