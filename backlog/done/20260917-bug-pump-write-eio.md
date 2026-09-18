# Bug: `pump_io` treats write-side `EIO` as an error, not as EOF

- Status: fixed

## Summary

The read phase of [`Session::pump_io`](crate::Session::pump_io) normalizes
`EIO` to "the peer is gone" and treats it as an end-of-stream signal. The write
phase does not: a write that fails with `EIO` (the child has died and the pty
no longer accepts input) is returned as an error, which is the documented
normal case for a session, not an error.

## Reproduction

Write to a session whose child has exited but whose session has not yet been
reaped — i.e. the child died, its output is still being drained, and the
caller enqueues one more input. On the next pump:

```text
session.enqueue_input(Input::Key(....));
session.pump_io(PumpBudget::default());

// observed: Err(...) propagated from the write phase (ErrorKind::ReadOnlyFilesystem / EIO)
// expected: Ok, with the write side observed as closed, the same way a read EIO is
```

By construction this window is reachable: a caller only learns the child is
gone on the *next* `try_wait`, after it has already queued a keystroke. The
read phase normalizing its `EIO` means the crate already knows the pty can
return it after the child dies; the write phase does not apply the same rule.

## Observed behavior

The crate's own documentation states that `ErrorKind::ReadOnlyFilesystem`
(which is how `EIO` surfaces) is a normal learning path on this platform, not a
failure. The read phase implements that; the write phase surfaces the error. The
result is that an ordinary race — keystroke arrives at the same moment the child
ends — takes a different path depending on which direction the pty is looked at.

## Expected behavior

A write-side `EIO` should be treated the same way its read-side equivalent is:
as the write half of the session being closed, not as a `pump_io` failure. The
two phases need not be identical in every detail, but they must agree on
whether "the pty went away" is an error, and they currently do not.

## Impact

Correctness for any caller that keeps a session alive to drain it after the
child exits — which is the pattern termnix's own API encourages (`try_wait`
reports the child, `status` stays `Live` until drained). The visible failure is
a pump that returns `Err` exactly when the child has just died, when the caller
is most likely to be handling that death and least likely to expect an error
from a call it made to flush a final keystroke.

Not ergonomics: the caller cannot get the right result from the public API in
that window. If investigation shows the write phase's `EIO` genuinely means
something other than "the child is gone" in some case, this becomes an RFC on
what the phases should each promise; as documented it is a bug.

## Notes

Fix should likely share the normalization with the read phase rather than
adding a second classification, so the two cannot drift again. A test should
force the window (exit the child, then pump a write) rather than relying on the
race happening.

## Outcome

Fixed in [#1](https://github.com/sile/termnix/pull/1) (merged as `861cf07`).

Both directions now classify through one predicate, `is_pty_gone`, keyed on the
raw `EIO` code. `read_phase` calls it in place of its inline check, and
`write_phase` treats its `EIO` as PTY EOF: it retires the outbound queue
(without touching the byte counters, the same way `close()` discards it) and
calls `note_eof()`, so the two directions can no longer disagree about whether
"the pty went away" is an error. This also removes a `needs_pump()` spin, since
clearing the queue makes `outbound_empty()` true.

A platform caveat found while testing: on Linux/Android a write to a master
whose slave has closed usually succeeds (the tty layer discards or buffers it)
rather than returning `EIO`, and `EIO` is reliably seen on the read side. So the
write arm cannot be forced by a plain child-exit fixture here, and the report's
"writing after child death crashes the loop" was not reproduced on this
platform. The fix is kept as the correct contract, since a write-side `EIO` is
never recoverable and classifying it opens no new path. The classification is
covered by a unit test (`session::tests::pty_gone_only_matches_eio`), and the
observable contract (a queued write after child death must not error or spin
`needs_pump`) by `tests/session.rs::write_to_a_gone_child_is_not_an_error`.

The scope is unchanged from what is described above.
