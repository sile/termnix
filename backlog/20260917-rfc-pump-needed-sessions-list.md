---
Created: 2026-09-17
Status: draft
---

# RFC: Give the caller a way to enumerate sessions that need pumping

## Summary

Add a call that yields the sessions whose `needs_pump()` is currently true, so
a multi-session caller can service them without keeping its own map iteration
order (and without accidentally starving the sessions that sort last).

## Motivation

termnix models each session independently: `Session::needs_pump()` answers for
one session, `Session::interests()` answers for one session, and the crate's
documentation explains that the caller owns the loop that visits them. A
terminal multiplexer holds several sessions at once and writes the loop by hand:

```rust
// what a real consumer writes today, over its own HashMap<SessionId, Session>
for session in self.sessions.values_mut() {
    if session.needs_pump() {
        while session.needs_pump() {
            session.pump_io(budget);
        }
    }
}
```

The inner `while` drains one session completely before moving on. If one
session is producing output faster than its budget consumes it, the sessions
after it in iteration order wait; over a long-lived process, the same session
tends to stay at the front of a `HashMap`'s arbitrary but stable-ish order and
the others get less service. Nothing in termnix *causes* this — it is the
caller's loop — but termnix offers no shape that discourages it either, and its
counter for the warning case (`pump_budget_exhaustions`) is prose the caller has
to have read.

## Guide-level explanation

Before, the caller decides how to be fair, from scratch, per project:

```rust
for session in self.sessions.values_mut() {
    if session.needs_pump() {
        session.pump_io(budget); // one quantum, or drain all? caller's choice
    }
}
```

After, termnix can hand back the set to visit, and the caller keeps the fair
loop it wants:

```rust
for id in self.sessions.needing_pump() { // however the caller stores them
    self.sessions.get_mut(&id).pump_io(budget); // exactly one quantum
}
```

## Reference-level explanation

Two axes are worth separating, and conflating them is why this has not been an
obvious add:

- **Who holds the sessions.** termnix does not own a collection of sessions —
  a `Session` is created and owned by the caller. So termnix cannot itself
  return "the sessions that need pumping" as a list of `&mut Session` without
  owning them. What it *can* offer is a helper over a caller-provided
  collection:

  ```rust
  /// Invoke `pump` once for each session in `sessions` whose `needs_pump()`
  /// is true, in a caller-visible order.
  pub fn pump_all_needing(sessions: &mut [Session], budget: PumpBudget) { /* ... */ }
  ```

- **Whether "once" or "until exhausted".** today's real loops drain a session
  until `needs_pump()` is false, which is the fairness problem. A helper that
  pumps exactly once per call and is meant to be called again next round
  encodes the fair policy; the caller can still drain by calling it in a loop.

Either shape is possible. The author's preference is the `pump_all_needing`
helper over a slice — it stays stateless, does not make termnix own the
collection, and the "exactly once" semantics are visible in the signature.

## Alternatives

### Do nothing; document fairness more strongly

Rejected as the resting place. The docs already describe the budget-exhaustion
signal, but the fair loop still has to be written and reasoned about per
project.

### Have `Session` own a shared registry

Rejected. It would make termnix own the set of sessions, which contradicts the
current design where the caller creates and owns each `Session`.

### Return an iterator of the sessions that need pumping

Not possible without termnix owning the collection (see above), unless the
caller passes its own slice or map and termnix only filters. A helper that
filters a caller-provided slice is the realistic form.

## Drawbacks

- A helper that pumps once per session is a policy ("be fair, do not drain") in
a crate that otherwise leaves policy to the caller. The RFC leans on "exactly
once, repeated by the caller" as a *mechanism* (a defined unit of work), but a
reviewer could reasonably call it policy and reject it.
- Slice-based filtering is convenient for a caller that stores sessions in a
`Vec`, less so for one that stores them in a map keyed by id. The `Vec` shape
may not fit every caller.

## Open questions

- Helper over a slice, or leave the loop entirely to the caller and strengthen
  the docs?
- If a helper, does it pump once and return, or expose the choice (a
  `max_rounds`/`drain` flag)?
- What order should the helper visit sessions in, given the point is to avoid
  depending on the caller's storage order?
