# muxnix

Unix-only foundation for building terminal multiplexers in Rust.

`muxnix` owns PTY process lifecycle and an I/O-free terminal emulator. Window and
pane management and input routing will follow. Host terminal raw mode and final
frame rendering stay with the caller.

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
pane); host focus is never implied. Plain text and raw bytes are written by the
caller. Mouse report bytes are not encoded yet—only `MouseButton` is defined for
later pane routing (coordinates reuse `Position`).

