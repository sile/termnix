# RFC: Deliver the window title as a request

- Status: draft

## Summary

Move the OSC 0 / OSC 2 window title off `TerminalState` state and onto the
request channel, as `OscRequest::SetTitle`, removed by the same
`take_osc_request()` that drains the clipboard request. This retires `TerminalState::title()`, the
`title` field, and the `title_changed` flag, which exists only so a `String`
can take part in the visible-change check that every other field does by being
`Copy`. The title is a value the child tells the caller; it is not part of what
the terminal draws, and holding it as state is what forces the crate to keep a
second, special-case change-tracking path for one non-`Copy` field.

## Motivation

The title is the one field of `TerminalState` that is visible state without
being drawable. Nothing about the title affects a cell, the cursor, or a mode;
the crate holds the bytes only because OSC 0 and OSC 2 are the earliest OSC
numbers it learned, back when "store it and expose a getter" was the whole of
its OSC handling. That choice has since accumulated three costs.

First, it is the reason `title_changed` exists. `feed` detects a visible change
by snapshotting `visible_scalars()` - the `Copy` fields - before parsing and
comparing after, a comparison that costs nothing because each field is a couple
of words. A `String` cannot join that compare without allocating on every feed,
so the title got a flag set by `osc_dispatch` and read (and cleared) in `feed`,
which is a second change-tracking mechanism kept alive for exactly one field.
The clipboard request, which arrived later, did not need one: it is a request,
not state, so it is not visible state and has no part in `revision()`.

Second, it makes the crate answer a question it cannot answer well. A title is
the child's to send and the host's to display. A host that embeds a child
session and shows the title in its own chrome wants the title at the moment it
changes; a host that does not show one wants nothing. `title()` returns the
*last* title and no signal that it changed, so a caller polling it for a change
has to compare the previous value itself - the exact bookkeeping the crate does
internally with `title_changed`, duplicated one layer up. The request channel
already delivers a change once, which is what a caller that cares about the
change actually wants.

Third, keeping the title as state creates an asymmetry with the clipboard that
has no rule behind it. Both are messages from the child to the caller, both
originate in OSC, and both are held by the crate only to hand over. One is a
non-destructive getter that participates in visible-change detection; the other
is a consumed-once request that does not. Nothing in the current design says
why.

## Guide-level explanation

Before, a host that wanted to follow the child's title had to poll and diff:

```rust
// Called after every feed, whether or not the title changed.
fn on_render(term: &mut TerminalState, last: &mut String) {
    if term.title() != last {
        *last = term.title().to_owned();
        set_window_title(last);
    }
}
```

After, the title arrives once, on the same channel as everything else the child
asks of the caller:

```rust
while let Some(request) = term.take_osc_request() {
    match request {
        OscRequest::SetClipboard { text, selection, append } => {
            set_clipboard(selection, &text, append)
        }
        OscRequest::SetTitle { title } => set_window_title(&title),
    }
}
```

(An exhaustive `match` also names any other variant the type carries; the
ones above are the clipboard and the title.)

A host that does not show a title stops mentioning it at all: there is no
getter to call and no previous value to remember. A host that wants the current
title remembers the last `OscRequest::SetTitle` it saw, which is the only place a
"current value" can come from once the crate stops holding one.

## Reference-level explanation

This proposal assumes the request channel the OSC handling policy calls for: a
consumed-once `OscRequest` type drained by `take_osc_request()`, whose first
variant is the clipboard request. What this RFC adds is one variant to that
type and the removal of the state the title no longer needs. How the title is removed from
state does not depend on which other variants exist, and a `match` over it
needs a wildcard-free arm only for the variants the type actually has.

### The variant

```rust
pub enum OscRequest {
    /// The child asked to change a selection (OSC 52).
    SetClipboard {
        text: Vec<u8>,
        selection: ClipboardSelection,
        append: bool,
    },
    /// The child asked to set the window title (OSC 0 or OSC 2).
    SetTitle { title: String },
}
```

The full type may carry further variants from the same series; only the one
this RFC adds is shown.

`String`, not `Vec<u8>`, because OSC 0 / 2 already decode to `String`
(`from_utf8_lossy` in `osc_dispatch`) and the resulting value is meant to be
displayed. This matches today's behavior exactly: the same lossy conversion,
now handed to the caller instead of stored.

### Removed API and fields

- `TerminalState::title()` is removed. No state remains for it to return.
- The `title: String` field is removed from `TerminalState`.
- The `title_changed: bool` field is removed.
- `osc_dispatch` stops assigning `self.term.title` / `self.term.title_changed`
  and instead pushes `OscRequest::SetTitle { title }`.
- `feed` stops clearing `title_changed` before parsing and stops reading it in
  the `changed` expression. The expression becomes the primary screen's dirty
  flag OR the `visible_scalars()` compare; the title is no longer a term in it.

### Effect on `revision()`

Setting the title **no longer bumps `revision()`**. This is a deliberate change
in what `revision()` means, and it is the part of this RFC that matters most.
`revision()` is documented as tracking the *visible* state - what a repaint
would have to redraw. A window title is drawn by the host's window system, not
by the cell grid the crate models, so a title change does not make the grid
stale. After this change, the doc comment on `revision()` must drop "and the
window title" from its list of tracked state, and the clipboard comment - which
explains that a request does not bump the counter because it draws nothing -
becomes the general rule the title also follows.

A caller that was using `revision()` to notice a title change (rather than
calling `title()`) must switch to draining the request channel. This is the
breaking half of the change for such a caller, and it is the intended trade:
the counter keeps one meaning (grid repaint needed) instead of two.

### Effect on `soft_reset` / RIS

RIS currently clears the title via `self.title.clear()`. With the title gone
from state, RIS produces no `OscRequest::SetTitle`: a reset does not *set* a
title, so there is nothing to report. A caller holding the last
`OscRequest::SetTitle` keeps it
until the child sends a new one, which is correct - RIS resets the terminal,
and a host's window title is the host's to reset. Note the interaction with
the clipboard, which `soft_reset` also clears: the clipboard is cleared because
a pending *request* is an unhandled ask that a reset invalidates; there is no
corresponding claim about the title once the title is not stored.

### The one thing that does not change

The request is still emitted from the same `"0" | "2"` arm, and OSC 0 and OSC 2
remain indistinguishable (both set the same title). This RFC does not split
them, and does not model the "icon name vs window title" distinction some
terminals draw; neither does the crate today.

## Drawbacks

- **A breaking change to a getter that has been there since the first
  milestone.** `title()` disappears and `revision()` changes meaning. A caller
  that reads the title synchronously, or that relies on `revision()` moving
  when only the title changed, breaks. This is the cost of removing the
  asymmetry, and it is real.
- **A caller that wants the current title must remember it.** The crate stops
  being a place to ask "what is the title now?" and becomes a place that says
  "the title is now X". A caller with several places that need the current
  title must thread the latest value to them, where a getter let each ask the
  crate.
- **`OscRequest::SetTitle` carries an owned `String` that most callers
  discard.** A caller that does not display a title pays for the allocation
  and the match arm anyway. The enum is exhaustive, so the arm is mandatory.
- **One visible-change rule becomes two.** After this RFC, "does it draw?" -
  not "is it visible state?" - is what puts a field on the `revision()` side.
  That is a cleaner rule, but it is a rule the doc has to state, because the
  previous one ("the fields `revision()` covers") is what a reader has today.

## Rationale and alternatives

- **Keep `title()` and add `OscRequest::SetTitle` beside it.** No break, and a
  caller that wants a snapshot keeps it. Rejected because it keeps the
  `title_changed` flag (the state must still participate in `revision()`, and
  the `String` still cannot join the scalar compare) and because the two
  values could diverge - the request fires once, then the getter answers with
  whatever the last one was, so a caller reading both has to reconcile them.
  The whole point is one source.
- **Keep the title as state and drop only the request idea (do nothing).**
  Keeps `title_changed` and the asymmetry. Rejected because the asymmetry is
  what this series of proposals exists to remove; leaving the title in place
  leaves the case that motivated the rule.
- **Expose the title as a `Copy`-able handle or an interned id so it can join
  `VisibleScalars`.** A small integer per title, compared like the other
  scalars. Rejected: it adds an interning table the crate does not otherwise
  need, to keep a field that does not belong in the visible-state compare at
  all. The problem is the field's membership, not its type.
- **Emit the title on a dedicated accessor (`take_title()`) instead of the
  request channel.** Rejected: a consumed-once value belongs on the channel
  that exists so there is one place to drain, and a caller draining two
  accessors has to decide an order between them that carries no meaning. The
  title is not special enough to earn the second accessor.
- **Index the request variant by OSC number (`OscRequest::Other { id, params }`
  and let the caller decode the title too).** Rejected: it would make the title
  untyped and push a `from_utf8_lossy` onto every caller, when the crate
  already has the decoded value in hand.

## Unresolved questions

- **Does `OscRequest::SetTitle` carry `String` or `Vec<u8>`?** `String` matches
  the current lossy decoding and the display intent. `Vec<u8>` would preserve
  the raw bytes for a caller that wants them, at the cost of moving the lossy
  conversion to every caller. Leaning `String`, settled at implementation
  time.
- **Should a title that is set to the same value as the last one still emit?**
  Today, writing the same title twice leaves `title_changed` false the second
  time only by accident of comparing *within a feed*; across feeds it emits
  nothing either way because there is no getter to read. Once the title is a
  request, a child that re-sends the same title is a `feed` that emitted a
  request. Whether to suppress an unchanged repeat (and how to know it is
  unchanged once the crate holds no previous value) is open.
- **Once the title leaves the `"0" | "2"` arm, is there a reason to keep the
  two numbers in one arm?** They are indistinguishable today; whether the
  request should carry which of the two arrived (so a caller could tell the
  icon name from the window title) is open, and the two are the same value in
  this crate.

## Future possibilities

- With the title on the request channel, a host that shows a title in its own
  chrome has a single thing to handle, and a host that shows the title in the
  host terminal's own title bar can forward `OscRequest::SetTitle` to it in
  one line.
- If the crate ever models the "icon name vs window title" split (OSC 0 vs
  OSC 2), that split is a second variant on the same channel rather than a
  second field, which is easier to add and easier to ignore.
- Removing `title_changed` leaves `visible_scalars()` with only `Copy` fields
  and the compare with nothing to special-case, which makes a future field of
  any type less likely to need its own flag.
