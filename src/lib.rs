//! A Unix-only foundation for building terminal multiplexers.
//!
//! `muxnix` provides PTY process lifecycle, terminal emulation, window and pane
//! management, and input routing without depending on an async runtime.
//! Host terminal raw mode and final frame rendering are left to the caller.

#![warn(missing_docs)]

mod input;
mod multiplexer;
mod pty;
mod size;
mod terminal;
mod terminal_buffer;
mod terminal_emu;
mod terminal_types;

pub use input::{KeyCode, KeyEvent, Modifiers, MouseButton, encode_key, encode_paste};
pub use multiplexer::{LifecycleEvent, Multiplexer, Pane, PaneId, Window, WindowId};
pub use pty::PtyProcess;
pub use size::Size;
pub use terminal::{
    Cell, Color, MouseReporting, Position, Style, TerminalAction, TerminalModes, TerminalState,
};
