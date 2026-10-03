# RFC: Carry OSC 8 hyperlinks as a cell attribute

- Status: draft

## Summary

Model OSC 8 hyperlinks by giving the pen a hyperlink attribute, so every cell
painted after an `OSC 8 ; params ; URI ST` remembers the link it belongs to.
The sequence changes what the grid means without drawing a cell itself - the
same shape as SGR, which sets the pen a later paint reads - so its home is the
pen, not the event channel. The crate keeps a table of the links it has seen and
cells refer to a link by a small id, which keeps `Style` and `Cell` `Copy` and
leaves the hyperlink out of the per-cell value a caller clones.

## Motivation

A host application that embeds a child session renders the child's output by
reading cells back and painting them. OSC 8 is how a child marks a run of cells
as a hyperlink: the sequence carries a URI and sets it as the current pen's
hyperlink, and a later sequence with an empty URI turns hyperlinking off. A
child that prints a clickable link - a compiler diagnostic pointing at a file,
a `git log` pointing at a commit, a tool printing an issue URL - emits one
`OSC 8` before the run and one after.

The crate drops those sequences today. That is worse than dropping a message:
a hyperlink is not a thing the child asks the caller to *do*, it is a thing the
child attaches to the grid. The crate paints the run as ordinary cells with no
record of the link, so a host that wants to make the text clickable has no way
to know which cells are linked, and a host that wants to show the URL on hover
has no way to get it. The only recourse is to re-tokenize the PTY stream beside
the crate and to track the pen's hyperlink state in parallel - the exact
bookkeeping the crate already does for the ordinary pen.

This is the case the OSC handling policy places on the *state* side and not the
event side: a hyperlink attribute changes what the grid means when a later
paint reads it, so a crate that handed it to a caller as bytes would paint
cells with a missing attribute and no way for the host to put it back. The
policy names OSC 8 as `state, once modelled`; this RFC is that modelling.

## Guide-level explanation

Before, a hyperlink run reached the caller as if it were plain text:

```rust
// The child wrote ESC ] 8 ; ; https://example.com ST click here ESC ] 8 ; ; ST
// Every cell in "click here" has `style.hyperlink` ... which does not exist.
```

After, the cells in the run point at the link, and the caller can look it up:

```rust
for row in term.rows() {
    for cell in row {
        if let Some(link) = term.hyperlink(cell.style.hyperlink) {
            // cell is inside the run for `link.uri`; make it clickable,
            // underline it, or show the URI on hover
        }
    }
}
```

The shift in thinking is that the pen has a hyperlink field the same way it has
a foreground colour. Setting `OSC 8` changes the pen; painting a cell copies the
pen into that cell. A cell does not carry the URI itself - it carries a small id
- so a cell stays a couple of words wide and the URI is stored once per run
rather than once per cell. A run of a thousand linked characters keeps one URI.

A host that does not show hyperlinks ignores the field. Nothing about the cell
layout, the text, or the style bits the crate already exposes changes; the field
is new data the caller may use or ignore.

## Reference-level explanation

### The pen field

`Style` gains one field:

```rust
pub struct Style {
    pub foreground: Color,
    pub background: Color,
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

### The id, and the table that gives it meaning

```rust
/// Identifies a hyperlink the terminal has seen (OSC 8).
///
/// Opaque. Pass it to [`TerminalState::hyperlink()`] to get the link's URI.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct HyperlinkId(NonZeroU32);

/// A hyperlink the terminal has seen.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Hyperlink {
    /// The URI the child attached to the run (`OSC 8 ; params ; URI`).
    pub uri: String,
    /// The link's `id` parameter, if the child gave one.
    ///
    /// The child uses `id` to say that two separate runs are one logical
    /// link, so a terminal that underlines links can avoid drawing a break
    /// between them.
    pub id: Option<String>,
}
```

`HyperlinkId` wraps a `NonZeroU32` so that `Option<HyperlinkId>` is the same
size as a `u32` (niche optimization), keeping `Style` small; `None` is the
common case and costs nothing. The id is assigned by the crate and is opaque to
the caller; it is not the child's `id` parameter, which is a string the child
controls and which two runs may share.

`TerminalState` holds `links: Vec<Hyperlink>` (or an intern table keyed by
`(uri, id)`), and the id is an index into it. Two `OSC 8` sequences with the
same `(uri, id)` resolve to the same id, so equal runs compare equal as cells;
two with the same URI but different `id` do not.

The id is what lets a caller tell *adjacent* runs apart. When a child closes
one link and opens another with no character between them, the cells of the
first run carry the first id and the cells of the second carry the second, so
the boundary is in the grid even though nothing was drawn there. The one case
that is *not* distinguished is two runs that share `(uri, id)`: they resolve to
one id by construction, because that is what the child's `id` asked for - a
caller that needs to see a boundary between two such runs has to get the child
to use distinct `id`s. This RFC does not second-guess the child's grouping.

### The getter

```rust
/// Returns the hyperlink a `HyperlinkId` refers to, if the terminal has seen
/// it.
///
pub fn hyperlink(&self, id: Option<HyperlinkId>) -> Option<&Hyperlink> {
    id.and_then(|id| self.links.get(id.index()))
}
```

`&self`, like every other non-event accessor: a link is a property of the state,
not a consumed-once message. The indirection is the point - the cell stays
cheap, and the caller that cares pays for the lookup.

### Parsing the sequence

The sequence is `OSC 8 ; params ; URI ST`, where `params` is a `:`-separated
list of `key=value` pairs and `URI` is the target. In `osc_dispatch`:

```rust
"8" => self.osc_hyperlink(params),
```

`osc_hyperlink` splits `params[1]` on `:` for the parameter pairs and reads
`params[2]` as the URI:

- An **empty URI** (or no `params[2]`) ends the current link: the pen's
  `hyperlink` becomes `None`, and the next cell paints with no link. This is how
  a child closes a run.
- A **non-empty URI** interns `(uri, id)` and sets the pen's `hyperlink` to the
  resulting id.

The `id` parameter is the only parameter the crate reads; it is the one xterm
and the common clients agree on. Any other `key=value` pair is ignored (not
dropped as a sequence - the sequence is handled, the unknown parameter is not
modelled).

The crate does **not** validate the URI, decode it, or split it into scheme and
path. It stores the bytes as the `String` the tokenizer produced, keeping the
same lossy decoding the title and clipboard paths use. A URI is displayed or
handed to the host, not interpreted by the crate.

### Effect on `revision()`

The pen is part of visible state, but only through the cells it paints. Setting
the pen's hyperlink does not by itself change a cell - it changes the *next*
cell painted. Once a cell is painted with the pen, that cell differs from what
it was, so the paint already marks the screen dirty by the mechanism that marks
any paint dirty. This RFC adds no new visible-change path: the hyperlink rides
the existing `primary_dirty` flag that a cell write sets. If the sequence sets a
link and no cell is painted before the run ends, nothing was drawn and
`revision()` correctly does not move.

The pen itself is not in `visible_scalars()` and is not compared across a feed,
which is unchanged: a pen change is observable only when a cell is painted with
it, and that paint is the change.

### Effect on `soft_reset` / RIS

RIS restores the terminal, so it clears the pen (`self.pen = Style::default()`,
which now also clears the hyperlink) and drops the link table. Clearing the
pen is already there; dropping the table is the only addition. A cell painted
before the reset keeps the id it was given, but after a reset the table is
gone, so `hyperlink()` returns `None` for it - the id is a reference into state
that no longer exists. That is the honest answer: the link table is part of the
terminal the reset restored, and the cells that survive are on the restored
screen. This is worth a doc note on `hyperlink()`.

### What is still not modelled

The sequence's `params` list can carry more than `id` (a host may add its own
keys), and the crate reads only `id`. A URI containing `;` or `:` is split by
the frame's rules before the crate sees it, which is a property of OSC framing,
not a choice here. Neither changes the shape of this RFC.

## Drawbacks

- **`Style` gains a field, which is a breaking change to a public struct.**
  Every caller that constructs `Style { .. }` literally (rather than via
  `Style::default()` and field assignment) stops compiling, and `Cell::EMPTY` /
  `Cell::CONTINUATION` literals in the crate gain a field. This is the cost of
  putting the attribute on the pen, and it is real for callers that build
  styles by hand.
- **A table that grows with the child's output.** Every distinct link the child
  emits adds an entry, and the table is never pruned while the terminal lives.
  A child that emits a distinct URI per line (a log with many links) grows the
  table without bound. A bound, or pruning links no surviving cell refers to,
  is not part of this proposal and is the most likely follow-on.
- **The caller does a lookup per cell to get a URI.** Rendering a linked run
  means one `hyperlink()` call per cell (or spotting the run boundaries first),
  where a cell that carried the URI would need none. The trade is the cell
  size: a `String` per cell would make `Cell` non-`Copy` and every cell carry a
  URI. The indirection is the cheaper mistake.
- **An id that outlives its table entry is representable.** After RIS, a cell
  can hold a `HyperlinkId` whose entry is gone, and `hyperlink()` returns `None`
  for it. This is correct but subtle; it needs the doc note above, and a caller
  that stashed a `HyperlinkId` across a reset must handle `None`.
- **The boundary of two runs that share `(uri, id)` is not recoverable from the
  id.** Cells of two adjacent runs of one logical link carry the same id, so a
  caller cannot tell from the grid alone how many `OSC 8` sequences the child
  sent or where each run began; it sees one linked region. This is the child's
  grouping being honoured rather than a loss, but a caller that wants to mark
  the runs individually cannot, and the only fix is on the child's side
  (distinct `id`s).
- **OSC 8 has no read form.** Unlike OSC 4 and the colour sequences, there is
  no `?` query for a hyperlink, so this RFC does not exercise the policy's
  "answer a query from state" path. That path belongs to the colour RFC.

## Rationale and alternatives

- **Put the URI (a `String`) directly in `Style`.** Rejected: `Style` would
  lose `Copy`, `Cell` would lose `Copy`, `Cell::EMPTY` could no longer be a
  `const`, and every cell in a linked run would carry its own copy of the URI.
  The id indirection keeps all four things and stores the URI once per run.
- **Make the hyperlink a consumed-once event instead of state.** Rejected by the
  policy and on its own merits: a hyperlink is an attribute of cells that
  outlive the sequence, not a message the caller takes once. An event would let
  a caller that drains it after the cells are painted miss the association
  between the link and the cells, and would leave the crate painting cells with
  no attribute. This is the state row of the policy's table.
- **Emit `OSC 8` on the passthrough channel and let the caller track the pen.**
  Rejected: the caller would have to reproduce the pen's hyperlink state and
  know which cells were painted while it was set, which is the grid bookkeeping
  the crate exists to do. The crate already owns the pen; the hyperlink is one
  more field on it.
- **Store the child's `id` parameter as the cell's reference, without
  interning.** Rejected: the child's `id` is a string, so it reintroduces the
  `String`-in-`Style` problem, and two runs with the same URI but no `id` (the
  common case, `OSC 8 ; ; URI`) would not compare equal unless the crate also
  compares URIs. Interning on `(uri, id)` gives a small id and the equality the
  caller wants.
- **Use a `char` as the cell's link reference instead of a `HyperlinkId`.**
  Rejected: a `char` is a character, and a link is not one, so the value would
  be a `u32` wearing a character's name - and a worse one, since `Option<char>`
  has no niche and would make `Style` larger than `Option<NonZeroU32>` does,
  while the surrogate range wastes codepoints that could otherwise be ids. It
  also would not change the adjacent-run question either way: separate links
  get separate values per cell whether the value is a `char` or a `u32`, and
  two runs sharing `(uri, id)` get the same value by the child's request.
- **Ignore unknown `params` by dropping the sequence.** Rejected: the sequence
  is understood (it is a hyperlink); only an unmodelled parameter is ignored,
  which is the same relationship the crate has to SGR values it does not
  recognize. Dropping the whole sequence would lose the URI.
- **Do nothing.** Hyperlinks stay invisible to every caller; a host cannot make
  linked text clickable or show its URI without a second tokenizer. See
  Motivation.

## Unresolved questions

- **Is the link table bounded, and if so how?** The table grows with distinct
  links and is never pruned. A cap (reject new links past N), a prune (drop
  links no live cell refers to), or leaving it unbounded are all open. This is
  the same "unbounded table the child drives" question the event queue has, and
  it is the strongest candidate for its own follow-on.
- **Should `hyperlink()` take `Option<HyperlinkId>` or two methods?** Taking
  `Option` lets a caller pass `cell.style.hyperlink` straight through, which is
  the common call. A separate `hyperlink(id)` that panics on a missing id, or a
  `Option`-of-`Option`, are alternatives; the `Option` argument is chosen for
  ergonomics and to make the "no link" case explicit.
- **Is `Hyperlink` the right public name, and should it expose `params`?** Its
  `uri` and `id` are the fields the crate models. Whether to keep the raw
  `params` string for a caller that wants the rest is open; today the crate
  reads `id` and drops the rest, which loses information a host might want.
- **Does a link need to survive the alternate-screen switch?** The table is on
  `TerminalState`, not on a `Screen`, so it survives the switch, and a cell on
  one screen can refer to a id shared with the other. Whether that is right (a
  hyperlink set on the primary screen and one on the alternate are unrelated
  runs) is open; the simplest answer is that the table is terminal-wide, as it
  is here.

## Future possibilities

- A host that wants clickable links reads the pen field and the table, and a
  host that wants to underline them uses the same field - the crate does not
  decide the presentation, it exposes the association.
- A prune or bound for the link table, if the unbounded version proves a
  problem, is a change to `TerminalState` only and does not touch `Style` or
  `Cell`.
- A later proposal for the colour sequences (OSC 4 and 10-12) is the same
  shape - state the pen or the palette holds, read back through `&self`
  accessors - and is the case that answers a `?` query from state, which this
  one does not have.
- A future hyperlink extension (a title, a hover text) adds a field to
  `Hyperlink`, which is a public struct the caller already looks up by id; it
  does not change `Style` or `Cell`.
