---
Created: 2026-09-17
Status: draft
---

# RFC: Add named constructors for `PumpBudget`

## Summary

Add named constructors for the two budgets a caller actually constructs by
intent — "do nothing" and "no ceiling" — so a caller with a test or a special
case does not have to spell out `PumpBudget::new(0, 1)` and read the docs to
know what it means. Related to, but independent of, the proposal to make
`PumpBudget`'s fields public.

## Motivation

The only constructor today is `PumpBudget::new(bytes, syscalls)`. Its two
`usize` parameters are positional and unnamed at the call site, and the sizes
are ceilings (both must be positive to do any work). A caller that wants a tiny
budget in a test, or a budget that never runs out, has nothing expressive to
write:

```rust
// meant to be "pump nothing" in a test
PumpBudget::new(0, 1)

// meant to be "pump until done"
PumpBudget::new(usize::MAX, usize::MAX)
```

Both are correct — `new(0, _)` is documented to pump nothing and report the
budget exhausted — but neither reads as the intent it encodes. The first looks
like an off-by-one a reader has to decode; the second is a magic constant.

## Guide-level explanation

Before:

```rust
let none = PumpBudget::new(0, 1);               // what, exactly?
let all  = PumpBudget::new(usize::MAX, usize::MAX); // hope this is enough
```

After:

```rust
let none = PumpBudget::none();        // pump nothing; report exhausted
let all  = PumpBudget::unbounded();   // move what there is to move
```

## Reference-level explanation

```rust
impl PumpBudget {
    /// A budget that permits no work. A pump with this budget moves no bytes
    /// and issues no writes, and reports itself exhausted.
    pub fn none() -> Self { Self { bytes: 0, syscalls: 0 } }

    /// A budget large enough that a single pump is effectively unlimited.
    /// Intended for callers that deliberately drain a session in one call;
    /// it is not a recommended default.
    pub fn unbounded() -> Self { Self { bytes: usize::MAX, syscalls: usize::MAX } }
}
```

`unbounded` uses `usize::MAX` rather than the crate's default ceilings because
"unbounded" must mean *no ceiling*, and the default is a real ceiling that
exists precisely to be a limit. A caller who wants the default still writes
`PumpBudget::default()`.

The names interact with the public-fields proposal: if `new` is removed and the
fields become public, `none()` and `unbounded()` stay useful (they name an
intent a struct literal spells out awkwardly), and `Default` remains the one
recommended value. This RFC does not depend on that one landing, and can be
implemented before, after, or instead of it.

## Alternatives

### Do nothing; document `new` more clearly

Rejected as the resting place. `new(0, 1)` will keep appearing in tests and
will keep being read as a mistake; a name costs nothing.

### Constants instead of functions

```rust
pub const NONE: PumpBudget = PumpBudget { bytes: 0, syscalls: 0 };
```

Possible, and `PumpBudget` is `Copy` with no invariant, so a constant is
feasible. Functions are preferred here only because they leave room for the
implementation to change without a breaking change to a `const`'s value, and
because `PumpBudget::none()` reads as a constructor at the call site where
`PumpBudget::NONE` reads as a marker.

### An `Option<PumpBudget>` where `None` means unbounded

Rejected. It moves the "unbounded" meaning onto `Option`, which is already used
elsewhere for "no value"; a `PumpBudget` that means unbounded is a value, not
the absence of one.

## Drawbacks

- `none()` looks like it might return `Option<PumpBudget>` to a reader used to
  `Option::None`, which is a small readability hazard.
- `unbounded()` encourages draining a session in one call, which is the fairness
  hazard described in the related enumeration RFC. It is offered as an explicit
  choice, not a default, but it does make the choice one word away.

## Open questions

- `none()` versus `zero()`, given the `Option` association? `none()` describes
  "no work", `zero()` describes the numbers; the RFC prefers the former.
- Does `unbounded()` need to exist, or is the whole case a caller that should be
  calling `pump_io` in a loop instead? The RFC includes it because `usize::MAX`
  is a worse thing to see in a call site than a name.
