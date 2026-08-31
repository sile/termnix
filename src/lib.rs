//! A Unix-only foundation for building terminal multiplexers.
//!
//! `muxnix` provides PTY process lifecycle, an I/O-free terminal emulator, and
//! logical input encoding without depending on an async runtime. Window, pane,
//! and layout concepts belong to the calling application; host terminal raw
//! mode and final frame rendering stay with the caller.

#![warn(missing_docs)]

mod input;
mod pty;
mod size;
mod snapshot;
mod terminal;
mod terminal_buffer;
mod terminal_emu;
mod terminal_scrollback;
mod terminal_types;

pub use input::{KeyCode, KeyEvent, Modifiers, MouseButton, encode_key, encode_paste};
pub use pty::PtyProcess;
pub use size::Size;
pub use snapshot::{ActiveScreen, TerminalLine, TerminalSnapshot};
pub use terminal::{
    Cell, Color, MouseReporting, Position, Style, TerminalAction, TerminalModes, TerminalState,
};
pub use terminal_scrollback::ScrollbackLimits;
