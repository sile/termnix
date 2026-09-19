# RFC: Give scrollback lines a sequence number

- Status: draft

## Summary

Give every [`ScrollbackLine`] a sequence number assigned when it is pushed into
the history and never reassigned, and expose it with `sequence()`. The history
can then be addressed by *identity* instead of by position. Positions are
exactly what history churn destroys; a number survives a push, a trim, and a
resize.

This is one field and one accessor. It is deliberately not an ergonomics
proposal: the point is that the current data model cannot express a stable
reference to a historical line at all, and every caller that needs one is
forced to copy the history. A number is the smallest thing that removes that
constraint. Any helper that makes lookups *convenient* can be added later
without reconsidering the model.

## Motivation

A terminal multiplexer's copy mode is the motivating consumer. When the user
enters copy mode, the application wants to browse the history while the child
process keeps running: output continues to arrive, lines are pushed, and the
caller's own trimming drops the oldest lines. The line the user is looking at
must stay addressable through all of that.

Today the history is `VecDeque<ScrollbackLine>`, exposed as
[`scrollback_lines()`], and it can only be addressed by position:

- `scrollback_lines()[0]` is "the oldest retained line", which changes meaning
every time a line is trimmed.
- `scrollback_lines()[i]` is "the line currently at offset `i`", which changes
  meaning every time a line is pushed.

So a position is not an anchor. Entering copy mode and holding an index means
holding something that is invalidated by ordinary progress. The only way to
keep a stable view is therefore to **copy the history** (`to_vec()`) on entry
and work on the snapshot, which costs a full clone of every retained cell at
the exact moment the user asks for responsiveness, and which then stops moving
with the live terminal.

That cost is not a tuning problem; it is what the data model forces. No amount
of caller-side arithmetic produces a stable handle on a line, because lines
carry no identity to name them by. Adding one number per line removes the
constraint at its source: the caller remembers a number, and the number keeps
meaning the same line while the history moves underneath it.

## Guide-level explanation

The history is a sequence of lines, and each line now carries its own number.
Numbers increase by one per line, starting at zero for the first line the state
ever retains, and are never renumbered: when the oldest lines are trimmed,
their numbers simply disappear from the sequence, and the retained lines keep
theirs.

Before, keeping a place in the history meant keeping a copy:

```rust
// Entering a history view: clone everything, so positions cannot shift.
let snapshot: Vec<ScrollbackLine> = state.scrollback_lines().iter().cloned().collect();
let mut cursor = snapshot.len();
// ... the snapshot never grows; new output is invisible to the view.
```

After, keeping a place means keeping a number:

```rust
// Entering a history view: remember which line the cursor is on.
let anchor = state.scrollback_lines().back().unwrap().sequence();
// ... history may be pushed and trimmed; the anchor still names that line.
let line = state.scrollback_lines().iter().find(|l| l.sequence() == anchor);
```

The second version copies nothing. Whether `line` is `Some` depends on whether
that line has been trimmed away yet; the number is the caller's way to ask
"is the line I was looking at still here, and where is it now?" without having
snapshotted anything.

## Reference-level explanation

`ScrollbackLine` gains a private `sequence: u64` field and a
`pub fn sequence(&self) -> u64` accessor. Nothing else about the type changes:
`cells()`, `len()`, and `is_empty()` keep their meanings, and `Debug`, `Clone`,
`PartialEq`, and `Eq` stay derived.

### Assignment

The number is assigned in `TerminalState::scroll_up_screen()`, the single site
that pushes into the history, and nowhere else. `TerminalState` holds a
`scrollback_next_sequence: u64` counter, initialized to `0`, incremented with
`wrapping_add` per pushed line. The first line ever retained has sequence `0`.

`ScrollbackLine::new` is not public, so it takes the number as an argument
(`ScrollbackLine::new(cells, sequence)`) and the constructor remains the only
way to build a line. There is no public constructor for a line with a
caller-chosen number, so the monotonic property cannot be violated from outside
the crate.

### Invariants

- **Monotonic.** Within a retained history, sequences strictly increase from
  the front of the deque to the back.
- **Stable.** A line's sequence never changes after it is pushed. Trimming
  removes lines; it never renumbers the survivors. `trim_scrollback()` is
  therefore unaffected, and does not need to know about sequences.
- **Not contiguous.** After trims, the front of the deque is generally not `0`
  and the retained numbers have gaps. Callers must not infer the count of lines
  ever produced, nor the size of the history, from a sequence number. If a
  count of lines ever produced is wanted, that is a separate counter.
- **Per state, not per session.** The counter belongs to `TerminalState`, so a
  state that is dropped and rebuilt starts numbering from zero again. Two
  `TerminalState` values are not comparable through their sequences.
- **Wrapping.** `wrapping_add` matches how `TerminalState::revision()` already
  documents its counter: strictly increasing in practice, wrapping only after
  `u64::MAX` pushes, which is unreachable.

### Equality

The number is part of the line's identity, so `PartialEq` includes it: two
lines with identical cells but different sequences are different lines. This is
the opposite of `TerminalState::revision` and `last_visible`, which are derived
bookkeeping and are excluded from equality; here the number *is* the identity
that the RFC is introducing. Because `ScrollbackLine` derives `PartialEq`, no
hand-written implementation is needed.

### Interaction with existing API

- [`scrollback_lines()`] keeps returning `&VecDeque<ScrollbackLine>`, so no
  caller needs to change to keep compiling (see Alternatives for the
  `BTreeMap` question).
- `trim_scrollback()` and `scrollback_cells()` are unchanged: trimming still
  removes whole lines oldest-first and still only tracks cells.
- `resize()` is unchanged. Lines retain the width they had when saved, and now
  also retain the number they were given then.

### Deliberately not included

No lookup helper is added by this RFC: no `line_by_sequence()`, no
`sequence_range()`, and no binary search. A caller that wants to map a number
back to a line iterates the deque and compares `sequence()`. This is a
conscious scope choice, not an oversight; see Future possibilities.

## Drawbacks

The history grows by eight bytes per line. With cells dominating the cost of a
line, this is small but not zero, and it is paid by every consumer, including
ones that never look at a number.

`ScrollbackLine` becomes wider and no longer fully describes a *visible* row's
content; the number is bookkeeping that travels with stored data. Putting
bookkeeping inside the data type is what makes the anchor survive, but it does
mean the type is no longer a pure container of cells.

Callers that construct expectations from `Debug` output or compare lines in
tests will see the new field, and equality now distinguishes two otherwise
identical lines.

## Rationale and alternatives

This design is the best among the alternatives because it is the smallest
change that removes the *impossibility* the Motivation describes. Every other
option either fails to make a line addressable, or changes far more of the
public surface than the problem requires.

- **Copy the history on copy-mode entry (the status quo).** This is what the
  RFC removes. It is correct but forces an O(history) clone at the moment the
  user asks for interactivity, and the view then diverges from the live
  terminal. Rejected as the thing to fix.
- **Anchor by distance from the newest line.** A caller could remember
  `len - k` instead of an index. This breaks on exactly the event the anchor
  exists to survive: trimming drops the oldest lines and changes `k` for the
  same physical line, while pushing shifts it in the other direction. Rejected.
- **Anchor by byte offset.** No byte stream is retained; only completed lines
  are. Rejected.
- **A parallel `VecDeque<u64>` of numbers beside the lines.** Two structures to
  keep in step, with a class of bug (divergence) that a field cannot have, for
  no benefit over putting the number in the line. Rejected.
- **A `scrollback_lines_removed` counter on `TerminalState`.** Lets a caller
  reconstruct an index by arithmetic, but leaks the buffer's layout into every
  caller and still needs the caller to track pushes and trims exactly. The
  number a line carries is strictly simpler and survives on its own. Rejected.
- **Change the container to `BTreeMap<u64, ScrollbackLine>`.** This is a real
  alternative and is examined on its own below.

### Why not `BTreeMap`

`VecDeque` offers no `binary_search` of its own, and it is not contiguous, so a
caller cannot borrow a `&[ScrollbackLine]` to search either. Without a lookup
helper, mapping a number back to a line therefore means a linear scan, which is
`O(n)` in the retained history. A `BTreeMap<u64, ScrollbackLine>` keyed by the
sequence would give `O(log n)` lookup, and `range(n..).next()` would resolve a
number whose line has already been trimmed to the next surviving line, which is
close to what a copy-mode cursor wants.

It is not included in this RFC for three reasons:

1. **It changes a public return type.** `scrollback_lines()` would stop being a
   `VecDeque` and callers that name that type in their own signatures would
   break. The RFC's claim is that a number can be added without a breaking
   change; folding in a container change would give that up and make the RFC two
   decisions instead of one.
2. **It reaches past the stated problem.** The Motivation is that a line cannot
   be *named* at all. Naming is solved by the number alone. Lookup speed is a
   separate question about a separate structure, and it can be revisited on its
   own evidence once callers exist.
3. **It gives up `get(i)`.** Sequential access by offset is convenient for
   rendering a window around the cursor, and a map does not offer it cheaply.

The counter-argument is real and is the reason this is written down rather than
dismissed: if a consumer re-resolves its anchor on every scroll step over a
history of tens of thousands of lines, `O(n)` per step is felt, and the map's
`O(log n)` would pay for itself. That is an argument to revisit this section
with a real caller, not to grow this RFC now. The design here does not prevent
it later: a field on the line and a map keyed by it can coexist, and migrating
the storage does not change the number a caller already holds.

## Unresolved questions

- **Is `O(n)` lookup acceptable for the copy-mode cursor?** This RFC takes the
  position that it does not need to be answered to add the number, but the
  answer decides whether the `BTreeMap` alternative should be taken up later.
  It needs a caller that scrolls repeatedly, not an estimate.
- **Should a session-wide (not state-local) counter exist?** A per-state counter
  restarts at zero for a rebuilt state. A counter that never restarts would let
  sequences distinguish lines from different states, at the cost of being
  owned somewhere other than `TerminalState`. Nothing in the Motivation needs
  it yet.

## Future possibilities

- **Lookup helpers.** `line_by_sequence(n)` and `sequence_range()` are additive
  and can be added once a caller wants them; `sequence_range()` in particular
  would let a caller compute an offset without scanning twice.
- **Range trimming by sequence.** Trimming by a number rather than a count
  ("drop everything older than `n`") becomes expressible once lines have
  numbers, and would let a caller pin its own retention policy to the anchor it
  is displaying.
- **Container migration.** If `O(log n)` lookup is ever needed, the storage can
  move to a `BTreeMap` (or an index beside the deque) without changing what a
  number means, and therefore without invalidating anchors that callers already
  hold.
- **A total-lines counter.** A cheap `u64` counting every line ever pushed would
  make the gaps in a retained range measurable, which the sequence number alone
  deliberately does not.

[`ScrollbackLine`]: ../src/terminal_types.rs
[`scrollback_lines()`]: ../src/terminal.rs
