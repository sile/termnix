use std::{
    io::{self, ErrorKind},
    os::unix::process::ExitStatusExt,
    process::Command,
    time::{Duration, Instant},
};

use termnix::{
    DriverConfig, DriverError, DriverEvent, KeyCode, KeyEvent, Position, Readiness,
    ScrollbackLimits, SessionConfig, SessionDriver, SessionId, SessionStatus, SignalOutcome, Size,
};

const DEADLINE: Duration = Duration::from_secs(15);

fn spawn_script(driver: &mut SessionDriver, script: &str) -> SessionId {
    let mut command = Command::new("/bin/sh");
    command.arg("-c").arg(script);
    driver
        .create_session(
            &mut command,
            SessionConfig {
                size: Size { rows: 24, cols: 80 },
                scrollback_limits: ScrollbackLimits::DISABLED,
            },
        )
        .expect("create session")
}

/// Renders the snapshot's visible cells as rows, dropping wide-character
/// continuation cells and trailing blanks.
fn visible_text(driver: &SessionDriver, id: SessionId) -> String {
    let snapshot = driver.snapshot(id).expect("snapshot");
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

/// Polls all registered fds, drives the driver, and polls child exits until
/// `cond` holds or the deadline expires. `cond` receives the driver and every
/// lifecycle event collected so far (events are never consumed by the helper).
fn drive_until<F>(driver: &mut SessionDriver, mut cond: F)
where
    F: FnMut(&SessionDriver, &[DriverEvent]) -> bool,
{
    let deadline = Instant::now() + DEADLINE;
    let mut seen_events: Vec<DriverEvent> = Vec::new();
    while Instant::now() < deadline {
        let mut pollfds = Vec::new();
        let mut tokens = Vec::new();
        let source = driver.poll_sources();
        for entry in source.entries() {
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
        let new_events = driver.poll_processes().expect("poll processes");
        seen_events.extend(new_events);
        if cond(driver, &seen_events) {
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
        let _ = driver.drive(&readiness).expect("drive");
        let new_events = driver.poll_processes().expect("poll processes");
        seen_events.extend(new_events);
        if cond(driver, &seen_events) {
            return;
        }
    }
    panic!("timed out waiting for condition");
}

/// Waits for the once-per-session exit event for `id`.
fn wait_exit(driver: &mut SessionDriver, id: SessionId) -> DriverEvent {
    let deadline = Instant::now() + DEADLINE;
    loop {
        for event in driver.poll_processes().expect("poll processes") {
            if let DriverEvent::SessionExited { id: event_id, .. } = event
                && event_id == id
            {
                return event;
            }
        }
        if Instant::now() > deadline {
            panic!("timed out waiting for exit event");
        }
        std::thread::sleep(Duration::from_millis(10));
    }
}

#[test]
fn decodes_child_output_into_terminal_state() {
    let mut driver = SessionDriver::with_default_config();
    let id = spawn_script(&mut driver, "printf 'hello-driver\\n'");
    drive_until(&mut driver, |d, _| {
        visible_text(d, id).contains("hello-driver")
            && matches!(d.session_status(id), Ok(SessionStatus::Eof))
    });
    assert!(matches!(driver.session_status(id), Ok(SessionStatus::Eof)));
}

#[test]
fn input_routes_to_the_target_session_only() {
    let mut driver = SessionDriver::with_default_config();
    let a = spawn_script(
        &mut driver,
        "stty -echo; while IFS= read -r line; do printf 'A:%s\\n' \"$line\"; done",
    );
    let b = spawn_script(
        &mut driver,
        "stty -echo; while IFS= read -r line; do printf 'B:%s\\n' \"$line\"; done",
    );

    driver.enqueue_text(a, "hello-a").expect("enqueue a");
    driver.enqueue_text(b, "hello-b").expect("enqueue b");
    driver
        .enqueue_key(a, KeyEvent::new(KeyCode::Enter))
        .expect("enter a");
    driver
        .enqueue_key(b, KeyEvent::new(KeyCode::Enter))
        .expect("enter b");

    drive_until(&mut driver, |d, _| {
        let text_a = visible_text(d, a);
        let text_b = visible_text(d, b);
        text_a.contains("A:hello-a")
            && text_b.contains("B:hello-b")
            && !text_a.contains("B:hello-b")
            && !text_b.contains("A:hello-a")
    });
}

#[test]
fn query_reply_returns_to_the_querying_session() {
    let mut driver = SessionDriver::with_default_config();
    // Send a primary DA request, read the 5-byte reply, and render its bytes
    // as decimal values so the reply content is observable in the snapshot.
    // Non-canonical mode is required because the reply contains no newline.
    let id = spawn_script(
        &mut driver,
        "stty -echo -icanon min 1 time 0; printf '\\033[c'; r=$(dd bs=1 count=5 2>/dev/null | od -An -tu1); printf 'REPLY:%s\\n' \"$r\"",
    );
    drive_until(&mut driver, |d, _| visible_text(d, id).contains("REPLY:"));
    let text = visible_text(&driver, id);
    // ESC [ ? 6 c => 27 91 63 54 99. `od` pads columns with spaces, so compare
    // with all whitespace removed.
    let normalized: String = text.split_whitespace().collect();
    assert!(
        normalized.contains("REPLY:2791635499"),
        "reply mismatch, text={text:?}"
    );
}

#[test]
fn resize_updates_child_and_terminal_state() {
    let mut driver = SessionDriver::with_default_config();
    let id = spawn_script(
        &mut driver,
        "stty -echo; while IFS= read -r line; do case \"$line\" in SIZE) stty size;; esac; done",
    );
    driver
        .resize(
            id,
            Size {
                rows: 33,
                cols: 121,
            },
        )
        .expect("resize");
    assert_eq!(
        driver.terminal_state(id).expect("terminal state").size(),
        Size {
            rows: 33,
            cols: 121
        }
    );
    driver.enqueue_text(id, "SIZE\n").expect("enqueue size");
    drive_until(&mut driver, |d, _| visible_text(d, id).contains("33 121"));
}

#[test]
fn observes_exit_and_reaps() {
    let mut driver = SessionDriver::with_default_config();
    let id = spawn_script(&mut driver, "printf 'bye\\n'; exit 0");
    let mut exit_count = 0;
    drive_until(&mut driver, |d, events| {
        exit_count = events
            .iter()
            .filter(
                |event| matches!(event, DriverEvent::SessionExited { id: eid, .. } if *eid == id),
            )
            .count();
        matches!(d.session_status(id), Ok(SessionStatus::Eof)) && exit_count >= 1
    });
    // The exit event is emitted exactly once.
    assert_eq!(exit_count, 1);

    // The final state and snapshot stay readable until the reap.
    driver.snapshot(id).expect("snapshot before reap");
    let status = driver.reap_session(id).expect("reap");
    assert!(status.success(), "status={status:?}");

    assert!(matches!(driver.snapshot(id), Err(DriverError::StaleId(_))));
    assert!(matches!(
        driver.reap_session(id),
        Err(DriverError::StaleId(_))
    ));
}

#[test]
fn stale_token_does_not_drive_a_new_session() {
    let mut driver = SessionDriver::with_default_config();
    let a = spawn_script(
        &mut driver,
        "stty -echo; IFS= read -r line; printf 'A:%s\\n' \"$line\"",
    );
    let token_a = {
        let source = driver.poll_sources();
        source
            .entries()
            .iter()
            .find(|entry| entry.token.session_id() == a)
            .expect("a token")
            .token
    };
    driver.close_session(a).expect("close a");
    wait_exit(&mut driver, a);
    driver.reap_session(a).expect("reap a");

    // B produces nothing until input arrives, so a stale readiness cannot
    // accidentally be attributed to it.
    let b = spawn_script(
        &mut driver,
        "stty -echo; IFS= read -r line; printf 'B:%s\\n' \"$line\"",
    );
    assert!(
        !visible_text(&driver, b).contains("B:"),
        "B produced output"
    );

    let result = driver
        .drive(&[(
            token_a,
            Readiness {
                readable: true,
                writable: false,
                hangup: false,
                error: false,
            },
        )])
        .expect("drive");
    assert!(
        result.stale_tokens.contains(&token_a),
        "stale token should be reported"
    );
    assert!(
        !visible_text(&driver, b).contains("B:"),
        "B must not change"
    );

    // A valid token for B still makes progress.
    driver.enqueue_text(b, "hi").expect("enqueue b");
    driver
        .enqueue_key(b, KeyEvent::new(KeyCode::Enter))
        .expect("enter b");
    drive_until(&mut driver, |d, _| visible_text(d, b).contains("B:hi"));
}

#[test]
fn close_then_force_terminate_reaps_a_stubborn_process_group() {
    let mut driver = SessionDriver::with_default_config();
    let id = spawn_script(&mut driver, "trap '' TERM HUP; while :; do sleep 30; done");
    std::thread::sleep(Duration::from_millis(200));

    driver.close_session(id).expect("close session");
    assert!(matches!(
        driver.session_status(id),
        Ok(SessionStatus::Closing)
    ));

    // Graceful termination is ignored by the process group.
    assert_eq!(
        driver.terminate_session(id).expect("terminate"),
        SignalOutcome::Sent
    );
    std::thread::sleep(Duration::from_millis(100));
    assert!(
        driver.poll_processes().expect("poll").is_empty(),
        "SIGTERM should be ignored"
    );

    assert_eq!(
        driver.force_terminate_session(id).expect("force terminate"),
        SignalOutcome::Sent
    );
    wait_exit(&mut driver, id);
    let status = driver.reap_session(id).expect("reap");
    assert_eq!(status.signal(), Some(libc::SIGKILL));
}

#[test]
fn shutdown_reaps_every_session() {
    let mut driver = SessionDriver::with_default_config();
    let _a = spawn_script(&mut driver, "while :; do sleep 30; done");
    let _b = spawn_script(&mut driver, "while :; do sleep 30; done");
    std::thread::sleep(Duration::from_millis(200));

    let outcome = driver.shutdown();
    assert_eq!(outcome.sessions.len(), 2);
    for (id, result) in &outcome.sessions {
        let status = result.as_ref().unwrap_or_else(|err| {
            panic!("session {id:?} reap failed: {err}");
        });
        // Closing the master can deliver SIGHUP before the SIGKILL lands, so
        // only require that the child was killed rather than exiting normally.
        assert!(!status.success(), "session {id:?} exited normally");
        assert!(status.signal().is_some(), "session {id:?} not signaled");
    }
}

#[test]
fn enqueue_is_all_or_nothing_under_backpressure() {
    let config = DriverConfig {
        write_queue_limit: 64,
        ..DriverConfig::default()
    };
    let mut driver = SessionDriver::new(config).expect("driver");
    let id = spawn_script(
        &mut driver,
        "stty -echo; while IFS= read -r line; do printf 'X:%s\\n' \"$line\"; done",
    );

    driver.enqueue_text(id, "hello\n").expect("fits");
    let oversized = "x".repeat(100);
    assert!(matches!(
        driver.enqueue_text(id, &oversized),
        Err(DriverError::Backpressure)
    ));
    // The oversized input was not partially admitted.
    drive_until(&mut driver, |d, _| {
        let text = visible_text(d, id);
        text.contains("X:hello") && !text.contains("X:xxxxx")
    });
}

#[test]
fn multiple_sessions_share_one_drive_without_mixing() {
    let mut driver = SessionDriver::with_default_config();
    let a = spawn_script(&mut driver, "printf 'A_READY\\n'; sleep 30");
    let b = spawn_script(&mut driver, "printf 'B_READY\\n'; sleep 30");
    drive_until(&mut driver, |d, _| {
        visible_text(d, a).contains("A_READY") && visible_text(d, b).contains("B_READY")
    });
    let text_a = visible_text(&driver, a);
    let text_b = visible_text(&driver, b);
    assert!(
        !text_a.contains("B_READY"),
        "A mixed in B output: {text_a:?}"
    );
    assert!(
        !text_b.contains("A_READY"),
        "B mixed in A output: {text_b:?}"
    );
}
