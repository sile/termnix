# RFC: Rename the OSC request type to ChildRequest

- Status: accepted

## Summary

Rename `OscRequest` to `ChildRequest` and its accessor `take_osc_request()` to
`take_child_request()`. No variant names change: `SetClipboard { text, selection,
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
`ChildRequest` names that property directly: the child sent it, and the caller
is asked to carry it out. It also reads correctly for the variant that carries
a sequence the crate did *not* interpret: an unmodelled OSC still came from the
child, and still needs somewhere to go.

`Request` was the word kept from the old name; only the framing prefix was
wrong. The obvious worry is that a *request* reads as an *ask* - politely put,
and optionally declined - while part of the channel is a command rather than an
ask: a child that sends RIS has commanded the terminal to reset, and the caller
is expected to obey. That worry does not survive contact with how `request` is
actually used. HTTP's `request` covers `GET` and `DELETE` alike: a request is
anything a client sends that the server is to carry out, whether it reads state
or changes it, whether it is phrased as a question or an order. The modality is
carried by the method, not by the word. The same holds here: the channel is
*what the child asks the caller to do*, and whether a given variant reports,
commands, or queries is the variant's business, not the type's. Naming every
value a `Request` is more honest than inventing a neutral word for a direction
that is already clear.

`ChildEvent` was the other candidate and was rejected. It names the sender,
which is right, but `event` carries the opposite commitment from `request`: an
event is something that *happened*, which the receiver observes and cannot
decline, whereas this channel is something the child *sent* for the caller to
*act on*. The child does not simply cause a state change the crate then reports
(a repaint, a resize); it delegates an action the crate cannot perform because
it does not own the resource. Outside this crate the word is further stretched:
an event stream is normally taken to include the child's own lifecycle - it
exited, it was signalled - and none of that belongs on this channel. `Request`
says who acts; `event` does not.

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
while let Some(request) = term.take_child_request() {
    match request {
        ChildRequest::SetClipboard { selection, text, append } => {
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
pub enum ChildRequest {
    SetClipboard {
        text: Vec<u8>,
        selection: ClipboardSelection,
        append: bool,
    },
}

impl TerminalState {
    pub fn take_child_request(&mut self) -> Option<ChildRequest>;
}
```

`ClipboardSelection` keeps its name: it is a value type, not part of the
channel, and it describes an OSC 52 selection rather than a child message.

The `TerminalState` field that holds the queue (`osc_requests`) is
`pub(crate)` and is renamed to match (`child_requests`), as is the helper that
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
- **`ChildEvent`.** Names the sender but commits to the wrong modality: an
  event is observed, not carried out, and the word drags in the child's own
  lifecycle (it exited, it was signalled), which is not on this channel. See
  the motivation. `ChildRequest` keeps the child prefix and says who acts.
- **`ChildMessage`.** A neutral word that would cover both a command and an
  ask, on the argument that no single word fits both. Rejected as hedging: the
  direction is already clear (child to caller) and HTTP shows `request` needs
  no hedge to cover a command. A word chosen only to avoid choosing says less
  than `ChildRequest`, which has a counterpart (`Response`) when a reply path
  is added.
- **`ChildInput`.** `ChildInput` would collide with the *input*
  side of the crate (the bytes the host sends to the child), which is the
  opposite direction. Rejected to keep "input" meaning one thing.
- **`TerminalRequest`, `Request`, or a framing-neutral `Control`.** Each drops
  the sender. The value of the name is that it says who sent it and therefore
  who the caller is; a generic name that could describe any message in the
  crate says less than `OscRequest` did.
- **A bare `Request` or `Event`.** With no `Child` to bound it, it reads as a
  request to or an event of the emulator itself (a repaint, a mode change)
  rather than one the child sent, and this crate already has an internal notion
  of events (a `feed` may change visible state) that must not be conflated.
- **Rename the accessor only, leave the type.** Rejected: the two are one
  contract, and half a rename reads as an oversight rather than a decision.

## Unresolved questions

- **Is `ChildRequest` too broad, in the end?** It admits anything the child sent
  that the caller acts on, which currently reaches one degree past the OSC
  family (a reset). It does not admit the crate's own outputs (the reply buffer,
  which the child did not send) nor the host's inputs (the keys the host
  writes to the child), so it stays honest today. Whether a future case that is
  "from the child" but "not something the caller acts on" (a sequence that is
  deliberately dropped) would strain the name is open; the passthrough proposal
  answers it by giving every unmodelled sequence a destination, so the dropped
  case is meant to disappear rather than grow.
- **Does `Request` overclaim when a variant reports rather than asks?** A prompt
  mark (OSC 133) or a working-directory report (OSC 7) tells the caller
  something rather than asking for it, and a `Request` noun is a slight stretch
  there. It is left as the type name anyway: the stretch is smaller than for
  `Event` (which would misfit the reset and the clipboard write), and the
  alternative is a neutral word that says less. Open whether a later revisiter
  agrees once the reporting variants exist.
- **Should the private field and helper rename too?** The field (`osc_requests`)
  is `pub(crate)` and can follow the type for one more line of consistency, but
  the tokenizer helper is named for the number it parses (`osc_clipboard`), not
  for the channel it feeds, so it can stay. Which private names move is open.
- **Does `ClipboardSelection` want a rename for symmetry?** It is the one
  remaining public name in the series still built on "clipboard" and "OSC 52".
  Left alone here because it names a value, not the channel, and a value is
  allowed to be specific. Open whether a later revisiter agrees.

## Future possibilities

- A reset proposal adds `ChildRequest::Reset`, which is the case that
  motivated this rename. With the name fixed first, that proposal only adds a
  variant.
- A passthrough proposal adds `ChildRequest::OtherOsc { id, params }` for an
  unmodelled sequence. The variant keeps the `Osc` framing because, unlike the
  rest of the channel, its meaning depends on it (it is the OSC sequence the
  crate did not interpret), while the type name carries the direction.
- If the crate ever grows a second, non-child stream (an internal request the
  caller observes but the child did not cause), the `Child` prefix is what
  keeps the two from being confused, and is a reason to keep the prefix rather
  than fall back to `Request`.

## Outcome

Implemented in [#17](https://github.com/sile/termnix/pull/17) (merged as `9f676cb`).

The rename landed as proposed. `OscRequest` is `ChildRequest` and `take_osc_request()` is `take_child_request()`, with no variant names touched: `SetClipboard { text, selection, append }` keeps its spelling and its fields, and the public surface changes in name only.

The private queue field followed the type (`osc_requests` to `child_requests`), which the RFC listed as open. The tokenizer helper stayed `osc_clipboard`: it is named for the number it parses rather than the channel it feeds, so it is not part of the channel's contract and renaming it would tie a private name to a public one for no reader's benefit.

One open question was settled by the landing itself rather than left as a branch. The reset RFC had asked which name to write against, assuming it might land either before or after this one; once this rename was merged, that question had one answer, so the reset RFC now states the settled names instead of carrying the stale alternative.

The reasoning in the Motivation held up unchanged: the name is about direction, not framing, and HTTP's `request` covers a command as readily as an ask, so no variant needed a hedge. `ChildEvent` and `ChildMessage` stayed rejected.

Nothing else moved. The reset delivery and the passthrough variant that motivated the rename are still their own proposals; this change adds neither, and the queue's reset behavior (`soft_reset` clearing it) is untouched.

The scope is unchanged from what is described above.
