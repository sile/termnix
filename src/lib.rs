//! A Unix-only foundation for building terminal multiplexers.
//!
//! `termnix` provides an I/O-free terminal emulator, logical input encoding,
//! and a [`Session`] that owns one PTY-backed child process and its lifecycle
//! and is driven from an external event loop, without depending on an async
//! runtime. Owning and scheduling several sessions, as well as window, pane,
//! and layout concepts, belong to the calling application; host terminal raw
//! mode and final frame rendering stay with the caller.

#![warn(missing_docs)]

mod input;
mod pty;
mod session;
mod size;
mod snapshot;
mod terminal;
mod terminal_buffer;
mod terminal_emu;
mod terminal_scrollback;
mod terminal_types;

pub use input::{KeyCode, KeyEvent, Modifiers, MouseButton, encode_key, encode_paste};
pub use pty::SignalOutcome;
pub use session::{Interests, Session, SessionMetrics, SessionStatus};
pub use size::Size;
pub use snapshot::{TerminalLine, TerminalSnapshot};
pub use terminal::{
    Cell, Color, MouseReporting, Position, Style, TerminalAction, TerminalModes, TerminalState,
};
