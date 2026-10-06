# RFC: Collect the colour state into a `Palette`

- Status: draft

## Summary

Move the terminal's colours out of loose accessors and into one public type,
`Palette`, that owns the resolved RGB values a host paints with. `Palette`
holds the 256-entry palette and the default foreground, background, and cursor
colours; `TerminalState::palette()` returns it by reference, and
`TerminalState::set_palette()` replaces it for a host that changes its theme
while a session runs. In the same step, `Color` stops carrying a `Default`
variant and stops resolving itself: it is a tag (`Indexed` or `Rgb`) that says
what colour a cell asks for, and resolution is the caller's job, done with the
`Palette` in hand. A style's foreground or background becomes `Option<Color>`,
where `None` is "the terminal default" - the SGR 39 / 49 state - so that the
resolved colour stays a function of the live palette instead of a value frozen
when the sequence arrived.

## Motivation

The colour state is spread across four accessors that answer overlapping
questions in different shapes:

```rust
let entry = term.palette_color(4);          // Rgb, with a hidden fallback
let fg = term.default_foreground_color();   // Option<Rgb>
let bg = term.default_background_color();   // Option<Rgb>
let rgb = color.to_rgb();                   // Option<Rgb>
```

A host that paints a child's output has to:

- Call `palette_color(i)` once per index, so a host that caches the whole
  palette loops `0..=255`, and after every `ColorsUpdated` it loops again.
- Accept that `palette_color(i)` silently substitutes the built-in xterm value
  when the child never set that entry, while `default_foreground_color()`
  refuses to substitute anything and answers `None`. The two accessors disagree
  on the same question - "what do I paint when the child set nothing?" - which
  means one of them must be wrong for its caller.
- Read `color.to_rgb()` and get `None` for `Color::Default`, even though a
  host painting a `Default` cell does have a colour to paint: the terminal's
  default. The method knows the `Color`, not the terminal, so it can only
  answer half the question.

The root of this is that one type, `Color`, carries three different things at
once: the tag a cell stores, the question "is this foreground or background",
and the resolution step. A tag cannot resolve itself, because it does not know
which palette or which default slot applies; the resolution has to be given the
palette and the slot. Splitting those responsibilities apart is what this
proposal does.

## Guide-level explanation

Today a host resolves a cell's colours through scattered accessors, and has to
special-case `Default` itself:

```rust
fn paint(cell: &Cell, term: &TerminalState) {
    let fg = match cell.style.foreground {
        Color::Default => term.default_foreground_color(),
        c => c.to_rgb(),
    };
    let bg = match cell.style.background {
        Color::Default => term.default_background_color(),
        c => c.to_rgb(),
    };
    renderer.set(fg, bg);
}
```

After, the palette holds every value the host needs, and the style resolves
against it:

```rust
let palette = term.palette();
let fg = cell.style.foreground_rgb(palette);
let bg = cell.style.background_rgb(palette);
renderer.set(fg, bg);
```

`foreground_rgb` is total: a `Some(Color)` resolves through the palette, and a
`None` becomes the palette's default foreground. The host never has to ask
"is this the default?" - the `Option` already said so, and the palette answers
it.

A host that caches the palette reads it in one call and keeps it until the next
`ColorsUpdated`:

```rust
let colors: &[Rgb; 256] = &term.palette().colors();
```

A host with its own theme replaces the whole palette - at construction, or
later when the user switches theme:

```rust
term.set_palette(theme::my_palette());
```

## Reference-level explanation

### The `Palette` type

```rust
// src/terminal_types.rs

/// The resolved colours a host paints with.
pub struct Palette {
    colors: [Rgb; 256],
    default_foreground: Rgb,
    default_background: Rgb,
    default_cursor: Rgb,
}

impl Palette {
    /// Builds a palette from a colour table and the three default slots.
    pub const fn new(
        colors: [Rgb; 256],
        default_foreground: Rgb,
        default_background: Rgb,
        default_cursor: Rgb,
    ) -> Self { ... }

    /// The xterm 256-colour palette with its default slots.
    pub const XTERM: Self = ...;

    /// The 256-entry palette, indexable by an ANSI/xterm colour index.
    pub const fn colors(&self) -> &[Rgb; 256] { &self.colors }

    /// The default foreground (OSC 10).
    pub const fn default_foreground(&self) -> Rgb { self.default_foreground }

    /// The default background (OSC 11).
    pub const fn default_background(&self) -> Rgb { self.default_background }

    /// The cursor colour (OSC 12).
    pub const fn default_cursor(&self) -> Rgb { self.default_cursor }
}
```

`Palette` is `Copy`, `Clone`, `PartialEq`, and `Eq` (the fields are `Copy` and
the table is fixed-size), so `TerminalState::set_palette` can compare a new
palette against the current one and skip a redundant `ColorsUpdated`.

Every default slot is a plain `Rgb`, not an `Option<Rgb>`. This keeps
resolution total: `Style::foreground_rgb` returns an `Rgb`, never an `Option`.
A host that wants to know whether the child ever set a default colour reads
that from the state, not from the palette (see "What does not change").

### Colours as tags

```rust
// src/terminal_types.rs

/// A cell's colour request: an index into the palette, or a direct value.
pub enum Color {
    Indexed(u8),
    Rgb(Rgb),
}

impl Color {
    /// Resolves this colour against a palette.
    pub const fn to_rgb(self, palette: &Palette) -> Rgb {
        match self {
            Self::Indexed(index) => palette.colors[index as usize],
            Self::Rgb(rgb) => rgb,
        }
    }
}
```

`Color` no longer has a `Default` variant. The "terminal default" state moves
to `Option`: a style holds `Option<Color>` per slot, and `None` is the default.
This is the same distinction `Color::Default` drew, expressed one level out,
where it can be resolved against a live palette.

`Color` also stops deriving `Default`. There is no neutral `Color` to start
from once `Default` is gone; a style's slots start as `None`, and
`Style::default()` supplies that.

### The style resolves its slots

```rust
// src/terminal_types.rs

pub struct Style {
    pub foreground: Option<Color>,
    pub background: Option<Color>,
    pub bold: bool,
    pub italic: bool,
    pub underline: bool,
    pub reverse: bool,
}

impl Style {
    /// The foreground to paint, resolving a default through the palette.
    pub const fn foreground_rgb(&self, palette: &Palette) -> Rgb {
        match self.foreground {
            Some(color) => color.to_rgb(palette),
            None => palette.default_foreground,
        }
    }

    /// The background to paint, resolving a default through the palette.
    pub const fn background_rgb(&self, palette: &Palette) -> Rgb {
        match self.background {
            Some(color) => color.to_rgb(palette),
            None => palette.default_background,
        }
    }
}
```

A style is `Copy`, so it resolves by value; `foreground_rgb` and
`background_rgb` are `const fn` and take the palette by reference, because the
palette is not `Copy` in the sense a style needs (it is 768 bytes, and passing
by reference keeps the call cheap).

The resolution is deliberately deferred: a cell keeps `None` (or
`Some(Indexed(i))`) and resolves at paint time. When the child redefines a
palette entry with OSC 4, or a host swaps the palette, every cell that points
at it paints differently on the next frame without any cell being rewritten.
This is why the default cannot be baked into the cell when SGR 39 / 49
arrives: a default foreground the child sets later must reach cells the child
already painted.

### `TerminalState`

```rust
// src/terminal.rs

impl TerminalState {
    /// Returns the colours the terminal resolves with.
    pub fn palette(&self) -> &Palette { &self.palette }

    /// Replaces the colours, marking the visible colour state changed.
    pub fn set_palette(&mut self, palette: Palette) {
        if self.palette != palette {
            self.palette = palette;
            self.events.mark_colors_updated();
        }
    }
}
```

`TerminalState::new(size)` starts with `Palette::XTERM`, so the constructor
signature does not change and every existing caller keeps working. A host that
wants another theme calls `set_palette` after construction; a host that
switches theme later calls it again. `set_palette` fires `ColorsUpdated` when
the palette actually differs, so the same signal that already tells a host to
re-resolve after OSC 4 also covers a host-driven change.

The old accessors - `palette_color`, `default_foreground_color`,
`default_background_color`, `default_cursor_color` - are removed; their
questions are answered by `palette()` and the style's `*_rgb` methods. The
`DefaultColors` struct and the `palette: Box<[Option<Rgb>; 256]>` field are
replaced by the `Palette` field.

### OSC handling

- **OSC 4 set**: `palette.colors[index] = rgb;` and `mark_colors_updated()`.
- **OSC 4 query**: answers `rgb:../..` from `palette.colors[index]`. The table
  is total, so there is no fallback and no child override to merge: the entry
  is whatever was last set, and the xterm default when nothing was.
- **OSC 10 / 11 / 12 set**: writes `palette.default_foreground` / `_background`
  / `_cursor` and fires `ColorsUpdated`.
- **OSC 10 / 11 / 12 query**: answers from the same slot.
- **RIS**: resets the palette to `Palette::XTERM`.

Because the table is `[Rgb; 256]` and starts at the xterm values, there is no
`None` entry and no per-index fallback branch. The one place a fallback used to
live - `palette_color` substituting `indexed_rgb(index)` - is gone, because the
substitution happened at construction instead.

### `Color::to_rgb` by hand

A caller that only has a `Color` and a palette resolves it directly, without a
style:

```rust
let rgb = color.to_rgb(palette);
```

This is the same resolution `foreground_rgb` and `background_rgb` do once a
slot is known to hold a `Some`.

## Drawbacks

- **A breaking change across the colour API.** `Color::Default` disappears,
  `Style`'s two colour fields change type, four `TerminalState` accessors are
  removed, and `to_rgb` changes shape. Every caller that matched on
  `Color::Default`, called `palette_color`, or read `style.foreground` as a
  `Color` must move. The crate is at `0.x`, so the cost is the changelog and a
  version bump, not a field migration.
- **`Palette` is a 768-byte value.** `Copy` on a type that large is a footgun
  if a caller passes it by value where a reference would do; the accessors take
  `&Palette` for that reason. It also means `TerminalState` carries the table
  inline rather than behind a `Box`, which the earlier design boxed on purpose.
  The box goes away because the table is now a value the host wants to read as
  a whole, not a sparse override map.
- **A `Set` on a total table loses a distinction.** The old sparse
  `[Option<Rgb>; 256]` recorded which entries the child had touched. The total
  table does not. Nothing in the API needs that distinction - resolution and
  queries care about the value, not its provenance - but a future feature that
  wants "did the child set this?" would have to add it back.
- **`Option<Color>` on a `Copy` style.** The style's colour fields get one
  level deeper, and a caller that wants the raw tag now writes `.foreground`
  and gets an `Option` where it used to get a `Color`.

## Rationale and alternatives

- **Keep `Color::Default` and `to_rgb(self) -> Option<Rgb>`.** Rejected: the
  `None` return is not a real "no colour" - a `Default` cell does paint, with
  the terminal's default - so the `Option` is a signal that the method lacks
  the information to answer. `to_rgb` cannot resolve `Default` without knowing
  foreground-vs-background and the default slots, which is exactly the
  information the palette and the style carry.
- **Keep `Color::Default` but give `to_rgb` the palette:
  `to_rgb(self, &Palette) -> Rgb`.** Rejected: `Color` still cannot tell
  foreground from background, so `Default` has no single answer. Either
  `to_rgb` returns the foreground default for a background cell (wrong), or the
  type carries a slot it should not, or the method does not compile for
  `Default`. Moving the default state to `Option<Color>` on the style is what
  lets the resolution stay total without guessing.
- **Make the resolution eager: on SGR 39 / 49, write the current default RGB
  into the pen.** Rejected: the default is a moving target. A child that sets
  OSC 10 after painting a `Default` cell must repaint that cell, and an eager
  write freezes the old default into the cell where a later palette change
  cannot reach it. The deferred `None` keeps the reference live.
- **Keep the four accessors and add `palette()` beside them.** Rejected: two
  ways to ask the same question drift. The accessors exist because the state
  was not one value; once it is a `Palette`, the accessors are a thinner
  spelling of `palette().default_foreground()` and should not both exist.
- **Expose the palette as `&[Option<Rgb>; 256]` plus `pub const XTERM_COLORS`,
  and let the caller fall back.** Rejected: it pushes the per-index fallback
  onto every caller and keeps two spellings of the default table (the state's
  and the constant). A total table built once from the xterm values answers the
  same question with no branch, and the constant is still reachable as
  `Palette::XTERM`.
- **Take the `Palette` in the constructor:
  `TerminalState::new(size, palette)`.** Rejected in favour of `set_palette`:
  a host that switches theme at runtime needs the setter anyway, and a host
  that does not can still call it once. A constructor parameter would force
  every caller and every test to name a palette for a value they usually do
  not care about.
- **Do nothing.** The colour API stays a set of accessors that disagree on the
  "nothing was set" case and a `to_rgb` that cannot answer for `Default`. See
  Motivation.

## Unresolved questions

- **Should `Palette::XTERM`'s three default slots be pinned to particular
  values, or the xterm system palette's own white/black?** The table's 0--15
  entries are the system palette; the default foreground and background are a
  separate pair. This proposal uses the xterm background as the default
  background and the xterm foreground as the default foreground, but the exact
  pair is a detail that can be settled at implementation without changing the
  design.
- **Should the `colors` accessor return `&[Rgb; 256]` or `&[Rgb]`?** The fixed
  length is a fact about the xterm palette and lets callers rely on it, but it
  ties the slice's length into the type. Open.
- **Does `Color` still need a `Default` for any internal use?** The parser and
  the buffer build colours directly and start from `None`, so this proposal
  expects not, but an implementation that finds a spot should say so rather
  than re-deriving it silently.

## Future possibilities

- A `Palette::from_hex` / `to_hex` pair, or a builder, would let a host
  describe a theme without spelling 256 `Rgb`s.
- A query for "did the child set this entry?" could be restored with a
  parallel `[bool; 256]` or a `Option<Rgb>` table if a feature ever needs it.
- The `Palette` type is the natural home for a theme's cursor colour, selection
  colour, and other slots a host may add; today it holds only what the OSC
  sequences can set.
