# RFC: Take consumed-once OSC events through one `take_event()`

- Status: draft

## Summary

Replace the single-purpose `take_clipboard()` on `TerminalState` with one
consumed-once event channel: an `Event` enum with a variant per family and a
single `take_event() -> Option<Event>` a caller drains from one place. Today
there is exactly one such family (an OSC 52 clipboard request) and its accessor
is named for it; the moment a second appears - and another proposal in this
series adds one - a caller would have to poll one method per family and would
have no single point at which "the child asked for something" can be handled.

This is a breaking change: `take_clipboard()` goes away and its callers move to
`take_event()`. The value it returned is unchanged; only the way it is reached
is.

## Motivation

The crate already distinguishes two shapes of terminal output a caller must
deal with, and only one of them is an event.

- A **reply** is a byte stream the caller may write back only partially. It is
  read with [`pending_reply_bytes()`](../src/terminal.rs) and released with
  [`advance_reply_bytes()`](../src/terminal.rs), so a caller that wrote half of
  it can resume. A reply is a byte stream on purpose: a query reply can be
  written to the PTY in pieces, so the accessor reports how much was consumed
  and leaves the rest in place. Nothing here changes that.
- An **event** is a discrete ask with no partial form: the child asked once, the
  caller either acts or does not, and the ask is over. Today the only event is
  the OSC 52 clipboard request, reached through `take_clipboard()`
  ([`src/terminal.rs`](../src/terminal.rs)), which returns
  `Option<ClipboardRequest>`.

With one event, a method named for the event is the whole API. The problem is
what the shape forces on the next family. A second event - a passthrough OSC a
caller must be offered, say, or a title change once a separate proposal moves
it - has no home except a second `take_*` method, and then a caller that wants
"everything the child asked for since I last looked" writes a growing list:

```rust
// The shape this RFC forecloses.
while let Some(req) = t.take_clipboard() { handle_clipboard(req); }
while let Some(other) = t.take_something_else() { handle_other(other); }
// ...and every new family appends another loop.
```

Two costs come with that shape, and neither is visible while the family count is
one:

- **No single drain point.** A caller servicing a poll loop wants one place to
  ask "is there anything to do?". With a method per family it must know the
  family list and call each one, so a family added later is handled only if the
  caller is updated. Nothing makes the omission visible.
- **No cross-family order.** A single `feed` can carry a clipboard request and a
  second family's ask. With one accessor per family the caller chooses an order
  to poll them in, and that order is not the order the bytes arrived. A single
  channel can preserve arrival order because the crate sees it; the caller
  cannot recover it after the fact.

This is the consolidation that a single-family accessor defers by design: when
the first event was added, a method named for it was the whole API because
naming a container for one variant buys nothing. The second family is what
changes that, and it is the condition under which the container starts to pay
for itself.

Nothing about *which* sequences are events changes here. A sequence becomes an
event because termnix decodes its arguments but does not use the result
internally - the test for whether a sequence is state, an event, or neither is
its own design question, answered elsewhere. This RFC is only the container the
event-shaped sequences are drained from. Passthrough, the title, and
grid-affecting sequences are separate proposals and not required for this one.

## Guide-level explanation

Before, a caller looked for the one thing a child could ask for:

```rust
if let Some(request) = t.take_clipboard() {
    host_clipboard.set(request.selection, &request.text, request.append);
}
```

After, a caller drains a channel and matches on what came out:

```rust
while let Some(event) = t.take_event() {
    match event {
        Event::Clipboard(request) => {
            host_clipboard.set(request.selection, &request.text, request.append);
        }
    }
}
```

The loop is the point: it keeps working when a variant is added, and the arms
the caller does not care about are answered once, in one place, instead of by a
call the caller has to remember to make. The `while` (not a single `if`) is
required, not stylistic: one `feed` may produce several events, and a caller
that takes one and stops would leave the rest pending - the same hazard the
take exists to prevent, one event later.

A caller that only cares about the clipboard matches one arm and ignores the
rest, which is what the empty-body arms in the example would be. There is no
second method to poll; `take_event()` returning `None` means the channel is
drained.

## Reference-level explanation

### The type

```rust
/// Something a child asked the caller to do, delivered once.
///
/// An event is not terminal state: it draws nothing, so taking one does not
/// move [`revision()`](TerminalState::revision). It is consumed by
/// [`take_event()`](TerminalState::take_event), so it is acted on once and is
/// not re-delivered on the next feed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Event {
    /// The child asked to change a selection (OSC 52).
    Clipboard(ClipboardRequest),
}
```

The enum is **not** `#[non_exhaustive]`. A later family therefore adds a
breaking variant, so every caller's `match` is revisited when it lands. That
is the intent: a caller that matches every variant gets a compile error and
has to decide what the new family means for it, rather than letting a
`_ => {}` arm swallow the new case silently. The cost is real - adding a
family is a breaking release - but the whole point of the single channel is
that no family's events can be lost without a caller choosing to lose them.

`ClipboardRequest` and `ClipboardSelection` are unchanged and stay exported. The
variant carries the same type `take_clipboard()` returned, so the value a caller
handles is byte-for-byte what it handled before; only the accessor moved.

### The accessor

```rust
impl TerminalState {
    /// Takes the next pending event, if any.
    ///
    /// Events are returned in the order the child produced them, oldest first.
    /// A caller that wants all of them drains in a `while let` loop; a caller
    /// that wants only the newest should keep draining until `None` and use the
    /// last, because the channel is a queue rather than a single slot (see
    /// below). Returns `None` when no event is pending. An event whose text is
    /// empty is still returned: a clipboard request with empty text is the
    /// "clear the selection" form, not the absence of a request.
    pub fn take_event(&mut self) -> Option<Event>;
}
```

`&mut self` is unchanged in spirit from `take_clipboard()`. Every other
accessor on `TerminalState` borrows `&self` because its values are properties
of the terminal; an event is not, and the mutation is what makes "I have acted
on this" expressible. The doc comment on the old method (why a take and not a
borrow, why not shaped like the reply buffer) is the doc comment on this one,
with "clipboard request" generalized to "event".

### A queue, not a slot

The current storage is `clipboard: Option<ClipboardRequest>`, a single slot: a
second OSC 52 in one feed overwrites the first. That is correct for one family
whose meaning is "the current request", and it is what `take_clipboard()`
documents (taking returns the request while it is pending, `None` once read).
The event channel is a queue instead:

```rust
// internal, illustrative
pub(crate) events: VecDeque<Event>,
```

A `VecDeque` because a caller drains from the front while `osc_dispatch` pushes
to the back, and because cross-family order has to be preserved (the second
motivation above).

This changes one observable behavior for the existing family, and the change is
intentional: two OSC 52 writes in a single `feed` now both survive, where the
slot kept only the second. A clipboard *write* is not a "latest value wins"
state update - each write is a distinct ask - so preserving both is the honest
record of what the child sent. It also hands the caller a choice it did not
have with the slot: it can act on both writes, or keep only the last one it
saw. The slot made that decision for the caller by discarding the earlier
write before the caller could look; the queue leaves the choice where it
belongs. It is also the shape the second family needs anyway.

The queue is unbounded. This is defensible on the same grounds the current
clipboard has no cap (a caller that takes an event is trusted with it, and a
child that floods the channel is a child that floods the PTY, which the caller
already has to survive), but it is the one part of this proposal most worth
pushing back on: unlike the slot, a queue can grow without the caller acting.
The alternative - a bound with a defined overflow policy - is a real design
question, not a detail, and is left open below rather than settled by default.

### Why `take_event`, not `next_event`

Both names are idiomatic for "remove and return the next item", so the choice
rests on what each implies about the receiver and the value.

- `next_event()` names the *position*: "give me the next one". It reads like an
  iterator adapter (`Iterator::next`) and so suggests the channel is something a
  caller steps through, with the queue as the thing being consumed. A caller
  might reasonably read it as side-effect-free stepping, or expect an iterator
  to exist beside it.
- `take_event()` names the *mutation*: "remove it from the crate". This is the
  same verb the crate already uses for the equivalent operation elsewhere -
  `Screen::take_dirty()` clears the flag as it reads it, and the old
  `take_clipboard()` did the same - so it is the word this codebase has already
  chosen for "read and clear".

The consistency argument decides it: termnix has an established `take_*` for
"destructive read of a pending flag", and `next_*` has no precedent here (the
only `next` in the crate is a local column variable in `terminal_emu.rs`).
`take_event()` also keeps the parallel with the method it replaces, so the
rename is the smallest change a reader can carry: `take_clipboard()` became
`take_event()` and returns `Event::Clipboard` instead of the request directly.

### Why `Event::Clipboard`, not a change-flavored name

A variant named for a *change* (something like `ClipboardUpdated`) would say
the value is a notification that state moved. It is not: termnix holds no
clipboard, so nothing of the crate's moved, and the request may be for a
selection the crate never had. The value is the child's ask, verbatim, and the
variant name says what it is (`Clipboard`) rather than what a caller should
infer about the terminal (`Updated`). The variant name says what the value is
(`Clipboard`), and the absence of a change verb is the honest reading: the
crate decoded the sequence but does not use the result, so there is no change
of the crate's own for the name to describe.

## Drawbacks

- **A breaking change for one family.** `take_clipboard()` is public and the
  new shape is strictly more indirection: a caller that only ever wanted the
  clipboard now matches on a variant to get it. The cost is paid now for a
  family that does not exist yet, which is the usual risk of consolidating
  ahead of the second case.
- **An unbounded queue in place of a bounded slot.** A slot cannot grow; a
  queue can, and a caller that never drains will hold every event the child
  ever produced. The queue also changes the existing family's behavior: two
  writes in one feed now both survive, so a caller that only wants the last one
  has to drain the rest. That is a choice moved to the caller, not a burden
  imposed on it, but a caller written against the slot still has to be updated
  to say which it wants.
- **A new family is a breaking change.** Because the enum is exhaustive, a
  caller that matches every variant stops compiling the moment a variant is
  added, and every downstream `match` must be updated. This is accepted for
  the reason the type gives - a silent `_ => {}` is the failure mode to avoid -
  but it means the crate cannot add a family in a patch or minor release.
- **The channel is still only OSC.** Nothing here makes a non-OSC event
  possible; the crate has no such event today. If the first non-OSC event turns
  out not to fit this shape, the enum is the wrong container and the
  generalization was premature.

## Rationale and alternatives

- **Keep `take_clipboard()` and add `take_event()` beside it.** No break, and a
  caller that only wants the clipboard keeps its method. Rejected because the
  two accessors would return overlapping data (the event channel still holds
  the clipboard request, so `take_clipboard()` would have to drain to find it
  or the request would be in two places), and because the point of the change
  is one drain point, which two methods do not give.
- **Return an iterator (`events(&mut self) -> impl Iterator<Item = Event>`).**
  Hides the drain loop and the queue behind a standard shape. Rejected for now:
  it makes the channel look infinite and read-only, while the actual contract is
  "take from a queue that a later `feed` refills", which a caller needs to
  understand to write a poll loop correctly. A `while let` over `take_event()`
  says the same thing without implying an iterator that outlives the borrow.
- **A callback (`on_event(&mut self, f: impl FnMut(Event))`).** Inverts control
  and removes the caller's need to loop. Rejected: it forces the handling code
  into a closure for no gain, and it makes "take one and stop" impossible,
  which a caller that wants the newest event needs. The take is the smaller
  contract.
- **A fixed struct of `Option`s (`Events { clipboard: Option<...>, ... }`).**
  An exhaustive struct with no wildcard arm, one `take_events()` returning the
  lot. Rejected: it grows a field per family (the same work as a method per
  family, one indirection up) and loses arrival order across fields, which is
  one of the two things the single channel is for.
- **`next_event()` as the name.** See above; rejected on consistency with the
  crate's existing `take_*` for destructive reads and because `next` reads as
  iteration over a collection rather than removal from the terminal.
- **Keep the clipboard a slot and make the channel a slot too (drop the second
  event).** Preserves today's behavior exactly. Rejected because it silently
  drops a child's request, which is the failure the whole OSC handling policy
  exists to stop.
- **Do nothing.** The clipboard stays reachable only as `take_clipboard()`, and
  the next family - which the policy says is coming - lands as another
  single-purpose method, with no drain point and no cross-family order. The
  cost is deferred, not avoided, and the deferred version has a caller already
  written against the one-method shape that then has to change.

## Unresolved questions

- **Is the event queue bounded?** Left open on purpose (see above). A bound
  needs an overflow policy (drop oldest, drop newest, refuse to enqueue) and a
  place for the limit to live; none of that is obvious enough to pick here, and
  adding a bound later is not a breaking change. A future family whose events
  are large (an OSC payload can be) makes this more urgent than the current
  clipboard-only channel does.
- **Should the `while let` drain be the documented idiom, or is a
  "clear all pending" method wanted?** A caller that only wants the newest
  event drains and discards; a method that returned the last event and dropped
  the rest would say so directly, but it also bakes in "newest wins", which is
  wrong for a family whose events are all distinct. Open.
- **Does `Event` live in `terminal_types.rs` beside `ClipboardRequest`, or in
  `terminal.rs`?** A placement question, not a design one; the crate keeps
  public data types in `terminal_types.rs`, so that is the default, but `Event`
  is also the type of an accessor, which argues for `terminal.rs`. Settled at
  implementation time.

## Future possibilities

- The passthrough proposal in this series adds a variant to this enum rather
  than a new accessor, which is the case this RFC is written for. Adding the
  variant is a breaking change, and every caller's `match` gets a compile error
  until it names the new case.
- The title proposal, if it moves the title to an event, is another variant.
  With the channel in place, the move becomes a value the caller drains rather
  than a second accessor, and the `title_changed` bookkeeping that accessor
  would have needed is not introduced.
- If the queue gains a bound, that bound is shared by every family, so it is a
  property of the channel rather than of any one event - a reason to settle the
  bound question in this RFC's later revision rather than in a per-family one.
