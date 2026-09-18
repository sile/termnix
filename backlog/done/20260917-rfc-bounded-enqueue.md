# RFC: Add a bounded `enqueue_input`

- Status: rejected

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

## Outcome

Rejected. No pull request: the reasoning was settled against the proposal
before any code was written.

The Motivation describes the caller as having "no *action* to take" and as
having only "a counter the caller must remember to check and compare against a
limit it chose itself". But the loop the RFC itself prints as the thing to
replace *is* the action, and it is expressible without the method:

```rust
if session.write_queue_len() + session.input_byte_len(input) > MY_LIMIT {
    // hold or drop the input
} else {
    session.enqueue_input(input);
}
```

That is the same decision `enqueue_input_bounded` would make internally, with
the policy left where the RFC also wants it: at the call site. This is the
"keep `enqueue_input` and document the counter harder" alternative, which the
RFC rejects as "missing a way to act on it without the caller implementing the
queue-depth arithmetic itself" -- and that missing piece is the whole
proposal. The arithmetic is one addition and one comparison, and the three
real call sites in this repository (`examples/headless.rs`, two in
`examples/tuinix.rs`, plus the `README.md` snippet) were all rewritten to
exactly that form while settling
`20260919-rfc-session-counters-cumulative-only`, so the shape is known to read
well in practice.

The alternatives now have a concrete decision behind them rather than a
preference. When the counter-cleaning proposal came to the same question -- a
`Session`-side helper that folds the comparison -- it was rejected for the
same reason this RFC should be: `would_exceed(limit)` and
`unwritten_after(input)` each collapse the addition and the comparison
convention into one name and hide what is being measured. Keeping the two
terms explicit was chosen deliberately.

What would revive this: evidence that the arithmetic is genuinely hard to get
right at the call site (an off-by-one in the comparison, mistaking the
pre-enqueue for the post-enqueue queue, or forgetting that `byte_len` depends
on the session's current modes and can change between calls). The RFC notes
that its limit is measured on the queue *after* a successful enqueue so that a
single oversized input is refused rather than allowed to exceed the limit --
that is a real distinction, and if callers demonstrably get it wrong, a helper
has a reason to exist. No such evidence was presented, and the repository's
own four sites did not need it. A default limit was not proposed either, and
unbounded `enqueue_input` remains correct.
