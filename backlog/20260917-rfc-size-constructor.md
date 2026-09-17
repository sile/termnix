---
Created: 2026-09-17
Status: draft
---

# RFC: Remove `Size::new` and construct `Size` by field

## Summary

Consider deleting `Size::new(rows: u16, cols: u16) -> Option<Size>`. `Size`
already has `pub rows: NonZeroU16` and `pub cols: NonZeroU16`, so call sites
could construct it with a struct literal instead.

The case for removing it is the argument order: two `u16` values sit next to
each other, so a swapped call compiles, while a struct literal's field names
would not allow it. The case against is that removing `new` makes callers reach
for `NonZeroU16` themselves, while the `Option` it carries is what the 39 call
sites unwrap — and that unwrapping does not go away with `new`; a local test
helper removes it either way.

## Motivation

`Size::new` is the only constructor for a type whose fields are already public,
and it does almost nothing:

```rust
pub fn new(rows: u16, cols: u16) -> Option<Self> {
    Some(Self {
        rows: NonZeroU16::new(rows)?,
        cols: NonZeroU16::new(cols)?,
    })
}
```

It is `NonZeroU16::new` applied twice, wrapped in an `Option`, with a `rows,
cols` argument order that the type does not otherwise enforce (the struct
literal's field names do). Because the fields are public, a caller who has
`NonZeroU16` values never needs it, and a caller who has plain `u16` values is
being told to check a condition the type system makes them check again.

An inventory of every call site in this repository (39 total: 15 in
`src/pty/tests.rs`, 15 in `tests/`, 8 in `examples/`, plus one dynamic
conversion) shows the failure path is never taken. Every call passes a literal
constant (`Size::new(24, 80)`, `Size::new(rows, cols)` from a test helper, and
so on) and every one of them is immediately followed by `.expect("nonzero
size")` or an equivalent. The `Option` is not telling callers anything they do
not already know, and its only observable effect is to add boilerplate to 39
sites. Exactly one site — the host-terminal size conversion in
`examples/tuinix.rs` — takes a value it does not control, and it already turns
the `Option` into its own error there.

There are two problems here, and it matters that they are separate. The first is
the argument order: `Size::new(rows, cols)` puts two `u16` values next to each
other, so a call that swaps them still compiles, and the struct literal's field
names are the only thing that would have caught it. The second is the
boilerplate: `Size::new` returns `Option`, so every call site unwraps it.

Removing `new` only fixes the first. Callers with a `u16` still have to convert
through `NonZeroU16`, and they still have to decide what to do when it is zero —
so the `expect` does not disappear, it moves into the struct literal and, worse,
repeats per field:

```rust
// with `new`
Size::new(rows, cols).expect("nonzero size")

// without `new`, the same call site
Size {
    rows: NonZeroU16::new(rows).expect("nonzero rows"),
    cols: NonZeroU16::new(cols).expect("nonzero cols"),
}
```

The `expect` is not protecting the test; it is restating "these are non-zero",
which the test's own literal already says. What actually removes the repetition
is a small helper local to the test that takes `u16` and unwraps the conversion
in one place:

```rust
// with `new`
fn size(rows: u16, cols: u16) -> Size {
    Size::new(rows, cols).expect("nonzero size")
}

// without `new`
fn size(rows: u16, cols: u16) -> Size {
    Size {
        rows: NonZeroU16::new(rows).expect("rows is non-zero"),
        cols: NonZeroU16::new(cols).expect("cols is non-zero"),
    }
}
```

That helper is where the assumption lives, and the test body reads as
`size(24, 80)` instead of the conversion chain. Note that it works either way:
the helper does not depend on whether `Size::new` exists. So the honest
motivation for removing `new` is the argument order, not the `expect`s.

## Proposal

Remove `Size::new`. Leave the type as is:

```rust
pub struct Size {
    /// Number of rows.
    pub rows: NonZeroU16,
    /// Number of columns.
    pub cols: NonZeroU16,
}
```

Call sites change in one of two ways, depending on whether they already hold
`NonZeroU16`:

- Test and example literals use a local helper that unwraps `NonZeroU16` once,
  as above.
- The single dynamic site in `examples/tuinix.rs` converts explicitly, keeping
  its existing error:

  ```rust
  fn size_from_host(size: tuinix::Size) -> Result<termnix::Size, String> {
      Ok(termnix::Size {
          rows: NonZeroU16::new(size.rows())
              .ok_or_else(|| format!("unsupported terminal size: {size:?}"))?,
          cols: NonZeroU16::new(size.cols())
              .ok_or_else(|| format!("unsupported terminal size: {size:?}"))?,
      })
  }
  ```

If a checked constructor turns out to be worth keeping for the dynamic case, it
belongs on the caller's side (a conversion from the host size), not as a
`u16`-taking constructor on `Size` itself.

### Scope

This proposal is only about the argument order. Reducing the number of
`.expect()` call sites in the tests is a separate change that does not depend on
removing `new`: a local `fn size(rows, cols) -> Size` helper achieves it whether
or not `Size::new` exists. If the `expect` boilerplate is the only complaint,
the helper is the whole fix and `Size::new` can stay.

That is the trade-off to decide. Removing `new` buys field-name-checked
construction, at the cost of making callers reach for `NonZeroU16` themselves.
Keeping `new` and adding a local test helper buys the same reduction in noise
with no API change, but leaves the positional argument. The choice is between
the positional argument and the `NonZeroU16` reach, not between changing and
not changing the API.

## Alternatives

### Keep `Size::new`, add a local test helper

The strongest alternative, and the one the current draft is undecided against.
Keep `new` as sugar over the `u16` -> `NonZeroU16` conversion, and add a
`fn size(rows: u16, cols: u16) -> Size` helper in the tests. That removes the
`.expect()` repetition — the part that is actually noisy — with no API change,
and `new` still centralizes the `Option` for the one dynamic site.

What it does not fix is the positional `rows, cols` arguments: a swapped call
still compiles. Whether that risk is worth calling `NonZeroU16` at the
construction site is the question this RFC should answer.

### Make the fields private and keep `new`

Rejected. That would make `new` load-bearing rather than redundant — a real
invariant (`NonZeroU16`) would then be enforced at the boundary, and callers
would stop touching raw `u16`. This is a coherent design, but it moves in the
opposite direction from the actual problem, which is that callers already hold
non-zero constants and are being asked to prove it. It also puts the type back
into the private-fields-with-getters shape that `PumpBudget` is being moved
away from in the same round, for no reason specific to `Size`.

### Keep `new` but return `Size` and panic on zero

Rejected. A constructor that cannot express failure converts a compile-time
known value into a runtime panic, and the panic would be unreachable at every
call site in the repository. A helper local to the caller can panic with a
message naming the field; a library constructor cannot say which one was bad.

### Add `From<(u16, u16)>`

Rejected. It has the same positional problem as `new` and the same
impossible-failure problem in the tuple. A struct literal names the fields.

## Drawbacks

If `new` is removed:

- Callers must reach for `NonZeroU16` directly. That is one import, and it is
the type the fields actually hold.
- A caller who got a `Size` from `Session` or `TerminalState` never calls
  `new`, so the change is confined to code that constructs a `Size` itself.
- A caller with `u16` from an untrusted source now writes the conversion
  themselves. That is unavoidable: the conversion has to happen somewhere, and
  only the caller knows what to do when a dimension is zero.

If `new` is kept:

- The positional `rows, cols` risk stays. A swapped call compiles. In practice
  the 39 call sites are literals in tests and examples, where a swap is caught
  the first time the test runs, so the risk is concentrated in the one dynamic
  site.

## Open questions

- Is the positional-argument risk worth making callers reach for `NonZeroU16`?
  If the answer is no, this RFC becomes "keep `new`, add a test-side `size`
  helper" and the removal is dropped.
- If `new` is kept, does the local helper belong in each test file or in a
  shared test module? A per-file helper is the current proposal; a shared one
  would need a home that is not the public API.
- If `new` is removed, should the dynamic conversion in `examples/tuinix.rs`
  name the failing dimension (rows or cols) rather than repeating the whole
  size?
