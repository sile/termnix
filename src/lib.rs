//! A Unix-only foundation for building terminal multiplexers.
//!
//! `termnix` provides PTY process lifecycle, an I/O-free terminal emulator,
//! logical input encoding, and a [`Session`] that owns one PTY-backed terminal
//! session and is driven from an external event loop, without depending on an
//! async runtime. Owning and scheduling several sessions, as well as window,
//! pane, and layout concepts, belong to the calling application; host terminal
//! raw mode and final frame rendering stay with the caller.

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
pub use pty::{ClosingPtyProcess, ObservedExit, PtyProcess, SignalOutcome};
pub use session::{
    DriveBudget, DriveResult, Interests, PollSourceEntry, Readiness, RegistrationToken, Session,
    SessionConfig, SessionError, SessionEvent, SessionStatus,
};
pub use size::Size;
pub use snapshot::{TerminalLine, TerminalSnapshot};
pub use terminal::{
    Cell, Color, MouseReporting, Position, Style, TerminalAction, TerminalModes, TerminalState,
};
