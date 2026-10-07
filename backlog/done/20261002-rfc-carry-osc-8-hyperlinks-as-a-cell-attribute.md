# RFC: Carry OSC 8 hyperlinks as a pen attribute

- Status: accepted

## Summary

Model OSC 8 hyperlinks by giving the pen a hyperlink attribute, so every cell
painted after an `OSC 8 ; params ; URL ST` remembers the link it belongs to.
The sequence changes what the grid means without drawing a cell itself - the
same shape as SGR, which sets the pen a later paint reads - so its home is the
pen, not the request channel. A cell refers to a link by a small opaque id, which
keeps `Style` and `Cell` `Copy` and leaves the hyperlink out of the per-cell
value a caller clones. Each time the child opens a link the crate emits a
`HyperlinkAdded` event carrying a fresh id and the URL, and the caller keeps the
id-to-URL mapping from there. The crate keeps no link state of its own beyond
the id on a cell and the counter that hands ids out, so there is nothing to
prune and nothing to keep exact.

## Motivation

A host application that embeds a child session renders the child's output by
reading cells back and painting them. OSC 8 is how a child marks a run of cells
as a hyperlink: the sequence carries a URL and sets it as the current pen's
hyperlink, and a later sequence with an empty URL turns hyperlinking off. A
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

The URL itself is not cell state, though. A cell carries a link id, and the id
names a URL the child wrote once for a whole run - the URL is not repeated per
cell and is not something the grid needs to answer "what does this cell mean".
The crate therefore keeps no URL table: it hands each URL to the caller, once,
as a `HyperlinkAdded` event, and the caller keeps the id-to-URL mapping. The
caller already has to hold a mapping of its own to render links (it is the
side that draws them), so the crate building a second one internally - and
spending a reference count on it to know when to drop it - would be bookkeeping
neither side needs.

This differs from the color sequences on purpose. A cell's color is a value on
the cell; the crate settles colors by handing each set and query to the caller
as a request. A hyperlink is not a value the crate can hand over in one go and
forget, because the cells that carry it outlive the sequence, so the crate
keeps the id on the cell and reports each opening as it happens. What it does
not do is keep the URL: that lives with the caller.

## Guide-level explanation

Before, a hyperlink run reached the caller as if it were plain text:

```rust
// The child wrote ESC ] 8 ; ; https://example.com ST click here ESC ] 8 ; ; ST
// Every cell in "click here" has `style.hyperlink` ... which does not exist.
```

After, the cells in the run name the link, and the caller keeps the URL it was
handed when the link opened:

```rust
let mut urls: HashMap<HyperlinkId, String> = HashMap::new();

while let Some(event) = term.dequeue_event() {
    if let Event::HyperlinkAdded { id, url } = event {
        urls.insert(id, url);
    }
}

for row in term.rows() {
    for cell in row {
        if let Some(url) = cell.style.hyperlink.and_then(|id| urls.get(&id)) {
            // cell is inside the run for `url`; make it clickable,
            // underline it, or show the URL on hover
        }
    }
}
```

The shift in thinking is that the pen has a hyperlink field the same way it has
a foreground color. Setting `OSC 8` changes the pen; painting a cell copies the
pen into that cell. A cell does not carry the URL itself - it carries a small id
- so a cell stays a couple of words wide and the URL is stored once per open
rather than once per cell. A run of a thousand linked characters keeps one small
id on each cell and one URL on the caller's side.

When the child opens a link, the crate emits `HyperlinkAdded` carrying a fresh
id and the URL, and the caller stores it. That event is the whole of the crate's
obligation: it says "this id means this URL", and from then on a cell's id
resolves against the caller's own map. The crate gives every open a fresh id
(even a repeat of the same URL), so a caller that wants two opens of one link to
share a map entry does the interning itself, keyed by whatever it likes - the
URL, or the child's `id` parameter if it reads the passthrough stream. The
caller is the side that renders the cells, so the map's size and lifetime are
its to choose.

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

### The URL

Each open carries one value, the event's `url` field (there is no separate
`Hyperlink` type):

- It is the URL the child attached to the run (`OSC 8 ; params ; URL`).
- It is stored as framed, with the same lossy UTF-8 decoding the title and
  clipboard paths use; the crate does not validate or interpret it.

The caller holds it in a map keyed by the id the event also carries. The crate
keeps no copy once the event is delivered.

### The id

```rust
/// Identifies one opening of a hyperlink (OSC 8).
///
/// Opaque. Look it up in the map the caller builds from `HyperlinkAdded`
/// events to get the URL.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct HyperlinkId(NonZeroU32);
```

`HyperlinkId` wraps a `NonZeroU32` so that `Option<HyperlinkId>` is the same
size as a `u32` (niche optimization), keeping `Style` small; `None` is the
common case and costs nothing. The id is assigned by the crate and is opaque to
the caller; it is not the child's `id` parameter, which is a string the child
controls and which two runs may share.

The id is **per opening, not interned**: each `OSC 8` with a non-empty URL takes
the next id, and two opens that name the same URL get two different ids. The
crate does not hold any set to recognize a repeat - that is deliberate. Interning
would mean keeping the set of `(url, id)` pairs it has opened, and nothing would
drop them (nothing tracks which cells still refer to a link), so the set would
grow with the child's output. Emitting a fresh id per open and holding nothing
keeps the crate's own state to a single counter. A caller that wants two opens of
one link to share an entry interns on its own side, keyed by the URL or by the
child's `id` parameter; the crate does not decide the grouping.

The id is a `NonZeroU32`, so the id space is a little over four billion values.
A terminal that hands out one id per open is not expected to reach it, and rather
than stop, the counter **wraps**: an id that would exceed the space starts again
from the low end. A wrap could, in principle, hand out an id a live cell already
refers to, which would make two different URLs share a cell's id in the caller's
map. That takes more than four billion opens in one terminal's life, which no
session reaches; the crate does not keep a set of handed-out ids to rule it out,
because that set would grow with the child's output for a case that does not
occur. The wrap is named here so the bound is not left implied by the integer
width.

### The event

`Event` gains one variant:

```rust
pub enum Event {
    // ... existing variants ...
    /// The child opened a hyperlink (OSC 8); `id` maps to `url` in the
    /// caller's own table.
    HyperlinkAdded { id: HyperlinkId, url: String },
}
```

It is emitted on every open of a non-empty URL, before the pen is set, one event
per `OSC 8`. Two opens of the same URL emit two events with two ids.

Unlike the state flags, this event carries a payload that must not be lost:
every open in a `feed` has to be delivered, so a feed that opens two links
yields two `HyperlinkAdded` events from `dequeue_event`, in order, and neither
collapses into the other. `RequestReceived` is the existing unmerged,
payload-carrying event, so `HyperlinkAdded` is the same kind of thing, not a
state flag. How it rides the channel is an implementation detail left to
Unresolved questions.

The crate keeps no URL table and exposes no lookup: the only path from a cell's
id to a URL is the caller's map, which the caller fills from these events. If
the caller drops its map, or drops an entry, a cell's id resolves to nothing -
which is the caller's choice, and correct: the crate handed it the URL once and
does not hold it back.

### Parsing the sequence

The sequence is `OSC 8 ; params ; URL ST`, where `params` is a `:`-separated
list of `key=value` pairs and `URL` is the target. In `osc_dispatch`:

```rust
"8" => self.osc_hyperlink(params),
```

`osc_hyperlink` reads `params[1]` as the parameter string and `params[2]` as the
URL:

- An **empty URL** (or no `params[2]`) ends the current link: the pen's
  `hyperlink` becomes `None`, and the next cell paints with no link. The close
  is not a change to any cell - the cells painted before it keep their id - and
  it emits no event; a caller that wants to know whether a link is current
  reads [`style()`](crate::TerminalState::style).
- A **non-empty URL** takes the next id and emits `HyperlinkAdded { id, url }`,
  then sets the pen's `hyperlink` to that id. Every open gets a fresh id; a
  repeat of the same URL is not detected and does not reuse an id.

The crate does not read the `params` list at all. The child's `id` parameter
(`OSC 8 ; id=... ; URL`) is one of the keys that can appear there, and the crate
ignores it: the grouping it expresses (two runs meant as one link) is a
rendering concern, and a caller that wants it reads the `id` key from the
passthrough stream or interns on the URL. Any other `key=value` pair is ignored
the same way (the sequence is handled; the parameter is not modelled). The raw
`params` string is not kept and not forwarded; a caller that needs it has the
passthrough bytes.

The crate does **not** validate the URL, decode it, or split it into scheme and
path. It stores the bytes as the `String` the tokenizer produced, keeping the
same lossy decoding the title and clipboard paths use. A URL is displayed or
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
the grid and the retained history. RIS does **not** reset the id counter. Ids are
handed out and not reused, so an id a caller kept across a reset still names the
URL the caller stored for it, and RIS does not take that back; a post-reset open
takes a fresh id beyond where the counter had reached.

The crate keeps no table or set here, so RIS has nothing else to do beyond the
pen it already clears.

### The event channel

As above, this adds one event, `HyperlinkAdded`, to the event channel. It is the
only new reporting surface; the request channel is untouched. It is not a
request: it says an id now means a URL, not that the caller must do something,
and the caller may ignore every one of them.

### What is still not modelled

The sequence's `params` list can carry keys beside the URL (the common one being
the child's `id`), and the crate reads none of them. A URL containing `;` or `:`
is split by the frame's rules before the crate sees it, which is a property of
OSC framing, not a choice here. Neither changes the shape of this RFC.

## Drawbacks

- **`Style` gains a field, which is a breaking change to a public struct.**
  Every caller that constructs `Style { .. }` literally (rather than via
  `Style::default()` and field assignment) stops compiling, and `Cell::EMPTY` /
  `Cell::CONTINUATION` literals in the crate gain a field. This is the cost of
  putting the attribute on the pen, and it is real for callers that build
  styles by hand.
- **The caller owns the id-to-URL map and its lifetime.** The crate hands each
  URL over once, in a `HyperlinkAdded` event, and never repeats it. A caller
  that discards its events, or drops the map, loses the ability to resolve ids
  it still sees on cells - there is no lookup fallback. This is the expected
  cost of not keeping the URL in the crate, but it is a contract the caller has
  to honour: events must be drained and the map kept for as long as the caller
  renders the cells.
- **A new id per open means the caller must intern if it wants grouping.** Since
  the crate does not intern, a child that prints the same link a thousand times
  makes a thousand `HyperlinkAdded` events, and a caller that keys its map by id
  alone grows it one entry per open. A caller that wants per-link entries
  interns on its own side (by URL, or by the child's `id` from the passthrough
  stream). This is the deliberate trade for the crate keeping no set of its own.
- **A wrap could make two URLs share an id.** The counter is not guarded by a
  set of handed-out ids, so in principle a wrapped id collides with a live one,
  giving two URLs the same id in the caller's map. The case needs more than four
  billion opens in one terminal's life, so it is named in the docs and left
  unreachable rather than paid for with a set that grows with the child's
  output.
- **The caller does a lookup per cell to get a URL.** Rendering a linked run
  means one id-to-URL lookup per cell against the caller's own map (or spotting
  the run boundaries first), where a cell that carried the URL would need none.
  The trade is the cell size: a `String` per cell would make `Cell` non-`Copy`
  and every cell carry a URL, and removing the id indirection would make `Style`
  larger and every cell wider. The indirection is the cheaper mistake.
- **OSC 8 has no read form.** Unlike OSC 4 and the color sequences, there is
  no `?` query for a hyperlink, so this RFC does not exercise the policy's
  "answer a query from state" path. That path went unbuilt: the color
  sequences were settled by handing their queries to the caller instead, so no
  sequence in the crate answers a `?` from state.

## Rationale and alternatives

- **Put the URL (a `String`) directly in `Style`.** Rejected: `Style` would
  lose `Copy`, `Cell` would lose `Copy`, `Cell::EMPTY` could no longer be a
  `const`, and every cell in a linked run would carry its own copy of the URL.
  The id indirection keeps all four things and stores the URL once per open on
  the caller's side.
- **Keep the URL in the crate and prune it by hand-maintained reference
  counts.** Rejected: `Cell` is `Copy`, so the count would have to be adjusted
  by hand on every path that writes or clears a cell - set, scroll, reflow, the
  edit and erase sequences, the alternate-screen switch, RIS - and a single
  missed decrement leaks an entry while a double decrement makes a live cell's
  id stop resolving. That is the largest correctness burden in the change, for
  a table the caller has to keep a parallel copy of anyway to render. Dropping
  the table drops the count and the burden with it.
- **Keep the URL in the crate and drop it only on RIS / when the grid is
  cleared.** Rejected: a cell can be overwritten in place without any of those
  events, and a history line can be trimmed, so neither hook is a correct
  lifetime boundary; the table would still leak. Correct expiry needs the
  count, which the previous alternative rejects.
- **Hand the URL to the caller with an add event, but report the last
  reference going away as a drop event too.** Rejected: a drop event would
  require the same hand-maintained count as keeping the table, so it buys the
  caller nothing over just keeping the map it already has. The crate hands over
  each URL once and is done; there is nothing to report on the way out.
- **Make the whole hyperlink a consumed-once request instead of a cell
  attribute.** Rejected by the policy and on its own merits: a hyperlink is an
  attribute of cells that outlive the sequence, not a message the caller takes
  once. The attribute stays on the cells, keyed by an id; the one-shot
  `HyperlinkAdded` event only sets up the id-to-URL mapping. This is the state
  row of the policy's table.
- **Carry the id-to-URL mapping on `ChildRequest` instead of an event.**
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
- **Intern in the crate (dedupe repeated URLs to one id).** Rejected: interning
  means holding the set of `(url, id)` pairs the crate has opened, and nothing
  drops them - nothing tracks which cells still refer to a link - so the set
  grows with the child's output. The crate's whole state stays a single counter
  instead, and a caller that wants dedupe interns on its own side, where it can
  also choose the key (URL, or the child's `id`). The cost is a `HyperlinkAdded`
  per open rather than per distinct link, which the caller can collapse as it
  likes.
- **Use a per-link `Arc`/`Rc` so cells share the URL by reference count.**
  Rejected: a cell would have to hold a reference-counted handle instead of a
  `Copy` id, which takes `Cell` out of `Copy`, makes `Cell::EMPTY` no longer a
  `const`, and puts a refcount increment on every cell write and copy -
  including the bulk `vec![Cell::EMPTY; n]` fills the screen does on every
  resize and clear. This RFC avoids that entirely by keeping no count at all: a
  cell holds a plain id and the URL lives only in the caller's map.
- **Store the child's `id` parameter as the cell's reference, without an
  opaque crate id.** Rejected: the child's `id` is a string, so it reintroduces
  the `String`-in-`Style` problem.
- **Use a `char` as the cell's link reference instead of a `HyperlinkId`.**
  Rejected: a `char` is a character, and a link is not one, so the value would
  be a `u32` wearing a character's name - and a worse one, since `Option<char>`
  has no niche and would make `Style` larger than `Option<NonZeroU32>` does,
  while the surrogate range wastes codepoints that could otherwise be ids.
- **Prune a crate-side set by sweeping the grid and history for still-
  referenced ids.** Rejected: a sweep is `O(cells)`, and it needs a reason to run
  - either every feed (too expensive) or on a timer (too coarse to bound
  anything usefully). It is only needed at all if the crate keeps a set; this
  RFC keeps none, so there is nothing to sweep.
- **Ignore unknown `params` by dropping the sequence.** Rejected: the sequence
  is understood (it is a hyperlink); only an unmodelled parameter is ignored,
  which is the same relationship the crate has to SGR values it does not
  recognize. Dropping the whole sequence would lose the URL.
- **Emit `OSC 8` on the passthrough channel and let the caller track the pen.**
  Rejected: the caller would have to reproduce the pen's hyperlink state and
  know which cells were painted while it was set, which is the grid bookkeeping
  the crate exists to do. The crate already owns the pen; the hyperlink is one
  more field on it.
- **Do nothing.** Hyperlinks stay invisible to every caller; a host cannot make
  linked text clickable or show its URL without a second tokenizer. See
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

## Future possibilities

- A host that wants clickable links reads the pen field and resolves the id
  through the event-fed map it keeps, and a host that wants to underline them
  uses the same field - the crate does not decide the presentation, it exposes
  the association.
- The child's `id` parameter, if a caller turns out to need it, is additive: a
  field on `HyperlinkAdded` carrying it is a small change, and no `Style` or
  `Cell` shape depends on it.
- Crate-side interning, if the per-open events prove too many, is additive too:
  the crate could collapse repeats to one id and emit `HyperlinkAdded` once per
  distinct URL, at the cost of holding the set it holds nothing of today.
- The color sequences (OSC 4 and 10-12) are the sibling case, and the crate
  settled the opposite way: rather than keep the state and resolve cells
  against it, termnix holds no color state and hands each set and query to the
  caller as a request. A hyperlink is different because a cell has to remember
  which run it belongs to, so the crate keeps the id on the cell and reports
  each open once; the URL itself stays on the caller's side, like the colors.
- A future hyperlink extension (a title, a hover text) is a field on
  `HyperlinkAdded`; it does not change `Style` or `Cell`.
- A drop event, if a caller ever wants to know when a link is no longer on any
  cell, would need the crate to track which cells refer to a link, i.e. the
  reference count this RFC rejects. It would be its own, larger change.

## Outcome

Implemented in [#26](https://github.com/sile/termnix/pull/26) (merged as `4617aa3`).

OSC 8 hyperlinks are now carried as a pen attribute. `Style` gained a
`hyperlink` field holding an `Option<HyperlinkId>`, an opaque id standing in
for the URL so `Style` and `Cell` stay `Copy`, and each opening is reported
once as `Event::HyperlinkAdded { id, url }`. The crate keeps no URL table and
no reference count: the caller owns the id-to-URL mapping, and the id on a cell
plus the counter that hands ids out is the crate's whole hyperlink state.

The id counter was made its own space rather than a raw `u32` whose next value
was mapped onto an id by a helper in the crate: that helper aliased the counter
values 0 and 1 onto id 1, so stepping the counter past its maximum handed out id
1 twice in a row. The counter is now a `NonZeroU32` that steps `MAX -> 1`, so no
id is handed out twice in a row and 0 is never an id.

The scope is unchanged from what is described above.
