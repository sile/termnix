# RFC: Encode mouse input for the PTY

- Status: draft

## Summary

Add a logical mouse input to [`Input`] so that a program that owns a
[`Session`] can forward host mouse events to the child, with the wire bytes
chosen from the child's current [`TerminalModes`] the same way [`Key`] and
[`Paste`] already are.

Today `MouseButton` exists and `TerminalModes::mouse` / `mouse_sgr` are tracked
from the child's output, but no public API connects them: the module says so
outright ("Mouse report byte sequences are out of scope here"). The two halves
of the feature are present and unwired, which leaves every host application to
write the encoder itself.

## Motivation

termnix models the host side of a session as three logical inputs - a key, a
paste, and raw bytes - and encodes the first two from the session's current
modes. Mouse is the one input a full-screen guest is likely to want that has no
logical form at all.

This shows up immediately in a terminal multiplexer. `tuke` is a single-pane
multiplexer: it draws a soft keyboard below a PTY-backed child and forwards the
host's input to that child. Host keys are forwarded by constructing a
`KeyEvent`; host pastes by `Input::Paste`. When the host clicks or scrolls in
the child's grid, tuke has nothing to construct. It can only fall back to
`Input::Raw`, which means tuke must itself decode the child's `?1000`/`?1002`/
`?1003`/`?1006` modes and build `CSI < b ; x ; y M|m` - exactly the reasoning
termnix exists to keep in one place. `komado` reaches the same wall and handles
mouse purely as its own UI gesture, because there is no way to pass it on.

The child's modes are the reason this must live in termnix rather than in the
caller. The same physical event encodes differently depending on what the child
asked for:

| child mode | bytes for a left-button press at (10, 5) |
| ---------- | ---------------------------------------- |
| all off | *nothing - the child did not ask for mouse* |
| `?1000` + `?1006` | `\x1b[<0;10;5M` |
| `?1000`, no `?1006` | `\x1b[M` + `0x20`, `0x2b`, `0x26` |
| `?9` (X10) | press only, no release |
| `?1002` | press/release plus drag motions |
| `?1003` | all motion |

A caller that does not read `TerminalModes::mouse` (and `mouse_sgr`) will send
the wrong bytes for the child in front of it. termnix already tracks both fields
for this purpose; the encoder is the missing half.

## Guide-level explanation

Enqueueing a mouse event looks like enqueueing a key:

```rust
// Before: the child's modes are invisible to this call, so the caller has to
// read them and encode by hand.
let modes = session.terminal_state().modes();
if modes.mouse.is_active() {
    let bytes = /* build SGR or X10 by hand from modes.mouse_sgr */;
    session.enqueue_input(Input::Raw(&bytes))?;
}

// After: the event is logical and the session encodes it.
session.enqueue_input(Input::Mouse(MouseEvent {
    kind: MouseEventKind::Press(MouseButton::Left),
    position: Position { row: 4, col: 9 },
    modifiers: Modifiers::default(),
}))?;
```

The caller states *what happened* - which button, where, with which
modifiers - and not *which bytes*. Whether that becomes SGR, the legacy
`CSI M` form, or nothing at all is decided from the child's modes at enqueue
time, exactly as `Input::Key` decides whether an arrow key is `CSI A` or
`SS3 A`.

When the child has mouse reporting off, the event encodes to zero bytes. That
is deliberate: a child that never turned mouse reporting on asked not to be
told about the mouse, so a caller can forward unconditionally and let the
session drop it. A caller that would rather not construct the event at all can
check `modes().mouse.is_active()` itself; both shapes are fine, because
`enqueue_input` stays total either way.

## Reference-level explanation

### New types

```rust
/// What happened to the mouse.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum MouseEventKind {
    /// A button went down.
    Press(MouseButton),
    /// A button came up.
    Release(MouseButton),
    /// The pointer moved. `button` is the held button, if any.
    ///
    /// The caller tracks which button is down: a session keeps no
    /// mouse-button state, so a drag is reported as `Some(button)` and a bare
    /// move as `None`. A held button has to be supplied by the caller rather
    /// than inferred, or the encoder cannot tell a drag from a bare move
    /// under `?1002`. Tracking it in the session would also make
    /// `Input::byte_len` depend on hidden state instead of on the event and
    /// the modes alone.
    Motion {
        /// The button held while moving, or `None` for a bare move.
        button: Option<MouseButton>,
    },
}

/// A logical mouse event at a grid position.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct MouseEvent {
    /// What happened.
    pub kind: MouseEventKind,
    /// Zero-based position on the active screen.
    pub position: Position,
    /// Held modifiers (Shift / Alt / Ctrl).
    pub modifiers: Modifiers,
}
```

`MouseButton` already exists and is reused unchanged. `Position` is reused as
its doc already declares it to be ("grid-local, zero-based cells").

### New `Input` variant

```rust
pub enum Input<'a> {
    Raw(&'a [u8]),
    Key(KeyEvent),
    Paste(&'a str),
    /// A mouse event; report bytes follow the current modes.
    Mouse(MouseEvent),
}
```

`MouseEvent` is `Copy`, so the new variant does not make `Input` less `Copy`
than it is today.

### Encoding rules

The encoder is a pure function of `(MouseEvent, TerminalModes)`, like
`write_key`. Rules, in the order they apply:

1. **Reporting off** (`modes.mouse == MouseReporting::Off`): emit nothing.
2. **Tracking gate by mode.** The mode decides which event kinds are reported
   at all:
   - `X10` (`?9`): a press, and only for `Left`, `Middle` or `Right`. Release,
     motion, and the wheel buttons emit nothing. The wheel codes (`64`/`65`)
     were added with `?1000`, after `?9`; sending one under `?9` would hand the
     child a button it did not ask for, so the wheel is not reported here. A
     child that wants the wheel enables `?1000` or later.
   - `Normal` (`?1000`): press and release. Motion emits nothing.
   - `ButtonEvent` (`?1002`): press, release, and motion while a button is held
     (`Motion { button: Some(..) }`). A bare move emits nothing.
   - `AnyEvent` (`?1003`): all of the above plus bare motion
     (`Motion { button: None }`).
3. **Encode the reported event** in the current encoding:
   - `mouse_sgr` (`?1006`) on: `CSI < b ; x ; y M` for press/motion, `... m`
     for release, where `b` is the button code below, and `x`/`y` are 1-based.
   - `mouse_sgr` off: `CSI M` followed by three bytes
     `0x20 + b`, `0x20 + x`, `0x20 + y` with `x`/`y` 1-based. This form cannot
     represent a coordinate above 223 (`0xff - 0x20`); clamp rather than wrap,
     which is what xterm does. The clamp is silent but must be documented on
     the public API: dropping the event instead would make "the child never
     enabled mouse reporting" and "the coordinate did not fit" both look like
     the same zero bytes, and the caller could not tell them apart. A child on
     a grid larger than 223 that wants exact coordinates has to enable `?1006`.
4. **Modifiers** add xterm's bits to `b`: Shift `+4`, Alt `+8`, Ctrl `+16`.

Button codes in the low bits of `b`:

| button | SGR `b` | notes |
| ------ | ------- | ----- |
| Left | 0 | |
| Middle | 1 | |
| Right | 2 | |
| *release* | 3 | legacy form only; SGR spells release with `m` |
| WheelUp | 64 | |
| WheelDown | 65 | |
| Motion flag | 32 | OR'd in for any motion event |

The SGR/legacy split and the wheel and motion bits are the parts that are easy
to get wrong and are why this belongs in one place instead of in each caller.

### Interaction with existing features

- **`byte_len`.** `Input::byte_len` must include the new variant; the answer
  depends on `modes`, as it does for `Key` and `Paste`, and `Mouse` can be the
  zero-byte case above.
- **`enqueue_input` and modes.** As with `Key` and `Paste`, the bytes are chosen
  from the modes current at enqueue time. A child that changes `?1006` between
the event and the enqueue gets the newer encoding; this matches the existing
  behaviour for keys and needs no special handling.
- **`Raw` stays.** The escape hatch remains for callers with bytes already in
  hand. This proposal does not remove it; it removes the *need* for it in the
  common case.

## Drawbacks

- More public API: two types and one variant, plus the encoder, for a feature a
  caller may never use. The crate is pre-1.0, so the cost is surface area rather
  than stability.
- The encoder embeds protocol detail (button codes, modifier bits, the 223
  coordinate ceiling of the legacy form) into termnix, where it can go stale as
  host terminals evolve. The module already accepts that trade for keys and
  paste; this extends it to mouse.
- Choosing "emit nothing when reporting is off" makes an `Input::Mouse`
  silently produce no output. A caller debugging "why does my click not
  arrive?" has to know to check the child's modes. The alternative - an error -
  is rejected below.

## Rationale and alternatives

### Proposed: logical `MouseEvent`, encoded at enqueue time from the modes

This keeps the three-input shape of the module intact and puts the mode
knowledge where it already lives. It is the mouse case of the rule the crate
already follows: the caller states the event, the session states the bytes.

### Alternative: expose the encoder as a free function, not an `Input` variant

A `fn encoded_mouse(event: MouseEvent, modes: TerminalModes) -> Vec<u8>` would
let a caller build bytes and pass them through `Input::Raw`. It keeps `Input`
smaller, but it says the caller is responsible for calling the encoder and for
looking up the modes to call it with - the very coupling this proposal removes.
The existing `Key` and `Paste` variants set the precedent that the session does
this lookup, and mouse should not be the exception.

### Alternative: keep it out of termnix; document what callers must do

The status quo. It is defensible - the module's current doc explicitly scopes
mouse out - but it means every consumer reimplements the same encoder, and each
one gets the modes-versus-encoding subtlety wrong independently. Two known
consumers (`tuke`, `komado`'s potential guest forwarding) already need it. The
cost of doing nothing is paid per consumer, forever.

### Alternative: return an error when reporting is off

`enqueue_input(Input::Mouse(..))` could fail when the child did not enable
mouse reporting. Rejected: it would turn a routine "the child is not
mouse-aware" case into a `Result` the caller has to handle, and it makes the
common forward-everything pattern require a mode check anyway. Emitting nothing
keeps the call total, which is what the other inputs are.

### Alternative: model only the SGR encoding (require `?1006`)

Modern guests overwhelmingly enable `?1006`. Rejected: `mouse_sgr` is already a
tracked mode precisely because it can be off, and a guest that enables only
`?1000` would silently receive nothing. Supporting both is a small amount of
code and matches the mode the crate already stores.

## Unresolved questions

- **Does `MouseEventKind` need a separate focus/past-events notion?** xterm's
  `?1004` (focus) and the `SGR-Pixels` (`?1016`) coordinate space are not
  modelled. Out of scope here; `mouse` is the only mouse mode the emulator
  stores today.

The four points this section used to raise are settled in the text above:
reporting off emits zero bytes and the call stays total (so a caller may
forward unconditionally or check `modes().mouse.is_active()` itself); the
legacy coordinate clamp is `223` and is documented rather than made an error;
`X10` does not report the wheel; and `Motion { button: Option<..> }` keeps the
held button in one variant, with the caller tracking it.

## Future possibilities

- **Pixel coordinates (`?1016`)** would add a second position space; the
  `MouseEvent` shape (a `Position`) would have to grow a variant or a companion
  type at that point.
- **Focus events (`?1004`)** are another mode the emulator could track, and
  once it does, `Input::Focus(bool)` is the natural mirror of this proposal.
- If a second caller needs a different event set (for example, only wheel for
  scrolling), `MouseEventKind` can grow variants without changing the `Input`
  shape, because the encoder is a pure function of the event and the modes.
- The same "logical input encoded from modes" pattern already covers keys and
  paste; mouse completes the set the host can produce, leaving only protocol
  replies (the terminal answering `CSI ... t` queries), which are a different
  concern.
