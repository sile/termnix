# Bug: DA2 and DA3 are answered with the DA1 reply

- Status: open

## Summary

A secondary device attributes request (`CSI > c`, DA2) and a tertiary device
attributes request (`CSI = c`, DA3) are both answered with the primary device
attributes reply `ESC [ ? 6 c`. `handle_csi` recognizes only the `?` private
marker, so the `>` and `=` markers look like a plain DA1 and fall into the
`'c' if !private` arm. A program that asks for its terminal's version and
gets a VT102 identity instead is told something it did not ask for, and the
reply it does receive is not in the shape the DA2 grammar allows.

## Reproduction

```text
let mut t = termnix::TerminalState::new(Size { rows: 5, cols: 10 });

t.feed(b"\x1b[>c");      // DA2, no parameters
observed: pending_reply_bytes() == b"\x1b[?6c"

t.feed(b"\x1b[>0c");     // DA2, parameter 0 (what tmux sends)
observed: pending_reply_bytes() == b"\x1b[?6c"

t.feed(b"\x1b[=c");      // DA3
observed: pending_reply_bytes() == b"\x1b[?6c"

t.feed(b"\x1b[c");       // DA1, the only form that should reply
observed: pending_reply_bytes() == b"\x1b[?6c"
```

All four inputs produce the identical bytes `1b 5b 3f 36 63`. The behavior does
not depend on how the input is split; a single `feed` of the whole sequence and
a `feed` per byte both reach the same reply, because the decision is made in
`csi_dispatch` once the final byte arrives.

## Observed behavior

`src/terminal_emu.rs` derives the private flag from the `?` marker alone:

```rust
let private = intermediates.first().copied() == Some(b'?');
```

and dispatches the attributes request on that flag:

```rust
'c' if !private => self.primary_da(),
```

A DA2 (`ESC [ > c`) and DA3 (`ESC [ = c`) both carry an intermediate byte that is
not `?`, so `private` is false and the `'c' if !private` arm matches. The
`>` and `=` markers are never consulted, and there is no code path that
produces a DA2 or DA3 reply at all.

## Expected behavior

`src/terminal.rs` documents the supported query set as "DSR, CPR, and primary
DA", and `primary_da`'s own comment says the reply is "shaped like a VT102",
which is the DA1 form `ESC [ ? 6 c`. Only a primary DA request (`ESC [ c`, with
no `>` or `=` marker) is a DA1 request, so only that form may reach
`primary_da`. A DA2 or DA3 request must not be answered with the DA1 reply;
the narrowest correct behavior is to leave the `>` and `=` forms unhandled (no
reply), and the fuller one is to answer DA2 with `ESC [ > ... c` and DA3 with
`ESC [ = ... c`.

## Impact

Any client that probes its terminal — tmux sends `ESC [ > 0 c` at startup, and
other programs send DA2 or DA3 to pick a terminfo entry or feature set — reads
a reply that does not match the request it sent. The reply arrives as terminal
*input*, so it lands in whatever the client does with unrecognized bytes. In
termnix this is a correctness bug reachable from the public API
(`TerminalState::feed` plus `pending_reply_bytes`), not an ergonomics one.

Found while driving a shell inside tuke, a PTY-backed soft-keyboard front end
built on a `termnix::Session`: running tmux inside the child terminal made the
cursor jump away and snap back on ordinary keystrokes. That symptom is
consistent with tmux parsing the DA1-shaped reply to its DA2 probe as stray
input, but the causal link has not been confirmed from tmux's side; the reply
mismatch above is reproducible on its own, without tmux.

## Notes

The fix has to distinguish three markers, not two: `?` (DA1), `>` (DA2), and
`=` (DA3), plus the no-marker form, which is also DA1. Widening `private` from
`Option<bool>` to the marker byte would keep DA1 on `primary_da` and give DA2
and DA3 somewhere to go. If only the misrouting is fixed and DA2/DA3 are left
unanswered, the `'c' if !private` arm still needs to reject `>` and `=`
rather than rely on `private` alone, or the same fold happens again.
