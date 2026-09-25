# RFC: Say who delivers SIGWINCH on resize

- Status: accepted

## Summary

Make the resize contract explicit: `Session::resize()` sets the kernel winsize
with `TIOCSWINSZ`, so the kernel delivers `SIGWINCH` to the child, while
`TerminalState::resize()` only re-lays out the emulator grid and touches no
signal. The rustdoc on [`TerminalState::resize()`](../src/terminal.rs) currently
says the opposite in effect — "the child sees a `SIGWINCH` only when the caller
arranges it" — and a caller reading it through `Session` has no way to tell
which of the two paths it is on.

## Motivation

kk is a TUI text editor that drives itself from a `tuinix::TerminalDriver`,
which installs a `SIGWINCH` handler at construction and wakes its poll loop
when the signal arrives (see `tuinix`'s `resize_signal_fd()`). An end-to-end
test drives the real `kk` binary as a child of a `termnix::Session`, resizes
through the public API, and asserts on the redrawn grid:

```rust
let mut kk = KkHarness::open(&path);
kk.wait_for_text("hello");
kk.resize(40, 100);        // -> Session::resize() -> TIOCSWINSZ
kk.wait_for_text("hello"); // the child repainted at the new size
```

This works: the child receives `SIGWINCH` and repaints, with nothing else in the
caller's loop doing so. But it works for a reason the docs do not state. The
relevant wording is on the emulator method:

> Changing the visible size here is independent of the kernel's view of the
> window: the PTY's `TIOCSWINSZ` is the caller's to set, and the child sees a
> `SIGWINCH` only when the caller arranges it.

That sentence is true of `TerminalState::resize()` in isolation, and the test
above shows it is misleading about `Session::resize()`. A caller who reads it
while holding a `Session` reasonably concludes that resizing will not notify
the child, and writes manual signal delivery that is at best redundant and at
worst wrong (delivering `SIGWINCH` twice, or to the wrong process group).

The gap is not in behavior; the kernel already does the right thing. It is that
neither method's rustdoc states the delivery, so the caller cannot tell from the
API whether notification is a promise or an accident.

## Guide-level explanation

There are two resizes and they do different things:

- `Session::resize()` resizes the session. It sets the PTY's winsize with
  `TIOCSWINSZ` **and** updates the emulator grid to match. Because the kernel
  sees the winsize change on the controlling terminal, it delivers `SIGWINCH`
  to the child's process group. The caller does nothing extra.

  ```rust
  session.resize(Size::new(40, 100))?;
  // The child has been sent SIGWINCH and can repaint on its own loop.
  ```

- `TerminalState::resize()` resizes the emulator alone. It re-lays out the grid,
  clamps the cursor and scroll region, and repairs broken wide characters. It
  touches no file descriptor and sends no signal. A caller holding only a
  `TerminalState` — with no PTY at all, for instance a screen recorder replaying
  captured bytes — gets exactly this and no signal behavior.

A caller using `Session::resize()` therefore treats notification as part of the
operation. A caller using `TerminalState::resize()` directly is responsible for
whatever signal behavior its own setup requires, and the doc should say so in
those terms rather than implying the session case lacks notification too.

## Reference-level explanation

The change is to documentation, not behavior.

**`TerminalState::resize()`.** Scope the sentence to this method. It should say
that this call updates the emulator's grid only, that it does not touch the
kernel winsize and sends no signal, and that a caller that owns a PTY and wants
the child notified should set `TIOCSWINSZ` on it — which is what
`Session::resize()` does. The contrast is with the *emulator*, not with the
session: the point is that this method knows nothing about a kernel or a child,
not that notification is left undone somewhere.

**`Session::resize()`.** State the delivery as part of the contract. The
current doc says the method updates "both the kernel PTY size and the emulator"
and that the ioctl runs first, which is accurate but stops short of the
consequence. Add that setting the winsize is what causes the kernel to deliver
`SIGWINCH` to the child, so the child can repaint without the caller sending a
signal, and note when delivery does **not** happen: an already-closed session
returns `ErrorKind::BrokenPipe` and delivers nothing, and reapplying the
current size returns early without an ioctl, so it delivers nothing either.

That last case is worth a sentence because it is the one place a caller might
be surprised. "Reapplying the current size succeeds without a syscall" is
already documented; the consequence — no winsize change, hence no `SIGWINCH` —
is not, and it is the same fact stated twice for two audiences.

**No change to `Size`, the ioctl, or the emulator.** The kernel's behavior here
is standard `tty` behavior, not something termnix implements, so there is no
code path to point at beyond `set_winsize()` in `pty.rs`. The RFC does not
propose termnix start sending signals itself; see the alternatives.

## Drawbacks

- It is a documentation-only change, so it cannot be tested by asserting on
  behavior. The check is that the wording matches what `tuinix` and `kk`
  demonstrably rely on.
- Stating a delivery in the doc makes it look more like a promise than it is.
  It is the kernel's behavior, and a caller on an unusual platform could see
  something else. The wording should attribute it to the kernel ("setting the
  winsize is what makes the kernel deliver `SIGWINCH`") rather than claiming
  termnix guarantees it, so the doc stays honest about who owns the behavior.

## Rationale and alternatives

**Do nothing.** The current wording reads correctly for the emulator method's
own scope. The problem is that a `Session` caller cannot tell which scope
applies, and the natural reading is the wrong one. The fix is a few sentences,
so the cost of leaving it is a caller who writes redundant or incorrect signal
handling.

**Have `Session::resize()` send `SIGWINCH` explicitly after the ioctl.** This
duplicates what the kernel already does on `TIOCSWINSZ`, so the child would
receive the signal twice. It also moves termnix from "sets the winsize" into
"decides when the child is notified", which is policy the caller should own.

**Make notification optional, e.g. `Session::resize(size)` vs.
`Session::resize_quiet(size)`.** There is nothing to make optional: `TIOCSWINSZ`
is a single ioctl, and its signal delivery is not separable from setting the
size. Splitting the API would imply termnix controls delivery when it does
not.

**Document the delivery only on `Session::resize()` and leave the emulator
method alone.** Half-fixes the confusion, because the emulator sentence is what
a caller is likely to find first when searching for resize behavior. The two
methods' docs should state the same distinction from their own sides.

## Unresolved questions

- Should the emulator method's doc mention `SIGWINCH` at all, given the method
  itself has nothing to do with it? Naming it is what lets a reader connect the
  two methods; omitting it keeps the emulator doc purely about the grid. This
  RFC argues for naming it once, as a pointer to `Session::resize()`, but the
exact wording is open.
- Is there a place in `README.md` for this? The README describes the crate's
  boundaries and notes that host terminal state stays with the caller. Whether
  the winsize/`SIGWINCH` split belongs there or only in rustdoc is unsettled;
  rustdoc is the safer default, since the README is deliberately scoped.

## Future possibilities

If termnix ever grows a documented answer for "child did not resize", such as
observing that the child asked for the size again, that would build on this
distinction rather than replace it: the delivery would still be the kernel's,
and the new fact would sit alongside it. Nothing here proposes such a feature.

## Outcome

Fill this in only when the proposal is settled.

## Outcome

Implemented in [#14](https://github.com/sile/termnix/pull/14) (merged as `0646f64`).

Implemented in [#14](https://github.com/sile/termnix/pull/14) (merged as `0646f64`).

`TerminalState::resize()` no longer says the child sees a `SIGWINCH` only when the
caller arranges it. It now states that the method re-lays out the emulator only —
no file descriptor, no kernel winsize, no signal — and points at `Session::resize()`
for a caller that owns a PTY. `Session::resize()` states that setting the winsize
is what makes the kernel deliver `SIGWINCH` to the child, attributes the delivery
to the kernel rather than to termnix, and names the two cases that deliver none: a
closed session (`ErrorKind::BrokenPipe`) and reapplying the current size (early
return, no ioctl).

No behavior changed; `Size`, the ioctl, and the emulator are untouched. The scope
is unchanged from what is described above.

The scope is unchanged from what is described above.
