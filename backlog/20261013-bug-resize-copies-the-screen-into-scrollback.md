# Bug: A vertical shrink pushes every surviving row into the scrollback

- Status: open

## Summary

When a `TerminalState` is resized to fewer rows, `TerminalState::resize`
appends the rows the shrink pushed off the top to the scrollback -- but on a
shrink those rows are not "displaced content" the way they are on a scroll.
The rows that leave are the rows *below* the new bottom edge, and they are
often blank; the rows that survive are kept bottom-aligned. So a single shrink
of a mostly-blank screen copies that blank screen into the history, and
the visible grid is left blank while its text moves into the history.

Repeating a shrink-and-grow cycle appends the same rows again every time. A
session that is resized periodically therefore grows its history without
bound, with the real output buried under repeated copies of one screen, and
the rows that a caller is showing on screen are the blank ones.

## Reproduction

Drive a terminal at 42 rows x 80 cols, write two lines at the top so the rest
of the grid is blank, then shrink to 2 rows x 80 cols, then read the
scrollback and the visible rows.

```rust
let mut term = TerminalState::new(size(42, 80));
term.feed(b"line one\r\nline two\r\n");

term.resize(size(2, 80));

// Observed: scrollback_lines() holds 40 lines -- "line one", "line two", then
// 38 empty lines -- and both visible rows are empty.
// Expected: history should hold nothing (nothing was scrolled off), and the
// two lines should still be visible.
```

A repeat of the cycle shows the growth; each round shrinks to 2 rows and grows
back to 42:

```rust
let mut term = TerminalState::new(size(42, 80));
term.feed(b"only text\r\n");
for _ in 0..4 {
    term.feed(b"prompt> ");
    term.resize(size(2, 80));
    term.resize(size(42, 80));
}

// Observed: history grows by 40 lines per round -- 40, 80, 120, 160 -- and
// 155 of the final 160 lines are empty.
// Expected: a shrink-and-grow cycle should leave the history as it found it,
// because no text ever scrolled off the screen.
```

Nothing about the fed text matters beyond there being blank rows below it,
which is the ordinary case (a prompt at the top of a tall, mostly empty
screen). The larger the shrink, the more rows are appended.

## Observed behavior

`TerminalState::resize` in `src/terminal.rs` appends the rows `Screen::resize`
reports as displaced, exactly as `scroll_up_screen` does for a real scroll:

```rust
let dropped = self.primary.resize(size);
// Both screens are kept at the same size, so they shift by the same
// amount; the primary's shift is the cursor's.
let _ = self.alternate.resize(size);
if !self.on_alternate {
    for row in dropped.rows {
        let line = ScrollbackLine::new(row);
        self.scrollback_cells = self.scrollback_cells.saturating_add(line.len());
        self.scrollback.push_back(line);
        self.events.mark_scrollback_line_added();
    }
}
```

`Screen::resize` in `src/terminal_buffer.rs` produces `dropped.rows` by
copying, for a shrink, the rows that do not fit at the new height:

```rust
let row_shift = old_rows - copy_rows;
let mut dropped = Vec::new();
for row in 0..row_shift {
    let start = row * old_cols;
    dropped.push(self.cells[start..start + old_cols].to_vec());
}
let mut cells = vec![Cell::EMPTY; cell_count(size)];
for row in 0..copy_rows {
    let src_row = row + row_shift;
    // ... copy the surviving rows from `src_row` ...
}
```

So the rows handed to the caller are the *first* `old_rows - new_rows` rows of
the old grid, and the rows kept are the *last* `new_rows` rows. Reading
"dropped" as "scrolled off the top" is what makes the append look right, but a
shrink is not a scroll: nothing moves up because output arrived, the viewport
itself got shorter, and the rows that leave the view sit below the new bottom
edge rather than above the top. Neither `scroll_up_screen`'s contract

> A full-screen scroll on the primary screen pushes the displaced rows into
> scrollback (top-to-bottom); partial regions, scroll-downs, line edits, and
> the alternate screen never do.

nor the `resize` doc comment's

> A vertical shrink keeps the newest rows visible and appends the rows it
> pushes off the top to the scrollback, at the pre-shrink width and in
> top-to-bottom order, the same end and order a full-screen scroll uses.

holds for a screen whose newest rows are blank: the rows "kept visible" are
the trailing blanks, and the appended rows are the ones that were covered by
the new, smaller viewport.

Two consequences are visible in the reproduction:

- **Text moves into history and off the screen.** After the shrink the two
  written lines are only in `scrollback_lines()`; `rows()` shows two blank
  rows. A caller that renders the visible grid shows an empty sheet, and the
  text it was showing before the resize is only reachable by reading history.
- **The history grows by the whole screen per cycle**, because growing back
does not remove what the shrink appended. Nothing bounds this: `resize` is
  the caller's own action, and a caller that re-lays out on a timer or on user
  input files the same rows again each time.

## Expected behavior

A shrink should not manufacture history out of rows that never scrolled off.
The two halves of the report are separable and either alone would be a fix:

- Do not append to the scrollback for a shrink whose displaced rows are
  entirely blank (the `resize` doc already frames the append as keeping
  content, and a blank row is not content).
- Keep the *content* rows visible rather than the blank ones. The grid is
  short by `old_rows - new_rows` rows when the viewport shrinks; the rows that
the caller can still show are the ones holding text, and the rows that should
be appended -- if anything is appended -- are the ones below them.

The invariant that is broken is the containment one: a shrink must not move
rows from the visible grid into history unless those rows are the ones the
caller can no longer display. Today it moves the rows the caller cannot
display *into* the display position and appends the ones it could.

## Impact

Correctness and resource use, both reachable from the public API:

- A host that renders the visible grid shows blank rows after an ordinary
  resize, and the text it was showing is only in history. A host that renders
  history followed by the visible grid shows the text, then a run of blanks,
  then the new screen -- the shape the reproduction prints.
- History grows by up to the full height of the old grid per shrink, with no
  built-in bound (documented: history grows until the caller trims it). A
  caller that trims to a line limit will drop real output in favour of the
  blank copies, because trimming removes the oldest lines first and the blanks
  are appended newest.
- A caller that re-lays out its child on every layout change -- the ordinary
  thing to do on `SIGWINCH` -- pays this on every step of a drag-resize.

## Notes

- `1a2e4ad` introduced the append when it changed a shrink to keep the newest
  rows visible; the rows had previously been dropped outright. The report is
  not that the rows should be dropped again -- the fix must not reintroduce
  the loss `1a2e4ad` addressed -- but that a blank row is not worth keeping in
  either store, and that the rows leaving the view are not the ones being
  appended.
- `25b8969` then made `Screen::resize` return `Displaced { rows, row_shift }`
  and had `TerminalState::resize` apply `row_shift` to the cursor. The same
  value is what tells the two cases apart here: a shrink's displaced rows are
  `old_rows - new_rows` rows taken from the *top* of a grid that was NOT
  scrolled, and the rows at the bottom are the ones with the newest content.
- A width-only change displaces no rows (`row_shift` is zero), so only a
  vertical shrink is affected. A vertical grow displaces no row either.
- The alternate screen is already excluded by the `!self.on_alternate` guard,
  so this is a primary-screen-only report.
- `Event::ScrollbackLineAppended` is marked inside the same loop, so a caller
  trimming on that event (the documented pattern) is handed a trim per shrink,
  not per append of real output.
