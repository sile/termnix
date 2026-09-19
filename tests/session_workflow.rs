//! End-to-end workflow across several real PTY-backed `Session`s.
//!
//! One workflow test drives sessions A, B, and C from a single public-API
//! poll loop, covering what the per-subsystem tests in `session.rs` do not:
//! A and B progressing together without output crossing over, A exiting and
//! being reaped while B keeps going, a fresh C joining the same loop, and A's
//! stale fd being unable to move C.
//!
//! The test uses a real PTY and real child processes; it does not use mocks,
//! the user's shell configuration, network access, or a fixed sleep as a
//! progress oracle. Waiting is always deadline + `poll` + `pump_io`.
//!
//! Reproduction:
//! `cargo test --test session_workflow -- --nocapture`

mod helpers;

use std::{
    os::{fd::RawFd, unix::process::ExitStatusExt},
    process::ExitStatus,
    time::{Duration, Instant},
};

use helpers::{
    DEADLINE, Teardown, default_size, enqueue, poll_once, pump_all, pump_until, rotate_until,
    screen_text, snapshot_text, snapshots, spawn, write_raw,
};
use termnix::{Input, PumpBudget, Session, SessionStatus};

/// Per-`pump_io` work ceiling used throughout the workflow.
///
/// Deliberately far below [`PumpBudget::default`] so a modest fixture cannot
/// be drained in one call. That makes the fairness check non-vacuous without
/// guessing an internal constant: the only requirement is that A's burst
/// exceeds one call's budget, which this budget guarantees, and the loop's
/// rotation is then observable. It also keeps the test fast, since the
/// smallest burst that stalls a pump is enough. The ceiling is expressed in
/// syscalls (4), because the filler emits one line per `printf` / read.
const WORK_BUDGET: PumpBudget = PumpBudget::new(48, 4);

/// A's initial payload. With [`WORK_BUDGET`] capping one call at 4 syscalls,
/// even this small burst spans many rotations, so B's marker can appear while
/// A is still mid-burst.
const A_FILLER_LINES: u32 = 200;

/// The last filler line: it exists only if the child's initial burst was
/// fully drained, so its absence proves A was still streaming when B's
/// sentinel appeared.
const A_FILLER_LAST: &str = "FILLER-0199";

/// A's output on `EXIT`: deliberately a few bytes so the child's exit can be
/// observed before the payload is drained. Kept far below any PTY buffer.
const A_FINAL: &str = "A_FINAL";

/// Builds a session script with a line protocol.
///
/// `INPUT:<value>` for any other line, `SIZE:<rows> <cols>` for `SIZE`, and
/// `<exit_marker>` plus exit 0 for `EXIT`. `stty -echo` plus `IFS= read -r`
/// means a logical Enter is enough to deliver a value.
fn session_script(prefix: &str, exit_marker: &str, with_filler: bool) -> String {
    let filler = if with_filler {
        format!(
            "i=0; while [ $i -lt {A_FILLER_LINES} ]; do printf 'FILLER-%04d\\n' \"$i\"; i=$((i+1)); done; "
        )
    } else {
        String::new()
    };
    format!(
        "stty -echo; printf '{prefix}_READY\\n'; {filler}\n\
         while IFS= read -r line; do\n\
           case \"$line\" in\n\
             SIZE) printf 'SIZE:%s\\n' \"$(stty size)\";;\n\
             EXIT) printf '{exit_marker}\\n'; exit 0;;\n\
             *) printf '{prefix}_INPUT:%s\\n' \"$line\";;\n\
           esac\n\
         done\n"
    )
}

/// Reaps one session, advancing it to `Reaped` within the test deadline.
fn reap(session: &mut Session) {
    let deadline = Instant::now() + DEADLINE;
    while Instant::now() < deadline {
        if session.status() == SessionStatus::Reaped {
            return;
        }
        let _ = session.try_wait().expect("try_wait");
        std::thread::sleep(Duration::from_millis(5));
    }
    panic!(
        "timed out reaping session; status={:?} snapshot={}",
        session.status(),
        snapshot_text(session)
    );
}

/// Steps 6-8: enqueue `EXIT` on A, observe the exit before the final drain,
/// save A's fd, then drain to EOF and reap. Returns the saved raw fd.
///
/// Takes the whole slice rather than `&mut sessions[0]` so the poll loop can
/// keep driving A alongside B while A winds down.
fn exit_and_drain_a(sessions: &mut [Session]) -> RawFd {
    // Wait until A has emitted its whole burst and drained to idle. Leaving
    // filler in flight would let the exit and the final payload be decoded in
    // the same pump, turning the required ordering into a scheduler race.
    //
    // Idleness is the oracle, not a visible marker: the tail of an 80 KiB
    // burst has scrolled far off the 24-row screen, so rendering scrollback
    // to search for it every iteration would dominate the test's runtime.
    // Once A is idle with an empty read buffer while its process is still
    // live, the child can only be blocked in `read` waiting for input, which
    // means the burst is complete.
    pump_until(
        sessions,
        WORK_BUDGET,
        "A filler fully emitted and drained",
        |sessions| {
            let a = &sessions[0];
            a.status() == SessionStatus::Live && !a.needs_pump()
        },
    );
    // The marker confirms the oracle: the whole burst really was emitted.
    assert!(
        snapshot_text(&sessions[0]).contains(A_FILLER_LAST),
        "A went idle without emitting its whole burst"
    );

    enqueue(sessions, 0, Input::Raw(b"EXIT\n"));
    // Flush the queued `EXIT` to A's PTY with exactly one pump, then observe
    // the child's exit with `try_wait` alone. A is never pumped again until the
    // exit has been seen, so `A_FINAL` cannot be read out of the PTY before
    // the exit is observed: the order is enforced by what is *not* driven, not
    // by the scheduler. B keeps being driven so the loop stays live.
    sessions[0].pump_io(WORK_BUDGET).expect("flush EXIT");
    let status = wait_exit_without_reading_a(sessions);
    // The pre-drain snapshot is still readable, and the final marker must not
    // be decoded yet, because the exit was observed first.
    let before = snapshot_text(&sessions[0]);
    assert!(
        !before.contains(A_FINAL),
        "A_FINAL was decoded before the exit was observed; A was not drained first"
    );
    // The process exited but the PTY master is still open, so the fd is still
    // a live registration until the drain reaches EOF.
    let stale_fd = sessions[0].fd().expect("A keeps its fd until EOF");
    pump_until(
        sessions,
        WORK_BUDGET,
        "A final output and EOF",
        |sessions| {
            snapshot_text(&sessions[0]).contains(A_FINAL)
                && matches!(
                    sessions[0].status(),
                    SessionStatus::Eof | SessionStatus::Reaped
                )
        },
    );
    assert!(status.success(), "A exit status: {status:?}");
    reap(&mut sessions[0]);
    stale_fd
}

/// Waits for `sessions[0]` to exit without ever reading its PTY.
///
/// A is only `try_wait`ed; every other session is pumped so the loop is not
/// idle. Because A's readable interest is deliberately left undriven, the
/// child's exit is guaranteed to be observed while `A_FINAL` is still sitting
/// unread in the PTY, which is what makes the ordering assertion deterministic
/// rather than a scheduler race.
fn wait_exit_without_reading_a(sessions: &mut [Session]) -> ExitStatus {
    let deadline = Instant::now() + DEADLINE;
    while Instant::now() < deadline {
        if let Some(status) = sessions[0].try_wait().expect("try_wait A") {
            return status;
        }
        // Wait on the remaining sessions; A's fd may be readable (its final
        // payload is pending) but it is intentionally not pumped.
        let (_, rest) = sessions.split_at_mut(1);
        if rest.is_empty() {
            std::thread::sleep(Duration::from_millis(5));
        } else {
            poll_once(rest, 5);
            pump_all(rest, WORK_BUDGET);
        }
    }
    panic!(
        "timed out waiting for A to exit; status={:?}\n{}",
        sessions[0].status(),
        snapshots(sessions),
    );
}

/// Step 10: writes `stale_fd` and asserts C neither advances nor emits output.
fn assert_stale_fd_does_not_drive(c: &mut Session, stale_fd: RawFd) {
    // A's fd number may or may not have been reused by C; either way, writing
    // the saved number must not, by itself, produce C's marker.
    let _ = write_raw(stale_fd, b"stale-charlie\n");
    let deadline = Instant::now() + DEADLINE;
    while Instant::now() < deadline {
        poll_once(std::slice::from_ref(&*c), 10);
        pump_all(std::slice::from_mut(c), WORK_BUDGET);
        if screen_text(c).contains("C_INPUT") {
            panic!("C advanced from A's stale fd: {}", screen_text(c));
        }
        // C produces no output of its own, so a bounded number of idle
        // rotations is enough: the point is that nothing arrives unprompted.
        if c.counters().pump_calls > 40 {
            break;
        }
    }
    assert!(
        !screen_text(c).contains("C_INPUT"),
        "C advanced without its own input"
    );
}

#[test]
fn sessions_survive_exit_reap_and_a_stale_fd() {
    let size = default_size();
    let a = spawn(&session_script("A", A_FINAL, true), size);
    let b = spawn(&session_script("B", "B_FINAL", false), size);
    // A and B stay in one slice for the whole joint phase; the guard only
    // covers teardown, so no session is moved out mid-workflow.
    let mut sessions = vec![a, b];
    let mut guard = Teardown::default();

    // Steps 1-3: A is mid-burst while B becomes ready. A's filler is larger
    // than one pump quantum, so the only way B's marker can appear is if every
    // session gets a turn per rotation instead of A being drained to idle.
    // Markers are read from the visible rows, and recorded once seen, so the
    // wait never renders A's growing scrollback.
    let mut a_started = false;
    let mut b_ready = false;
    rotate_until(
        &mut sessions,
        WORK_BUDGET,
        "A streaming while B becomes ready",
        |sessions| {
            a_started |= screen_text(&sessions[0]).contains("FILLER-");
            b_ready |= screen_text(&sessions[1]).contains("B_READY");
            a_started && b_ready
        },
    );
    // No cross-talk: B's markers never reach A, and A's never reach B.
    assert!(
        !screen_text(&sessions[0]).contains("B_"),
        "B output appeared in A: {}",
        screen_text(&sessions[0])
    );
    assert!(
        !screen_text(&sessions[1]).contains("FILLER-"),
        "A output appeared in B: {}",
        screen_text(&sessions[1])
    );
    // Fairness is only meaningful while A is still producing, so require the
    // end of A's burst has not been reached: B progressed mid-stream. No poll
    // count or wall-clock time is used as the oracle.
    assert!(
        !snapshot_text(&sessions[0]).contains(A_FILLER_LAST),
        "B was ready only after A finished, so rotation was not exercised"
    );

    // Step 4: distinct input routed to each session.
    enqueue(&mut sessions, 0, Input::Raw(b"alpha\n"));
    enqueue(&mut sessions, 1, Input::Raw(b"beta\n"));
    pump_until(
        &mut sessions,
        WORK_BUDGET,
        "A_INPUT:alpha and B_INPUT:beta",
        |sessions| {
            screen_text(&sessions[0]).contains("A_INPUT:alpha")
                && screen_text(&sessions[1]).contains("B_INPUT:beta")
        },
    );
    assert!(
        !screen_text(&sessions[0]).contains("B_INPUT"),
        "input crossed into A"
    );
    assert!(
        !screen_text(&sessions[1]).contains("A_INPUT"),
        "input crossed into B"
    );

    // Step 5: B resizes. The child's own `stty size` is the authority, so the
    // child kernel size and B's snapshot must agree, and A must not move.
    let new_size = helpers::size(31, 111);
    sessions[1].resize(new_size).expect("resize B");
    assert_eq!(
        sessions[1].terminal_state().size(),
        new_size,
        "B snapshot did not follow the resize"
    );
    assert_eq!(
        sessions[0].terminal_state().size(),
        size,
        "resizing B changed A's size"
    );
    enqueue(&mut sessions, 1, Input::Raw(b"SIZE\n"));
    pump_until(
        &mut sessions,
        WORK_BUDGET,
        "B reports the resized stty size",
        |sessions| screen_text(&sessions[1]).contains("SIZE:31 111"),
    );
    assert!(
        !screen_text(&sessions[0]).contains("SIZE:"),
        "B's size report leaked into A"
    );

    // Steps 6-8: A exits and is reaped while B stays in the loop.
    let stale_fd = exit_and_drain_a(&mut sessions);

    // Step 9: B keeps working after A is reaped.
    enqueue(&mut sessions, 1, Input::Raw(b"gamma\n"));
    pump_until(
        &mut sessions,
        WORK_BUDGET,
        "B input after A reap",
        |sessions| screen_text(&sessions[1]).contains("B_INPUT:gamma"),
    );
    let shrink = helpers::size(20, 60);
    sessions[1].resize(shrink).expect("resize B again");
    enqueue(&mut sessions, 1, Input::Raw(b"SIZE\n"));
    pump_until(
        &mut sessions,
        WORK_BUDGET,
        "B reports size after A reap",
        |sessions| screen_text(&sessions[1]).contains("SIZE:20 60"),
    );

    // Step 10: C waits for input without producing output. A's stale fd must
    // not move it; only C's own fd and a real enqueue do.
    let mut c = spawn(
        "stty -echo; IFS= read -r line; printf 'C_INPUT:%s\\n' \"$line\"",
        size,
    );
    assert_stale_fd_does_not_drive(&mut c, stale_fd);
    enqueue(std::slice::from_mut(&mut c), 0, Input::Raw(b"charlie\n"));
    pump_until(
        std::slice::from_mut(&mut c),
        WORK_BUDGET,
        "C_INPUT:charlie",
        |sessions| screen_text(&sessions[0]).contains("C_INPUT:charlie"),
    );

    // Step 11: explicit teardown. C is shut down here; the guard covers
    // whatever is still looping (B), so no live session outlives the test.
    let status = c.shutdown().expect("shutdown C");
    assert!(
        status.code().is_some() || status.signal().is_some(),
        "unexpected teardown status: {status:?}"
    );
    for session in sessions {
        guard.adopt(session);
    }
    assert_eq!(guard.len(), 2, "guard should own both remaining sessions");
}
