# RFC: Expose the child's process id on a session

- Status: draft

## Summary

Add an accessor that reports the direct child's process id, so a caller that
embeds a session can ask operating-system questions about that child — the most
immediate being "what is its current working directory?" — without termnix
having to grow an API for each such question.

## Motivation

A host application that drives several child sessions often wants a new child to
start where an existing one is: a terminal multiplexer that opens a new sheet in
the directory the focused sheet is sitting in, or a tool launcher that inherits
the shell's location. The information lives in the operating system, not in the
terminal, so the natural way to read it is to name the child and ask the OS.

Today the child is reachable only through termnix's own lifecycle API:
[`Session::fd()`](crate::Session::fd) exposes the PTY master, and the exit
methods expose a status, but nothing names the process itself. There is no way
for a caller to learn the pid, so it cannot, for example, read the process's
current working directory out of a `/proc/<pid>/cwd` symlink. The caller is
left to invent its own bookkeeping — usually by pre-arranging a directory on the
command it hands in, which only covers "where it was told to start", not "where
it has moved to".

The gap is a missing *question*, not a missing capability: a process id is the
smallest fact that lets the caller ask the operating system anything at all
about the child, and because the child is a session leader, the pid is also its
process group id.

## Guide-level explanation

```rust
let mut session = Session::new(&mut command, size)?;
if let Some(pid) = session.child_pid() {
    // The child is a session leader, so `pid` is also its process group id.
    let cwd = std::fs::read_link(format!("/proc/{pid}/cwd"));
    // ...
}
```

The caller that already holds a `Session` gains one method. It needs no new
poll interest, no readiness edge, and no ownership change: the pid is available
from the moment the session is spawned, and it is a plain value like the fd or
the exit status.

## Reference-level explanation

```rust
impl Session {
    /// Returns the direct child's process id, or `None` once it is gone.
    pub fn child_pid(&self) -> Option<u32>;
}
```

- The value is the pid of the direct child, which equals its process group id
  because the child is a session leader (`setsid` runs before `exec`).
- `None` once the session no longer owns a live PTY: after the child is reaped
  and the session has moved to `Closing`/`Reaped`, and — for symmetry with
  [`Session::fd()`](crate::Session::fd) — while the session is closing. A
  session in `Eof` (the child has exited but its output is still being drained)
  still reports the pid, exactly as it still reports the fd.
- The method takes `&self`, performs no I/O, and does not reap or probe the
  child. A child that has exited but not yet been reaped still has a pid; a
  caller that wants "is it still running" pairs this with
  [`Session::exit_status()`](crate::Session::exit_status) or
  [`Session::try_wait()`](crate::Session::try_wait), exactly as for the fd.
- The pid is *not* replayed or cached in a way that survives reaping: the
  question is only meaningful while the process exists, and returning a stale
  pid after reaping would invite the caller to read a recycled process's
  `/proc` entry.

The accessor is deliberately a bare pid rather than a convenience such as
`current_dir()`. Reading `/proc` is Linux- and Termux-specific, it can fail or
race (a process that changes directory between two reads), and every caller
wants a different slice of it: the environment, the cwd, the executable path,
resource usage. Naming the child and letting the caller ask is the smallest
surface that serves all of them and keeps OS policy out of a terminal emulator.

## Drawbacks

- It exposes an operating-system handle through an API that has otherwise stayed
  on the PTY side of the boundary. A caller could use the pid to signal the
  child directly, bypassing [`Session::terminate()`](crate::Session::terminate)
  and its ownership rules; nothing stops that, but nothing needs to — the pid
  was always obtainable by other means, and termnix does not attempt to be a
  security boundary between the caller and its own child.
- It is one more method on the public surface, and it invites follow-up requests
  for `current_dir()`, `exe()`, and so on; the rationale above is the answer to
  those, and it should be repeated when they come.

## Rationale and alternatives

- **Do nothing.** The caller pre-arranges the directory on the command it
  spawns. This only covers the *initial* directory and cannot follow a child
  that has `cd`-ed, which is the actual use case (a shell sheet that has moved).
- **Add `Session::current_dir() -> io::Result<PathBuf>`.** Convenient, but it
  hard-codes procfs, turns a portable crate into a Linux-only one at that
  method, and picks one of many questions a caller may have. Rejected in favour
  of the general fact.
- **Add `Session::child_pid()` returning `u32` (chosen).** Minimal, portable
  (a pid means the same thing everywhere termnix runs), and leaves the OS
  question to the caller.
- **Expose the pid on `Session::status()` or a new `SessionInfo` struct.**
  Considered, but the pid is stable and cheap, unlike status which changes; a
  dedicated method matches [`Session::fd()`](crate::Session::fd).

## Unresolved questions

- Should the return type be `u32` or the platform's `pid_t`? `u32` keeps the
  signature identical on every target and matches Rust's `std::process::id()`;
  `pid_t` is `i32` on Unix and would leak a libc type (and the crate's `libc`
  dependency) into the public API. Resolved to `u32`: the child's id is already a
  `std::process::Child::id()` (`u32`), and `pid_t` conversions stay internal.
- Whether `None` should also be returned while the session is `Eof` (the child
  has exited but its output is still being drained). Resolved to `Some`, for
  symmetry with [`Session::fd()`](crate::Session::fd): that method returns
  `None` only for `Closing`/`Reaped`, so an `Eof` session still reports an fd —
  and its child pid is likewise valid and unreaped.

## Future possibilities

- A caller can build cwd-, env-, or resource-reading helpers on top without
  termnix changing again.
- If several such helpers turn out to be wanted by most callers, a small
  opt-in `SessionInfo` could be layered on later without disturbing this method.
