---
Created: 2026-09-17
Status: draft
---

# RFC: Remove `Size::new` and construct `Size` by field

## Summary

Delete `Size::new(rows: u16, cols: u16) -> Option<Size>`. `Size` already has
`pub rows: NonZeroU16` and `pub cols: NonZeroU16`, so call sites construct it
with a struct literal. Callers with a `u16` convert through `NonZeroU16` at the
point where they know the value is non-zero.

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

There is also a construction-side problem the constructor does not solve. In
tests, a helper like this is the common shape:

```rust
fn term(rows: u16, cols: u16) -> TerminalState {
    TerminalState::new(Size::new(rows, cols).expect("nonzero size"))
}
```

The `expect` is not protecting the test; it is restating "these are non-zero",
which the test's own literal already says. The same intent is expressed more
directly by a small helper local to the test that takes `u16` and unwraps the
`NonZeroU16` conversion in one place:

```rust
fn size(rows: u16, cols: u16) -> Size {
    Size {
        rows: NonZeroU16::new(rows).expect("rows is non-zero"),
        cols: NonZeroU16::new(cols).expect("cols is non-zero"),
    }
}
```

That helper is where the assumption lives, it names the field it is checking,
and the test body reads as `size(24, 80)` instead of the conversion chain.

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

## Alternatives

### Keep `Size::new`

Rejected. The fields are already public, so the constructor enforces nothing
that a struct literal does not. Its only effect across the repository is 39
`.expect()` call sites.

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

- Callers must reach for `NonZeroU16` directly. That is one import, and it is
the type the fields actually hold.
- Removing a public constructor is a breaking change for the crate's users.
  `termnix` is at `0.1.0` and the type is unlikely to be constructed by anyone
  who is not already holding `NonZeroU16` (a caller who got a `Size` from
  `Session` or `TerminalState` never calls `new` at all).
- A caller with `u16` from an untrusted source now writes the conversion
  themselves. That is unavoidable: the conversion has to happen somewhere, and
  only the caller knows what to do when a dimension is zero.

## Open questions

- Whether to keep a `pub(crate)` helper for the test and example literals, or
  to let each test file define its own. A per-file helper is the proposal;
  a shared one would need a home that is not the public API.
- Whether the error message for the dynamic conversion should name the failing
  dimension (rows or cols) rather than repeating the whole size.
