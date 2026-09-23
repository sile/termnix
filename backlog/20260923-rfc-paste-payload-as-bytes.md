# RFC: Let a paste payload be arbitrary bytes

- Status: draft

## Summary

Change [`Input::Paste`](../src/input.rs) so a paste can carry `&[u8]` instead of
`&str`, so that a caller can forward a paste the host reported as bytes without
silently dropping the parts that are not valid UTF-8.

## Motivation

tuke is a TUI soft keyboard that embeds one child process in a PTY and
forwards host input to it. It reads host input through `tuinix`, whose paste
event is deliberately reported as `Vec<u8>`: the terminal gives you bytes and
tuinix does not decide what they mean. tuke then has to hand those bytes to
`termnix::Session`.

`termnix::Input::Paste(&str)` cannot represent that. tuke currently works
around it by calling `std::str::from_utf8` on the paste and dropping the whole
paste when it fails:

```rust
let Some(text) = event.to_guest_paste() else { /* dropped */ };
```

The failure is not a corner case a user can avoid. Copying a block out of any
binary file, a file with Latin-1 bytes, or a UTF-8 sequence split across the
copy boundary produces a non-UTF-8 `Vec<u8>`, and the paste disappears with no
feedback. Worse, the loss is silent: the caller cannot even ask "did this
paste make it?" because there is no error to observe.

The mismatch is between two crates that were both designed carefully. tuinix
reports bytes because a terminal is a byte device; termnix takes `&str`
because it thinks of a paste as text. Neither is wrong on its own, but the
pair cannot be composed without a lossy conversion somewhere.

## Guide-level explanation

Today a caller with a `&[u8]` has to check UTF-8 itself and then accept that
some pastes cannot be sent:

```rust
// before: text-only, lossy for bytes
if let Ok(text) = std::str::from_utf8(&bytes) {
    session.enqueue_input(Input::Paste(text))?;
}
// else: no way to express the paste at all
```

After the change the paste is just bytes, and the existing mode handling is
unchanged: the bracketed-paste markers still depend on the child's current
mode at enqueue time, exactly as they do for text today.

```rust
// after: bytes, still mode-aware
session.enqueue_input(Input::Paste(&bytes))?;
```

A caller that already has a `&str` is unaffected; `text.as_bytes()` is one
added call, and it is explicit about the fact that a paste is bytes on the
wire.

## Reference-level explanation

Change the variant to carry a byte slice:

```rust
pub enum Input<'a> {
    Raw(&'a [u8]),
    Key(KeyEvent),
    Paste(&'a [u8]),
    Mouse(MouseEvent),
}
```

`write_paste` becomes:

```rust
fn write_paste(out: &mut Vec<u8>, bytes: &[u8], modes: TerminalModes) {
    if modes.bracketed_paste {
        out.extend_from_slice(b"\x1b[200~");
        out.extend_from_slice(bytes);
        out.extend_from_slice(b"\x1b[201~");
    } else {
        out.extend_from_slice(bytes);
    }
}
```

The only behavioural change is that the payload is no longer required to be
UTF-8. Marker emission, `byte_len`, and the mode handling are untouched, and
the invariant "a paste is bracketed iff the child currently enables bracketed
paste" still holds.

Note what this does *not* relax. `Paste` and `Raw` remain distinct: `Raw` is
appended verbatim with no markers, `Paste` is marker-aware. The reason a paste
is not sent as `Input::Raw` even when it is already bytes is precisely the
markers — the caller cannot know whether to add them without reading the
child's modes, which is the session's job. Keeping a byte-carrying `Paste`
variant is what lets the session stay the only place that knows the modes.

The absence of any validity requirement on the bytes is intentional. `&[u8]`
is not a promise that the bytes are text in any encoding; it is the same
contract `Input::Raw` already has. A caller that wants to refuse non-UTF-8 can
do that above termnix; a caller that wants fidelity gets it.

## Drawbacks

- A `&[u8]` payload can hold bytes that are meaningless to the child, such as a
  fragment of a multi-byte sequence cut mid-copy. termnix does not try to
  repair or validate this, and it should not: it has no more information about
  the paste than the caller passed in.
- Callers with a `&str` gain a call. This is a small ergonomic cost paid to
  make the byte case representable.
- It is a breaking change to a public variant. Any existing caller that
  matches `Input::Paste(text)` as a `&str` must add `.as_bytes()` at
  construction. The change is mechanical.

## Rationale and alternatives

**Keep `&str` and let callers reconstruct the bytes.** Impossible without
losing data. A non-UTF-8 paste cannot be expressed as any `&str`, so there is
no conversion on the caller's side that preserves it. The current workaround
is to drop it.

**Add a second variant, `PasteBytes(&[u8])`, and keep `Paste(&str)`.** This
avoids the breaking change but duplicates the marker logic on two paths, and a
reader has to check which one is normative. Since a paste is bytes and `&str`
is the special case, one byte-oriented variant is simpler than two variants of
the same thing.

**Send non-UTF-8 pastes as `Input::Raw`.** This is what a caller might reach
for, and it is wrong for the reason above: `Raw` adds no markers, so a child
with bracketed paste enabled would receive an unbracketed paste and could not
tell it apart from typed input. Reconstructing the markers in the caller would
require the caller to know the child's mode, which is exactly the knowledge
termnix exists to hold.

**Do nothing.** The loss is silent and the workaround is already in a real
consumer. The cost of the workaround is a paste that vanishes with no
feedback; the cost of the fix is one mechanical signature change.

## Unresolved questions

None. `Input::Raw` already carries arbitrary bytes with no validity contract,
so there is no new question about what termnix promises about a payload.

## Future possibilities

A byte-oriented `Paste` makes it possible later to add paste-specific byte
handling without another signature change, for example normalising the
payload against the child's encoding or splitting very large pastes. Neither
is proposed here; the payoff of this RFC is only that the data is no longer
lost at the boundary.
