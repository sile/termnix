# RFC: Report colour changes, and report a title change only as a title change

- Status: draft

## Summary

`ScreenUpdated` should mean one thing - the cells are different - and every
other value a caller can read should have its own event when it changes. Today
two state rows break that rule at once: a window-title change raises
`ScreenUpdated` even though `TitleUpdated` already reports it, and a palette or
default-colour change raises nothing at all, so a caller that resolved a colour
through `palette_color()` has no way to learn it must resolve it again. This
proposal stops raising `ScreenUpdated` for a title, and adds `ColorsUpdated`
for the colours, mirroring the title: state the crate holds, plus an event that
says it moved.

## Motivation

A host application that embeds a child session reacts to the child through
[`next_event()`](crate::TerminalState::next_event). It has one arm for
"repaint" and wants every other arm to name exactly the thing that changed. Two
state rows violate that today.

**The title raises the repaint event.** An OSC 0 / OSC 2 title change sets the
`title_changed` detector that `feed()` folds into the same `changed` test as a
cell write, so one title change yields both `TitleUpdated` and `ScreenUpdated`.
The host therefore repaints a screen whose cells are byte-for-byte identical.
Reading `TitleUpdated` and repainting anyway is a small waste; the real problem
is what `ScreenUpdated` means. Once a host treats it as "the cells changed", a
signal that also fires for a title change is one it cannot trust, and it has to
keep a second, more careful signal to know when a repaint is actually needed.

**The colours raise nothing.** A caller told to resolve an [`Indexed`](Color::Indexed)
cell through [`palette_color()`](crate::TerminalState::palette_color) - which is
what that method's documentation says to do - cannot observe an OSC 4 override
or an OSC 10/11/12 default-colour change at all. A test pins the current
behaviour:

```rust
#[test]
fn osc_4_does_not_raise_screen_updated() {
    // A colour set draws nothing; it changes what a later paint means.
    let mut t = term(1, 8);
    t.feed(b"\x1b]4;1;rgb:ff/00/00\x07");
    assert!(!screen_updated(&mut t), "a palette set is not a visible change");
}
```

The rationale in that comment is sound as far as it goes - a colour set draws
nothing - but it answers a question the host never asked. The host is not
asking whether it must draw; it is asking whether the value it already read is
still current. For the palette and the default colours the answer changes under
it, and no event says so. If the host does not re-read the palette, the child's
recolouring is silently ignored, which undercuts the reason `palette_color()`
exists.

Both problems are the same problem seen from two sides: `ScreenUpdated` is
carrying "something a host might want to react to" instead of "the cells are
different". Deciding what it means forces a decision about which state rows get
their own event, and the title and the colours are the two rows where the
current answer is wrong.

## Guide-level explanation

Before, one arm has two meanings and one change has no arm:

```rust
while let Some(event) = term.next_event() {
    match event {
        // Also fires when only the title moved, so this repaints for nothing.
        Event::ScreenUpdated => redraw(&term),
        Event::TitleUpdated => update_title(&term),
        Event::RequestReceived(request) => handle(request),
        _ => {}
    }
}
```

After, each arm names one thing, and a colour change is one of them:

```rust
while let Some(event) = term.next_event() {
    match event {
        Event::ScreenUpdated => redraw(&term),
        Event::TitleUpdated => update_title(&term),
        Event::ColorsUpdated => {
            // Resolve Indexed cells again: palette_color() may now differ.
            repaint_with_current_colours(&term);
        }
        Event::RequestReceived(request) => handle(request),
        _ => {}
    }
}
```

A host that does not care about colours simply leaves that arm out of its
drain loop; the event being dropped does not lose anything, because the
colours remain readable through [`palette_color()`](crate::TerminalState::palette_color)
and the `default_*` accessors whenever the host does repaint for another
reason. The event is a hint to look again, not the only way to see the value -
exactly how `TitleUpdated` relates to [`title()`](crate::TerminalState::title).

## Reference-level explanation

Three changes.

**1. A title change stops raising `ScreenUpdated`.** The `title_changed` field
is a per-feed detector whose only job is to fold the title into the `changed`
test in `feed()`:

```rust
// src/terminal.rs
let changed = primary_dirty || self.title_changed || (self.visible_scalars() != before);
```

That use of the field is removed, so a title change no longer moves the repaint
signal. The title is the one visible field that is not `Copy`, which is why it
had a detector of its own instead of joining the scalar comparison; with the
fold removed, the detector is no longer needed at all, because
`osc_dispatch` already calls `events.mark_title_updated()` next to it. Both
`title_changed` and its per-feed reset in `feed()` go away.

**2. A new `Event::ColorsUpdated` variant.** Doc shape mirrors `TitleUpdated`:
reports that a colour the terminal resolves through
[`palette_color()`](crate::TerminalState::palette_color) or one of the
`default_*` accessors changed, and says nothing about the cells, so it is not a
repaint request on its own - it is a signal to resolve those colours again.

**3. The colour paths mark it.** OSC 4 (`osc_palette`) and OSC 10/11/12
(`osc_default_color`) both live in `src/terminal_emu.rs`. Each stores into
`self.term.palette` or `self.term.default_colors`; each of those stores is
followed by a mark, mirroring how `osc_dispatch` already pairs storing the
title with `mark_title_updated()`. The natural home is a
`mark_colors_updated()` beside the other `Events` markers, or a single
`events.mark_colors_updated()` call on each successful set. A query (`?`) fetches
from that state and stores nothing, so it does not mark.

Not changed: `ScreenUpdated` keeps its current behaviour for cell writes; a
resize still marks it unconditionally (the visible size changed, which is a
repaint in its own right); `TerminalReset` still outranks everything and is
raised alongside the colours it resets, so a caller sees one reset rather than
a reset plus a colour event it would otherwise act on.

Yield order stays fixed: `TerminalReset`, then `ScreenUpdated`,
`ScrollbackLineAppended`, `TitleUpdated`, and now `ColorsUpdated`, then
requests. The new variant is a merged state flag like the others, so many
colour changes in one `feed` arrive as one event.

The visible state a caller reads is unchanged; only which events a change
raises changes.

## Drawbacks

- One more event variant, one more arm in an exhaustive `match` the crate's own
  example and doc examples use.
- A title change that used to also repaint is now a title-only event. A host
  that was relying on the fallthrough behaviour (repaint because it did not
  handle `TitleUpdated`) would stop repainting on a title change. In practice
  a title change does not alter the drawn cells, so this only exposes a host
  that was repainting for the wrong reason.
- A new event is a new thing every caller must decide to ignore consciously;
  an unhandled `ColorsUpdated` is harmless but is one more variant to skip.

## Rationale and alternatives

- **Why a dedicated `ColorsUpdated` rather than folding colours into
  `ScreenUpdated`?** Folding would keep the variant count down, but it restates
  the mistake the title makes today: it makes `ScreenUpdated` mean "something a
  host might want to react to" again, and forces hosts that never render from
  palette state to repaint on every OSC 4 set. A colour change draws nothing,
  so the honest event is one that says the resolved value moved, not that the
  screen did.
- **Why not model colour changes the way the title is modelled today, i.e. no
  event at all and read state?** The host already reads `title()` on the
  `TitleUpdated` event, and the title is only visible when rendered; colours
  affect the *interpretation* of `Indexed` cells already on screen, so a host
  cannot infer a colour change from anything it re-reads unless it re-reads the
  whole palette every frame. That is the cost the event exists to remove.
- **Why change the title at all if it only wastes one repaint?** Because the
  value of `ScreenUpdated` is that a host can trust it. A signal that also
  fires for a title change has to be double-checked with `TitleUpdated`,
  turning one clear signal into two rules. Fixing it costs one removed field and
  makes the `ScreenUpdated` doc true as written.
- **What is the impact of doing nothing?** The two rows keep contradicting each
  other: `ScreenUpdated` over-reports for the title and under-reports for the
  colours, and neither `palette_color()`'s contract nor `ScreenUpdated`'s doc
  matches what a host observes.

## Unresolved questions

- Does the example (`examples/tuinix.rs`) need to render the palette/host
  colours to show `ColorsUpdated` mattering, or is documenting the arm enough?
  Leaning toward documenting the arm: the example already drops most requests,
  and its screen rendering is not palette-faithful today.
- Should `resize()` keep marking only `ScreenUpdated`, or also `ColorsUpdated`?
  A resize rearranges cells, not colours, so the plan is `ScreenUpdated` only
  (as it is today); a default-colour change is a separate thing a resize does
  not cause.
- Is there a fourth state row this settles the pattern for? No other visible
  value changes in isolation today, so the rule ("every visible value has a
  matching event; `ScreenUpdated` means the cells changed") is the whole plan,
  not just the two rows above.

## Future possibilities

- If a caller later needs a coarse "anything changed, repaint" signal - for a
  host that resolves palette colours every frame and does not want to track
  per-value events - it can be built on top of these variants rather than
  reintroduced into `ScreenUpdated`.
- `ColorsUpdated` is the first state-row event added after the pair that
  shipped with the event channel; if a third row appears, this fix makes the
  rule explicit for it to follow.
