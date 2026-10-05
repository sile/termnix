# RFC: Introduce a unified event channel

- Status: draft

## Summary

Replace the crate's two ways of telling a caller that something happened - the
`revision()` counter a caller polls and the `take_child_request()` queue it
drains - with a single `Event` enum drained through one `Events` value, in the
shape the `Action`/`Actions` pair uses in Raft implementations the maintainers
have read: an enum whose variants are the individual things that happened, and a
collecting type that holds the not-yet-taken ones, merges the ones that can be
merged, and yields the rest in priority order as an `Iterator`. The collector
stays a private implementation detail; the only public entry point is
`next_event()`.

## Motivation

A host application that embeds a child session has to react to two kinds of
thing the child does, and today has to learn about them two different ways.

When the child draws, the host wants to repaint. To notice that, it polls
`TerminalState::revision()`: it remembers the last value it saw and compares.
That is a *question about state* - "has the visible state moved since I last
looked" - and it is deliberately cheap and repeatable: the same value can be
read any number of times, and a change missed on one turn is still visible on
the next because the counter does nothing but advance.

When the child asks for something the crate cannot finish itself - write to a
clipboard termnix does not own, answer a query only the host can resolve - the
host drains `take_child_request()`. That is a *one-shot event*: taking it
consumes it, because a request the host has acted on must not be handed back on
the next `feed` or the next repaint. The crate's own documentation says this
out loud: a request "is an event, and the event is over once it has been read".

Both are legitimate readings of "something happened", and each way fits the
thing it carries: the repaint signal is a property of the terminal that may be
re-checked, and the request is a discrete fact that must be consumed. But a
host has to write both loops - a poll-and-compare for repaint, a drain-and-match
for requests - and the crate gives no single place where "everything the child
did that this host cares about" arrives. The two differ in spelling (a counter
versus a queue), in the verb (`revision()` versus `take_child_request()`), and
in when a host remembers to call them.

It also makes the crate hard to extend uniformly. Queued asks are one concept
with two shapes; a third thing worth telling the host - and there are
already two more proposed in this series, a terminal reset (`ESC c`) and a
window title, and a third certain to be wanted: the bell, `BEL`, which today is
discarded with no way to observe it at all - has no obvious home. Added to the
request queue, it must pretend to be a *request the host carries out*; observed
only as a `revision()` advance, it must be inferred from state, which the crate
elsewhere rejects.

## Guide-level explanation

Before, the host writes two loops that do not look alike:

```rust
// Repaint: poll, compare, repaint if moved.
if term.revision() != last_seen {
    last_seen = term.revision();
    redraw(&term);
}

// Requests: drain until empty.
while let Some(request) = term.take_child_request() {
    match request {
        ChildRequest::SetClipboard { selection, text, append } => {
            host_clipboard.set(selection, &text, append);
        }
    }
}
```

After, there is one loop and one thing to match on:

```rust
while let Some(event) = term.next_event() {
    match event {
        Event::ScreenUpdated => redraw(&term),
        Event::ScrollbackUpdated => refresh_history_view(&term),
        Event::TitleUpdated => update_title(&term),
        Event::TerminalReset => {
            // Everything derived from the child is stale.
            host_clipboard.clear();
        }
        Event::RequestReceived(request) => {
            match request {
                ChildRequest::SetClipboard { selection, text, append } => {
                    host_clipboard.set(selection, &text, append);
                }
                ChildRequest::RingBell => ring(),
            }
        }
    }
}
```

A host that only cares about repaint matches one arm and ignores the rest; a
host that only cares about requests still walks the loop, but the loop is the
same one and the event it wants is `RequestReceived`. There is no second verb
to remember and no counter to keep.

## Reference-level explanation

`Event` is the enum, one variant per thing the crate tells the caller about:

```rust
pub enum Event {
    ScreenUpdated,
    ScrollbackUpdated,
    TitleUpdated,
    TerminalReset,
    RequestReceived(ChildRequest),
}
```

The names are not forced into one verb. A variant is named for what actually
happened in the crate: the screen is replaced cell by cell, so it is
`ScreenUpdated`; a line is only ever appended to the history, never rewritten,
so `ScrollbackUpdated` carries that the history grew (the one-directional fact
is spelled out in its documentation rather than in its name, so that a caller
sees one word, `Updated`, for everything that updates); a title is replaced, so
it is `TitleUpdated`; a reset is the whole terminal, so it is `TerminalReset`;
a request is received from the child, so it is `RequestReceived`.

`Events` is the collecting type. It is private, an implementation detail the
caller does not name or hold; the one public accessor is a drain loop:

```rust
impl TerminalState {
    /// Returns the next event and consumes it, or `None` if none is pending.
    pub fn next_event(&mut self) -> Option<Event>;
}

// Private: not part of the public surface, only the mechanism behind
// `next_event()`. It is an `Iterator` whose `next` backs `next_event`.
struct Events { /* merged flags and a request queue */ }

impl Iterator for Events {
    type Item = Event;
    fn next(&mut self) -> Option<Event>;
}
```

The name is `next_event()`, not `take_event()`. `take` in Rust means "swap the
whole thing out for its default" (`mem::take`, `Option::take`) or "cut the
iterator short here" (`Iterator::take`); neither carries the idea of a queue
drained one item at a time, which is why the old `take_child_request()` read as
"take it once and be done" even when more were queued. `next` is the word the
language already uses for "the next item, consumed" (`Iterator::next`), which
is exactly the drain loop above. Because the collector is private, the public
surface never has to expose the `Iterator` vocabulary itself - a caller sees
only `next_event()` - but the internal shape and the name agree.

Collecting is per variant, and the difference between a variant that may be
merged and one that may not is the crux of the design:

- `ScreenUpdated`, `ScrollbackUpdated`, `TitleUpdated` and `TerminalReset` are
  *mergeable*: two of each in one `feed` are one fact, and the caller wants the
  latest state, not a count. They are stored as flags and yielded once.
- `RequestReceived(ChildRequest)` is *not* mergeable: three clipboard writes and
  three bells in one `feed` are three things to do, in order. The requests are
  held in a queue and every one is yielded.

If a host resizes the terminal itself, it already knows; a resize is not an
`Event` for the same reason the crate does not notify a caller of a method the
caller just called. If a host trims the scrollback itself with
`trim_scrollback()`, that is likewise its own action and not reported.
`revision()` is removed: its job - "the visible state moved" - is
`ScreenUpdated`, and the merging of that flag is what preserved the old
counter's guarantee that a change missed on one turn is still visible on the
next.

`take_child_request()` is removed; the requests it drained are reached through
`Event::RequestReceived`. The reasoning its documentation carried - why a
request is taken by `&mut self` and not borrowed like the reply buffer - moves
onto `next_event()`. Its verb also changes in the move: `take_child_request()`
was already the wrong word for a queue, per the naming note above.

## Drawbacks

- It is a breaking change to the crate's whole notification surface: a caller
  migrates from two mechanisms to one, and both of the old names are gone.
- "One event loop" is a claim that only pays off for a caller that wants more
  than one kind of event. A caller that only ever repainted now has to run a
  drain loop where it used to compare one integer, which is more code for the
  same job.
- An event enum is a single breaking surface for the future: every later kind
  of event is a new variant, and an exhaustive `match` in a caller breaks each
  time one lands.
- There is no way to look without taking. The only accessor drains, so a caller
  that wants to know *whether* a kind of event is pending, but not consume it
  yet, cannot ask. This is deliberate - a `bool` flag that can be peeked and
  left set risks being read and forgotten - but it is a capability the drain
  loop does not offer.

## Rationale and alternatives

- **Why this is the best among the alternatives.** The alternative is keeping
  the split and adding each new thing to whichever half fits. That is what
  this series already does: the reset is being routed through
  `take_child_request()`, and the title through a `ChildRequest::SetTitle`
  variant, even though a reset is something the crate performs itself before
  telling anyone, and a title is state the crate already holds. Each addition
  then has to argue again whether it is a "state" or an "event", and a host
  reads the answer from whichever method it is told to call. One enum makes
  that a property of the type instead of a per-feature decision.
- **`take_event()` as the name.** Rejected in favour of `next_event()`; see
  the naming note under the reference-level accessor. `take` reads as a
  one-shot or a cut-off, not as draining a queue, and this RFC takes the
  opportunity to correct the verb that `take_child_request()` got wrong.
- **Exposing `Events` so a host can take the batch whole.** Modelled on the
  `Action`/`Actions` pair, where the collector is public and a host can
  `mem::take` it and control when the batch runs. Rejected for now: that
  pairing exists there because persistence must be ordered before outbound
  messages, and termnix has no such ordering constraint, so the fine-grained
  control buys nothing yet. It can be added later without a breaking change
  (see future possibilities), so it is left out of the first cut.
- **`Event` variants as one per kind of merge, with data.** For instance a
  single `Updated { screen: bool, scrollback: bool, title: bool }`. Rejected:
  it makes a caller match once and then read fields, and it abandons the
  enum's whole point, which is that a caller names the thing it handles.
- **The impact of doing nothing.** Every proposed feature keeps its own
  argument about whether it is state or an event, hosts keep two loops, and the
  bell stays unobservable. The crate does not become wrong; it becomes harder
  to extend and harder to explain.

## Unresolved questions

- **What is the priority order of `Events::next()`?** The `Action`/`Actions`
  pair orders local persistence before outbound messages for a specific
  safety reason. This crate has no such ordering constraint that is settled
  yet; the order is part of the design and is left open here.
- **Does `TitleUpdated` belong at all, or is the title delivered as a
  request?** The title is proposed elsewhere in this series as
  `ChildRequest::SetTitle`, i.e. a request the host carries out. If the title
  is instead state the crate holds - which it is today, behind `title()` -
  then `TitleUpdated` is the event and no request is needed. This RFC assumes
  the event and leaves the request proposal to be withdrawn or reconciled.
- **Is `TerminalReset` a complete name?** It names the scope (the whole
  terminal) rather than a `Hard`/`Soft` distinction, which the reset proposal
  rejects while only one reset exists. Whether a caller ever needs to know
  *what* a reset invalidated, rather than just that one happened, is open.
- **Does the bell's merge rule want to be more than `bool`?** A bell is a
  request, so it is not merged here: three bells are three `RingBell`s. If a
  host wants bells coalesced, that is the host's counter, but it is worth
  confirming that no merge is wanted for the one case where a burst is
  plausible.

## Future possibilities

- **Exposing `Events` for batch access.** If a host ever wants to take the whole
  collector with `std::mem::take` and drain it at a point of its choosing, an
  `events_mut()` accessor can be added beside `next_event()` without breaking
  anything. The `Action`/`Actions` pair does this from the start; termnix can
  earn it when a caller asks.
- **More event kinds.** Mouse reporting, hyperlink activation, or any other
  child-driven signal gains a home as a new variant, with its merge rule as
  part of its definition.
- **A matching channel on `Session`.** Process exit is currently reported
  through `Session::try_wait()` and `status()`, which is a third notification
  shape. A `Session`-level events value could unify it with the terminal's,
  without merging the two layers.
- **The request enum shrinks.** As features move from "the host carries it
  out" to "the crate holds it as state", `ChildRequest` may one day hold only
  the asks the crate genuinely cannot resolve - a clipboard read, whose target
  is decided by the host and not by the crate.
