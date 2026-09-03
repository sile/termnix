use std::{
    io::{self, ErrorKind},
    os::unix::process::ExitStatusExt,
    process::Command,
    time::{Duration, Instant},
};

const DEADLINE: Duration = Duration::from_secs(15);

fn spawn_session(script: &str) -> termnix::Session {
    let mut command = Command::new("/bin/sh");
    command.arg("-c").arg(script);
    termnix::Session::new(
        &mut command,
        termnix::Size::new(24, 80).expect("default size"),
    )
    .expect("create session")
}

/// Renders the snapshot's scrollback and visible cells as rows, dropping
/// wide-character continuation cells and trailing blanks.
fn visible_text(session: &termnix::Session) -> String {
    let snapshot = session.snapshot();
    let mut out = String::new();
    for line in snapshot.scrollback() {
        let mut row = String::new();
        for cell in line.cells() {
            if cell.width == 0 {
                continue;
            }
            row.push(cell.ch);
        }
        out.push_str(row.trim_end());
        out.push('\n');
    }
    for row in 0..snapshot.size().rows.get() {
        let mut line = String::new();
        for col in 0..snapshot.size().cols.get() {
            let cell = snapshot
                .cell(termnix::Position { row, col })
                .expect("cell in range");
            if cell.width == 0 {
                continue;
            }
            line.push(cell.ch);
        }
        out.push_str(line.trim_end());
        out.push('\n');
    }
    out
}

/// Pumps every session once and drains each `needs_pump` loop.
fn pump_all(sessions: &mut [termnix::Session]) {
    for session in sessions.iter_mut() {
        session.pump_io().expect("pump");
        while session.needs_pump() {
            session.pump_io().expect("pump");
        }
    }
}

/// Pumps every session and polls their registered fds until `cond` holds or
/// the deadline expires. `cond` receives the sessions mutably so tests can
/// reap or enqueue from inside it.
fn pump_until<F>(sessions: &mut [termnix::Session], mut cond: F)
where
    F: FnMut(&mut [termnix::Session]) -> bool,
{
    let deadline = Instant::now() + DEADLINE;
    while Instant::now() < deadline {
        pump_all(sessions);
        if cond(sessions) {
            return;
        }
        let mut pollfds = Vec::new();
        for session in sessions.iter() {
            if let Some(fd) = session.fd() {
                let interests = session.interests();
                let mut events = 0;
                if interests.readable {
                    events |= libc::POLLIN;
                }
                if interests.writable {
                    events |= libc::POLLOUT;
                }
                pollfds.push(libc::pollfd {
                    fd,
                    events,
                    revents: 0,
                });
            }
        }
        if pollfds.is_empty() {
            std::thread::sleep(Duration::from_millis(10));
            continue;
        }
        let rc = unsafe { libc::poll(pollfds.as_mut_ptr(), pollfds.len() as libc::nfds_t, 10) };
        if rc < 0 {
            let err = io::Error::last_os_error();
            if err.kind() == ErrorKind::Interrupted {
                continue;
            }
            panic!("poll failed: {err}");
        }
    }
    panic!("timed out waiting for condition");
}

/// Pumps until the session's child has exited and returns its status.
fn wait_exit(session: &mut termnix::Session) -> std::process::ExitStatus {
    let deadline = Instant::now() + DEADLINE;
    loop {
        pump_all(std::slice::from_mut(session));
        if let Some(status) = session.try_wait().expect("try_wait") {
            return status;
        }
        if Instant::now() > deadline {
            panic!("timed out waiting for exit");
        }
        std::thread::sleep(Duration::from_millis(10));
    }
}

#[test]
fn decodes_child_output_into_terminal_state() {
    let mut session = spawn_session("printf 'hello-session\\n'");
    pump_until(std::slice::from_mut(&mut session), |sessions| {
        visible_text(&sessions[0]).contains("hello-session")
            && sessions[0].status() == termnix::SessionStatus::Eof
    });
    let status = wait_exit(&mut session);
    assert!(status.success(), "status={status:?}");
}

#[test]
fn input_routes_to_the_target_session_only() {
    let mut a =
        spawn_session("stty -echo; while IFS= read -r line; do printf 'A:%s\\n' \"$line\"; done");
    let mut b =
        spawn_session("stty -echo; while IFS= read -r line; do printf 'B:%s\\n' \"$line\"; done");

    a.enqueue_input(b"hello-a\n").expect("enqueue a");
    b.enqueue_input(b"hello-b\n").expect("enqueue b");

    let mut sessions = [a, b];
    pump_until(&mut sessions, |sessions| {
        let text_a = visible_text(&sessions[0]);
        let text_b = visible_text(&sessions[1]);
        text_a.contains("A:hello-a")
            && text_b.contains("B:hello-b")
            && !text_a.contains("B:hello-b")
            && !text_b.contains("A:hello-a")
    });
}

#[test]
fn key_text_paste_and_raw_reach_the_child() {
    // Capture raw bytes received by the child so all enqueue paths are
    // observable. The child switches to raw mode and signals readiness before
    // we enqueue, so no line-discipline translation (CR->LF) applies.
    let mut session = spawn_session(
        "stty raw -echo; printf 'READY\\n'; r=$(dd bs=1 count=9 2>/dev/null | od -An -v -tu1); printf 'GOT:%s\\n' \"$r\"",
    );
    pump_until(std::slice::from_mut(&mut session), |sessions| {
        visible_text(&sessions[0]).contains("READY")
    });
    let modes = session.terminal_state().modes();
    session.enqueue_input(b"AB").expect("text");
    session
        .enqueue_input(&termnix::encode_key(
            termnix::KeyEvent::new(termnix::KeyCode::Enter),
            modes,
        ))
        .expect("enter");
    session
        .enqueue_input(&termnix::encode_paste("CD", modes))
        .expect("paste");
    session.enqueue_input(b"EFGH").expect("raw");

    pump_until(std::slice::from_mut(&mut session), |sessions| {
        visible_text(&sessions[0]).contains("GOT:")
    });
    let text = visible_text(&session);
    // A B \r C D E F G H => 65 66 13 67 68 69 70 71 72.
    let normalized: String = text.split_whitespace().collect();
    assert!(
        normalized.contains("GOT:656613676869707172"),
        "captured bytes mismatch, text={text:?}"
    );
}

#[test]
fn query_reply_returns_to_the_querying_session() {
    // Send a primary DA request, read the 5-byte reply, and render its bytes
    // as decimal values so the reply content is observable in the snapshot.
    // Non-canonical mode is required because the reply contains no newline.
    let mut session = spawn_session(
        "stty -echo -icanon min 1 time 0; printf '\\033[c'; r=$(dd bs=1 count=5 2>/dev/null | od -An -tu1); printf 'REPLY:%s\\n' \"$r\"",
    );
    pump_until(std::slice::from_mut(&mut session), |sessions| {
        visible_text(&sessions[0]).contains("REPLY:")
    });
    let text = visible_text(&session);
    // ESC [ ? 6 c => 27 91 63 54 99. `od` pads columns with spaces, so compare
    // with all whitespace removed.
    let normalized: String = text.split_whitespace().collect();
    assert!(
        normalized.contains("REPLY:2791635499"),
        "reply mismatch, text={text:?}"
    );
}

#[test]
fn input_and_reply_keep_fifo_order() {
    // Enqueue "ABT": the child's trigger read consumes "A", so "BT" is the
    // A-part accepted before the reply. R = the 5-byte DA reply. B = "CD"
    // accepted after. The child emits the query only after reading the
    // trigger and prints Q_SENT after the query, so R is generated strictly
    // after A is accepted and the child observes A -> R -> B on the wire:
    // 66 84 27 91 63 54 99 67 68.
    let mut session = spawn_session(
        "stty raw -echo; printf 'READY\\n'; dd bs=1 count=1 >/dev/null 2>&1; printf '\\033[c'; printf 'Q_SENT\\n'; r=$(dd bs=1 count=9 2>/dev/null | od -An -v -tu1); printf 'GOT:%s\\n' \"$r\"",
    );
    pump_until(std::slice::from_mut(&mut session), |sessions| {
        visible_text(&sessions[0]).contains("READY")
    });
    session.enqueue_input(b"ABT").expect("accept before reply");
    pump_until(std::slice::from_mut(&mut session), |sessions| {
        // The query has been written and decoded, so the reply is generated
        // strictly after A. Enqueue B only now.
        visible_text(&sessions[0]).contains("Q_SENT")
    });
    session.enqueue_input(b"CD").expect("accept after reply");

    pump_until(std::slice::from_mut(&mut session), |sessions| {
        visible_text(&sessions[0]).contains("GOT:")
    });
    let text = visible_text(&session);
    let normalized: String = text.split_whitespace().collect();
    assert!(
        normalized.contains("GOT:668427916354996768"),
        "fifo order mismatch, text={text:?}"
    );
}

#[test]
fn reply_flood_is_bounded_and_nothing_is_dropped() {
    // 50 DA queries produce 50 replies (250 bytes). Decoding pauses after
    // each reply until it is written, so the pending reply never exceeds the
    // 14-byte bound even while the child never drains.
    let mut session = spawn_session(
        "stty raw -echo; for i in $(seq 1 50); do printf '\\033[c'; done; r=$(dd bs=1 count=250 2>/dev/null | od -An -v -tu1); printf 'GOT:%s\\n' \"$r\"",
    );
    pump_until(std::slice::from_mut(&mut session), |sessions| {
        let session = &sessions[0];
        let metrics = session.metrics();
        assert!(
            metrics.pending_reply_bytes <= 14,
            "pending reply exceeded the internal bound: {}",
            metrics.pending_reply_bytes
        );
        assert_eq!(
            metrics.pending_input_bytes + metrics.pending_reply_bytes,
            metrics.pending_write_bytes,
            "reply/input split must partition the write queue"
        );
        visible_text(session).contains("GOT:")
    });
    assert_eq!(
        session.metrics().terminal_reply_bytes_generated,
        250,
        "a reply was dropped"
    );
    let text = visible_text(&session);
    let normalized: String = text.split_whitespace().collect();
    // 50 x "2791635499" concatenated, preceded by GOT:.
    let expected = format!("GOT:{}", "2791635499".repeat(50));
    assert!(
        normalized.contains(&expected),
        "reply flood mismatch, text={text:?}"
    );
}

#[test]
fn metrics_reflect_pending_and_cumulative_state() {
    let mut session =
        spawn_session("stty -echo; while IFS= read -r line; do printf 'X:%s\\n' \"$line\"; done");
    session.enqueue_input(b"hello\n").expect("enqueue");

    let before = session.metrics();
    assert_eq!(before.input_bytes_enqueued, 6);
    assert_eq!(before.pending_write_bytes, 6);
    assert_eq!(before.pending_input_bytes, 6);
    assert_eq!(before.pending_reply_bytes, 0);
    assert!(session.interests().writable, "writable interest missing");
    assert!(session.needs_pump(), "queued input should need a pump");

    pump_until(std::slice::from_mut(&mut session), |sessions| {
        visible_text(&sessions[0]).contains("X:hello")
    });
    let after = session.metrics();
    assert!(after.pty_bytes_written >= 6, "bytes not written");
    assert_eq!(after.pending_write_bytes, 0);
    assert!(!session.interests().writable, "stale writable interest");
    // With nothing queued and the fd drained to WouldBlock, no immediate
    // re-pump is requested.
    assert!(
        !session.needs_pump(),
        "no work should remain after a full drain"
    );
}

#[test]
fn resize_updates_child_and_terminal_state() {
    let mut session = spawn_session(
        "stty -echo; while IFS= read -r line; do case \"$line\" in SIZE) stty size;; esac; done",
    );
    session
        .resize(termnix::Size::new(33, 121).expect("size"))
        .expect("resize");
    assert_eq!(
        session.terminal_state().size(),
        termnix::Size::new(33, 121).expect("nonzero size")
    );
    session.enqueue_input(b"SIZE\n").expect("enqueue size");
    pump_until(std::slice::from_mut(&mut session), |sessions| {
        visible_text(&sessions[0]).contains("33 121")
    });
}

#[test]
fn try_wait_reaps_and_keeps_state_readable() {
    let mut session = spawn_session("printf 'bye\\n'; exit 7");
    pump_until(std::slice::from_mut(&mut session), |sessions| {
        sessions[0].status() == termnix::SessionStatus::Eof
    });

    // The final state stays readable until the reap.
    assert!(visible_text(&session).contains("bye"));

    let status = wait_exit(&mut session);
    assert_eq!(status.code(), Some(7), "status={status:?}");
    assert_eq!(session.status(), termnix::SessionStatus::Reaped);

    // After the reap the state, snapshot, and metrics remain accessible.
    assert!(session.terminal_state().size().rows.get() > 0);
    assert!(visible_text(&session).contains("bye"));
    let metrics = session.metrics();
    assert!(metrics.pty_bytes_read > 0);
    assert!(session.fd().is_none(), "fd should be gone after reap");
}

#[test]
fn try_wait_before_eof_still_drains_output() {
    // Reap as soon as the child exits; remaining PTY output must still be
    // readable until EOF.
    let mut session = spawn_session("printf '%s\\n' $(seq 1 200); exit 3");
    let deadline = Instant::now() + DEADLINE;
    let mut exit = None;
    loop {
        session.pump_io().expect("pump");
        while session.needs_pump() {
            session.pump_io().expect("pump");
        }
        if exit.is_none() {
            exit = session.try_wait().expect("try_wait");
        }
        let text = visible_text(&session);
        let finished = text.contains("200")
            && matches!(
                session.status(),
                termnix::SessionStatus::Eof | termnix::SessionStatus::Reaped
            );
        if finished {
            break;
        }
        if Instant::now() > deadline {
            panic!("timed out; text={text:?} status={:?}", session.status());
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    assert!(
        visible_text(&session).contains("200"),
        "final output lost after early try_wait"
    );
    let status = exit.expect("child should have exited");
    assert_eq!(status.code(), Some(3), "status={status:?}");
    // Drain/reap to the spent state.
    let _ = wait_exit(&mut session);
    assert_eq!(session.status(), termnix::SessionStatus::Reaped);
}

#[test]
fn close_then_force_terminate_reaps_a_stubborn_process_group() {
    let mut session = spawn_session("trap '' TERM HUP; while :; do sleep 30; done");
    std::thread::sleep(Duration::from_millis(200));

    session.close();
    assert_eq!(session.status(), termnix::SessionStatus::Closing);
    assert!(session.fd().is_none(), "fd should be gone after close");

    // Graceful termination is ignored by the process group.
    assert_eq!(
        session.terminate().expect("terminate"),
        termnix::SignalOutcome::Sent
    );
    std::thread::sleep(Duration::from_millis(100));
    assert!(
        session.try_wait().expect("try_wait").is_none(),
        "SIGTERM should be ignored"
    );

    assert_eq!(
        session.force_terminate().expect("force terminate"),
        termnix::SignalOutcome::Sent
    );
    let status = wait_exit(&mut session);
    assert_eq!(status.signal(), Some(libc::SIGKILL));
}

#[test]
fn shutdown_reaps_every_session() {
    let a = spawn_session("while :; do sleep 30; done");
    let b = spawn_session("while :; do sleep 30; done");
    std::thread::sleep(Duration::from_millis(200));

    for session in [a, b] {
        let status = session.shutdown().expect("shutdown");
        // Closing the master can deliver SIGHUP before the SIGKILL lands, so
        // only require that the child was killed rather than exiting normally.
        assert!(!status.success(), "child exited normally");
        assert!(status.signal().is_some(), "child not signaled");
    }
}

#[test]
fn closed_session_rejects_input_and_resize() {
    let mut session = spawn_session("while :; do sleep 30; done");
    std::thread::sleep(Duration::from_millis(200));

    // Open session accepts all bytes.
    session.enqueue_input(b"hello").expect("accept while open");
    session.close();
    // pump_io after close is a successful no-op.
    session.pump_io().expect("pump after close");

    let err = session.enqueue_input(b"x").expect_err("closed rejects");
    assert_eq!(err.kind(), ErrorKind::BrokenPipe);
    let err = session
        .resize(termnix::Size::new(40, 40).expect("size"))
        .expect_err("closed rejects");
    assert_eq!(err.kind(), ErrorKind::BrokenPipe);
}

#[test]
fn multiple_sessions_are_driven_independently() {
    let a = spawn_session("printf 'A_READY\\n'; sleep 30");
    let b = spawn_session("printf 'B_READY\\n'; sleep 30");
    let mut sessions = [a, b];
    pump_until(&mut sessions, |sessions| {
        visible_text(&sessions[0]).contains("A_READY")
            && visible_text(&sessions[1]).contains("B_READY")
    });
    let text_a = visible_text(&sessions[0]);
    let text_b = visible_text(&sessions[1]);
    assert!(
        !text_a.contains("B_READY"),
        "A mixed in B output: {text_a:?}"
    );
    assert!(
        !text_b.contains("A_READY"),
        "B mixed in A output: {text_b:?}"
    );
}

#[test]
fn trim_scrollback_via_session_matches_terminal_state() {
    let mut session =
        spawn_session("for i in $(seq 1 40); do printf 'line-%02d\\n' \"$i\"; done; sleep 0.2");
    pump_until(std::slice::from_mut(&mut session), |sessions| {
        sessions[0].metrics().scrollback_lines >= 10
    });
    let before = session.metrics();
    assert!(before.scrollback_lines > 0);
    assert_eq!(before.scrollback_lines, before.max_scrollback_lines);
    assert!(before.scrollback_cells > 0);

    session.trim_scrollback(5, usize::MAX);
    let after = session.metrics();
    assert_eq!(after.scrollback_lines, 5);
    // Each retained row keeps its full 80-cell width.
    assert_eq!(after.scrollback_cells, 5 * 80);

    session.trim_scrollback(usize::MAX, 0);
    assert_eq!(session.metrics().scrollback_lines, 0);
    assert_eq!(session.metrics().scrollback_cells, 0);
}
