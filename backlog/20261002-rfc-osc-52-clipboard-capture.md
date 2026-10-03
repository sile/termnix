# RFC: Record the OSC 52 clipboard sequence instead of dropping it

- Status: draft

## Summary

Teach [`TerminalState`](../src/terminal.rs) to recognize OSC 52 ("manipulate
selection data") and retain the selection data it carries, exposed through
`take_clipboard()` beside `title()`. Today an OSC 52 sequence is swallowed
without a trace, and `TerminalState` exposes no clipboard at all. The change is
on the *reading* side of the same protocol that a host application writes on the
*writing* side.

## Motivation

A host application embeds a child session: it drives a PTY and renders the
child's output through `TerminalState`. When that child cuts text it writes
OSC 52 to its PTY, and `TerminalState` is the thing parsing the bytes. What the
child asked for is "put this on the selection named `c`" - a request for the
host to reach the host terminal's clipboard, which the host is the only party
that can do, because the host is the one that owns the host terminal.

That request is currently discarded. `osc_dispatch`
([`src/terminal_emu.rs`](../src/terminal_emu.rs)) matches OSC `0` and `2` and
ignores every other identifier, which is the documented behavior: the OSC list
in `TerminalState`'s rustdoc ends at "OSC 0/2: window title (stored)", and the
first line of `osc_dispatch` says the other numbers "are ignored so their
payloads never appear as printable text". So the request is not a bug in
termnix - the behavior cannot be defended as *sufficient* yet, but it is
faithful to what the crate documents.

The consequence is a dead end at a boundary termnix already straddles. The
in-process half is done: a cut in the child reaches termnix as bytes, gets
tokenized, and is discarded at the last step. The out-of-process half is the
writer's job (the host library that owns the terminal) and does not need
termnix's help - but a *test* of that writer does. `termnix` is the crate that
parses PTY output, so a consumer that writes OSC 52 and wants to assert the
base64 on the wire decoded back to the text it meant to put on the clipboard
has two options:
parse the bytes itself in the consumer's test suite, or have `TerminalState`
decode them. Only the second is reusable, and only the second keeps the base64
knowledge in the crate that owns terminal sequences.

termnix is not in the business of asserting that a child's clipboard write
*worked*, and this RFC does not try to make it do so. The child asks; the host
owns the clipboard; the host's outcome is unobservable (see "Failure is not
observed"). termnix's part is only to *see* the request at all, so the host
that owns the clipboard has something to act on, and so a test has something to
check.

## Guide-level explanation

The set of sequences termnix understands grows by one, and for the first time
it grows on the side that is normally the answer rather than the question. OSC
52 is an application asking the terminal to change its clipboard; termnix is
the terminal, so it records the ask. It is the same shape as the window title:
the application writes a sequence, termnix keeps the value, a caller reads it
type by type.

```rust
let mut t = TerminalState::new(size);
// A child requests the system clipboard be replaced with "hello".
t.feed(b"\x1b]52;c;aGVsbG8=\x1b\\");
assert_eq!(t.take_clipboard().unwrap().text, "hello");
```

After a feed, a caller calls `take_clipboard()` and either acts on it (writes
the text to the host terminal through whatever library owns that terminal) or
does nothing. What it must not do is assume the text is still there later -
the accessor is a take, so the value is consumed once, and a second call
returns `None` until the child asks again. The take is what keeps "the child
asked once" from turning into "the caller acts once per repaint".

A caller that would rather look than take has no other accessor: there is no
"current clipboard content" in termnix, because there is nothing true to report
after a take and a child that never asks. `take_clipboard()` returns the
request while it is pending, and `None` once it has been read or once the
request was too large to keep.

Nothing a child does today changes meaning. A program that sets the window
title keeps setting it; a program that writes an OSC 52 sequence keeps getting
it swallowed on every terminal that does not implement it, and now gets it
*kept* on termnix. The scope is deliberately the sequence only: no key binding,
no paste path, and no new way for a user to reach a clipboard inside termnix
itself.

## Reference-level explanation

### The sequence

OSC 52 arrives as `ESC ] 52 ; <Pc> ; <Pd> ST`, where `vte` hands
`osc_dispatch` the parameters split on `;`:

- `params[0]` is the identifier `b"52"`, matching the existing
  `std::str::from_utf8(params[0])` check beside `"0"` and `"2"`.
- `params[1]` is the selection: `c` is the system clipboard, `p` the primary
  selection. A missing selection (`ESC ] 52 ; ; <Pd> ST`) means `c` per the
  xterm convention.
- `params[2]` is the payload, whose default form is base64. The literal `?`
  means a *read request* ("send me the selection") rather than a write, and some
  terminals accept a non-base64 payload; termnix decodes base64 and ignores
  what does not decode, which makes `?` a non-request that stores nothing.

The selection argument lands in the retained value rather than being filtered
out, so `c` and `p` are told apart by the caller instead of being silently
collapsed:

```rust
/// A selection an OSC 52 sequence addressed.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum Selection {
    /// The system clipboard (`c`), also what a missing selection means.
    Clipboard,
    /// The primary selection (`p`).
    Primary,
    /// A selection name termnix does not model, kept as written.
    Other(String),
}

/// An OSC 52 clipboard request, retained for the caller to take.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ClipboardRequest {
    /// Decoded selection text.
    pub text: String,
    /// Which selection the application addressed.
    pub selection: Selection,
    /// Whether the application asked to append instead of replace.
    pub append: bool,
}
```

A plain `String` is used rather than a small-string type: the point is that an
unmodeled selection name survives instead of degrading to `Other` with the name
dropped, and selection names are short enough that the extra allocation is not
worth a dependency or a hand-rolled type.

`Other` is kept from the first version even though `c` and `p` are the only
selections termnix models. Collapsing to those two now and adding `Other` later
would make a caller's existing `match` on `Selection` non-exhaustive, so the
version that keeps the name is the one that stays compatible; the cost is a
`String` in a type most callers will match on two arms and ignore.

`append` is the append sign: a `+` prefix on the payload
(`ESC ] 52 ; c ; +<b64> ST`) is xterm's request to append. It is not a termnix
concept - it is the child's own distinction, and it is recorded verbatim rather
than collapsed, so a caller mirroring the cut into the host clipboard can
mirror an append as an append. An empty payload (`ESC ] 52 ; c ; ST`) is the
"clear the selection" form: it decodes to the empty string, which termnix keeps
as a request with empty `text` rather than as nothing, so the caller can tell
"clear it" apart from "never asked". The two are different: `None` from
`take_clipboard()` means nothing is pending, and `Some(request)` with empty text
means the child asked for an empty selection.

### The accessor

```rust
impl TerminalState {
    /// Takes the pending OSC 52 request, if any.
    ///
    /// Returns the request and clears it, so one sequence is acted on once.
    /// Returns `None` when no request is pending or when the previous one was
    /// already taken. A request whose decoded text is empty is still returned,
    /// because an empty payload is the "clear the selection" form and is not
    /// the same as no request.
    pub fn take_clipboard(&mut self) -> Option<ClipboardRequest>;
}
```

`&mut self` is the unusual part and it is deliberate. Every other accessor on
`TerminalState` takes `&self` and returns borrowed state, because the values are
plain properties of the terminal. Clipboard content is not a property of the
terminal: termnix does not own a clipboard, the request is an event, and the
event is over once it has been read. A borrow-based `fn take_clipboard(&self) ->
Option<&ClipboardRequest>` would leave the caller with no way to say "I have
acted on this", so the same request would be re-sent on every later feed and
every repaint, which is precisely the hazard the take removes.

The name is `take_clipboard()` rather than a bare `clipboard()`: `title()` is a
non-destructive getter over state the terminal owns, while this accessor
consumes the event, and the `take_` prefix makes the mutation visible at the
call site even though the `&mut self` receiver already implies it. If the return
type ever trips a reader, the doc comment is the answer: taking is what makes
the accessor well-defined.

This is deliberately *not* modelled on the reply buffer
([`pending_reply_bytes()`](../src/terminal.rs) plus
[`advance_reply_bytes()`](../src/terminal.rs)), even
though both are "the terminal produced something for the caller". A reply is a
byte stream the caller may write only partially, so it wants a `&self` view and
an explicit consumed-length that can stop mid-stream. A clipboard request is a
single discrete event with no partial form: there is nothing to resume, only
"seen" or "not seen", so a take is the whole model and the advance machinery
would be a longer way to say the same thing. Keeping the two shapes apart is the
point, not an oversight.

One field is added to `TerminalState`:

```rust
pub(crate) clipboard: Option<ClipboardRequest>,
```

It is *not* part of `VisibleScalars` and does *not* bump `revision()`. A
clipboard request draws nothing, so it is not a visible change; a caller that
polls `revision()` to decide whether to repaint must not be told to repaint
because a child cut text. `clipboard` is also different from `title` in a way
that matters here: the title lives in the state and is compared by identity
through its change flag, while the clipboard field holds at most one pending
request and is read destructively. Nothing about the `Debug` impl needs care
beyond adding the field, since `ClipboardRequest` is `Debug`.

### Validity and cost

The decode is left alone where it is unreliable. Base64 is decoded with the
same standard alphabet every OSC 52 writer uses; a payload that does not decode
(an unmodeled alphabet, a truncated tail, the `?` read form) stores nothing and
sets nothing. Failing a decode must not produce a request with mangled text,
because the caller has no way to tell a mangled request from a real one.

There is no size cap beyond what the protocol itself imposes. A cap was
considered and dropped: the bytes travel over a local PTY between the child and
the process that embeds it, so termnix cannot be made to allocate more than the
child already allocated to write the sequence, and the cap that actually
matters belongs to whichever side owns a clipboard to protect. A host that
wants to reject an oversized cut can measure `text` after taking it, and a
child can bound what it sends; neither needs termnix to decide for it. Adding a
cap later is not a breaking change, while removing one would discard values
callers may already be handling, so the bounded choice is to leave it out until
something actually needs it.

One bound is implied by the protocol and stays: because `vte` splits OSC
parameters on `;`, a payload containing `;` cannot round-trip through
`osc_dispatch` in the first place. That is a protocol fact, not a choice, and it
does not need a separate check in the implementation: a sequence that cannot be
split cannot be decoded, and a payload that is not decodable stores nothing.
That delimiter is why OSC 52 cannot be used to push arbitrary bytes through the
parameter channel, which is worth saying in the doc comment so a future reader
does not try.

### Failure is not observed

OSC 52 has no reply. There is no sequence a caller can write to ask "did the
clipboard take the text?", and even DA1-style probing cannot report it, because
a terminal that accepts the bytes may still have no clipboard to write to (a
headless server, a terminal with the feature off) and a terminal under tmux may
forward the sequence only when its `set-clipboard` option allows. And termnix
is the *host* here rather than the application, so it has even less to observe:
the child's request is an app-side assumption whose result termnix can never
receive. `take_clipboard()` therefore reports what was asked for, never whether
it worked. This is the same reasoning that makes the writer silent, arriving
here from the other end: the protocol is write-only in both directions.

### What this buys a consumer

A host application's clipboard passthrough writes OSC 52 to the host terminal.
A test of that change has, without this RFC, only the raw output stream to
inspect, which asserts that some bytes were written. With this RFC, `termnix` is
the consumer in the test: feed the child's output through `TerminalState`, take
`take_clipboard()`, and assert the decoded text and the selection. That turns
"an escape sequence appeared" into "the sequence encoded the text that was
cut", which is the half worth testing.

That is also the whole of the dependency story. A consumer's *implementation*
does not need this RFC: how the host writes OSC 52 is that host library's
business, and termnix is not in that path. What depends on this RFC is a *test*
that asserts the text rather than the bytes. So this RFC is an enabler, not a
gate: the host can land its passthrough without it, and the test gets stronger
when it lands. It has no dependency in the other direction either.

There is also a direct use that does not involve a test at all, and it is the
reason the section above says "host" rather than naming an application: a
*nested* termnix, or any application that embeds a PTY behind `TerminalState`,
can now offer the same clipboard passthrough for the program it hosts that such
an application offers for the child it hosts. The bytes are already arriving;
this RFC is the step that keeps them long enough to forward.

## Drawbacks

- `osc_dispatch` grows a second identifier family and, with it, base64 decoding
  written by hand (termnix has no base64 dependency today, and pulling one in
  for this is not worth it). That is real new code in a file that is currently
  a switch over identifiers.
- The accessor takes `&mut self`, unlike every other getter on `TerminalState`.
  A reader must learn the one exception, and the exception is invisible in the
  call site's type. The alternative is worse (see above), but it is still a
  wart.
- termnix gains a concept it does not act on: it records a clipboard request
  and never does anything with it, because doing anything would mean owning a
  host terminal, which the crate deliberately does not. A future reader may
  reasonably ask why the emulator keeps data it cannot use.
- A child that writes OSC 52 and a caller that takes it now have a path where
  none existed, which is a new thing to get right (and to document as not
  being a real clipboard - a caller that takes a request and drops it is
  indistinguishable from a caller that never looked).
- The `;` delimiter means not all OSC 52 sequences are representable, and the
  failure is silent. Silent is consistent with the rest of the protocol's error
  model, but it does mean a caller cannot fully verify what it did not see.
- With no cap, a caller that takes a request is trusted to decide what to do
  with a very large one. That is more allocation than a bounded API would
  allow, and it is paid only by callers that ask for the value.

## Rationale and alternatives

- **Keep ignoring OSC 52; let the consumer parse the bytes.** This is today,
  and it is what a host's own test would have to do. It works, but it moves
  base64 and the selection and append syntax into the consumer's test suite, so
  the knowledge is duplicated exactly where the writer side argued it should
  not be, and no other consumer benefits. Rejected for the same reason that
  made the host library the writer rather than its caller.
- **Retain the raw base64 payload and let the caller decode.** This keeps the
  decoder out of termnix but hands every caller a protocol detail, and it means
  the invalid-payload cases cannot be rejected where the knowledge lives. The
  title precedent retains the *decoded* value, not the sequence; do the same.
- **Retain only a "last sequence seen" counter or a boolean.** Enough for a
  test to assert that OSC 52 was *delivered*, but not that the right text was,
  which is the assertion that has value. Rejected, and it is what the writer's
  RFC calls the weaker kind of test.
- **Answer a read request (`ESC ] 52 ; c ; ? ST`) with the retained selection.**
  There is nothing to answer *with*: termnix holds no clipboard, only the last
  pending request, and once that has been taken there is nothing true to
  report. Answering with a stale or empty selection would be worse than not
  answering, which is what the crate already does for DA2 and DA3 - a probe
  gets no reply rather than a wrong one. Out of scope, and probably never in
  scope.
- **Make the accessor a non-destructive getter, `clipboard(&self) -> Option<&ClipboardRequest>`.**
  Rejected: with no take, a caller cannot distinguish "not yet acted on" from
  "already acted on", so every later feed would re-deliver the request. The
  destructive read is not a stylistic choice, it is what makes the value have a
  lifetime.
- **Filter to the system clipboard and drop other selections.** Simpler, but
  it throws away information the child deliberately sent and leaves no room for
  the caller to decide. Record the selection name; let the caller filter.
- **Do nothing.** The cost is that the *only* way for a consumer to test an
  OSC 52 write is to reimplement the decoding, and the only way for a nested
  emulator to support a clipboard passthrough is to write its own tokenizer
  beside `vte`. termnix is the crate that already parses the bytes.

## Unresolved questions

- Whether a cap is ever needed is left open. It is deliberately absent (see
  above); if a real caller turns out to need one, adding a constant later is
  not a breaking change, and where the limit belongs is then a question with an
  actual answer rather than a guess. `Selection::Other` is *not* open; it is
  kept (see above).

## Future possibilities

- A read request (`?`) could be answered from a selection the *caller* seeds,
  turning the one-way protocol into a two-way one for as long as the caller
  keeps the text. That is a much larger proposal (state the caller owns, a
  reply channel, staleness) and is not implied by anything here.
- If termnix ever grows a host-terminal abstraction, a take here becomes the
  input side of it and OSC 52 passthrough stops needing a consumer at all.
- Other selections (primary, or the numbered cut buffers) become usable without
  another protocol change if `Selection` keeps their names.
- If a second family of consumed-once events appears (another sequence whose
  payload a caller takes rather than reads), the take-style accessors could
  collapse into a single one: an `Event` enum with a variant per family, and a
  single `take_event() -> Option<Event>` that a caller drains from one place
  instead of polling one method per family. It is not done here because one
  family does not justify the indirection, and the enum still grows a variant
  per family, which is the same work at the call site as a method per family.
  The reply buffer is not part of this: it is a byte stream, not an event (see
  The accessor).
