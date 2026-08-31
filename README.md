# muxnix

Unix-only foundation for building terminal multiplexers in Rust.

`muxnix` owns PTY process lifecycle and an I/O-free terminal emulator. Window,
pane, and layout concepts belong to the calling application. Host terminal raw
mode and final frame rendering stay with the caller.

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

## Roadmap

A multi-session driver that integrates PTY readiness, bounded I/O, and process
lifecycle for use from an external event loop is planned but not yet
implemented.
