# RFC: Hand colour requests to the caller

- Status: draft

## Summary

Stop owning colour state. termnix already frames the OSC sequences that carry
colour - `4` (palette entries), `10` / `11` / `12` (default foreground,
background, cursor) - but today it *interprets* them: it keeps a palette table
and a set of default slots, applies each `set` to them, and answers each `?`
query from them. This proposal moves colour to the same footing as the
clipboard: a child's `set` becomes a [`ChildRequest::SetColor`] the caller
receives, and a child's `?` becomes a [`ChildRequest::GetColor`] the caller
answers by enqueuing a colour message as input. termnix keeps no palette and
resolves no colour; it decodes the fields and hands them over.

The consequence is that there is no `Palette` type, no
`TerminalState::palette()` / `set_palette()`, no `Color::to_rgb`, and no
`Style::foreground_rgb` / `background_rgb`. A host that paints reads the
child's colour requests as they arrive and writes back the colour messages it
owes; the rendering layer owns the palette, because it is the layer that can
observe the host terminal's actual colours.

This RFC is written against the same problem as
[`20261008-rfc-collect-colours-into-a-palette`](20261008-rfc-collect-colours-into-a-palette.md),
which takes the opposite path - keep the colour state and fold it into one
public `Palette` type. That proposal is not withdrawn; the two are alternatives
and the choice is open.

## Motivation

The colour state exists because termnix chose to resolve `Color::Indexed` into
an `Rgb` itself. But that resolution is not a fact about the terminal, it is a
fact about the *host terminal's theme*, which only the rendering layer can see:

- **termnix cannot answer a colour query correctly.** When the child sends
  `OSC 4 ; 1 ; ?`, the true answer is "whatever the host terminal paints for
  index 1". termnix does not know that - it knows the built-in xterm table, not
  the user's theme - so any answer it gives is a guess. Today it does answer,
  from its own table, and the guess is silently wrong for every themed terminal.
- **There are two authorities for one fact.** If termnix keeps a palette *and*
  the rendering layer keeps one (it must, to paint), the two can disagree.
  Changing the theme has to update both, and a caller that forgets one side
  paints a palette the child's queries no longer match. A single authority -
  the rendering layer - has no such failure mode.
- **The colour accessors are a second, thinner spelling of state the caller
  already has.** A host that tracks OSC 4 itself learns nothing new from
  `palette_color(i)`; it learns a value it already recorded, in a shape it has
  to reconcile with its own.

termnix already has the shape for this. The clipboard has exactly the same
problem - termnix frames OSC 52 but does not own a clipboard - and the answer
there is [`ChildRequest::SetClipboard`] and [`ChildRequest::GetClipboard`]: the
child's ask is recorded and handed to the caller, and a read is answered by the
caller writing back to the PTY. Colour is the same kind of resource. This
proposal makes it the same kind of request.

What termnix *should* own is the framing. The OSC grammar, the field split, the
`rgb:` decoding, and the report encoding are tedious and easy to get wrong; a
caller should not have to re-tokenize the PTY stream to find an OSC 4. So the
crate keeps parsing - it hands over a decoded `slot` and `Rgb` - and gives up
ownership.

## Guide-level explanation

Today a host reads colour out of the terminal state through accessors, and the
terminal answers colour queries from a table it built:

```rust
// The host wants to know what to paint for a cell...
let fg = cell.style.foreground.to_rgb();        // None for Color::Default
let fg = term.default_foreground_color();       // Option<Rgb>, disagrees
let entry = term.palette_color(1);              // Rgb, hidden xterm fallback

// ...and the child's `OSC 4 ; 1 ; ?` is answered by termnix, from its
// built-in table, whether or not that matches the host's theme.
```

After, the child's colour asks arrive as requests, next to the clipboard's:

```rust
let mut events = session.terminal_state_mut().next_event();
while let Some(event) = events {
    if let Event::RequestReceived(request) = event {
        match request {
            ChildRequest::SetColor { slot, rgb } => {
                // Remember it; paint with it next frame.
                self.palette.set(slot, rgb);
            }
            ChildRequest::GetColor { slot } => {
                // Answer it, from the palette only we can see.
                let rgb = self.palette.get(slot);
                session.enqueue_input(Input::Color { slot, rgb })?;
            }
            _ => {}
        }
    }
    events = session.terminal_state_mut().next_event();
}
```

The host now owns exactly one palette - its own - and termnix writes the report
bytes. A host that only forwards and does not answer leaves the child waiting,
which is the same behaviour as a caller that ignores `GetClipboard`.

## Reference-level explanation

### A colour is named by a slot

OSC 4 names a palette index (`0..=255`); OSC 10, 11, and 12 name one of the
three default slots. Both are "a colour the terminal can be asked about," so
both arms live in one type:

```rust
// src/terminal_types.rs

/// A colour the terminal can be asked to set or report.
pub enum ColorSlot {
    /// A palette entry (`0..=255`), as OSC 4 addresses.
    Indexed(u8),
    /// The default foreground (OSC 10).
    DefaultForeground,
    /// The default background (OSC 11).
    DefaultBackground,
    /// The cursor colour (OSC 12).
    DefaultCursor,
}
```

This is `Copy`, `Clone`, `PartialEq`, `Eq`, and `Hash` (it holds a `u8` at
most), so it can travel in `ChildRequest` and in `Input` without a
`Vec`. It is deliberately not an enum over "the number the OSC used": the two
ranges mean different things (an index into a table vs a named default), and
merging them into a raw `u32` would push the decode back onto the caller.

### Setting a colour is a request

```rust
// src/terminal_types.rs

pub enum ChildRequest {
    // ... existing variants ...

    /// An OSC 4 / 10 / 11 / 12 colour write.
    ///
    /// The sequence asks the terminal to change a colour; termnix owns no
    /// palette, so the ask is what a caller receives. Which colour it is is
    /// carried in [`ColorSlot`].
    SetColor {
        /// Which colour the application addressed.
        slot: ColorSlot,
        /// The decoded 24-bit value it asked to set.
        rgb: Rgb,
    },

    /// An OSC 4 / 10 / 11 / 12 colour read (`... ? ST`).
    ///
    /// The sequence asks the terminal to report a colour. termnix owns no
    /// palette and cannot see the host terminal's theme, so it cannot answer
    /// correctly and writes nothing to the output buffer: the caller that owns
    /// the palette is the one that answers, by enqueuing an
    /// [`Input::Color`](crate::Input::Color). A caller that ignores this
    /// variant leaves the child waiting, which is what a caller that only
    /// forwards writes will do.
    GetColor {
        /// Which colour the application asked for.
        slot: ColorSlot,
    },
}
```

`ChildRequest` does not gain `Copy` or `Hash`; it already holds a `Vec<u8>` in
`SetClipboard`, so the new variants fit the existing derive set (`Clone`,
`PartialEq`, `Eq`).

### A colour message the host sends is input

A colour report travels *toward* the child, which is the direction [`Input`]
already covers. Rather than a method on `TerminalState` that reaches into the
session's write queue, the report is an `Input` variant, so it joins the
existing enqueue path with its accounting and backpressure:

```rust
// src/input.rs

pub enum Input<'a> {
    // ... existing variants ...

    /// A colour report, as OSC 4 / 10 / 11 / 12.
    ///
    /// Encoded as the OSC that names the slot: OSC 4 for
    /// [`ColorSlot::Indexed`], OSC 10 / 11 / 12 for the default slots. OSC has
    /// no request/response framing, so this is simply the colour message the
    /// host sends back; a child waiting on a `... ?` query is the usual
    /// sender's motive, but nothing in the wire form says so.
    Color {
        /// Which colour is being reported.
        slot: ColorSlot,
        /// The value to report.
        rgb: Rgb,
    },
}
```

`Color` is `Copy` and `Hash` because `ColorSlot` and `Rgb` are, so `Input`'s
existing derives are unchanged.

`Input`'s doc comment currently reads "Application input to enqueue on a
Session" and describes `Key`, `Paste`, and `Mouse` as "turned into PTY bytes
with the session's current modes". A colour report is not application input in
the sense of a user action, but it is the same direction and the same queue,
and the doc is broadened to say so: the enum is "bytes the host sends toward
the child," of which user input is the common case and a protocol message is
another. This is the one place the semantics widen rather than a new type being
added.

The variant is named `Color` rather than `ColorResponse` or `ColorReply`
because OSC has no request/response layer to draw the name from. A child writes
`OSC 4 ; 1 ; ?`; the host writes `OSC 4 ; 1 ; rgb:...`; the two are the same
kind of message in opposite directions. `Input::Color` names the resource and
leaves the direction to the enum, matching `Key`, `Mouse`, and `Paste`.

### What `input.rs` gains

`Input::Color` needs a `write_to` arm. The encoding is fixed by the slot:

- `ColorSlot::Indexed(i)` -> `ESC ] 4 ; i ; rgb:RR/GG/BB ST`
- `ColorSlot::DefaultForeground` -> `ESC ] 10 ; rgb:RR/GG/BB ST`
- `ColorSlot::DefaultBackground` -> `ESC ] 11 ; rgb:RR/GG/BB ST`
- `ColorSlot::DefaultCursor` -> `ESC ] 12 ; rgb:RR/GG/BB ST`

The `rgb:` form and its hex are what the child sent in other OSCs; the same
encoder the crate uses to parse `rgb:` (or its inverse) is written once here.
`Input::byte_len` gains the same arm.

### What the OSC handler does

- **OSC 4 set** -> `ChildRequest::SetColor { slot: ColorSlot::Indexed(i), rgb }`.
- **OSC 4 query** -> `ChildRequest::GetColor { slot: ColorSlot::Indexed(i) }`.
- **OSC 10 / 11 / 12 set** -> `ChildRequest::SetColor { slot, rgb }` with the
  matching default slot.
- **OSC 10 / 11 / 12 query** -> `ChildRequest::GetColor { slot }`.
- **RIS** -> no colour state to reset; only the palette *record* the host keeps
  is affected, and resetting that is the host's call. termnix emits nothing for
  colours on RIS.

The parsing of the `rgb:` payload is unchanged; only its destination is. A
malformed colour sequence is still ignored as before - the crate does not hand
a half-decoded colour to the caller.

### What is removed

- `TerminalState::palette_color`, `default_foreground_color`,
  `default_background_color`, `default_cursor_color`.
- `Color::Default`, and with it `Color`'s `Default` derive (a style's slots
  become `Option<Color>` where `None` is the default).
- `Color::to_rgb`, and with it the resolution path entirely.
- The `palette` field on `TerminalState`, the `DefaultColors` struct, and the
  `XTERM_SYSTEM` / `indexed_rgb` / `palette_level` / `xterm_colors` tables
  that only existed to resolve an index or seed a fallback.

### API at a glance

Against `main` (not against any draft that once added a `Palette`):

| Item | Before | After |
|---|---|---|
| `ColorSlot` | - | new (`Indexed(u8)` / `DefaultForeground` / `DefaultBackground` / `DefaultCursor`) |
| `ChildRequest::SetColor` | - | new (`{ slot, rgb }`), from OSC 4/10/11/12 set |
| `ChildRequest::GetColor` | - | new (`{ slot }`), from OSC 4/10/11/12 query |
| `Input` | `Raw` / `Key` / `Paste` / `Mouse` | + `Color { slot, rgb }` |
| OSC 4/10/11/12 set | writes `TerminalState` colour state | emits `ChildRequest::SetColor` |
| OSC 4/10/11/12 query | answered from `TerminalState` | emits `ChildRequest::GetColor` |
| `TerminalState::palette_color(i)` | accessor (`Rgb`, xterm fallback) | removed |
| `TerminalState::default_foreground_color()` | accessor (`Option<Rgb>`) | removed |
| `TerminalState::default_background_color()` | accessor (`Option<Rgb>`) | removed |
| `TerminalState::default_cursor_color()` | accessor (`Option<Rgb>`) | removed |
| `Color::Default` | enum variant | removed; default is `Option<Color>` on `Style` |
| `Style::foreground` / `background` | `Color` | `Option<Color>` |
| `Color::to_rgb` | resolver | removed |
| `palette` field / `default_colors` | state on `TerminalState` | removed |
| `XTERM_SYSTEM` / `indexed_rgb` | internal tables | removed |

Every removed accessor answered "what is this colour now?". That question is
now the caller's, kept from the `SetColor` requests it receives:
`palette_color(i)` from `Indexed(i)` requests, and the three
`default_*_color()` accessors from the three default slots. The accessors'
`Option<Rgb>` - "did the child set one?" - becomes the caller's own
`Option<Rgb>`; termnix no longer keeps the distinction, because the caller that
owns the palette is the one that cares whether an entry has been touched.

One small loss: `Color::to_rgb` also turned a `Color::Rgb(rgb)` into its own
`rgb`, so removing it drops the easy way to pull a direct value out of a
`Color`. `Color` keeps both variants public, so a caller that needs it matches
`Color::Rgb(rgb) => rgb`; nothing about the direct case required the resolver.

`Color` loses its `Default` variant. A style's foreground or background
becomes `Option<Color>`, where `None` is "the terminal default" - the SGR 39
/ 49 state. This is the same distinction `Color::Default` drew, expressed
where it belongs: the cell still has to say "the default", but that is a
statement about the slot, not about a colour value. `Color` becomes only a
concrete colour request (`Indexed` or `Rgb`), and `Color`'s `Default` derive
goes with it (a style's slots start as `None`, which `Style::default()`
supplies).

`Style::foreground` and `Style::background` thus change type from `Color` to
`Option<Color>`; `terminal_emu`'s SGR 39 / 49 arms set them to `None` instead
of `Color::Default`.

`ColorsUpdated` may still be fired internally for a `SetColor`, so that a host
caching a *view* of colour state can invalidate it; whether it remains depends
on whether anything still needs it once no accessor reads the state. An
implementation may drop it if the host's own OSC-4 handling is the only
reader. Flagged in Unresolved.

## Drawbacks

- **A breaking change across the colour API.** `Color::to_rgb`, the four
  `TerminalState` colour accessors, and `Style`'s resolving methods all
disappear; a caller that painted from `TerminalState` must now paint from its
  own palette and read `ChildRequest`s. The crate is at `0.x`, so the cost is a
  changelog and a version bump.
- **Every host that paints now owns a palette.** A host that was happy to let
  termnix answer colour queries must now keep a table and answer them, or leave
  the child waiting. This is the point of the change - a host that does not
  paint already does not care - but it is real work for a host that did lean on
  the built-in table.
- **`Input` grows a variant that is not application input.** The enum's name
  and doc say "input"; a protocol message is a new kind of thing in it. The
  alternative is a separate enqueue path, which duplicates the queue,
  accounting, and backpressure. See Rationale.
- **`ColorSlot` merges two OSC ranges.** A caller that only cares about OSC 4
  must match `Indexed` and can ignore the three defaults; a caller that only
  cares about OSC 10 must ignore the 256 indices. One enum is fewer types than
two, but the two uses are different, and a caller's match is slightly wider
than the message.
- **The xterm table moves out of the crate.** A host that wants the built-in
  defaults must carry its own copy, or depend on a crate that has one. termnix
  no longer ships the table it keeps today behind `palette_color`'s fallback.

## Rationale and alternatives

- **Keep the resolution in termnix and fold it into one `Palette`.** This is
  the sibling RFC. Rejected here because termnix cannot observe the host
  terminal's theme, so any table it keeps is a guess that can disagree with the
  layer that paints. The `Palette` proposal removes the *inconsistency* between
  termnix's accessors but leaves two authorities for one fact.
- **Keep the state but make it optional: `Palette` as
  `[Option<Rgb>; 256]`.** Rejected for the same reason plus the query problem:
  an `Option` records "the child set this," but a `?` for an unset index still
  has no correct answer in termnix. The record is only half the resource.
- **Reply to `?` from a termnix-held table as a fallback.** Rejected: it is the
  guess this change exists to remove. A wrong answer is worse than none for a
  child that trusts the report; a child that does not trust it falls back
  anyway, so no answer costs nothing.
- **Add `TerminalState::reply_color(slot, rgb)` instead of an `Input`
  variant.** Rejected in favour of `Input::Color` (though both are workable): a
  session is the thing with a write queue, and `TerminalState` does not expose a
  write path. A `Session`-only `reply_color` would be a thin alias for
  `enqueue_input(Input::Color { .. })`; making the variant the primitive and
  skipping the alias keeps one spelling. A caller that wants to format bytes
  itself still has `Input::Raw`.
- **Give the clipboard the same reply variant, for symmetry.** Rejected for
  now. An `Input` variant earns its place by being the write path for bytes the
  host owes the child, and only the colour query needs one today: a child
  waiting on `OSC 4 ; 1 ; ?` has nothing else to read. The clipboard read has no
  such wait, so `Input::Clipboard` would be a variant with no sender. The two
  are asymmetric because the crate ships only what a caller must write back;
  if a clipboard reply is ever needed, it takes the same shape as `Input::Color`.
  Flagged in Unresolved.
- **Hand the child's raw OSC to the caller as `OtherOsc` and stop parsing
  colour entirely.** Rejected: the crate already frames and splits OSC fields,
  and `rgb:` decoding plus the reply encoding are exactly the fiddly parts a
  caller should not repeat. Parsing without owning is the point.
- **Do nothing.** The colour state stays a guess that can disagree with the
  rendering layer, and the accessors stay a second spelling of it. See
  Motivation.

## Unresolved questions

- **Does `ColorsUpdated` survive?** With no accessor reading the colour state,
  the event may have no reader. It may still be useful to a host that caches a
  view of colour, or it may be dead. Depends on whether the host's own OSC-4
  handling is the only reader, which the implementation will show.
- **Should `SetColor` and `GetColor` name the OSC, or the slot only?** A caller
  that needs to distinguish "OSC 4 index 10" from "OSC 10" has the slot
  already; a caller that wants the raw channel does not. The slot alone is
  proposed, but the fidelity of the current `OtherOsc` escape hatch is lost for
  these two sequences - they no longer arrive as `OtherOsc`.
- **Is `ColorSlot::DefaultCursor` worth a variant?** OSC 12 is barely used and
  its query is near-nonexistent. Keeping it costs one arm; dropping it would
  turn OSC 12 into `OtherOsc`. Open.
- **Should the crate ship the built-in xterm table as a public constant anyway?**
  A host that wants the standard defaults would otherwise copy it. A
  `pub const XTERM_COLORS: [Rgb; 256]` with no state attached is not a second
  authority - it is data - so it could stay even after the state is gone.
- **Does the answer to a `?` have to correspond to one `GetColor`?** OSC 4 has
  no round-trip number, so termnix cannot pair a colour report with its ask.
  The caller is trusted to answer each `GetColor` once; a spurious
  `Input::Color` is encoded and sent regardless. Same trust the clipboard read
  already places in the caller.
- **Should the clipboard read gain a matching reply variant?** Today
  `ChildRequest::GetClipboard` has no `Input` counterpart, so a host that wants
  to answer it must build the OSC 52 bytes itself (or rely on `OtherOsc`). That
  is a real asymmetry with colour, but the clipboard read does not block a child
  the way a colour query does, so no variant is proposed here. If a reply path
  is wanted, it takes the same shape as `Input::Color`; the two should then
  share a spelling.

## Future possibilities

- A `ColorSlot` -> OSC encoder helper (or a `TerminalState` convenience that
  builds an `Input::Color` from `(slot, rgb)`) would save a caller the two-field
  variant, if the verbosity proves annoying.
- If OSC 4 `?` queries turn out to be common in practice (they are not today;
  see Motivation), a small crate-provided helper that answers from a caller
  palette could be added above the raw request.
- The same treatment could extend to other resources termnix frames but does
  not own, if any move from "interpreted" to "handed over" (for example, a
  future sequence whose state is purely the host's).
