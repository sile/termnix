# RFC: Drop the running maxima from `SessionCounters`

- Status: draft

## Summary

Remove the four `max_*` fields from [`SessionCounters`] —
`max_buffered_read_bytes`, `max_pending_write_bytes`, `max_scrollback_lines`,
and `max_scrollback_cells` — along with the private `update_maxes()` helper and
its six call sites.

[`SessionCounters`]: https://docs.rs/termnix/latest/termnix/session/struct.SessionCounters.html

## Motivation

`SessionCounters` documents itself as a record of cumulative totals, and says
so twice: it "record[s] only production and consumption totals", and
"[q]uantities owned by the terminal emulator, such as the current scrollback
size, are reached through [`Session::terminal_state()`] instead, so this type
never mixes the two observation subjects."

The four `max_*` fields violate both statements at once. They are `usize`
residuals rather than cumulative `u64` totals, and two of them
(`max_scrollback_lines`, `max_scrollback_cells`) are snapshots of emulator-owned
state, which the type's own doc says belongs to `terminal_state()`.

This is a leftover from an earlier split. Commit `5c351a2` ("Split session
metrics into counters and pending bytes") broke a combined `SessionMetrics`
type into a cumulative side and a current-value side. The peak fields belonged
to neither and were simply left on the cumulative side, where they remain.

Two further facts make the fields hard to justify as-is:

- **Nothing observes them.** No example and no test reads
  `max_buffered_read_bytes`, `max_pending_write_bytes`, or
  `max_scrollback_cells`. The only reference anywhere is a single assertion in
  `tests/session.rs` that `max_scrollback_lines` equals the current scrollback
  length — a check of the peak against the very accessor a caller already has.
- **They are computed by a second path.** `update_maxes()` reads the session's
  internal buffers (`buffered_read_len()`, `unsent()`) and calls
  `self.term.scrollback_lines()` directly, rather than going through the public
  residual accessors (`undecoded_read()`, `unwritten()`) that are supposed to
  be the single source of those quantities. The two computations can drift
  apart without any test noticing.

## Guide-level explanation

Today a caller who wants to track peaks reads them off the counters:

```rust
let counters = session.counters();
eprintln!("peak buffered read: {} bytes", counters.max_buffered_read_bytes);
eprintln!("peak unwritten: {} bytes", counters.max_pending_write_bytes);
eprintln!("peak scrollback: {} lines", counters.max_scrollback_lines);
```

After this change, a caller that wants a peak tracks it itself, from the
quantities that are already public. The current queue depths are on the
counters; the scrollback size is on the terminal state:

```rust
let counters = session.counters();
let term = session.terminal_state();
peaks.buffered_read = peaks.buffered_read.max(counters.undecoded_read());
peaks.unwritten = peaks.unwritten.max(counters.unwritten());
peaks.scrollback_lines = peaks.scrollback_lines.max(term.scrollback_lines().len());
peaks.scrollback_cells = peaks.scrollback_cells.max(term.scrollback_cells());
```

This is more code at the call site, but it is the same arithmetic, and it uses
only names that are public and documented today. The counters stop carrying a
second, private notion of every quantity they already expose.

Note the honest limitation: the crate updates its maxima from inside `pump_io`,
so it sees every intermediate state a pump passes through, including a
scrollback that grows and is then trimmed within a single pump. A caller
polling between pumps can miss such a transient. That gap is not introduced by
this change — a caller cannot recover those intermediate states today either,
since the maxima are only ever updated on the same call sites — but the removal
makes the limitation visible instead of hiding it behind a field.

## Reference-level explanation

Delete from `SessionCounters`:

```rust
/// Highest observed undecoded read bytes.
pub max_buffered_read_bytes: usize,
/// Highest observed unwritten write bytes.
pub max_pending_write_bytes: usize,
/// Highest observed scrollback lines.
pub max_scrollback_lines: usize,
/// Highest observed scrollback cells.
pub max_scrollback_cells: usize,
```

Delete the helper:

```rust
/// Updates the running maxima tracked in the session counters.
fn update_maxes(&mut self) { ... }
```

Delete its six call sites in `src/session.rs`: one in `enqueue_input()` (571)
and five around `pump_io` (575, 1059, 1131, 1148) as they stand today, plus the
definition at 865. The helper's inputs `buffered_read_len()` and `unsent()` both
keep other callers — the former in `needs_pump()` and the read-buffer limit
arithmetic, the latter in the `write_phase` budget calculation — so nothing else
becomes dead code.

Update the single test reference: `tests/session.rs:653` currently asserts
`before_len == before.max_scrollback_lines`, where `before_len` is
`session.terminal_state().scrollback_lines().len()`. Either drop the assertion
or keep the test's intent by comparing two `terminal_state()` reads.

This is a breaking change: the four fields are `pub` and the type is
`#[derive(PartialEq)]`, so removing them changes the public surface. A minor
version bump is required under the crate's semver policy.

## Drawbacks

- Callers that do use the maxima must write and keep their own tracking code,
  and must remember to sample after every pump.
- Polling from outside cannot reproduce peaks that occur and are undone within
  a single `pump_io` call. If that precision matters, it needs a dedicated API,
  not these fields.
- It is a breaking change for a small amount of removed code.

## Rationale and alternatives

- **Do nothing.** The fields keep contradicting the type's documented contract,
  keep computing quantities by a second path, and keep being untested. The
  contradiction is the kind that misleads readers about where to look for
  measurements.
- **Document the peaks instead.** Adding a paragraph that explains the three
  kinds of value on the type would make the fields less surprising, but it would
  not resolve the contradiction with `terminal_state()` ownership, and it would
  preserve a second computation of the residuals. Documentation is the right fix
  for a naming ambiguity, not for a misplaced observation subject.
- **Rename the fields.** `max_pending_write_bytes` uses `pending` where the rest
  of the type says `unwritten`. But the type's vocabulary problem is wider than
  this one field — the cumulative fields name themselves, and pulling on this
  thread would imply renaming a good deal more. Deleting the peaks removes the
  odd vocabulary out entirely and needs no rename.
- **Move the maxima to a separate peak-tracking type.** A dedicated type would
  keep the feature while fixing the ownership. Given that nothing in the crate
  observes any of the four peaks, this adds surface to preserve an unobserved
  convenience. It remains available later if a real consumer appears.
- **Move only the scrollback peaks to `TerminalState`.** The strongest of the
  alternatives, and left as an open question below rather than folded into this
  proposal.

## Unresolved questions

- Should scrollback peaks, if anyone genuinely needs them, live on
  `TerminalState` as its own observable maximum? That would place the
  measurement on the subject that owns the state, but it is not required by any
  known consumer and is out of scope here.
- Are the peaks part of a supported measurement story that this RFC has not
  found evidence for? The commit that introduced them states no reason in its
  message and adds no test.

## Future possibilities

Removing the peaks makes `SessionCounters` uniformly cumulative, which makes the
remaining vocabulary question — that `unwritten` is a derived residual among
cumulative totals — answerable on its own terms. It also removes the second
computation path for `undecoded_read()` and `unwritten()`, so that any future
consistency check between the counters and the internal buffers compares one
computation against itself.
