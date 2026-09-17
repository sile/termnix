//! A Unix-only foundation for building terminal multiplexers.
//!
//! `termnix` provides an I/O-free terminal emulator, [`Input`] for session
//! key / paste / raw bytes, and a [`Session`] that owns one PTY-backed child
//! process and its lifecycle and is driven from an external event loop,
//! without depending on an async runtime. Owning and scheduling several
//! sessions, as well as window, pane, and layout concepts, belong to the
//! calling application; host terminal raw mode and final frame rendering stay
//! with the caller.

#![warn(missing_docs)]
#![deny(unsafe_code)]

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

pub use input::{Input, KeyCode, KeyEvent, Modifiers, MouseButton};
pub use pty::SignalOutcome;
pub use session::{Interests, PumpBudget, Session, SessionCounters, SessionStatus};
pub use size::Size;
pub use snapshot::{ScrollbackLine, TerminalSnapshot};
pub use terminal::{
    Cell, Color, MouseReporting, Position, Style, TerminalAction, TerminalModes, TerminalState,
};

/// Compiles the code example in `README.md` as a doctest so it cannot drift
/// away from the API.
///
/// The example is marked `no_run`: it needs a live PTY.
#[cfg(doctest)]
#[doc = include_str!("../README.md")]
struct ReadmeDoctests;
