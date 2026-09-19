# RFC: Drop the equality impls that only the tests use

- Status: accepted

## Summary

Remove `PartialEq`/`Eq` from `TerminalState` and from the crate-internal
`Screen`. Both exist to serve this repository's test helpers, and what those
helpers want is a comparison of the *public* state, which they should spell out
themselves. Keeping the impls costs an exclusion list ("`revision`,
`title_changed`, `dirty` are bookkeeping, skip them") that grows every time a
derived field is added, and publishes an equality contract the crate does not
actually mean.

The equality that is genuinely part of a type's value is left alone:
`ScrollbackLine` keeps its derived `PartialEq`, because a retained line is an
immutable value and comparing two of them is comparing the thing itself.

## Motivation

`TerminalState` implements `PartialEq` by hand:

```rust
impl PartialEq for TerminalState {
    fn eq(&self, other: &Self) -> bool {
        self.size == other.size
            && self.primary == other.primary
            && self.alternate == other.alternate
            && self.on_alternate == other.on_alternate
            && self.cursor == other.cursor
            && self.saved == other.saved
            && self.wrap_pending == other.wrap_pending
            && self.pen == other.pen
            && self.modes == other.modes
            && self.title == other.title
            && self.scroll_top == other.scroll_top
            && self.scroll_bottom == other.scroll_bottom
            && self.actions == other.actions
            && self.scrollback == other.scrollback
            && self.scrollback_cells == other.scrollback_cells
        // `revision` and `title_changed` are derived bookkeeping, not part of
        // the terminal's observable value, so they are excluded from equality.
    }
}
```

and `Screen`, the per-screen cell buffer, does the same:

```rust
impl PartialEq for Screen {
    fn eq(&self, other: &Self) -> bool {
        self.size == other.size && self.cells == other.cells
    }
}
```

The question worth asking is not "is anyone calling this?" but "is this
comparison an equality of the type's *value*?" Two observations answer it.

First, **what the callers need is observational equivalence, not equality.**
The comparisons that exist are in the test suite:

```rust
// tests/terminal.rs
assert_eq!(a, b, "{where_}: internal emulator state mismatch");
// tests/terminal_state.rs
assert_eq!(snap_whole, snap_split, ...);        // feed partitioning
assert_eq!(prefix_term, fresh, ...);            // prefix determinism
assert_eq!(t, fresh);
assert_ne!(t, fresh);
```

Each one asks "did these two states end up looking the same through the public
API?" - which is exactly what a caller can observe, and no more. It is not an
equality of the type's value: two states that agree on every accessor can still
diverge on the next `feed`, because the parser's uncommitted continuation state
is not observable and is not compared. The impl even says so by excluding
fields. Once a type excludes part of itself from `==` and still calls the
result equality, the concept has already slipped from "value equality" to
"these look the same right now."

Second, **the hand-written exclusion list is a standing tax.** `revision` was
excluded; the visible-change work added `title_changed` and extended the
comment; `Screen` excludes `dirty` for the same reason. Every new derived field
reopens the question, and the answer is always "exclude it", which is the sign
that the comparison does not belong in the type.

The same reasoning does not apply to every type in the crate, which is the point
of naming them:

| type | is the comparison part of the value? | decision |
| ---- | ------------------------------------ | -------- |
| `TerminalState` | no - it is a mutable machine's current observation | remove |
| `Screen` | no - internal cell buffer; nothing compares it except the impl above | remove |
| `ScrollbackLine` | yes - an immutable retained line | keep |
| `Cell`, `Style`, `Position`, `Size` | yes - plain values | keep |

`ScrollbackLine` is the contrast that makes the rule concrete: it is a snapshot
of a line as it was scrolled off, so two of them being equal is a fact about
the values. `Screen` looks similar (a grid of cells) but is a live buffer whose
only comparison was reached through `TerminalState`, and `TerminalState`'s
comparison was reached only from tests.

Note which tests do the comparing. In `tests/terminal.rs` the `assert_eq!(a, b)`
is a *catch-all* at the end of a helper that has already checked `size`,
`cursor`, every cell, `modes`, alternate state, `title`, `style`, and the
drained queue one by one; it exists to catch whatever the individual assertions
forgot, not to be the primary comparison. The helper even documents its own
limit:

```rust
/// ... then compares the full emulator state
/// via `PartialEq` (which covers saved cursor, wrap pending, scroll region,
/// and the inactive screen, but not the parser's private continuation state).
```

A comparison that is a catch-all for other assertions is a property of the test,
not of the type.

The situation is the mirror image of the `revision()` question settled
separately: there, the API was right and the implementation was wrong. Here, the
API is spelled as equality but the concept it expresses is a test's
observational equivalence.

## Guide-level explanation

You do not compare two `TerminalState` values today; you read the state through
its accessors. That does not change.

Before, if you did compare them, the type told you it knew how:

```rust
// works today, means "the emulator states are equal"
assert_eq!(before, after);
```

After, this does not compile, and you instead say which observations you mean:

```rust
assert_eq!(before.size(), after.size());
assert_eq!(before.cursor(), after.cursor());
assert_eq!(before.cell(at), after.cell(at));
assert_eq!(before.title(), after.title());
assert_eq!(before.drain_actions(), after.drain_actions());
```

That is more code, but it is also the only version that is honest about what
"equal" means: the observations a caller can actually make. Two states that
agree on every public accessor are indistinguishable through the API, whether or
not they agree on internal bookkeeping. Spelling the comparison out also removes
the trap where a new field silently joins or leaves the comparison: an accessor
that is not compared is a missing line in a test, not a hidden clause in a
trait impl.

`Screen` is crate-internal, so its impl has no user-facing story at all; it goes
away because nothing outside `TerminalState`'s impl ever called it.

The tests in this repository keep their catch-all property by moving that
comparison into a test helper, where the list of public observations already
lives, instead of hiding it behind `PartialEq`.

## Reference-level explanation

### Remove

- `impl PartialEq for TerminalState` and `impl Eq for TerminalState {}`.
- The `revision` / `title_changed` exclusion comment that exists only to justify
  the impl.
- `impl PartialEq for Screen` and `impl Eq for Screen {}`, plus the `dirty`
  exclusion note in `Screen` that exists only to justify them.

The hand-written `Debug` impls stay. They are there for the same reason (they
omit the derived fields), and nothing about this proposal changes `Debug`.

### Replace the comparisons in the tests

`drain_and_compare` in `tests/terminal.rs` already compares `size`, `cursor`,
every active-screen cell, `modes`, alternate-screen state, `title`, `style`, and
the drained actions. Replace the trailing `assert_eq!(a, b, ...)` with the two
public observations it does not yet make and that `PartialEq` was supplying:

- `scrollback_lines()` (the retained history),
- `scrollback_cells()` (the retained cell count).

Both already have accessors, so this is strictly *more* coverage than the
current code has in an explicit, readable form, and it drops the `PartialEq`
dependency. The property test that compared two states directly
(`chunk_boundaries_do_not_change_terminal_state`) calls the same helper instead
of comparing the states, which also starts checking the action queue there.

`tests/terminal_state.rs` has four direct comparisons. It gains a local helper
with the same list of observations, and each site calls it. The one negative
assertion (`assert_ne!(t, fresh)`) is written as "some public observation
differs", since there is no boolean to negate.

The helper is defined locally in each file rather than shared. Two files is
borderline, but it is a pure function of the public API, the existing shared
module drives sessions and has a different subject, and a small amount of
repetition across test files costs less than a shared module whose name no
longer fits what it holds.

### Fields that lose their only comparison

Four fields are compared today only because `PartialEq` reaches into them, and
they have no public accessor:
| field | status | decision |
| ----- | ------ | -------- |
| `saved` (saved cursor) | internal | not compared; see below |
| `wrap_pending` | internal | not compared; see below |
| `scroll_top` / `scroll_bottom` | public state, accessor missing | not compared here; see below |

`saved` and `wrap_pending` are internal mechanics, not observations. A caller
cannot save a cursor, restore it, and then ask "where was it saved?" - so there
is no public state to compare. Leaving them out is the point: the helper should
compare what the API exposes, and these are not exposed.

`scroll_top` / `scroll_bottom` are different. A scroll region *is* part of the
terminal's meaning - it changes how later output is laid out, the same way modes
do - so the state is public in substance even though `TerminalState` has no
accessor for it. This RFC does not add one: giving the scroll region an accessor
is a separate API decision with its own shape (one method returning a range? two
methods? does setting it need to be public?), and folding it into an RFC whose
subject is "stop implementing `PartialEq`" would obscure both. It is recorded
there as a known gap, and the comparison is dropped here rather than invented.

### Invariants

- No public observation changes; only a trait impl is removed.
- Every accessor the helper relies on keeps its current signature.
- If the strengthened helper finds a divergence between two states that reach
the same public observations, that is a bug the tests were not catching, and it
should be fixed rather than papered over.

## Alternatives

### Keep the impls and keep excluding derived fields

This is doing nothing, and it is the current cost: two hand-written impls, an
exclusion list that grows with each type, and a public promise with no consumer.
It is defensible - the impls are small and the exclusions are documented - but it
keeps paying the maintenance tax for a comparison the crate does not express.

### Keep `TerminalState`'s `PartialEq`, derive it, and make every field comparable

Removing the hand-written exclusions would require making `revision` and
`title_changed` part of equality, which would make `assert_eq!` fail after two
states reached the same visible state through different numbers of feeds. That
is wrong: those fields are observably invisible. The exclusion is not
incidental; it is the whole reason the impl is hand-written.

### Keep `PartialEq` but expose the comparison as a new method

A `fn same_public_state(&self, other: &Self) -> bool` on `TerminalState` would
keep the comparison in the library and out of the tests. Rejected: it is the
same comparison with a friendlier name, and worse, it is a permanent public
method whose list of compared fields becomes a maintenance burden exactly like
the impl it replaces. The comparison is a test's concern; it belongs in the
test.

### Remove `PartialEq` and add accessors for `saved` and `wrap_pending`

Rejected. Neither is an observation, so the accessors would exist to satisfy a
test's catch-all rather than a caller. Adding API to keep a test convenient is
the wrong direction; dropping the comparison is the right one.

### Remove `PartialEq` and add a scroll-region accessor at the same time

Tempting, because the scroll region genuinely is public state. Rejected for
scope: the two changes have independent motivations, and mixing them means a
reviewer who objects to the accessor's shape cannot accept the removal. The
scroll region is instead named as a known gap so it is not forgotten.

### Remove `PartialEq` from `ScrollbackLine` too

Rejected, and it is the contrast that gives the rule its edge. A retained line
is an immutable value: two lines being equal is a fact about the values, and
the test suite relies on exactly that when it compares two states'
`scrollback_lines()`. The types being changed here are live buffers whose
comparisons express "these look the same right now", not "these values are the
same".

### Remove `PartialEq` from `Screen` but keep `TerminalState`'s

Rejected as an incomplete version of the proposal. `Screen`'s impl has no caller
of its own; its only consumer is `TerminalState`'s impl, which in turn is called
only from tests. Removing just the outer one would leave an impl that is
unreachable and would still need the `dirty` exclusion.

## Drawbacks

- Removing `TerminalState`'s impl is a breaking change. Downstream code that
  compares values (or uses the type as a map key, which `Eq` allowed) stops
  compiling. Nothing in this repository does, and the crate is pre-1.0 with a
  0.3.0 bucket already open for breaking work, but the break is real.
- The catch-all guarantee weakens if the helper is written carelessly: a future
  accessor added to `TerminalState` will not automatically be compared, where
  `PartialEq` would have picked the field up. This is the intended trade - an
  explicit list is reviewable, an implicit one is not - but it is a trade.
- Four internal fields lose their only comparison, so two divergent states could
  compare equal through public observation. For `saved` and `wrap_pending` that
  is accepted; for the scroll region it is a real hole until the accessor
  exists.
- `Screen`'s impl is internal, so removing it is not a public break, but it does
  remove the ability to write `assert_eq!` on screens from a unit test in `src/`
  should one be added later. That is one line of code to restore, and until
  something needs it there is nothing to restore it for.

## Rationale and alternatives

Removing the impls makes the tests say what they mean and removes two impls
whose only consumer chain ends in those tests. The alternative - keep them, keep
excluding derived fields - is not *wrong*, it is just unpaid rent: every new
field reopens the question, and the answer is always the same (exclude the
derived ones), which is a sign the comparison does not belong in the type at
all.

Doing nothing leaves a public `PartialEq` that promises an equality this crate
does not use and cannot fully define (it already declines to cover the parser's
continuation state). The impact is small but permanent.

## Unresolved questions

- Does any downstream consumer use `TerminalState` with `assert_eq!` or as a map
  key? In this repository only the test suite does, and the examples do not. If
  an external one exists, that is new evidence and the removal should be
  reconsidered.
- Should the scroll region get an accessor? Not settled here; named as a known
  gap. If it does, the helper gains one more explicit comparison.
- If the strengthened helper exposes a real divergence, is it fixed in the same
  change? Yes - a test failure caused by comparing *more* public state is a bug,
  not a reason to compare less.
- Should the test helper be shared once a third test file needs it? Not settled;
  the two copies are small and a shared module would have to be named for the
  subject (not `helpers`), which is its own decision.

## Future possibilities

- If a public scroll-region accessor lands, the helper's list grows by one line,
  and the last remaining public-state gap closes.
- The same reasoning applies to other derived fields: anything the type keeps
  for bookkeeping (`revision`, `title_changed`, `dirty`) simply is not part of
  equality, without a comment explaining why.
- If a future type in this crate is a live buffer compared only from tests, it
  starts with no equality impl and the same question is not asked again.

## Outcome

Implemented in [#7](https://github.com/sile/termnix/pull/7) (merged as `49b7e69`).

The change landed as described, with two things settled during review:

- The change was widened to include `Screen`, not left as the `TerminalState`
  removal the first draft proposed. Once `TerminalState`'s impl went, `Screen`'s
  had no caller left, and removing it in the same change was clearly the same
  decision rather than a separate one.
- The draft's premise that the only caller was the `drain_and_compare` helper in
  `tests/terminal.rs` was wrong. `tests/terminal_state.rs` had four more direct
  comparisons (two `==`, an `assert_eq!`, and an `assert_ne!`). The conclusion
  did not change, but the reasoning did: the argument for removing the impls is
  not "few callers" but that what those callers want is observational
  equivalence, not value equality. The Motivation and Alternatives above were
  rewritten around that before merging.

Two questions from the section above are still open and were not answered by the
merge:

- Whether the scroll region should get a public accessor (the known gap is still
  a gap; nothing was added).
- Whether the test helper should be shared once a third test file needs it.
  `tests/terminal_state.rs` now defines `assert_same_public_state` locally and
  `tests/terminal.rs` keeps `drain_and_compare`, following the convention that a
  shared helper module should be named for its subject rather than be a single
  `helpers` parent.

The scope is unchanged from what is described above.
