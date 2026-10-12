# Bug: shrinking a terminal drops visible rows instead of keeping them

- Status: fixed

## Summary

When a `TerminalState` is resized to fewer rows, the rows that no longer fit
are discarded outright: they are neither kept on the visible grid nor pushed
into the scrollback. A host application that keeps history for its user (for
example, by rendering the scrollback plus the visible grid) therefore loses
every row that was on screen at the instant of the shrink and was not already
in history. Because history is only ever fed by `scroll_up_screen`, a row that
was visible but never scrolled off exists in neither store, so it cannot be
recovered from any part of the public API.

## Reproduction

Drive a terminal at 4 rows x 8 cols, write four distinguishable rows so the
grid is full, then resize to 2 rows x 8 cols, then read the scrollback and the
visible rows.

```rust
let size = Size { rows: 4, cols: 8 };
let mut term = TerminalState::new(size);
// Feed "AAAA\r\nBBBB\r\nCCCC\r\nDDDD" so each of the four rows holds one line.
term.feed(b"AAAA\r\nBBBB\r\nCCCC\r\nDDDD");

term.resize(Size { rows: 2, cols: 8 });

// Expected: two rows in history (AAAA, BBBB) and two still visible (CCCC, DDDD),
// so all four lines remain readable.
// Observed: history stays empty; the visible grid holds only CCCC and DDDD;
// AAAA and BBBB are gone with no way to reach them.
```

Nothing about how the four lines are fed matters; what matters is that none of
them was ever scrolled off, so none reached history before the shrink.

## Observed behavior

`Screen::resize` in `src/terminal_buffer.rs` allocates a fresh cell buffer and
copies only the overlap:

```rust
let copy_rows = self.size.rows.get().min(size.rows.get());
let copy_cols = self.size.cols.get().min(size.cols.get());
for row in 0..copy_rows {
    // ... copy only this row into the new buffer ...
}
self.size = size;
self.cells = cells;
```

Rows at indices `>= copy_rows` -- the ones past the new bottom edge when the
terminal shrinks -- are never read, so the new buffer leaves them as
`Cell::EMPTY`. `TerminalState::resize` in `src/terminal.rs` then only re-lays
out the emulator:

```rust
self.primary.resize(size);
self.alternate.resize(size);
self.size = size;
// ... cursor and scroll-region clamping ...
self.events.mark_screen_updated();
```

It appends nothing to `self.scrollback`. The only path that ever appends to
history is `TerminalState::scroll_up_screen`, documented as the path that
pushes displaced rows into scrollback:

> A full-screen scroll on the primary screen pushes the displaced rows into
> scrollback (top-to-bottom); partial regions, scroll-downs, line edits, and
> the alternate screen never do.

A resize is none of those, so the dropped rows do not enter history through it
either. The documented behavior for `TerminalState::resize` says only that

> The visible grid reflows to the new size; the scrollback does not.

which describes what happens to the *scrollback* on resize; it does not say the
visible rows themselves may vanish, and no other documented invariant covers
their loss. The result is a violated containment: a row that was on screen
before the resize is present in neither `rows()` nor `scrollback_lines()`
after it.

## Expected behavior

The visible rows that a shrink pushes past the new bottom edge should be
preserved, in the one place that already holds off-screen rows: the scrollback.
After the reproduction, `scrollback_lines()` should end with `AAAA` and `BBBB`
(in that order), and `rows()` should hold `CCCC` and `DDDD`. This keeps the
containment invariant that the union of history and the visible grid only ever
grows when the terminal shrinks: every cell a caller could see before a resize
is reachable from the public API after it.

The rows should be appended at the *newest* end of history, the same end and
the same order that `scroll_up_screen` uses for displaced rows, so that
stitching `scrollback_lines()` then `rows()` still reads top-to-bottom in the
order the text appeared. They should keep the width they had before the resize,
consistent with a retained `ScrollbackLine` keeping the width it had when it was
scrolled off.

## Impact

A correctness problem reachable from the public API: a caller cannot recover
history it was previously able to show. It bites any host that keeps a log of
the session for the user, because shrinking the window -- an ordinary,
user-driven action, and one a layout change can trigger without the user
touching the terminal -- silently truncates that log. Larger shrinks lose more,
and repeated resizes lose the visible rows each time, so a session that is
resized a few times can lose most of what was on screen. There is no bound or
stuck loop involved; the effect is purely the missing rows.

## Notes

The width mismatch between history and the visible grid is already documented:
a retained `ScrollbackLine` keeps the width it had when it was scrolled off, so
after a resize `rows()` and `scrollback_lines()` can have different column
counts, and callers that stitch them together are told to track width per line.
Pushing the dropped rows into history preserves exactly that property -- they
carry the pre-shrink width -- so the fix removes no existing guarantee.

This report is only about *not losing* the rows. Reflowing retained history to
the new width when the terminal grows is a separate question about what the API
should offer and is not implied here.

## Outcome

Fixed in [#27](https://github.com/sile/termnix/pull/27) (merged as `404e1fd`).

A vertical shrink now keeps the newest rows visible and appends the rows it pushes off the top to the scrollback, at their pre-shrink width and in top-to-bottom order, the same end and order a full-screen scroll uses. So after shrinking a full 4x8 grid holding `AAAA`..`DDDD` to 2x8, the scrollback ends with `AAAA`, `BBBB` and the visible grid holds `CCCC`, `DDDD`, and the union of history and the visible grid only grows on a shrink. The alternate screen has no scrollback, so its displaced rows are discarded; a vertical grow displaces no row.

Note on reflow: this fix preserves rows, it does not re-wrap them. A horizontal shrink still clips each physical row at the new width and never re-wraps text onto the following rows -- termnix re-lays out cells but never re-derives logical lines, so it has no way to grow the height of a clipped line. Reflowing the live screen belongs to the child, which repaints itself at the new size on `SIGWINCH`. Reflowing settled output and the scrollback cannot be done from the child side, because the child cannot see the emulator's history, so if anyone ever did it, it would have to be termnix -- but that would impose a policy the crate's "mechanism only" stance leaves to the caller, and a retained `ScrollbackLine` deliberately keeps the width it was saved at. Reflow is therefore out of scope here and is not implied by keeping the rows.

The scope is unchanged from what is described above.
