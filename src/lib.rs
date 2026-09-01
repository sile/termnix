//! A Unix-only foundation for building terminal multiplexers.
//!
//! `muxnix` provides PTY process lifecycle, an I/O-free terminal emulator,
//! logical input encoding, and a [`SessionDriver`] that owns several
//! PTY-backed terminal sessions and is driven from an external event loop,
//! without depending on an async runtime. Window, pane, and layout concepts
//! belong to the calling application; host terminal raw mode and final frame
//! rendering stay with the caller.

#![warn(missing_docs)]

mod driver;
mod input;
mod pty;
mod size;
mod snapshot;
mod terminal;
mod terminal_buffer;
mod terminal_emu;
mod terminal_scrollback;
mod terminal_types;

pub use driver::{
    DriveResult, DriverConfig, DriverError, DriverEvent, Interests, PollSource, PollSourceEntry,
    Readiness, RegistrationToken, SessionConfig, SessionDriver, SessionId, SessionStatus,
    ShutdownOutcome,
};
pub use input::{KeyCode, KeyEvent, Modifiers, MouseButton, encode_key, encode_paste};
pub use pty::{ClosingPtyProcess, ObservedExit, PtyProcess, SignalOutcome};
pub use size::Size;
pub use snapshot::{TerminalLine, TerminalSnapshot};
pub use terminal::{
    Cell, Color, MouseReporting, Position, Style, TerminalAction, TerminalModes, TerminalState,
};
pub use terminal_scrollback::ScrollbackLimits;
