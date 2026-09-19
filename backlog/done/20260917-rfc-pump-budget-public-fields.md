# RFC: Make `PumpBudget` a plain struct with public fields

- Status: accepted

## Summary

Replace `PumpBudget`'s private fields, `PumpBudget::new(bytes, syscalls)`, and
the `bytes()` / `syscalls()` getters with public fields `bytes` and `syscalls`,
keeping `Default` as the only constructor with a prescribed value.

## Motivation

`PumpBudget` is documented as a ceiling on the work a single
[`Session::pump_io()`](crate::Session::pump_io) call performs: how many bytes
the pump may move and how many read/write syscalls it may issue. It is passed
by value into every pump call, so callers touch it constantly.

The type is shaped like a value object with an invariant — private fields, a
checked constructor, accessor methods — but it has no invariant. Any value is
valid. Zero is not an error: it means "pump nothing and report the budget as
exhausted", which is a documented, intentional way to observe small fixtures.
The bytes and syscalls ceilings are independent, and neither constrains the
other. The constructor performs no validation and cannot fail; it only assigns
two `usize` values, and the getters only read them back.

That shape has three concrete costs:

- `PumpBudget::new(65536, 64)` gives no clue which `usize` is which. The
  parameter names in the rustdoc are the only signal, and call sites do not
  carry them.
- Encapsulation with nothing to protect invites a reader to look for the
  invariant the type is guarding. There is none, so the search ends in
  confusion rather than in a rule.
- It is inconsistent with `Interests` in the same module, which is a plain
  struct with `pub readable: bool` / `pub writable: bool`. Two value types in
  one module, two opposite conventions, no difference in kind to justify it.

Public fields also let a caller write a struct literal, which is the most
direct way to say "these two ceilings and nothing else". A constructor that
assigns fields one-for-one does not add safety to compensate.

## Proposal

```rust
pub struct PumpBudget {
    /// Maximum bytes the pump may move in one call.
    pub bytes: usize,
    /// Maximum read and write syscalls the pump may issue in one call.
    pub syscalls: usize,
}

impl Default for PumpBudget {
    /// The ceilings used when a caller has no reason to pick their own.
    fn default() -> Self {
        Self {
            bytes: 65536,
            syscalls: 64,
        }
    }
}
```

Remove `PumpBudget::new`, `PumpBudget::bytes()`, and `PumpBudget::syscalls()`.
Keep `consume`, `bytes_left`, and `exhausted` as `pub(crate)`: they are the
pump's internal accounting, not part of the caller's vocabulary.

`Default` stays because it carries a decision the caller should not have to
reconstruct: the ceilings most callers want when they have no reason to choose.
It is not a convenience constructor; it is the recommended value.

## Alternatives

### Keep the constructor and getters

Rejected. The type has no invariant, so the encapsulation only hides which
number is which and adds a call to every construction site. Removing it costs
nothing in safety and removes a rule that never existed.

### Make `new` a `const fn` and drop the getters

Rejected. This keeps the positional `usize` problem, which is the more visible
of the two complaints. Constructing by field name fixes the readability
problem at its source instead of papering over it with argument order.

### Keep private fields, add named setters

Rejected. `PumpBudget` is `Copy` and built once per call. Builder-style setters
would add ceremony to a value that a struct literal expresses directly.

## Drawbacks

- Callers can now construct an arbitrary `PumpBudget` that looks deliberate.
  This is already true through `new`; the difference is that nothing suggests a
  rule was checked.
- If an invariant is added later (say, a relationship between bytes and
  syscalls), it will need `pub(crate)` fields or a checked constructor again.
  That is a hypothetical future, and `PumpBudget` has been through three
  releases without acquiring one. Guarding against it now costs every call site
  a `.expect()`-shaped invocation of a constructor that cannot fail.
- `Default` is now the only place that names the recommended ceilings, so the
  rustdoc on it has to carry what `new` used to.

## Open questions

- Whether `bytes` and `syscalls` should be named `bytes` / `syscalls` or
  something that reads better at a struct literal (for example
  `byte_ceiling` / `syscall_ceiling`). Field names are now the only vocabulary,
  so they matter more than they did as parameter names.

## Outcome

Implemented in [#5](https://github.com/sile/termnix/pull/5) (merged as `7a79a10`).

The type is now a plain struct: `pub bytes` and `pub syscalls`, no `new`, no
getters, no constructor to call. That part landed as proposed.

Three details differ from the text above, and they are worth recording because
the text would otherwise mislead a reader of the code.

**`consume`, `bytes_left`, and `exhausted` stayed private, not `pub(crate)`.**
The proposal asks for `pub(crate)` on the grounds that they are "the pump's
internal accounting, not part of the caller's vocabulary" -- which is the
argument for keeping them *out* of the public surface, and `pub(crate)` is not
that. Nothing outside `src/session.rs` uses them: they are called only by
`pump_io`, in the same file. `pub(crate)` would have widened the visibility
without changing a single call site, so the change would have been noise. If a
second module ever needs them, `pub(crate)` is one word away; there is no cost
to waiting for that module to exist.

**The recommended ceilings are documented on the struct, not on `Default`.**
The Drawbacks section above expects `Default`'s rustdoc to carry what `new`
used to, and the first draft did exactly that. It read badly: `Default` is an
impl-block item, a reader lands on the type and sees the fields, and the
paragraph explaining when to pick your own ceilings was two screens away under
a trait heading. The `# Choosing a value` section on the struct now carries it
-- pass `PumpBudget::default()`, reach for a struct literal only to tighten or
widen -- and each field says what a zero in it means. `Default`'s own doc is a
two-line pointer at the struct. The ceilings still live in exactly one place in
code (`impl Default`), which is what the Drawbacks line was protecting; only
the prose moved.

**`Default` is kept, so no `none()` or `unbounded()`.** This RFC leaves the
`Default` question open and the sibling proposal wanted the two named
constructors. Keeping `Default` answers it: it already carries the recommended
64 KiB / 64 syscall ceiling, which is the value most callers want, and neither
`none()` nor `unbounded()` has a caller in this repository. `PumpBudget { bytes:
0, syscalls: 0 }` is what a fixture that wants no work writes, and it says so
at the call site.

The four call sites all moved to struct literals or `default()`, and the test
that pinned `default.bytes()` / `default.syscalls()` now reads the fields
directly -- the asserts that just restated the field values were dropped along
with the getters, since the struct literal above them already says it.

One comment was wrong and was fixed rather than carried: a test built
`PumpBudget { bytes: 0, syscalls: 1 }` under the comment "budget of one
syscall". `bytes: 0` is what makes the pump stop, and the syscall count is
never reached. The comment now says so.

The scope is unchanged from what is described above.
