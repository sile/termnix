# RFC: Make "did the visible state change?" cheap to ask

- Status: draft

## Summary

Replace the full-grid fingerprint that `feed()` computes on every call with a
cheaper signal (a dirty flag or a screen write-counter plus scalar compare), so
asking "did anything change since I last looked?" stops costing a hash of the
entire active screen per `feed`.

## Motivation

The current change-detection design hashes the whole visible state on every
feed, regardless of whether anything changed:

```rust
pub fn feed(&mut self, bytes: &[u8]) {
    feed_bytes(self, bytes);
    self.refresh_revision();
}

fn refresh_revision(&mut self) {
    let fingerprint = self.visible_fingerprint(); // hashes active screen cells + size + cursor + pen + modes + title
    if fingerprint != self.last_visible {
        self.last_visible = fingerprint;
        self.revision = self.revision.wrapping_add(1);
    }
}
```

The observable API (`revision()`) is a plain read, and that part is fine. The
cost is on the write side, and it is paid even when the feed changed nothing
visible:

- a `BEL`, an ignored sequence, an unrecognized CSI, or a partial escape that
  does not complete a sequence all still hash the entire grid;
- a feed that writes one cell still hashes every cell of the active screen;
- the result of that hash carries exactly one bit of information (changed / not
  changed), which is all `revision()` needs to be well-defined.

A caller servicing a poll loop feeds a session whenever the PTY is readable,
which for an idle-but-chatty program (a prompt redrawing its status line, a
spinner, cursor-blink heartbeats the child writes itself) means the full-grid
hash runs on every wakeup. The per-call cost scales with `rows * cols`, and it
is pure overhead for a caller that only ever asks the boolean question.

Note what is *not* wrong here: `revision()` itself is side-effect free, so a
caller cannot corrupt state by polling it, and the counter wraps safely. The
issue is narrower than "a getter with side effects" - it is that the mechanism
chosen for change detection (hash and compare) is more expensive than the
question requires, and its cost is on a path the caller hits every poll round.

## Guide-level explanation

Before, the emulator answers "did the visible state change?" by recomputing a
fingerprint of everything visible and comparing it to the last one, every feed.

After, mutation paths raise a dirty signal when they write something visible,
and `revision()` is derived from that signal:

```rust
// internal, illustrative
pub(crate) dirty: bool,

pub fn feed(&mut self, bytes: &[u8]) {
    self.dirty = false;
    feed_bytes(self, bytes); // mutation paths set self.dirty = true on visible writes
    if self.dirty {
        self.revision = self.revision.wrapping_add(1);
    }
}
```

The externally visible contract is unchanged: `revision()` still increments
when, and only when, the visible state changed since the previous read, and it
stays unchanged for inputs with no visible effect.

## Reference-level explanation

Three shapes are plausible; they differ in where the "changed" decision is made
and how easy it is to get right.

### (a) Dirty flag set by mutation paths

Every code path that writes a visible field (cell writes, cursor moves, SGR
changes, mode changes, title changes, resize of the active screen) sets
`self.dirty = true`. `feed` clears it before parsing and bumps `revision` after
if it was set.

- Pros: O(1) per feed in the common case; no hashing at all.
- Cons: correctness now depends on *every* mutation path remembering to set the
  flag. A missed site is a silent bug (a real visible change that does not bump
  `revision`). This is the classic dirty-flag hazard and the reason the current
  fingerprint design may have been chosen deliberately: it is hard to get
  wrong.

### (b) Compare only what is cheap and sufficient

Keep the compare-based approach but narrow it: give `Screen` a monotonic
write/generation counter bumped on cell writes, and compare that counter plus
the small scalars (cursor, size, modes, title) instead of hashing all cells.

- Pros: keeps "did it change" decidable without trusting every call site to
  remember a boolean; the cell-write path has one place to maintain.
- Cons: still requires the cell-write path to maintain a counter, though that
  is one site inside `Screen` rather than many scattered call sites.

### (c) Keep the fingerprint, make it opt-in

The hashing is only expensive for callers that feed frequently. Offer a mode
(or a separate method pair) where change detection is explicit - the caller
calls `feed` and then asks for a *cheap* dirty answer, while callers that want
today's guarantee keep it.

- Pros: no behavior change for existing callers; cost only where it is asked
  for.
- Cons: two code paths for one concept; the expensive path's users get no
  benefit and the crate carries both.

The author leans toward (a) or (b): the point is to stop paying `rows * cols`
per feed, and any shape that does that keeps the external contract identical.
Between them, (b) is more robust (one counter on `Screen` instead of many
call sites) at the cost of a slightly larger change to `Screen`; (a) is smaller
and faster but concentrates risk into "did every site set the flag?".

## Alternatives

### Do nothing; document the cost

Rejected as the resting place. The doc comment on `revision()` currently frames
change detection as derived bookkeeping and does not warn that each `feed`
hashes the grid. At minimum the docs could say so, but a caller that feeds in a
poll loop cannot act on the warning except by avoiding `feed`, which it cannot.

### Remove `revision()` and make the caller diff frames

Rejected. `revision()` is the right shape for a Sans I/O caller (see the
`needs_pump()` / `interests()` split elsewhere): it lets the caller decide
"did anything change?" without owning a frame buffer or diffing cells itself.
The problem is the cost of producing it, not its existence.

### Make `revision()` itself compute the fingerprint lazily

Not applicable. `revision()` is already side-effect free and does no work; a
lazy variant would *move* the hashing to the read side, which is worse (every
poll round that reads it pays the hash, whether or not anything changed).

## Drawbacks

- (a)/(b) trade the current design's "hard to get wrong" property for speed:
  the fingerprint approach is correct without any discipline from the mutation
  paths, and a dirty-flag/counter approach is correct only if every site is
  maintained.
- Changing `refresh_revision` touches code paths that tests may pin (revision
  increments, `PartialEq` excluding bookkeeping fields). The `revision` field is
  already excluded from `PartialEq` and `Debug`, so the equality contract need
  not change, but the exact increment timing should be re-verified.

## Open questions

- Is the per-feed full-grid hash actually a measured cost in real use, or a
  theoretical one? A microbenchmark feeding no-op and single-cell inputs across
  a few sizes would settle whether (a)/(b) are worth the correctness risk.
- If (a): can the mutation paths be made to fail loudly (a debug assertion or a
  test that feeds every escaping construct and asserts `revision()` moved)
  rather than relying on review?
- If (b): where does the cell generation counter live - `Screen` (per screen)
  or `TerminalState` (covering alternate-screen switches as one event)?
- Does anything outside `feed` and resize mutate visible state today? If so,
  `refresh_revision`'s current placement already covers it, and (a)/(b) must
  cover those paths too.
