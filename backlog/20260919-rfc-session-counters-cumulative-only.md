# RFC: Keep only cumulative totals in `SessionCounters`

- Status: draft

## Summary

Make `SessionCounters` hold cumulative totals and nothing else, and provide the
quantities it currently mixes in from the places that own them.

- Remove the four `max_*` peak fields, private `update_maxes()`, and its call
  sites.
- Remove the derived residual methods `undecoded_read()`, `unwritten()`,
  `unwritten_input()`, and `unwritten_reply()`.
- Add `Session::write_queue_len()` for the current write-queue occupancy, and
  carry over `Session::input_byte_len()` from the separate input-size proposal.

The type's own documentation already says it records only running totals and
that quantities owned elsewhere are reached elsewhere. This proposal is the
work that makes that sentence true.

## Motivation

`SessionCounters` currently holds three different kinds of value under a name
and a doc comment that promise one:

```rust
/// Cumulative counters tracked by a session, returned by [`Session::counters()`].
///
/// The counters record only production and consumption totals. How much is
/// buffered right now is derived from them through [`undecoded_read()`],
/// [`unwritten_input()`], [`unwritten_reply()`], and [`unwritten()`].
pub struct SessionCounters {
    // 12 cumulative u64 totals ...
    pub max_buffered_read_bytes: usize,   // a peak, not a total
    pub max_pending_write_bytes: usize,   // a peak, not a total
    pub max_scrollback_lines: usize,      // a peak of emulator-owned state
    pub max_scrollback_cells: usize,      // a peak of emulator-owned state
}
```

- **The peaks are not totals.** Four `usize` running maxima sit among `u64`
  cumulative fields, and two of them measure scrollback, which the type's doc
  says is reached through [`Session::terminal_state()`]. Nothing outside the
  crate observes any of them: no example reads them, and the single test
  reference asserts that `max_scrollback_lines` equals the scrollback length
  that `terminal_state()` already reports.
- **The residuals are not fields, but they read like them.** `undecoded_read()`
  and the `unwritten` family are derived methods on the counter type, so a
  reader scanning `impl SessionCounters` meets field-like names that are
  recomputed from totals. Only one of them has a caller outside the crate: the
  `unwritten` family is what every consumer's backpressure check uses.

This is a leftover from an earlier split. Commit `5c351a2` ("Split session
metrics into counters and pending bytes") broke a combined `SessionMetrics`
type into a cumulative side and a current-value side. The split settled where
the two totals belonged but left the peaks with no home and put the residuals
onto the cumulative type, where they have been read as if they were part of it.

**The name is the symptom, not the cause.** `unwritten()` reads as "the
complement of `written()`", and the two do form a pair — but no caller reads
them as a pair. Every in-tree use is a backpressure check against an input size:

```rust
// examples/headless.rs:170, examples/tuinix.rs:308 and 350
session.counters().unwritten().saturating_add(need) > WRITE_SOFT_LIMIT
// README.md:94
session.counters().unwritten() < 4096
```

What the caller wants is "how many bytes are still queued for writing", which
is a current value owned by the session, not a total owned by the counters.

## Guide-level explanation

A caller applying its own input limit writes the check like this today:

```rust
let modes = session.terminal_state().modes();
let after = session.counters().unwritten() + key.byte_len(modes);
if after > MY_LIMIT {
    hold(key);
} else {
    session.enqueue_input(key);
}
```

There is no missing information here — every value is public — but the
expression mixes three accessors for two quantities, and the second line reads
as if the counter were the session's queue. After this change the two
quantities each have one name, both on the session:

```rust
if session.write_queue_len() + session.input_byte_len(key) > MY_LIMIT {
    hold(key);
} else {
    session.enqueue_input(key);
}
```

The limit, the addition, and the direction of the comparison all stay at the
call site. What is gone is the trip through `counters()` for a value the
session owns and through `terminal_state()` for modes the session owns.

For measurements, the counters still carry the totals:

```rust
let counters = session.counters();
eprintln!("read {} bytes, wrote {}", counters.pty_bytes_read, counters.written());
```

A caller that wants a peak now tracks it from the current values, which are the
only thing the session reports:

```rust
peaks.queue = peaks.queue.max(session.write_queue_len());
peaks.scrollback_lines = peaks
    .scrollback_lines
    .max(session.terminal_state().scrollback_lines().len());
```

This is more code at the call site, but it is the same arithmetic on names that
are public and documented today, and it stops the counters from carrying a
second, private notion of every quantity they expose.

## Reference-level explanation

### `SessionCounters`

Delete the four peak fields:

```rust
/// Highest observed undecoded read bytes.
pub max_buffered_read_bytes: usize,
/// Highest observed unwritten write bytes.
pub max_pending_write_bytes: usize,
/// Highest observed scrollback lines.
pub max_scrollback_lines: usize,
/// Highest observed scrollback cells.
pub max_scrollback_cells: usize,
```

Delete the four derived methods and keep the untouched helper `byte_delta`:

```rust
pub fn undecoded_read(&self) -> usize
pub fn unwritten(&self) -> usize
pub fn unwritten_input(&self) -> usize
pub fn unwritten_reply(&self) -> usize
```

`written()` and `byte_delta()` stay. `written()` is a total, and it is the only
member of the `unwritten` pair that belongs on this type. After the change every
public member is a cumulative `u64` total or a sum of them.

### `Session`

Add:

```rust
impl Session {
    /// Returns how many bytes are queued for writing to the PTY and have not
    /// been written yet, counting application input and terminal replies
    /// together.
    ///
    /// This is the occupancy of the session's single write queue. Compare it
    /// against a limit of your own to apply backpressure; the session applies
    /// none. Takes `&self` and performs no syscall.
    pub fn write_queue_len(&self) -> usize {
        self.unsent()
    }

    /// Returns how many bytes `input` would add to the write queue with the
    /// session's current terminal modes.
    ///
    /// Equal to `input.byte_len(self.terminal_state().modes())`. Provided
    /// because the modes are part of the session's state: a caller applying an
    /// input limit through [`Session::write_queue_len()`] needs this size and
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

The existing private `unsent()` becomes the body of `write_queue_len()`; it
currently has one other caller, the `write_phase` budget calculation, which
keeps using it.

### Internal helper for the breakdown

The removed public `unwritten_input()` and `unwritten_reply()` are unused outside
the crate, but `write_queue_len()` does not need them: it measures the queue
directly rather than subtracting totals, which removes the second computation
path that currently exists between `update_maxes()` and the public accessors.
If the breakdown is wanted later, it belongs on `Session` next to
`write_queue_len()`, not on the counters.

### Call sites and tests

- `src/session.rs`: delete `update_maxes()` and its six call sites — one in
  `enqueue_input()` (571) and five around `pump_io` (575, 1059, 1131, 1148),
  plus the definition at 865. Both of its inputs survive: `buffered_read_len()`
  keeps callers in `needs_pump()` and the read-buffer limit arithmetic, and
  `unsent()` keeps the `write_phase` caller.
- `tests/session.rs:653`: `assert_eq!(before_len, before.max_scrollback_lines)`
  loses its right-hand side. Keep the test's intent by comparing two
  `terminal_state()` readings, or drop the assertion.
- `tests/session_workflow.rs:120`: drop
  `a.counters().undecoded_read() == 0 &&`. It sits in the same predicate as
  `!a.needs_pump()`, and `needs_pump()` already returns true whenever
  `buffered_read_len() > 0`, so the term is a redundant restatement of the
  condition beside it.
- `README.md`: the "Input and backpressure" section tells the reader to compare
  `unwritten` bytes against the input size, and its example uses
  `session.counters().unwritten() < 4096`. Both become
  `session.write_queue_len()` plus `session.input_byte_len(input)`. The
  Lifecycle section's mention of peak values also goes.
- `examples/headless.rs:170` and `examples/tuinix.rs:308, 350`: each becomes a
  `write_queue_len() + input_byte_len(input) > WRITE_SOFT_LIMIT` check. The
  `terminal_state().modes()` fetch disappears from all three.
- `src/session.rs` crate doc (line 51) points at `SessionCounters::unwritten()`;
  it should point at `Session::write_queue_len()`. The `SessionCounters` doc
  comment loses its residual sentence and its four link definitions, and the
  `Session::enqueue_input()` doc loses its `SessionCounters::unwritten()` link.

This is a breaking change: it removes public fields and public methods and
changes `SessionCounters`' `PartialEq`. The crate is at `0.0.1` and does not
value long-term stability, so no deprecation shim is proposed; the whole
surface moves at once.

## Drawbacks

- The residual quantities become one call further away for a caller holding a
  `SessionCounters` value: it must ask the session, not the counters. The
  counter reference alone is no longer enough to answer "how full is the
  queue".
- Consumers that used the peaks must write and keep their own tracking, and
  must remember to sample after every pump.
- Polling from outside cannot reproduce a peak that grows and is undone within
  a single `pump_io` call. If that precision is wanted, it needs a dedicated
  API, not these fields.
- `input_byte_len()` overlaps `Input::byte_len()`: the two cannot disagree for
  the same modes, but there are now two entry points.

## Rationale and alternatives

- **Do nothing.** The type keeps contradicting its own documentation and keeps
  computing the same quantities by a second path, and the most common consumer
  expression keeps spelling one concept with three accessors. Documentation is
  the right fix for an ambiguity, not for three kinds of value in one type.
- **Only delete the peaks, keeping the residual methods.** This was the original
  scope of this item, and it is the weakest half-measure: `SessionCounters`
  would become "cumulative totals plus four derived residuals", which is still
  not what its doc says, and the residual that every consumer touches would stay
  under a name that reads as its own complement.
- **Only rename `unwritten`.** It is a breaking change that leaves the residuals
  on the counters and leaves `undecoded_read()` in the same position. Renaming
  one member of a misplaced group does not fix the placement.
- **Rename to `pending_write_bytes()`** and keep it on the counters. It fixes
  the name but not the owner: the quantity is a current value, and the type is
  documented as cumulative. It also collides with the internal
  `pending_reply_range` and with `max_pending_write_bytes`, which this proposal
  removes instead.
- **Return the projected queue length instead of the input size**
  (`session.queue_len_after(input)` or similar, reporting
  `write_queue_len() + input_byte_len(input)` in one call). Rejected: it hides
  the addition the caller is making, folds two independent quantities into one
  name, and gives the crate the caller's policy question (what to do when the
  projected size is too large) without the policy.
- **Take a limit and return a verdict**
  (`enqueue_input_bounded(input, limit) -> Result<(), WouldBlock>`). Rejected:
  the limit is policy and belongs at the call site, the comparison's edge
  behavior (`>` versus `>=`) is the caller's to choose, and it would introduce a
  new public error type for no new information.
- **Move the peaks to a separate peak-tracking type.** A dedicated type would
  keep the feature while fixing ownership. Given that nothing observes any of
  the four peaks, this adds surface to preserve an unobserved convenience. It
  remains available later if a real consumer appears.
- **Move the scrollback peaks to `TerminalState` instead.** Rejected for the same
  reason the peaks are removed: no consumer needs them, and a consumer that does
  can sample the public `scrollback_lines()` and `scrollback_cells()` itself.
  Putting observation state on a type that is otherwise a pure view of the
  emulator is the larger change for the smaller need.

## Unresolved questions

None. Each point that was open while the item was drafted was settled before
this text was written:

- **`undecoded_read()`** is deleted rather than moved. Its only caller outside
the crate was a test predicate that restated `needs_pump()`, and `needs_pump()`
answers the question a consumer actually has — whether there is work left to do.
- **Scrollback peaks** are deleted, not relocated. A consumer that wants one can
sample `terminal_state().scrollback_lines().len()` and
`terminal_state().scrollback_cells()` itself, which is the same arithmetic the
crate was doing internally.
- **The name** is `write_queue_len()`, not `write_queue_bytes()`. The queue is a
byte buffer, and `_len()` is already the crate's spelling for its size
(`buffered_read_len()`, `unsent()`).
- **The `input` and `reply` breakdown** is not exposed. No consumer uses it
in-tree, the queue is shared by design, and the internal distinction stays
available if a need appears.

## Future possibilities

- With `SessionCounters` uniformly cumulative, the remaining question — whether
  derived accessors belong on it at all — is answered, and the type's doc
  becomes a one-line contract.
- A projected queue length can be built from `write_queue_len()` and
  `input_byte_len()` if a caller ever wants it, without changing either name.
- A scrollback peak, if a consumer ever wants one, can be sampled from the
  public `TerminalState` accessors without touching the counters, which is what
  a caller can do today.
