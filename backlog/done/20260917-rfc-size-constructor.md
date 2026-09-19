# RFC: Remove `Size::new` and construct `Size` by field

- Status: accepted

## Summary

Remove `Size::new(rows: u16, cols: u16) -> Option<Size>`. `Size` already has
`pub rows: NonZeroU16` and `pub cols: NonZeroU16`, so call sites construct it
with a struct literal instead, or through a small helper local to the file
that needs one.

`new` hides which dimension is which at the moment of construction: the one
place a reader most needs the field names is the one place the call does not
show them. Removing it costs callers an explicit `NonZeroU16` conversion, and
buys call site code that says what it does without a trip to the type's
definition.

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

There are three problems here, and it matters that they are separate. The
first is the argument order: `Size::new(rows, cols)` puts two `u16` values next
to each other, so a call that swaps them still compiles, and the struct
literal's field names are the only thing that would have caught it. The second
is the boilerplate: `Size::new` returns `Option`, so every call site unwraps it.
The third is what the call site says about itself, and it is the one that
decides this proposal.

The second problem, the `expect`s, is not a reason to remove `new`. Callers
with a `u16` still have to convert through `NonZeroU16` and still have to
decide what to do when it is zero, so the `expect` does not disappear; it moves
into the struct literal and repeats per field:

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

// without `new`, the version this proposal ships
fn size(rows: u16, cols: u16) -> Size {
    Size {
        rows: NonZeroU16::new(rows).expect("rows is non-zero"),
        cols: NonZeroU16::new(cols).expect("cols is non-zero"),
    }
}
```

The helper is where the assumption lives, and the test body reads as
`size(24, 80)` instead of the conversion chain. Note that it works either way:
the helper does not depend on whether `Size::new` exists. So the `expect`s are
not a reason to remove `new`; a local helper is the whole fix for them, and
this proposal includes one.

### The field names are already public

The third problem is the one that decides the proposal. `Size` exposes
`pub rows` and `pub cols`, so the names of its two values are already part of
the type's public surface. `Size::new(24, 80)` is then the single place where
that information is *withdrawn*: the meaning of `24` and `80` is visible every
other time a `Size` is handled, and absent at the one moment a caller supplies
them.

A struct literal keeps it:

```rust
Size { rows: 24, cols: 80 }
```

The same line says what is being constructed and what each value means. The
information was never private -- it is a `pub` field name -- so there was
nothing for `new` to protect by hiding it.

This is why the local helper is not a reason to keep `new`. A helper is local
to the file or crate that defines it, so `size(24, 80)` is positional *and*
co-located: the signature with `rows` and `cols` sits in the same file, usually
a few dozen lines up, and the reader never leaves the code they are working in.
`Size::new` is positional and *across a crate boundary*: the parameter names
live in termnix's rustdoc or `src/size.rs`, so the reader has to leave the
caller's code to recover what `24` and `80` mean.

That distinction is the whole argument. Positional arguments are a real cost
when the meaning is on the other side of a boundary, and close to free when it
is the same file. It is also worth more than it used to be: in a codebase
where callers are frequently read and written by tools that do not follow a
cross-crate jump, a public API that puts the meaning of its arguments inside
its own documentation is worse than one that leaves the names at the call
site.

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

- Test and example literals use a local `fn size(rows: u16, cols: u16) -> Size`
  helper that unwraps `NonZeroU16` once per file, as above. The helper stays
  positional on purpose: it is file-local, so its parameter names are a few
  dozen lines away, and its call sites read as `size(24, 80)`. A file that
  constructs only one or two sizes can skip the helper and write the struct
  literal directly.
- The single dynamic site in `examples/tuinix.rs` converts explicitly. It is
  the one place where the caller holds dimensions it does not control, and the
  conversion belongs with the rest of that caller's validation:

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

### What the helper is not

The local `size(rows, cols)` helper does not weaken the argument above, even
though it is positional. A helper is local to the file that defines it, so its
parameter names are a few dozen lines from its call sites; `Size::new` is
positional across a crate boundary, so its parameter names are in termnix's
documentation. The difference is developed in the motivation above.

So the helper is not an alternative to removing `new`, and it is not a reason
to keep it. It is how the `NonZeroU16` reach is kept to one place per file once
`new` is gone. The cost it leaves behind is one import per file that constructs
a `Size`, paid once, against a constructor whose argument meaning is paid for
at every call site.

## Alternatives

### Keep `Size::new`, add a local test helper

The strongest alternative, and the one this proposal rejects. Keep `new` as
sugar over the `u16` -> `NonZeroU16` conversion, and add a
`fn size(rows: u16, cols: u16) -> Size` helper in the tests. That removes the
`.expect()` repetition with no API change, and `new` still centralizes the
`Option` for the one dynamic site.

What it does not fix is the concealment: `Size::new(24, 80)` still puts the
meaning of its arguments in the type's own documentation. The helper does not
compensate, because it is a different thing in a different place -- it hides
field names that are near the caller, while `new` hides field names that are
far from the caller. Adding one does not justify the other. The helper is part
of this proposal, not an alternative to it.

Its one real argument is that it keeps callers from naming `NonZeroU16`. That
is a genuine cost, and it is paid once per file rather than once per call. It
is not enough to outweigh keeping the public constructor positional.

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

Rejected. It has the same concealment as `new` -- a tuple carries no field
names, so `Size::from((24, 80))` is positional and says less than `Size::new`
did -- and the same impossible-failure problem. A struct literal names the
fields.

## Drawbacks

- Callers must reach for `NonZeroU16` directly. That is one import per file
  that constructs a `Size`, and it is the type the fields actually hold.
- A caller who got a `Size` from `Session` or `TerminalState` never calls
  `new`, so the change is confined to code that constructs a `Size` itself.
- A caller with `u16` from an untrusted source now writes the conversion
  themselves. That is unavoidable: the conversion has to happen somewhere, and
  only the caller knows what to do when a dimension is zero.
- Test helpers stay positional, so `size(24, 80)` still compiles if the two
  arguments are swapped. This is accepted: the helper is file-local, so its
  parameter names are in view, and a swap is caught the first time the test
  runs. The dynamic site, which is where a swap would be genuinely hard to
  notice, writes the fields by name.

## Open questions

- If `new` is removed, should the dynamic conversion in `examples/tuinix.rs`
  name the failing dimension (rows or cols) rather than repeating the whole
  size? The site currently reports an unsupported *size* for a zero dimension
  and a named *dimension* for an out-of-range one, so the two errors are
  already inconsistent with each other. Combining them into one check per
  dimension is the likely answer, but it is an implementation detail and can be
  settled in the pull request.
- Should `Size` ever grow a checked constructor back? If a future caller has a
  genuinely untrusted size, the conversion belongs at that boundary. Nothing in
  this repository needs one today, and a `u16`-taking constructor on `Size`
  would reintroduce the concealment this proposal removes.

## Outcome

Implemented in [#4](https://github.com/sile/termnix/pull/4) (merged as `15ab779`).

The `impl Size` block is removed and all 39 call sites are converted. Two
things in the text above do not match what was built, and both are worth
correcting here rather than leaving a reader to copy the wrong thing.

**The dynamic-site sketch calls methods that do not exist.** The Proposal
writes `size.rows()` and `size.cols()`, but `tuinix::Size` has no accessors: it
has `pub rows: usize` and `pub cols: usize`, and `examples/tuinix.rs` builds it
by struct literal. The real conversion reads the fields directly, so
`NonZeroU16::new(size.rows)` and not `size.rows()`. Since this sketch is the
part of the RFC a reader would copy for their own conversion, the field form is
the one worth having in the text.

**The sketch's error message is also superseded.** It has both dimensions
report `unsupported terminal size: {size:?}`, which is the size-level report
the open question above calls inconsistent. The conversion now names the
dimension that failed for both cases:

```
terminal rows is zero: 0
unsupported terminal rows: 70000
```

So the open question resolves toward naming the dimension, one check per
dimension, which is the answer the RFC guessed was "likely". That was the only
item left to the pull request.

The second open question -- whether `Size` should ever grow a checked
constructor back -- was not acted on, and is not a question this change
settles. Nothing in the repository needed one.

The one place the implementation departs from the Proposal's shape is the
file-local helper: `tests/terminal.rs` and `tests/terminal_state.rs` already
had a `term(rows, cols)` helper, so `size(rows, cols)` was added underneath it
and `term` now calls `size`. That is the same "one import per file, one
conversion per file" the RFC describes, just sharing the conversion with the
helper that was already there rather than sitting beside it.

The scope is unchanged from what is described above.
