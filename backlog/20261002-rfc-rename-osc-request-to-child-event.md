# RFC: Rename the OSC request type to ChildEvent

- Status: draft

## Summary

Rename `OscRequest` to `ChildEvent` and its accessor `take_osc_request()` to
`take_child_event()`. No variant names change: `SetClipboard { text, selection,
append }` keeps its spelling and its fields.

The rename does not add behavior. It fixes a name that is about to become
wrong, and it does so while the type has exactly one variant, which is the
cheapest moment there will ever be: every later family (an unmodelled-sequence
passthrough, a window title, a clipboard read, a terminal reset) is a change to
this one enum, so each one added before the rename is another name to correct.

## Motivation

The type is named for one framing, OSC (`ESC ]`), but the channel it is meant
to carry is not limited to that framing. The family this series is about to add
is the proof: a terminal reset arrives as `ESC c` (RIS) or `CSI ! p` (DECSTR),
which are an escape sequence and a control sequence, not an operating system
command. Naming the channel `OscRequest` commits it to a spelling of the
problem - "one of these OSC numbers" - that the very next proposals do not fit.

The deeper reason the name is wrong is that the type's identity is not a
framing at all; it is a *direction*. Every value on this channel is something
the child sent into the PTY that the crate has interpreted but cannot finish on
the caller's behalf, because the crate does not own the resource it names (a
clipboard, a title, a palette). That property is about who sent it and who must
act, not about whether the bytes were introduced by `ESC ]`, `ESC`, or `CSI`.
`ChildEvent` names that property directly. It also reads correctly for the
variant that carries a sequence the crate did *not* interpret: an unmodelled
OSC still came from the child, and still needs somewhere to go.

The name was chosen over `ChildRequest`, which was the first candidate. Both
name the sender, but `request` implies the message is an ask the reader may
accept or decline, and half the channel is not an ask. A clipboard write is a
request in that sense; a reset is not. A child that sends RIS has commanded the
terminal to reset, and the caller is expected to obey rather than consider.
`ChildEvent` covers both without promising a modal shape that only part of the
channel has.

## Guide-level explanation

Before, a caller wrote the channel out of the crate:

```rust
while let Some(request) = term.take_osc_request() {
    match request {
        OscRequest::SetClipboard { selection, text, append } => {
            host_clipboard.set(selection, &text, append);
        }
    }
}
```

After, the same code names where the value came from:

```rust
while let Some(event) = term.take_child_event() {
    match event {
        ChildEvent::SetClipboard { selection, text, append } => {
            host_clipboard.set(selection, &text, append);
        }
    }
}
```

The body of the loop does not change; only the two names do. The variant, its
fields, the order requests drain in, and the fact that taking one consumes it
all stay as they are.

## Reference

```rust
pub enum ChildEvent {
    SetClipboard {
        text: Vec<u8>,
        selection: ClipboardSelection,
        append: bool,
    },
}

impl TerminalState {
    pub fn take_child_event(&mut self) -> Option<ChildEvent>;
}
```

`ClipboardSelection` keeps its name: it is a value type, not part of the
channel, and it describes an OSC 52 selection rather than a child message.

The `TerminalState` field that holds the queue (`osc_requests`) is
`pub(crate)` and is renamed to match (`child_events`), as is the helper that
fills it in the tokenizer (`osc_clipboard` becomes something less OSC-bound
only if a later RFC moves it; this RFC leaves the private names alone, since a
private name costs a caller nothing and this RFC is about the public one).

## Alternatives considered

- **Keep `OscRequest`, and let the next proposal add a reset variant anyway.**
  The smallest change today, and the reason the rename is arguably not urgent.
  Rejected because the reset proposal is already written and it is the case
  that falsifies the name; renaming once, now, is cheaper than renaming after
  the enum has grown, and leaving the name wrong writes the wrong premise into
the next proposal's own text.
- **`ChildRequest`.** Names the sender but not the modality. A reset is not a
  request. See the motivation; the two words differ on whether the caller may
  decline, and on this channel they may not always.
- **`ChildEvent` vs `ChildInput`.** `ChildInput` would collide with the *input*
  side of the crate (the bytes the host sends to the child), which is the
  opposite direction. Rejected to keep "input" meaning one thing.
- **`TerminalRequest`, `Request`, or a framing-neutral `Control`.** Each drops
  the sender. The value of the name is that it says who sent it and therefore
  who the caller is; a generic name that could describe any message in the
  crate says less than `OscRequest` did.
- **A bare `Event`.** Rejected earlier in this series: with no `Child` to bound
  it, it reads as an event of the emulator (a repaint, a mode change) rather
  than one the child caused, and this crate already has an internal notion of
  events (a `feed` may change visible state) that must not be conflated.
- **Rename the accessor only, leave the type.** Rejected: the two are one
  contract, and half a rename reads as an oversight rather than a decision.

## Unresolved questions

- **Is `ChildEvent` too broad, in the end?** It admits anything the child sent
  that the caller processes, which currently reaches one degree past the OSC
  family (a reset). It does not admit the crate's own outputs (the reply buffer,
  which the child did not send) nor the host's inputs (the keys the host
  writes to the child), so it stays honest today. Whether a future case that is
  "from the child" but "not something the caller processes" (a sequence that is
  deliberately dropped) would strain the name is open; the passthrough proposal
  answers it by giving every unmodelled sequence a destination, so the dropped
  case is meant to disappear rather than grow.
- **Should the private field and helper rename too?** The field (`osc_requests`)
  is `pub(crate)` and can follow the type for one more line of consistency, but
  the tokenizer helper is named for the number it parses (`osc_clipboard`), not
  for the channel it feeds, so it can stay. Which private names move is open.
- **Does `ClipboardSelection` want a rename for symmetry?** It is the one
  remaining public name in the series still built on "clipboard" and "OSC 52".
  Left alone here because it names a value, not the channel, and a value is
  allowed to be specific. Open whether a later revisiter agrees.

## Future possibilities

- A reset proposal adds `ChildEvent::HardReset` (and later `SoftReset`), which
  is the case that motivated this rename. With the name fixed first, that
  proposal only adds a variant.
- A passthrough proposal adds `ChildEvent::Other { id, params }` for an
  unmodelled sequence. The name `Other` is easier to justify on a type called
  `ChildEvent` ("some child event the crate did not classify") than on
  `OscRequest`, where it invites the question of what an unmodelled OSC even
  means.
- If the crate ever grows a second, non-child event stream (an internal event
  the caller observes but the child did not cause), the `Child` prefix is what
  keeps the two from being confused, and is a reason to keep the prefix rather
  than fall back to `Event`.
