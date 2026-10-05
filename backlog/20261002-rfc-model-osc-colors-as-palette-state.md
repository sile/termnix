# RFC: Model OSC colors as palette state

- Status: draft

## Summary

Model the colour-setting OSC numbers - OSC 4 (a palette entry) and OSC 10, 11,
12 (the default foreground, background, and cursor colour) - as terminal state
the crate holds and answers queries from. A colour sequence does not draw a cell
and does not ask the caller to do anything: it redefines what an existing cell
*means* when a later paint resolves it, which is the same shape as SGR. Its home
is therefore the state row of the OSC handling policy, read back through
`&self` accessors, and it is the first `?` query the crate can answer from its
own state rather than delegate. The cell colour type `Color` does not change:
a palette entry is not a cell colour, and conflating the two is the mistake this
RFC exists to avoid.

## Motivation

A host application renders a child's output by reading cells back and painting
them. A cell carries a `Color`, which is either a direct `Rgb` value or an
`Indexed(u8)` palette number. When the cell is `Indexed`, the host has to answer
"what colour is index 4?" to paint it, and the crate already offers an answer:
`Color::to_rgb()` resolves an index through the xterm 256-colour palette.

That answer is a *default*, and the rustdoc says so - it warns that host
terminals may override the first sixteen entries and that an application needing
exact colours should consult its own palette. A child can override them for
real, from inside the session, with `OSC 4`; and the related sequences `OSC 10`,
`11`, `12` redefine the default foreground, background, and cursor colour. The
crate drops all four today, so a child that recolours its palette is
invisible: the host keeps resolving `Indexed(4)` through the default table and
paints the wrong colour, and the two ends of the same screen disagree about what
the screen contains.

Nothing here lets a caller *act* on a colour, which is why it is state and not
an event. A colour sequence is not addressed to the caller; it is addressed to
the terminal, to change what a later paint means. The caller does not have to
cooperate for the change to take effect, the way it must for a clipboard
request. The crate is the thing that must remember it, because the crate is the
thing that later hands out cells whose meaning depends on it.

This is also the case the OSC handling policy names as the one that answers a
`?` query from state. `OSC 4 ; 1 ; ?` and `OSC 10 ; ?` ask the terminal for a
value it now holds, and unlike a clipboard read there is a true answer that does
not require the caller: the value the child last set, or the built-in default if
it never did. Routing it to the caller, as a read of something the caller owns,
would be wrong - there is nothing of the caller's in the answer.

## Guide-level explanation

Before, a child's `OSC 4 ; 1 ; rgb:ff/00/00` changed nothing the host could see:

```rust
// The child wrote ESC ] 4 ; 1 ; rgb:ff/00/00 ST
let cell = term.cell(at).unwrap();
// cell.style.foreground is Indexed(1), and stays Indexed(1)...
// ...and Color::Indexed(1).to_rgb() still returns the built-in default,
// not the red the child asked for.
```

After, the crate holds the override and resolves it through the same table the
cell colour type already resolves through:

```rust
let cell = term.cell(at).unwrap();
// The child set index 1 to red; ask the terminal, not the default table.
let rgb = term.palette_color(1);
// term.default_foreground() returns the OSC 10 value, or None if the child
// never set one (the host's default is the host's, not the crate's).
```

The shift in thinking is that the palette and the colour slots are terminal
*state* that the child mutates, exactly like the title before it moved or the
pen's hyperlink attribute. A cell does not store the palette; it stores an
index, and the index is resolved against the state at paint time. Because the
state
lives on the terminal, `?` has an answer that is the terminal's, not the
caller's.

A host that does not care about palette overrides ignores every accessor and
keeps using `Color::to_rgb()`, and sees default colours - the same thing it saw
before, which is now explicitly "the default", not "the child's choice".

## Reference-level explanation

### `Color` does not change

The cell colour stays:

```rust
pub enum Color {
    Default,
    Indexed(u8),
    Rgb(u8, u8, u8),
}
```

A palette entry is not a cell colour. `Color::Rgb` says "this cell is that
red"; a palette override says "index 4 means that red". They are different
types of fact, and giving the palette its own type keeps the cell colour enum
as stable as it was before OSC 8 added one field to `Style`. The palette table
and the colour slots are stored on `TerminalState` and are not exposed as a
`pub` struct: the only public entry points are the accessors below.

### The state

`TerminalState` gains two pieces of state, both internal:

```rust
// 256 entries, each None until the child overrides it.
palette: [Option<(u8, u8, u8)>; 256],
// The three OSC 10-12 slots, None until the child sets one.
default_colors: DefaultColors, // fg/bg/cursor, each Option<Rgb>
```

`Rgb` here is a `(u8, u8, u8)` triple; the stored value is exactly what the
child sent, not resolved through the palette. An override is kept as sent, so it
can be returned verbatim to a later `?`.

### The accessors

```rust
/// Returns the colour the terminal resolves `index` to.
///
/// Returns the child's override when it set one, otherwise the built-in
/// xterm 256-colour default (`Color::Indexed(index).to_rgb()`). Total:
/// every `u8` resolves to a colour.
pub fn palette_color(&self, index: u8) -> (u8, u8, u8);

/// Returns the default foreground colour the child set, if it set one.
pub fn default_foreground(&self) -> Option<(u8, u8, u8)>;

/// Returns the default background colour the child set, if it set one.
pub fn default_background(&self) -> Option<(u8, u8, u8)>;

/// Returns the cursor colour the child set, if it set one.
pub fn default_cursor(&self) -> Option<(u8, u8, u8)>;
```

`palette_color` returns a bare tuple, not an `Option`, because it is total:
the argument is a `u8` and the built-in table gives every index a value. The
`Option` on `Color::to_rgb` is not a model to copy here - it is needed there
because `Color::Default` is a real "no value" case, which has no analogue in a
palette index. Wrapping a total result in `Option` would make every caller
unwrap something that cannot be `None`.

`palette_color` resolves an override first, then falls back to the built-in
table. It does not consult the default colours: index 0-15 are a *palette*, and
`OSC 10` is a separate slot even though a host may render `Default` using it.
Mixing them would make `palette_color(0)` change meaning when the child sets
`OSC 10`, which is not what the sequences say.

The three `default_*` methods return the child's value for that slot, or `None`
if it never set one. They do not fall back to a built-in default, because for
`OSC 10`-`12` the built-in value is the host's, not the terminal's: the crate
does not know the host's default colour and should not invent one. They are
three methods rather than one `default_color(slot)` because a slot is not a
value the crate has anywhere else - it exists only to pick a method - and
introducing a public enum for that would be a type wrapping a method choice.
The three share one private helper, so the implementation is not duplicated.
If a caller ever needs to choose a slot dynamically, a `default_color(slot)`
can be added beside these later without removing them.

### Parsing the sequences

The sequences share a form: `OSC <n> ; <key> ; <value> ST`, where `key` is a
palette index (for OSC 4) or omitted / a slot selector (for 10-12), and `value`
is `rgb:RR/GG/BB`, optionally with more digits per channel, or the literal `?`.
In `osc_dispatch`:

```rust
"4" => self.osc_palette(params),
"10" | "11" | "12" => self.osc_default_color(params),
```

Each handler:

- Reads the colour spec with a `parse_osc_rgb(&[u8]) -> Option<(u8, u8, u8)>`
  helper that accepts the `rgb:` form and converts each channel to `u8`. A
  malformed spec stores nothing (the same silent-ignore the crate uses for an
  undecodable OSC 52 payload).
- If the value is `?`, builds a reply from the *current* state and appends it to
  the reply buffer (see the query section).
- Otherwise stores the value: `palette[key] = Some(rgb)` for OSC 4,
  `default_colors[slot] = Some(rgb)` for 10-12. A child can query one key,
  set another, or send several `key;value` pairs, so the OSC 4 handler loops
  over the `key;value` pairs in `params[1..]` rather than reading a fixed
  `params[1]` / `params[2]`.

`?` and a value are mutually exclusive per pair, so "set" and "query" do not
fight; a pair is one or the other.

### Answering `?`

This is the policy's "answer a query from state" path. The reply reuses the
same number, as OSC framing requires: `OSC 4 ; 1 ; rgb:...` is answered with
`OSC 4 ; 1 ; rgb:...`, and `OSC 10 ; ?` with `OSC 10 ; rgb:...`. The bytes are
appended to the reply buffer
([`pending_reply_bytes()`](../src/terminal.rs),
[`advance_reply_bytes()`](../src/terminal.rs)) - the existing channel for
"queries termnix itself knows the answer to", alongside DSR and CPR - not to the
event channel, because the answer is not an instruction to the caller and the
caller has nothing to do with producing it.

- `palette` query: answer with the override if set, else the built-in
  `Color::Indexed(index).to_rgb()` value. A palette query is always answerable,
  because the built-in table is a valid answer to "what does index N resolve
  to?".
- `default_color` query: answer with the child's value if set. If it is not,
  the crate has no true answer (it does not know the host default), so it
  answers nothing - the same "a probe gets no reply rather than a wrong one"
  principle the DA2/DA3 handling follows.

The reply cannot precede a caller's write to the PTY because the reply buffer is
drained by the caller through the existing API; this RFC adds nothing to the
session layer.

### Effect on visible-change detection

The palette and the colour slots are not cells. Setting one changes what a later
paint of an `Indexed` cell *means*, but it does not paint a cell, and no cell
value changes because of it. Two consequences, both matching the OSC 8
reasoning:

- Setting a colour does not by itself mark the screen dirty. No
  `Event::ScreenUpdated` is reported for the sequence alone.
- An existing `Indexed` cell that is later repainted (because the child prints
  something that overwrites it, or the screen is resized) resolves through the
  new palette, and that paint sets the existing `primary_dirty` flag as any
  paint does. No new visible-change path is added.

This is deliberate, and it is the point where a caller *can* disagree: a host
that repaints its own copy of the screen from the accessors will want to
repaint when a colour changes, and no `Event::ScreenUpdated` tells it to. A
future notification for colour changes (see Unresolved) is where that would be
addressed; this RFC does not add one.

### Effect on RIS

RIS (`reset_child_state`) restores the terminal, so it clears both pieces of
state: every
`palette` entry returns to `None` and every `default_colors` slot to its
`None`. The built-in default table is a constant and is not cleared. This
matches the title (cleared) and the link table (dropped): RIS returns the
theme the terminal had before the child touched it.

### What is still not modelled

- The `rgb:` spec is accepted in its common form; a spec using a different
  colour-space prefix, or a fractional value, stores nothing. The set of
  accepted forms is small and can grow without changing the shape of this RFC.
- `OSC 4` and 10-12 can also carry a `?` alongside other `key;value` pairs in
  one sequence. The handler processes each pair independently, so a mixed
  sequence sets some keys and answers a query for others; the reply includes
  only the queried pairs.
- The palette is *not* re-resolved for already-painted cells. An `Indexed` cell
  that is never repainted keeps the meaning the palette has when someone asks,
  which is the only consistent rule given that the cell stores an index and not
  a colour.

## Drawbacks

- **The crate now holds colour state it did not before.** A palette table and
  three slots are new persistent state that a child can grow/overwrite. The
  table is fixed-size (256 entries, three slots), so it cannot grow unbounded
the way the hyperlink table can, but it is still state the crate must clear on
  RIS and thread through resize/reset.
- **Two ways to answer "what colour is index N" now exist, and they can
  disagree on purpose.** `Color::to_rgb()` returns the built-in default;
  `palette_color(N)` returns the override. A caller that uses the first where
  it should use the second paints the default. The crate cannot prevent this by
  making `to_rgb` consult the terminal (that would couple the value type to
  mutable state), so the distinction has to be learned from the docs.
- **`palette_color(N)` and `Color::to_rgb()` return the same thing for an
  un-overridden index, from two call sites.** A caller that resolves every
  cell through `palette_color` pays a terminal lookup for indices the child
  never touched, where `to_rgb()` on the cell's colour would have sufficed.
  The crate cannot fold the two without coupling the value type to terminal
  state, so the duplication of *answers* (not of tables) is the price.
- **The accessors are `&self` and read state a caller may not repaint on.** A
  host that caches its own rendering sees no `Event::ScreenUpdated` when a
  colour changed, so it must poll or repaint unconditionally. This is the
  deliberate cost of treating a non-drawing change as a non-drawing change;
  the alternative (mark the screen dirty on every colour set) over-triggers.
- **A `?` reply can be generated even though nothing on screen changed.** A
  child that queries a palette entry gets an answer while the grid is
  untouched; that is correct, but it means "a reply was produced" and "the
  screen changed" are independent, which a caller relying on
  `Event::ScreenUpdated` alone must understand (it already must, for DSR/CPR).

## Rationale and alternatives

- **Put the palette on `Color`.** Rejected: `Color` is a `Copy` value type
  that appears in cells and in `Style`; it cannot hold a 256-entry mutable
  table, and `Color::Indexed(4)` must keep meaning "index 4" and not "whatever
  index 4 is now". The palette belongs to the terminal, the index belongs to
  the cell.
- **Resolve overrides inside `Color::to_rgb()`.** Rejected: it would make a
  value method read mutable terminal state, so `to_rgb()` would stop being a
  pure function of `self` and would need a `&TerminalState`. The crate keeps
  the default-table resolution pure and adds a separate accessor for
  overrides.
- **Make colour sequences events the caller acts on.** Rejected: the caller
  has nothing to do for the change to take effect. A colour is terminal state
  a later paint reads, the state row of the policy, not a message.
- **Send colour changes over the passthrough channel.** Rejected: the cell
  would paint with the default colour because the crate did not apply the
  override, which is exactly the bug this RFC fixes. The crate owns the paint,
  so the crate must own the palette.
- **Route `?` to passthrough instead of answering.** Rejected for the palette
  slots: the crate *has* the answer (override or built-in), so delegating would
  ask the caller to reproduce state the crate holds. For `OSC 10`-`12` with no
  value set, the crate has no answer, so those do not reply - which is the
  policy's "route it, do not invent it" applied inside one RFC rather than
  between two.
- **Treat `OSC 10`-`12` as palette entries 0/7/cursor.** Rejected: the
  sequences are defined as slots, not indexes; a host may set palette index 0
  and the default background independently, and the crate must not conflate
  them, or `palette_color(0)` would change meaning under `OSC 11`.
- **Do nothing.** The cell colour enum stays simple, but a child's palette
  override is invisible and the host paints the wrong colour with no way to
  find out. See Motivation.

## Unresolved questions

- **Should a colour change inform the caller at all?** A host that caches a
  rendering has no signal that a palette entry changed and must repaint
  unconditionally or poll. Whether colour sets should also emit an event (the
  policy allows a state value to carry a separate notification when the change
  is something a caller must act on) is open; no other state-row sequence
  currently does, and adding one would make this RFC the first.
- **Do the three `default_*` methods ever need to become
  `default_color(slot)`?** They are three methods because a slot is not a value
  the crate otherwise names. If a caller needs to choose a slot dynamically, a
  fourth method taking a slot can be added beside them without removing the
  three; the reverse (starting with the slot enum) would leave the enum
  unnecessary until that caller appears.
- **Should the stored override keep the child's exact form or a resolved
  `Rgb`?** This RFC stores `(u8, u8, u8)` because `?` answers in the crate's
  own form (`rgb:RR/GG/BB`), not the child's. A host that needs the exact bytes
  the child sent cannot get them. Whether the answer should echo the child's
  precision is open.
- **Is the built-in table the right fallback for a palette `?`?** A palette
  query is answered with the built-in value when nothing is overridden. If a
  host considers its own palette the truth, that answer is wrong for the
  first 16 entries; if the host never told the crate what its palette is, there
  is no better answer. This RFC keeps the built-in fallback and does not add a
  way for the caller to supply an initial palette. The demand for one is
  expected to be low: the caller is the code that interprets the child's
  output, and the host's real colours usually live outside it (in the host
  terminal's own configuration), while a child that queries a palette is
  satisfied by getting back what it set. Whether to add a caller-supplied
  initial palette is left to a future RFC, to be written when a caller that
  needs one actually appears; the accessors are the same either way, only the
  seed differs.

## Future possibilities

- If the crate gains a caller-supplied initial palette, the built-in table
  becomes a default the caller can override, and `?` answers the caller's
  values - the same accessors, a different seed.
- A notification for colour changes (the first Unresolved item), if added,
  would ride the event channel the other RFCs introduce, and would be the
  policy's first state-row value with its own event.
- OSC 5 (special colours, an rgb-indexed set beyond 255) and OSC 13/14
  (cursor/pointer foreground) are the same shape and would extend the state
  and add accessors, not change the design.
- `Color::to_rgb()` could gain an explicit doc sentence that it resolves the
  *default* table and never a child override, pointing at `palette_color` for
  the latter - a doc change only, no code.
