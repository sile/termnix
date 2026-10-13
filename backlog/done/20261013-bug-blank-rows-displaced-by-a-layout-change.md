# Bug: Blank rows displaced by a layout change go into the scrollback

- Status: fixed

## Summary

Rows that leave the grid because the layout changed end up in the scrollback
whether or not they were content, so a blank screen that is erased or resized
files its blanks as history. Two entry points reach the same append:

- **A vertical shrink.** `TerminalState::resize` appends the rows the shrink
  pushed off the top -- but on a shrink those rows are not "displaced content"
  the way they are on a scroll. The rows that leave are the rows *below* the
  new bottom edge, and they are often blank; the rows that survive are kept
  bottom-aligned. So a single shrink of a mostly-blank screen copies that
  blank screen into the history, and the visible grid is left blank while its
  text moves into the history.
- **An erase followed by a scroll.** `CSI 2 J` blanks the screen; the output
  that follows scrolls the blanks off the top through the ordinary
  `scroll_up_screen`, which appends them like any other scrolled row. Here the
  rows really did scroll off, but they are blanks the clear made rather than
  anything the child wrote. See "Second entry point" below.

In both, the blanks are appended *newest*, so a caller trimming to a limit
drops real output to make room for them. Repeating a shrink-and-grow cycle
appends the same rows again every time: a session that is resized periodically
grows its history without bound, with the real output buried under repeated
copies of one screen, and the rows that a caller is showing on screen are the
blank ones.

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

## Second entry point: erasing the screen

The same append fires when the screen is erased and then scrolled, with no
resize involved. `CSI 2 J` (erase in display) clears the screen and leaves the
cursor where it was; output that follows reaches the bottom row, scrolls the
screen, and the scroll pushes the rows above it into the scrollback. On a
screen the clear just blanked, those rows are blanks.

```rust
let mut term = TerminalState::new(size(24, 80));
term.feed(b"first\r\nsecond\r\n");
term.feed(b"\x1b[2J");                     // clear; cursor stays where it was
term.feed(b"first\r\nsecond\r\n");         // rewrite the two lines
for _ in 0..30 {
    term.feed(b"\r\n");                   // carry the cursor to the bottom row
}

// Observed: scrollback_lines() holds 11 lines, and the first two are empty --
// ahead of "first" and "second".
// Expected: nothing scrolled off *before* the two lines were written, and the
// blanks the clear made are not the child's output, so the history should
// begin with "first".
```

The 30 newlines are only there to drive the scroll; any output that reaches
the bottom row does it, and 11 is just what this size leaves. Clearing alone
does not append (a clear followed by two lines leaves the history empty), and
that is the trap: the append needs both the erase and a scroll, so it shows up
in a program that clears before drawing rather than in the clear itself.

Unlike the resize case, the scroll here is a real `scroll_up_screen` and the
rows really do leave the top of the grid; what is wrong is that the rows it
leaves with are the blank ones the clear produced rather than anything the
child wrote. Read against `scroll_up_screen`'s contract ("pushes the displaced
rows into scrollback") the append is faithful -- the rows were displaced -- so
this is the same question as the resize case one level down: whether a blank
row displaced by the screen's own layout is worth keeping, and whether the
rows leaving the view are the ones that should be kept.

For a host that hands the history to a reader, the visible symptom is a run of
blank lines before the output, exactly as in the resize case.

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
- The erase case costs a blank run per clear-and-draw. A full-screen program
  that clears before drawing (the common way to redraw a TUI) appends its own
  blanked screen every time it redraws, so its history fills with the blanks
  between its frames.

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
- The clear case ("Second entry point") reaches the append by a different
  route -- `scroll_up_screen` rather than `resize` -- and so does not depend on
  `Screen::resize`'s `row_shift` accounting. A fix for the resize case alone
  (deciding there what may be appended) leaves the clear case appending blanks
  through the ordinary scroll path. If the two are to be fixed by one rule,
  the rule belongs where both meet the scrollback -- at the append itself,
  deciding whether a blank row is worth keeping -- rather than in `resize`.

## Outcome

Fixed in [#30](https://github.com/sile/termnix/pull/30) (merged as `9bddb09`).

The append is now decided at one place. `push_displaced_to_scrollback` in
`src/terminal.rs` skips a displaced row when every cell in it is a blank
glyph, and both callers -- `resize` and `scroll_up_screen` -- go through it,
so the erase-then-scroll entry point is covered by the same rule as the
shrink.

`Screen::resize` no longer displaces the top rows of the old grid on a
shrink. It measures the effective grid -- the rows up to the last one holding
a non-blank glyph -- and keeps the bottom of that, so a shrink of a
mostly-blank screen leaves the text visible instead of moving it into
history. The rows that do not fit in the new height are handed back as
displaced, still top-to-bottom and at the pre-shrink width, but they are now
the rows below the kept content rather than the ones above it. A grow is
unchanged.

`TerminalState::resize` uses the active screen's `row_shift` for the cursor,
because the two screens can shift by different amounts once the shift follows
the content rather than the height alone.

Blankness is glyph-only: a row counts as blank when every cell is the space
glyph, whatever its style. Testing the glyphs keeps a run of styled blanks --
background colour set, no text -- out of the scrollback, which is the case the
report is about.

The cursor tests from #29 were rewritten to put content at the bottom of the
grid so that a real shift still occurs, and the reference model in
`tests/terminal_state.rs` now skips blank rows in `ref_scroll` while still
counting every scroll. Regression tests cover the shrink leaving content
visible with no history, a shrink-and-grow cycle leaving the history
unchanged, a scroll over an erased screen adding no blanks, and a shrink
pushing only the content above the kept rows.

The scope is unchanged from what is described above.
