# RFC: Carry OSC 8 hyperlinks as a cell attribute

- Status: draft

## Summary

Model OSC 8 hyperlinks by giving the pen a hyperlink attribute, so every cell
painted after an `OSC 8 ; params ; URI ST` remembers the link it belongs to.
The sequence changes what the grid means without drawing a cell itself - the
same shape as SGR, which sets the pen a later paint reads - so its home is the
pen, not the request channel. A cell refers to a link by a small opaque id, which
keeps `Style` and `Cell` `Copy` and leaves the hyperlink out of the per-cell
value a caller clones. Each time the child opens a link the crate emits a
`HyperlinkAdded` event carrying the new id and its URI, and the caller keeps the
id-to-URI mapping from there. The crate keeps no link state of its own beyond
the id on a cell and the counter that hands ids out, so there is nothing to
prune and nothing to keep exact.

## Motivation

A host application that embeds a child session renders the child's output by
reading cells back and painting them. OSC 8 is how a child marks a run of cells
as a hyperlink: the sequence carries a URI and sets it as the current pen's
hyperlink, and a later sequence with an empty URI turns hyperlinking off. A
child that prints a clickable link - a compiler diagnostic pointing at a file,
a `git log` pointing at a commit, a tool printing an issue URL - emits one
`OSC 8` before the run and one after.

The crate offers those sequences on the passthrough channel, so the bytes are
not lost - but an offered sequence is not an attached attribute. A hyperlink is
not a thing the child asks the caller to *do*, it is a thing the child attaches
to the grid, and a caller that only sees the offered bytes has to reproduce the
pen's hyperlink state and track which cells were painted while it was set. The
crate paints the run as ordinary cells with no record of the link, so a host
that wants to make the text clickable has no way to know which cells are linked
from the grid alone, and a host that wants to show the URL on hover has to
reconstruct the association from the passthrough stream. That reconstruction is
the exact bookkeeping the crate already does for the ordinary pen.

This is the case the OSC handling policy places on the *state* side and not the
request side: a hyperlink attribute changes what the grid means when a later
paint reads it, so offering it to a caller as bytes leaves the cells painted
with a missing attribute and no way for the host to put it back. The policy
names OSC 8 as `state, once modelled`; this RFC is that modelling.

The URI itself is not cell state, though. A cell carries a link id, and the id
names a URI the child wrote once for a whole run - the URI is not repeated per
cell and is not something the grid needs to answer "what does this cell mean".
The crate therefore keeps no URI table: it hands each URI to the caller, once,
as a `HyperlinkAdded` event, and the caller keeps the id-to-URI mapping. The
caller already has to hold a mapping of its own to render links (it is the
side that draws them), so the crate building a second one internally - and
spending a reference count on it to know when to drop it - would be bookkeeping
neither side needs.

This differs from the color sequences on purpose. A cell's color is a value on
the cell; the crate settles colors by handing each set and query to the caller
as a request. A hyperlink is not a value the crate can hand over in one go and
forget, because the cells that carry it outlive the sequence, so the crate
keeps the id on the cell and reports each opening as it happens. What it does
not do is keep the URI: that lives with the caller.

## Guide-level explanation

Before, a hyperlink run reached the caller as if it were plain text:

```rust
// The child wrote ESC ] 8 ; ; https://example.com ST click here ESC ] 8 ; ; ST
// Every cell in "click here" has `style.hyperlink` ... which does not exist.
```

After, the cells in the run name the link, and the caller keeps the URI it was
handed when the link opened:

```rust
let mut links: HashMap<HyperlinkId, Hyperlink> = HashMap::new();

while let Some(event) = term.dequeue_event() {
    if let Event::HyperlinkAdded { id, link } = event {
        links.insert(id, link);
    }
}

for row in term.rows() {
    for cell in row {
        if let Some(link) = cell.style.hyperlink.and_then(|id| links.get(&id)) {
            // cell is inside the run for `link.uri`; make it clickable,
            // underline it, or show the URI on hover
        }
    }
}
```

The shift in thinking is that the pen has a hyperlink field the same way it has
a foreground color. Setting `OSC 8` changes the pen; painting a cell copies the
pen into that cell. A cell does not carry the URI itself - it carries a small id
- so a cell stays a couple of words wide and the URI is stored once per link
rather than once per cell. A run of a thousand linked characters keeps one small
id on each cell and one URI on the caller's side.

When the child opens a link, the crate emits `HyperlinkAdded` carrying the id
and the URI, and the caller stores it. That event is the whole of the crate's
obligation: it says "this id means this URI" once, and from then on a cell's id
resolves against the caller's own map. The crate never re-uses an id for a
different URI, so the map only grows within a terminal's life unless the caller
decides to drop entries it no longer needs - and the caller is the side that can
know, because it is the side that renders the cells.

A host that does not show hyperlinks ignores both the field and the event.
Nothing about the cell layout, the text, or the style bits the crate already
exposes changes; the field and the event are new data the caller may use or
ignore.

## Reference-level explanation

### The pen field

`Style` gains one field:

```rust
pub struct Style {
    pub foreground: Option<Color>,
    pub background: Option<Color>,
    pub bold: bool,
    pub italic: bool,
    pub underline: bool,
    pub reverse: bool,
    /// Link this cell belongs to, if any (OSC 8).
    pub hyperlink: Option<HyperlinkId>,
}
```

`Style` stays `Copy`, `Hash`, and `Eq` because `HyperlinkId` is a small value
(see below). This matters: `Cell` embeds `Style` and is `Copy`, callers hash
styles, and the `Style::default()` and `Cell::EMPTY` constants must keep
working in a `const` context. A `String` field would end all three.

`Style::default()` has `hyperlink: None`, and the `Cell::EMPTY` /
`Cell::CONTINUATION` literals gain `hyperlink: None` to keep compiling.

### The link value

Each open carries one value:

```rust
/// A hyperlink the child opened (OSC 8).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Hyperlink {
    /// The URI the child attached to the run (`OSC 8 ; params ; URI`).
    ///
    /// Stored as framed, with the same lossy UTF-8 decoding the title and
    /// clipboard paths use; the crate does not validate or interpret it.
    pub uri: String,
    /// The link's `id` parameter, if the child gave one.
    ///
    /// The child uses `id` to say that two separate runs are one logical link
    /// (so a terminal that underlines links can avoid drawing a break between
    /// them). `None` when the sequence carried no `id`.
    pub id: Option<String>,
}
```

The caller receives this in the `HyperlinkAdded` event (see below). The crate
keeps no copy once the event is delivered; the caller holds it in a map keyed by
the id the event also carries.

### The id

```rust
/// Identifies a hyperlink the child opened (OSC 8).
///
/// Opaque. Look it up in the map the caller builds from `HyperlinkAdded`
/// events to get the link's URI.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct HyperlinkId(NonZeroU32);
```

`HyperlinkId` wraps a `NonZeroU32` so that `Option<HyperlinkId>` is the same
size as a `u32` (niche optimization), keeping `Style` small; `None` is the
common case and costs nothing. The id is assigned by the crate and is opaque to
the caller; it is not the child's `id` parameter, which is a string the child
controls and which two runs may share.

The id is **interned**: two `OSC 8` sequences that name the same `(uri, id)`
pair resolve to the same `HyperlinkId`, and the crate emits `HyperlinkAdded` for
the pair only the first time. Interning keeps the caller's map to one entry per
distinct link rather than one per run, and it makes the id a target rather than
an occurrence: the cells of two runs the child meant as one link compare equal,
because they carry the same id.

The crate holds the interned pairs only long enough to recognize a repeat - a
set of `(uri, id)` it has already opened - and hands the caller each id and URI
the first time. It does not track which cells still refer to an id, so nothing
is dropped; the set only grows, bounded by the number of distinct links the
child opens. The caller's map grows the same way and is the caller's to manage.

The id is a `NonZeroU32`, so the id space is a little over four billion values.
A terminal that hands out one id per distinct link is not expected to reach it,
and rather than stop, the counter **wraps**: an id that would exceed the space
starts again from the low end. A wrap could, in principle, hand out an id a live
cell already refers to, but that takes more than four billion distinct links in
one terminal's life, which no session reaches; the wrap is named here so the
bound is not left implied by the integer width.

### The event

`Event` gains one variant:

```rust
pub enum Event {
    // ... existing variants ...
    /// The child opened a hyperlink (OSC 8); `id` names `link`.
    HyperlinkAdded { id: HyperlinkId, link: Hyperlink },
}
```

It is emitted once per distinct interning key: the first time the crate sees an
`OSC 8` naming a `(uri, id)` pair, it assigns an id, stores the pair so a repeat
resolves to the same id, and emits `HyperlinkAdded` before the pen is set. A
second sequence naming the same pair emits nothing - the caller already has the
mapping.

Unlike the state flags, this event carries a payload that must not be lost:
every open in a `feed` has to be delivered, so a feed that opens two links
yields two `HyperlinkAdded` events from `dequeue_event`, in order, and neither
collapses into the other. It is not a request (see Rationale) and not per-cell
state; it is a one-time announcement of an id-to-URI mapping the caller is
expected to keep. How this payload rides the existing channel (which so far
carries merged state flags and an unmerged request queue) is an implementation
detail left to Unresolved questions.

The crate keeps no URI table and exposes no `hyperlink()` lookup: the only path
from a cell's id to a URI is the caller's map, which the caller fills from these
events. If the caller drops its map, or drops an entry, a cell's id resolves to
nothing - which is the caller's choice, and correct: the crate handed it the
URI once and does not hold it back.

### Parsing the sequence

The sequence is `OSC 8 ; params ; URI ST`, where `params` is a `:`-separated
list of `key=value` pairs and `URI` is the target. In `osc_dispatch`:

```rust
"8" => self.osc_hyperlink(params),
```

`osc_hyperlink` splits `params[1]` on `:` for the parameter pairs and reads
`params[2]` as the URI:

- An **empty URI** (or no `params[2]`) ends the current link: the pen's
  `hyperlink` becomes `None`, and the next cell paints with no link. The close
  is not a change to any cell - the cells painted before it keep their id - and
  it emits no event; a caller that wants to know whether a link is current
  reads [`style()`](crate::TerminalState::style).
- A **non-empty URI** is interned: if the crate has already seen the same
  `(uri, id)` pair, it reuses that id and emits nothing; otherwise it takes the
  next id, records the pair, and emits `HyperlinkAdded { id, link }`. Either way
  the pen's `hyperlink` is set to the id.

The `id` parameter is the only parameter the crate reads; it is the one xterm
and the common clients agree on, and it is part of the interning key (two runs
with the same `(uri, id)` are one link). Any other `key=value` pair is ignored
(not dropped as a sequence - the sequence is handled, the unknown parameter is
not modelled). The raw `params` string is not kept and not forwarded: the keys
that appear beside `id` are vendor-specific, so a caller that needs them is a
caller that knows the number. Forwarding the raw string is a later, additive
change (see Future possibilities), because `Hyperlink` is the crate's own type
and adding a field breaks no caller.

The crate does **not** validate the URI, decode it, or split it into scheme and
path. It stores the bytes as the `String` the tokenizer produced, keeping the
same lossy decoding the title and clipboard paths use. A URI is displayed or
handed to the host, not interpreted by the crate.

### Effect on visible-change detection

The pen is part of visible state, and it is already compared across a feed:
`feed` snapshots `visible_scalars()` before parsing and compares it after, and
the pen (`Style`) is one of its fields. A change to the pen is therefore a
visible change by itself. Adding the hyperlink field does not need a new path
for the pen half: setting or clearing the pen's hyperlink makes
`visible_scalars()` differ, so `feed` reports `Event::ScreenUpdated`, exactly as
it already does when an SGR sets a color.

The cell half is unchanged. Painting a cell with the pen copies the link id
into the cell, and that write marks the screen dirty by the mechanism that
marks any paint dirty, so a cell painted inside a linked run differs from what
it was. A feed that sets a link but paints no cell still reports a change,
because the pen moved; a feed that sets a link, paints a cell, and clears the
link reports one `ScreenUpdated` in total, the flag being merged.

`HyperlinkAdded` is a separate event on the same channel and is not merged away
by `ScreenUpdated`: a feed that opens a link reports both. It fires whether or
not the crate paints any cell with the link, because it reports the *opening*,
not a change to the grid.

### Effect on RIS

RIS (`reset_child_state`) restores the terminal, so it clears the pen
(`self.pen = Style::default()`, which now also clears the hyperlink) and clears
the grid and the retained history. RIS does **not** clear the interning set or
reset the id counter. Ids are handed out and not reused, and the interning set
only exists to recognize a `(uri, id)` pair the crate has already opened; an id
a caller kept across a reset still names the URI the caller stored for it, and
RIS does not take that back. Clearing the set would only let a post-reset open
of the same pair take a fresh id, breaking the interning the caller's map relies
on.

There is no link table to wipe, so RIS has nothing else to do here beyond the
pen it already clears.

### The event channel

As above, this adds one event, `HyperlinkAdded`, to the merge-into-a-flag event
channel. It is the only new reporting surface; the request channel is untouched.
It is not a request: it says an id now means a URI, not that the caller must do
something, and the caller may ignore every one of them.

### What is still not modelled

The sequence's `params` list can carry more than `id` (a host may add its own
keys), and the crate reads only `id`. A URI containing `;` or `:` is split by
the frame's rules before the crate sees it, which is a property of OSC framing,
not a choice here. The child's `id` parameter takes part in interning (two runs
with the same `(uri, id)` are one link) and is kept on the `Hyperlink` as
written; the caller has it in its map for a caller that wants the grouping. The
parameter is not consulted beyond the interning key. Neither changes the shape
of this RFC.

## Drawbacks

- **`Style` gains a field, which is a breaking change to a public struct.**
  Every caller that constructs `Style { .. }` literally (rather than via
  `Style::default()` and field assignment) stops compiling, and `Cell::EMPTY` /
  `Cell::CONTINUATION` literals in the crate gain a field. This is the cost of
  putting the attribute on the pen, and it is real for callers that build
  styles by hand.
- **The caller owns the id-to-URI map and its lifetime.** The crate hands each
  URI over once, in a `HyperlinkAdded` event, and never repeats it. A caller
  that discards its events, or drops the map, loses the ability to resolve ids
  it still sees on cells - there is no `hyperlink()` fallback. This is the
  expected cost of not keeping the URI in the crate, but it is a contract the
  caller has to honour: events must be drained and the map kept for as long as
  the caller renders the cells.
- **The crate's interning set only grows.** To recognize a repeat it keeps the
  `(uri, id)` pairs it has opened, and nothing drops them, because nothing
  tracks which cells still refer to a link. It is bounded by the number of
  distinct links the child opens, not by the child's output (interning collapses
  repeats), but a session that opens an unbounded number of distinct links grows
  it without bound. The caller's map grows the same way.
- **The caller does a lookup per cell to get a URI.** Rendering a linked run
  means one id-to-URI lookup per cell against the caller's own map (or spotting
  the run boundaries first), where a cell that carried the URI would need none.
  The trade is the cell size: a `String` per cell would make `Cell` non-`Copy`
  and every cell carry a URI, and removing the id indirection would make `Style`
  larger and every cell wider. The indirection is the cheaper mistake.
- **OSC 8 has no read form.** Unlike OSC 4 and the color sequences, there is
  no `?` query for a hyperlink, so this RFC does not exercise the policy's
  "answer a query from state" path. That path went unbuilt: the color
  sequences were settled by handing their queries to the caller instead, so no
  sequence in the crate answers a `?` from state.

## Rationale and alternatives

- **Put the URI (a `String`) directly in `Style`.** Rejected: `Style` would
  lose `Copy`, `Cell` would lose `Copy`, `Cell::EMPTY` could no longer be a
  `const`, and every cell in a linked run would carry its own copy of the URI.
  The id indirection keeps all four things and stores the URI once per link on
  the caller's side.
- **Keep the URI in the crate and prune it by hand-maintained reference
  counts.** Rejected: `Cell` is `Copy`, so the count would have to be adjusted
  by hand on every path that writes or clears a cell - set, scroll, reflow, the
  edit and erase sequences, the alternate-screen switch, RIS - and a single
  missed decrement leaks an entry while a double decrement makes a live cell's
  id stop resolving. That is the largest correctness burden in the change, for
  a table the caller has to keep a parallel copy of anyway to render. Dropping
  the table drops the count and the burden with it.
- **Keep the URI in the crate and drop it only on RIS / when the grid is
  cleared.** Rejected: a cell can be overwritten in place without any of those
  events, and a history line can be trimmed, so neither hook is a correct
  lifetime boundary; the table would still leak. Correct expiry needs the
  count, which the previous alternative rejects.
- **Hand the URI to the caller with an add event, but report the last
  reference going away as a drop event too.** Rejected: a drop event would
  require the same hand-maintained count as keeping the table, so it buys the
  caller nothing over just keeping the map it already has. The crate hands over
  each URI once and is done; there is nothing to report on the way out.
- **Make the whole hyperlink a consumed-once request instead of a cell
  attribute.** Rejected by the policy and on its own merits: a hyperlink is an
  attribute of cells that outlive the sequence, not a message the caller takes
  once. The attribute stays on the cells, keyed by an id; the one-shot
  `HyperlinkAdded` event only sets up the id-to-URI mapping. This is the state
  row of the policy's table.
- **Carry the id-to-URI mapping on `ChildRequest` instead of an event.**
  Rejected: a [`ChildRequest`](crate::ChildRequest) is defined as something the
  caller is *expected to do*; a link opening is something the caller may or may
  not act on, and `HyperlinkAdded` fits the event channel the crate already uses
  for "here is something you may want to know". The request channel is for
  actions, and there is no action here.
- **Report a `Closed` (or `Removed`) event when the pen's link is cleared.**
  Rejected: the pen closing is not a change to any cell, and a caller that
  wants the current link reads [`style()`](crate::TerminalState::style). The
  cells painted before the close keep their id, so the close is not a lifetime
  boundary for the link.
- **Do not intern; give each open a fresh id (and a fresh event).** Rejected:
  two runs the child meant as one link would then compare unequal as cells, and
  a caller's map would grow one entry per run instead of one per link. Interning
  collapses repeats, which is what keeps both the crate's set and the caller's
  map proportional to distinct links rather than to runs. It costs a key
  comparison on each open, which is cheap next to carrying the event for a
  repeat the caller already has.
- **Use a per-link `Arc`/`Rc` so cells share the URI by reference count.**
  Rejected: a cell would have to hold a reference-counted handle instead of a
  `Copy` id, which takes `Cell` out of `Copy`, makes `Cell::EMPTY` no longer a
  `const`, and puts a refcount increment on every cell write and copy -
  including the bulk `vec![Cell::EMPTY; n]` fills the screen does on every
  resize and clear. This RFC avoids that entirely by keeping no count at all: a
  cell holds a plain id and the URI lives only in the caller's map.
- **Store the child's `id` parameter as the cell's reference, without an
  opaque crate id.** Rejected: the child's `id` is a string, so it reintroduces
  the `String`-in-`Style` problem.
- **Use a `char` as the cell's link reference instead of a `HyperlinkId`.**
  Rejected: a `char` is a character, and a link is not one, so the value would
  be a `u32` wearing a character's name - and a worse one, since `Option<char>`
  has no niche and would make `Style` larger than `Option<NonZeroU32>` does,
  while the surrogate range wastes codepoints that could otherwise be ids.
- **Keep the crate's interning set bounded by sweeping the grid and history
  for still-referenced ids.** Rejected: a sweep is `O(cells)`, and it needs a
  reason to run - either every feed (too expensive) or on a timer (too coarse
  to bound anything usefully). Interning already collapses repeats, so the set
  is proportional to distinct links rather than to cell writes; the sweep would
  trade that for a per-feed cost to reclaim entries the child may yet reuse.
- **Ignore unknown `params` by dropping the sequence.** Rejected: the sequence
  is understood (it is a hyperlink); only an unmodelled parameter is ignored,
  which is the same relationship the crate has to SGR values it does not
  recognize. Dropping the whole sequence would lose the URI.
- **Emit `OSC 8` on the passthrough channel and let the caller track the pen.**
  Rejected: the caller would have to reproduce the pen's hyperlink state and
  know which cells were painted while it was set, which is the grid bookkeeping
  the crate exists to do. The crate already owns the pen; the hyperlink is one
  more field on it.
- **Do nothing.** Hyperlinks stay invisible to every caller; a host cannot make
  linked text clickable or show its URI without a second tokenizer. See
  Motivation.

## Unresolved questions

- **How does `HyperlinkAdded` ride the event channel?** Open, but low risk.
  The channel already has two kinds: merged state flags, and unmerged
  payload-carrying events (`RequestReceived` carries a `ChildRequest` and is
  delivered in order). `HyperlinkAdded` is the second kind, so it belongs on
  the unmerged path, not the flag path. Whether that path is reused as-is or
  named more generally (an "event" queue rather than a "request" queue) is a
  detail to settle when the channel's shape is next looked at; it does not
  change this RFC.
- **Should the crate keep its interning set at all, or emit on every open?**
  Open. Interning is what collapses repeats and keeps the caller's map
  proportional to distinct links, and it is cheap (a key compare per open). The
  alternative - emit `HyperlinkAdded` on every open and let the caller dedup -
  would grow the event stream with the child's output and leave the caller to
  intern anyway. Leaning interning, but not settled.

## Future possibilities

- A host that wants clickable links reads the pen field and resolves the id
  through the event-fed map it keeps, and a host that wants to underline them
  uses the same field - the crate does not decide the presentation, it exposes
  the association.
- A raw `params` string on `Hyperlink`, if a caller ever needs the keys beside
  `id`, is additive: `Hyperlink` is the crate's own type, so a new field breaks
  no caller the way a new `Style` field does.
- The color sequences (OSC 4 and 10-12) are the sibling case, and the crate
  settled the opposite way: rather than keep the state and resolve cells
  against it, termnix holds no color state and hands each set and query to the
  caller as a request. A hyperlink is different because a cell has to remember
  which run it belongs to, so the crate keeps the id on the cell and reports
  each open once; the URI itself stays on the caller's side, like the colors.
- A future hyperlink extension (a title, a hover text) is a field on the
  `Hyperlink` the event carries; it does not change `Style` or `Cell`.
- A drop event, if a caller ever wants to know when a link is no longer on any
  cell, would need the crate to track which cells refer to a link, i.e. the
  reference count this RFC rejects. It would be its own, larger change.
