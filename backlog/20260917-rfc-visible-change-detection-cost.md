# RFC: Make "did the visible state change?" cheap to ask

- Status: draft

## Summary

Stop computing a visible-state fingerprint inside `feed()`. Record visible
change where it happens instead - a dirty flag maintained by `Screen`'s cell
writes, a before/after compare of the `Copy` scalar fields, and a flag set where
`osc_dispatch` assigns the title - and let `feed` do only an O(1) read of those
signals to decide whether to bump `revision()`. Asking "did anything change
since I last looked?" then costs nothing proportional to `rows * cols`.

The externally visible API does not change; the proposal moves work that `feed`
does per call onto the paths that actually write visible state.

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
issue is narrower than "a getter with side effects." It is two things, and the
second is the larger one:

- the mechanism chosen for change detection (hash and compare) is more
expensive than the question requires; and
- the work is attached to `feed` rather than to the question, so it runs even
  when its result is discarded. A caller that folds several feeds into one
  redraw pays the hash on every feed and reads `revision()` once, so all but the
  last hash are thrown away. The cost is not only too large, it is in the wrong
  place.

## Guide-level explanation

Before, the emulator answers "did the visible state change?" by recomputing a
fingerprint of everything visible and comparing it to the last one, every feed.

The fingerprint hashes the whole active screen. That cost is paid on every
`feed` call, including the calls whose input changed nothing, and including the
calls whose result is never read because the caller folds several feeds into one
redraw. The work is attached to `feed` rather than to the question.

After, the emulator keeps a small amount of bookkeeping where visible state is
actually written, and `feed` only reads it:

```rust
// internal, illustrative
pub fn feed(&mut self, bytes: &[u8]) {
    let before = self.scalars();        // cheap, Copy fields
    self.primary.begin_feed();
    self.alternate.begin_feed();
    feed_bytes(self, bytes);            // cell writes set Screen::dirty;
                                        // osc_dispatch sets title_changed
    let changed = self.primary.take_dirty()
        | self.alternate.take_dirty()
        | self.title_changed
        | self.scalars() != before;
    if changed {
        self.revision = self.revision.wrapping_add(1);
    }
}
```

The externally visible contract is unchanged: `revision()` still increments
when, and only when, the visible state changed since the previous read, and it
stays unchanged for inputs with no visible effect. How many times the visible
state changed within a single `feed` is not observable and not counted: one
`feed` bumps `revision` at most once.

## Reference-level explanation

The visible state splits into three kinds of field, and each is tracked where it
is written rather than re-derived in `feed`.

### Cell writes: a dirty flag on `Screen`

`Screen` gains a `dirty: bool` that its writing methods set when they store
something. The methods already return early when the write is out of range or is
a no-op by construction (`col >= cols`, `count == 0`, `row` outside the region),
and those paths do not set the flag. The flag is a boolean, not a counter: the
question `revision()` answers is whether the visible state changed, and how many
cells changed within one `feed` is deliberately not observable.

`feed` clears both screens' flags, runs the parser, and then reads them. This is
O(1) and independent of `rows * cols`.

`Screen` derives `PartialEq`, so the new field must be excluded from equality
the same way `TerminalState` already excludes `revision` and `last_visible`: the
flag is derived bookkeeping, not part of the screen's observable value.
`Screen`'s `PartialEq` becomes hand-written (or the field is otherwise kept out
of the comparison) so existing `assert_eq!` on terminal states keep passing.

### Scalar fields: a before/after compare in `feed`

The fields that live outside `Screen` and are cheap to compare - `size`,
`cursor`, `pen`, `modes`, `on_alternate`, and `wrap_pending` - are `Copy` or
small and comparable by value. `feed` snapshots them before parsing and compares
again after. The comparison is a fixed number of small value comparisons, not a
function of the grid size.

The one non-`Copy` visible field is `title`. Snapshotting a `String` for the
before/after compare would allocate on every `feed`, which is exactly the kind
of per-feed cost this proposal removes. Instead, `osc_dispatch`, which is the
only place a title is assigned, sets a `title_changed` flag when it stores a new
title. That is one site, so it does not reintroduce the scattered-flag problem
that makes option (a) below fragile.

### What `feed` does

`feed` performs the parser run, two O(1) flag reads, and a fixed-size scalar
compare. Nothing it does scales with `rows * cols`. `resize` is outside `feed`
but mutates visible state (it already calls `refresh_revision` today), so it
must use the same signals: it goes through `Screen::resize` for the cells and
compares or re-derives the scalars it writes.

### Increment semantics

The counter contract is unchanged: it increments when the visible state changed
and stays put otherwise. It is not a count of edits. A caller that compares
`revision()` across two reads only learns "same" or "different", which is all
the API has ever promised.

### Options considered earlier

See the alternatives below for the three shapes this RFC originally proposed and
why the hybrid above was preferred to each.

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

### (a) A dirty flag set by every mutation path

Every code path that writes a visible field sets a single `self.dirty = true`:
cell writes, cursor moves, SGR changes, mode changes, title changes, and the
resize of the active screen. `feed` clears it before parsing and bumps
`revision` after if it was set.

- Pros: O(1) per feed, no hashing anywhere.
- Cons: correctness depends on *every* mutation path remembering to set the
  flag. `TerminalState` has roughly two dozen such methods plus direct field
  assignments, so a missed site is a silent bug - a real visible change that
  does not bump `revision`. This is the classic dirty-flag hazard.

The hybrid above keeps the O(1) property but moves the flag to `Screen` (where
the cell writes already funnel through a handful of methods) and keeps the
scalar fields on a compare, so the two-dozen-site discipline is not required.
The one non-cell field that would need a scattered flag, `title`, is assigned in
a single place, so it gets one.

### (b) A write/generation counter on `Screen` plus scalar compares

Give `Screen` a monotonic counter bumped on cell writes, and compare that
counter plus the small scalars (cursor, size, modes, title) instead of hashing
all cells.

- Pros: keeps "did it change" decidable without trusting every call site to
  remember a boolean.
- Cons: the counter is redundant with a boolean for this API. Because a single
  `feed` bumps `revision` at most once, the count of writes is never observed;
  only whether the count is nonzero matters, which is a dirty flag. A counter
  would also need the same `PartialEq` exclusion as the flag.

The hybrid uses the boolean the API actually needs and reserves the counter for
the case it would earn its place: if a future feature ever needed to know how
much changed, not just whether.

### (c) Keep the fingerprint and make it opt-in

Offer a mode (or a separate method pair) where change detection is explicit.

- Pros: no behavior change for existing callers.
- Cons: two code paths for one concept, and the expensive path's users get no
  benefit from the cheap one. It also leaves the per-feed grid hash in the code
  and only makes it opt-out, which does not remove the work this RFC is about.

It was not needed once the goal narrowed to "`feed` should do no grid-sized
work": the hybrid does that for every caller with no mode to select.

### Make `revision()` itself compute the fingerprint lazily

Rejected, but for a different reason than an earlier draft of this RFC gave.
That draft said a lazy variant would "move the hashing to the read side, which
is worse, because every poll round that reads it pays the hash." That reasoning
assumes the caller reads `revision()` as often as it feeds, and it is exactly
backwards for the case this RFC is about: a caller that folds several feeds into
one redraw reads once and feeds many times, so computing on read would hash once
instead of many times, and not compute at all when the value is unused.

It is still rejected, on two narrower grounds. First, with the signals above,
there is nothing left to compute lazily - the flags are already maintained where
the writes happen, and reading them is O(1) either way. A lazy fingerprint would
keep the `rows * cols` hash and only move *when* it runs, which does not remove
the work, it relocates it. Second, it would change `revision()` from a plain read
into a read that can cost `rows * cols`, so a caller that polls it would pay on
the read side with no bound. The flag-based design keeps both `feed` and
`revision()` free of grid-sized work.

## Drawbacks

- The signal is now maintained where the writes happen rather than re-derived,
  so it trades the fingerprint's "correct without any discipline from the
  mutation paths" property for one that depends on the `Screen` writers (and
  the single `title` assignment site) keeping their flags honest. The blast
  radius is smaller than option (a) - a handful of `Screen` methods, not two
  dozen scattered call sites - but it is not zero.
- A cell write of a value equal to what was already there still sets the flag,
  so a feed that rewrites identical cells bumps `revision` where the old
  fingerprint did not. This is a false positive, not a false negative: the
  counter is allowed to move when the state may have changed, and a caller that
  compares it only redraws unnecessarily. The existing test that asserts
  `revision` holds for BEL, a partial escape, and an ignored OSC still passes,
  because those paths do not reach a `Screen` writer.
- `Screen` derives `PartialEq`, so the new field has to be excluded from it
  (and from `Debug`, if the flag would otherwise appear), the same way
  `TerminalState` already excludes `revision` and `last_visible`. Getting this
  wrong would break existing `assert_eq!` on terminal states.
- `resize` also mutates visible state and must use the same signals; today it
  calls `refresh_revision`, which is the one non-`feed` path the new design must
  cover explicitly.

## Open questions

- Is the per-feed full-grid hash actually a measured cost in real use, or a
  theoretical one? Resolved as not the deciding question. The cost scales with
  `rows * cols` and is attached to `feed`, so it is paid on feeds whose result
  is never read; measuring the constant would say how bad the waste is, not
  whether it is waste. The design removes the work rather than tuning it, so no
  benchmark gates it.
- If (a): can the mutation paths be made to fail loudly rather than relying on
  review? Not applicable to the chosen shape: the flag lives on `Screen`, whose
  writers are few and already co-located, and the single `title` assignment
  site is one line.
- Where does the dirty signal live - `Screen` (per screen) or `TerminalState`?
  Resolved: cells on `Screen` (per screen, because alternate and primary are
  written independently), scalar fields compared in `feed`, title flagged at
  its one assignment site.
- Does anything outside `feed` and resize mutate visible state today? Resolved:
  no. `feed` and `resize` are the only entry points that reach visible
  mutation, and `resize` is covered by the same `Screen` and scalar signals.

## Future possibilities

- If a caller ever needs to know *how much* changed rather than *whether*,
  `Screen`'s boolean dirty flag can become a counter without changing the
  externally visible contract of `revision()`.
- Once the per-cell write path has a cheap "did this store change the cell"
  check, the dirty flag could narrow from "a write happened" to "a write stored
  a new value," removing the false positives in the Drawbacks section.
