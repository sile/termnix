//! Window and pane containment for a process-local multiplexer.
//!
//! A [`Multiplexer`] owns windows; each [`Window`] owns one or more pane
//! identifiers; each [`Pane`] owns a [`PtyProcess`](crate::PtyProcess) and a
//! [`TerminalState`](crate::TerminalState). Layout geometry is out of scope
//! here and is added later.
//!
//! # Empty multiplexer
//!
//! [`Multiplexer::new`] starts with no windows. Deleting the last window leaves
//! an empty multiplexer (`active_window` is [`None`]). Empty windows are not
//! allowed: removing the last pane of a window also removes that window.

use std::{
    collections::{HashMap, VecDeque},
    io,
    process::{Command, ExitStatus},
};

use crate::pty::PtyProcess;
use crate::size::Size;
use crate::terminal::TerminalState;

/// Stable identifier for a window within one [`Multiplexer`] instance.
///
/// Identifiers are never reused for a different window in the same instance, so
/// a stale value cannot resolve to a newly created window.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct WindowId(u64);

/// Stable identifier for a pane within one [`Multiplexer`] instance.
///
/// Identifiers are never reused for a different pane in the same instance, so a
/// stale value cannot resolve to a newly created pane.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct PaneId(u64);

/// Lifecycle changes produced by model mutations and child-process reaping.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LifecycleEvent {
    /// A window was created.
    WindowCreated {
        /// New window.
        window: WindowId,
    },
    /// A window was removed (including after its last pane was removed).
    WindowClosed {
        /// Closed window.
        window: WindowId,
    },
    /// A pane was created.
    PaneCreated {
        /// Window that owns the pane.
        window: WindowId,
        /// New pane.
        pane: PaneId,
    },
    /// A pane was removed from the model.
    PaneClosed {
        /// Window that owned the pane.
        window: WindowId,
        /// Closed pane.
        pane: PaneId,
    },
    /// The child process of a pane exited (reaped via [`Multiplexer::poll_exits`]).
    PaneExited {
        /// Window that owns the pane.
        window: WindowId,
        /// Pane whose child exited.
        pane: PaneId,
        /// Exit status of the child.
        status: ExitStatus,
    },
    /// The multiplexer's active window changed.
    ActiveWindowChanged {
        /// New active window, or [`None`] when the multiplexer is empty.
        window: Option<WindowId>,
    },
    /// A window's focus pane changed.
    FocusPaneChanged {
        /// Window whose focus changed.
        window: WindowId,
        /// New focus pane.
        pane: PaneId,
    },
}

/// A pane: one PTY child and its terminal emulator state.
#[derive(Debug)]
pub struct Pane {
    id: PaneId,
    pty: PtyProcess,
    terminal: TerminalState,
    exit_status: Option<ExitStatus>,
}

impl Pane {
    /// Returns this pane's identifier.
    pub fn id(&self) -> PaneId {
        self.id
    }

    /// Returns the PTY process for this pane.
    pub fn pty(&self) -> &PtyProcess {
        &self.pty
    }

    /// Returns a mutable PTY process for this pane.
    pub fn pty_mut(&mut self) -> &mut PtyProcess {
        &mut self.pty
    }

    /// Returns the terminal emulator state for this pane.
    pub fn terminal(&self) -> &TerminalState {
        &self.terminal
    }

    /// Returns a mutable terminal emulator state for this pane.
    pub fn terminal_mut(&mut self) -> &mut TerminalState {
        &mut self.terminal
    }

    /// Returns the child exit status if it has already been reaped.
    pub fn exit_status(&self) -> Option<ExitStatus> {
        self.exit_status
    }
}

/// A window: an ordered set of panes and a per-window focus pane.
#[derive(Debug)]
pub struct Window {
    id: WindowId,
    pane_ids: Vec<PaneId>,
    focus: PaneId,
}

impl Window {
    /// Returns this window's identifier.
    pub fn id(&self) -> WindowId {
        self.id
    }

    /// Returns the focused pane in this window.
    pub fn focus(&self) -> PaneId {
        self.focus
    }

    /// Returns pane identifiers in creation order.
    pub fn panes(&self) -> impl Iterator<Item = PaneId> + '_ {
        self.pane_ids.iter().copied()
    }

    /// Returns the number of panes in this window.
    pub fn pane_count(&self) -> usize {
        self.pane_ids.len()
    }
}

/// Process-local collection of windows and panes.
///
/// Focus is stored per window. Switching the active window does not clear each
/// window's focus pane. Input routing that ignores focus is handled by later
/// driver APIs that accept an explicit [`PaneId`].
#[derive(Debug)]
pub struct Multiplexer {
    windows: HashMap<WindowId, Window>,
    window_order: Vec<WindowId>,
    panes: HashMap<PaneId, Pane>,
    pane_window: HashMap<PaneId, WindowId>,
    active: Option<WindowId>,
    next_window_id: u64,
    next_pane_id: u64,
    events: VecDeque<LifecycleEvent>,
}

impl Default for Multiplexer {
    fn default() -> Self {
        Self::new()
    }
}

impl Multiplexer {
    /// Creates an empty multiplexer with no windows or panes.
    pub fn new() -> Self {
        Self {
            windows: HashMap::new(),
            window_order: Vec::new(),
            panes: HashMap::new(),
            pane_window: HashMap::new(),
            active: None,
            next_window_id: 1,
            next_pane_id: 1,
            events: VecDeque::new(),
        }
    }

    /// Returns whether this multiplexer has no windows.
    pub fn is_empty(&self) -> bool {
        self.windows.is_empty()
    }

    /// Returns the number of windows.
    pub fn window_count(&self) -> usize {
        self.windows.len()
    }

    /// Returns window identifiers in creation order.
    pub fn windows(&self) -> impl Iterator<Item = WindowId> + '_ {
        self.window_order.iter().copied()
    }

    /// Returns the active window, if any.
    pub fn active_window(&self) -> Option<WindowId> {
        self.active
    }

    /// Returns a shared reference to a window.
    pub fn window(&self, id: WindowId) -> Option<&Window> {
        self.windows.get(&id)
    }

    /// Returns a shared reference to a pane.
    pub fn pane(&self, id: PaneId) -> Option<&Pane> {
        self.panes.get(&id)
    }

    /// Returns a mutable reference to a pane.
    pub fn pane_mut(&mut self, id: PaneId) -> Option<&mut Pane> {
        self.panes.get_mut(&id)
    }

    /// Returns the window that owns `pane`.
    pub fn window_of_pane(&self, pane: PaneId) -> Option<WindowId> {
        self.pane_window.get(&pane).copied()
    }

    /// Returns the focus pane of `window`.
    pub fn focus_pane(&self, window: WindowId) -> Option<PaneId> {
        self.window(window).map(|w| w.focus)
    }

    /// Creates a window containing one pane that runs `command`.
    ///
    /// The new window becomes active. The new pane becomes that window's focus.
    /// `size` is used for both the PTY window size and the initial terminal
    /// screen (layout-driven sizes arrive in a later milestone).
    pub fn create_window(
        &mut self,
        command: &mut Command,
        size: Size,
    ) -> io::Result<(WindowId, PaneId)> {
        let window_id = self.alloc_window_id();
        let pane_id = self.alloc_pane_id();
        let pane = self.spawn_pane(pane_id, command, size)?;

        let window = Window {
            id: window_id,
            pane_ids: vec![pane_id],
            focus: pane_id,
        };
        self.windows.insert(window_id, window);
        self.window_order.push(window_id);
        self.panes.insert(pane_id, pane);
        self.pane_window.insert(pane_id, window_id);

        self.push_event(LifecycleEvent::WindowCreated { window: window_id });
        self.push_event(LifecycleEvent::PaneCreated {
            window: window_id,
            pane: pane_id,
        });
        self.set_active(Some(window_id));
        Ok((window_id, pane_id))
    }

    /// Creates a pane in `window` that runs `command`.
    ///
    /// The new pane becomes the window's focus. The active window is unchanged.
    /// Returns [`None`] if `window` does not exist.
    pub fn create_pane(
        &mut self,
        window: WindowId,
        command: &mut Command,
        size: Size,
    ) -> io::Result<Option<PaneId>> {
        if !self.windows.contains_key(&window) {
            return Ok(None);
        }
        let pane_id = self.alloc_pane_id();
        let pane = self.spawn_pane(pane_id, command, size)?;

        let win = self.windows.get_mut(&window).expect("window checked above");
        win.pane_ids.push(pane_id);
        win.focus = pane_id;
        self.panes.insert(pane_id, pane);
        self.pane_window.insert(pane_id, window);

        self.push_event(LifecycleEvent::PaneCreated {
            window,
            pane: pane_id,
        });
        self.push_event(LifecycleEvent::FocusPaneChanged {
            window,
            pane: pane_id,
        });
        Ok(Some(pane_id))
    }

    /// Makes `window` the active window.
    ///
    /// Returns `false` if `window` does not exist.
    pub fn select_window(&mut self, window: WindowId) -> bool {
        if !self.windows.contains_key(&window) {
            return false;
        }
        self.set_active(Some(window));
        true
    }

    /// Sets the focus pane of `window` to `pane`.
    ///
    /// Returns `false` if `pane` is missing or does not belong to `window`.
    pub fn set_focus(&mut self, window: WindowId, pane: PaneId) -> bool {
        let Some(owner) = self.window_of_pane(pane) else {
            return false;
        };
        if owner != window {
            return false;
        }
        let Some(win) = self.windows.get_mut(&window) else {
            return false;
        };
        if win.focus != pane {
            win.focus = pane;
            self.push_event(LifecycleEvent::FocusPaneChanged { window, pane });
        }
        true
    }

    /// Removes `pane` and drops its PTY resources once.
    ///
    /// If it was the last pane in its window, the window is removed as well.
    /// If it was the focused pane, another remaining pane (if any) becomes
    /// focus. If the window was active and is removed, another window becomes
    /// active, or the multiplexer becomes empty.
    ///
    /// Returns `false` if `pane` does not exist.
    pub fn remove_pane(&mut self, pane: PaneId) -> bool {
        let Some(window) = self.window_of_pane(pane) else {
            return false;
        };
        if !self.windows.contains_key(&window) {
            return false;
        }

        let (was_last, new_focus) = {
            let win = self.windows.get_mut(&window).expect("window checked above");
            let idx = win
                .pane_ids
                .iter()
                .position(|&id| id == pane)
                .expect("pane_window and window.pane_ids stay in sync");
            win.pane_ids.remove(idx);
            let was_last = win.pane_ids.is_empty();
            let new_focus = if was_last {
                None
            } else if win.focus == pane {
                // Prefer the pane that followed the removed one; wrap to first.
                Some(win.pane_ids[idx.min(win.pane_ids.len() - 1)])
            } else {
                None
            };
            if let Some(focus) = new_focus {
                win.focus = focus;
            }
            (was_last, new_focus)
        };

        if let Some(focus) = new_focus {
            self.push_event(LifecycleEvent::FocusPaneChanged {
                window,
                pane: focus,
            });
        }

        self.panes.remove(&pane).expect("pane exists");
        self.pane_window.remove(&pane);
        self.push_event(LifecycleEvent::PaneClosed { window, pane });

        if was_last {
            self.remove_window_inner(window);
        }
        true
    }

    /// Removes `window` and all of its panes, dropping each PTY once.
    ///
    /// Returns `false` if `window` does not exist.
    pub fn remove_window(&mut self, window: WindowId) -> bool {
        if !self.windows.contains_key(&window) {
            return false;
        }
        let pane_ids: Vec<PaneId> = self
            .windows
            .get(&window)
            .expect("checked above")
            .pane_ids
            .clone();
        for pane in pane_ids {
            self.panes.remove(&pane).expect("pane exists");
            self.pane_window.remove(&pane);
            self.push_event(LifecycleEvent::PaneClosed { window, pane });
        }
        self.remove_window_inner(window);
        true
    }

    /// Reaps exited children without blocking and records [`LifecycleEvent::PaneExited`].
    ///
    /// Panes remain in the model after exit until [`Multiplexer::remove_pane`]
    /// or [`Multiplexer::remove_window`] drops them. A pane that already has an
    /// exit status is skipped.
    pub fn poll_exits(&mut self) -> io::Result<()> {
        let mut exited = Vec::new();
        for (pane_id, pane) in &mut self.panes {
            if pane.exit_status.is_some() {
                continue;
            }
            if let Some(status) = pane.pty.try_wait()? {
                pane.exit_status = Some(status);
                exited.push((*pane_id, status));
            }
        }
        for (pane_id, status) in exited {
            let window = self
                .pane_window
                .get(&pane_id)
                .copied()
                .expect("pane_window tracks live panes");
            self.push_event(LifecycleEvent::PaneExited {
                window,
                pane: pane_id,
                status,
            });
        }
        Ok(())
    }

    /// Removes and returns queued lifecycle events in order.
    pub fn drain_events(&mut self) -> Vec<LifecycleEvent> {
        self.events.drain(..).collect()
    }

    fn spawn_pane(&self, id: PaneId, command: &mut Command, size: Size) -> io::Result<Pane> {
        let pty = PtyProcess::spawn(command, size)?;
        let terminal = TerminalState::new(size);
        Ok(Pane {
            id,
            pty,
            terminal,
            exit_status: None,
        })
    }

    fn alloc_window_id(&mut self) -> WindowId {
        let id = WindowId(self.next_window_id);
        self.next_window_id = self.next_window_id.saturating_add(1);
        id
    }

    fn alloc_pane_id(&mut self) -> PaneId {
        let id = PaneId(self.next_pane_id);
        self.next_pane_id = self.next_pane_id.saturating_add(1);
        id
    }

    fn push_event(&mut self, event: LifecycleEvent) {
        self.events.push_back(event);
    }

    fn set_active(&mut self, window: Option<WindowId>) {
        if self.active != window {
            self.active = window;
            self.push_event(LifecycleEvent::ActiveWindowChanged { window });
        }
    }

    fn remove_window_inner(&mut self, window: WindowId) {
        self.windows.remove(&window).expect("window exists");
        self.window_order.retain(|&id| id != window);
        self.push_event(LifecycleEvent::WindowClosed { window });

        if self.active == Some(window) {
            let next = self.window_order.first().copied();
            self.set_active(next);
        }
    }
}
