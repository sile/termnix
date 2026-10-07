# RFC: Rename `next_event()` to `dequeue_event()`

- Status: accepted

## Summary

Rename the pending-event accessor `next_event()` to `dequeue_event()` on both
[`TerminalState`](crate::TerminalState) and [`Session`](crate::Session). The
method's body, its `&mut self` receiver, the `Event` values it yields, and the
order they arrive in are all unchanged. Only the name moves.

The rename does not add behavior. It corrects a verb that already reads as
something the method is not, and it does so while the name is young and the
call sites are few.

## Motivation

The accessor is the one place a caller learns what the child did: it *removes*
the next pending `Event` from a queue and hands it over. On an empty queue it
returns `None`; after it returns `Some`, that event is gone and will not be
seen again. The doc comment already says so - "Returns the next pending event
and consumes it" - and the `&mut self` receiver is there precisely because the
method has an effect.

The name `next_event()` does not say that. `next` is the word Rust gives an
`Iterator`, and `Iterator::next` is understood by many readers as a *look at*
- advance a cursor to the next item - rather than a *take from a queue*. The
mismatch is not new. The RFC that introduced this channel
([unified event channel](../../done/20261005-rfc-introduce-a-unified-event-channel.md))
chose `next_event` over `take_event` on the grounds that `take`, in Rust, means
"swap the whole thing out for its default" (`mem::take`, `Option::take`) or
"cut the iteration short" (`Iterator::take`), and neither carries the idea of a
queue drained one item at a time. That reasoning is correct about `take` and it
did rule `take_event` out - but it does not rule *in* `next`, which inherits
the very `Iterator` association the RFC leaned on, and which does not name the
queue operation either.

What the method actually does is `dequeue`: remove from the front of a queue.
That is the word for the operation, and the crate already reaches for it: the
same vocabulary is the one [`Session::enqueue_input()`](crate::Session::enqueue_input)
is built on, where application input is put *onto* its queue. `dequeue` is the
verb that names the other half of that idea - *off* a queue - and it is the
verb `next` is standing in for. The two methods sit on different queues (the
PTY write queue and the pending-event queue), so the point is not that the
crate is symmetric, only that `dequeue` is the word for a queue operation the
crate already spells the opposite of, and the accessor is doing that operation
under an `Iterator` name instead.

A concrete caller is the code that drives the PTY. On each turn it feeds the
bytes the child wrote and then drains events to react to them:

```rust
while let Some(event) = session.next_event() {
    // react to `event`
}
```

`next_event()` reads here as "look at the next event, maybe there is one". The
body of the loop, though, is written as if the event were consumed - it is not
revisited, and the next call does not return it again. A reader has to go to
the doc comment to learn that the second reading is the right one.
`dequeue_event()` reads as the take it is, and uses the same word the caller
already writes on its input path (`enqueue_input`), even though the two touch
different queues - the point is the verb, not a matched pair.

## Guide-level explanation

Before, a caller drained events with a name shared with `Iterator`:

```rust
while let Some(event) = session.next_event() {
    match event {
        termnix::Event::ScreenUpdated => { /* repaint */ }
        termnix::Event::RequestReceived(request) => { /* act on `request` */ }
        _ => {}
    }
}
```

After, the same loop names the operation:

```rust
while let Some(event) = session.dequeue_event() {
    match event {
        termnix::Event::ScreenUpdated => { /* repaint */ }
        termnix::Event::RequestReceived(request) => { /* act on `request` */ }
        _ => {}
    }
}
```

Nothing inside the loop changes. The events are the same, the merging is the
same, and the order is the same. The only difference is that the method now
says it *takes* an event, matching what its `&mut self` receiver and its doc
comment already said.

A caller that also sends input sees the pair:

```rust
session.enqueue_input(input)?;   // host -> child
while let Some(event) = session.dequeue_event() { /* child -> host */ }
```

## Reference-level explanation

The change is a rename on two impls, with no signature or body change:

```rust
impl TerminalState {
    pub fn dequeue_event(&mut self) -> Option<Event>;
}

impl Session {
    pub fn dequeue_event(&mut self) -> Option<crate::Event>;
}
```

Both keep `&mut self`, keep returning `Option<Event>`, and keep the semantics
described in the current doc comment: the merged state-change flags are
yielded once each and cleared, and the request queue is drained oldest-first.
`Session::dequeue_event()` stays a thin delegate to the terminal's.

Two kinds of mention move with the name:

- **Call sites and doctests** in `README.md`, `examples/tuinix.rs`,
  `examples/headless.rs` (the note explaining why the headless example does
  not drain), and the `src/` doctests. These are mechanical.
- **Cross-references** in rustdoc: intra-doc links
  (`TerminalState::next_event()`, `Session::next_event()`), the `Events` type's
doc comment, the `TerminalState::events` field's doc, and the
`Event`/`Events` overview text in `src/terminal_types.rs`. All become
`dequeue_event`.

The settled RFCs under `backlog/done/` that mention the old name are **not**
rewritten. An RFC records how a decision was reached at the time it was made,
and this rename does not change what those RFCs decided. The one that picked
the old name keeps its text and its rejected alternative; this RFC supersedes
that particular choice, and says so here rather than editing history.

`enqueue_input()` is deliberately *not* touched. `next`/`enqueue` were never a
matched pair (there is no `enqueue_event`), so there is nothing on that side to
make symmetric; the pairing this RFC introduces is `enqueue`/`dequeue` across
the two directions, and it is already correct on the input side. Whether the
`_input` qualifier should stay is a separate question (see the unresolved
questions) and is out of scope here.

## Drawbacks

- It is a breaking change to a public name for a readability gain only. Any
  caller outside this repository that already migrated to `next_event()` must
  migrate again.
- The accessor was just named, and its naming was argued in a settled RFC that
  these notes otherwise treat as closed. Reopening it costs the reader a trip
  to that RFC to see why the first answer was overridden.
- `dequeue_event()` does not say *what kind* of event, only that one is taken.
  A caller still reads the doc comment to learn that some events are merged and
  some are not.

## Rationale and alternatives

- **Why this is the best among the alternatives.** `dequeue_event` names the
  operation (take from a queue) and pairs with `enqueue_input` on the other
  direction, so the crate has one queue vocabulary instead of an `Iterator`
  word on one end and a queue word on the other. It is also the term the doc
  comment's "consumes it" is already describing.
- **Keep `next_event()`.** The status quo, and not wrong - `Iterator`-style
  readers are not misled into a *bug*, only into a second look at the docs.
  The cost of the rename is a breaking change, so "do nothing" is a real
  option. It is rejected because the name is young, the honest alternatives
  (`take`, `pop`, `poll`) are worse for the reasons below, and the pairing with
  `enqueue_input` is worth more than the churn costs now while there are few
  call sites.
- **`take_event()`.** Rejected when the accessor was first named: `take` in
  Rust reads as "swap the whole thing out" or "cut the iteration short", not
  as draining a queue. That analysis still holds and this RFC does not revisit
  it; `dequeue` is the word that `take` was reaching for.
- **`pop_event()`.** `pop` is a queue word and pairs with `push`, but termnix's
  other end is spelled `enqueue_input`, not `push_input`, so `pop_event` would
  pair with a method that does not exist. `dequeue`/`enqueue` is the consistent
  pair. `pop` also carries the `Vec`/`VecDeque` association of "the last
  element", which reads oddly for a method that always takes the oldest.
- **`poll_event()`, in the `Future`/`task` sense.** Suggests a non-blocking
  probe that may leave the event where it is, which is exactly the reading this
  rename is trying to remove; the method always consumes.
- **Return an `Iterator` (or `impl Iterator`) instead of a single method.**
  Would let a caller write `for event in session.events()`. Rejected as a
  larger change with its own questions (what a caller may do with the borrow
  while iterating), and this RFC is about the name, not the shape.
- **Do nothing.** The name stays `next_event()`. Impact: readers keep
  consulting the doc comment to learn that each call consumes, and the crate
  keeps two vocabularies (`Iterator`'s and the queue's) for one queue.

## Unresolved questions

- **Should `enqueue_input()` drop `_input` to `enqueue()`?** Not required by
  this rename, and it would make the pair `enqueue`/`dequeue_event`
  asymmetric in the other direction. It plays into the pairing this RFC is
  built on, so it is flagged here rather than silently decided against.
- **Do the settled RFCs under `backlog/done/` that argue for `next_event()`
  need a pointer?** This RFC states, in its reference section, that it
  supersedes that choice without rewriting those files; whether that is enough
  or the old RFC deserves an inline note is open.

## Future possibilities

- If the crate later exposes an `Iterator` over events (for `for` loops),
  the name `dequeue_event()` keeps `next` free to mean the `Iterator`'s own
  method without a collision of meaning.
- If a second queue is ever drained on this crate's public surface (the reply
  buffer is the nearest thing, currently split into
  `pending_reply_bytes()`/`advance_reply_bytes()`), `dequeue`/`enqueue` gives
  the vocabulary to name it consistently.

## Outcome

Implemented in [#25](https://github.com/sile/termnix/pull/25) (merged as `3a64bbf`).

The rename landed as proposed. `TerminalState::next_event()` and `Session::next_event()` are `dequeue_event()`, with the receiver, the returned `Option<Event>`, the values yielded, and their order all unchanged. The second is still a thin delegate to the first.

Call sites, doctests, and intra-doc links moved with the name across `src/`, `examples/`, `tests/`, and `README.md`.

The settled RFCs under `backlog/done/` were left as they were: they record how an earlier decision was reached, and this change supersedes one of those choices without rewriting what was decided at the time. The one open proposal that still named the accessor had its mention updated so it points at the current name.

The scope is unchanged from what is described above.

The scope is unchanged from what is described above.
