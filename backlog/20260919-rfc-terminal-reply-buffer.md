# RFC: Replace `TerminalAction` with a reply buffer on `TerminalState`

- Status: draft

## Summary

Remove `TerminalAction` and let `TerminalState` hold its outgoing bytes in one
contiguous buffer that the caller consumes with an explicit `advance`. A query
reply is then appended to that buffer where it is produced, instead of being
wrapped in a single-variant enum, collected into a `Vec<TerminalAction>`, and
copied a second time into the caller's own queue.

The externally visible change is that `drain_actions()` and the
`TerminalAction` type go away, and `Session`'s separate `outbound` queue stops
holding terminal replies. The bytes a caller must write do not change.

## Motivation

`TerminalAction` has exactly one variant:

```rust
pub enum TerminalAction {
    /// Bytes that should be written to the PTY master (for example CPR or DA replies).
    WritePty(Vec<u8>),
}
```

Its only production sites are three `push` calls in `src/terminal_emu.rs`
(`device_status` for DSR and CPR, `primary_da` for DA1), and its only
production consumer is one loop in `Session::decode_byte`
(`src/session.rs`):

```rust
let actions = self.term.drain_actions();
let mut reply = Vec::new();
for action in actions {
    let TerminalAction::WritePty(bytes) = action;
    reply.extend_from_slice(&bytes);
}
```

The loop does not dispatch on the variant; it concatenates every action. So the
enum buys no dispatch, while costing:

- one `Vec<u8>` allocation per reply, immediately copied and dropped;
- a second copy out of that allocation into the session's `outbound`;
- an intermediate `Vec<TerminalAction>` allocation per drain.

Worse, the copy is redundant with state the session already keeps. `Session`
has its own growable queue with a written-prefix offset:

```rust
outbound: Vec<u8>,
write_offset: usize,
```

compacted through `compact_outbound_if_needed()` against
`OUTBOUND_COMPACT_THRESHOLD`. That is the `bytes + offset` buffer pattern, and
`TerminalState` cannot use it because replies leave the emulator as separately
owned `Vec`s. The same abstraction is implemented once in `Session` and not at
all where the bytes are produced.

A second cost lands on the caller. Because a reply is only visible after
`drain_actions()`, and because `Session` must know which bytes of `outbound`
are still-unsent reply bytes rather than input, `Session` tracks that as a
separate range:

```rust
/// Range of the one unsent terminal reply inside `outbound`.
pending_reply_range: Option<Range<usize>>,
```

plus a `pending_reply_unsent_len()` helper and its own tests. This bookkeeping
exists only to reconstruct, after the fact, a fact the emulator knew for free:
which bytes are a reply.

## Guide-level explanation

Before, a caller writing a query reply to the PTY does this:

```rust
state.feed(b"\x1b[6n");
for action in state.drain_actions() {
    let TerminalAction::WritePty(bytes) = action;
    pty.write_all(&bytes)?;
}
```

After, the emulator exposes the bytes it wants written, and the caller reports
how many it managed to write:

```rust
state.feed(b"\x1b[6n");
let mut written = 0;
while written < state.pending_reply_bytes().len() {
    let n = pty.write(&state.pending_reply_bytes()[written..])?; // partial writes are fine
    written += n;
}
state.advance_reply_bytes(written);
```

A caller that always writes everything it can simply does
`state.advance_reply_bytes(state.pending_reply_bytes().len())`. A caller that
wrote part of
it passes the count it actually wrote, and the rest stays pending for the next
attempt. Nothing about *when* bytes are produced changes; only how the caller
gets them.

This is the same shape the session already uses for bytes travelling the other
direction, so there is one buffer discipline in the crate rather than two.

## Reference-level explanation

`TerminalState` replaces its `actions: Vec<TerminalAction>` field with a reply
buffer, reusing the `bytes + offset` form already proven in `Session`:

```rust
struct ReplyBuf {
    bytes: Vec<u8>,
    offset: usize,
}

impl ReplyBuf {
    /// Bytes produced but not yet consumed.
    fn pending(&self) -> &[u8] {
        &self.bytes[self.offset..]
    }

    /// Appends a reply. Callers write into `pending_mut()` instead where possible.
    fn push(&mut self, bytes: &[u8]) { /* extend_from_slice */ }

    /// Marks `n` bytes consumed, reclaiming the buffer when drained.
    fn advance(&mut self, n: usize) { /* offset += n; clear when fully consumed */ }
}
```

and exposes it as:

```rust
/// Returns the bytes the emulator wants written to the PTY master, if any.
pub fn pending_reply_bytes(&self) -> &[u8];

/// Marks `n` bytes of `pending_reply_bytes()` as written.
///
/// # Panics
/// Panics if `n > self.pending_reply_bytes().len()`.
pub fn advance_reply_bytes(&mut self, n: usize);
```

The bound is checked on every call, in release builds as well as debug. This
diverges from `Buf::advance`, which documents the same precondition but only
asserts it in debug builds; here the panic is not a debug aid but the contract.
An out-of-range `n` means the caller miscounted what it wrote, and clamping
would silently mark a reply consumed that never reached the PTY. A missing
reply to a query is a protocol failure with no later detection point, so it is
worth one comparison per call. Replies are produced by queries and advance is
called at most once per reply, so the branch is not on any hot path.

The names carry three facts: `pending` that the bytes are produced but not yet
written, `reply` that they answer a query (not, say, a bell or a title change),
and `_bytes` that the value is a byte slice rather than a reply object. The
`_bytes` suffix matches the vocabulary `SessionCounters` already uses
(`terminal_reply_bytes_generated`, `reply_bytes_written`), and applies to both
methods so the pair reads as two verbs on the same noun.

The reply sites write into the buffer directly, so the per-reply `Vec`
disappears:

```rust
// device_status, CPR
let row = self.cursor.row + 1;
let col = self.cursor.col + 1;
let buf = self.reply.pending_mut(); // &mut Vec<u8>, truncated then written
write!(buf, "\x1b[{row};{col}R").expect("write to Vec<u8> is infallible");
```

Invariants:

- `pending_reply_bytes()` is empty when no reply is outstanding; it is never a
  partial escape sequence, because a reply is appended in one step.
- `advance_reply_bytes(0)` is a no-op and is always valid.
- `advance_reply_bytes(n)` is checked against the current
  `pending_reply_bytes().len()` on every call; `n == len` is in range and drains
  the buffer, `n > len` panics rather than clamping (see the API comment above).
- `advance_reply_bytes(n)` for the full length drains the buffer and resets the
  offset, so a steady state of small replies does not grow `bytes` without
  bound. `Session`'s `compact_outbound_if_needed()` is the existing precedent
  for this reclamation.
- Reply bytes are produced in the order the queries arrived, and
  `pending_reply_bytes()` preserves that order because it is one buffer.
- There is exactly one reply buffer on `TerminalState`. It is not owned by a
  `Screen`, so neither `soft_reset` (RIS) nor a primary/alternate switch drops
  a reply that is still owed to the PTY. This preserves today's behavior, where
  `actions` is top-level and `soft_reset` leaves it untouched.

`Session::decode_byte` becomes an append followed by a pass-through, with no
intermediate `Vec` and no `TerminalAction` match; `pending_reply_range` and
`pending_reply_unsent_len()` are deleted. `Session::outbound` continues to hold
input from `enqueue_input`, so the two directions stop sharing one queue and
`write_offset` stops needing to know which bytes are replies.

## Drawbacks

- Breaking change: `TerminalAction` and `drain_actions()` are removed from the
  public API. This costs a minor version.
- `advance` puts an obligation on the caller that `drain_actions()` did not:
  the caller must report what it wrote, and a caller that forgets to advance
  re-writes the same reply on its next poll. `drain_actions()` could not be
  forgotten in this way because taking was the only option.
- The panic contract on `advance_reply_bytes` is a new failure mode where
  `drain_actions()` had none. A caller that passes a count from a stale
  `pending_reply_bytes()` length panics instead of silently misbehaving. The
  check runs in release builds too, so this is a real panic rather than a
  debug-only assertion that would clamp in practice.
- `TerminalState` gains mutable I/O-shaped state, which is closer to the
  boundary the module documents itself as staying behind. It stays Sans I/O —
  nothing is written to a descriptor — but "the emulator has a queue" is a
  larger claim than "the emulator returns values".

## Rationale and alternatives

### Keep the enum and add variants

Rejected. Every candidate future variant (a bell notification, a title-change
notification) is *also* just bytes to write, because the emulator's only
outbound channel is the PTY. An enum whose variants are all `Vec<u8>` is a
`Vec<u8>`. The grep-able fact is that the one consumer concatenates all
variants unconditionally.

### Keep `drain_actions()`, but return one concatenated `Vec<u8>`

This removes the enum and the per-reply allocation but keeps the hand-off
allocation and, more importantly, keeps `Session` owning reply bytes. It also
preserves the "take it or lose it" semantics, which is exactly the semantics
that are awkward for a partial write: a caller that can only write part of the
reply has nothing to do with the rest, because `drain_actions()` already gave
it up. The buffer-and-advance shape handles partial writes, which PTY masters
genuinely produce when the reader is slow.

### Keep the reply buffer per screen (primary/alternate)

Rejected. A reply is produced by a query, not by screen content, so it is an
obligation to the PTY rather than a property of either screen. Splitting the
buffer would make `\x1b[?1049h` (or RIS) hide a reply that is still owed: a
caller that asked for the cursor position and has not yet written the answer
would find it silently gone from `pending_reply_bytes()`. It would also break
`advance_reply_bytes`: a count read from `pending_reply_bytes()` before a switch
would refer to a different buffer after it, turning a simple call into a panic
or a wrong-buffer write. One buffer matches the single outbound stream the
terminal actually has, and matches today's top-level `actions` field.

### Make the reply buffer a type the caller owns

Rejected. It moves the buffer out of the emulator, so the producer would have
to be handed a `&mut ReplyBuf` on every `feed`. The emulator would still need
to know a reply is outstanding to avoid dropping it, so the state does not
actually leave.

### Have `feed` return the bytes

Rejected. `feed` is documented as accepting bytes that may end mid-sequence and
may change nothing observable. Returning a value from it invites the caller to
treat every call as producing output, and the reply commonly arrives in a later
`feed` than the query.

### Do nothing

The cost is one redundant copy and one redundant allocation per query reply.
Query replies are rare (a handshake, a probe), so this is not a performance
emergency. The reason to do it is not throughput but that the current shape
implements the same buffer discipline in one place and not the other, and
makes `Session` reconstruct a distinction the emulator already had. Doing
nothing leaves two buffer idioms and a `pending_reply_range` field that exists
only to undo the enum.

### Equality

The `PartialEq` question that the first draft of this RFC had to settle is now
moot. `TerminalState` and `Screen` no longer implement `PartialEq`/`Eq`; the
roster was removed in the equality RFC (merged), and tests compare the public
API field by field instead of delegating to a trait impl. There is therefore no
impl to exclude an outstanding reply from, and this RFC does not need to touch
equality at all.

The tests compensate explicitly:

- `drain_and_compare` in `tests/terminal.rs` compared the drained
  `Vec<TerminalAction>` values of the two states. Under the buffer model that
  line becomes a comparison of `pending_reply_bytes()` for each state,
  preserving the existing requirement that the two states produce the same
  reply bytes.
- `assert_same_public_state` in `tests/terminal_state.rs` compares only
  persistent observables and never looked at pending output, so it needs no
  change; if we want it to also witness that a reply is pending, that is an
  addition, not a repair.
- `cursor_position_report_is_an_action` in `tests/terminal.rs` builds a
  `vec![TerminalAction::WritePty(...)]` literal; it becomes a byte comparison
  against `pending_reply_bytes()`, and the test name loses "action".

## Unresolved questions

None. The questions raised while reviewing the early draft are all settled in
the sections above: the panic-vs-clamp choice on `advance_reply_bytes`
(panics, checked in release builds), whether the buffer is shared across
screens (one buffer, not per screen), the method names
(`pending_reply_bytes` / `advance_reply_bytes`), and whether `ReplyBuf` becomes
a type shared with `Session` (not now; see Future possibilities).

## Future possibilities

- Extract `ReplyBuf` into its own module once a second caller needs it.
  `Session` has two `bytes + offset` pairs already
  (`read_buffer`/`read_offset` and `outbound`/`write_offset`), but their
  compaction policies differ: they use different thresholds, and the read
  buffer compacts when it is fully consumed while `outbound` compacts by size.
  A shared type would have to take the policy as a parameter, which is not worth
  it for one caller. Doing this would also be the moment to revisit the method
  names.
- A `write_reply(&mut self, w: impl io::Write) -> io::Result<usize>` convenience
  for callers that hold a `&mut dyn Write` to the master and do not want to
  handle partial writes themselves.
- Richer outbound events (bell, title change) if a caller ever needs to observe
  them rather than write them; they would be a separate channel, not this
  buffer.
