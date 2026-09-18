# RFC: Make "the child exited" a single predicate

- Status: rejected

## Summary

Provide one way to ask whether a session's child process has exited, distinct
from whether the session is fully reaped, so that a caller that closes a pane
when the program inside it ends does not have to combine `exit_status()` and
`status()` by hand.

## Motivation

A terminal multiplexer closes a pane when the process running in it exits, even
though the session may still hold output the child wrote before exiting. The
pane must stay alive long enough to drain that output, and then close. The
question the caller asks is therefore *"has the child exited?"*, not *"is the
session over?"*.

termnix splits the state correctly — an exited child is observed separately
from the session being fully `Reaped` — but it does not name the combination.
A real caller writes:

```rust
// what a consumer writes today
let status = session.try_wait(); // Option<ExitStatus>, memoized
if status.is_some() && session.status() != SessionStatus::Reaped {
    // the child is gone; keep draining, then close the pane
}
```

That works, but it encodes a rule that lives nowhere in the crate: "a child
that exited while output remains is a state you must handle, and you detect it
by checking both". A caller who checks only `status()` misses it (the session is
still `Live`); a caller who checks only `try_wait()` closes the pane before
draining. Both mistakes are easy, and the correct incantation is learned by
reading two methods' docs and inferring the combination.

## Guide-level explanation

Before:

```rust
if session.try_wait().is_some() && session.status() != SessionStatus::Reaped {
    close_when_drained(session);
}
```

After:

```rust
if session.child_exited() {
    close_when_drained(session);
}
```

## Reference-level explanation

```rust
impl Session {
    /// Whether the child process has exited (with `try_wait` having observed
    /// it), regardless of whether the session has finished draining its
    /// output and reached [`SessionStatus::Reaped`].
    pub fn child_exited(&self) -> bool { /* try_wait().is_some() */ }
}
```

Alternatives to settling on `child_exited`:

- **A variant on `SessionStatus`.** Today the status has (at least) `Live` and
  `Reaped`. An `Exited` variant between them would name the state directly:
  child is gone, I/O continues. This is arguably the more honest model — the
  state *is* on the status axis — but it is a larger change because the variant
  has to be handled by every match over the status (including any loop that
  waits for `Reaped`). Recorded as the structural alternative.
- **A predicate method.** `child_exited()` is the smaller change and reads
  well at a call site; it is the author's preference because the combination it
  encapsulates is the whole point and does not need a new variant to be
  expressible.

The predicate and the status must stay consistent: if `child_exited()` is true,
`status()` is either `Live` (draining) or `Reaped` (done), never something that
contradicts it. A test should pin that relationship.

## Alternatives

### Do nothing; document the combination

Rejected as the resting place. The combination is already correct and already
required; what is missing is a name, and each caller invents its own (or gets
it wrong).

### Expose the raw fields and trust the caller

`exit_status()` and `status()` already exist. The proposal is not to hide them,
but to add the predicate so the common question has a common answer.

## Drawbacks

- Two ways to ask about the child: the predicate and the two underlying
  accessors. A caller that needs the exit status itself (to report a code) still
  uses `try_wait()`; the predicate is only for the yes/no question.
- If `SessionStatus` grows an `Exited` variant later, `child_exited()` becomes a
  thin alias over it and there are then two idioms for one state, unless one is
  retired.

## Open questions

- Predicate method, or the `SessionStatus::Exited` variant (see above)?
- Should there also be a way to be *notified* of the transition (a flag a loop
  checks), or is a predicate enough given the caller already polls?

## Outcome

Not implemented; settled by documenting the distinction instead, in
[#2](https://github.com/sile/termnix/pull/2) (merged as `d3f91f2`).

Investigating the proposal showed the predicate would add no information, which
is why it was dropped rather than renamed or narrowed. `SessionStatus::Reaped`
is reached only once the child's exit has been observed *and* the I/O is
finished, so `exit_status().is_some()` is already true whenever it matters, and
`status() != SessionStatus::Reaped` — the second half of the combination this
RFC wanted to name — never changes the answer. The combination is therefore not
a rule the crate was missing; it is a redundant one that callers assembled
because the two questions were not written down side by side.

The two methods stay public and keep their meanings: `exit_status()` answers
"has the child exited?" without a syscall, and `try_wait()` observes and reaps
when the caller wants that. `status() == Reaped` keeps its place as the answer
to "is the session over?", which is a different question and the one a pane
that must drain before closing is actually asking. What changed in #2 is the
documentation: `Session::exit_status()` now names the distinction and shows it,
`SessionStatus::Reaped` says which of the two questions it answers, and
`Session::try_wait()` points at the syscall-free accessor for the yes/no
question. The two existing examples are cited as the two halves of the
distinction — `examples/headless.rs` tracks the child's exit separately from
draining, `examples/tuinix.rs` drops a pane only once the session is `Reaped` —
so a reader has runnable code for each question rather than a new guide.

`SessionStatus::Exited`, the structural alternative recorded above, is not
needed for the same reason: the state it would name is already observable, and
adding a variant would change every match over the status without making any of
them easier.

The scope is unchanged from what is described above.
