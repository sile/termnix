# RFC: Let the session supply the modes for `Input::byte_len`

- Status: draft

## Summary

Add `Session::input_byte_len(&self, input) -> usize`, which reports how many
bytes an input would add to the write queue using the session's current
terminal modes. It is `input.byte_len(session.terminal_state().modes())`
with the modes supplied by the session, so a caller applying its own input
limit does not have to reach into the terminal state for them.

## Motivation

`Session::enqueue_input()` applies no backpressure policy, and its rustdoc says
so: a caller that wants a limit must compare `Input::byte_len()` against
[`SessionCounters::unwritten()`] and decide for itself whether to enqueue, hold,
or drop the input. The comparison is the caller's job and should stay that way.

The problem is what the caller has to write to perform it:

```rust
// what a consumer writes today
let modes = session.terminal_state().modes();
let after = session.counters().unwritten() + key.byte_len(modes);
if after > MY_LIMIT {
    // hold the input; enqueueing it would exceed the limit
} else {
    session.enqueue_input(key);
}
```

There is no missing information here — every value is already public — but the
expression drags in two things that have nothing to do with the limit: a
`TerminalState` and a `TerminalModes`. The modes are part of the session's own
state, written by the emulator as the child sets them, so fetching them is the
session's business rather than the caller's. The caller who forgets this step
does not get a wrong answer, because there is no way to call `byte_len`
without them; the cost is that the correct expression has five moving parts
(`session`, `counters()`, `unwritten()`, `terminal_state()`, `modes()`) and
spans two methods' documentation.

By contrast, a caller with no limit only needs `session.enqueue_input(key)`.
The convenience method narrows the gap between the two cases without moving
the policy: the limit stays at the call site, and nothing about when input is
accepted changes.

## Guide-level explanation

Before, the caller fetches the modes to size the input:

```rust
let modes = session.terminal_state().modes();
let after = session.counters().unwritten() + key.byte_len(modes);
if after > MY_LIMIT {
    hold(key);
} else {
    session.enqueue_input(key);
}
```

After, the session answers for the input it was handed:

```rust
let after = session.counters().unwritten() + session.input_byte_len(key);
if after > MY_LIMIT {
    hold(key);
} else {
    session.enqueue_input(key);
}
```

The limit, the comparison, and the decision to hold are all still the caller's.
What is gone is the trip through `TerminalState` for a value the session
already owns. `input_byte_len` reads as "how big is this input, here", in the
same vocabulary as [`Input::byte_len()`], and a caller who only needs the size
(for a budget, a log line, or a capacity reservation) can use it on its own.

## Reference-level explanation

```rust
impl Session {
    /// Returns how many bytes `input` would add to the write queue with the
    /// session's current terminal modes.
    ///
    /// Equal to `input.byte_len(self.terminal_state().modes())`. Provided
    /// because the modes are part of the session's state: a caller applying an
    /// input limit through [`SessionCounters::unwritten()`] needs this size and
    /// would otherwise have to fetch them from [`Session::terminal_state()`].
    ///
    /// Takes `&self` and performs no syscall. The session's modes can change
    /// between calls as the child writes escape sequences, so a size is only
    /// valid for the modes at the moment it was taken.
    pub fn input_byte_len(&self, input: Input<'_>) -> usize {
        input.byte_len(self.term.modes())
    }
}
```

Points that need care:

- **No enqueue, no state change.** The method is a pure query: it neither
  enqueues nor reserves anything, so calling it and then deciding not to enqueue
  leaves the session exactly as it was. It must not be implemented by writing
  the input somewhere and measuring the result, which would allocate and could
  clobber the queue if it used the real one.
- **`Raw` ignores the modes.** `Input::byte_len` already documents this; the
  method inherits it and should say so by pointing at `byte_len` rather than
  restating the rule, so the two cannot drift.
- **`&self`, not `&mut self`.** Unlike `enqueue_input`, nothing here observes
  or advances the child, so it works in every phase, including after the child
  has been reaped.
- **Modes are read at call time.** The size is consistent with what
  `enqueue_input` would write only if no pump runs in between. A caller that
  sizes an input, pumps, then enqueues can see the input encode differently if
  the child changed the modes; sizing and enqueueing back to back is the
  intended use.
- **`Paste` is why this matters.** For a single keystroke the difference is a
  few bytes and the caller could ignore it, but a paste can be thousands of
  bytes, which is exactly when a limit is worth having. This is the case that
  makes "compare against a limit" more than a formality.

## Drawbacks

- One more method on `Session`, and a second way to compute a size that
  `Input::byte_len` already computes. The two cannot disagree for the same
  modes, but a reader now has two entry points to choose between.
- It is a convenience, not new capability: every value it returns is already
  computable from public API. Its only claim is that the modes belong to the
  session.
- It invites the reader to treat the size as the whole answer to
  backpressure. It is not; the caller still owns the limit, and the queue also
  receives emulator replies that no input limit can predict.

## Rationale and alternatives

- **Return the queue length instead of the input size**
  (`input_queue_len(input)` or similar, reporting
  `unwritten() + input.byte_len(modes)` in one call). Rejected for this RFC
  because it answers a different question — the projected queue depth — and
  drags in the naming of the unwritten-byte counters, which is unresolved (see
  below). Returning the input's own size keeps this change to one concept and
  leaves the queue-length question for its own proposal.
- **Take the limit and return a verdict** (`enqueue_input_bounded(input, limit)
  -> Result<(), WouldBlock>`). Rejected: the limit is policy and belongs at the
  call site, the comparison's edge behavior (`>` versus `>=`) is the caller's to
  choose, and it would introduce a new public error type for no new
  information.
- **Make `Input::byte_len` take the session.** Rejected: `Input` deliberately
  knows nothing about I/O or sessions, and its modes argument is what keeps
  encoding separable from the session. Supplying the modes from the session in
  a method on `Session` is the smaller change.
- **Do nothing.** The status quo is correct, and a caller who reads both docs
  can write the expression. What is missing is not correctness but a name for
  "how big is this input, here", and callers currently spell that name as a
  two-hop accessor chain. Doing nothing keeps the friction that produced this
  proposal.

## Unresolved questions

- Should `Session` also expose the projected queue length, so that the common
  limit check is one call instead of an addition? Deferred deliberately: the
  counter's vocabulary (`unwritten`, `unwritten_input`, `unwritten_reply`) is
  itself under question, and settling it first avoids adding a second name for
  the same quantity.
- Should the `unwritten` family be renamed for clarity (for example around
  "pending write")? Out of scope here; it is a breaking change and deserves its
  own item.
- Is `input_byte_len` the right name, or should it echo `Input::byte_len` more
  directly? The current choice matches the type and method it delegates to.

## Future possibilities

- A projected queue-length method, once the counter vocabulary is settled,
  would make the limit check a single call and could delegate to this method
  for its input term.
- Renaming the unwritten-byte counters would let both this method and that one
  share one vocabulary, so a reader meets one concept per quantity instead of
  two names for the same bytes.
- If a per-modality cost is ever wanted (the byte unit is precise but not
  intuitive), this method is the natural place to add it, since it already
  owns the modes.
