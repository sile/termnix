---
Created: 2026-09-17
Status: draft
---

# RFC: Add a bounded `enqueue_input`

## Summary

Add a way to enqueue input to a session with a ceiling on how much may sit
unwritten, so a caller that cannot afford to buffer without limit can ask the
session to stop accepting more input instead of discovering the growth later.
For example `Session::enqueue_input_bounded(input, limit) -> Result<(), WouldBlock>`.

## Motivation

`Session::enqueue_input` appends to the session's write queue and returns. It
does not cap the queue. Its rustdoc points the caller at
[`SessionCounters::unwritten()`](crate::SessionCounters::unwritten) to observe
backpressure, which is honest about where the policy lives — but the caller has
no *action* to take. There is no call that says "stop, I am full".

The natural caller loop is:

```rust
// what a real consumer writes today
session.enqueue_input(key);
while session.needs_pump() {
    session.pump_io(budget);
}
// ... later, poll, repeat
```

If the child never reads, nothing here pushes back. The queue grows on every
keystroke while `pump_io` moves zero bytes, and the only signal is a counter the
caller must remember to check and compare against a limit it chose itself. A
consumer driven by a per-poll budget (which `PumpBudget` already encourages)
has a per-call bound on the pump but no per-session bound on the backlog.

## Guide-level explanation

Before, the caller checks the counter and hopes the check is in the right
place:

```rust
if session.counters().unwritten() > MY_LIMIT {
    drop_input(); // or grow the limit; the session does not say which
} else {
    session.enqueue_input(key);
}
```

After, the session answers whether it can take the input:

```rust
if session.enqueue_input_bounded(key, MY_LIMIT).is_err() {
    // backpressure: the session refused because MY_LIMIT unwritten bytes are
    // already queued
}
```

## Reference-level explanation

Two shapes:

```rust
/// Enqueue input, refusing it if more than `limit` bytes would be unwritten.
///
/// The limit is measured on the queue *after* a successful enqueue, so a
/// single input larger than `limit` is refused rather than allowed to exceed
/// it. Returns `Ok(())` when the input was queued, `Err(WouldBlock)` when it
/// was not; a refused input is not partially queued.
pub fn enqueue_input_bounded(
    &mut self,
    input: Input,
    limit: usize,
) -> Result<(), WouldBlock>;
```

or a state-based form where the limit is stored once and settles the question:

```rust
session.set_input_limit(Option<NonZeroUsize>); // None = unbounded (today)
```

The author leans toward the one-shot `_bounded` method: it keeps the limit at
the call site where the policy lives, and it leaves `enqueue_input` exactly as
it is for callers that have no limit.

Either shape needs `Input::byte_len(modes)`, which already exists, to measure
what is being added. `PumpBudget`'s default ceilings are a reasonable model for
what a default limit might look like if a `Default` limit is ever desired, but
the RFC does not propose a default — unbounded stays the default, and the
bounded call is opt-in.

## Alternatives

### Keep `enqueue_input` and document the counter harder

Rejected as the resting place. The counter is already documented; what is
missing is a way to act on it without the caller implementing the queue-depth
arithmetic itself.

### Have `pump_io` apply backpressure all on its own

Rejected. The pump moves what the child will take; it has no notion of "too
much waiting" and should not invent one. Backpressure is the caller's policy,
and this RFC keeps it there while giving the caller a call to express it.

### Block inside `enqueue_input` until there is room

Rejected outright. termnix is Sans I/O and driven by the caller's poll loop;
blocking inside an enqueue would deadlock against that loop.

## Drawbacks

- One more method, and one more thing to pick between when deciding how to
  queue input.
- A limit expressed in bytes is precise but not intuitive (how many bytes is
  one keystroke's `Input`?). If a byte limit is judged the wrong unit, this
  needs an RFC of its own for a per-modality cost — which is why the RFC keeps
  the unit explicit and opt-in.

## Open questions

- One-shot method versus stored limit (see above).
- Bytes, or a coarser unit like "number of sessions' worth of input"? The RFC
  uses bytes because `byte_len` is what the crate already reports.
