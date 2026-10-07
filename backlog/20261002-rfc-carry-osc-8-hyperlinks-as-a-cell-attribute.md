# RFC: Carry OSC 8 hyperlinks as a cell attribute

- Status: draft

## Summary

Model OSC 8 hyperlinks by giving the pen a hyperlink attribute, so every cell
painted after an `OSC 8 ; params ; URI ST` remembers the link it belongs to.
The sequence changes what the grid means without drawing a cell itself - the
same shape as SGR, which sets the pen a later paint reads - so its home is the
pen, not the event channel. A cell refers to a link by a small opaque id, which
keeps `Style` and `Cell` `Copy` and leaves the hyperlink out of the per-cell
value a caller clones. The crate keeps each link's URI in a table the id keys
into, and drops a table entry once no cell refers to it, so the table does not
grow without bound. The crate does not tell the caller when it prunes; the
caller resolves a cell's link through [`hyperlink()`](crate::TerminalState::hyperlink).

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
event side: a hyperlink attribute changes what the grid means when a later
paint reads it, so offering it to a caller as bytes leaves the cells painted
with a missing attribute and no way for the host to put it back. The policy
names OSC 8 as `state, once modelled`; this RFC is that modelling.

There is a second half the passthrough stream cannot carry: a link the crate
does not model is one the crate cannot expire. A table of URIs keyed by link
would grow for the life of the terminal unless something drops the entries no
cell needs - and the crate is the only party that sees a cell overwritten in
place or a history line pushed out. So the crate keeps the table and prunes it:
an entry lives only while some cell refers to it, and the crate can tell,
because it sees every write that creates or destroys a referring cell.

## Guide-level explanation

Before, a hyperlink run reached the caller as if it were plain text:

```rust
// The child wrote ESC ] 8 ; ; https://example.com ST click here ESC ] 8 ; ; ST
// Every cell in "click here" has `style.hyperlink` ... which does not exist.
```

After, the cells in the run name the link, and the caller looks its URI up in the
crate's table:

```rust
for row in term.rows() {
    for cell in row {
        if let Some(link) = cell.style.hyperlink.and_then(|id| term.hyperlink(id)) {
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
id on each cell and one URI in the crate's table.

The crate keeps the URI in a table the id keys into, and prunes that table: once
no cell in the grid or the history refers to a link, its entry is gone, so a
child that prints a link per line does not grow the table for the life of the
terminal. Pruning is invisible to the caller: there is no add event and no drop
event, and [`hyperlink(id)`](crate::TerminalState::hyperlink) simply returns
`None` for an id that is no longer referred to. The caller treats it the same as
`None` from the start - an id it was given from a cell whose link is since gone.

A host that does not show hyperlinks ignores the field. Nothing about the cell
layout, the text, or the style bits the crate already exposes changes; the field
is new data the caller may use or ignore.

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

The table stores one value per link:

```rust
/// A hyperlink the terminal has interned (OSC 8).
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
    /// them). It participates in interning; `None` when the sequence carried
    /// no `id`.
    pub id: Option<String>,
}
```

The caller reads this through
[`TerminalState::hyperlink()`](crate::TerminalState::hyperlink). It owns the
URI inside the crate, not on the caller's side; the id on a cell keys into it.

### The id

```rust
/// Identifies a hyperlink the terminal has interned (OSC 8).
///
/// Opaque. Pass it to
/// [`TerminalState::hyperlink()`](crate::TerminalState::hyperlink) to get the
/// link's URI.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct HyperlinkId(NonZeroU32);
```

`HyperlinkId` wraps a `NonZeroU32` so that `Option<HyperlinkId>` is the same
size as a `u32` (niche optimization), keeping `Style` small; `None` is the
common case and costs nothing. The id is assigned by the crate and is opaque to
the caller; it is not the child's `id` parameter, which is a string the child
controls and which two runs may share.

The id is an **interned** handle: the crate keys its URI table by id, and two
`OSC 8` sequences that name the same `(uri, id)` pair resolve to the same
`HyperlinkId`. Interning is what keeps the table to one entry per distinct link
rather than one per run, and it makes the id a target rather than an
occurrence: the cells of two runs the child meant as one link compare equal,
because they carry the same id.

The id is a `NonZeroU32`, so the id space is a little over four billion values.
A terminal that hands out one id per distinct link is not expected to reach it,
and rather than stop, the counter **wraps**: an id that would exceed the space
starts again from the low end. A wrap could, in principle, hand out an id a live
cell already refers to, but that takes more than four billion distinct links in
one terminal's life, which no session reaches; the wrap is named here so the
bound is not left implied by the integer width.

### The link table

`TerminalState` keeps the interned link for each id, with a count of how many
live cells refer to it:

```rust
// id -> the link and how many live cells refer to it
links: HashMap<HyperlinkId, LinkEntry>,

struct LinkEntry {
    link: Hyperlink,
    // number of live cells referring to the link
    refs: usize,
}
```

A link is interned into the table, with `refs: 0`, when the first `OSC 8` that
names it arrives, and the pen is set to its id. The count then rises as cells
are painted with the pen's hyperlink, falls as those cells are overwritten,
erased, or discarded, and the entry is pruned when it reaches zero. The table
holds one entry per distinct link, and pruning keeps it to the links some cell
still needs, so it does not grow with the child's output the way a URI table
with no prune would.

Retrieving a URI is a lookup:

```rust
pub fn hyperlink(&self, id: HyperlinkId) -> Option<&Hyperlink>;
```

It returns `None` for an id whose entry has been pruned, which is what a caller
sees when it asks about a cell whose link is gone. There is no event for the
prune: a caller holds no id it did not get from a live cell, and a cell whose
link is pruned is a cell that no longer exists, so the `None` is the whole
story.

A cell moving from the grid into the history does not change the count: the
cells are copied into a `ScrollbackLine` and the ones in the grid are gone, so
the number of live cells referring to the link is the same before and after.
The count changes only when a referring cell is created or destroyed:

- **+1** when a cell is painted with the pen's hyperlink set, or copied
  (scrolled, reflowed, moved to the alternate screen) into a position that did
  not already hold a cell referring to that link;
- **-1** when a cell that referred to a link is overwritten by a cell that does
  not, is erased (ED / EL / ECH), or is dropped from the history by
  [`trim_scrollback()`](crate::TerminalState::trim_scrollback);
- **pruned** when the count reaches zero.

RIS (`ESC c`) does **not** reset the table. Every other piece of child state is
restored by RIS, but the link table is not child-visible state the crate
restores - it is a cache keyed by an id a caller may still hold. Resetting it
would make an id a caller kept across a reset resolve as if it had never
referred to a link, which is worse than a stale entry: a stale entry is pruned
as soon as the cells that referred to it are gone (which RIS does clear), so the
table empties itself without the reset having to force it. Keeping the table
across RIS is what lets an id stay meaningful for the life of the terminal.

The count is the load-bearing part. `Cell` is `Copy`, so there is no destructor
to hook: every path that writes or clears a cell has to adjust the count by
hand, and the paths are many (the screen's set, scroll, reflow, the edit and
erase sequences, the alternate-screen switch, RIS). A path that forgets to
decrement leaks the entry (the table keeps a link no cell needs); a path that
decrements twice can prune an entry a cell still refers to, which makes a live
cell's id resolve to `None`. The increments and decrements are therefore the
load-bearing part of this change, and the tests must exercise each path.

The history holds a copy, so a link referred to only by a history line stays in
the table until that line is trimmed, which is the correct lifetime but means
"the grid no longer shows it" is not "it is pruned". Because the caller cannot
see a cell overwritten in place, nor a history line pushed out, nor a reset
blanking the grid, it cannot maintain this count itself; the crate sees every
write by construction, so the count is the crate's to keep.

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
  it does not touch the table; a caller that wants to know whether a link is
  current reads [`style()`](crate::TerminalState::style).
- A **non-empty URI** is interned: if the crate has already seen the same
  `(uri, id)` pair, it reuses that id; otherwise it takes the next id and adds
  an entry to the table with the URI as framed and `refs: 0`. Either way the
  pen's `hyperlink` is set to the id.

The id that the close drops from the pen is not dropped from the table: the
cells painted while it was current still refer to it, and the entry is pruned
only when the last such cell is gone.

The `id` parameter is the only parameter the crate reads; it is the one xterm
and the common clients agree on, and it participates in interning (two runs with
the same `(uri, id)` are one link). Any other `key=value` pair is ignored (not
dropped as a sequence - the sequence is handled, the unknown parameter is not
modelled). The raw `params` string is not kept and not forwarded: the keys that
appear beside `id` are vendor-specific, so a caller that needs them is a caller
that knows the number. Forwarding the raw string is a later, additive change
(see Future possibilities), because `Hyperlink` is the crate's own type and
adding a field breaks no caller.

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

The prune is independent of this flag and invisible to the caller: it raises no
event. A feed that opens a link and paints no cell still reports `ScreenUpdated`
because the pen moved; a feed that overwrites the last cell of a link reports
`ScreenUpdated` because a cell changed, and the table entry is pruned as part of
that write. The flag says "repaint", and the table is read through
[`hyperlink()`](crate::TerminalState::hyperlink) when the caller resolves an id,
not through an event.

### Effect on RIS

RIS (`reset_child_state`) restores the terminal, so it clears the pen
(`self.pen = Style::default()`, which now also clears the hyperlink) and clears
the grid and the retained history. It does **not** clear the link table. RIS
clears every cell that referred to a link, so those links drop to zero
references and are pruned as part of the clear; but an id a caller kept across a
reset still resolves if a cell still needs it. RIS does not reset the id counter
either: ids are handed out and not reused, so an id a caller kept can never be
mistaken for the id of a link added after it.

Both halves matter for the same reason: RIS restores *child-visible* state, and
the link table is not that. It is a cache the crate keeps to answer
[`hyperlink()`](crate::TerminalState::hyperlink), keyed by an id a caller may
still hold; wiping it on reset would not restore anything the child sees, it
would only make a caller's held id stop resolving while the cell that gave it
the id is gone anyway. The table prunes itself as the reset's cleared cells are
processed, so leaving it alone and letting the count fall is both correct and
simpler than forcing a wipe.

### The event channel

No event is added. The area the crate reports through is unchanged: merged state
flags (`ScreenUpdated`, `ScrollbackLineAppended`, `TitleUpdated`,
`TerminalReset`) and the unmerged request queue (`RequestReceived`). A hyperlink
is read from the grid and resolved through
[`hyperlink()`](crate::TerminalState::hyperlink), the same way a cell's color is
read from the cell.

Earlier drafts of this RFC carried the URI to the caller as an
`HyperlinkAdded` event and reported the last reference going away as
`HyperlinkDropped`. Both are dropped here. The crate keeping the table makes the
add event unnecessary (the URI is available for the life of the link), and the
prune makes the drop event unnecessary (a caller holds no id it did not get from
a live cell). Removing both keeps the event channel as it is, at the cost of the
crate owning the URI table - which it can, because it can prune it.

### What is still not modelled

The sequence's `params` list can carry more than `id` (a host may add its own
keys), and the crate reads only `id`. A URI containing `;` or `:` is split by
the frame's rules before the crate sees it, which is a property of OSC framing,
not a choice here. The child's `id` parameter takes part in interning (two runs
with the same `(uri, id)` are one link) and is kept on the `Hyperlink` as
written; `hyperlink()` gives it back to a caller that wants the grouping. The
parameter is not consulted beyond the interning key. Neither changes the shape
of this RFC.

## Drawbacks

- **`Style` gains a field, which is a breaking change to a public struct.**
  Every caller that constructs `Style { .. }` literally (rather than via
  `Style::default()` and field assignment) stops compiling, and `Cell::EMPTY` /
  `Cell::CONTINUATION` literals in the crate gain a field. This is the cost of
  putting the attribute on the pen, and it is real for callers that build
  styles by hand.
- **The crate keeps the URI table and must keep its count exact.** `Cell` is
  `Copy`, so the count is maintained by hand on every path that writes or
  clears a cell, and a path that misses an increment or decrement either leaks
  an entry (the table keeps a URI no cell needs) or prunes one early (a live
  cell's id stops resolving). This is the largest correctness burden in the
  change. A link with no cells yet - interned, pen set, no paint - sits in the
  table at zero until a cell refers to it or a reset clears it; a link whose
  only cell is overwritten by an equal value that carries the same id must not
  be counted as a prune.
- **The caller does a lookup per cell to get a URI.** Rendering a linked run
  means one id-to-URI lookup per cell against the crate's table (or spotting
  the run boundaries first), where a cell that carried the URI would need none.
  The trade is the cell size: a `String` per cell would make `Cell` non-`Copy`
  and every cell carry a URI, and removing the id indirection would make `Style`
  larger and every cell wider. The indirection is the cheaper mistake.
- **A URI stays in the table until the last referring cell is gone, which may
  be long after the child closed the link.** A link whose cells are never
  overwritten, erased, or trimmed keeps its URI for the life of the terminal.
  That is the correct lifetime for a cell-keyed table, but it is not the same
  as "the child closed the link"; a caller that wants the close reads
  [`style()`](crate::TerminalState::style), and the table's growth is bounded by
  the number of links some live cell needs, not by the child's output.
- **OSC 8 has no read form.** Unlike OSC 4 and the color sequences, there is
  no `?` query for a hyperlink, so this RFC does not exercise the policy's
  "answer a query from state" path. That path went unbuilt: the color
  sequences were settled by handing their queries to the caller instead, so no
  sequence in the crate answers a `?` from state.

## Rationale and alternatives

- **Put the URI (a `String`) directly in `Style`.** Rejected: `Style` would
  lose `Copy`, `Cell` would lose `Copy`, `Cell::EMPTY` could no longer be a
  `const`, and every cell in a linked run would carry its own copy of the URI.
  The id indirection keeps all four things and stores the URI once per link in
  the crate's table.
- **Keep the URI in the crate, looked up by id, but never drop an entry.**
  Rejected: this is the shape that made a link table grow without bound. The
  crate is the only party that sees every cell write, so it is the only party
  that can tell when a URI is unused - and once it can tell, it can prune the
  entry. Keeping the table **and** the count is what this RFC does; keeping
  only the table is the unbounded shape that was rejected.
- **Hand the URI to the caller instead of keeping it, with add / drop events.**
  Rejected: this is the earlier shape of this RFC. It reached the same
  lifetime answer but by moving the URI to the caller and reporting the two
  ends of its life as events. Since the crate has to keep a per-link count
  anyway to know when it can prune, keeping the URI beside that count is
  strictly less machinery than emitting two events and holding the count to
  drive them. The caller's cost is a lookup per cell instead of a table of its
  own.
- **Make the whole hyperlink a consumed-once request instead of a cell
  attribute.** Rejected by the policy and on its own merits: a hyperlink is an
  attribute of cells that outlive the sequence, not a message the caller takes
  once. The attribute stays on the cells and the URI stays in the crate's
  table. This is the state row of the policy's table.
- **Carry the link on `ChildRequest`.** Rejected: a
  [`ChildRequest`](crate::ChildRequest) is defined as something the caller is
  *expected to do*; a link appearing is something the caller may or may not act
  on, and a link being pruned is not an action at all - the cells that carried
  it are simply gone. Neither belongs on the request channel.
- **Report a `Closed` (or `Removed`) event when the pen's link is cleared.**
  Rejected: the pen closing is not a change to any cell, and a caller that
  wants the current link reads [`style()`](crate::TerminalState::style). The
  cells painted before the close keep their id, so the close is not a lifetime
  boundary for the link.
- **Report a `Dropped` event when an entry is pruned.** Rejected: the caller
  holds no id it did not get from a live cell, so an id it holds is either
  still resolvable or belongs to a cell that no longer exists. A prune event
  would say "forget this URI you may never have had", which a caller that reads
  URIs from the table does not need.
- **Do not intern; give each run a fresh id.** Rejected: interning is what
  keeps the table to one entry per distinct link instead of one per run and
  makes two runs the child meant as one link compare equal as cells. It costs
  a URI comparison on each open, which the crate can afford because it owns the
  URIs anyway to prune them.
- **Use a per-link `Arc`/`Rc` so the count is maintained by the type system.**
  Rejected: a cell would have to hold a reference-counted handle instead of a
  `Copy` id, which takes `Cell` out of `Copy`, makes `Cell::EMPTY` no longer a
  `const`, and puts a refcount increment on every cell write and copy -
  including the bulk `vec![Cell::EMPTY; n]` fills the screen does on every
  resize and clear. The manual count keeps `Cell` `Copy` and small; the cost is
  the bookkeeping the next section is about.
- **Store the child's `id` parameter as the cell's reference, without an
  opaque crate id.** Rejected: the child's `id` is a string, so it reintroduces
  the `String`-in-`Style` problem.
- **Use a `char` as the cell's link reference instead of a `HyperlinkId`.**
  Rejected: a `char` is a character, and a link is not one, so the value would
  be a `u32` wearing a character's name - and a worse one, since `Option<char>`
  has no niche and would make `Style` larger than `Option<NonZeroU32>` does,
  while the surrogate range wastes codepoints that could otherwise be ids.
- **Prune by sweeping the grid and history for unreferenced ids, instead of
  counting.** Rejected: a sweep is `O(cells)` and would have to run often enough
  to be timely, while the count is maintained at the writes that already cost
  the most. The count can drift if a path forgets; a debug-only sweep that
  cross-checks the count is a cheaper safety net than a production sweep (see
  Unresolved questions).
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

- **How do we keep the reference count exact?** Open. The count is maintained
  by hand on every path that writes or clears a cell, and the paths are many.
  The plan is exhaustive tests per path plus a debug-only sweep that recomputes
  the count from the grid and history and asserts it matches - but whether that
  sweep is worth its complexity, and whether the count should instead be
  derived lazily, is not settled.
- **Does a cell overwritten by an equal value that carries the same id count
  as a prune and re-add, or not at all?** Open, but it must not prune: the
  value still refers to the link. The count is a set of references, not a
  sequence of writes, so equal-to-equal is a no-op. How that is implemented
  (compare before decrementing, or decrement the old and increment the new) is
  a detail for the implementation.
- **Where does the reference count live, and who maintains it?** Open. The
  grid is held by `Screen` (one per screen, plus the history), while the table
  has to be shared across the whole terminal - both screens and the history -
  because an id is meaningful wherever a referring cell is. So the table
  cannot live inside `Screen`. Two shapes:
  - **(X) Pass the table down into `Screen`.** Every method that writes or
    clears a cell takes `&mut LinkTable` and adjusts the count itself, through
    a small set of store/clear helpers so no assignment touches the table
    directly. `Screen` stays unaware of what a link is beyond the helpers, but
    every signature grows a parameter, and the rotate/fill/reflow paths have
    to know which cells are leaving and which are being overwritten.
  - **(Y) Keep the table on `TerminalState` and adjust it around screen
    operations.** The screen operations that move or fill cells return the
    cells they displaced or overwrote (or enough to reconstruct them), and
    `TerminalState` adjusts the count once per operation. Fewer signatures
    change, but the screen has to report back cells it would otherwise drop,
    and the rotate/fill paths still have to say which cells left.
  Neither is obviously cheaper: (X) localises the bookkeeping next to the
  assignments but widens every call; (Y) keeps `Screen` narrow but moves the
  bookkeeping away from the code that knows which cells moved. Not settled.

## Future possibilities

- A host that wants clickable links reads the pen field and resolves the id
  through [`hyperlink()`](crate::TerminalState::hyperlink), and a host that
  wants to underline them uses the same field - the crate does not decide the
  presentation, it exposes the association.
- A raw `params` string on `Hyperlink`, if a caller ever needs the keys beside
  `id`, is additive: `Hyperlink` is the crate's own type, so a new field breaks
  no caller the way a new `Style` field does.
- The color sequences (OSC 4 and 10-12) are the sibling case, and the crate
  has since settled the opposite way: rather than keep the state and resolve
  cells against it, termnix holds no color state and hands each set and query
  to the caller as a request. A hyperlink is different only because a cell has
  to remember which run it belongs to, so the crate has to keep a table and a
  count to answer *which run* - the color a cell carries is already a value on
  the cell and needs no table.
- A future hyperlink extension (a title, a hover text) is a field on the
  `Hyperlink` in the table; it does not change `Style` or `Cell`.
- A bound on the number of live links, if the table ever proves too large, would
  be a policy on top of the count (prune the least-recently-used link early)
  rather than a different data structure.
