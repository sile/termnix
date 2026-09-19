# RFC: Drop `PartialEq` from `TerminalState`

- Status: draft

## Summary

Remove `TerminalState`'s `PartialEq` and `Eq` implementations. The one caller is
a test helper in this repository, and what that helper wants is a comparison of
the *public* state, which it should spell out itself. `PartialEq` currently
offers a comparison nobody outside the tests asks for, at the cost of an
exclusion list ("`revision` and `title_changed` are bookkeeping, skip them")
that grows every time the type gains a derived field.

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

Three things are wrong with this picture, and they compound.

First, **nobody uses it.** `TerminalState` is a public type, but within the
crate and its examples the only comparison of two terminals is one line in a
test helper:

```rust
// tests/terminal.rs
assert_eq!(a, b, "{where_}: internal emulator state mismatch");
```

`examples/headless.rs` and `examples/tuinix.rs` read the state through
`size()`, `cursor()`, `cell()`, `rows()`, `modes()`, `title()`, `style()` and
`drain_actions()`; neither ever compares two states. A public trait impl with one
caller, all of it in this repository's tests, is an API surface that exists only
to serve the tests.

Second, **the test already compares the same fields by hand.** The same helper
that ends with `assert_eq!(a, b)` first checks `size`, `cursor`, every cell,
`modes`, alternate-screen state, `title`, `style`, and the drained action queue
individually. So the `PartialEq` assertion is a *catch-all* for whatever the
individual assertions forgot, not the primary comparison. The helper even
documents its own limit:

```rust
/// ... then compares the full emulator state
/// via `PartialEq` (which covers saved cursor, wrap pending, scroll region,
/// and the inactive screen, but not the parser's private continuation state).
```

Third, **the exclusion list is a maintenance tax that keeps growing.** Equality
is a public promise about the type's value, so every new field forces a decision:
compare it, or exclude it and justify why. `revision` was excluded; the recent
visible-change work added `title_changed` and had to extend the comment. A future
field carries the same decision again. None of this is needed if the comparison
lives where the comparison is actually wanted.

The situation is the mirror image of the `revision()` question settled
separately: there, the API was right and the implementation was wrong. Here, the
API itself has no consumer.

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

The tests in this repository keep their catch-all property by moving that
comparison into the helper, where the list of public observations already lives,
instead of hiding it behind `PartialEq`.

## Reference-level explanation

### Remove

- `impl PartialEq for TerminalState` and `impl Eq for TerminalState {}`.
- The `revision` / `title_changed` exclusion comment that exists only to justify
  the impl.

The hand-written `Debug` impl stays. It is there for the same reason (it omits
the derived fields), and nothing about this proposal changes `Debug`.

### Strengthen the test helper

`drain_and_compare` in `tests/terminal.rs` already compares `size`, `cursor`,
every active-screen cell, `modes`, alternate-screen state, `title`, `style`, and
the drained actions. Replace the trailing `assert_eq!(a, b, ...)` with the two
public observations it does not yet make and that `PartialEq` was supplying:

- `scrollback_lines()` (the retained history),
- `scrollback_cells()` (the retained cell count).

Both already have accessors, so this is strictly *more* coverage than the
current code has in an explicit, readable form, and it drops the `PartialEq`
dependency.

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

### Keep `PartialEq` and keep excluding derived fields

This is doing nothing, and it is the current cost: a hand-written impl, an
exclusion list that grows with the type, and a public promise with no consumer.
It is defensible - the impl is small and the exclusions are documented - but it
keeps paying the maintenance tax for a caller that does not exist.

### Keep `PartialEq`, derive it, and make every field comparable

Removing the hand-written exclusions would require making `revision` and
`title_changed` part of equality, which would make `assert_eq!` fail after two
states reached the same visible state through different numbers of feeds. That
is wrong: equality has to mean "observably the same", and those fields are
observably invisible. The exclusion is not incidental; it is the whole reason
the impl is hand-written.

### Keep `PartialEq` but restrict it to the test helper via a new method

A `fn same_public_state(&self, other: &Self) -> bool` on `TerminalState` would
keep the comparison in the library and out of the tests. Rejected: it is the
same unused API with a friendlier name, and worse, it is a permanent public
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

## Drawbacks

- It is a breaking change. Downstream code that compares `TerminalState` values
  (or uses it as a map key, which `Eq` allowed) stops compiling. Nothing in this
  repository does, and the crate is pre-1.0 with a 0.3.0 bucket already open for
  breaking work, but the break is real.
- The catch-all guarantee weakens if the helper is written carelessly: a future
  accessor added to `TerminalState` will not automatically be compared, where
  `PartialEq` would have picked the field up. This is the intended trade - an
  explicit list is reviewable, an implicit one is not - but it is a trade.
- Four internal fields lose their only comparison, so two divergent states could
  compare equal through public observation. For `saved` and `wrap_pending` that
  is accepted; for the scroll region it is a real hole until the accessor
  exists.

## Rationale and alternatives

Removing the impl makes the tests say what they mean and removes a public trait
impl whose only consumer is those tests. The alternative - keep it, keep
excluding derived fields - is not *wrong*, it is just unpaid rent: every new
field reopens the question, and the answer is always the same (exclude the
derived ones), which is a sign the comparison does not belong in the type at
all.

Doing nothing leaves a public `PartialEq` that promises an equality this crate
does not use and cannot fully define (it already declines to cover the parser's
continuation state). The impact is small but permanent.

## Unresolved questions

- Does any downstream consumer use `TerminalState` with `assert_eq!` or as a map
  key? The crate has one known consumer in this repository's examples, neither
  of which does. If an external one exists, that is new evidence and the
  removal should be reconsidered.
- Should the scroll region get an accessor? Not settled here; named as a known
  gap. If it does, the helper gains one more explicit comparison.
- If the strengthened helper exposes a real divergence, is it fixed in the same
  change? Yes - a test failure caused by comparing *more* public state is a bug,
  not a reason to compare less.

## Future possibilities

- If a public scroll-region accessor lands, the helper's list grows by one line,
  and the last remaining public-state gap closes.
- The same reasoning applies to other derived fields: anything the type keeps
  for bookkeeping (`revision`, `title_changed`) simply is not part of equality,
  without a comment explaining why.
