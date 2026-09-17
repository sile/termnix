termnix
=======

[![Crates.io](https://img.shields.io/crates/v/termnix.svg)](https://crates.io/crates/termnix)
[![Documentation](https://docs.rs/termnix/badge.svg)](https://docs.rs/termnix)
[![Actions Status](https://github.com/sile/termnix/workflows/CI/badge.svg)](https://github.com/sile/termnix/actions)
![License](https://img.shields.io/crates/l/termnix)

A Unix-only foundation for building terminal multiplexers.

`termnix` provides an I/O-free terminal emulator, logical `Input`, and one
PTY-backed `Session` per child process, all driven from the caller's event loop
without an async runtime.

## What it does not own

The boundaries are the design:

- **No event loop.** The caller registers `Session::fd` with the interests from
  `Session::interests`, and calls `Session::pump_io` when the fd is ready.
  `termnix` never blocks on a poll.
- **No async runtime.** The fd is non-blocking; `pump_io` discovers what is
  possible from its own `WouldBlock` results, which keeps edge-triggered loops
  correct.
- **No scheduling policy.** One `pump_io` call is the scheduling quantum. The
  caller decides how to interleave sessions; the work one call may do is
  bounded by a caller-supplied `PumpBudget`, so a chatty child cannot starve
  the others.
- **No host terminal state.** Raw mode, the alternate screen, and final frame
  rendering stay with the caller.
- **No window, pane, or layout concepts.** Those belong to the application.

## Terminal emulator coverage

`TerminalState` is a state machine: feed PTY bytes, read cells, cursor, and
modes, and drain query replies as `TerminalAction` values. It never writes to
a file descriptor.

- **C0**: BEL (ignored), BS, HT, LF/VT/FF, CR
- **ESC**: IND, NEL, RI, DECSC/DECRC, RIS
- **CSI cursor**: CUU/CUD/CUF/CUB, CNL/CPL, CHA/HPA, VPA, CUP/HVP
- **CSI editing**: ICH/DCH/IL/DL/ED/EL/ECH/SU/SD, DECSTBM scroll regions
- **SGR**: bold, italic, underline, reverse, 16/256/24-bit color
- **DEC private modes**: application cursor/keypad, origin, autowrap, cursor
  visibility, bracketed paste, mouse reporting (including SGR)
- **Alternate screen**: `?1049`, `?47`, `?1047`
- **OSC 0/2**: window title (stored)
- **Queries**: DSR, CPR, and primary DA, answered through
  `TerminalAction::WritePty`

Explicitly out of scope: Sixel, Kitty graphics, iTerm2 images, and DCS
payloads, which are ignored without becoming visible text.

## Input and backpressure

`Input` carries raw PTY bytes, a `KeyEvent`, or paste text.
`Session::enqueue_input` turns `Key` / `Paste` into bytes using the session's
current `TerminalModes` and appends them to the write queue; `Raw` is appended
unchanged.

Application input and terminal replies share one FIFO write queue, so a reply
never overtakes previously accepted input and at most one bounded reply is
pending. The queue itself is unbounded: the session applies no backpressure
policy. Compare `Input::byte_len` with
`Session::metrics().pending_write_bytes` to decide whether to enqueue, hold, or
drop. Mouse report bytes are not produced yet—only `MouseButton` is defined, so
application-side routing can share button identity.

## Lifecycle

Child exit and PTY EOF are separate. `try_wait` / `wait` cache the exit status
without disabling I/O, so remaining master-side output can still be drained
until EOF. Use `close`, `terminate`, `force_terminate`, and `shutdown` for
teardown. `SessionMetrics` exposes cumulative counters plus current and maximum
values for the read buffer, the write queue, and scrollback.

## Example

```rust,no_run
use termnix::{Input, KeyCode, KeyEvent, PumpBudget, Session};

fn pump(session: &mut Session) -> std::io::Result<()> {
    // Drain work before blocking in the caller's poll loop.
    session.pump_io(PumpBudget::default())?;
    while session.needs_pump() {
        session.pump_io(PumpBudget::default())?;
    }

    // Register the fd with these interests, then poll it outside this crate.
    let _ = (session.fd(), session.interests());

    if session.metrics().pending_write_bytes < 4096 {
        session.enqueue_input(Input::Key(KeyEvent::new(KeyCode::Enter)))?;
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
