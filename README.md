termnix
=======

[![Crates.io](https://img.shields.io/crates/v/termnix.svg)](https://crates.io/crates/termnix)
[![Documentation](https://docs.rs/termnix/badge.svg)](https://docs.rs/termnix)
[![Actions Status](https://github.com/sile/termnix/workflows/CI/badge.svg)](https://github.com/sile/termnix/actions)
![License](https://img.shields.io/crates/l/termnix)

A Unix-only library for PTY-backed child processes and terminal emulation.

`termnix` provides three things: sessions that own a child's pseudo-terminal,
an I/O-free terminal emulator that turns its bytes into a screen grid, and an
I/O model driven entirely from the caller's poll loop without an async
runtime. Multiplexing several sessions is one use of these primitives, not the
scope of the crate.

## What it does not own

The boundaries are the design:

- **No event loop.** The caller registers the session's fd and interests, and
  calls the pump method when it is ready. `termnix` never blocks on a poll.
- **No async runtime.** The fd is non-blocking; pumping discovers what is
  possible from its own `WouldBlock` results.
- **No scheduling policy.** The caller decides how to interleave sessions. One
  pump call is the scheduling quantum, bounded by a caller-supplied work
  budget, so a chatty child cannot starve the others.
- **No host terminal state.** Raw mode, the alternate screen, and rendering
  stay with the caller.
- **No window, pane, or layout concepts.** Those belong to the application.

## Terminal emulator

The emulator is a pure state machine: feed it PTY bytes, read back the screen
grid, cursor, modes, and style, and drain query replies as values for the
caller to write. It never touches a file descriptor.

It covers the cursor, editing, scroll-region, SGR, alternate-screen, and
DEC-private-mode sequences a real application needs, answers the common
queries, and ignores what it does not implement (Sixel, Kitty graphics,
iTerm2 images, DCS payloads) without letting those bytes become visible text.
See `TerminalState` in the API documentation for the exact list.

## Input and backpressure

Input is raw bytes, a decoded key event, or paste text; enqueuing the latter
two encodes them with the session's current modes. Application input and
terminal replies share one FIFO write queue, so replies are never reordered,
and the queue is unbounded: the session applies no backpressure policy of its
own. Compare the input size with the pending write bytes in the session metrics
to decide whether to enqueue, hold, or drop.

## Lifecycle

Child exit and PTY EOF are separate, so the exit status can be observed while
remaining master-side output is still drained. Exit status, EOF, and teardown
are independent, so the session exposes them as separate operations. Metrics
report cumulative counters plus current and maximum values for the read buffer,
the write queue, and scrollback.

Details for all of the above live in the API documentation.

## Example

The crate owns no loop, so a program is responsible for calling it whenever
there is work to do:

```rust,no_run
fn pump_once(session: &mut termnix::Session) -> std::io::Result<()> {
    // Register this fd with these interests in your own poll loop.
    let (_fd, _interests) = (session.fd(), session.interests());

    // Do the work a single readiness edge allows, without blocking.
    session.pump_io(termnix::PumpBudget::default())?;

    // Keep going while work remains, so a large backlog still drains.
    while session.needs_pump() {
        session.pump_io(termnix::PumpBudget::default())?;
    }

    // Send a keystroke to the child, holding off if writes are piling up.
    if session.metrics().pending_write_bytes < 4096 {
        let enter = termnix::KeyEvent::new(termnix::KeyCode::Enter);
        session.enqueue_input(termnix::Input::Key(enter))?;
    }
    Ok(())
}
```

## Examples

- [`examples/headless.rs`](examples/headless.rs) drives one `Session` with
  `libc::poll` and no host terminal or UI. Run it with
  `cargo run --quiet --example headless </dev/null`.
- [`examples/tuinix.rs`](examples/tuinix.rs) runs two sessions behind a host
  terminal built with [`tuinix`](https://crates.io/crates/tuinix), and shows
  how to bridge a snapshot into a host frame buffer.

## Roadmap

No planned work is currently tracked.
