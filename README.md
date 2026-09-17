termnix
=======

[![Crates.io](https://img.shields.io/crates/v/termnix.svg)](https://crates.io/crates/termnix)
[![Documentation](https://docs.rs/termnix/badge.svg)](https://docs.rs/termnix)
[![Actions Status](https://github.com/sile/termnix/workflows/CI/badge.svg)](https://github.com/sile/termnix/actions)
![License](https://img.shields.io/crates/l/termnix)

A Unix-only library for PTY-backed child processes and terminal emulation.

The crate owns three things: the pseudo-terminal of each child process, an
I/O-free emulator that turns its bytes into a screen grid, and an I/O model
driven from the caller's own poll loop. What it leaves to the caller is just
as much a part of the design. Multiplexing several sessions is one use of
these primitives, not the scope of the crate.

## Why caller-owned I/O

Nothing runs behind the caller's back. The caller owns the only poll loop;
the crate owns no loop of its own. Each pump call does a bounded amount of
work, and what it leaves undone is readable from the session itself, so
fairness, backpressure, and redraw timing stay decisions the caller makes with
full information.

That is what makes the crate fit where a heavier terminal library would not. A
TUI that owns its rendering, a test harness that drives interactive commands, a
scraper that reads progress out of a program that insists on a terminal, a
recorder that captures what a command would have displayed, or a multiplexer
that interleaves several sessions in one loop. The emulator can be used on its
own, with no child process at all, and the PTY machinery on its own, with no
screen.

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

## The pieces

### Terminal emulator

The emulator is a pure state machine. Feed it PTY bytes and read back the
screen grid, cursor, modes, and style; query replies come back as values for
the caller to write. It never touches a file descriptor. It implements cursor,
editing, SGR, and alternate-screen sequences, and swallows the sequences it does
not implement instead of turning them into visible text. See the
[`TerminalState`][] API documentation for the exact list.

### Input and backpressure

Input is raw bytes, a decoded key event, or paste text. The latter two are
encoded with the session's current modes. Application input and terminal
replies share one FIFO write queue, so replies are never reordered. The queue
is unbounded: the session applies no backpressure policy of its own. Compare
the unwritten bytes reported by [`Session::counters()`][] against the input
size to decide whether to enqueue, hold, or drop.

### Lifecycle

Child exit and PTY EOF are tracked separately, so remaining output can still be
drained after the exit status is observed. [`Session::counters()`][] reports
the session's running totals plus peak values, including how many bytes are
still waiting to be decoded or written.

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
    if session.counters().unwritten() < 4096 {
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
  how to bridge a terminal state into a host frame buffer.

[`Session::counters()`]: https://docs.rs/termnix/latest/termnix/struct.Session.html#method.counters
[`TerminalState`]: https://docs.rs/termnix/latest/termnix/struct.TerminalState.html
