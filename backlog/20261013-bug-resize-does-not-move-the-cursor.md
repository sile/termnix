# Bug: A vertical shrink moves the rows but leaves the cursor behind

- Status: open

## Summary

A vertical shrink keeps the newest rows visible by moving every surviving row
up to fill the new, shorter grid, but it only *clamps* the cursor into the new
bounds and never moves it up by the same amount. The clamp fires only when the
cursor would leave the new height; a cursor that stays inside keeps its old row
number, so it ends up on a row that no longer holds what the child last wrote
there. The child's next repaint (sent in response to the `SIGWINCH` a caller
raises for it) then lands on the wrong row, and the screen shows the newest
line plus a run of blank rows, or blanks entirely when the grid is short
enough.

This is the same resize path that [#27](https://github.com/sile/termnix/pull/27)
changed to keep the bottom rows visible; that change moved the rows but left
the cursor where it was.

## Reproduction

Put the cursor in the *middle* of a 24x80 grid, then shrink the height by two
rows so that the cursor stays inside the new height and the clamp never fires.

```rust
// `term` is a `TerminalState` of size 24x80; `text_at` reads a visible row
// through `rows()` (both are the testing helpers the library tests use).
term.feed(b"\x1b[6;1H");            // cursor to row 5 (0-based)
term.feed(b"here");                 // cursor is now at row 5, col 4

term.resize(size(22, 80));

// Rows are kept bottom-aligned: every surviving row moves up by two, so the
// text written at row 5 is now on row 3.
assert_eq!(text_at(&term, 3), "here");      // passes: the text moved up
assert_eq!(term.cursor().row, 3);            // FAILS: cursor is still row 5
```

Observed: `term.cursor().row` is `5` — the old row, unchanged. The clamp is
`min(cursor.row, new_rows - 1)` = `min(5, 21)` = `5`, so it does not fire, and
the cursor keeps the row number it had while the row it names moved up by two.
`cursor()` and `rows()` disagree by the two rows the content moved.

Choosing a cursor that is *not* near the new bottom is what makes the bug
visible. With the cursor on the old bottom row, the clamp floor (`new_rows -
1`) happens to coincide with the required shift, and the bug hides; the
Reproduction above avoids that coincidence on purpose.

A caller that owns a PTY sets the kernel winsize and the child redraws at the
cursor it is told about (its own stored position, refreshed from `CPR`), so in
practice the child erases from the wrong row and paints its line there: the
text ends up one or two rows below where it belongs, with blank rows above it,
and the top of the buffer is short two rows of history relative to the cursor.
Shrinking further makes the gap grow until the screen is blank.

## Observed behavior

`Screen::resize()` in `src/terminal_buffer.rs` keeps the bottom rows and moves
each surviving row up by `row_shift`:

```rust
let row_shift = old_rows - copy_rows;
...
for row in 0..copy_rows {
    let src_row = row + row_shift;
    ...
}
```

but `TerminalState::resize()` in `src/terminal.rs` only *clamps* the cursor
into the new bounds and never applies that same shift:

```rust
self.size = size;
self.scroll_top = 0;
self.scroll_bottom = size.rows.get().saturating_sub(1);
self.cursor.row = self.cursor.row.min(size.rows.get() - 1);
self.cursor.col = self.cursor.col.min(size.cols.get() - 1);
```

The clamp is a no-op for any shrink that leaves the old cursor row inside the
new height. When the cursor was on the old bottom row it lands exactly on the
clamp floor `new_rows - 1`, which coincides with the row the shift would have
produced, and the bug hides; a cursor anywhere above that keeps its old row
number while the row it names has moved up.

The invariant this breaks is the one `rows()` and the cursor are read
against: after a resize, `cursor()` must name the same cell the child last
wrote, and `rows()` must still place that cell one row above the bottom edge.
The two disagree by `old_rows - new_rows` rows whenever the clamp does not
fire, and by less when it partially does.

## Expected behavior

`TerminalState::resize()` should move `cursor.row` up by the same amount the
surviving rows moved, then clamp. `old_rows - new_rows` is already computed as
`row_shift` inside `Screen::resize()`; the state has only to apply it (whether
`Screen::resize()` returns it, or `TerminalState::resize()` reads `old_rows
- new_rows` before overwriting `self.size`, is up to the fix). On a grow
(`new_rows > old_rows`) the shift is zero and the clamp still applies.

This is what the doc comment on `TerminalState::resize()` implies when it
says the resize "cop[ies] the overlapping region" and then clamps: the
overlapping region is the bottom-aligned one, so the cursor, which is a point
in that region, has to move with it.

## Impact

Correctness: after a vertical shrink that does not bring the cursor onto the
new bottom row, the reported cursor position disagrees with the visible grid,
so any caller that repaints from `cursor()` — which is every caller that a
child sends a repaint to — draws at the wrong row. For a host embedding a
child session (for example an editor, a pager, or a shell prompt), the visible
symptom is a prompt drawn below where the surviving text sits, with blank rows
above it; the discrepancy grows with each additional shrink until the screen
clears.

This is reproducible from the public API alone: `feed()`, `resize()`, and
`cursor()` are all public, and the reproduction above needs no PTY.

## Notes

- The existing test `resize_keeps_the_bottom_row_and_clamps_cursor` in
`tests/terminal.rs` does not catch this, because there the cursor is on the
bottom row and the shrink is a single row: the clamp floor (`new_rows - 1`)
and the required shift (`old_rows - new_rows`) are both zero, so clamping
happens to give the right answer. The bug needs the cursor to end up above the
new bottom row, which is what the Reproduction arranges.
- A pure width change does not shift rows, so `cursor.row` is correct there;
only a vertical shrink is affected.
- The `scroll_top` / `scroll_bottom` reset in the same block (added by the same
pull request) is correct as it stands: the next `scroll_up_screen()` compares
against the new `size`, and a stale region would be the row-shift bug's
sibling. Only the cursor move is missing.
- `repair_cursor_cell()` runs after the clamp and only walks back off a
zero-width cell; it cannot be relied on to find the shifted row.
