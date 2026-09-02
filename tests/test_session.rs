use std::{
    io::{self, ErrorKind},
    os::unix::process::ExitStatusExt,
    process::Command,
    time::{Duration, Instant},
};

use termnix::{
    DriveBudget, KeyCode, KeyEvent, Position, Readiness, Session, SessionConfig, SessionError,
    SessionEvent, SessionStatus, SignalOutcome, Size,
};

const DEADLINE: Duration = Duration::from_secs(15);

fn session_config() -> SessionConfig {
    SessionConfig::default()
}

fn spawn_session(script: &str) -> Session {
    spawn_session_with(script, session_config())
}

fn spawn_session_with(script: &str, config: SessionConfig) -> Session {
    let mut command = Command::new("/bin/sh");
    command.arg("-c").arg(script);
    Session::new(&mut command, config).expect("create session")
}

/// Renders the snapshot's visible cells as rows, dropping wide-character
/// continuation cells and trailing blanks.
fn visible_text(session: &Session) -> String {
    let snapshot = session.snapshot().expect("snapshot");
    let mut out = String::new();
    for row in 0..snapshot.size().rows {
        let mut line = String::new();
        for col in 0..snapshot.size().cols {
            let cell = snapshot.cell(Position { row, col }).expect("cell in range");
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

/// Polls every session's registered fd, drives each session with its own
/// readiness, and polls child exits until `cond` holds or the deadline
/// expires. `cond` receives the sessions and every lifecycle event collected
/// so far (events are never consumed by the helper).
fn drive_until<F>(sessions: &mut [Session], mut cond: F)
where
    F: FnMut(&[Session], &[SessionEvent]) -> bool,
{
    let deadline = Instant::now() + DEADLINE;
    let mut seen_events: Vec<SessionEvent> = Vec::new();
    while Instant::now() < deadline {
        let mut pollfds = Vec::new();
        let mut tokens = Vec::new();
        for session in sessions.iter_mut() {
            if let Some(entry) = session.poll_source() {
                let mut events = 0;
                if entry.interests.readable {
                    events |= libc::POLLIN;
                }
                if entry.interests.writable {
                    events |= libc::POLLOUT;
                }
                pollfds.push(libc::pollfd {
                    fd: entry.fd,
                    events,
                    revents: 0,
                });
                tokens.push(entry.token);
            }
        }
        let new_events = poll_processes(sessions).expect("poll processes");
        seen_events.extend(new_events);
        if cond(sessions, &seen_events) {
            return;
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
        let mut readiness = Vec::new();
        for (token, pollfd) in tokens.iter().zip(pollfds.iter()) {
            let mut r = Readiness::default();
            if pollfd.revents & libc::POLLIN != 0 {
                r.readable = true;
            }
            if pollfd.revents & libc::POLLOUT != 0 {
                r.writable = true;
            }
            if pollfd.revents & libc::POLLHUP != 0 {
                r.hangup = true;
            }
            if pollfd.revents & libc::POLLERR != 0 {
                r.error = true;
            }
            if r != Readiness::default() {
                readiness.push((*token, r));
            }
        }
        // Drive each session with the readiness observed on its own fd. A
        // session with no observed edge still gets a drive so buffered work
        // can progress.
        for session in sessions.iter_mut() {
            let token = session.poll_source().map(|entry| entry.token);
            let r = token.and_then(|token| {
                readiness
                    .iter()
                    .find(|(t, _)| *t == token)
                    .map(|(_, r)| (token, *r))
            });
            let _ = session.drive(r, DriveBudget::default()).expect("drive");
        }
    }
    panic!("timed out waiting for condition");
}

/// Observes child exits on every session, collecting at-most-once events.
fn poll_processes(sessions: &mut [Session]) -> Result<Vec<SessionEvent>, SessionError> {
    let mut events = Vec::new();
    for session in sessions.iter_mut() {
        if let Some(event) = session.poll_process()? {
            events.push(event);
        }
    }
    Ok(events)
}

/// Waits for the once-per-session exit event.
fn wait_exit(session: &mut Session) -> SessionEvent {
    let deadline = Instant::now() + DEADLINE;
    loop {
        if let Some(event) = session.poll_process().expect("poll processes") {
            return event;
        }
        if Instant::now() > deadline {
            panic!("timed out waiting for exit event");
        }
        std::thread::sleep(Duration::from_millis(10));
    }
}

#[test]
fn decodes_child_output_into_terminal_state() {
    let mut session = spawn_session("printf 'hello-session\\n'");
    drive_until(std::slice::from_mut(&mut session), |sessions, _| {
        visible_text(&sessions[0]).contains("hello-session")
            && sessions[0].session_status() == SessionStatus::Eof
    });
}

#[test]
fn input_routes_to_the_target_session_only() {
    let mut a =
        spawn_session("stty -echo; while IFS= read -r line; do printf 'A:%s\\n' \"$line\"; done");
    let mut b =
        spawn_session("stty -echo; while IFS= read -r line; do printf 'B:%s\\n' \"$line\"; done");

    a.enqueue_text("hello-a").expect("enqueue a");
    b.enqueue_text("hello-b").expect("enqueue b");
    a.enqueue_key(KeyEvent::new(KeyCode::Enter))
        .expect("enter a");
    b.enqueue_key(KeyEvent::new(KeyCode::Enter))
        .expect("enter b");

    let mut sessions = [a, b];
    drive_until(&mut sessions, |sessions, _| {
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
    // Capture raw bytes received by the child so all four enqueue paths are
    // observable. The child switches to raw mode and signals readiness before
    // we enqueue, so no line-discipline translation (CR->LF) applies.
    let mut session = spawn_session(
        "stty raw -echo; printf 'READY\\n'; r=$(dd bs=1 count=9 2>/dev/null | od -An -v -tu1); printf 'GOT:%s\\n' \"$r\"",
    );
    drive_until(std::slice::from_mut(&mut session), |sessions, _| {
        visible_text(&sessions[0]).contains("READY")
    });
    session.enqueue_text("AB").expect("text");
    session
        .enqueue_key(KeyEvent::new(KeyCode::Enter))
        .expect("enter");
    session.enqueue_paste("CD").expect("paste");
    session.enqueue_raw(b"EFGH").expect("raw");

    drive_until(std::slice::from_mut(&mut session), |sessions, _| {
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
    drive_until(std::slice::from_mut(&mut session), |sessions, _| {
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
fn held_reply_resumes_without_a_new_os_edge() {
    // Fill the outbound queue so a terminal reply cannot be admitted, then
    // verify the reply is re-admitted and written once the queue drains, all
    // from drive calls that observe no new OS edge.
    let config = SessionConfig {
        write_queue_limit: 40,
        pending_reply_limit: 40,
        ..session_config()
    };
    // 36 filler bytes leave 4 free bytes, less than the 5-byte reply. The
    // child reads 36 + 5 = 41 bytes total.
    let mut session = spawn_session_with(
        "stty raw -echo; printf '\\033[c'; r=$(dd bs=1 count=41 2>/dev/null | od -An -v -tu1); printf 'REPLY:%s\\n' \"$r\"",
        config,
    );
    session.enqueue_raw(&b"F".repeat(36)).expect("fill queue");

    drive_until(std::slice::from_mut(&mut session), |sessions, _| {
        visible_text(&sessions[0]).contains("REPLY:")
    });
    let text = visible_text(&session);
    let normalized: String = text.split_whitespace().collect();
    // 36 x 'F' (70) followed by the 5-byte reply (27 91 63 54 99).
    let reply_index = normalized.find("REPLY:").expect("REPLY marker");
    let digits = &normalized[reply_index + "REPLY:".len()..];
    assert_eq!(
        digits.matches("70").count(),
        36,
        "filler bytes mismatch, text={text:?}"
    );
    assert!(
        digits.ends_with("2791635499"),
        "reply after queue drain mismatch, text={text:?}"
    );
}

#[test]
fn resize_updates_child_and_terminal_state() {
    let mut session = spawn_session(
        "stty -echo; while IFS= read -r line; do case \"$line\" in SIZE) stty size;; esac; done",
    );
    session
        .resize(Size {
            rows: 33,
            cols: 121,
        })
        .expect("resize");
    assert_eq!(
        session.terminal_state().expect("terminal state").size(),
        Size {
            rows: 33,
            cols: 121
        }
    );
    session.enqueue_text("SIZE\n").expect("enqueue size");
    drive_until(std::slice::from_mut(&mut session), |sessions, _| {
        visible_text(&sessions[0]).contains("33 121")
    });
}

#[test]
fn observes_exit_and_reaps() {
    let mut session = spawn_session("printf 'bye\\n'; exit 0");
    let mut exit_count = 0;
    drive_until(std::slice::from_mut(&mut session), |sessions, events| {
        exit_count = events
            .iter()
            .filter(|event| matches!(event, SessionEvent::SessionExited { .. }))
            .count();
        sessions[0].session_status() == SessionStatus::Eof && exit_count >= 1
    });
    // The exit event is emitted exactly once.
    assert_eq!(exit_count, 1);

    // The final state and snapshot stay readable until the reap.
    session.snapshot().expect("snapshot before reap");
    let status = session.reap().expect("reap");
    assert!(status.success(), "status={status:?}");

    assert_eq!(session.session_status(), SessionStatus::Reaped);
    assert!(matches!(session.snapshot(), Err(SessionError::Reaped)));
    assert!(matches!(session.reap(), Err(SessionError::Reaped)));
}

#[test]
fn stale_token_does_not_drive_a_new_session() {
    // A token issued for one session must never be accepted by a different
    // session, even though the OS may reuse the same fd number.
    let mut a = spawn_session("stty -echo; IFS= read -r line; printf 'A:%s\\n' \"$line\"");
    let token_a = a.poll_source().expect("a source").token;
    a.close().expect("close a");
    wait_exit(&mut a);
    a.reap().expect("reap a");

    // B produces nothing until input arrives, so a stale readiness cannot
    // accidentally be attributed to it.
    let mut b = spawn_session("stty -echo; IFS= read -r line; printf 'B:%s\\n' \"$line\"");
    assert!(!visible_text(&b).contains("B:"), "B produced output");

    let result = b
        .drive(
            Some((
                token_a,
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
        result.stale_tokens.contains(&token_a),
        "stale token should be reported"
    );
    assert!(!visible_text(&b).contains("B:"), "B must not change");

    // A valid token for B still makes progress.
    b.enqueue_text("hi").expect("enqueue b");
    b.enqueue_key(KeyEvent::new(KeyCode::Enter))
        .expect("enter b");
    drive_until(std::slice::from_mut(&mut b), |sessions, _| {
        visible_text(&sessions[0]).contains("B:hi")
    });
}

#[test]
fn close_then_force_terminate_reaps_a_stubborn_process_group() {
    let mut session = spawn_session("trap '' TERM HUP; while :; do sleep 30; done");
    std::thread::sleep(Duration::from_millis(200));

    session.close().expect("close session");
    assert_eq!(session.session_status(), SessionStatus::Closing);

    // Graceful termination is ignored by the process group.
    assert_eq!(session.terminate().expect("terminate"), SignalOutcome::Sent);
    std::thread::sleep(Duration::from_millis(100));
    assert!(
        session.poll_process().expect("poll").is_none(),
        "SIGTERM should be ignored"
    );

    assert_eq!(
        session.force_terminate().expect("force terminate"),
        SignalOutcome::Sent
    );
    wait_exit(&mut session);
    let status = session.reap().expect("reap");
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
fn enqueue_is_all_or_nothing_under_backpressure() {
    let config = SessionConfig {
        write_queue_limit: 64,
        ..session_config()
    };
    let mut session = spawn_session_with(
        "stty -echo; while IFS= read -r line; do printf 'X:%s\\n' \"$line\"; done",
        config,
    );

    session.enqueue_text("hello\n").expect("fits");
    let oversized = "x".repeat(100);
    assert!(matches!(
        session.enqueue_text(&oversized),
        Err(SessionError::Backpressure)
    ));
    // The oversized input was not partially admitted.
    drive_until(std::slice::from_mut(&mut session), |sessions, _| {
        let text = visible_text(&sessions[0]);
        text.contains("X:hello") && !text.contains("X:xxxxx")
    });
}

#[test]
fn multiple_sessions_are_driven_independently() {
    let a = spawn_session("printf 'A_READY\\n'; sleep 30");
    let b = spawn_session("printf 'B_READY\\n'; sleep 30");
    let mut sessions = [a, b];
    drive_until(&mut sessions, |sessions, _| {
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
