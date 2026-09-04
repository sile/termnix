# termnix

Unix-only terminal session engine for Rust.

`termnix` provides an I/O-free terminal emulator, `Input` for session key /
paste / raw bytes, and one `Session` per PTY-backed child process (including
that child's lifecycle), driven from an external event loop without an async
runtime. Owning and scheduling multiple sessions, as well as window, pane,
and layout concepts, belong to the calling application. Host terminal raw mode
and final frame rendering stay with the caller.

## Terminal emulator coverage

`TerminalState` is a state machine: feed PTY bytes, read cells / cursor / modes,
and drain query replies as `TerminalAction` values. It never writes to a fd.

Supported in the modes milestone:

- C0: BEL (ignored), BS, HT, LF/VT/FF, CR
- ESC: IND, NEL, RI, DECSC/DECRC, RIS
- CSI cursor motion and addressing (CUU/CUD/CUF/CUB, CNL/CPL, CHA/HPA, VPA, CUP/HVP)
- CSI editing (ICH/DCH/IL/DL/ED/EL/ECH/SU/SD) and DECSTBM scroll regions
- SGR: bold, italic, underline, reverse, 16-color, 256-color, 24-bit color
- DEC private modes retained for later input encoding: application cursor/keypad,
  origin, autowrap, cursor visibility, bracketed paste, mouse reporting (+ SGR)
- Alternate screen (`?1049` / `?47` / `?1047`)
- OSC 0/2 window title (stored)
- DSR / CPR / primary DA replies via `TerminalAction::WritePty`

Explicitly out of scope: Sixel, Kitty graphics, iTerm2 images, and DCS payloads
(ignored without becoming visible text).

## Input

`Input` carries raw PTY bytes, a `KeyEvent`, or paste text.
`Session::enqueue_input` turns `Key` / `Paste` into bytes with the session's
current `TerminalModes` and appends them to the write queue; `Raw` is appended
unchanged. Compare `Input::byte_len` with
`Session::metrics().pending_write_bytes` when applying caller-side
backpressure. Mouse report bytes are not produced yet—only `MouseButton` is
defined for application-side routing (coordinates reuse `Position`).

## Snapshots and scrollback

`TerminalState::snapshot` returns an owned `TerminalSnapshot` (visible screen,
cursor, modes, style, title, alternate-screen flag, and primary-derived scrollback)
without I/O, so a consumer can render or retain the data while the session
keeps running. The primary screen's full-screen scrolls (LF/VT/FF, IND, NEL,
autowrap, CSI SU) retain displaced rows with no built-in cap; `trim_scrollback`
removes the oldest whole lines to caller-chosen line and cell bounds (either
bound at zero clears the history). Partial scroll regions, line edits,
scroll-downs, the alternate screen, and resize never enter history. CSI ED 2
clears the visible screen, CSI ED 3 clears only the scrollback, and RIS clears
both.

## Terminal session API

`Session` owns one PTY-backed terminal session. The caller registers
`Session::fd` with the interests from `Session::interests`, calls
`Session::pump_io` whenever the fd is ready (and after any state change), and
uses `Session::needs_pump` to know when more work is available without a new
readiness edge. The fd is non-blocking, so no readiness flags are passed to
the session; `pump_io` learns what is possible from its own `WouldBlock`
results, which keeps edge-triggered loops correct. One `pump_io` call is the
scheduling quantum: with a single session, drain `needs_pump` before blocking;
with several sessions, rotate among runnable ones so a chatty session cannot
starve the others.

```rust
// Single session: drain internal work before waiting in the poll loop.
session.pump_io()?;
while session.needs_pump() {
    session.pump_io()?;
}
reregister(session.fd(), session.interests());

// Multiple sessions: round-robin one quantum at a time.
let mut i = 0;
while sessions.iter().any(Session::needs_pump) {
    if sessions[i].needs_pump() {
        sessions[i].pump_io()?;
    }
    i = (i + 1) % sessions.len();
}
```

Application input (`Session::enqueue_input` with `Input`) and terminal replies
share one FIFO write queue: a reply is appended in chronological order behind
already accepted input, and decoding pauses until that reply is fully written,
so at most one bounded reply is ever pending. The write queue itself is
unbounded; the session applies no backpressure policy to application
input—compare `Input::byte_len` against
`Session::metrics().pending_write_bytes` to decide whether to enqueue, hold,
or drop. Child exit and PTY EOF are separate:
`try_wait` / `wait` cache the exit status without disabling I/O, so remaining
master-side output can still be drained until EOF. Use `close`, `terminate`,
`force_terminate`, and `shutdown` for teardown. `SessionMetrics` exposes
cumulative counters plus current and maximum values for the read buffer, write
queue, and scrollback.

## Examples

`examples/headless.rs` drives one `Session` with `libc::poll` and no host
terminal or UI—see that file's module docs for why it exists and what it
proves. Run `cargo run --quiet --example headless </dev/null`.

## Roadmap

No planned work is currently tracked.
