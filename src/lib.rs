//! A Unix-only foundation for building terminal multiplexers.
//!
//! `muxnix` provides PTY process lifecycle, terminal emulation, window and pane
//! management, and input routing without depending on an async runtime.
//! Host terminal raw mode and final frame rendering are left to the caller.

#![warn(missing_docs)]

mod pty;
mod size;
mod terminal;

pub use pty::PtyProcess;
pub use size::Size;
pub use terminal::{Cell, Position, Terminal};
