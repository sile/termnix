# RFC: Route clipboard reads to the caller

- Status: draft

## Summary

Answer an OSC 52 read request by handing it to the caller, not by dropping it
or by writing something into the reply buffer. A read request
(`ESC ] 52 ; <selection> ; ? ST`) asks the terminal to send a selection's
contents back to the child; the crate owns no clipboard, so it has nothing to
send, and today it returns silently. This RFC splits the `"52"` arm so that a
set still becomes a clipboard request (as it does now) and a read becomes a
request of its own, carrying the requested selection. The caller that owns the
clipboard is then the one that answers, and the crate never claims an answer it
does not have.

## Motivation

A host application that embeds a child session sits between the child and the
host terminal, and a host terminal is where the clipboard lives. A child that
wants to paste runs `osc52_read "c"` or its language's equivalent, which writes
`ESC ] 52 ; c ; ? ST` to the PTY and then reads the reply. In a terminal that
implements OSC 52, the terminal answers with a set sequence carrying the
selection's base64 contents. In a host that embeds this crate, the crate is the
terminal as far as the child can tell, so the child's read lands here.

The crate has no clipboard. It never has: an OSC 52 set is recorded and handed
to the caller, which is the side that owns the selection. That arrangement is
exactly why a read cannot be answered from state - the bytes the child is
asking for live above the crate - and it is why the current silent return is
wrong for the same reason the silent drop of an unmodelled sequence is wrong.
The sequence reached a terminal that will not answer it, and the one party that
could answer, the caller, was never told the question was asked.

The concrete failure: a child in a host terminal that does implement OSC 52 can
paste, and the same child in a host that embeds this crate cannot - not
because the host lacks a clipboard, but because the crate discarded the request
before the host saw it. The host cannot recover by looking at the output, since
the request produced none. The only way to fix it from outside the crate is to
re-tokenize the PTY stream beside the crate, duplicating the framing the crate
already applies.

This is not a request for the crate to grow a clipboard. The crate still owns
no selection and still answers nothing itself. It is a request to deliver the
question to the party that holds the answer.

## Guide-level explanation

Before, a child's paste request vanished:

```rust
// The child wrote ESC ] 52 ; c ; ? ST and is now waiting for a reply.
// `osc_clipboard` sees the `?`, returns, and stores nothing. The child waits
// until its own timeout. The host is never told a request was made.
```

After, the request arrives on the event stream, and the host - which owns the
clipboard - answers it:

```rust
while let Some(event) = term.next_event() {
    if let Event::RequestReceived(request) = event {
        match request {
            ChildRequest::SetClipboard { text, selection, append } => {
                // the child asked us to replace a selection
                apply_write(text, selection, append);
            }
            ChildRequest::GetClipboard { selection } => {
                // the child asked us for a selection's contents
                answer_read(session, selection);
            }
            // ... other variants
        }
    }
}
```

`answer_read` is the host's own code: it looks up the selection it owns, builds
an OSC 52 set sequence carrying those bytes, and writes it to the PTY master
through the session. The crate is not involved in the answer, and does not need
to be - the host already has a channel to the child.

The shift in thinking is that OSC 52 is now two messages in the same number.
A *set* is the child telling the caller what to put in a selection; a *read* is
the child asking the caller what a selection holds. They point in opposite
directions, so they are two variants rather than one value with a flag, and the
names lead with the direction: `SetClipboard` and `GetClipboard`. A host that
only forwards sets ignores the get variant; a host that also serves pastes
handles both.

A host that wants neither ignores both, as it already ignores anything it does
not match. The crate still never writes a reply on its own.

## Reference-level explanation

### Splitting the `"52"` arm

`osc_dispatch` keeps `"52"` as an interpreted number; what changes is that the
arm no longer has a single outcome. The selection is resolved first (as
`osc_clipboard` does now), then the payload decides the message:

```rust
// inside osc_dispatch
"52" => self.osc_clipboard(params),
```

`osc_clipboard` resolves `selection` from `params[1]` exactly as today, then:

```rust
let payload = params.get(2).copied().unwrap_or(b"");
if payload == b"?" {
    self.events.push_request(ChildRequest::GetClipboard { selection });
    return;
}
```

The current `if payload == b"?" { return; }` becomes the push above. Everything
after it (the `+` append split, the base64 decode, the push) is unchanged and
still produces the set request.

### Where each number goes

OSC 52 is the first number whose arm produces more than one kind of message, so
this is where the dispatch outcomes are worth stating together. The whole rule:

| What arrives | The crate does | The caller sees |
| --- | --- | --- |
| OSC 52, payload `?` | recognizes a read, resolves the selection | `GetClipboard { selection }` |
| OSC 52, any other payload | decodes the base64, applies the `+` | `SetClipboard { text, selection, append }` |
| A number the crate interprets (title, clipboard) | decodes it | the variant for that number |
| Any other number | splits the fields, decodes nothing | `Other { id, params }` |

The middle two rows are the same rule seen twice: **a number the crate
interprets has a destination of its own, and never reaches the passthrough
arm.** Only the last row is uninterpreted. OSC 52 does not move between rows
when its payload is `?` - it stays an interpreted number, and only the message
the arm produces changes. That is why a read is a typed variant here and not an
`Other` with id `b"52"`: the crate did interpret it, and the type says so.

### The variant, and the write it pairs with

```rust
pub enum ChildRequest {
    /// The child asked to replace a selection (OSC 52 set).
    SetClipboard {
        text: Vec<u8>,
        selection: ClipboardSelection,
        append: bool,
    },
    /// The child asked for a selection's contents (OSC 52 read).
    ///
    /// The crate owns no clipboard, so it cannot answer this and writes
    /// nothing to the reply buffer. The caller that owns the selection is the
    /// one that answers, by writing an OSC 52 set sequence to the PTY master.
    GetClipboard {
        selection: ClipboardSelection,
    },
}
```

The two are struct variants, so each carries exactly the fields it has: a set
has `text` and `append`, a get has neither. A shared
`ClipboardContents { text, selection, append }` with a `read: bool` would let a
caller construct `read: true` with a `text` and an `append`, values that mean
nothing for a read; the type would permit states the protocol cannot produce.
The direction belongs in the variant, not in a flag.

`append` is meaningful only for a set - it is the `+` prefix on a base64
payload - so it lives on the set variant. A read payload is the single byte
`?`, so there is nothing to split into fields beyond `selection`.

### Why not the reply buffer

The reply buffer (`pending_reply_bytes` / `advance_reply_bytes`) carries answers
the crate *can* give from its own state: a cursor position report, a terminal
status, a device attributes reply. Each is assembled from `cursor`, `modes`,
and friends - the state the crate holds. A clipboard read is the opposite case:
the answer is state the crate does not hold and cannot compute. Routing it into
the reply buffer would either answer nothing (the buffer would never gain bytes,
and the child would still wait) or answer with a value the crate invented, which
is worse than silence because it is wrong and the child cannot tell.

So a read request contributes nothing to `pending_reply_bytes`, and the reply
buffer's contract is unchanged: it is still only what the crate knows.

### Why not passthrough

A read request is not an unmodelled sequence. The crate recognizes `52`, knows
`?` is a read, and knows which selection was asked for; that is interpretation,
and it produces a typed value with a name for the message. Passthrough exists
for numbers the crate does not interpret, and if a read arrived as a raw
identifier and arguments, every caller would have to learn the `?` convention
and re-derive the selection from bytes - the framing work the crate already
did. Because `52` stays an interpreted number, a read never reaches the
passthrough arm.

### Effect on visible-change detection and state

None in either direction. A read request changes no cell and no mode, so no
`Event::ScreenUpdated` is reported; like a set, it draws nothing. And a read is
a request, not state, so there is nothing to clear on a reset (the set side has
a stored request to clear, which is why that side is touched in
`reset_child_state`; a read is delivered once through
`Event::RequestReceived` and is gone).

### Ordering

A read is delivered on the same event stream as everything else, so a read that
follows a set in one `feed` arrives after it, and a caller that both applies
sets and answers reads sees them in the order the child wrote them.

## Drawbacks

- **A variant that a host without a clipboard must handle.** The enum is
exhaustive, so a caller that only forwards writes now names a read variant it
ignores. This is the cost every variant of the channel has, and the trade is
that the request is visible at all.
- **The crate now emits a read with no obligation behind it.** Nothing in the
crate makes the caller answer; a host that ignores `GetClipboard` leaves the
child waiting, which is where it was before, but the host now has to make that
choice knowingly rather than by the crate's silence.
- **A correct answer is entirely the caller's to build.** The crate hands over
a selection and stops; building the OSC 52 set sequence, encoding the base64,
and writing it to the PTY is caller code. That is by design - the crate owns no
clipboard - but it is more than a getter would be, and a host that expected the
crate to answer will not find one.
- **One number now has two directions in the source.** `osc_clipboard` used to
end in a return for `?`; it now branches into two requests. The function is
slightly less linear, and the split point (`payload == b"?"`) is the contract
that decides direction, so it is worth a comment saying so.

## Rationale and alternatives

- **Write an empty or error reply into the reply buffer.** Rejected: the crate
  has no contents to send, so the only available replies are an empty selection
  (wrong - the caller may own contents) and an explicit failure (a reply the
  crate invents rather than knows). The child cannot distinguish a wrong answer
  from a right one, which is worse than no answer.
- **Route the read into the reply buffer and let the caller fill it.** Rejected:
  the reply buffer's contract is that its bytes are ready to write and are the
  crate's answer. A buffer the caller must *backfill* before it is valid inverts
  the contract, and the caller already has its own write path to the PTY master
  through the session, so the buffer would add a second path to the same place.
- **Emit the read as `Other` with id `b"52"`.** Rejected: it works, but
  forces every caller to compare `id == b"52"` and inspect `params[1]` for `?`,
  and it presents `52` as an uninterpreted number when the crate did interpret
  it - it decided this is a read and resolved the selection. A typed variant
  keeps that classification in the type instead of in each caller's guard.
- **Answer from a crate-owned clipboard.** Rejected in the sibling decision that
  introduced the clipboard request: the crate owns no selection, and giving it
  one would put a second source of truth beside the host's.
- **Keep dropping, but document why.** Rejected: a child that asks and is not
  answered waits on a reply that will not come, and the host that could answer
  is never told. A documented drop is still a drop.
- **Do nothing.** The read stays silently ignored; a child's paste fails in this
  host and works in a terminal that implements OSC 52, with no way for the host
  to notice. See Motivation.

## Unresolved questions

- **Is `GetClipboard` the right variant name?** It pairs with
  `SetClipboard`, and both lead with the direction. Every variant name on the
  channel follows the same action-led rule; see the container RFC for where
  that rule is recorded.
- **Should `GetClipboard` carry anything more than `selection`?** A get has
  only a selection today. If a future extension adds a field (a maximum size,
  say), the variant gains a field inline; the shape is chosen then, not
  pre-emptively.
- **Should the crate do anything if the caller never answers?** It cannot: it
  does not know the child is waiting, and it holds no reply to send. Settled by
  the implementation: the obligation is documented on the variant rather than
  left implicit. See the Outcome.

## Future possibilities

- A caller that owns several selections (a host with its own clipboard stack)
  can answer a get with any of them; the variant carries only *which* was
  asked for, so the policy stays with the caller.
- If the crate ever gains a reason to model the reply side of OSC 52 (a
  protocol-level timeout, say), this variant is the point where the answer would
  originate, and the reply buffer is where the bytes would go - but only once
  the crate actually knows the contents, which today it does not.
- The same split - one number, two directions, two variants - will come up
  wherever an OSC number has both a set and a query. This is the first such
  case, and it fixes the shape: direction is a property of the message, not a
  flag on a shared type.
