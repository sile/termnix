# RFC: Pass unmodelled OSC sequences through as requests

- Status: draft

## Summary

Give every OSC identifier the crate does not interpret a place to go, by
emitting it on the child-request channel as a `ChildRequest::OtherOsc` carrying
the identifier and the raw arguments. Today such a sequence is silently dropped
by the `_ => {}` arm in `osc_dispatch`, which is the one outcome that loses the
sequence for every caller. This RFC is the proposal the OSC handling policy
names as the fallback for an unmodelled number; it adds one variant to the
request type and turns the drop arm into an emit arm. It deliberately does not
add a "drop" option to the type: dropping is what happens when a sequence has
no destination, and after this change no sequence is in that position by
default.

## Motivation

A host application that embeds a child session sits between two parties that
both speak OSC: the child that writes sequences into the PTY, and the host
terminal that would act on or answer them. The host renders the child's output
through `TerminalState`, so the crate is the tokenizer in the middle. For the
two identifiers it models, the crate hands the value to the host. For every
other identifier, `osc_dispatch` falls through to `_ => {}` and the sequence
is gone before the host has a `TerminalState` to ask.

The consequences are concrete. A child that announces its working directory
(OSC 7, which many prompts emit before every command) or brackets its prompt
(OSC 133, which lets a host tell a prompt from command output) is telling the
host something the host may well want to act on, and the host has no way to
see it. A vendor extension a host wants to support has the same problem. The
only recourse is to re-tokenize the PTY stream beside `vte`, duplicating the
framing rules the crate already applies - the exact work a caller uses this
crate to avoid.

This is not a request for the crate to understand more sequences. Most of them
should stay opaque. The problem is that "opaque" is currently spelled
"discarded", and discarded is not a neutral default: it is the one choice that
withholds the sequence from every caller, including the caller that wanted it.
An unmodelled sequence should be offered, and the caller should decide whether
to use it.

## Guide-level explanation

Before, a host that wanted to react to an OSC number the crate does not model
could not:

```rust
// The child wrote ESC ] 7 ; file://host/home/user ST
// There is nothing to call. The sequence was consumed by the tokenizer and
// dropped inside `osc_dispatch`.
```

After, the sequence arrives on the same event stream as the clipboard request,
and the host drains it like any other event:

```rust
while let Some(event) = term.next_event() {
    if let Event::RequestReceived(request) = event {
        match request {
            ChildRequest::SetClipboard { text, selection, append } => {
                // the crate decoded this one for us
                set_clipboard(selection, &text, append);
            }
            ChildRequest::OtherOsc { id, params } => {
                // the crate handed this one over uninterpreted
                if id == b"7" {
                    set_working_directory(params.first().map(Vec::as_slice));
                }
                // anything else: decide, or ignore
            }
            // other requests
        }
    }
}
```

The shift in thinking is that the channel has two kinds of value on it.
One is *interpreted*: the crate recognized the number, decoded its arguments,
and produced a typed request (`ChildRequest::SetClipboard`). The other is
*uninterpreted*: the crate recognized only where the sequence ended and where
its fields were, and produced the identifier and arguments as bytes
(`ChildRequest::OtherOsc`). In both cases the caller is the one that acts; the
difference is how much the crate did before handing it over.

A host that wants none of this ignores it. The channel is drained the same way
it already is, and an `OtherOsc` a caller does not match on is something it
simply does not handle - which is what it does today, except that deciding
*not* to handle it is now the caller's decision rather than the crate's.

## Reference-level explanation

### The variant

```rust
pub enum ChildRequest {
    /// The child asked to change a selection (OSC 52).
    SetClipboard {
        text: Vec<u8>,
        selection: ClipboardSelection,
        append: bool,
    },
    /// An OSC sequence the crate does not interpret, handed over as-is.
    ///
    /// `id` is the identifier field and `params` the argument fields, both
    /// exactly as the tokenizer split them. Nothing has been decoded.
    OtherOsc { id: Vec<u8>, params: Vec<Vec<u8>> },
}
```

`OtherOsc` is the one variant whose name carries no action: the crate did not
interpret the sequence, so it cannot say whether the child asked for a change,
asked for contents, or merely reported something. An action-led name would be
a guess the crate is not in a position to make, so the variant stays a noun.

It is `OtherOsc`, not a bare `Other`, because on a type called `ChildRequest`
`Other` would read as "any other child request" and lose the one thing that
bounds it: an unmodelled OSC is still an OSC. The rest of the channel does not
need the framing in its variant names - `SetClipboard` does not name `Osc` -
because the type name carries the direction and the variant carries the action.
This variant is the exception: its action *is* "an OSC the crate did not
interpret", so the framing is the only part of its meaning that the name can
keep. A reset is reported elsewhere, as `Event::TerminalReset`, because it is a
change of terminal state rather than a request, so `OtherOsc` is the only
variant here whose name has to say what frame it came from.

`id` is `Vec<u8>`, not a number and not `String`. Not a number because a
number is only conventional: a vendor extension may use an identifier that is
not a decimal integer, and parsing to an integer would either fail or lie
about what arrived. Not `String` because the field is arbitrary bytes the crate
has not validated as UTF-8, and forcing it through `String` would either lose
data or turn a decoding failure into a silently missing request. The caller
that knows its own identifier compares bytes, as in the guide example.

### The split, and why the crate does it

`vte` hands `osc_dispatch` the sequence already split on `;`:
`params[0]` is the identifier and `params[1..]` are the arguments. Today's
`osc_clipboard` already relies on this - it reads `params.get(1)` for the
selection and `params.get(2)` for the payload. The variant carries that same
split through to the caller.

```rust
// inside osc_dispatch, replacing the current `_ => {}` arm
_ => self.events.push_request(ChildRequest::OtherOsc {
    id: params[0].to_vec(),
    params: params[1..].iter().map(|p| p.to_vec()).collect(),
}),
```

Splitting by `;` is *framing*, not interpretation. The crate already knows
where an OSC's fields end - that is what makes it a tokenizer - so passing the
fields on whole is not a policy decision the caller cannot see. Passing a
single raw byte string instead would push the same split onto every caller,
who would each re-implement it from the same rule the crate already applies.
That is the extra caller-side code the crate exists to remove.

The crate still does *not* interpret the arguments: it does not know that
OSC 7's first argument is a URI, that OSC 133's first argument is a letter
code, or that OSC 4's first argument is a colour index. It knows only where the
fields are. Everything past the split is the caller's to decode.

### What is *not* passthrough

An identifier the crate interprets never reaches this arm. `"0"` and `"2"`
stay title handling; `"52"` stays `osc_clipboard`. The `match` arms are the
list of interpreted numbers, and the doc comment on `osc_dispatch` should name
them so the boundary is readable in one place. A number that is modeled as
*state* later (a hyperlink, a palette entry) also leaves this arm at that point,
by the same reasoning: an interpreted sequence has a destination of its own.

### Read requests

An OSC 52 read request (`ESC ] 52 ; c ; ? ST`) is already dropped by
`osc_clipboard` before this RFC. This RFC does not change that path: the "52"
arm stays interpreted, so a read request never becomes an `OtherOsc`. Routing
read requests is a separate question the policy leaves open, and nothing here
prejudges it.

### Ordering

Requests are not merged, so an `OtherOsc` keeps its position relative to other
requests from the same `feed`. A caller that cares about OSC ordering (a
prompt mark arriving after output, say) sees the order the child wrote. The
pending-request buffer is unbounded, as it already is; an `OtherOsc` adds no
new bound question beyond the one that buffer has.

## Drawbacks

- **A variant that most callers ignore.** A host that only wants the clipboard
  now has an arm it does not use. Because the enum is exhaustive, it must name
  the variant rather than fall through, so the cost is visible at every call
  site even for a caller that does not care. This is the intended trade - the
  alternative is a `_ => {}` that hides the next variant too - but it is a
  real cost on the common caller.
- **The crate now copies bytes it may never be asked for.** Every unmodelled
  OSC is copied out of the tokenizer's buffers into owned `Vec`s held for the
  caller, whether or not the caller ever drains it. A caller that ignores
  `OtherOsc` pays for it in allocation and memory, not just in a match arm. How
  large that can get is a property of the child's output, and the buffer is
  unbounded.
- **Passthrough is not free of interpretation.** Choosing `id` + `params`
  means the crate commits to `;` being the field separator for *every* number,
  including ones whose grammar might treat `;` differently. `vte` already makes
  that commitment when it tokenizes, so this RFC inherits it rather than
  creating it, but the RFC should not pretend the commitment is not there.
- **A payload containing `;` cannot be represented.** By the same framing, an
  OSC whose argument legitimately contains `;` (a base64 payload never does, but
  an opaque vendor payload might) arrives already split into more fields than
  the sender intended. This is a property of the frame, not a bug this RFC
  introduces, but it bounds what passthrough can promise.

## Rationale and alternatives

- **Emit the whole sequence as one raw byte string.** `Other(Vec<u8>)` with
  the bytes from `ESC ]` to the terminator. Rejected: it makes every caller
  re-split on `;`, duplicating the framing the crate already applied, and it
  forces callers to re-scan for the identifier and terminator. The split is the
  crate's work to do once, not every caller's to redo.
- **Parse the identifier to a number (`id: u32`).** Rejected: identifiers are
  conventional, not numeric. A vendor extension with a non-integer identifier
  would either be dropped or have its identifier mangled, and both are worse
  than carrying the bytes.
- **Add a `Drop`/`Ignored` variant so a caller can choose to discard.**
  Rejected: there is no sequence known to be unsafe to deliver, and a caller
  that does not want a sequence ignores it after receiving it. A variant for
  "do nothing" would be a destination that exists only to be a no-op, and - the
  same problem the channel was built to avoid - it would give a future
  unmodelled number somewhere to vanish silently.
- **Keep dropping, but document why.** Rejected: the policy already rejects
  documenting a drop as the crate's answer to an identifier. A comment does not
  make the bytes reach the caller, and a documented drop is still a drop.
- **Model OSC 7 / OSC 133 as their own variants now.** Rejected for this RFC:
  both are sequences whose *arguments* the crate would have to interpret (a URI,
  a mark code), which is a different decision from handing them over whole.
  They are examples of what passthrough is *for*, not part of it. If they are
  ever modelled, they leave the passthrough arm then.
- **Do nothing.** The `_ => {}` arm stays, and unmodelled sequences are lost to
  every caller. This is the situation the policy exists to end; see Motivation.

## Unresolved questions

- **Is `OtherOsc` the right variant name?** It is the catch-all for a sequence
  the crate did not interpret, and unlike the rest of the channel its name
  carries no action because the crate cannot know the direction; `OtherOsc`
  keeps the framing, `Uninterpreted` or `Raw` do not. A naming question,
  settled at implementation time.
- **Should `params` be non-empty even when the sequence had none?** An OSC with
  an identifier and no arguments (`params` has one element) would carry an empty
  `Vec`. Whether that is worth a special case, or whether an empty `Vec` is the
  honest answer, is open.
- **Does the pending-request buffer need a bound now that it can carry
  arbitrary child bytes?** It was already unbounded; `OtherOsc` makes the
  growth easier to trigger (a child can emit unmodelled sequences freely) but
  does not change the question. It is the same bound question the buffer owns;
  if that one is settled with a bound, this variant inherits it.
- **Where do the argument bytes come from as a sequence grows?** Each field is
  copied into its own `Vec`. Whether a single allocation for the whole sequence
  plus offsets is worth it is an implementation detail, not part of this
  proposal.

## Future possibilities

- A host that wants a typed view of a common unmodelled sequence (a working
  directory, a prompt mark) can build it above the crate out of `OtherOsc`, and
  a later proposal can lift the common case into its own variant - which
  is what "model OSC 7 / OSC 133" would mean when it is wanted. This RFC makes
  that a caller-side matter rather than a blocking gap.
- The read-request routing the policy leaves open can be decided with
  passthrough already in place: a read request that is not answered from state
  is already representable to a caller, whether it arrives as an `OtherOsc`
  or as a variant of its own.
- The same "do not decode, just offer" question will come up for DCS the crate
  now ignores in `hook`/`put`/`unhook`. That is a separate frame with its own
  framing rules and is not covered here, but the shape of this decision is the
  shape that one will take.
