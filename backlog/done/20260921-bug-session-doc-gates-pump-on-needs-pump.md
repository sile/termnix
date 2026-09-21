# Bug: the `Session` crate example gates the first pump on `needs_pump()`

- Status: fixed

## Summary

The crate-level example in `src/session.rs` opens the loop with
`while session.needs_pump() { pump_io() }`, so the first `pump_io` of a round is
skipped whenever `needs_pump()` is false. That is exactly the state left behind
by a `read` that hit `WouldBlock`, and only `pump_io` itself clears it, so a
caller that copies the example reads nothing and spins on a readable fd. The
same crate documents the opposite shape fifteen lines of comment above it
("Step 1 must come first"), shows the unconditional first pump in `README.md`,
and writes it that way in `tests/session.rs::pump_all`.

## Reproduction

Copy the crate example into a loop that polls `session.fd()` for
`session.interests()`:

```text
while session.needs_pump() { session.pump_io(termnix::PumpBudget::default())?; }
let interests = session.interests();
poll(session.fd(), interests);

step 1: pump_io -> read -> EAGAIN -> read_would_block = true
step 2: interests() reports POLLIN for the buffered child output
step 3: poll() returns immediately with POLLIN
loop: needs_pump() is false -> pump_io is never called again
```

Observed with a child shell under tuke: the loop logged `poll woke ...
session=0x1` and `pump calls=0` on every iteration, `read=0`, `read_wb=1`. The
shell prompt sat unread in the PTY until an unrelated keystroke happened to
re-enter the loop through a path that pumped unconditionally.

## Observed behavior

`Session::needs_pump()` (`src/session.rs`) returns false while the fd is still
readable:

```rust
if self.phase == Phase::Live && !self.read_paused() && !self.read_would_block {
    return true;
}
```

`read_would_block` is set by the `WouldBlock` arm of the read phase and cleared
only at the top of `pump_io`. A caller that reaches `pump_io` solely through
`while needs_pump()` therefore never clears it, and poll returns immediately on
every pass because `interests()` keeps asking for `POLLIN`.

## Expected behavior

`src/session.rs`'s own prose states the invariant the example breaks:

> Step 1 must come first: `interests()` reports what blocking would usefully
> wait for, not what `pump_io` can do right now. Waiting while `needs_pump()` is
> still true can hang, because the session has work that produces no further
> readiness edge.

"Must come first" is not a property of `while needs_pump()`: the loop body
never runs when `needs_pump()` is false. The example, the prose above it, the
`README.md` Example (which pumps unconditionally, then drains), and
`tests/session.rs::pump_all` ("Pumps every session once and drains each
`needs_pump` loop") cannot all be right. `pump_all` and the README are the
correct shape; the crate example is not.

## Impact

A correctness trap for any edge-triggered single-session caller, which is the
first thing the crate-level docs are read for. The failure is a stuck loop, not
an error: the child's output is never read and the process spins at full CPU,
so it is easy to misread as a child-side problem. tuke hit it and only found it
by instrumenting the pump counters.

## Notes

The fix is to the example, not to the API: pump once unconditionally, then
`while needs_pump()` to drain, matching `README.md` and `tests/session.rs`. The
`while` should stay — the RFC in `done/20260917-rfc-pump-needed-sessions-list.md`
settled that an edge-triggered loop must drain before blocking — but the extra
unconditional call before it has to appear in the crate example, since that is
the one place a reader is most likely to copy and it is currently the only one
of the four that omits it.

## Outcome

Fixed in [#9](https://github.com/sile/termnix/pull/9) (merged as `b3e623f`).

The crate example now pumps unconditionally before its `while session.needs_pump()` drain, so the first pump of a round is no longer gated on a predicate that only `pump_io` itself can clear. The comment above it says why, and the example now matches the shape in `README.md` and `tests/session.rs::pump_all`.

The fix stayed in the example; the `needs_pump()` contract is unchanged.

The scope is unchanged from what is described above.
