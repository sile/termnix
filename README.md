# termnix

Unix-only terminal session engine for Rust.

`termnix` provides PTY process lifecycle and an I/O-free terminal emulator for
building terminal sessions. Each session represents one PTY-backed child
process. Owning and scheduling multiple sessions, as well as window, pane, and
layout concepts, belong to the calling application. Host terminal raw mode and
final frame rendering stay with the caller.

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

## Input encoding

`encode_key` and `encode_paste` turn logical input into PTY bytes. Modes come
from an explicit `TerminalModes` argument (for example from the destination
terminal state); host focus is never implied. Plain text and raw bytes are
written by the caller. Mouse report bytes are not encoded yet—only
`MouseButton` is defined for application-side routing (coordinates reuse
`Position`).

## Snapshots and scrollback

`TerminalState::snapshot` returns an owned `TerminalSnapshot` (visible screen,
cursor, modes, style, title, alternate-screen flag, and primary-derived scrollback)
without I/O, so a consumer can render or retain the data while the session
keeps running. `TerminalState::with_scrollback` enables bounded scrollback:
the primary screen's full-screen scrolls (LF/VT/FF, IND, NEL, autowrap, CSI SU)
retain displaced rows up to `ScrollbackLimits` (line and cell bounds; either
bound at zero disables history), evicting the oldest complete lines first. Partial scroll regions, line edits,
scroll-downs, the alternate screen, and resize never enter history. CSI ED 2
clears the visible screen, CSI ED 3 clears only the scrollback, and RIS clears
both.

## Terminal session API

`Session` owns one PTY-backed terminal session and is driven from an external
event loop without an async runtime. The caller registers the session's fd
(`Session::poll_source`), feeds readiness back (`Session::drive` with a
per-drive `DriveBudget`), and reads lifecycle events and the poll source
changes from the result. Owning, identifying, and scheduling several sessions
is the caller's responsibility; bounded read/write, terminal replies,
mode-aware input encoding, resize, and the child-process lifecycle stay inside
the session.

## Roadmap

A public API for updating scrollback limits while a session is running is
planned but not yet implemented.
