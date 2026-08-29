use std::{
    process::Command,
    time::{Duration, Instant},
};

const SIZE: muxnix::Size = muxnix::Size { rows: 24, cols: 80 };

fn true_cmd() -> Command {
    Command::new("true")
}

fn sleep_cmd() -> Command {
    let mut command = Command::new("sleep");
    command.arg("30");
    command
}

fn exit_cmd(code: i32) -> Command {
    let mut command = Command::new("/bin/sh");
    command.arg("-c").arg(format!("exit {code}"));
    command
}

fn wait_for_exit(mux: &mut muxnix::Multiplexer, pane: muxnix::PaneId, timeout: Duration) {
    let deadline = Instant::now() + timeout;
    loop {
        mux.poll_exits().expect("poll exits");
        if mux.pane(pane).expect("pane").exit_status().is_some() {
            return;
        }
        if Instant::now() >= deadline {
            panic!("timed out waiting for pane {pane:?} to exit");
        }
        std::thread::sleep(Duration::from_millis(10));
    }
}

#[test]
fn new_multiplexer_is_empty() {
    let mux = muxnix::Multiplexer::new();
    assert!(mux.is_empty());
    assert_eq!(mux.active_window(), None);
    assert_eq!(mux.window_count(), 0);
}

#[test]
fn create_window_sets_active_and_focus() {
    let mut mux = muxnix::Multiplexer::new();
    let (window, pane) = mux
        .create_window(&mut true_cmd(), SIZE)
        .expect("create window");
    assert_eq!(mux.active_window(), Some(window));
    assert_eq!(mux.focus_pane(window).expect("focus"), pane);
    assert_eq!(mux.window_of_pane(pane).expect("owner"), window);
    assert_eq!(mux.pane(pane).expect("pane").terminal().size(), SIZE);

    let events = mux.drain_events();
    assert!(events.contains(&muxnix::LifecycleEvent::WindowCreated { window }));
    assert!(events.contains(&muxnix::LifecycleEvent::PaneCreated { window, pane }));
    assert!(
        events.contains(&muxnix::LifecycleEvent::ActiveWindowChanged {
            window: Some(window)
        })
    );
}

#[test]
fn per_window_focus_survives_active_window_round_trip() {
    let mut mux = muxnix::Multiplexer::new();
    let (w1, p1a) = mux.create_window(&mut true_cmd(), SIZE).expect("window 1");
    let _p1b = mux.create_pane(w1, &mut true_cmd(), SIZE).expect("pane 1b");
    mux.set_focus(w1, p1a).expect("focus 1a");

    let (w2, _p2a) = mux.create_window(&mut true_cmd(), SIZE).expect("window 2");
    let p2b = mux.create_pane(w2, &mut true_cmd(), SIZE).expect("pane 2b");
    assert_eq!(mux.focus_pane(w2).expect("w2 focus"), p2b);
    assert_eq!(mux.active_window(), Some(w2));

    mux.select_window(w1).expect("select w1");
    assert_eq!(mux.active_window(), Some(w1));
    assert_eq!(mux.focus_pane(w1).expect("w1 focus"), p1a);

    mux.select_window(w2).expect("select w2");
    assert_eq!(mux.active_window(), Some(w2));
    assert_eq!(mux.focus_pane(w2).expect("w2 focus"), p2b);

    mux.select_window(w1).expect("select w1 again");
    assert_eq!(mux.focus_pane(w1).expect("w1 focus"), p1a);
    assert_eq!(mux.focus_pane(w2).expect("w2 focus"), p2b);
}

#[test]
fn stale_pane_id_is_rejected() {
    let mut mux = muxnix::Multiplexer::new();
    let (window, pane) = mux.create_window(&mut true_cmd(), SIZE).expect("window");
    let _ = mux
        .create_pane(window, &mut true_cmd(), SIZE)
        .expect("second pane");
    mux.drain_events();

    mux.remove_pane(pane).expect("remove pane");
    assert_eq!(
        mux.pane(pane).expect_err("stale pane"),
        muxnix::MultiplexerError::UnknownPane(pane)
    );
    assert_eq!(
        mux.remove_pane(pane).expect_err("stale remove"),
        muxnix::MultiplexerError::UnknownPane(pane)
    );
    assert_eq!(
        mux.set_focus(window, pane).expect_err("stale focus"),
        muxnix::MultiplexerError::UnknownPane(pane)
    );
}

#[test]
fn stale_window_id_is_rejected() {
    let mut mux = muxnix::Multiplexer::new();
    let (window, _pane) = mux.create_window(&mut true_cmd(), SIZE).expect("window");
    mux.drain_events();

    mux.remove_window(window).expect("remove window");
    assert_eq!(
        mux.window(window).expect_err("stale window"),
        muxnix::MultiplexerError::UnknownWindow(window)
    );
    assert_eq!(
        mux.select_window(window).expect_err("stale select"),
        muxnix::MultiplexerError::UnknownWindow(window)
    );
    assert_eq!(
        mux.remove_window(window).expect_err("stale remove"),
        muxnix::MultiplexerError::UnknownWindow(window)
    );
}

#[test]
fn ids_are_not_reused_after_removal() {
    let mut mux = muxnix::Multiplexer::new();
    let (w1, p1) = mux.create_window(&mut true_cmd(), SIZE).expect("first");
    mux.remove_window(w1).expect("remove");
    let (w2, p2) = mux.create_window(&mut true_cmd(), SIZE).expect("second");
    assert_ne!(w1, w2);
    assert_ne!(p1, p2);
    assert_eq!(
        mux.pane(p1).expect_err("old pane"),
        muxnix::MultiplexerError::UnknownPane(p1)
    );
}

#[test]
fn removing_last_pane_closes_window_and_can_empty_multiplexer() {
    let mut mux = muxnix::Multiplexer::new();
    let (window, pane) = mux.create_window(&mut true_cmd(), SIZE).expect("window");
    mux.drain_events();

    mux.remove_pane(pane).expect("remove last pane");
    assert!(mux.is_empty());
    assert_eq!(mux.active_window(), None);
    assert_eq!(
        mux.window(window).expect_err("window gone"),
        muxnix::MultiplexerError::UnknownWindow(window)
    );

    let events = mux.drain_events();
    assert!(events.contains(&muxnix::LifecycleEvent::PaneClosed { window, pane }));
    assert!(events.contains(&muxnix::LifecycleEvent::WindowClosed { window }));
    assert!(events.contains(&muxnix::LifecycleEvent::ActiveWindowChanged { window: None }));
}

#[test]
fn removing_window_closes_all_panes() {
    let mut mux = muxnix::Multiplexer::new();
    let (window, p1) = mux.create_window(&mut true_cmd(), SIZE).expect("window");
    let p2 = mux
        .create_pane(window, &mut true_cmd(), SIZE)
        .expect("pane 2");
    mux.drain_events();

    mux.remove_window(window).expect("remove window");
    assert_eq!(
        mux.pane(p1).expect_err("p1"),
        muxnix::MultiplexerError::UnknownPane(p1)
    );
    assert_eq!(
        mux.pane(p2).expect_err("p2"),
        muxnix::MultiplexerError::UnknownPane(p2)
    );

    let events = mux.drain_events();
    assert!(events.contains(&muxnix::LifecycleEvent::PaneClosed { window, pane: p1 }));
    assert!(events.contains(&muxnix::LifecycleEvent::PaneClosed { window, pane: p2 }));
    assert!(events.contains(&muxnix::LifecycleEvent::WindowClosed { window }));
}

#[test]
fn child_exit_becomes_lifecycle_event() {
    let mut mux = muxnix::Multiplexer::new();
    let (window, pane) = mux.create_window(&mut exit_cmd(7), SIZE).expect("window");
    mux.drain_events();

    wait_for_exit(&mut mux, pane, Duration::from_secs(5));
    let status = mux
        .pane(pane)
        .expect("pane still present")
        .exit_status()
        .expect("exit status");
    assert_eq!(status.code(), Some(7));

    let events = mux.drain_events();
    assert!(events.iter().any(|e| matches!(
        e,
        muxnix::LifecycleEvent::PaneExited {
            window: w,
            pane: p,
            status: s,
        } if *w == window && *p == pane && s.code() == Some(7)
    )));
}

#[test]
fn poll_exits_is_idempotent_for_reaped_pane() {
    let mut mux = muxnix::Multiplexer::new();
    let (_window, pane) = mux.create_window(&mut exit_cmd(0), SIZE).expect("window");
    mux.drain_events();
    wait_for_exit(&mut mux, pane, Duration::from_secs(5));
    mux.drain_events();

    mux.poll_exits().expect("poll again");
    assert!(mux.drain_events().is_empty());
}

#[test]
fn remove_after_exit_drops_pty_once() {
    let mut mux = muxnix::Multiplexer::new();
    let (window, pane) = mux.create_window(&mut exit_cmd(0), SIZE).expect("window");
    wait_for_exit(&mut mux, pane, Duration::from_secs(5));
    mux.remove_pane(pane).expect("remove exited pane");
    assert_eq!(
        mux.pane(pane).expect_err("gone"),
        muxnix::MultiplexerError::UnknownPane(pane)
    );
    assert_eq!(
        mux.window(window).expect_err("window gone"),
        muxnix::MultiplexerError::UnknownWindow(window)
    );
}

#[test]
fn remove_window_while_child_still_running() {
    let mut mux = muxnix::Multiplexer::new();
    let (window, pane) = mux.create_window(&mut sleep_cmd(), SIZE).expect("window");
    assert!(mux.pane(pane).expect("pane").exit_status().is_none());
    mux.remove_window(window).expect("remove running window");
    assert_eq!(
        mux.pane(pane).expect_err("pane dropped"),
        muxnix::MultiplexerError::UnknownPane(pane)
    );
}

#[test]
fn focus_rejects_pane_from_other_window() {
    let mut mux = muxnix::Multiplexer::new();
    let (w1, p1) = mux.create_window(&mut true_cmd(), SIZE).expect("w1");
    let (w2, p2) = mux.create_window(&mut true_cmd(), SIZE).expect("w2");
    assert_eq!(
        mux.set_focus(w1, p2).expect_err("cross window"),
        muxnix::MultiplexerError::PaneNotInWindow {
            pane: p2,
            window: w1,
        }
    );
    assert_eq!(mux.focus_pane(w1).expect("unchanged"), p1);
    assert_eq!(mux.focus_pane(w2).expect("w2"), p2);
}

#[test]
fn create_pane_on_unknown_window_fails() {
    let mut mux = muxnix::Multiplexer::new();
    let (window, _) = mux.create_window(&mut true_cmd(), SIZE).expect("window");
    mux.remove_window(window).expect("remove");
    let err = mux
        .create_pane(window, &mut true_cmd(), SIZE)
        .expect_err("unknown window");
    match err {
        muxnix::CreateError::Model(muxnix::MultiplexerError::UnknownWindow(id)) => {
            assert_eq!(id, window);
        }
        other => panic!("unexpected error: {other:?}"),
    }
}
