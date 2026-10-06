# RFC: Name the RGB triple

- Status: accepted

## Summary

Give the `(u8, u8, u8)` triple that the colour API returns a name: an `Rgb`
value type. Today the palette accessors, the default-colour slots, and
`Color::to_rgb()` all hand the caller a bare tuple, which carries no field names
and no meaning in its type. A named `Rgb` says what the three bytes are and
gives the callers a place to hang methods and docs. The cell colour variant
`Color::Rgb` then carries that same `Rgb` rather than its own copy of the three
bytes, so the crate has one spelling of "three channels" instead of two.

## Motivation

A host application that paints a child's output reads colours back from the
terminal and pushes them into a renderer. The calls it makes return a tuple:

```rust
let index_color: (u8, u8, u8) = term.palette_color(4);
let fg: Option<(u8, u8, u8)> = term.default_foreground();
let resolved: Option<(u8, u8, u8)> = color.to_rgb();
```

A tuple is a poor carrier for a value the API talks about this much:

- It has no field names, so a caller that wants only the green channel writes
  `.1` and has to remember the order.
- Its type is anonymous, so two different triples in a signature are the same
  type, and the rustdoc for a returned tuple has nowhere to attach an
  explanation beyond the prose that already sits there.
- It cannot grow. Adding a constructor, a conversion, or a named constant to
  the triple means adding a free function or a method somewhere else.

This is the one place the earlier colour work left a tuple on purpose. That
proposal kept `(u8, u8, u8)` so it would not pull `Color` into its scope; the
value type is a separate change, and this is it.

## Guide-level explanation

Before, three channels are positional, and a cell colour carries its own copy
of them:

```rust
let (r, g, b) = term.palette_color(4);
let color = Color::Rgb(r, g, b);
let Color::Rgb(r, g, b) = color else { todo!() };
```

After, the same values arrive named, and the cell colour holds the named value:

```rust
let rgb = term.palette_color(4);
let color = Color::Rgb(rgb);
let Rgb { r, g, b } = rgb;
```

A caller that works with the channels keeps the same names, because `Rgb` can be
destructured like the tuple it replaces:

```rust
let Rgb { r, g, b } = term.palette_color(4);
```

The shift is small and mechanical: the triple gains a name, the signatures that
returned it return `Rgb` instead, the cell colour variant carries that value
instead of its own three bytes, and the callers name the channels rather than
counting them.

## Reference-level explanation

### The type

```rust
/// A 24-bit RGB colour, one byte per channel.
///
/// The channels are the direct value of a colour, not an index into a palette.
/// A palette index stays a `u8`, and a host resolves it through
/// [`TerminalState::palette_color()`](TerminalState::palette_color).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct Rgb {
    /// The red channel.
    pub r: u8,
    /// The green channel.
    pub g: u8,
    /// The blue channel.
    pub b: u8,
}

impl Rgb {
    /// Builds a colour from its three channels.
    pub const fn new(r: u8, g: u8, b: u8) -> Self {
        Self { r, g, b }
    }
}
```

`Rgb` is `Copy` and `Eq` so it behaves like the tuple it replaces: a caller that
compared two tuples for equality, hashed one, or copied one out of a borrowed
cell keeps doing so. It is `Hash` for the same reason the tuple was - it is a
plain value. `Default` is not derived, because `(u8, u8, u8)` has no `Default`
either and `Rgb::default()` would silently be black, which is a colour a caller
might not have meant.

The fields are public and named, matching `Color::Rgb`, which now carries this
type inline; making this type opaque would force an accessor where the cell
colour type needs none.

### Signatures that change

The returned type changes; the shapes do not.

```rust
// src/terminal_types.rs
pub fn to_rgb(&self) -> Option<Rgb>;

// src/terminal.rs
pub fn palette_color(&self, index: u8) -> Rgb;
pub fn default_foreground(&self) -> Option<Rgb>;
pub fn default_background(&self) -> Option<Rgb>;
pub fn default_cursor(&self) -> Option<Rgb>;
```

Internally the palette array, the three default-colour slots, the built-in
table helper `indexed_rgb`, and the OSC parser `parse_osc_rgb` all move from
the tuple to `Rgb` in the same step, so no conversion sits between the parser
and the accessors.

### What does not change

- [`Color`] keeps its variants. `Color::Indexed(u8)` stays an index, because an
  index is not an `Rgb`, and `Color::Default` stays a variant, because it is
  the absence of a colour and has no `Rgb` to hold. `Color::Rgb` now carries an
  `Rgb` instead of the three `u8`s, so the cell colour and the resolved colour
  share one type rather than two spellings of "three bytes".
- `Color::to_rgb()` still resolves the *built-in* table and never a child
  override; the override is still `palette_color`'s job. The only change is
  that both now return the same named type.

### Naming

`Rgb` follows the crate's existing spelling of the three channels - `Color::Rgb`
and the `rgb:` OSC prefix both use the same word - rather than a longer
`RgbColor` or `Color8`. There is no `Rgb8`/`Rgb16` pair to disambiguate, so the
bit width is not in the name.

## Drawbacks

- **A breaking change to `Color::Rgb`.** Every caller that constructs
  `Color::Rgb(r, g, b)` or destructures it in a `match` must switch to
  `Color::Rgb(Rgb::new(r, g, b))` and `Color::Rgb(rgb)`. The crate is at `0.x`,
  so the real cost is the changelog line and the compiler version bump, not a
  migration in the field.
- **The tuple and the struct are layout-compatible but not interchangeable.**
  `Rgb { r, g, b }` is not `(u8, u8, u8)`, so a caller that passed the tuple to
  a function taking a tuple must switch to `(rgb.r, rgb.g, rgb.b)` or update
  that function. This is the intended break; there is no implicit conversion.

## Rationale and alternatives

- **Keep the tuple.** Rejected: the value is the thing the docs describe as "a
  colour" and it crosses the public API at five call sites; a name is where the
  docs and any future methods can go. The tuple was kept only so the palette
  proposal would not pull `Color` into its scope, which is no longer a reason.
- **Make `Rgb` a newtype over the tuple (`struct Rgb((u8, u8, u8))`).** Rejected:
  it buys nothing a struct with named fields does not, and it hides the fields
  behind `.0` or accessors where `Color::Rgb` exposes them.
- **Add accessor methods (`r()`, `g()`, `b()`) instead of public fields.**
  Rejected: `Color::Rgb` is a public-field variant, so the cell colour type and
  this value type would disagree on how a channel is read for no gain; a public
  field is the same access with less ceremony.
- **Remove `Color` and return `Option<Rgb>` everywhere.** Rejected: the two
  answer different questions - a cell colour may be `Default` or an index,
  while a resolved colour is always three bytes - and folding them together
  would make every resolution return a `Color` that can be `Default` or
  `Indexed`, re-introducing the cases the accessors already exclude. `Color`
  keeps its variants; only the `Rgb` arm changes to carry the named value.
- **Return a third-party colour type.** Rejected: termnix does not depend on a
  colour crate, and pulling one in to name three bytes is a dependency for a
  three-field struct.
- **Do nothing.** The tuple stays, and every future method on a colour has to
  be a free function instead. See Motivation.

## Unresolved questions

- **Should `Rgb` gain a `from(Color)` conversion?** A helper that turns an
  `Indexed` or `Rgb` cell colour into a resolved `Rgb` would duplicate what
  `to_rgb` already does; whether a `From` impl reads better than a method is
  open, but it would not change this proposal's shape.

## Future possibilities

- A `Display` or `FromStr` for the `rgb:RR/GG/BB` form would let the parser and
  the reply builder share the type, instead of the crate formatting the bytes
  by hand in the reply.
- An `Rgb::from_hex`/`to_hex` pair would give callers the string form the OSC
  sequences use.
- If a third-party colour type is ever adopted, `Rgb` is the single place a
  conversion would be written.

## Outcome

Implemented in [#22](https://github.com/sile/termnix/pull/22) (merged as `33187c7`).

The `(u8, u8, u8)` triple has a name: the new `Rgb` value type, with public
`r`, `g`, and `b` fields. The palette accessor, the three default-colour
accessors, and `Color::to_rgb()` now return `Rgb`, and `Color::Rgb` carries an
`Rgb` instead of its own three bytes. The parser and the palette table build
`Rgb` directly, so no conversion sits between them and the accessors.

`Rgb` derives `Copy`, `Eq`, and `Hash` like the tuple it replaces; `Default`
is not derived, so `Rgb::default()` cannot silently be black. `Color` keeps
its variants - `Indexed(u8)` stays an index and `Default` stays the absence of
a colour - and `to_rgb()` still resolves only the built-in table.

The scope is unchanged from what is described above.
